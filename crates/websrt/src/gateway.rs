//! High-level WebTransport gateway: accept loop, session spawning, fanout.
//!
//! Wraps the lower-level building blocks (`Broadcaster`, `BrowserSession`,
//! `SrtInitiator`) into a single run-loop. The caller provides an `Ingester`
//! (either immediately or deferred via `source_handle`).

use crate::broadcaster::Broadcaster;
use crate::ingest::Ingester;
use crate::session::BrowserSession;
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
    latency_ms: u64,
    health_port: u16,
    #[cfg(feature = "sim-loss")]
    sim_loss: u8,
    #[cfg(feature = "sim-loss")]
    sim_seed: u64,
}

struct GatewayInner {
    broadcaster: Mutex<Option<Arc<Broadcaster>>>,
    max_viewers: usize,
    broadcast_capacity: usize,
    health_port: u16,
}

/// Builder for [`Gateway`].
pub struct GatewayBuilder {
    bind_addr: SocketAddr,
    identity: Option<Identity>,
    max_viewers: usize,
    broadcast_capacity: usize,
    latency_ms: u64,
    path: String,
    auth_token: Option<String>,
    allowed_origins: Vec<String>,
    health_port: u16,
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
            latency_ms: 300,
            path: "/wt".to_string(),
            auth_token: None,
            allowed_origins: Vec::new(),
            health_port: 0,
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

        if self.health_port > 0 {
            let inner = self.inner.clone();
            tokio::spawn(async move {
                let listener = match std::net::TcpListener::bind(format!("0.0.0.0:{}", inner.health_port)) {
                    Ok(l) => l,
                    Err(e) => {
                        tracing::warn!(?e, port = inner.health_port, "health server bind failed");
                        return;
                    }
                };
                tracing::info!(port = inner.health_port, "health server listening");
                loop {
                    let (stream, addr) = match listener.accept() {
                        Ok(c) => c,
                        Err(e) => {
                            tracing::warn!(?e, "health accept error");
                            continue;
                        }
                    };
                    let inner = inner.clone();
                    tokio::spawn(async move {
                        Self::handle_health(stream, addr, &inner).await;
                    });
                }
            });
        }

        let session_handles: Arc<Mutex<Vec<(Arc<Notify>, tokio::task::JoinHandle<()>)>>> =
            Arc::new(Mutex::new(Vec::new()));

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
                    let (session_shutdown, handle) = BrowserSession::spawn(
                        connection, viewer,
                        self.sim_loss, self.sim_seed, self.latency_ms,
                    );
                    #[cfg(not(feature = "sim-loss"))]
                    let (session_shutdown, handle) = BrowserSession::spawn(
                        connection, viewer,
                        0, 0, self.latency_ms,
                    );
                    {
                        let mut handles = session_handles.lock().await;
                        handles.retain(|(_, h)| !h.is_finished());
                        handles.push((session_shutdown, handle));
                    }
                }
            }
        }

        // Graceful drain: signal each session's shutdown Notify, wait up to
        // 3s for it to exit on its own, then fall back to abort.
        let handles = std::mem::take(&mut *session_handles.lock().await);
        if !handles.is_empty() {
            tracing::info!(count = handles.len(), "shutting down active sessions");
            // Signal graceful shutdown
            for (notify, _) in &handles {
                notify.notify_one();
            }
            // Wait up to 3s per session, then abort. We use select! on
            // `&mut handle` (instead of `tokio::time::timeout`, which would
            // move the JoinHandle) so we can still call `.abort()` + `.await`
            // after the deadline elapses.
            for (_notify, mut handle) in handles {
                tokio::select! {
                    _ = &mut handle => {}
                    _ = tokio::time::sleep(Duration::from_secs(3)) => {
                        tracing::warn!("session didn't drain in 3s; aborting");
                        handle.abort();
                        let _ = handle.await;
                    }
                }
            }
            tracing::info!("shutdown complete");
        }

        Ok(())
    }

    async fn handle_health(
        stream: std::net::TcpStream,
        addr: std::net::SocketAddr,
        inner: &GatewayInner,
    ) {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let mut stream = tokio::net::TcpStream::from_std(stream).unwrap();

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
        self.max_viewers = n;
        self
    }

    pub fn broadcast_capacity(mut self, n: usize) -> Self {
        self.broadcast_capacity = n;
        self
    }

    pub fn latency_ms(mut self, ms: u64) -> Self {
        self.latency_ms = ms;
        self
    }

    pub fn path(mut self, path: impl Into<String>) -> Self {
        self.path = path.into();
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

    /// Configure simulated packet loss for testing.
    #[cfg(feature = "sim-loss")]
    pub fn sim_loss(mut self, pct: u8, seed: u64) -> Self {
        self.sim_loss = pct;
        self.sim_seed = seed;
        self
    }

    /// Build the gateway. Panics if identity was not set.
    pub fn build(self) -> Gateway {
        Gateway {
            inner: Arc::new(GatewayInner {
                broadcaster: Mutex::new(None),
                max_viewers: self.max_viewers,
                broadcast_capacity: self.broadcast_capacity,
                health_port: self.health_port,
            }),
            bind_addr: self.bind_addr,
            identity: self.identity.expect("identity must be set"),
            path: self.path,
            auth_token: self.auth_token,
            allowed_origins: self.allowed_origins,
            latency_ms: self.latency_ms,
            health_port: self.health_port,
            #[cfg(feature = "sim-loss")]
            sim_loss: self.sim_loss,
            #[cfg(feature = "sim-loss")]
            sim_seed: self.sim_seed,
        }
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
