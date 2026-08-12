//! Multi-viewer fanout: a single Ingester feeds N browser sessions, each with
//! its own receiver. Lagging receivers (slow browsers) miss messages rather
//! than blocking the source.
//!
//! Also surfaces a session cap (`max_viewers`).

use crate::ingest::{Ingester, TsMessage};
use anyhow::Result;
use parking_lot::Mutex;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Instant;
use tokio::sync::broadcast;
use tokio::sync::Notify;

/// One viewer's subscription. Holds a `broadcast::Receiver`. Each browser
/// session owns one of these and polls it for messages to feed into its
/// SRT sender.
pub struct ViewerRx {
    rx: broadcast::Receiver<TsMessage>,
}

/// Wraps an Ingester in a many-reader pipeline. The source is read exactly
/// once; every `ViewerRx` gets its own copy of each message.
///
/// `tx` is held behind a `Mutex<Option<_>>` so the background task can drop
/// the sender when the source ends, closing the channel so viewers attached
/// to a dead source observe `Closed` on their next `try_recv()` instead of
/// hanging until the SRT idle timeout fires.
pub struct Broadcaster {
    tx: Mutex<Option<broadcast::Sender<TsMessage>>>,
    /// Maximum viewers; enforced by `subscribe()`.
    pub max_viewers: usize,
    alive: Arc<AtomicBool>,
    /// Number of messages pulled from the source and offered to the broadcast
    /// channel (i.e. ingester attempts, regardless of receiver count).
    messages_sent: AtomicU64,
    /// Number of offered messages that had no active receiver (dropped).
    send_failures: AtomicU64,
    /// Cumulative payload bytes pulled from the source and offered to the
    /// broadcast channel. Sampled by `measured_bitrate_bps` to derive a fixed
    /// send rate for `LiveBandwidthMode::Max`, avoiding the feedback loop that
    /// `LiveBandwidthMode::Estimated` creates with the batchy gateway input.
    bytes_sent: AtomicU64,
    /// `(last_sample_time, last_total_bytes)` — baseline for the next EWMA
    /// sample. Updated lazily on each `measured_bitrate_bps` call.
    rate_sample: Mutex<(Instant, u64)>,
    /// Current EWMA of the stream bitrate, in bytes/sec.
    ewma_rate_bps: AtomicU64,
    /// Shutdown signal. `notify_one()` on this causes the background task's
    /// `select!` to fire and the task to exit cleanly.
    shutdown: Arc<Notify>,
    task_handle: Mutex<Option<tokio::task::JoinHandle<()>>>,
}

impl Broadcaster {
    /// Spawn the broadcaster. `capacity` is the broadcast ring-buffer depth;
    /// larger values absorb viewer-side latency spikes but cost memory.
    ///
    /// `shutdown` is a `Notify` the caller retains a clone of; calling
    /// `notify_one()` on it (or [`Broadcaster::shutdown`]) causes the
    /// background task to exit promptly, even if the ingester is stuck in an
    /// infinite reconnect loop.
    pub fn spawn<I>(
        mut ingester: I,
        max_viewers: usize,
        capacity: usize,
        shutdown: Arc<Notify>,
    ) -> Arc<Self>
    where
        I: Ingester + Send + 'static,
    {
        let tx = broadcast::channel(capacity).0;
        let alive = Arc::new(AtomicBool::new(true));
        let alive_task = alive.clone();
        let broadcaster = Arc::new(Self {
            tx: Mutex::new(Some(tx.clone())),
            max_viewers,
            alive,
            messages_sent: AtomicU64::new(0),
            send_failures: AtomicU64::new(0),
            bytes_sent: AtomicU64::new(0),
            rate_sample: Mutex::new((Instant::now(), 0)),
            ewma_rate_bps: AtomicU64::new(0),
            shutdown: shutdown.clone(),
            task_handle: Mutex::new(None),
        });
        let bc_clone = broadcaster.clone();
        let tx2 = tx.clone();
        let shutdown_notify = shutdown.clone();
        let handle = tokio::spawn(async move {
            let mut sent = 0u64;
            loop {
                tokio::select! {
                    biased;
                    _ = shutdown_notify.notified() => {
                        tracing::info!("broadcaster shutdown signal received");
                        break;
                    }
                    msg = ingester.next_message() => {
                        match msg {
                            Ok(Some(msg)) => {
                                sent += 1;
                                bc_clone.messages_sent.fetch_add(1, Ordering::Relaxed);
                                bc_clone
                                    .bytes_sent
                                    .fetch_add(msg.1.len() as u64, Ordering::Relaxed);
                                if tx2.send(msg).is_err() {
                                    if tx2.receiver_count() > 0 {
                                        bc_clone.send_failures.fetch_add(1, Ordering::Relaxed);
                                    }
                                    tracing::trace!(
                                        rx_count = tx2.receiver_count(),
                                        "broadcast send skipped (no active receivers)"
                                    );
                                }
                            }
                            Ok(None) => {
                                tracing::info!("ingester source ended; broadcaster shutting down");
                                break;
                            }
                            Err(e) => {
                                tracing::warn!(?e, "ingester error");
                                tokio::time::sleep(std::time::Duration::from_millis(100)).await;
                            }
                        }
                    }
                }
            }
            alive_task.store(false, Ordering::SeqCst);
            // Drop the task's Sender clone, then clear the Broadcaster's copy so
            // the broadcast channel closes and every ViewerRx::try_recv()
            // returns Ok(None) instead of hanging until the SRT idle timeout.
            drop(tx2);
            *bc_clone.tx.lock() = None;
            tracing::info!(sent, "broadcaster task exited");
        });
        *broadcaster.task_handle.lock() = Some(handle);
        broadcaster
    }

