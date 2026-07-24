//! Read-only MPEG-TS continuity counter probe.
//!
//! Wraps an [`Ingester`] and scans every delivered message for TS continuity
//! counter (CC) discontinuities without modifying the data. A CC gap seen here
//! indicates loss upstream of the gateway (OBS→gateway SRT); the absence of
//! gaps means any downstream loss originates in the broadcaster fanout or the
//! per-session SRT/QUIC path.

use super::{Ingester, TsMessage};
use anyhow::Result;
use async_trait::async_trait;
use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

const TS_PACKET_SIZE: usize = 188;
const TS_SYNC_BYTE: u8 = 0x47;

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
    last_cc: HashMap<u16, u8>,
    warned_pids: HashSet<u16>,
    cc_gaps: Arc<AtomicU64>,
    cc_checks: Arc<AtomicU64>,
    messages_seen: Arc<AtomicU64>,
}

impl<I> TsContinuityChecker<I> {
    pub fn new(inner: I) -> Self {
        Self {
            inner,
            last_cc: HashMap::new(),
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
            if chunk[0] != TS_SYNC_BYTE {
                continue;
            }
            let pid = (u16::from(chunk[1] & 0x1F) << 8) | u16::from(chunk[2]);
            let cc = chunk[3] & 0x0F;
            let afc = (chunk[3] >> 4) & 0x03;

            // CC only increments on payload-bearing packets (AFC 0b01 or 0b11).
            if afc == 0b00 || afc == 0b10 {
                continue;
            }
            // AFC 0b11 carries an adaptation field: honor its
            // discontinuity_indicator (bit 7 of the AF flags byte, chunk[5])
            // by resetting this PID's state and skipping the CC check.
            if afc == 0b11 && chunk[5] & 0x80 != 0 {
                self.last_cc.remove(&pid);
                continue;
            }

            self.cc_checks.fetch_add(1, Ordering::Relaxed);
            if let Some(&prev) = self.last_cc.get(&pid) {
                let expected = (prev + 1) & 0x0F;
                if expected != cc {
                    let total = self.cc_gaps.fetch_add(1, Ordering::Relaxed) + 1;
                    if self.warned_pids.insert(pid) {
                        tracing::warn!(
                            "ingester TS CC gap: PID 0x{:x} expected {} got {} (total gaps: {})",
                            pid, expected, cc, total
                        );
                    } else {
                        tracing::debug!(
                            "ingester TS CC gap: PID 0x{:x} expected {} got {} (total gaps: {})",
                            pid, expected, cc, total
                        );
                    }
                }
            }
            self.last_cc.insert(pid, cc);
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

    fn make_ts_packet(pid: u16, cc: u8, afc: u8) -> [u8; 188] {
        let mut pkt = [0u8; 188];
        pkt[0] = 0x47;
        pkt[1] = (pid >> 8) as u8 & 0x1F;
        pkt[2] = pid as u8;
        pkt[3] = (afc << 4) | (cc & 0x0F);
        pkt
    }

    fn make_ts_packet_with_disc(pid: u16, cc: u8) -> [u8; 188] {
        let mut pkt = make_ts_packet(pid, cc, 0b11);
        pkt[5] = 0x80;
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
                    pkt[5] = 0x80;
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
