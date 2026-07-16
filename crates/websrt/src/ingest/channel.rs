use super::{Ingester, TsMessage};
use anyhow::Result;
use async_trait::async_trait;
use tokio::sync::mpsc;

/// Ingester backed by an mpsc channel. Used when a browser publishes upstream
/// data — the session pushes TsMessages into the channel, and the Broadcaster
/// reads them via this ingester.
pub struct ChannelIngester {
    rx: mpsc::Receiver<TsMessage>,
}

impl ChannelIngester {
    pub fn new(rx: mpsc::Receiver<TsMessage>) -> Self {
        Self { rx }
    }
}

#[async_trait]
impl Ingester for ChannelIngester {
    async fn next_message(&mut self) -> Result<Option<TsMessage>> {
        match self.rx.recv().await {
            Some(msg) => Ok(Some(msg)),
            None => Ok(None),
        }
    }
}
