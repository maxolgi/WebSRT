//! High-level WebTransport gateway: accept loop, session spawning, fanout.
//!
//! Wraps the lower-level building blocks (`Broadcaster`, `BrowserSession`,
//! `SrtInitiator`) into a single run-loop. The caller provides an `Ingester`
//! (either immediately or deferred via `source_handle`).

use crate::broadcaster::Broadcaster;
use crate::ingest::Ingester;
use crate::registry::SessionRegistry;
use crate::session::BrowserSession;
use crate::srt_sender::SrtConfig;
use anyhow::Result;
use percent_encoding::percent_decode_str;
use std::future::Future;
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::{Mutex, Notify};
use wtransport::Endpoint;
use wtransport::Identity;
use wtransport::ServerConfig;

/// Default viewer cap.
const DEFAULT_MAX_VIEWERS: usize = 16;
/// Broadcast ring-buffer depth. At ~1700 msg/sec this is ~2.4s of buffer.
const DEFAULT_BROADCAST_CAPACITY: usize = 4096;

/// High-level SRT-over-WebTransport gateway.
///
/// Build with [`Gateway::builder`], set an ingester via [`Gateway::source_handle`],
/// then call [`Gateway::run`] to start the accept loop.
pub struct Gateway {
    inner: Arc<GatewayInner>,
    bind_addr: SocketAddr,
    identity: Identity,
    path: String,
    auth_token: Option<String>,
    allowed_origins: Vec<String>,
    srt_config: SrtConfig,
    health_port: u16,
    health_bind_addr: String,
    #[cfg(feature = "sim-loss")]
    sim_loss: u8,
    #[cfg(feature = "sim-loss")]
    sim_seed: u64,
}

struct GatewayInner {
    broadcaster: Mutex<Option<Arc<Broadcaster>>>,
    max_viewers: usize,
    broadcast_capacity: usize,
}

/// Builder for [`Gateway`].
pub struct GatewayBuilder {
    bind_addr: SocketAddr,
    identity: Option<Identity>,
    max_viewers: usize,
    broadcast_capacity: usize,
    srt_config: SrtConfig,
    path: String,
    auth_token: Option<String>,
    allowed_origins: Vec<String>,
    health_port: u16,
    health_bind_addr: String,
    #[cfg(feature = "sim-loss")]
    sim_loss: u8,
    #[cfg(feature = "sim-loss")]
    sim_seed: u64,
}

/// Handle for setting the source ingester on a [`Gateway`].
///
/// Obtained via [`Gateway::source_handle`] before calling `run`. The handle
/// can be moved into a background task to connect the source asynchronously
/// (e.g. waiting for OBS to connect).
pub struct GatewaySourceHandle {
    inner: Arc<GatewayInner>,
}

impl Gateway {
    /// Create a new builder.
    pub fn builder() -> GatewayBuilder {
        GatewayBuilder {
            bind_addr: "127.0.0.1:4433".parse().unwrap(),
            identity: None,
            max_viewers: DEFAULT_MAX_VIEWERS,
            broadcast_capacity: DEFAULT_BROADCAST_CAPACITY,
            srt_config: SrtConfig::default(),
            path: "/wt".to_string(),
            auth_token: None,
            allowed_origins: Vec::new(),
            health_port: 0,
            health_bind_addr: "127.0.0.1".to_string(),
            #[cfg(feature = "sim-loss")]
            sim_loss: 0,
            #[cfg(feature = "sim-loss")]
            sim_seed: 42,
        }
    }

    /// Get a handle for setting the source ingester.
    ///
    /// Call this before `run()`. The handle owns its own `Arc` clone, so it
    /// can be moved into a separate task.
    pub fn source_handle(&self) -> GatewaySourceHandle {
        GatewaySourceHandle {
            inner: self.inner.clone(),
        }
    }

