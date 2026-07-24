//! Per-browser session: WT connection ↔ SRT initiator pump.
//!
//! The session is split into two parts:
//! - **recv_pump** (this file): a per-session task that drains incoming WT
//!   datagrams (handshake replies, ACK/NAK) into the SRT initiator. It is
//!   cheap — it blocks on WT datagram receive — so one per session is fine.
//! - **sender drive** (the centralized ticker in [`crate::registry`]): a
//!   single shared task ticks every active session's SRT state machine ~2ms
//!   and pushes viewer data.
//!
//! The two halves share `SrtInitiator`, `LossInjector`, the WT `Connection`,
//! and the shutdown signal via an `Arc<SessionEntry>` (see [`registry`]).

#[cfg(feature = "sim-loss")]
use rand::{rngs::StdRng, Rng, SeedableRng};

use crate::broadcaster::ViewerRx;
use crate::ingest::TsMessage;
use crate::registry::SessionEntry;
use crate::srt_sender::{SenderAction, SrtConfig, SrtInitiator};
use std::net::{IpAddr, SocketAddr};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex as StdMutex};
use std::time::Instant;
use tokio::sync::{Mutex, Notify};
use tokio::task::JoinHandle;
use wtransport::Connection;

/// Monotonic counter for short per-session correlation IDs.
static SESSION_COUNTER: AtomicU64 = AtomicU64::new(1);

/// Probabilistic outgoing-datagram dropper. Only data packets are dropped;
/// control packets (handshake, ACK, NAK, KeepAlive, Shutdown) always pass so
/// the SRT reliability machinery stays functional.
#[cfg(feature = "sim-loss")]
pub(crate) struct LossInjector {
    enabled: bool,
    pct: u8, // 0..=100
    rng: StdRng,
    dropped: u64,
    sent: u64,
    first_drop_logged: bool,
}

#[cfg(feature = "sim-loss")]
impl LossInjector {
    fn new(pct: u8, seed: u64) -> Self {
        Self {
            enabled: pct > 0,
            pct,
            rng: rand::rngs::StdRng::seed_from_u64(seed),
            dropped: 0,
            sent: 0,
            first_drop_logged: false,
        }
    }

    /// Returns true if the caller should DROP this datagram.
    fn should_drop(&mut self, bytes: &[u8]) -> bool {
        if !self.enabled {
            return false;
        }
        // SRT packet format (srt-protocol/src/packet/mod.rs:108):
        //   bit 7 of first byte = 0  → DATA packet
        //   bit 7 of first byte = 1  → CONTROL packet
        // We only drop data packets so handshake/ACK/NAK always get through.
        if bytes.first().map(|b| b & 0x80 == 0).unwrap_or(false) {
            self.sent += 1;
            let roll: u32 = self.rng.gen_range(0..100);
            if roll < self.pct as u32 {
                self.dropped += 1;
                if !self.first_drop_logged {
                    self.first_drop_logged = true;
                    tracing::info!(
                        pct = self.pct,
                        sent = self.sent,
                        "sim-loss: first data-packet drop fired (NAK/retransmit will recover)"
                    );
                }
                return true;
            }
        }
        false
    }
}

/// No-op stub when sim-loss feature is disabled.
#[cfg(not(feature = "sim-loss"))]
pub(crate) struct LossInjector;

#[cfg(not(feature = "sim-loss"))]
impl LossInjector {
    fn new(_pct: u8, _seed: u64) -> Self {
        Self
    }
    fn should_drop(&mut self, _bytes: &[u8]) -> bool {
        false
    }
}

/// A single browser session: constructs the shared `SessionEntry` and spawns
/// the per-session `recv_pump`. Sender-side driving is handled centrally by
/// [`crate::registry::SessionRegistry::tick_all`].
pub(crate) struct BrowserSession;

impl BrowserSession {
    /// Dummy values required by srt-protocol's Connect/Listen state machines.
    /// They're never used on the WebTransport path — srt-protocol just needs
    /// them for internal bookkeeping (socket IDs, etc).
    const DUMMY_LOCAL_IP: IpAddr = IpAddr::V4(std::net::Ipv4Addr::LOCALHOST);
    const DUMMY_REMOTE_ADDR: SocketAddr =
        SocketAddr::new(IpAddr::V4(std::net::Ipv4Addr::LOCALHOST), 1);

