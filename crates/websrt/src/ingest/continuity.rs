//! Read-only MPEG-TS continuity counter probe.
//!
//! Wraps an [`Ingester`] and scans every delivered message for TS continuity
//! counter (CC) discontinuities without modifying the data. A CC gap seen here
//! indicates loss upstream of the gateway (OBS→gateway SRT); the absence of
//! gaps means any downstream loss originates in the broadcaster fanout or the
//! per-session SRT/QUIC path.
//!
//! Parsing and CC semantics live in the mpeg2ts fork ([`mpeg2ts::ts::CcChecker`]):
//! each 188-byte chunk is parsed with [`mpeg2ts::ts::TsPacketReader`] and fed
//! to the checker. This fixes the old scanner's misread of
//! `adaptation_field_length == 0` packets (where it treated the first payload
//! byte as AF flags and spuriously reset per-PID tracking).

use super::{Ingester, TsMessage};
use anyhow::Result;
use async_trait::async_trait;
use mpeg2ts::ts::{CcChecker, CcStatus, ReadTsPacket, TsPacketReader};
use std::collections::HashSet;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

const TS_PACKET_SIZE: usize = 188;

/// Cloneable view onto a [`TsContinuityChecker`]'s counters. The checker is
/// moved into the broadcaster pipeline; this handle lets the embedding
/// application (e.g. a health endpoint) keep reading the live counters.
#[derive(Clone)]
pub struct TsStatsHandle {
    pub cc_gaps: Arc<AtomicU64>,
    pub cc_checks: Arc<AtomicU64>,
    pub messages_seen: Arc<AtomicU64>,
}

impl TsStatsHandle {
    pub fn cc_gaps(&self) -> u64 {
        self.cc_gaps.load(Ordering::Relaxed)
    }

    pub fn cc_checks(&self) -> u64 {
        self.cc_checks.load(Ordering::Relaxed)
    }

    pub fn messages_seen(&self) -> u64 {
        self.messages_seen.load(Ordering::Relaxed)
    }
}

/// Read-only TS continuity counter probe. Wraps an [`Ingester`] and checks
/// CC continuity on every 188-byte TS packet in each delivered message.
/// Does NOT modify the data — pure diagnostic.
pub struct TsContinuityChecker<I> {
    inner: I,
    cc_checker: CcChecker,
    warned_pids: HashSet<u16>,
    cc_gaps: Arc<AtomicU64>,
    cc_checks: Arc<AtomicU64>,
    messages_seen: Arc<AtomicU64>,
}

impl<I> TsContinuityChecker<I> {
    pub fn new(inner: I) -> Self {
        Self {
            inner,
            cc_checker: CcChecker::new(),
            warned_pids: HashSet::new(),
            cc_gaps: Arc::new(AtomicU64::new(0)),
            cc_checks: Arc::new(AtomicU64::new(0)),
            messages_seen: Arc::new(AtomicU64::new(0)),
        }
    }

    /// Cloneable handle to the live counters. Keep this before moving the
    /// checker into the broadcaster pipeline.
    pub fn stats_handle(&self) -> TsStatsHandle {
        TsStatsHandle {
            cc_gaps: self.cc_gaps.clone(),
            cc_checks: self.cc_checks.clone(),
            messages_seen: self.messages_seen.clone(),
        }
    }

    pub fn cc_gaps(&self) -> u64 {
        self.cc_gaps.load(Ordering::Relaxed)
    }

    pub fn cc_checks(&self) -> u64 {
        self.cc_checks.load(Ordering::Relaxed)
    }

    pub fn messages_seen(&self) -> u64 {
        self.messages_seen.load(Ordering::Relaxed)
    }

