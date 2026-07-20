//! Per-browser session: WT connection ↔ SRT initiator pump.
//!
//! The session is split into two parts:
//! - **recv_pump** (this file): a per-session task that drains incoming WT
//!   datagrams (handshake replies, ACK/NAK) into the SRT initiator. It is
//!   cheap — it blocks on WT datagram receive — so one per session is fine.
//! - **sender drive** (the centralized ticker in [`crate::registry`]): a
//!   single shared task ticks every active session's SRT state machine ~2ms
//!   and pushes viewer data. Replaces the historical per-session sender_pump,
//!   which at 500+ viewers generated ~250k timer wakeups/sec.
//!
//! The two halves share `SrtInitiator`, `LossInjector`, the WT `Connection`,
//! and the shutdown signal via an `Arc<SessionEntry>` (see [`registry`]).
//!
//! Phase 5 note: optional `--sim-loss N` drops outgoing data datagrams with
//! N% probability, exercising NAK/retransmit and TLPKTL. The dropper lives
//! here so both recv_pump (handshake/ACK replies) and the ticker (data
//! packets) share one injector per session.

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
    fn new(_pct: u8, _seed: u64) -> Self { Self }
    fn should_drop(&mut self, _bytes: &[u8]) -> bool { false }
}

/// A single browser session: constructs the shared `SessionEntry` and spawns
/// the per-session `recv_pump`. Sender-side driving is handled centrally by
/// [`crate::registry::SessionRegistry::tick_all`].
pub struct BrowserSession;

