//! Integration tests for [`websrt::Broadcaster`] fanout behavior.
//!
//! Covers: basic message delivery, viewer-cap enforcement, end-of-stream
//! propagation, and slow-viewer lag handling.
//!
//! Note: `Broadcaster` retains a clone of the broadcast `Sender`, so the
//! channel only closes once the `Broadcaster` itself is dropped. The
//! background task flips `alive=false` when the ingester returns `Ok(None)`,
//! but the underlying broadcast channel stays open until `Broadcaster` is
//! dropped. These tests therefore drain with `try_recv` (non-blocking) after
//! waiting for the task to exit.

use bytes::Bytes;
use std::time::{Duration, Instant};
use websrt::ingest::Ingester;
use websrt::Broadcaster;

/// Mock ingester: yields a fixed list of `(Instant, Bytes)` TS messages, then
/// either signals end-of-stream (`Ok(None)`) or — when `block_forever` is set
/// — never resolves again, keeping the broadcaster alive.
struct MockIngester {
    messages: Vec<(Instant, Bytes)>,
    idx: usize,
    block_forever: bool,
}

impl MockIngester {
    fn new(messages: Vec<(Instant, Bytes)>) -> Self {
        Self {
            messages,
            idx: 0,
            block_forever: false,
        }
    }

    /// After the message list is exhausted, block forever instead of returning
    /// `Ok(None)`. Use this to test the "alive" path without racing the
    /// background task's shutdown.
    fn block_forever(mut self) -> Self {
        self.block_forever = true;
        self
    }
}

#[async_trait::async_trait]
impl Ingester for MockIngester {
    async fn next_message(&mut self) -> anyhow::Result<Option<(Instant, Bytes)>> {
        if self.idx < self.messages.len() {
            let msg = self.messages[self.idx].clone();
            self.idx += 1;
            Ok(Some(msg))
        } else if self.block_forever {
            std::future::pending::<()>().await;
            unreachable!()
        } else {
            Ok(None)
        }
    }
}

/// Build a fake 188-byte MPEG-TS packet (sync byte 0x47 repeated).
fn ts_packet() -> (Instant, Bytes) {
    (Instant::now(), Bytes::from_static(&[0x47; 188]))
}

/// Poll `is_alive()` until it flips to false, or give up after `timeout`.
async fn wait_until_dead(broadcaster: &Broadcaster, timeout: Duration) -> bool {
    let deadline = tokio::time::Instant::now() + timeout;
    while broadcaster.is_alive() {
        if tokio::time::Instant::now() >= deadline {
            return false;
        }
        tokio::time::sleep(Duration::from_millis(2)).await;
    }
    true
}

/// 1. Basic fanout: every message the ingester emits is delivered to a
/// subscribed viewer exactly once.
#[tokio::test]
async fn basic_fanout_delivers_all_messages() {
    let n = 4;
    let msgs: Vec<_> = (0..n).map(|_| ts_packet()).collect();
    let bc = Broadcaster::spawn(MockIngester::new(msgs), 1, 16);

    // Subscribe synchronously before awaiting so the broadcaster task hasn't
    // dropped early messages for lack of a receiver (current-thread runtime).
    let mut viewer = bc.subscribe().expect("subscribe should succeed while alive");

    assert!(
        wait_until_dead(&bc, Duration::from_secs(2)).await,
        "broadcaster should exit after draining the ingester"
    );

    // Drain the (still-open) ring buffer.
    let mut received = 0;
    loop {
        match viewer.try_recv() {
            Ok(Some(_)) => received += 1,
            Ok(None) => break,
            Err(lag) => panic!("viewer unexpectedly lagged by {}", lag),
        }
    }
    assert_eq!(received, n, "viewer should receive every emitted message");
}

/// 2. Viewer cap: `max_viewers == 2` admits two subscribers and rejects a
/// third with `None`.
#[tokio::test]
async fn viewer_cap_rejects_excess_subscribers() {
    // Use a never-ending ingester so the broadcaster stays alive for the
    // duration of the test (otherwise the empty stream trips end-of-stream).
    let bc = Broadcaster::spawn(MockIngester::new(vec![]).block_forever(), 2, 16);

    let v1 = bc.subscribe();
    let v2 = bc.subscribe();
    assert!(v1.is_some(), "first subscribe should succeed");
    assert!(v2.is_some(), "second subscribe should succeed");
    assert_eq!(bc.viewer_count(), 2);

    assert!(
        bc.subscribe().is_none(),
        "third subscribe should be rejected by the viewer cap"
    );
}

/// 3. Dead broadcaster: after the ingester returns `Ok(None)`, `is_alive()`
/// flips to false and further `subscribe()` calls return `None`.
#[tokio::test]
async fn dead_broadcaster_rejects_subscriptions() {
    let bc = Broadcaster::spawn(MockIngester::new(vec![]), 1, 16);

    assert!(
        wait_until_dead(&bc, Duration::from_secs(2)).await,
        "broadcaster should exit after end-of-stream"
    );
    assert!(!bc.is_alive(), "is_alive should be false after task exit");

    assert!(
        bc.subscribe().is_none(),
        "dead broadcaster should reject subscribe"
    );
}

/// 4. Lag handling: a slow viewer that never drains while the source pushes
/// `capacity + N` messages reports `Lagged` on its next `try_recv`.
#[tokio::test]
async fn slow_viewer_reports_lagged() {
    let capacity = 2;
    let pushed = 5;
    let msgs: Vec<_> = (0..pushed).map(|_| ts_packet()).collect();
    let bc = Broadcaster::spawn(MockIngester::new(msgs), 1, capacity);

    // Subscribe before the broadcaster task runs so the receiver's read
    // pointer starts at position 0 and every subsequent send overwrites it.
    let mut viewer = bc.subscribe().expect("subscribe should succeed");

    assert!(
        wait_until_dead(&bc, Duration::from_secs(2)).await,
        "broadcaster should exit after pushing all messages"
    );

    // Ring buffer holds only `capacity` of the `pushed` messages, so the
    // receiver is `pushed - capacity` behind.
    let lag = viewer
        .try_recv()
        .err()
        .expect("try_recv should report Lagged, not Ok");
    assert_eq!(
        lag,
        (pushed - capacity) as u64,
        "lag should equal (pushed - capacity)"
    );
}
