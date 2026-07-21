//! Multi-stream registry: maps stream names to broadcasters.
//!
//! Each stream has its own [`Broadcaster`] that fans TS messages out to
//! viewers. Sources publish by feeding an [`Ingester`] (or an
//! [`mpsc::Sender<TsMessage>`]) into the registry; viewers subscribe by name.
//!
//! All map operations use a `std::sync::Mutex` because the critical sections
//! are tiny (HashMap insert/lookup) and never held across an `.await`.

use crate::broadcaster::{Broadcaster, ViewerRx};
use crate::ingest::{ChannelIngester, Ingester, TsMessage};
use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use tokio::sync::mpsc;

/// Snapshot of one stream's state for health/stats reporting.
#[derive(Debug, Clone)]
pub struct StreamStats {
    /// Stream name (route key).
    pub name: String,
    /// True if the source is still producing.
    pub alive: bool,
    /// Current viewer count.
    pub viewers: usize,
}

pub struct StreamRegistry {
    streams: Mutex<HashMap<String, Arc<Broadcaster>>>,
    max_viewers: usize,
    broadcast_capacity: usize,
}

impl StreamRegistry {
    pub fn new(max_viewers: usize, broadcast_capacity: usize) -> Self {
        Self {
            streams: Mutex::new(HashMap::new()),
            max_viewers: max_viewers.max(1),
            broadcast_capacity: broadcast_capacity.max(1),
        }
    }

    /// Publish a new stream backed by a [`ChannelIngester`]. Returns the
    /// [`mpsc::Sender`] for feeding TS messages into the stream.
    ///
    /// A previously registered stream under the same name is replaced. Any
    /// viewers still attached to the old broadcaster keep receiving until that
    /// broadcaster's source ends; new subscribers get the new stream.
    pub fn publish(&self, name: &str) -> mpsc::Sender<TsMessage> {
        let (tx, rx) = mpsc::channel(self.broadcast_capacity);
        let ingester = ChannelIngester::new(rx);
        let broadcaster =
            Broadcaster::spawn(ingester, self.max_viewers, self.broadcast_capacity);
        self.streams
            .lock()
            .unwrap()
            .insert(name.to_string(), broadcaster);
        tx
    }

    /// Publish a stream from any [`Ingester`] (e.g. SRT from OBS, file
    /// fixture). The stream is immediately available for viewers to subscribe.
    ///
    /// Replaces any previously registered stream under the same name.
    pub fn publish_ingester<I>(&self, name: &str, ingester: I)
    where
        I: Ingester + Send + 'static,
    {
        let broadcaster =
            Broadcaster::spawn(ingester, self.max_viewers, self.broadcast_capacity);
        self.streams
            .lock()
            .unwrap()
            .insert(name.to_string(), broadcaster);
    }

    /// Subscribe to a stream by name. Returns `None` if the stream doesn't
    /// exist, is dead (source ended), or the per-stream viewer cap is reached.
    pub fn subscribe(&self, name: &str) -> Option<ViewerRx> {
        let streams = self.streams.lock().unwrap();
        streams.get(name).and_then(|b| b.subscribe())
    }

    /// Check if a stream exists and is alive.
    pub fn is_alive(&self, name: &str) -> bool {
        let streams = self.streams.lock().unwrap();
        streams.get(name).map(|b| b.is_alive()).unwrap_or(false)
    }

    /// Get viewer count for a single stream.
    pub fn viewer_count(&self, name: &str) -> usize {
        let streams = self.streams.lock().unwrap();
        streams.get(name).map(|b| b.viewer_count()).unwrap_or(0)
    }

    /// Sum of viewer counts across all streams (for health reporting).
    pub fn total_viewers(&self) -> usize {
        let streams = self.streams.lock().unwrap();
        streams.values().map(|b| b.viewer_count()).sum()
    }

    /// Snapshot all streams' state, sorted by name for stable output.
    /// Used by [`crate::gateway::GatewayStatsHandle`] for health/stats reporting.
    pub fn snapshot_streams(&self) -> Vec<StreamStats> {
        let streams = self.streams.lock().unwrap();
        let mut entries: Vec<StreamStats> = streams
            .iter()
            .map(|(name, bc)| StreamStats {
                name: name.clone(),
                alive: bc.is_alive(),
                viewers: bc.viewer_count(),
            })
            .collect();
        entries.sort_by(|a, b| a.name.cmp(&b.name));
        entries
    }

    /// Number of streams whose source is still alive.
    pub fn alive_stream_count(&self) -> usize {
        let streams = self.streams.lock().unwrap();
        streams.values().filter(|b| b.is_alive()).count()
    }

    /// Remove dead streams. Called periodically or on lookup.
    pub fn cleanup(&self) {
        let mut streams = self.streams.lock().unwrap();
        streams.retain(|_, b| b.is_alive());
    }

    /// Total number of registered streams (alive or dead).
    pub fn stream_count(&self) -> usize {
        self.streams.lock().unwrap().len()
    }

    pub fn max_viewers(&self) -> usize {
        self.max_viewers
    }