    /// Run the WebTransport accept loop until `shutdown` completes.
    pub async fn run(self, shutdown: impl Future<Output = ()>) -> Result<()> {
        let config = ServerConfig::builder()
            .with_bind_address(self.bind_addr)
            .with_identity(self.identity)
            .build();

        let server = Endpoint::server(config)?;

        tracing::info!(
            addr = %self.bind_addr,
            path = %self.path,
            "WebTransport server listening"
        );

        let mut health_server_handle: Option<tokio::task::JoinHandle<()>> = None;
        if self.health_port > 0 {
            let inner = self.inner.clone();
            let health_port = self.health_port;
            let health_bind = self.health_bind_addr.clone();
            let health_handle = tokio::spawn(async move {
                let listener = match tokio::net::TcpListener::bind(
                    format!("{}:{}", health_bind, health_port)
                ).await {
                    Ok(l) => l,
                    Err(e) => {
                        tracing::warn!(?e, port = health_port, "health server bind failed");
                        return;
                    }
                };
                tracing::info!(port = health_port, "health server listening");
                loop {
                    match listener.accept().await {
                        Ok((stream, addr)) => {
                            let inner = inner.clone();
                            tokio::spawn(async move {
                                Self::handle_health(stream, addr, &inner).await;
                            });
                        }
                        Err(e) => {
                            tracing::warn!(?e, "health accept error");
                            continue;
                        }
                    }
                }
            });
            health_server_handle = Some(health_handle);
        }

        let session_handles: Arc<Mutex<Vec<(Arc<Notify>, tokio::task::JoinHandle<()>)>>> =
            Arc::new(Mutex::new(Vec::new()));

        // Centralized session registry + ticker. ONE ticker task drives all
        // sessions' SRT state machines, replacing N per-session sender_pump
        // tasks (each with its own 2ms interval timer).
        let registry = Arc::new(SessionRegistry::new());
        let ticker_shutdown = Arc::new(Notify::new());
        let ticker_registry = registry.clone();
        let ticker_shutdown_for_task = ticker_shutdown.clone();
        let ticker_handle = tokio::spawn(async move {
            run_ticker(ticker_registry, ticker_shutdown_for_task.notified()).await;
        });

        tokio::pin!(shutdown);

        loop {
            tokio::select! {
                _ = &mut shutdown => {
                    tracing::info!("shutdown signal received; draining");
                    break;
                }
                incoming_session = server.accept() => {
                    let session_request = match incoming_session.await {
                        Ok(req) => req,
                        Err(e) => {
                            tracing::warn!(?e, "incoming session failed");
                            continue;
                        }
                    };

                    let request_path = session_request.path();
                    let (path_only, query) = match request_path.find('?') {
                        Some(idx) => (&request_path[..idx], &request_path[idx + 1..]),
                        None => (request_path, ""),
                    };

                    tracing::info!(
                        path = path_only,
                        authority = session_request.authority(),
                        "WT session request"
                    );

                    if path_only != self.path {
                        session_request.not_found().await;
                        continue;
                    }

                    // Origin allowlist check
                    if !self.allowed_origins.is_empty() {
                        let origin_ok = session_request
                            .origin()
                            .map(|o| self.allowed_origins.iter().any(|allowed| allowed == o))
                            .unwrap_or(false);
                        if !origin_ok {
                            tracing::warn!("session rejected: origin not allowed");
                            session_request.not_found().await;
                            continue;
                        }
                    }

                    // Auth check
                    if let Some(ref expected_token) = self.auth_token {
                        let token_valid = query
                            .split('&')
                            .find_map(|kv| {
                                let mut parts = kv.splitn(2, '=');
                                if parts.next()? == "token" {
                                    Some(parts.next().unwrap_or(""))
                                } else {
                                    None
                                }
                            })
                            .map(|t| {
                                let decoded = percent_decode_str(t).decode_utf8_lossy();
                                constant_time_eq(decoded.as_bytes(), expected_token.as_bytes())
                            })
                            .unwrap_or(false);

                        if !token_valid {
                            tracing::warn!(path = %path_only, "session rejected: invalid or missing auth token");
                            session_request.not_found().await;
                            continue;
                        }
                    }

                    // Pre-accept check: reject up-front without consuming a viewer
                    // slot. The actual `subscribe()` happens after accept succeeds
                    // so a failed accept cannot briefly inflate/decrement the
                    // viewer count. `subscribe()` re-checks alive + cap after
                    // accept to close the gap between this check and the real
                    // subscription.
                    {
                        let guard = self.inner.broadcaster.lock().await;
                        match guard.as_ref() {
                            Some(b) if !b.is_alive() => {
                                drop(guard);
                                tracing::warn!("session rejected: source is dead");
                                session_request.too_many_requests().await;
                                continue;
                            }
                            Some(b) if b.viewer_count() >= self.inner.max_viewers => {
                                drop(guard);
                                tracing::warn!("session rejected: viewer cap reached");
                                session_request.too_many_requests().await;
                                continue;
                            }
                            None => {
                                drop(guard);
                                tracing::warn!("session rejected: source not ready yet");
                                session_request.too_many_requests().await;
                                continue;
                            }
                            _ => {}
                        }
                    }

                    let connection = match session_request.accept().await {
                        Ok(c) => c,
                        Err(e) => {
                            tracing::warn!(?e, "accept failed");
                            continue;
                        }
                    };

                    // Post-accept: actually subscribe. If this fails now (source
                    // died or cap filled between the pre-check and here), close
                    // the just-accepted connection rather than leaking it.
                    let viewer = {
                        let guard = self.inner.broadcaster.lock().await;
                        match guard.as_ref().and_then(|b| b.subscribe()) {
                            Some(v) => v,
                            None => {
                                drop(guard);
                                tracing::warn!(
                                    "post-accept subscribe failed; closing session"
                                );
                                connection.close(1u32.into(), b"subscribe failed");
                                continue;
                            }
                        }
                    };

                    let peer = connection.remote_address();
                    tracing::info!(%peer, "WT session established");

                    match connection.max_datagram_size() {
                        Some(max) if max >= 1200 => {
                            tracing::debug!(max, "WT datagram PMTU adequate");
                        }
                        Some(max) => {
                            tracing::warn!(
                                max,
                                required = 1200,
                                "WT datagram PMTU too small; closing session"
                            );
                            connection.close(
                                1u32.into(),
                                b"datagram PMTU too small for SRT payload",
                            );
                            continue;
                        }
                        None => {
                            tracing::warn!("WT datagrams unsupported by peer; closing session");
                            connection.close(1u32.into(), b"datagrams unsupported");
                            continue;
                        }
                    }

                    #[cfg(feature = "sim-loss")]
                    let (entry, handle) = BrowserSession::create(
                        connection, viewer,
                        self.sim_loss, self.sim_seed, self.srt_config.clone(),
                        None, None,
                    ).await;
                    #[cfg(not(feature = "sim-loss"))]
                    let (entry, handle) = BrowserSession::create(
                        connection, viewer,
                        0, 0, self.srt_config.clone(),
                        None, None,
                    ).await;
                    let session_shutdown = entry.shutdown.clone();
                    registry.insert(entry);
                    {
                        let mut handles = session_handles.lock().await;
                        handles.retain(|(_, h)| !h.is_finished());
                        handles.push((session_shutdown, handle));
                    }
                }
            }
        }

        // Stop the centralized ticker first so it doesn't keep ticking
        // sessions we're about to drain. Bounded wait: tick_all iterates all
        // sessions sequentially, so under heavy load this could take a while;
        // cap at 2s and move on.
        ticker_shutdown.notify_one();
        let _ = tokio::time::timeout(Duration::from_secs(2), ticker_handle).await;

        // Graceful drain: signal each session's shutdown Notify, wait up to
        // 3s for it to exit on its own, then fall back to abort.
        let handles = std::mem::take(&mut *session_handles.lock().await);
        if !handles.is_empty() {
            tracing::info!(count = handles.len(), "shutting down active sessions");
            // Signal graceful shutdown
            for (notify, _) in &handles {
                notify.notify_one();
            }
            // Drain all sessions concurrently with a global 3s deadline
            let drain_futures: Vec<_> = handles
                .into_iter()
                .map(|(_, mut handle)| async move {
                    tokio::select! {
                        _ = &mut handle => {}
                        _ = tokio::time::sleep(Duration::from_secs(3)) => {
                            tracing::warn!("session didn't drain in 3s; aborting");
                            handle.abort();
                            let _ = handle.await;
                        }
                    }
                })
                .collect();
            futures::future::join_all(drain_futures).await;
            tracing::info!("shutdown complete");
        }

        if let Some(h) = health_server_handle {
            h.abort();
        }

        Ok(())
    }

