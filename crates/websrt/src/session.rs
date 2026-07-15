//! Per-browser session: WT connection ↔ SRT initiator pump + ingester fan-in.
//!
//! Phase 4: drives the HSv5 handshake, then pumps TS messages from a shared
//! Ingester into the SRT sender and shuttles ACK/NAK datagrams back.
//! Phase 5: optional `--sim-loss N` drops outgoing data datagrams with N%
//! probability, exercising NAK/retransmit and TLPKTL.
//! Phase 9: each session gets its own `ViewerRx` from the broadcaster.
//! Phase 10: ACK recv-loop and sender tick/push-loop run as concurrent tasks
//! sharing `SrtInitiator` + `LossInjector` via `Arc<tokio::sync::Mutex<_>>`.
//! Earlier the two ran in a single `select!`; under heavy ACK traffic the
//! recv branch ran almost to the exclusion of the tick branch, the sender
//! pump fell behind, and the peer idle timeout (30s) fired. Splitting the
//! two into separate tasks lets the runtime interleave them fairly.

#[cfg(feature = "sim-loss")]
use rand::{rngs::StdRng, Rng, SeedableRng};

use crate::broadcaster::ViewerRx;
use crate::srt_sender::{SenderAction, SrtInitiator};
use std::net::{IpAddr, SocketAddr};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::{Mutex, Notify};
use tokio::task::JoinHandle;
use wtransport::Connection;

/// Monotonic counter for short per-session correlation IDs.
static SESSION_COUNTER: AtomicU64 = AtomicU64::new(1);

/// Probabilistic outgoing-datagram dropper. Only data packets are dropped;
/// control packets (handshake, ACK, NAK, KeepAlive, Shutdown) always pass so
/// the SRT reliability machinery stays functional.
#[cfg(feature = "sim-loss")]
struct LossInjector {
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

    fn sent(&self) -> u64 { self.sent }
    fn dropped(&self) -> u64 { self.dropped }
}

/// No-op stub when sim-loss feature is disabled.
#[cfg(not(feature = "sim-loss"))]
struct LossInjector;

#[cfg(not(feature = "sim-loss"))]
impl LossInjector {
    fn new(_pct: u8, _seed: u64) -> Self { Self }
    fn should_drop(&mut self, _bytes: &[u8]) -> bool { false }
    fn sent(&self) -> u64 { 0 }
    fn dropped(&self) -> u64 { 0 }
}

/// A single browser session.
pub struct BrowserSession;

impl BrowserSession {
    /// Dummy socket addresses required by srt-protocol's Connect/Listen state
    /// machines. They're never used on the WebTransport path — srt-protocol
    /// just needs them for internal bookkeeping (socket IDs, etc).
    const DUMMY_LOCAL_ADDR: SocketAddr =
        SocketAddr::new(IpAddr::V4(std::net::Ipv4Addr::LOCALHOST), 0);
    const DUMMY_REMOTE_ADDR: SocketAddr =
        SocketAddr::new(IpAddr::V4(std::net::Ipv4Addr::LOCALHOST), 1);

    /// Spawn a session. `viewer` is this session's private subscription to the
    /// ingester fanout. Returns a `(shutdown, join_handle)` tuple so the caller
    /// can trigger graceful drain via `shutdown.notify_one()`.
    pub fn spawn(conn: Connection, viewer: ViewerRx, sim_loss: u8, sim_seed: u64, latency_ms: u64) -> (Arc<Notify>, tokio::task::JoinHandle<()>) {
        let shutdown = Arc::new(Notify::new());
        let handle = tokio::spawn({
            let shutdown = shutdown.clone();
            async move {
                if let Err(e) = Self::run(conn, viewer, sim_loss, sim_seed, latency_ms, shutdown).await {
                    tracing::info!(?e, "browser session ended");
                }
            }
        });
        (shutdown, handle)
    }