    pub fn broadcast_capacity(&self) -> usize {
        self.broadcast_capacity
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use crate::ingest::TsMessage;
    use bytes::Bytes;
    use std::time::Instant;

    /// Test helper: poll `try_recv` until a message arrives or 500ms elapses.
    /// Replaces the deleted `ViewerRx::recv` for test ergonomics.
    async fn recv_one(viewer: &mut ViewerRx) -> Option<TsMessage> {
        let deadline = std::time::Instant::now() + std::time::Duration::from_millis(500);
        loop {
            match viewer.try_recv() {
                Ok(Some(m)) => return Some(m),
                Ok(None) => {
                    if std::time::Instant::now() >= deadline {
                        return None;
                    }
                    tokio::time::sleep(std::time::Duration::from_millis(1)).await;
                }
                Err(lag) => panic!("viewer lagged: {}", lag),
            }
        }
    }

    /// Minimal ingester that yields a fixed number of messages then ends.
    pub(crate) struct FiniteIngester {
        remaining: u32,
    }

    impl FiniteIngester {
        pub(crate) fn new(n: u32) -> Self {
            Self { remaining: n }
        }
    }

    #[async_trait::async_trait]
    impl Ingester for FiniteIngester {
        async fn next_message(&mut self) -> anyhow::Result<Option<TsMessage>> {
            if self.remaining == 0 {
                return Ok(None);
            }
            self.remaining -= 1;
            Ok(Some((Instant::now(), Bytes::from_static(b"x"))))
        }
    }

    fn msg() -> TsMessage {
        (Instant::now(), Bytes::from_static(b"hello"))
    }

    #[tokio::test]
    async fn publish_then_subscribe_receives_data() {
        let registry = StreamRegistry::new(4, 8);
        let tx = registry.publish("foo");
        let mut viewer = registry.subscribe("foo").expect("stream exists");
        tx.try_send(msg()).unwrap();
        let received = recv_one(&mut viewer).await;
        assert!(received.is_some());
    }

    #[test]
    fn subscribe_unknown_stream_returns_none() {
        let registry = StreamRegistry::new(4, 8);
        assert!(registry.subscribe("nope").is_none());
    }

    #[test]
    fn is_alive_false_for_unknown_stream() {
        let registry = StreamRegistry::new(4, 8);
        assert!(!registry.is_alive("nope"));
        assert_eq!(registry.viewer_count("nope"), 0);
    }

    #[tokio::test]
    async fn publish_ingester_makes_stream_subscribable() {
        let registry = StreamRegistry::new(4, 8);
        registry.publish_ingester("obs", FiniteIngester::new(3));
        assert!(registry.is_alive("obs"));
        assert!(registry.subscribe("obs").is_some());
        assert_eq!(registry.stream_count(), 1);
    }

    #[tokio::test]
    async fn publish_replaces_existing_stream() {
        let registry = StreamRegistry::new(4, 8);
        let tx1 = registry.publish("dup");
        let tx2 = registry.publish("dup");
        // Re-publishing replaces; the new sender is the live one.
        tx2.try_send(msg()).unwrap();
        let mut viewer = registry.subscribe("dup").expect("stream exists");
        let received = recv_one(&mut viewer).await;
        assert!(received.is_some());
        // Old sender is orphaned (no receivers) but try_send still buffers.
        let _ = tx1;
    }

    #[tokio::test]
    async fn cleanup_removes_dead_streams() {
        let registry = StreamRegistry::new(4, 8);
        registry.publish_ingester("ephemeral", FiniteIngester::new(1));
        // Drain to completion so the broadcaster exits.
        {
            let mut viewer = registry
                .subscribe("ephemeral")
                .expect("stream exists before drain");
            // Give the broadcaster task time to flush + close.
            tokio::time::sleep(std::time::Duration::from_millis(100)).await;
            while viewer.try_recv().ok().flatten().is_some() {}
        }
        assert!(!registry.is_alive("ephemeral"));
        registry.cleanup();
        assert_eq!(registry.stream_count(), 0);
    }

    #[tokio::test]
    async fn total_viewers_and_alive_counts() {
        let registry = StreamRegistry::new(4, 8);
        registry.publish_ingester("a", FiniteIngester::new(10));
        registry.publish_ingester("b", FiniteIngester::new(10));
        let _va = registry.subscribe("a").unwrap();
        let _vb1 = registry.subscribe("b").unwrap();
        let _vb2 = registry.subscribe("b").unwrap();
        assert_eq!(registry.total_viewers(), 3);
        assert_eq!(registry.alive_stream_count(), 2);
        assert_eq!(registry.viewer_count("a"), 1);
        assert_eq!(registry.viewer_count("b"), 2);
    }

    #[test]
    fn new_clamps_capacity_to_1() {
        let registry = StreamRegistry::new(0, 0);
        assert_eq!(registry.max_viewers(), 1);
        assert_eq!(registry.broadcast_capacity(), 1);
    }

    #[tokio::test]
    async fn viewer_cap_enforced_by_subscribe() {
        let registry = StreamRegistry::new(2, 8);
        registry.publish_ingester("capped", FiniteIngester::new(10));
        let _v1 = registry.subscribe("capped").unwrap();
        let _v2 = registry.subscribe("capped").unwrap();
        assert!(registry.subscribe("capped").is_none());
    }
}
