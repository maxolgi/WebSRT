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
use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex as StdMutex, RwLock};
use std::time::Instant;
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
}

/// Central registry of active sessions, polled once per ~2ms by a single
/// ticker task spawned in [`crate::gateway::Gateway::run`].
pub(crate) struct SessionRegistry {
    entries: RwLock<HashMap<u64, Arc<SessionEntry>>>,
}

impl SessionRegistry {
    pub fn new() -> Self {
        Self {
            entries: RwLock::new(HashMap::new()),
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

        for entry in &entries {
            if entry.finished.load(Ordering::Relaxed) {
                to_remove.push(entry.session_id);
                continue;
            }

            // Lock the initiator, drive the SRT state machine, and push any
            // viewer messages that became available. The viewer std Mutex is
            // released before each `push_message` call (push is sync but we
            // keep the critical section tight).
            let (actions, data) = {
                let mut init = entry.initiator.lock().await;
                let (mut actions, mut data) = init.tick(now);
                if init.is_connected() {
                    for _ in 0..MAX_MSGS_PER_TICK {
                        let maybe_msg = {
                            let mut viewer = entry.viewer.lock().unwrap();
                            // Publish-only session (None) has nothing to drain.
                            viewer.as_mut().map(|v| v.try_recv())
                        };
                        match maybe_msg {
                            Some(Ok(Some(m))) => {
                                // Refresh `now` per push so each packet gets a
                                // strictly monotonic timestamp. Reusing the
                                // tick's `now` for all 32 pushes would stamp
                                // them identically, breaking TSBPD spacing.
                                let now = Instant::now();
                                let (a, d) = init.push_message(m, now);
                                actions.extend(a);
                                data.extend(d);
                            }
                            Some(Ok(None)) => break,
                            Some(Err(lag)) => {
                                tracing::warn!(
                                    session_id = entry.session_id,
                                    lag,
                                    "viewer lagged; messages dropped"
                                );
                                break;
                            }
                            None => break,
                        }
                    }
                }
                (actions, data)
            };

            // Route released upstream data (browser→gateway) to the publish channel.
            for (ts, bytes) in data {
                route_release_data(entry, ts, &bytes);
            }

            // Send the actions out via WebTransport. The loss injector is
            // shared with the recv_pump; the tokio::sync::Mutex serializes
            // access. `send_action` errors (e.g. WT connection closed) are
            // ignored here — the recv_pump will observe the failure and mark
            // the session finished.
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

        for id in to_remove {
            self.remove(id);
        }
    }
}
