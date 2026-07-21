//! Multi-viewer fanout: a single Ingester feeds N browser sessions, each with
//! its own receiver. Lagging receivers (slow browsers) miss messages rather
//! than blocking the source.
//!
//! Also surfaces a session cap (`max_viewers`).

use crate::ingest::{Ingester, TsMessage};
use anyhow::Result;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use tokio::sync::broadcast;

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
}

impl Broadcaster {
    /// Spawn the broadcaster. `capacity` is the broadcast ring-buffer depth;
    /// larger values absorb viewer-side latency spikes but cost memory.
    pub fn spawn<I>(mut ingester: I, max_viewers: usize, capacity: usize) -> Arc<Self>
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
        });
        let bc_clone = broadcaster.clone();
        let tx2 = tx.clone();
        tokio::spawn(async move {
            let mut sent = 0u64;
            loop {
                match ingester.next_message().await {
                    Ok(Some(msg)) => {
                        sent += 1;
                        if tx2.send(msg).is_err() {
                            tracing::debug!(
                                rx_count = tx2.receiver_count(),
                                "broadcast send failed (no active receivers)"
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
            alive_task.store(false, Ordering::SeqCst);
            // Drop the task's Sender clone, then clear the Broadcaster's copy so
            // the broadcast channel closes and every ViewerRx::try_recv()
            // returns Ok(None) instead of hanging until the SRT idle timeout.
            drop(tx2);
            *bc_clone.tx.lock().unwrap() = None;
            tracing::info!(sent, "broadcaster task exited");
        });
        broadcaster
    }

    /// Subscribe a new viewer. Returns `None` if the session cap is reached
    /// or the broadcaster is dead (source ended).
    pub fn subscribe(&self) -> Option<ViewerRx> {
        if !self.alive.load(Ordering::SeqCst) {
            return None;
        }
        let guard = self.tx.lock().unwrap();
        let tx = guard.as_ref()?;
        if tx.receiver_count() >= self.max_viewers {
            return None;
        }
        Some(ViewerRx {
            rx: tx.subscribe(),
        })
    }

    pub fn is_alive(&self) -> bool {
        self.alive.load(Ordering::SeqCst)
    }

    pub fn viewer_count(&self) -> usize {
        self.tx
            .lock()
            .unwrap()
            .as_ref()
            .map(|t| t.receiver_count())
            .unwrap_or(0)
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