    async fn run(
        conn: Connection,
        viewer: ViewerRx,
        sim_loss: u8,
        sim_seed: u64,
        latency_ms: u64,
        shutdown: Arc<Notify>,
    ) -> anyhow::Result<()> {
        let peer = conn.remote_address();
        let session_id = SESSION_COUNTER.fetch_add(1, Ordering::Relaxed);
        tracing::info!(session_id, %peer, sim_loss, "session: starting SRT initiator");

        let initiator = Arc::new(Mutex::new(SrtInitiator::new(
            Self::DUMMY_LOCAL_ADDR.ip(),
            Self::DUMMY_REMOTE_ADDR,
            latency_ms,
        )));
        let loss = Arc::new(Mutex::new(LossInjector::new(sim_loss, sim_seed)));

        // Kick off the handshake immediately.
        {
            let mut init = initiator.lock().await;
            let mut l = loss.lock().await;
            let now = Instant::now();
            for action in init.tick(now) {
                Self::send_action(&conn, action, &mut l)?;
            }
        }

        let recv_task: JoinHandle<anyhow::Result<()>> = tokio::spawn(Self::recv_pump(
            conn.clone(),
            initiator.clone(),
            loss.clone(),
            shutdown.clone(),
            session_id,
        ));
        let sender_task: JoinHandle<anyhow::Result<()>> = tokio::spawn(Self::sender_pump(
            conn.clone(),
            initiator.clone(),
            loss.clone(),
            viewer,
            shutdown.clone(),
            session_id,
            latency_ms,
        ));
        let recv_abort = recv_task.abort_handle();
        let sender_abort = sender_task.abort_handle();

        // When either task finishes (Close, peer hangup, or error), abort the
        // other and return. notify_waiters() covers the rare case where the
        // surviving task is still mid-await on shutdown.notified().
        let joined = tokio::select! {
            r = recv_task => {
                sender_abort.abort();
                r
            }
            r = sender_task => {
                recv_abort.abort();
                r
            }
        };
        joined??;
        Ok(())
    }

    /// Task A: drain incoming WT datagrams (handshake replies, ACK/NAK) into
    /// the initiator. Exits on connection error or Close action.
    async fn recv_pump(
        conn: Connection,
        initiator: Arc<Mutex<SrtInitiator>>,
        loss: Arc<Mutex<LossInjector>>,
        shutdown: Arc<Notify>,
        session_id: u64,
    ) -> anyhow::Result<()> {
        loop {
            let d = tokio::select! {
                biased;
                _ = shutdown.notified() => return Ok(()),
                res = conn.receive_datagram() => match res {
                    Ok(d) => d,
                    Err(e) => {
                        tracing::info!(session_id, ?e, "session: recv datagram stream closed");
                        shutdown.notify_waiters();
                        return Ok(());
                    }
                },
            };
            let payload = d.payload();
            let now = Instant::now();
            let mut should_close = false;
            let actions = {
                let mut init = initiator.lock().await;
                init.handle_datagram(&payload, now)
            };
            {
                let mut l = loss.lock().await;
                for action in actions {
                    if matches!(action, SenderAction::Close) {
                        should_close = true;
                    }
                    Self::send_action(&conn, action, &mut l)?;
                }
            }
            if should_close {
                tracing::info!(session_id, "session: initiator returned Close; recv loop exiting");
                shutdown.notify_waiters();
                return Ok(());
            }
        }
    }

