//! Centralized session registry + ticker: one task drives all sessions' SRT state machines.
//!
//! One ticker task drives all sessions' SRT state machines, eliminating
//! N separate 2ms interval timers (at 500 viewers that's ~250k timer wakeups/s
//! avoided). Per-session `recv_pump` tasks remain — they're cheap, blocking
//! on WT datagram receive.
//!
//! Lock strategy:
//! - `entries` lives behind a `std::sync::RwLock` — insert/remove from the
//!   accept loop, snapshot from the ticker. The lock is held only long enough
//!   to clone the `Arc<SessionEntry>` values; never across `.await`.
//! - `entry.viewer` uses `std::sync::Mutex` — it's touched only at known sync
//!   points inside `tick_all` and never held across `.await`.
//! - `entry.initiator` and `entry.loss` use `tokio::sync::Mutex` — they're
//!   shared between the ticker and the recv_pump and may be held across
//!   `.await`. Both lockers always acquire them in the same order
//!   (initiator → loss) so there is no deadlock cycle.

use crate::broadcaster::ViewerRx;
use crate::ingest::TsMessage;
use crate::session::{route_release_data, send_action, LossInjector};
use crate::srt_sender::SrtInitiator;
use srt_protocol::statistics::SocketStatistics;
use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex as StdMutex, RwLock};
use std::time::{Duration, Instant};
use tokio::sync::{Mutex, Notify};
use wtransport::Connection;

/// Per-tick cap on viewer messages drained into a session's sender. Prevents
/// one fast session from starving others under bulk backlog.
const MAX_MSGS_PER_TICK: usize = 32;

/// All per-session state shared between the recv_pump task and the centralized
/// ticker. Held inside `Arc` so both can reference it concurrently.
pub(crate) struct SessionEntry {
    pub conn: Connection,
    pub initiator: Arc<Mutex<SrtInitiator>>,
    pub loss: Arc<Mutex<LossInjector>>,
    /// Ticker-exclusive; std::sync::Mutex is sufficient because it is never
    /// held across an `.await` (locked only for `try_recv`). `None` for
    /// publish-only sessions that have no downstream to drain.
    pub viewer: StdMutex<Option<ViewerRx>>,
    pub session_id: u64,
    pub shutdown: Arc<Notify>,
    pub finished: AtomicBool,
    /// Channel for routing upstream data (browser→gateway) to a broadcaster.
    /// None for viewer-only sessions.
    pub publish_tx: Option<tokio::sync::mpsc::Sender<TsMessage>>,
    /// Diagnostic counters, updated by the ticker.
    pub messages_pushed: AtomicU64,
    pub viewer_lag_count: AtomicU64,
    /// Last SRT stats snapshot, refreshed each tick for periodic logging.
    pub last_srt_stats: StdMutex<Option<SocketStatistics>>,
}

/// Central registry of active sessions, polled once per ~2ms by a single
/// ticker task spawned in [`crate::gateway::Gateway::run`].
pub(crate) struct SessionRegistry {
    entries: RwLock<HashMap<u64, Arc<SessionEntry>>>,
    last_stats_log: StdMutex<Option<Instant>>,
}

impl SessionRegistry {
    pub fn new() -> Self {
        Self {
            entries: RwLock::new(HashMap::new()),
            last_stats_log: StdMutex::new(None),
        }
    }

    /// Insert a session entry. The key is `entry.session_id` (assigned by
    /// `BrowserSession::create` via the global session counter).
    /// Returns the session_id.
    pub fn insert(&self, entry: Arc<SessionEntry>) -> u64 {
        let id = entry.session_id;
        let mut w = self.entries.write().unwrap();
        w.insert(id, entry);
        id
    }

    pub(crate) fn remove(&self, session_id: u64) {
        let mut w = self.entries.write().unwrap();
        w.remove(&session_id);
    }

    /// Snapshot all active session entries. Used by the ticker for iteration
    /// and by the periodic stats logger.
    pub(crate) fn snapshot(&self) -> Vec<Arc<SessionEntry>> {
        self.entries.read().unwrap().values().cloned().collect()
    }

    /// Drive every active session's SRT state machine once. Called by the
    /// single ticker task approximately every 2ms.
    pub(crate) async fn tick_all(&self) {
        let now = Instant::now();
        let entries = self.snapshot();
        let mut to_remove: Vec<u64> = Vec::new();

        let should_log_stats = {
            let mut last = self.last_stats_log.lock().unwrap();
            let should = last.map(|t| now.duration_since(t) >= Duration::from_secs(5)).unwrap_or(true);
            if should { *last = Some(now); }
            should
        };

        for entry in &entries {
            if entry.finished.load(Ordering::Relaxed) {
                to_remove.push(entry.session_id);
                continue;
            }

            let (actions, data) = {
                let mut init = entry.initiator.lock().await;
                let (mut actions, mut data) = init.tick(now);
                if init.is_connected() {
                    for _ in 0..MAX_MSGS_PER_TICK {
                        let maybe_msg = {
                            let mut viewer = entry.viewer.lock().unwrap();
                            viewer.as_mut().map(|v| v.try_recv())
                        };
                        match maybe_msg {
                            Some(Ok(Some(m))) => {
                                let now = Instant::now();
                                let (a, d) = init.push_message(m, now);
                                actions.extend(a);
                                data.extend(d);
                                entry.messages_pushed.fetch_add(1, Ordering::Relaxed);
                            }
                            Some(Ok(None)) => break,
                            Some(Err(lag)) => {
                                tracing::warn!(
                                    session_id = entry.session_id,
                                    lag,
                                    "viewer lagged; messages dropped"
                                );
                                entry.viewer_lag_count.fetch_add(1, Ordering::Relaxed);
                                break;
                            }
                            None => break,
                        }
                    }
                }
                if let Some(s) = init.stats() {
                    *entry.last_srt_stats.lock().unwrap() = Some(s.clone());
                }
                (actions, data)
            };

            for (ts, bytes) in data {
                route_release_data(entry, ts, &bytes);
            }

            {
                let mut loss = entry.loss.lock().await;
                for action in actions {
                    if matches!(action, crate::srt_sender::SenderAction::Close) {
                        entry.finished.store(true, Ordering::Relaxed);
                        entry.shutdown.notify_waiters();
                    }
                    let _ = send_action(&entry.conn, action, &mut loss);
                }
            }

            if entry.finished.load(Ordering::Relaxed) {
                to_remove.push(entry.session_id);
            }
        }

        if should_log_stats {
            for entry in &entries {
                if entry.finished.load(Ordering::Relaxed) { continue; }
                let guard = entry.last_srt_stats.lock().unwrap();
                if let Some(s) = guard.as_ref() {
                    tracing::info!(
                        session_id = entry.session_id,
                        tx_data = s.tx_data,
                        tx_retransmit = s.tx_retransmit_data,
                        tx_loss = s.tx_loss_data,
                        tx_buffered = s.tx_buffered_data,
                        rx_rtt = ?s.rx_average_rtt,
                        tx_rtt = ?s.tx_average_rtt,
                        messages_pushed = entry.messages_pushed.load(Ordering::Relaxed),
                        viewer_lag = entry.viewer_lag_count.load(Ordering::Relaxed),
                        "per-session SRT stats"
                    );
                }
            }
        }

        for id in to_remove {
            self.remove(id);
        }
    }
}
