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

#[cfg(test)]
mod tests {
    use super::*;
    use bytes::Bytes;
    use std::time::{Duration, Instant};

    #[tokio::test]
    async fn preserves_upstream_timestamp() {
        let (tx, rx) = tokio::sync::mpsc::channel(8);
        let mut ingester = ChannelIngester::new(rx);
        let original_ts = Instant::now() - Duration::from_secs(5);
        tx.send((original_ts, Bytes::from_static(b"x")))
            .await
            .unwrap();
        let msg = ingester.next_message().await.unwrap().unwrap();
        assert_eq!(
            msg.0, original_ts,
            "ChannelIngester must preserve upstream Instant"
        );
    }
}