    async fn handle_health(
        mut stream: tokio::net::TcpStream,
        addr: std::net::SocketAddr,
        inner: &GatewayInner,
    ) {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};

        // Read and discard the HTTP request
        let mut buf = [0u8; 1024];
        let _ = tokio::time::timeout(std::time::Duration::from_millis(500), stream.read(&mut buf)).await;

        let (viewers, alive) = {
            let guard = inner.broadcaster.lock().await;
            match guard.as_ref() {
                Some(b) => (b.viewer_count(), b.is_alive()),
                None => (0usize, false),
            }
        };

        let json = format!(
            r#"{{"status":"{}","viewers":{},"max_viewers":{}}}"#,
            if alive { "ok" } else { "no_source" },
            viewers,
            inner.max_viewers,
        );
        let response = format!(
            "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
            json.len(),
            json,
        );
        let _ = stream.write_all(response.as_bytes()).await;
        let _ = stream.flush().await;
        let _ = addr;
    }
}

/// Single shared ticker task that drives every active session's SRT state
/// machine ~every 2ms. Replaces N per-session `sender_pump` tasks. Exits when
/// `shutdown` completes (signaled from `Gateway::run`'s drain path).
async fn run_ticker(registry: Arc<SessionRegistry>, shutdown: impl Future<Output = ()>) {
    let mut ticker = tokio::time::interval(Duration::from_millis(2));
    ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    let mut stats_interval = tokio::time::interval(Duration::from_secs(5));
    // Consume the immediate first tick so the first stats log is at t+5s.
    stats_interval.tick().await;

    tokio::pin!(shutdown);
    loop {
        tokio::select! {
            biased;
            _ = &mut shutdown => break,
            _ = stats_interval.tick() => {
                let active = registry.len();
                // Aggregate WT-RTT check: warn for sessions whose RTT suggests
                // the configured TSBPD latency is too low (4×RTT rule of thumb).
                let entries = registry.snapshot();
                let rtt_warn_count = entries.iter().filter(|e| {
                    let wt_rtt_ms = e.conn.rtt().as_secs_f64() * 1000.0;
                    wt_rtt_ms * 4.0 > e.latency_ms as f64
                }).count();
                tracing::info!(active, "ticker stats");
                if rtt_warn_count > 0 {
                    tracing::warn!(
                        rtt_warn_count,
                        "sessions with WT RTT > 4×latency; consider raising --latency"
                    );
                }
            }
            _ = ticker.tick() => {
                registry.tick_all().await;
            }
        }
    }
    tracing::info!("ticker task exiting");
}

