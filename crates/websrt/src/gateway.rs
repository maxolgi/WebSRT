//! High-level WebTransport gateway: accept loop, session spawning, fanout.
//!
//! Wraps the lower-level building blocks (`Broadcaster`, `BrowserSession`,
//! `SrtInitiator`) into a single run-loop. The caller provides an `Ingester`
//! (either immediately or deferred via `source_handle`).

use crate::ingest::{Ingester, TsMessage};
use crate::registry::SessionRegistry;
use crate::session::BrowserSession;
use crate::srt_sender::SrtConfig;
use crate::stream_registry::{StreamRegistry, StreamStats};
use anyhow::Result;
use std::future::Future;
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::mpsc;
use tokio::sync::Notify;
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
    streams: Arc<StreamRegistry>,
    bind_addr: SocketAddr,
    identity: Identity,
    policy: Arc<dyn crate::hooks::SessionPolicy>,
    srt_config: SrtConfig,
    sim_loss: u8,
    sim_seed: u64,
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
    session_policy: Option<Arc<dyn crate::hooks::SessionPolicy>>,
    sim_loss: u8,
    sim_seed: u64,
}

/// Handle for setting the source ingester on a [`Gateway`].
///
/// Obtained via [`Gateway::source_handle`] before calling `run`. The handle
/// can be moved into a background task to connect the source asynchronously
/// (e.g. waiting for OBS to connect).
pub struct GatewaySourceHandle {
    streams: Arc<StreamRegistry>,
}

/// Snapshot of gateway-wide stats. Returned by [`GatewayStatsHandle::stats`].
#[derive(Debug, Clone)]
pub struct GatewayStats {
    /// Total registered streams (alive or dead).
    pub streams: usize,
    /// Streams whose source is still producing.
    pub alive_streams: usize,
    /// Sum of viewer counts across all streams.
    pub total_viewers: usize,
    /// Configured per-stream viewer cap.
    pub max_viewers: usize,
    /// Configured broadcast ring-buffer depth.
    pub broadcast_capacity: usize,
    /// Per-stream snapshot, sorted by name.
    pub per_stream: Vec<StreamStats>,
}

/// Handle for reading gateway stats after [`Gateway::run`] has consumed the gateway.
///
/// Obtained via [`Gateway::stats_handle`] before calling `run`. The handle
/// owns its own `Arc` clone, so it can be moved into a separate task (e.g. a
/// health/metrics HTTP server spawned by the embedding application).
pub struct GatewayStatsHandle {
    streams: Arc<StreamRegistry>,
}

