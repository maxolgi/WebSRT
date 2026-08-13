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
use std::time::{Duration, Instant};
use tokio::sync::broadcast;
use tokio::sync::Notify;

/// How often the EWMA message-rate sampler refreshes from `messages_sent`.
const RATE_SAMPLE_INTERVAL: Duration = Duration::from_secs(1);
/// Per-tick cap returned when the stream hasn't been measured yet.
const RATE_DEFAULT_CAP: usize = 32;
/// Extra messages per tick beyond the measured rate, for slow catch-up.
const RATE_OVERHEAD: usize = 2;

/// Lazy-sampled EWMA of a stream's message rate (messages/sec). Sampled
/// on read from the atomic `messages_sent` counter every
/// [`RATE_SAMPLE_INTERVAL`]. A 90/10 EWMA keeps the estimate stable against
/// short bursts.
struct RateSampler {
    last_sample: Instant,
    last_count: u64,
    ewma_msg_per_sec: f64,
}

impl RateSampler {
    fn new() -> Self {
        Self {
            last_sample: Instant::now(),
            last_count: 0,
            ewma_msg_per_sec: 0.0,
        }
    }
}

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
    /// Shutdown signal. `notify_one()` on this causes the background task's
    /// `select!` to fire and the task to exit cleanly.
    shutdown: Arc<Notify>,
    task_handle: Mutex<Option<tokio::task::JoinHandle<()>>>,
    rate_sampler: Mutex<RateSampler>,
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
            shutdown: shutdown.clone(),
            task_handle: Mutex::new(None),
            rate_sampler: Mutex::new(RateSampler::new()),
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

    /// Estimated per-tick message cap for smooth drain. Samples
    /// [`messages_sent`](Self::messages_sent) every [`RATE_SAMPLE_INTERVAL`]
    /// and maintains a 90/10 EWMA. Returns `ceil(ewma / ticks_per_sec) + 2`,
    /// or [`RATE_DEFAULT_CAP`] when no measurement exists yet.
    pub fn msg_rate_per_tick(&self, ticks_per_sec: u32) -> usize {
        let mut sampler = self.rate_sampler.lock();
        let now = Instant::now();
        if now.duration_since(sampler.last_sample) >= RATE_SAMPLE_INTERVAL {
            let current = self.messages_sent.load(Ordering::Relaxed);
            let elapsed = now.duration_since(sampler.last_sample).as_secs_f64();
            if elapsed > 0.0 {
                let delta = current.saturating_sub(sampler.last_count);
                let instant_rate = delta as f64 / elapsed;
                if sampler.ewma_msg_per_sec == 0.0 {
                    sampler.ewma_msg_per_sec = instant_rate;
                } else {
                    sampler.ewma_msg_per_sec = 0.9 * sampler.ewma_msg_per_sec + 0.1 * instant_rate;
                }
            }
            sampler.last_sample = now;
            sampler.last_count = current;
        }
        if sampler.ewma_msg_per_sec <= 0.0 || ticks_per_sec == 0 {
            return RATE_DEFAULT_CAP;
        }
        let per_tick = (sampler.ewma_msg_per_sec / ticks_per_sec as f64).ceil() as usize;
        per_tick.saturating_add(RATE_OVERHEAD).max(1)
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

    /// Drain and discard all immediately-available messages. Called once
    /// when a viewer first connects so it starts from the live edge instead
    /// of replaying messages that accumulated during the SRT handshake —
    /// those are already past the TSBPD deadline and delivering them only
    /// triggers NAK/retransmit churn.
    pub fn drop_backlog(&mut self) {
        loop {
            match self.try_recv() {
                Ok(Some(_)) | Err(_) => continue,
                Ok(None) => break,
            }
        }
    }
}