    /// Build the shared session state and spawn the recv_pump task.
    ///
    /// `viewer` is this session's private subscription to the ingester
    /// fanout; it is stored inside the returned entry so the centralized
    /// ticker can drain it. `None` for publish-only sessions.
    ///
    /// Returns `(entry, recv_pump_handle)`. The caller must insert `entry`
    /// into the [`crate::registry::SessionRegistry`] for the ticker to drive
    /// the SRT state machine. The recv_pump task exits when the WT datagram
    /// stream closes, the initiator returns `Close`, or the entry's shutdown
    /// Notify is signaled; on exit it marks `entry.finished = true`.
    pub(crate) async fn create(
        conn: Connection,
        viewer: Option<ViewerRx>,
        sim_loss: u8,
        sim_seed: u64,
        config: SrtConfig,
        publish_tx: Option<tokio::sync::mpsc::Sender<TsMessage>>,
        guard: Option<crate::limits::SessionGuard>,
        stream_name: String,
    ) -> (Arc<SessionEntry>, JoinHandle<()>) {
        let session_id = SESSION_COUNTER.fetch_add(1, Ordering::Relaxed);
        let peer = conn.remote_address();
        let quic_rtt = conn.rtt();
        let quic_stats = conn.quic_connection().stats().path;
        tracing::info!(
            session_id, %peer, sim_loss,
            ?quic_rtt,
            cwnd_bytes = quic_stats.cwnd,
            "session: starting SRT initiator"
        );

        let initiator = Arc::new(Mutex::new(SrtInitiator::new(
            Self::DUMMY_LOCAL_IP,
            Self::DUMMY_REMOTE_ADDR,
            &config,
            quic_rtt,
        )));
        let loss = Arc::new(Mutex::new(LossInjector::new(sim_loss, sim_seed)));
        let shutdown = Arc::new(Notify::new());

        let entry = Arc::new(SessionEntry {
            conn: conn.clone(),
            initiator: initiator.clone(),
            loss: loss.clone(),
            viewer: StdMutex::new(viewer),
            session_id,
            shutdown: shutdown.clone(),
            finished: AtomicBool::new(false),
            publish_tx,
            messages_pushed: AtomicU64::new(0),
            viewer_lag_count: AtomicU64::new(0),
            last_srt_stats: StdMutex::new(None),
            guard,
            peer,
            stream_name,
        });

        let entry_for_task = entry.clone();
        let handle = tokio::spawn(async move {
            Self::recv_pump(entry_for_task.clone()).await;
            // Defensive: ensure finished is set on every exit path so the
            // ticker reclaims the entry on its next sweep.
            entry_for_task.finished.store(true, Ordering::Relaxed);
        });
        (entry, handle)
    }

    /// Drain incoming WT datagrams (handshake replies, ACK/NAK) into the
    /// initiator and dispatch the resulting sender actions. Exits on
    /// connection error, `Close` action, or shutdown signal — and runs a
    /// single cleanup block (close conn + mark finished + notify ticker).
    async fn recv_pump(entry: Arc<SessionEntry>) {
        let session_id = entry.session_id;
        'recv: loop {
            let d = tokio::select! {
                biased;
                _ = entry.shutdown.notified() => {
                    tracing::info!(session_id, "session: shutdown signal received");
                    break 'recv;
                }
                res = entry.conn.receive_datagram() => match res {
                    Ok(d) => d,
                    Err(e) => {
                        tracing::info!(session_id, ?e, "session: recv datagram stream closed");
                        break 'recv;
                    }
                },
            };
            let payload = d.payload();
            let now = Instant::now();
            let (actions, data) = {
                let mut init = entry.initiator.lock().await;
                init.handle_datagram(&payload, now)
            };
            for (ts, bytes) in data {
                route_release_data(&entry, ts, &bytes);
            }
            {
                let mut l = entry.loss.lock().await;
                for action in actions {
                    let is_close = matches!(action, SenderAction::Close);
                    if let Err(e) = send_action(&entry.conn, action, &mut l) {
                        tracing::info!(?e, session_id, "session: send_action failed");
                        break 'recv;
                    }
                    if is_close {
                        tracing::info!(session_id, "session: initiator returned Close");
                        break 'recv;
                    }
                }
            }
        }
        // Single cleanup path for all exit conditions.
        let final_stats = entry.conn.quic_connection().stats().path;
        tracing::info!(
            session_id,
            rtt = ?entry.conn.rtt(),
            cwnd = final_stats.cwnd,
            lost_packets = final_stats.lost_packets,
            congestion_events = final_stats.congestion_events,
            "session: closing — final QUIC stats"
        );
        entry.conn.close(0u32.into(), b"close");
        entry.finished.store(true, Ordering::Relaxed);
        entry.shutdown.notify_waiters();
    }
}

/// Execute one sender action against the WT connection, applying the
/// per-session loss injector. Shared between `recv_pump` (handshake/ACK
/// replies) and the centralized ticker (data packets).
pub(crate) fn send_action(
    conn: &Connection,
    action: SenderAction,
    loss: &mut LossInjector,
) -> anyhow::Result<()> {
    match action {
        SenderAction::SendDatagram(bytes) => {
            if loss.should_drop(&bytes) {
                tracing::debug!(len = bytes.len(), "sim-loss dropped");
                return Ok(());
            }
            if bytes.len() > 1200 {
                tracing::warn!(
                    len = bytes.len(),
                    "outgoing datagram > 1200B; QUIC may reject"
                );
            }
            if let Err(e) = conn.send_datagram(bytes) {
                tracing::warn!(?e, "send_datagram failed; closing session");
                return Err(anyhow::Error::new(e));
            }
        }
        SenderAction::HandshakeComplete => {
            tracing::info!("session: HandshakeComplete");
        }
        SenderAction::Close => {
            tracing::info!("session: Close");
        }
    }
    Ok(())
}

/// Route a `ReleaseData` action to the session's publish channel (if any).
/// Called from both `recv_pump` and `tick_all` — centralizes the routing
/// logic and the backpressure-debug log.
pub(crate) fn route_release_data(
    entry: &SessionEntry,
    ts: std::time::Instant,
    bytes: &bytes::Bytes,
) {
    if let Some(tx) = &entry.publish_tx {
        if tx.try_send((ts, bytes.clone())).is_err() {
            tracing::debug!(
                session_id = entry.session_id,
                queued = tx.capacity(),
                "publish_tx try_send drop"
            );
        }
    }
}
