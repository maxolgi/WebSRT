//! High-level WebTransport gateway: accept loop, session spawning, fanout.
//!
//! Wraps the lower-level building blocks (`Broadcaster`, `BrowserSession`,
//! `SrtInitiator`) into a single run-loop. The caller provides an `Ingester`
//! (either immediately or deferred via `source_handle`).

use crate::broadcaster::Broadcaster;
use crate::ingest::Ingester;
use crate::session::BrowserSession;
use anyhow::Result;
use std::future::Future;
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::Mutex;
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
    latency_ms: u64,
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
    latency_ms: u64,
    path: String,
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

        let session_handles: Arc<Mutex<Vec<tokio::task::JoinHandle<()>>>> =
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

                    tracing::info!(
                        path = session_request.path(),
                        authority = session_request.authority(),
                        "WT session request"
                    );

                    if session_request.path() != self.path {
                        session_request.not_found().await;
                        continue;
                    }

                    let viewer = {
                        let guard = self.inner.broadcaster.lock().await;
                        match guard.as_ref() {
                            Some(b) if !b.is_alive() => {
                                drop(guard);
                                tracing::warn!("session rejected: source is dead");
                                session_request.too_many_requests().await;
                                continue;
                            }
                            Some(b) => match b.subscribe() {
                                Some(v) => v,
                                None => {
                                    drop(guard);
                                    tracing::warn!("session rejected: viewer cap reached");
                                    session_request.too_many_requests().await;
                                    continue;
                                }
                            },
                            None => {
                                drop(guard);
                                tracing::warn!("session rejected: source not ready yet");
                                session_request.too_many_requests().await;
                                continue;
                            }
                        }
                    };

                    let connection = match session_request.accept().await {
                        Ok(c) => c,
                        Err(e) => {
                            tracing::warn!(?e, "accept failed");
                            continue;
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
                                0u32.into(),
                                b"datagram PMTU too small for SRT payload",
                            );
                            continue;
                        }
                        None => {
                            tracing::warn!("WT datagrams unsupported by peer; closing session");
                            connection.close(0u32.into(), b"datagrams unsupported");
                            continue;
                        }
                    }

                    #[cfg(feature = "sim-loss")]
                    let handle = BrowserSession::spawn(
                        connection, viewer,
                        self.sim_loss, self.sim_seed, self.latency_ms,
                    );
                    #[cfg(not(feature = "sim-loss"))]
                    let handle = BrowserSession::spawn(
                        connection, viewer,
                        0, 0, self.latency_ms,
                    );
                    session_handles.lock().await.push(handle);
                }
            }
        }

        // Graceful drain: abort all session tasks.
        let handles = std::mem::take(&mut *session_handles.lock().await);
        if !handles.is_empty() {
            tracing::info!(count = handles.len(), "draining active sessions");
            for h in &handles {
                h.abort();
            }
            tokio::time::sleep(Duration::from_secs(2)).await;
            tracing::info!("drain complete");
        }

        Ok(())
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
            }),
            bind_addr: self.bind_addr,
            identity: self.identity.expect("identity must be set"),
            path: self.path,
            latency_ms: self.latency_ms,
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