    /// Task B: periodic tick + viewer-message push. The 2ms interval drives the
    /// SRT sender state machine; viewer.recv() interleaves new TS messages.
    async fn sender_pump(
        conn: Connection,
        initiator: Arc<Mutex<SrtInitiator>>,
        loss: Arc<Mutex<LossInjector>>,
        mut viewer: ViewerRx,
        shutdown: Arc<Notify>,
        session_id: u64,
        latency_ms: u64,
    ) -> anyhow::Result<()> {
        let mut ticker = tokio::time::interval(Duration::from_millis(2));
        ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        let mut stats_interval = tokio::time::interval(Duration::from_secs(5));
        stats_interval.tick().await; // consume the immediate first tick
        let mut action_count: u64 = 0;

        loop {
            let (msg, do_stats) = tokio::select! {
                biased;
                _ = shutdown.notified() => return Ok(()),
                _ = stats_interval.tick() => (None, true),
                _ = ticker.tick() => (None, false),
                msg = viewer.recv() => match msg {
                    Ok(Some(m)) => (Some(m), false),
                    Ok(None) => {
                        tracing::info!(session_id, "session: viewer source ended");
                        return Ok(());
                    }
                    Err(e) => {
                        tracing::warn!(session_id, ?e, "viewer recv error");
                        return Ok(());
                    }
                },
            };
            let now = Instant::now();
            let mut should_close = false;
            let lock_start = Instant::now();
            let actions = {
                let mut init = initiator.lock().await;
                let lock_ms = lock_start.elapsed().as_millis();
                if lock_ms > 10 {
                    tracing::warn!(session_id, lock_ms, "sender_pump: initiator lock contention");
                }
                let mut v = init.tick(now);
                if let Some(m) = msg {
                    if init.is_connected() {
                        v.extend(init.push_message(m, now));
                    }
                }
                if init.is_connected() {
                    let mut drained = 0;
                    loop {
                        if drained >= 32 { break; }
                        match viewer.try_recv() {
                            Ok(Some(m)) => { v.extend(init.push_message(m, now)); drained += 1; }
                            Ok(None) => break,
                            Err(lag) => {
                                tracing::warn!(session_id, lag, "viewer lagged in try_recv drain; dropped messages");
                                break;
                            }
                        }
                    }
                }
                v
            };
            {
                let mut l = loss.lock().await;
                action_count += actions.len() as u64;
                for action in actions {
                    if matches!(action, SenderAction::Close) {
                        should_close = true;
                    }
                    Self::send_action(&conn, action, &mut l)?;
                }
            }
            if do_stats {
                let (sent, dropped) = {
                    let l = loss.lock().await;
                    (l.sent(), l.dropped())
                };
                let lag = viewer.lag_count();
                let wt_rtt = conn.rtt();
                let srt_stats = {
                    let init = initiator.lock().await;
                    init.stats().cloned()
                };
                if let Some(ref s) = srt_stats {
                    tracing::info!(
                        session_id,
                        sent,
                        dropped,
                        lag,
                        wt_rtt_ms = wt_rtt.as_secs_f64() * 1000.0,
                        srt_rtt_ms = s.tx_average_rtt.as_secs_f64() * 1000.0,
                        tx_data = s.tx_data,
                        tx_unique = s.tx_unique_data,
                        tx_rexmit = s.tx_retransmit_data,
                        tx_dropped = s.tx_dropped_data,
                        tx_loss = s.tx_loss_data,
                        rx_ack = s.rx_ack,
                        rx_nak = s.rx_nak,
                        tx_buffered = s.tx_buffered_data,
                        actions = action_count,
                        "session stats"
                    );
                    let wt_rtt_ms = wt_rtt.as_secs_f64() * 1000.0;
                    if wt_rtt_ms * 4.0 > latency_ms as f64 {
                        tracing::warn!(
                            session_id,
                            wt_rtt_ms,
                            latency_ms,
                            recommended = (wt_rtt_ms * 4.0) as u64,
                            "WT RTT suggests latency is too low; consider --latency {}",
                            (wt_rtt_ms * 4.0).max(120.0) as u64,
                        );
                    }
                } else {
                    tracing::debug!(session_id, sent, dropped, lag, "session stats (no SRT stats yet)");
                }
                action_count = 0;
                let is_closed = initiator.lock().await.is_closed();
                if is_closed {
                    tracing::info!(session_id, "session: initiator closed");
                    return Ok(());
                }
            }
            if should_close {
                tracing::info!(session_id, "session: initiator returned Close; exiting loop");
                shutdown.notify_waiters();
                return Ok(());
            }
        }
    }

    fn send_action(
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
                    tracing::warn!(len = bytes.len(), "outgoing datagram > 1200B; QUIC may reject");
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
            SenderAction::Log(s) => {
                tracing::info!("session: log: {s}");
            }
        }
        Ok(())
    }
}
