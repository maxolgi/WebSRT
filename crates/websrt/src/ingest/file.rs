//! FileIngester: read .ts, pace at real-time, loop, emit `(Instant, Bytes)`.
//!
//! A "message" is N × 188-byte TS packets, where N is chosen to fit the SRT
//! payload size (default 1100 B → N=5, total 940 B per message).
//!
//! Real-time pacing: the fixture is captured at 30fps with 1s GOP and Opus
//! 64kbps. We compute the wall-clock duration of each message from the byte
//! position vs. the file's total bitrate; if reading ahead of real-time we
//! sleep, otherwise we emit immediately. We loop forever.
//!
//! `FileIngester` is the one synthetic-time ingester — there is no upstream
//! SRT clock to honor, so the emitted `Instant` is wall-clock now at emit
//! time. `SrtIngester` and `ChannelIngester` (browser publish path) preserve
//! the upstream TSBPD release instant instead.

use super::{Ingester, TsMessage};
use anyhow::{anyhow, Context, Result};
use async_trait::async_trait;
use bytes::Bytes;
use std::path::Path;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::time::sleep;

const TS_PACKET: usize = 188;
const PAYLOAD_BYTES: usize = crate::srt_sender::PAYLOAD_SIZE as usize;
const PACKETS_PER_MESSAGE: usize = PAYLOAD_BYTES / TS_PACKET; // = 5

pub struct FileIngester {
    data: Arc<Vec<u8>>,
    /// Index into `data` for the next message.
    cursor: usize,
    /// Wall-clock time when ingester started emitting.
    start: Option<Instant>,
    /// Total bytes per second, derived from the file's apparent duration.
    bytes_per_sec: f64,
    /// Total bytes emitted so far (loops over).
    emitted: u64,
}

impl FileIngester {
    pub fn new(path: impl AsRef<Path>, duration_secs: f64) -> Result<Self> {
        let path = path.as_ref();
        let data = std::fs::read(path).with_context(|| format!("read {}", path.display()))?;
        if data.len() % TS_PACKET != 0 {
            return Err(anyhow!(
                "fixture {} is {} bytes — not a multiple of 188",
                path.display(),
                data.len()
            ));
        }
        for (i, chunk) in data.chunks(TS_PACKET).enumerate() {
            if chunk[0] != 0x47 {
                return Err(anyhow!(
                    "fixture {} sync byte error at offset {} (expected 0x47, got 0x{:02x}) — not a valid MPEG-TS file",
                    path.display(),
                    i * TS_PACKET,
                    chunk[0]
                ));
            }
        }
        let bytes_per_sec = if duration_secs > 0.0 {
            (data.len() as f64) / duration_secs
        } else {
            (data.len() as f64) / 10.0
        };
        tracing::info!(
            bytes = data.len(),
            packets = data.len() / TS_PACKET,
            est_bytes_per_sec = bytes_per_sec,
            "loaded fixture"
        );
        Ok(Self {
            data: Arc::new(data),
            cursor: 0,
            start: None,
            bytes_per_sec,
            emitted: 0,
        })
    }

    /// Per-message byte budget.
    fn message_bytes(&self) -> usize {
        PACKETS_PER_MESSAGE * TS_PACKET
    }
}

#[async_trait]
impl Ingester for FileIngester {
    async fn next_message(&mut self) -> Result<Option<TsMessage>> {
        let chunk = self.message_bytes();

        // Loop the file if we've reached the end.
        if self.cursor + chunk > self.data.len() {
            self.cursor = 0;
            tracing::info!("fixture looped");
        }

        // First call establishes the start time at "now"; the message's
        // timestamp is the capture time of its first TS packet relative to
        // start. We use wall-clock now for `Instant` (TSBPD uses sender's
        // send clock per the plan).
        let now = Instant::now();
        if self.start.is_none() {
            self.start = Some(now);
        }

        // Pace: target wall-clock position is `emitted / bytes_per_sec` seconds
        // from start. If we're ahead of that, sleep.
        let target_elapsed = (self.emitted as f64) / self.bytes_per_sec;
        let actual_elapsed = now.duration_since(self.start.unwrap()).as_secs_f64();
        if target_elapsed > actual_elapsed {
            sleep(Duration::from_secs_f64(target_elapsed - actual_elapsed)).await;
        }

        let bytes = Bytes::copy_from_slice(&self.data[self.cursor..self.cursor + chunk]);
        self.cursor += chunk;
        self.emitted += chunk as u64;

        // The Instant timestamp we hand to srt-protocol is "now" at the time
        // of emission. The TSBPD latency (default 120ms in ConnInitSettings)
        // will delay delivery on the receiver.
        Ok(Some((Instant::now(), bytes)))
    }
}
