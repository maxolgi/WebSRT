//! Input ingest: produces `(std::time::Instant, bytes::Bytes)` TS messages.

pub mod channel;
pub mod continuity;
pub mod file;
pub mod srt;

pub use channel::ChannelIngester;
pub use continuity::{TsContinuityChecker, TsStatsHandle};

use anyhow::Result;
use async_trait::async_trait;
use bytes::Bytes;
use std::time::Instant;

/// One TS message: N × 188-byte TS packets, with the `Instant` indicating when
/// this message became available to the gateway. For SRT-backed ingesters
/// (`SrtIngester`, browser publish path) this is the TSBPD release instant from
/// the upstream SRT receiver. For synthetic sources (`FileIngester`) it is the
/// wall-clock emission time.
///
/// NOTE: this Instant is informational only — it is NOT stamped into outgoing
/// SRT data packets. The gateway→browser SRT session maintains its own TSBPD
/// timeline, so `SrtInitiator::push_message` ignores `msg.0` and uses the
/// current gateway `Instant::now()` for the packet timestamp. Using the upstream
/// release instant would cause browser-side PacketTooLate drops because that
/// instant is already in the past by the time the packet traverses the
/// broadcaster + ticker.
pub type TsMessage = (Instant, Bytes);

#[async_trait]
pub trait Ingester: Send {
    /// Wait for the next TS message. Returns `None` at end-of-stream.
    async fn next_message(&mut self) -> Result<Option<TsMessage>>;
}