    fn scan(&mut self, bytes: &[u8]) {
        for chunk in bytes.chunks_exact(TS_PACKET_SIZE) {
            // A fresh reader per chunk keeps the scan aligned even when a
            // chunk fails to parse (bad sync byte, malformed fields): such
            // chunks are skipped, exactly like the old scanner's sync check.
            let packet = match TsPacketReader::new(chunk).read_ts_packet() {
                Ok(Some(packet)) => packet,
                _ => continue,
            };
            let pid = packet.header.pid.as_u16();
            let status = self.cc_checker.check(&packet);

            // cc_checks counts payload-bearing packets that participated in
            // a CC comparison (i.e. everything except signalled resets).
            if packet.payload.is_some() && !matches!(status, CcStatus::Reset) {
                self.cc_checks.fetch_add(1, Ordering::Relaxed);
            }
            if let CcStatus::Discontinuity { expected, got } = status {
                let total = self.cc_gaps.fetch_add(1, Ordering::Relaxed) + 1;
                if self.warned_pids.insert(pid) {
                    tracing::warn!(
                        "ingester TS CC gap: PID 0x{:x} expected {} got {} (total gaps: {})",
                        pid,
                        expected.as_u8(),
                        got.as_u8(),
                        total
                    );
                } else {
                    tracing::debug!(
                        "ingester TS CC gap: PID 0x{:x} expected {} got {} (total gaps: {})",
                        pid,
                        expected.as_u8(),
                        got.as_u8(),
                        total
                    );
                }
            }
        }
    }
}