    /// Signal the background task to shut down. Returns immediately; the task
    /// sets `alive = false` and exits on its next `select!` poll.
    pub fn shutdown(&self) {
        self.shutdown.notify_one();
    }

    /// Await the background task's completion. Best-effort: gives up after 2s.
    /// `shutdown()` must be called first (or the source must have ended);
    /// otherwise this will hit the timeout.
    pub async fn join(&self) {
        let handle = self.task_handle.lock().take();
        if let Some(h) = handle {
            if tokio::time::timeout(std::time::Duration::from_secs(2), h)
                .await
                .is_err()
            {
                tracing::warn!("broadcaster task did not exit within 2s");
            }
        }
    }

    /// Subscribe a new viewer. Returns `None` if the session cap is reached
    /// or the broadcaster is dead (source ended).
    pub fn subscribe(&self) -> Option<ViewerRx> {
        if !self.alive.load(Ordering::SeqCst) {
            return None;
        }
        let guard = self.tx.lock();
        let tx = guard.as_ref()?;
        if tx.receiver_count() >= self.max_viewers {
            return None;
        }
        Some(ViewerRx { rx: tx.subscribe() })
    }

    pub fn is_alive(&self) -> bool {
        self.alive.load(Ordering::SeqCst)
    }

    pub fn viewer_count(&self) -> usize {
        self.tx
            .lock()
            .as_ref()
            .map(|t| t.receiver_count())
            .unwrap_or(0)
    }

    /// Messages pulled from the ingester and offered to the broadcast channel.
    pub fn messages_sent(&self) -> u64 {
        self.messages_sent.load(Ordering::Relaxed)
    }

    /// Offered messages dropped because no viewer was subscribed.
    pub fn send_failures(&self) -> u64 {
        self.send_failures.load(Ordering::Relaxed)
    }

    /// Measured stream bitrate in bytes/sec (lazy-sampled EWMA). Returns 0
    /// until at least one 2-second sample window has elapsed since spawn, so a
    /// freshly started stream reports no measurement. The rate is refreshed on
    /// read: if 2+ seconds have passed since the last sample, a new point is
    /// folded into the EWMA (`ewma = ewma*9/10 + instant*1/10`); the first
    /// non-zero sample seeds the EWMA directly.
    pub fn measured_bitrate_bps(&self) -> u64 {
        let now = Instant::now();
        let mut sample = self.rate_sample.lock();
        let elapsed = now.duration_since(sample.0);
        if elapsed >= std::time::Duration::from_secs(2) {
            let total = self.bytes_sent.load(Ordering::Relaxed);
            let delta_bytes = total.saturating_sub(sample.1);
            let elapsed_ms = elapsed.as_millis() as u64;
            let instant_rate = if elapsed_ms > 0 {
                delta_bytes * 1000 / elapsed_ms
            } else {
                0
            };
            let prev = self.ewma_rate_bps.load(Ordering::Relaxed);
            let new_ewma = if prev == 0 {
                instant_rate
            } else {
                (prev * 9 + instant_rate) / 10
            };
            self.ewma_rate_bps.store(new_ewma, Ordering::Relaxed);
            *sample = (now, total);
            new_ewma
        } else {
            self.ewma_rate_bps.load(Ordering::Relaxed)
        }
    }
}

impl ViewerRx {
    /// Non-async try-receive: returns Ok(Some) if a message was immediately
    /// available, Ok(None) if empty, Err(n) if lagged `n` messages.
    pub fn try_recv(&mut self) -> Result<Option<TsMessage>, u64> {
        match self.rx.try_recv() {
            Ok(m) => Ok(Some(m)),
            Err(broadcast::error::TryRecvError::Empty) => Ok(None),
            Err(broadcast::error::TryRecvError::Lagged(n)) => Err(n),
            Err(broadcast::error::TryRecvError::Closed) => Ok(None),
        }
    }
}
