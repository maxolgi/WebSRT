//! Input ingest: produces `(std::time::Instant, bytes::Bytes)` TS messages.

pub mod channel;
pub mod file;
pub mod srt;

pub use channel::ChannelIngester;

use anyhow::Result;
use async_trait::async_trait;
use bytes::Bytes;
use std::time::Instant;

/// One TS message: N × 188-byte TS packets, with the `Instant` indicating when
/// this message became available to the gateway. For SRT-backed ingesters
/// (`SrtIngester`, browser publish path) this is the TSBPD release instant from
/// the upstream SRT receiver. For synthetic sources (`FileIngester`) it is the
/// wall-clock emission time. The downstream `SrtInitiator::push_message` stamps
/// this instant into outgoing SRT data packets.
pub type TsMessage = (Instant, Bytes);

#[async_trait]
pub trait Ingester: Send {
    /// Wait for the next TS message. Returns `None` at end-of-stream.
    async fn next_message(&mut self) -> Result<Option<TsMessage>>;
}