#[async_trait]
impl<I: Ingester> Ingester for TsContinuityChecker<I> {
    async fn next_message(&mut self) -> Result<Option<TsMessage>> {
        let msg = self.inner.next_message().await?;
        if let Some((_ts, bytes)) = msg.as_ref() {
            self.messages_seen.fetch_add(1, Ordering::Relaxed);
            self.scan(bytes);
        }
        Ok(msg)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;

    const TS_SYNC_BYTE: u8 = 0x47;

    fn make_ts_packet(pid: u16, cc: u8, afc: u8) -> [u8; 188] {
        let mut pkt = [0u8; 188];
        pkt[0] = 0x47;
        pkt[1] = (pid >> 8) as u8 & 0x1F;
        pkt[2] = pid as u8;
        pkt[3] = (afc << 4) | (cc & 0x0F);
        pkt
    }

    fn make_ts_packet_with_disc(pid: u16, cc: u8) -> [u8; 188] {
        // afc=0b11 with a real one-byte adaptation field (length 1) whose
        // flags byte carries discontinuity_indicator (bit 7).
        let mut pkt = make_ts_packet(pid, cc, 0b11);
        pkt[4] = 1; // adaptation_field_length: one flags byte follows
        pkt[5] = 0x80; // discontinuity_indicator = 1
        pkt
    }

    fn scan_packets(checker: &mut TsContinuityChecker<()>, packets: &[[u8; 188]]) {
        let mut buf = Vec::new();
        for p in packets {
            buf.extend_from_slice(p);
        }
        checker.scan(&buf);
    }

    #[test]
    fn afc_no_payload_skips_cc_check() {
        let mut checker = TsContinuityChecker::<()>::new(());
        let packets = [
            make_ts_packet(0x100, 5, 0b00),
            make_ts_packet(0x100, 9, 0b10),
            make_ts_packet(0x100, 3, 0b00),
        ];
        scan_packets(&mut checker, &packets);
        assert_eq!(checker.cc_checks(), 0);
        assert_eq!(checker.cc_gaps(), 0);
    }

    #[test]
    fn sequential_cc_stream_never_reports_gap() {
        let mut checker = TsContinuityChecker::<()>::new(());
        let packets = [
            make_ts_packet(0x100, 0, 0b01),
            make_ts_packet(0x100, 1, 0b01),
            make_ts_packet(0x100, 2, 0b01),
            make_ts_packet(0x100, 3, 0b01),
            make_ts_packet(0x100, 4, 0b01),
        ];
        scan_packets(&mut checker, &packets);
        assert_eq!(checker.cc_checks(), 5);
        assert_eq!(checker.cc_gaps(), 0);
    }

    #[test]
    fn cc_gap_detected() {
        let mut checker = TsContinuityChecker::<()>::new(());
        let packets = [
            make_ts_packet(0x100, 0, 0b01),
            make_ts_packet(0x100, 2, 0b01),
        ];
        scan_packets(&mut checker, &packets);
        assert_eq!(checker.cc_gaps(), 1);
    }

    #[test]
    fn cc_wraparound_not_a_gap() {
        let mut checker = TsContinuityChecker::<()>::new(());
        let packets = [
            make_ts_packet(0x100, 15, 0b01),
            make_ts_packet(0x100, 0, 0b01),
        ];
        scan_packets(&mut checker, &packets);
        assert_eq!(checker.cc_gaps(), 0);
    }

    #[test]
    fn discontinuity_indicator_resets_state() {
        let mut checker = TsContinuityChecker::<()>::new(());
        let packets = [
            make_ts_packet(0x100, 0, 0b01),
            make_ts_packet_with_disc(0x100, 1),
            make_ts_packet(0x100, 5, 0b01),
        ];
        scan_packets(&mut checker, &packets);
        assert_eq!(checker.cc_gaps(), 0);
    }

    #[test]
    fn multiple_pids_independent() {
        let mut checker = TsContinuityChecker::<()>::new(());
        let packets = [
            make_ts_packet(0x100, 0, 0b01),
            make_ts_packet(0x200, 0, 0b01),
            make_ts_packet(0x100, 1, 0b01),
            make_ts_packet(0x200, 1, 0b01),
            make_ts_packet(0x100, 2, 0b01),
            make_ts_packet(0x200, 2, 0b01),
        ];
        scan_packets(&mut checker, &packets);
        assert_eq!(checker.cc_gaps(), 0);
    }

    #[test]
    fn sync_byte_mismatch_skipped() {
        let mut checker = TsContinuityChecker::<()>::new(());
        let mut pkt = [0u8; 188];
        pkt[0] = 0xFF;
        checker.scan(&pkt);
        assert_eq!(checker.cc_checks(), 0);
        assert_eq!(checker.cc_gaps(), 0);
    }

    // Regression test for the old scanner's bug: with afc=0b11 and
    // adaptation_field_length == 0 (legal single stuffing byte), byte 5 is
    // PAYLOAD, not AF flags. The old scanner read chunk[5] as flags, so a
    // payload byte with bit 7 set spuriously cleared per-PID CC tracking and
    // masked the real gap on the next packet. The fork-based scanner must
    // keep tracking: gap after the stuffed packet is still detected.
    #[test]
    fn afc11_zero_len_af_payload_bit7_does_not_reset_cc() {
        let mut checker = TsContinuityChecker::<()>::new(());
        let mut stuffed = make_ts_packet(0x100, 3, 0b11);
        assert_eq!(stuffed[4], 0); // adaptation_field_length == 0
        stuffed[5] = 0xFF; // payload byte with bit 7 set
        let packets = [
            make_ts_packet(0x100, 2, 0b01), // initialize tracking at cc=2
            stuffed,                        // cc=3: in-sequence, must NOT reset
            make_ts_packet(0x100, 5, 0b01), // real gap (expected 4) — must be caught
        ];
        scan_packets(&mut checker, &packets);
        assert_eq!(checker.cc_checks(), 3);
        assert_eq!(checker.cc_gaps(), 1);
    }

    proptest! {
        #[test]
        fn cc_counters_bounded_by_payload_bearing_packets(
            packets in proptest::collection::vec(
                (0x100u16..0x200u16, 0u8..16u8, 0u8..4u8, any::<bool>()),
                1..=100usize,
            ),
        ) {
            let mut checker = TsContinuityChecker::<()>::new(());
            let mut buf = Vec::new();
            let mut payload_bearing = 0u64;

            for (pid, cc, afc, disc) in &packets {
                let mut pkt = [0u8; 188];
                pkt[0] = TS_SYNC_BYTE;
                pkt[1] = ((pid >> 8) & 0x1F) as u8;
                pkt[2] = (pid & 0xFF) as u8;
                pkt[3] = (afc << 4) | (cc & 0x0F);
                if *afc == 0b11 && *disc {
                    pkt[4] = 1; // adaptation_field_length: one flags byte
                    pkt[5] = 0x80; // discontinuity_indicator = 1
                }
                buf.extend_from_slice(&pkt);

                if (*afc == 0b01 || *afc == 0b11) && !(*afc == 0b11 && *disc) {
                    payload_bearing += 1;
                }
            }

            checker.scan(&buf);

            let checks = checker.cc_checks();
            let gaps = checker.cc_gaps();
            prop_assert!(
                checks <= payload_bearing,
                "cc_checks ({}) must not exceed payload-bearing packets ({})",
                checks,
                payload_bearing,
            );
            prop_assert!(
                gaps <= checks,
                "cc_gaps ({}) must not exceed cc_checks ({})",
                gaps,
                checks,
            );
        }
    }
}
