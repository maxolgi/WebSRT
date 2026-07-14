//! WebTransport server: accept loop, datagram driver.

use crate::broadcaster::Broadcaster;
use crate::cert::Cert;
use crate::ingest::file::FileIngester;
use crate::ingest::srt::SrtIngester;
use crate::ingest::Ingester;
use crate::session::BrowserSession;
use crate::Cli;
use anyhow::Result;
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::{oneshot, Mutex};
use wtransport::Endpoint;
use wtransport::ServerConfig;

/// Default viewer cap. Override with `--max-viewers N`.
const DEFAULT_MAX_VIEWERS: usize = 16;
/// Broadcast ring-buffer depth. At ~1700 msg/sec this is ~2.4s of buffer.
const BROADCAST_CAPACITY: usize = 4096;

pub async fn run(cert: Cert, cli: Cli) -> Result<()> {
    let bind_addr: SocketAddr = format!("{}:{}", cli.bind, cli.wt_port).parse()?;

    let config = ServerConfig::builder()
        .with_bind_address(bind_addr)
        .with_identity(cert.identity.clone_identity())
        .build();

    let server = Endpoint::server(config)?;

    tracing::info!("WebTransport server listening on https://{}/wt", bind_addr);

    // Build the source ingester. For `--input srt`, the SRT listener bind
    // happens in a background task so we don't block browser accepts waiting
    // for OBS to connect — browsers can queue up and will start receiving as
    // soon as the source is live.
    let broadcaster: Arc<Mutex<Option<Arc<Broadcaster>>>> = Arc::new(Mutex::new(None));
    match cli.input {
        crate::InputMode::File => {
            let ingester = FileIngester::new(&cli.fixture, cli.fixture_duration).map_err(|e| {
                tracing::error!(?e, "failed to open fixture; pass --fixture <path>");
                e
            })?;
            let b = Broadcaster::spawn(ingester, DEFAULT_MAX_VIEWERS, BROADCAST_CAPACITY);
            *broadcaster.lock().await = Some(b);
            tracing::info!(input = ?cli.input, fixture = ?cli.fixture, "ingester ready");
        }
        crate::InputMode::Srt => {
            let (tx, rx) = oneshot::channel::<Arc<Broadcaster>>();
            let max_viewers = DEFAULT_MAX_VIEWERS;
            let capacity = BROADCAST_CAPACITY;
            let srt_mode = cli.srt_mode;
            let srt_port = cli.srt_port;
            let srt_call = cli.srt_call.clone();
            tokio::spawn(async move {
                let ingester_result = match srt_mode {
                    crate::SrtMode::Listener => {
                        tracing::info!(port = srt_port, "binding SRT listener for OBS");
                        SrtIngester::bind(srt_port).await
                    }
                    crate::SrtMode::Caller => {
                        let addr = srt_call.unwrap_or_else(|| "127.0.0.1:9000".to_string());
                        tracing::info!(%addr, "SRT caller mode: dialing OBS");
                        SrtIngester::call(&addr).await
                    }
                };
                match ingester_result {
                    Ok(ingester) => {
                        tracing::info!("OBS connected; starting broadcaster");
                        let b = Broadcaster::spawn(ingester, max_viewers, capacity);
                        let _ = tx.send(b);
                    }
                    Err(e) => {
                        tracing::error!(?e, "SRT ingester setup failed");
                    }
                }
            });
            // Asynchronously wait for the ingester to come up, then publish
            // the broadcaster so viewer sessions can subscribe.
            let bc_clone = broadcaster.clone();
            tokio::spawn(async move {
                if let Ok(b) = rx.await {
                    *bc_clone.lock().await = Some(b);
                } else {
                    tracing::error!("ingester task disappeared");
                }
            });
        }
    }

    // Ctrl-C handler: graceful drain.
    let shutdown = tokio::signal::ctrl_c();
    tokio::pin!(shutdown);

    let session_handles: Arc<Mutex<Vec<tokio::task::JoinHandle<()>>>> =
        Arc::new(Mutex::new(Vec::new()));

    loop {
        tokio::select! {
            _ = &mut shutdown => {
                tracing::info!("ctrl-c received; shutting down");
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

                if session_request.path() != "/wt" {
                    session_request.not_found().await;
                    continue;
                }

                // Wait on the broadcaster (None until OBS connects in --input srt mode).
                let viewer = {
                    let guard = broadcaster.lock().await;
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

                let handle = BrowserSession::spawn(connection, viewer, cli.sim_loss, cli.sim_seed, cli.latency);
                session_handles.lock().await.push(handle);
            }
        }
    }

    // Graceful drain: abort all session tasks so their connections close.
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

// Keep the Ingester trait visible for downstream tooling.
#[allow(dead_code)]
fn _ingester_trait_anchor<I: Ingester>(_i: I) {}
