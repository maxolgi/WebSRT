//! Input ingest: produces `(std::time::Instant, bytes::Bytes)` TS messages.

pub mod channel;
pub mod continuity;
pub mod file;
pub mod srt;
pub mod srt_listener;

pub use channel::ChannelIngester;
pub use continuity::{TsContinuityChecker, TsStatsHandle};
pub use srt::SrtConnectionIngester;
pub use srt_listener::SrtListenerService;

use anyhow::{anyhow, Result};
use async_trait::async_trait;
use bytes::Bytes;
use srt_protocol::options::KeySize;
use srt_protocol::settings::KeySettings;
use std::time::Instant;

/// Build per-connection `KeySettings` from an optional passphrase string.
/// Validates the SRT 10–79 char requirement and converts to the wire type.
/// Returns `None` when no passphrase is set (unencrypted).
pub(super) fn build_key_settings(passphrase: &Option<String>) -> Result<Option<KeySettings>> {
    passphrase
        .as_ref()
        .map(|p| {
            if !(10..=79).contains(&p.len()) {
                anyhow::bail!("SRT passphrase must be 10–79 chars, got {}", p.len());
            }
            Ok(KeySettings {
                key_size: KeySize::Unspecified,
                passphrase: p.clone().try_into().map_err(|e| anyhow!("{e:?}"))?,
            })
        })
        .transpose()
}

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