impl BrowserSession {
    /// Dummy socket addresses required by srt-protocol's Connect/Listen state
    /// machines. They're never used on the WebTransport path — srt-protocol
    /// just needs them for internal bookkeeping (socket IDs, etc).
    const DUMMY_LOCAL_ADDR: SocketAddr =
        SocketAddr::new(IpAddr::V4(std::net::Ipv4Addr::LOCALHOST), 0);
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
        streamid: Option<String>,
    ) -> (Arc<SessionEntry>, JoinHandle<()>) {
        let session_id = SESSION_COUNTER.fetch_add(1, Ordering::Relaxed);
        let peer = conn.remote_address();
        tracing::info!(session_id, %peer, sim_loss, "session: starting SRT initiator");

        let initiator = Arc::new(Mutex::new(SrtInitiator::new(
            Self::DUMMY_LOCAL_ADDR.ip(),
            Self::DUMMY_REMOTE_ADDR,
            &config,
            streamid,
        )));
        let loss = Arc::new(Mutex::new(LossInjector::new(sim_loss, sim_seed)));
        let shutdown = Arc::new(Notify::new());

        let entry = Arc::new(SessionEntry {
            conn: conn.clone(),
            initiator: initiator.clone(),
            loss: loss.clone(),
            viewer: StdMutex::new(viewer),
            // Deadline is now so the ticker's first iteration picks this
            // session up immediately and runs the induction handshake.
            next_deadline: StdMutex::new(Instant::now()),
            session_id,
            shutdown: shutdown.clone(),
            finished: AtomicBool::new(false),
            publish_tx,
            publish_try_send_drops: AtomicU64::new(0),
            publish_tick_try_send_drops: AtomicU64::new(0),
            publish_release_total: AtomicU64::new(0),
            publish_bytes_total: AtomicU64::new(0),
            viewer_pushed_bytes: AtomicU64::new(0),
            viewer_sent_datagrams: AtomicU64::new(0),
            drain_wait_count: AtomicU64::new(0),
            drain_wait_last_us: AtomicU64::new(0),
        });

        // Kick off the handshake immediately so the INDUCTION packet leaves
        // before the ticker's first iteration. Subsequent handshake replies
        // from the browser are handled by recv_pump.
        {
            let mut init = initiator.lock().await;
            let mut l = loss.lock().await;
            let now = Instant::now();
            let (actions, _wait) = init.tick(now);
            for action in actions {
                if matches!(action, SenderAction::Close) {
                    entry.finished.store(true, Ordering::Relaxed);
                }
                let _ = send_action(&conn, action, &mut l);
            }
        }

        let entry_for_task = entry.clone();
        let handle = tokio::spawn(async move {
            if let Err(e) = Self::recv_pump(entry_for_task.clone()).await {
                tracing::info!(?e, "browser session recv_pump ended");
            }
            // Defensive: ensure finished is set on every exit path so the
            // ticker reclaims the entry on its next sweep.
            entry_for_task.finished.store(true, Ordering::Relaxed);
        });
        (entry, handle)
    }

    /// Drain incoming WT datagrams (handshake replies, ACK/NAK) into the
    /// initiator and dispatch the resulting sender actions. Exits on
    /// connection error, `Close` action, or shutdown signal — and marks the
    /// entry finished so the ticker stops servicing it.
    async fn recv_pump(entry: Arc<SessionEntry>) -> anyhow::Result<()> {
        let session_id = entry.session_id;
        loop {
            let d = tokio::select! {
                biased;
                _ = entry.shutdown.notified() => {
                    entry.conn.close(0u32.into(), b"shutdown");
                    return Ok(());
                }
                res = entry.conn.receive_datagram() => match res {
                    Ok(d) => d,
                    Err(e) => {
                        tracing::info!(session_id, ?e, "session: recv datagram stream closed");
                        entry.conn.close(0u32.into(), b"");
                        entry.finished.store(true, Ordering::Relaxed);
                        entry.shutdown.notify_waiters();
                        return Ok(());
                    }
                },
            };
            let payload = d.payload();
            let now = Instant::now();
            let mut should_close = false;
            let (actions, wait) = {
                let mut init = entry.initiator.lock().await;
                init.handle_datagram(&payload, now)
            };
            // Pull in the next deadline so the ticker runs this session's
            // Timer-driven logic (ACK/NAK/EXP/TSBPD release) promptly.
            // Without this, publish-only sessions are only ticked at the
            // periodic interval returned by the last tick(); incoming data
            // never reschedules it, so the SRT receiver starves and packets
            // stall then drain in bursts.
            {
                let mut nd = entry.next_deadline.lock().unwrap();
                let candidate = now + wait;
                if candidate < *nd {
                    *nd = candidate;
                }
            }
            {
                let mut l = entry.loss.lock().await;
                for action in actions {
                    match &action {
                        SenderAction::ReleaseData((ts, bytes)) => {
                            if let Some(tx) = &entry.publish_tx {
                                entry.publish_release_total.fetch_add(1, Ordering::Relaxed);
                                entry.publish_bytes_total.fetch_add(bytes.len() as u64, Ordering::Relaxed);
                                if tx.try_send((*ts, bytes.clone())).is_err() {
                                    entry.publish_try_send_drops.fetch_add(1, Ordering::Relaxed);
                                    tracing::debug!(
                                        session_id = entry.session_id,
                                        queued = tx.capacity(),
                                        "publish_tx try_send drop in recv_pump"
                                    );
                                }
                            }
                            continue;
                        }
                        SenderAction::Close => {
                            should_close = true;
                        }
                        SenderAction::DrainWait(d) => {
                            entry.drain_wait_count.fetch_add(1, Ordering::Relaxed);
                            entry.drain_wait_last_us.store(d.as_micros() as u64, Ordering::Relaxed);
                            continue;
                        }
                        _ => {}
                    }
                    send_action(&entry.conn, action, &mut l)?;
                }
            }
            if should_close {
                tracing::info!(session_id, "session: initiator returned Close; recv loop exiting");
                entry.conn.close(0u32.into(), b"close");
                entry.finished.store(true, Ordering::Relaxed);
                entry.shutdown.notify_waiters();
                return Ok(());
            }
        }
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
        SenderAction::ReleaseData(_) => {
            // Handled by caller before send_action is invoked.
        }
        SenderAction::Close => {
            tracing::info!("session: Close");
        }
        SenderAction::Log(s) => {
            tracing::info!("session: log: {s}");
        }
        SenderAction::DrainWait(_) => {
            // Instrumentation only; handled by caller.
        }
    }
    Ok(())
}