impl GatewayStatsHandle {
    /// Read the current gateway stats. Cheap to call — locks the stream
    /// registry briefly, no I/O.
    pub fn stats(&self) -> GatewayStats {
        GatewayStats {
            streams: self.streams.stream_count(),
            alive_streams: self.streams.alive_stream_count(),
            total_viewers: self.streams.total_viewers(),
            max_viewers: self.streams.max_viewers(),
            broadcast_capacity: self.streams.broadcast_capacity(),
            per_stream: self.streams.snapshot_streams(),
        }
    }
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
            session_policy: None,
            sim_loss: 0,
            sim_seed: 0,
        }
    }

    /// Get a handle for setting the source ingester.
    ///
    /// Call this before `run()`. The handle owns its own `Arc` clone, so it
    /// can be moved into a separate task.
    pub fn source_handle(&self) -> GatewaySourceHandle {
        GatewaySourceHandle {
            streams: self.streams.clone(),
        }
    }

    /// Get a handle for reading gateway stats.
    ///
    /// Call this before `run()`. The handle owns its own `Arc` clone, so it
    /// can be moved into a separate task (e.g. a health/metrics HTTP server).
    pub fn stats_handle(&self) -> GatewayStatsHandle {
        GatewayStatsHandle {
            streams: self.streams.clone(),
        }
    }

    /// Run the WebTransport accept loop until `shutdown` completes.
    pub async fn run(self, shutdown: impl Future<Output = ()>) -> Result<()> {
        let config = ServerConfig::builder()
            .with_bind_address(self.bind_addr)
            .with_identity(self.identity)
            .max_idle_timeout(Some(Duration::from_secs(10)))
            .map_err(|e| anyhow::anyhow!("invalid idle timeout: {e}"))?
            .build();

        let server = Endpoint::server(config)?;

        tracing::info!(
            addr = %self.bind_addr,
            "WebTransport server listening"
        );

        let mut session_handles: Vec<(Arc<Notify>, tokio::task::JoinHandle<()>)> = Vec::new();

        // Centralized session registry + ticker. ONE ticker task drives all
        // sessions' SRT state machines (~2ms cadence).
        let registry = Arc::new(SessionRegistry::new());
        let ticker_shutdown = Arc::new(Notify::new());
        let ticker_registry = registry.clone();
        let ticker_streams = self.streams.clone();
        let ticker_shutdown_for_task = ticker_shutdown.clone();
        let ticker_handle = tokio::spawn(async move {
            run_ticker(ticker_registry, ticker_streams, ticker_shutdown_for_task.notified()).await;
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

                    // Build a SessionRequest and ask the policy.
                    let origin_header = session_request.origin();
                    let session_req = crate::hooks::SessionRequest {
                        path: path_only,
                        query,
                        origin: origin_header,
                        authority: session_request.authority(),
                        remote_address: session_request.remote_address(),
                    };
                    match self.policy.decide(&session_req) {
                        crate::hooks::Decision::Accept => {}
                        crate::hooks::Decision::Reject => {
                            tracing::info!(
                                path = path_only,
                                origin = ?origin_header,
                                "session rejected by policy"
                            );
                            session_request.not_found().await;
                            continue;
                        }
                    }

                    // Parse stream routing from query params.
                    //   ?stream=<name> or ?subscribe=<name> → view this stream
                    //   ?publish=<name>                     → publish this stream
                    // A session may both publish and view (different streams).
                    let stream_name = crate::hooks::parse_query_param(query, "stream")
                        .or_else(|| crate::hooks::parse_query_param(query, "subscribe"))
                        .unwrap_or_else(|| "default".to_string());
                    let publish_name = crate::hooks::parse_query_param(query, "publish");

                    // Pre-accept check: for viewer-only sessions, reject
                    // up-front if the requested stream isn't available (no
                    // source yet, or source ended). This avoids a wasted WT
                    // handshake. Publishing sessions skip this — they create
                    // the stream after accept.
                    if publish_name.is_none() && !self.streams.is_alive(&stream_name) {
                        tracing::warn!(stream = %stream_name, "session rejected: stream not available");
                        session_request.not_found().await;
                        continue;
                    }

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

                    // Resolve the viewer subscription and/or publish channel
                    // after a successful accept + PMTU check. `subscribe()`
                    // re-checks alive + per-stream cap atomically; if it fails
                    // now (source died or cap filled between the pre-check and
                    // here) close the just-accepted connection rather than
                    // leaking it.
                    let (viewer, publish_tx) = match &publish_name {
                        Some(pub_name) => {
                            // Publishing session: create the stream channel.
                            let tx = self.streams.publish(pub_name);
                            // Optionally also view a different stream.
                            let view = if pub_name.as_str() != stream_name.as_str() {
                                self.streams.subscribe(&stream_name)
                            } else {
                                None
                            };
                            (view, Some(tx))
                        }
                        None => match self.streams.subscribe(&stream_name) {
                            Some(v) => (Some(v), None),
                            None => {
                                tracing::warn!(
                                    stream = %stream_name,
                                    "post-accept subscribe failed; closing session"
                                );
                                connection.close(1u32.into(), b"stream not found");
                                continue;
                            }
                        },
                    };

                    let (entry, handle) = BrowserSession::create(
                        connection, viewer,
                        self.sim_loss, self.sim_seed, self.srt_config.clone(),
                        publish_tx,
                    ).await;
                    let session_shutdown = entry.shutdown.clone();
                    registry.insert(entry);
                    session_handles.retain(|(_, h)| !h.is_finished());
                    session_handles.push((session_shutdown, handle));
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
        let handles = std::mem::take(&mut session_handles);
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

        Ok(())
    }
}

/// Single shared ticker task that drives every active session's SRT state
/// machine ~every 2ms. Also periodically prunes dead streams from the
/// registry. Exits when `shutdown` completes (signaled from `Gateway::run`'s
/// drain path).
async fn run_ticker(
    registry: Arc<SessionRegistry>,
    streams: Arc<StreamRegistry>,
    shutdown: impl Future<Output = ()>,
) {
    let mut ticker = tokio::time::interval(Duration::from_millis(2));
    ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    let mut cleanup_interval = tokio::time::interval(Duration::from_secs(60));
    cleanup_interval.tick().await; // consume immediate first tick

    tokio::pin!(shutdown);
    loop {
        tokio::select! {
            biased;
            _ = &mut shutdown => break,
            _ = cleanup_interval.tick() => {
                let before = streams.stream_count();
                streams.cleanup();
                let after = streams.stream_count();
                if before != after {
                    tracing::info!(before, after, "stream registry cleanup removed dead streams");
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

    /// Set a custom session-acceptance policy. When set, this REPLACES the
    /// default path/origin/auth_token checks — call `chain()` yourself if you
    /// want to layer your policy on top of the built-ins.
    ///
    /// ```no_run
    /// use websrt::Gateway;
    /// use websrt::hooks::{chain, path_policy, auth_token_policy};
    /// use wtransport::Identity;
    ///
    /// # fn main() -> anyhow::Result<()> {
    /// let gateway = Gateway::builder()
    ///     .identity(Identity::self_signed(&["localhost".to_string()])?)
    ///     .session_policy(chain(
    ///         path_policy("/wt".into()),
    ///         auth_token_policy("s3cret".into()),
    ///     ))
    ///     .build()?;
    /// # Ok(())
    /// # }
    /// ```
    pub fn session_policy<P: crate::hooks::SessionPolicy>(mut self, policy: P) -> Self {
        self.session_policy = Some(Arc::new(policy));
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
        let streams = Arc::new(StreamRegistry::new(self.max_viewers, self.broadcast_capacity));

        // Construct the session policy. If `session_policy` was explicitly set,
        // use it directly. Otherwise build a default chain from path/origin/auth_token.
        let policy: Arc<dyn crate::hooks::SessionPolicy> = match self.session_policy {
            Some(p) => p,
            None => {
                // Always check the path.
                let base: Arc<dyn crate::hooks::SessionPolicy> =
                    Arc::new(crate::hooks::path_policy(self.path.clone()));
                // Conditionally add origin allowlist.
                let base = if self.allowed_origins.is_empty() {
                    base
                } else {
                    Arc::new(crate::hooks::chain(
                        base,
                        crate::hooks::origin_allowlist_policy(self.allowed_origins.clone()),
                    ))
                };
                // Conditionally add auth token.
                let base = match self.auth_token {
                    Some(token) => Arc::new(crate::hooks::chain(
                        base,
                        crate::hooks::auth_token_policy(token),
                    )),
                    None => base,
                };
                base
            }
        };

        Ok(Gateway {
            streams,
            bind_addr: self.bind_addr,
            identity,
            policy,
            srt_config: self.srt_config,
            sim_loss: self.sim_loss,
            sim_seed: self.sim_seed,
        })
    }
}

impl GatewaySourceHandle {
    /// Publish a stream from an external ingester (e.g. SRT from OBS, a file
    /// fixture). The stream is immediately available for viewers to subscribe
    /// under `name`. Replaces any previously registered stream of the same
    /// name.
    pub fn publish_stream<I: Ingester + Send + 'static>(&self, name: &str, ingester: I) {
        self.streams.publish_ingester(name, ingester);
    }

    /// Create a channel-backed stream for browser upstream sessions. Returns
    /// the [`mpsc::Sender`] for pushing TS messages into the stream. The stream
    /// is immediately available for viewers to subscribe under `name`.
    pub fn publish(&self, name: &str) -> mpsc::Sender<TsMessage> {
        self.streams.publish(name)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn builder_max_viewers_clamps_to_1() {
        let builder = Gateway::builder()
            .max_viewers(0)
            .identity(Identity::self_signed(&["localhost".to_string()]).unwrap());
        let gateway = builder.build().unwrap();
        assert_eq!(gateway.streams.max_viewers(), 1);
    }

    #[test]
    fn builder_broadcast_capacity_clamps_to_1() {
        let builder = Gateway::builder()
            .broadcast_capacity(0)
            .identity(Identity::self_signed(&["localhost".to_string()]).unwrap());
        let gateway = builder.build().unwrap();
        assert_eq!(gateway.streams.broadcast_capacity(), 1);
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
    fn build_fails_without_identity() {
        let result = Gateway::builder().build();
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn stats_handle_reports_streams_and_viewers() {
        let gateway = Gateway::builder()
            .identity(Identity::self_signed(&["localhost".to_string()]).unwrap())
            .build()
            .unwrap();

        // Publish a stream via the source handle, keeping the Sender alive so
        // the broadcaster stays alive while we read stats. (FiniteIngester
        // drains in microseconds, racing the assertion.)
        let source = gateway.source_handle();
        let _tx = source.publish("test-stream");

        // Give the broadcaster task a moment to start.
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;

        let stats_handle = gateway.stats_handle();
        let stats = stats_handle.stats();
        assert!(stats.streams >= 1, "should report at least one stream");
        assert!(stats.alive_streams >= 1, "stream should be alive");
        let test_stream = stats.per_stream.iter().find(|s| s.name == "test-stream");
        assert!(test_stream.is_some(), "test-stream should be in the snapshot");
        assert!(test_stream.unwrap().alive, "test-stream should be alive");
    }
}
