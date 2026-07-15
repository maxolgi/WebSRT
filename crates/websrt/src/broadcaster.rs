//! Multi-viewer fanout: a single Ingester feeds N browser sessions, each with
//! its own receiver. Lagging receivers (slow browsers) miss messages rather
//! than blocking the source.
//!
//! Phase 9: also surfaces a session cap (`max_viewers`).

use crate::ingest::{Ingester, TsMessage};
use anyhow::Result;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use tokio::sync::broadcast;

/// One viewer's subscription. Holds a `broadcast::Receiver`. Each browser
/// session owns one of these and polls it for messages to feed into its
/// SRT sender.
pub struct ViewerRx {
    rx: broadcast::Receiver<TsMessage>,
    lag_count: u64,
}

/// Wraps an Ingester in a many-reader pipeline. The source is read exactly
/// once; every `ViewerRx` gets its own copy of each message.
pub struct Broadcaster {
    tx: broadcast::Sender<TsMessage>,
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
        // broadcast::channel requires ≥1 receiver to exist; we drop it immediately
        // since each viewer creates its own via tx.subscribe().
        let (tx, _rx0) = broadcast::channel(capacity);
        let alive = Arc::new(AtomicBool::new(true));
        let alive_task = alive.clone();
        let broadcaster = Arc::new(Self {
            tx: tx.clone(),
            max_viewers,
            alive,
        });
        let tx2 = tx.clone();
        tokio::spawn(async move {
            let mut sent = 0u64;
            let mut last_sent = 0u64;
            let mut heartbeat_at = tokio::time::Instant::now() + std::time::Duration::from_secs(5);
            loop {
                let next_msg = ingester.next_message();
                tokio::pin!(next_msg);
                tokio::select! {
                    res = &mut next_msg => match res {
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
                    },
                    _ = tokio::time::sleep_until(heartbeat_at) => {
                        tracing::info!(
                            sent, delta = sent - last_sent,
                            rx_count = tx2.receiver_count(),
                            "broadcaster heartbeat"
                        );
                        last_sent = sent;
                        heartbeat_at = tokio::time::Instant::now() + std::time::Duration::from_secs(5);
                    }
                }
            }
            alive_task.store(false, Ordering::SeqCst);
            tracing::info!("broadcaster task exited");
        });
        broadcaster
    }

    /// Subscribe a new viewer. Returns `None` if the session cap is reached
    /// or the broadcaster is dead (source ended).
    pub fn subscribe(&self) -> Option<ViewerRx> {
        if !self.alive.load(Ordering::SeqCst) {
            return None;
        }
        if self.tx.receiver_count() >= self.max_viewers {
            return None;
        }
        Some(ViewerRx {
            rx: self.tx.subscribe(),
            lag_count: 0,
        })
    }

    pub fn is_alive(&self) -> bool {
        self.alive.load(Ordering::SeqCst)
    }

    pub fn viewer_count(&self) -> usize {
        self.tx.receiver_count()
    }
}

impl ViewerRx {
    /// Non-async try-receive: returns Ok(Some) if a message was immediately
    /// available, Ok(None) if empty, Err(n) if lagged `n` messages.
    pub fn try_recv(&mut self) -> Result<Option<TsMessage>, u64> {
        match self.rx.try_recv() {
            Ok(m) => Ok(Some(m)),
            Err(broadcast::error::TryRecvError::Empty) => Ok(None),
            Err(broadcast::error::TryRecvError::Lagged(n)) => {
                self.lag_count += n;
                Err(n)
            }
            Err(broadcast::error::TryRecvError::Closed) => Ok(None),
        }
    }

    /// Get the next TS message. Returns `None` only when the source has ended.
    pub async fn recv(&mut self) -> Result<Option<TsMessage>> {
        loop {
            match self.rx.recv().await {
                Ok(m) => return Ok(Some(m)),
                Err(broadcast::error::RecvError::Lagged(n)) => {
                    self.lag_count += n;
                    tracing::warn!(
                        lagged = n,
                        total_lag = self.lag_count,
                        "viewer lagged behind; dropped messages"
                    );
                    continue;
                }
                Err(broadcast::error::RecvError::Closed) => return Ok(None),
            }
        }
    }

    pub fn lag_count(&self) -> u64 {
        self.lag_count
    }
}
