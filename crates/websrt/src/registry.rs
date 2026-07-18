//! Centralized session registry + ticker: replaces per-session sender_pump tasks.
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
//! - `entry.viewer` and `entry.next_deadline` use `std::sync::Mutex` — they're
//!   touched only at known sync points inside `tick_all` and never held across
//!   `.await`.
//! - `entry.initiator` and `entry.loss` use `tokio::sync::Mutex` — they're
//!   shared between the ticker and the recv_pump and may be held across
//!   `.await`. Both lockers always acquire them in the same order
//!   (initiator → loss) so there is no deadlock cycle.

use crate::broadcaster::ViewerRx;
use crate::ingest::TsMessage;
use crate::session::{send_action, LossInjector};
use crate::srt_sender::SrtInitiator;
use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex as StdMutex, RwLock};
use std::time::{Duration, Instant};
use tokio::sync::{Mutex, Notify};
use wtransport::Connection;

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
    /// Next time this session's SRT state machine needs servicing, set by
    /// the ticker from the `WaitForData` duration returned by `tick()`.
    pub next_deadline: StdMutex<Instant>,
    pub session_id: u64,
    pub shutdown: Arc<Notify>,
    pub finished: AtomicBool,
    /// TSBPD latency (ms) — used for aggregate WT-RTT warnings in the ticker.
    pub latency_ms: u64,
    /// Channel for routing upstream data (browser→gateway) to a broadcaster.
    /// None for viewer-only sessions.
    pub publish_tx: Option<tokio::sync::mpsc::Sender<TsMessage>>,
    /// Publish-path: ReleaseData arrived in recv_pump but try_send to the
    /// publish mpsc failed (channel full). Increments forever; logged every
    /// 5s in the ticker stats line. 0 on viewer-only sessions.
    pub publish_try_send_drops: AtomicU64,
    /// Publish-path: ReleaseData arrived in tick_all but try_send failed.
    /// Same semantics as above, but for the (rarer) tick-driven release path.
    pub publish_tick_try_send_drops: AtomicU64,
    /// Publish-path: total ReleaseData events observed across both paths
    /// (drops + successes). Lets us compute a drop rate in the log line.
    pub publish_release_total: AtomicU64,
    /// Publish-path: total bytes released by ReleaseData. Lets us compute
    /// the actual ingest byte rate independent of message count.
    pub publish_bytes_total: AtomicU64,
    /// Viewer-path: total bytes pushed to this session's SrtInitiator via
    /// push_message. Lets us compare publish byte rate vs viewer push rate.
    pub viewer_pushed_bytes: AtomicU64,
    /// Viewer-path: total SendDatagram actions emitted to this session's
    /// WT connection. Lets us compare push rate vs actual datagram sends.
    pub viewer_sent_datagrams: AtomicU64,
    /// Drain loop returned WaitForData this many times (across recv_pump + tick_all).
    pub drain_wait_count: AtomicU64,
    /// Last WaitForData duration (microseconds) from drain.
    pub drain_wait_last_us: AtomicU64,
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

    pub fn remove(&self, session_id: u64) {
        let mut w = self.entries.write().unwrap();
        w.remove(&session_id);
    }

    /// Number of active sessions. Used by the ticker's periodic stats logger;
    /// cheaper than `snapshot().len()`.
    pub fn len(&self) -> usize {
        self.entries.read().unwrap().len()
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

            // Fast path: skip sessions whose deadline hasn't arrived AND have
            // no pending viewer data. If try_recv does return a message, we
            // MUST hold onto it — consuming-and-dropping it here would lose
            // TS packets and corrupt the video stream.
            //
            // Publish sessions are exempt: their SRT receiver holds incoming
            // data in a TSBPD buffer that drains on each tick(). Skipping
            // based on next_deadline starves the drain, causing data to pile
            // up and eventually be released too late (or not at all). The
            // tick() call is cheap when there's nothing to release.
            let is_publish = entry.publish_tx.is_some();
            let mut fast_path_msg = {
                let next = *entry.next_deadline.lock().unwrap();
                if now >= next || is_publish {
                    // Deadline arrived (or publish session — always service).
                    // Don't consume a message here; the drain loop below
                    // handles it.
                    None
                } else {
                    // Check for new data without losing it.
                    let mut v = entry.viewer.lock().unwrap();
                    match v.as_mut() {
                        Some(vx) => match vx.try_recv() {
                            Ok(Some(m)) => Some(m),
                            _ => None,
                        },
                        // Publish-only session: no downstream data to push.
                        None => None,
                    }
                }
            };
            if fast_path_msg.is_none() && !is_publish && now < *entry.next_deadline.lock().unwrap() {
                continue;
            }

            // Lock the initiator, drive the SRT state machine, and push any
            // viewer messages that became available. The viewer std Mutex is
            // released before each `push_message` call (push is sync but we
            // keep the critical section tight).
            let (actions, wait_dur) = {
                let mut init = entry.initiator.lock().await;
                let (mut actions, mut wait) = init.tick(now);

                // Push the message captured by the fast-path check (if any)
                // BEFORE draining the rest, to preserve arrival order.
                if let Some(m) = fast_path_msg.take() {
                    if init.is_connected() {
                        entry.viewer_pushed_bytes.fetch_add(m.1.len() as u64, Ordering::Relaxed);
                        let (a, w) = init.push_message(m, now);
                        actions.extend(a);
                        wait = w;
                    }
                }

                if init.is_connected() {
                    let mut drained = 0u32;
                    loop {
                        if drained >= 32 {
                            break;
                        }
                        let maybe_msg = {
                            let mut viewer = entry.viewer.lock().unwrap();
                            // Publish-only session (None) has nothing to drain.
                            viewer.as_mut().map(|v| v.try_recv())
                        };
                        match maybe_msg {
                            Some(Ok(Some(m))) => {
                                entry.viewer_pushed_bytes.fetch_add(m.1.len() as u64, Ordering::Relaxed);
                                let (a, w) = init.push_message(m, now);
                                actions.extend(a);
                                wait = w;
                                drained += 1;
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

                let dur = if init.is_closed() {
                    entry.finished.store(true, Ordering::Relaxed);
                    entry.shutdown.notify_waiters();
                    Duration::from_millis(2)
                } else {
                    wait
                };

                (actions, dur)
            };

            // Schedule the next service point.
            *entry.next_deadline.lock().unwrap() = now + wait_dur;

            // Send the actions out via WebTransport. The loss injector is
            // shared with the recv_pump; the tokio::sync::Mutex serializes
            // access. `send_action` errors (e.g. WT connection closed) are
            // ignored here — the recv_pump will observe the failure and mark
            // the session finished.
            {
                let mut loss = entry.loss.lock().await;
                for action in actions {
                    match &action {
                        crate::srt_sender::SenderAction::ReleaseData((ts, bytes)) => {
                            if let Some(tx) = &entry.publish_tx {
                                entry.publish_release_total.fetch_add(1, Ordering::Relaxed);
                                entry.publish_bytes_total.fetch_add(bytes.len() as u64, Ordering::Relaxed);
                                if tx.try_send((*ts, bytes.clone())).is_err() {
                                    entry.publish_tick_try_send_drops.fetch_add(1, Ordering::Relaxed);
                                    tracing::debug!(
                                        session_id = entry.session_id,
                                        queued = tx.capacity(),
                                        "publish_tx try_send drop in tick_all"
                                    );
                                }
                            }
                            continue;
                        }
                        crate::srt_sender::SenderAction::SendDatagram(_) => {
                            entry.viewer_sent_datagrams.fetch_add(1, Ordering::Relaxed);
                        }
                        crate::srt_sender::SenderAction::DrainWait(d) => {
                            entry.drain_wait_count.fetch_add(1, Ordering::Relaxed);
                            entry.drain_wait_last_us.store(d.as_micros() as u64, Ordering::Relaxed);
                            continue;
                        }
                        crate::srt_sender::SenderAction::Close => {
                            entry.finished.store(true, Ordering::Relaxed);
                            entry.shutdown.notify_waiters();
                        }
                        _ => {}
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

impl Default for SessionRegistry {
    fn default() -> Self {
        Self::new()
    }
}
