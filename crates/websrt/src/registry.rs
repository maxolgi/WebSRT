//! Centralized session registry + ticker: one task drives all sessions' SRT state machines.
//!
//! One ticker task drives all sessions' SRT state machines, eliminating
//! N separate 2ms interval timers (at 500 viewers that's ~250k timer wakeups/s
//! avoided). Per-session `recv_pump` tasks remain — they're cheap, blocking
//! on WT datagram receive.
//!
//! Lock strategy:
//! - `entries` lives behind a `parking_lot::RwLock` — insert/remove from the
//!   accept loop, snapshot from the ticker. The lock is held only long enough
//!   to clone the `Arc<SessionEntry>` values; never across `.await`.
//! - `entry.viewer` uses `parking_lot::Mutex` — it's touched only at known sync
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
use std::sync::Arc;
use std::time::{Duration, Instant};
use parking_lot::{Mutex as StdMutex, RwLock};
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
    /// Ticker-exclusive; parking_lot::Mutex is sufficient because it is never
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
    /// Upstream data drops when the publish channel is full. Incremented by
    /// `route_release_data` on `try_send` failure.
    pub publish_dropped: AtomicU64,
    /// Last SRT stats snapshot, refreshed each tick for periodic logging.
    pub last_srt_stats: StdMutex<Option<SocketStatistics>>,
    /// RAII guard for connection limiting. Drops when the session exits,
    /// releasing the per-IP and global session slot. Field is never read —
    /// its purpose is to be held and dropped (like `SrtIngester.kind`).
    #[allow(dead_code)]
    pub guard: Option<crate::limits::SessionGuard>,
    /// Peer's remote address (for stats reporting).
    pub peer: std::net::SocketAddr,
    /// Stream name this session is subscribed to or publishing (for stats).
    pub stream_name: String,
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
        let mut w = self.entries.write();
        w.insert(id, entry);
        id
    }

    pub(crate) fn remove(&self, session_id: u64) {
        let mut w = self.entries.write();
        w.remove(&session_id);
    }

    pub(crate) fn clear(&self) {
        self.entries.write().clear();
    }

    /// Snapshot all active session entries. Used by the ticker for iteration
    /// and by the periodic stats logger.
    pub(crate) fn snapshot(&self) -> Vec<Arc<SessionEntry>> {
        self.entries.read().values().cloned().collect()
    }

    /// Snapshot all active sessions' stats for health/metrics reporting.
    pub(crate) fn snapshot_sessions(&self) -> Vec<crate::gateway::SessionStats> {
        let entries = self.snapshot();
        entries
            .iter()
            .filter(|e| !e.finished.load(Ordering::Relaxed))
            .map(|e| crate::gateway::SessionStats {
                session_id: e.session_id,
                peer: e.peer,
                stream_name: e.stream_name.clone(),
                messages_pushed: e.messages_pushed.load(Ordering::Relaxed),
                viewer_lag_count: e.viewer_lag_count.load(Ordering::Relaxed),
                publish_dropped: e.publish_dropped.load(Ordering::Relaxed),
                srt: e.last_srt_stats.lock().as_ref().map(|s| {
                    crate::gateway::SrtStatsSnapshot {
                        tx_data: s.tx_data,
                        tx_retransmit: s.tx_retransmit_data,
                        tx_loss: s.tx_loss_data,
                        tx_buffered: s.tx_buffered_data,
                        rx_rtt: Some(s.rx_average_rtt),
                        tx_rtt: Some(s.tx_average_rtt),
                    }
                }),
            })
            .collect()
    }

    /// Number of active (non-finished) sessions.
    pub(crate) fn active_session_count(&self) -> usize {
        self.entries.read().values()
            .filter(|e| !e.finished.load(Ordering::Relaxed))
            .count()
    }

    /// Drive every active session's SRT state machine once. Called by the
    /// single ticker task approximately every 2ms.
    pub(crate) async fn tick_all(&self) {
        let now = Instant::now();
        let entries = self.snapshot();
        let mut to_remove: Vec<u64> = Vec::new();

        let should_log_stats = {
            let mut last = self.last_stats_log.lock();
            let should = last.map(|t| now.duration_since(t) >= Duration::from_secs(5)).unwrap_or(true);
            if should { *last = Some(now); }
            should
        };

        for (idx, entry) in entries.iter().enumerate() {
            if idx > 0 && idx % 32 == 0 {
                tokio::task::yield_now().await;
            }
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
                            let mut viewer = entry.viewer.lock();
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
                    *entry.last_srt_stats.lock() = Some(s.clone());
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
                let guard = entry.last_srt_stats.lock();
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

        let elapsed = now.elapsed();
        if elapsed > Duration::from_millis(2) {
            tracing::warn!(
                duration_us = elapsed.as_micros() as u64,
                sessions = entries.len(),
                "tick_all exceeded 2ms budget"
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::srt_sender::SrtConfig;
    use bytes::Bytes;
    use std::net::{IpAddr, Ipv4Addr, SocketAddr};
    use wtransport::tls::Sha256Digest;
    use wtransport::{ClientConfig, Endpoint, Identity, ServerConfig};

    const LOCAL: IpAddr = IpAddr::V4(Ipv4Addr::LOCALHOST);
    const DUMMY_REMOTE: SocketAddr = SocketAddr::new(LOCAL, 9000);

    /// Server-side endpoint type alias (avoids spelling the generic param at
    /// every helper signature).
    type ServerEndpoint = Endpoint<wtransport::endpoint::endpoint_side::Server>;

    /// Boot a self-signed WT server bound to an ephemeral loopback port.
    /// Returns `(server, bound_addr, cert_hash_for_client)`.
    async fn make_test_server() -> (ServerEndpoint, SocketAddr, [u8; 32]) {
        let identity = Identity::self_signed(&["localhost".to_string()]).unwrap();
        let chain = identity.certificate_chain();
        let leaf = chain.as_slice().first().unwrap();
        let hash = *leaf.hash().as_ref();
        let config = ServerConfig::builder()
            .with_bind_address("127.0.0.1:0".parse().unwrap())
            .with_identity(identity)
            .build();
        let server = Endpoint::server(config).unwrap();
        let addr = server.local_addr().unwrap();
        (server, addr, hash)
    }

    /// Accept a single client connection on `server`. Returns the server-side
    /// `Connection`. The client side is established and then dropped — registry
    /// tests don't depend on the client staying alive.
    async fn accept_one_conn(
        server: &ServerEndpoint,
        hash: [u8; 32],
        port: u16,
    ) -> Connection {
        let client_config = ClientConfig::builder()
            .with_bind_default()
            .with_server_certificate_hashes([Sha256Digest::new(hash)])
            .build();
        let client_endpoint = Endpoint::client(client_config).unwrap();
        let url = format!("https://localhost:{port}/wt");
        let connect_task = tokio::spawn(async move {
            let _ = client_endpoint.connect(url).await;
        });
        let incoming = server.accept().await;
        let req = incoming.await.unwrap();
        let conn = req.accept().await.unwrap();
        let _ = connect_task.await;
        conn
    }

    /// Build a minimal `SessionEntry` for testing. `publish_tx` lets the
    /// `route_release_data` test wire a real channel; pass `None` otherwise.
    fn make_entry(
        conn: Connection,
        session_id: u64,
        publish_tx: Option<tokio::sync::mpsc::Sender<TsMessage>>,
    ) -> Arc<SessionEntry> {
        Arc::new(SessionEntry {
            conn,
            initiator: Arc::new(Mutex::new(SrtInitiator::new(
                LOCAL,
                DUMMY_REMOTE,
                &SrtConfig::default(),
                Duration::from_millis(50),
            ))),
            loss: Arc::new(Mutex::new(LossInjector::new(0, 0))),
            viewer: StdMutex::new(None),
            session_id,
            shutdown: Arc::new(Notify::new()),
            finished: AtomicBool::new(false),
            publish_tx,
            messages_pushed: AtomicU64::new(0),
            viewer_lag_count: AtomicU64::new(0),
            publish_dropped: AtomicU64::new(0),
            last_srt_stats: StdMutex::new(None),
            guard: None,
            peer: DUMMY_REMOTE,
            stream_name: format!("stream-{session_id}"),
        })
    }

    #[tokio::test]
    async fn empty_registry_is_noop() {
        let reg = SessionRegistry::new();
        assert_eq!(reg.active_session_count(), 0);
        assert!(reg.snapshot().is_empty());
        assert!(reg.snapshot_sessions().is_empty());
        // tick_all on empty registry must not panic across rapid back-to-back
        // calls (exercises both branches of the should_log_stats gate).
        reg.tick_all().await;
        reg.tick_all().await;
    }

    #[tokio::test]
    async fn insert_and_snapshot() {
        let (server, addr, hash) = make_test_server().await;
        let conn = accept_one_conn(&server, hash, addr.port()).await;
        let reg = SessionRegistry::new();
        let id = reg.insert(make_entry(conn, 11, None));
        assert_eq!(id, 11);
        assert_eq!(reg.snapshot().len(), 1);
        assert_eq!(reg.active_session_count(), 1);
        assert_eq!(reg.snapshot()[0].session_id, 11);
        server.close(0u32.into(), b"");
    }

    #[tokio::test]
    async fn remove_drops_entry() {
        let (server, addr, hash) = make_test_server().await;
        let conn = accept_one_conn(&server, hash, addr.port()).await;
        let reg = SessionRegistry::new();
        reg.insert(make_entry(conn, 7, None));
        assert_eq!(reg.snapshot().len(), 1);
        reg.remove(7);
        assert!(reg.snapshot().is_empty());
        assert_eq!(reg.active_session_count(), 0);
        server.close(0u32.into(), b"");
    }

    #[tokio::test]
    async fn clear_empties_registry() {
        let (server, addr, hash) = make_test_server().await;
        let reg = SessionRegistry::new();
        for i in 1..=3u64 {
            let conn = accept_one_conn(&server, hash, addr.port()).await;
            reg.insert(make_entry(conn, i, None));
        }
        assert_eq!(reg.snapshot().len(), 3);
        reg.clear();
        assert!(reg.snapshot().is_empty());
        assert_eq!(reg.active_session_count(), 0);
        server.close(0u32.into(), b"");
    }

    #[tokio::test]
    async fn active_session_count_excludes_finished() {
        let (server, addr, hash) = make_test_server().await;
        let reg = SessionRegistry::new();
        for i in 1..=3u64 {
            let conn = accept_one_conn(&server, hash, addr.port()).await;
            let entry = make_entry(conn, i, None);
            if i == 2 {
                entry.finished.store(true, Ordering::Relaxed);
            }
            reg.insert(entry);
        }
        // snapshot returns all entries; active_session_count filters finished.
        assert_eq!(reg.snapshot().len(), 3);
        assert_eq!(reg.active_session_count(), 2);
        server.close(0u32.into(), b"");
    }

    #[tokio::test]
    async fn snapshot_sessions_filters_finished_and_reads_counters() {
        let (server, addr, hash) = make_test_server().await;
        let reg = SessionRegistry::new();

        let conn1 = accept_one_conn(&server, hash, addr.port()).await;
        let e1 = make_entry(conn1, 1, None);
        e1.messages_pushed.store(42, Ordering::Relaxed);
        e1.viewer_lag_count.store(3, Ordering::Relaxed);
        e1.publish_dropped.store(5, Ordering::Relaxed);
        reg.insert(e1);

        let conn2 = accept_one_conn(&server, hash, addr.port()).await;
        let e2 = make_entry(conn2, 2, None);
        e2.finished.store(true, Ordering::Relaxed);
        reg.insert(e2);

        let snaps = reg.snapshot_sessions();
        assert_eq!(snaps.len(), 1, "finished session must be filtered");
        let s = &snaps[0];
        assert_eq!(s.session_id, 1);
        assert_eq!(s.messages_pushed, 42);
        assert_eq!(s.viewer_lag_count, 3);
        assert_eq!(s.publish_dropped, 5);
        assert_eq!(s.stream_name, "stream-1");

        server.close(0u32.into(), b"");
    }

    #[tokio::test]
    async fn tick_all_removes_finished_entries() {
        let (server, addr, hash) = make_test_server().await;
        let reg = SessionRegistry::new();

        let conn1 = accept_one_conn(&server, hash, addr.port()).await;
        let e1 = make_entry(conn1, 1, None);
        e1.finished.store(true, Ordering::Relaxed);
        reg.insert(e1);

        let conn2 = accept_one_conn(&server, hash, addr.port()).await;
        let e2 = make_entry(conn2, 2, None);
        e2.finished.store(true, Ordering::Relaxed);
        reg.insert(e2);

        assert_eq!(reg.snapshot().len(), 2);
        // Finished entries hit the early `continue` branch — no initiator or
        // conn activity; they land in `to_remove` and are pruned at the end.
        reg.tick_all().await;
        assert_eq!(
            reg.snapshot().len(),
            0,
            "tick_all must remove finished entries"
        );
        server.close(0u32.into(), b"");
    }

    #[tokio::test]
    async fn tick_all_stats_log_gating_does_not_panic() {
        // Two rapid tick_all calls exercise both branches of the 5s gate:
        // first call sets `last_stats_log`, second sees <5s elapsed and skips.
        // A non-finished entry with populated `last_srt_stats` ensures the
        // inner stats-logging loop has work to do on the firing branch.
        let (server, addr, hash) = make_test_server().await;
        let reg = SessionRegistry::new();

        let conn = accept_one_conn(&server, hash, addr.port()).await;
        let entry = make_entry(conn, 1, None);
        *entry.last_srt_stats.lock() = Some(SocketStatistics::default());
        reg.insert(entry);

        reg.tick_all().await; // should_log_stats = true (first call)
        reg.tick_all().await; // should_log_stats = false (<5s since last)

        server.close(0u32.into(), b"");
    }

    #[tokio::test]
    async fn route_release_data_increments_publish_dropped_on_full_channel() {
        // Exercises item #9: a full publish channel must increment the
        // publish_dropped counter rather than silently dropping.
        use crate::session::route_release_data;

        let (server, addr, hash) = make_test_server().await;
        let conn = accept_one_conn(&server, hash, addr.port()).await;

        // Capacity-1 channel; fill it so the next try_send must fail.
        let (tx, _rx) = tokio::sync::mpsc::channel::<TsMessage>(1);
        let now = Instant::now();
        tx.try_send((now, Bytes::from_static(b"x"))).unwrap();

        let entry = make_entry(conn, 99, Some(tx));
        assert_eq!(
            entry.publish_dropped.load(Ordering::Relaxed),
            0,
            "counter starts at zero"
        );

        route_release_data(&entry, now, &Bytes::from_static(b"payload"));
        assert_eq!(
            entry.publish_dropped.load(Ordering::Relaxed),
            1,
            "first drop must increment counter"
        );

        route_release_data(&entry, now, &Bytes::from_static(b"payload"));
        assert_eq!(
            entry.publish_dropped.load(Ordering::Relaxed),
            2,
            "second drop must increment counter again"
        );

        server.close(0u32.into(), b"");
    }
}
