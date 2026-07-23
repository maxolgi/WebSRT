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

const TS_PACKET_SIZE: usize = 188;
const TS_SYNC_BYTE: u8 = 0x47;

/// Read-only TS continuity counter probe. Wraps an [`Ingester`] and checks
/// CC continuity on every 188-byte TS packet in each delivered message.
/// Does NOT modify the data — pure diagnostic.
pub struct TsContinuityChecker<I> {
    inner: I,
    last_cc: HashMap<u16, u8>,
    warned_pids: HashSet<u16>,
    cc_gaps: AtomicU64,
    cc_checks: AtomicU64,
    messages_seen: AtomicU64,
}

impl<I> TsContinuityChecker<I> {
    pub fn new(inner: I) -> Self {
        Self {
            inner,
            last_cc: HashMap::new(),
            warned_pids: HashSet::new(),
            cc_gaps: AtomicU64::new(0),
            cc_checks: AtomicU64::new(0),
            messages_seen: AtomicU64::new(0),
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