impl GatewayBuilder {
    pub fn bind_addr(mut self, addr: impl Into<SocketAddr>) -> Self {
        self.bind_addr = addr.into();
        self
    }

    pub fn identity(mut self, identity: Identity) -> Self {
        self.identity = Some(identity);
        self
    }

    pub fn max_viewers(mut self, n: usize) -> Self {
        self.max_viewers = n.max(1);
        self
    }

    pub fn broadcast_capacity(mut self, n: usize) -> Self {
        self.broadcast_capacity = n.max(1);
        self
    }

    pub fn latency_ms(mut self, ms: u64) -> Self {
        let dur = std::time::Duration::from_millis(ms.max(10));
        self.srt_config.send_latency = dur;
        self.srt_config.recv_latency = dur;
        self
    }

    /// Set the full SRT protocol configuration. Overrides any prior `latency_ms`
    /// settings (and vice-versa — whichever is called last wins).
    pub fn srt_config(mut self, config: SrtConfig) -> Self {
        self.srt_config = config;
        self
    }

    pub fn path(mut self, path: impl Into<String>) -> Self {
        let p = path.into();
        self.path = if p.starts_with('/') { p } else { format!("/{p}") };
        self
    }

    pub fn auth_token(mut self, token: impl Into<String>) -> Self {
        self.auth_token = Some(token.into());
        self
    }

    /// Set allowed origins for WebTransport requests (e.g., `https://example.com`).
    /// When non-empty, requests whose Origin header doesn't match are rejected.
    /// When empty (default), all origins are allowed.
    pub fn allowed_origins(mut self, origins: Vec<String>) -> Self {
        self.allowed_origins = origins;
        self
    }

    /// Set the HTTP health/metrics port (0 to disable).
    pub fn health_port(mut self, port: u16) -> Self {
        self.health_port = port;
        self
    }

    /// Set the bind address for the HTTP health/metrics server.
    pub fn health_bind_addr(mut self, addr: impl Into<String>) -> Self {
        self.health_bind_addr = addr.into();
        self
    }

