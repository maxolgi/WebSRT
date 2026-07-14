//! Input ingest: produces `(std::time::Instant, bytes::Bytes)` TS messages.
//! Implemented in Phase 4 (FileIngester) and Phase 8 (SrtIngester).

pub mod file;
pub mod srt;

use anyhow::Result;
use async_trait::async_trait;
use bytes::Bytes;
use std::time::Instant;

/// One TS message: N × 188-byte TS packets, captured at `capture_time`.
pub type TsMessage = (Instant, Bytes);

#[async_trait]
pub trait Ingester: Send {
    /// Wait for the next TS message. Returns `None` at end-of-stream.
    async fn next_message(&mut self) -> Result<Option<TsMessage>>;
}