    /// Configure simulated packet loss for testing.
    #[cfg(feature = "sim-loss")]
    pub fn sim_loss(mut self, pct: u8, seed: u64) -> Self {
        self.sim_loss = pct;
        self.sim_seed = seed;
        self
    }

    /// Build the gateway. Returns error if identity was not set.
    pub fn build(self) -> Result<Gateway> {
        let identity = self.identity.ok_or_else(|| anyhow::anyhow!("identity must be set"))?;
        Ok(Gateway {
            inner: Arc::new(GatewayInner {
                broadcaster: Mutex::new(None),
                max_viewers: self.max_viewers,
                broadcast_capacity: self.broadcast_capacity,
            }),
            bind_addr: self.bind_addr,
            identity,
            path: self.path,
            auth_token: self.auth_token,
            allowed_origins: self.allowed_origins,
            srt_config: self.srt_config,
            health_port: self.health_port,
            health_bind_addr: self.health_bind_addr,
            #[cfg(feature = "sim-loss")]
            sim_loss: self.sim_loss,
            #[cfg(feature = "sim-loss")]
            sim_seed: self.sim_seed,
        })
    }
}

impl GatewaySourceHandle {
    /// Set the source ingester. Creates a [`Broadcaster`] and makes it
    /// immediately available to new viewer sessions.
    pub async fn set_ingester<I: Ingester + Send + 'static>(&self, ingester: I) {
        let b = Broadcaster::spawn(
            ingester,
            self.inner.max_viewers,
            self.inner.broadcast_capacity,
        );
        *self.inner.broadcaster.lock().await = Some(b);
    }
}

/// Constant-time byte comparison to prevent timing side-channel attacks
/// on auth token validation.
fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    let mut result: u8 = 0;
    for (x, y) in a.iter().zip(b.iter()) {
        result |= x ^ y;
    }
    result == 0
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn constant_time_eq_equal() {
        assert!(constant_time_eq(b"hello", b"hello"));
    }

    #[test]
    fn constant_time_eq_different() {
        assert!(!constant_time_eq(b"hello", b"world"));
    }

    #[test]
    fn constant_time_eq_different_lengths() {
        assert!(!constant_time_eq(b"short", b"longer_string"));
    }

    #[test]
    fn constant_time_eq_empty() {
        assert!(constant_time_eq(b"", b""));
    }

    #[test]
    fn builder_max_viewers_clamps_to_1() {
        let builder = Gateway::builder()
            .max_viewers(0)
            .identity(Identity::self_signed(&["localhost".to_string()]).unwrap());
        let gateway = builder.build().unwrap();
        assert_eq!(gateway.inner.max_viewers, 1);
    }

    #[test]
    fn builder_broadcast_capacity_clamps_to_1() {
        let builder = Gateway::builder()
            .broadcast_capacity(0)
            .identity(Identity::self_signed(&["localhost".to_string()]).unwrap());
        let gateway = builder.build().unwrap();
        assert_eq!(gateway.inner.broadcast_capacity, 1);
    }

    #[test]
    fn builder_latency_clamps_to_10() {
        let builder = Gateway::builder()
            .latency_ms(0)
            .identity(Identity::self_signed(&["localhost".to_string()]).unwrap());
        let gateway = builder.build().unwrap();
        assert!(gateway.srt_config.send_latency.as_millis() >= 10);
    }

    #[test]
    fn builder_path_prepends_slash() {
        let builder = Gateway::builder()
            .path("stream")
            .identity(Identity::self_signed(&["localhost".to_string()]).unwrap());
        let gateway = builder.build().unwrap();
        assert_eq!(gateway.path, "/stream");
    }

    #[test]
    fn builder_path_with_slash_unchanged() {
        let builder = Gateway::builder()
            .path("/stream")
            .identity(Identity::self_signed(&["localhost".to_string()]).unwrap());
        let gateway = builder.build().unwrap();
        assert_eq!(gateway.path, "/stream");
    }

    #[test]
    fn build_fails_without_identity() {
        let result = Gateway::builder().build();
        assert!(result.is_err());
    }

    #[test]
    fn health_bind_addr_defaults_to_localhost() {
        let builder = Gateway::builder()
            .identity(Identity::self_signed(&["localhost".to_string()]).unwrap());
        let gateway = builder.build().unwrap();
        assert_eq!(gateway.health_bind_addr, "127.0.0.1");
    }
}
