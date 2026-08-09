//! Demo gateway binary: CLI parse → cert setup → Gateway::run().
//!
//! By default launches a GUI (eframe/egui) with a config form and Start/Stop
//! buttons. Use `--no-gui` for the original headless CLI behavior.
//!
//! This is the reference application built on the `websrt` library.
//! For embedding, use the library crate directly.

mod gui;
mod log_buffer;
mod web_server;

use anyhow::{Context, Result};
use clap::{Parser, ValueEnum};
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use tokio::sync::Notify;
use tokio::task::JoinHandle;
use tracing_subscriber::EnvFilter;
use tracing_subscriber::layer::SubscriberExt as _;
use tracing_subscriber::util::SubscriberInitExt as _;
use websrt::cert::{Cert, CertSource};
use websrt::ingest::file::FileIngester;
use websrt::ingest::srt::SrtIngester;
use websrt::ingest::{SrtListenerService, TsContinuityChecker, TsStatsHandle};
use websrt::{Gateway, GatewayStatsHandle};

use log_buffer::{BufferMaker, LogBuffer};

#[derive(Debug, Clone, Copy, ValueEnum, PartialEq, serde::Serialize, serde::Deserialize)]
pub enum CertMode {
    /// Self-signed ECDSA P-256, regenerated each boot.
    Self_,
    /// PEM files on disk, e.g. produced by `mkcert`.
    Mkcert,
}

#[derive(Debug, Clone, Copy, ValueEnum, PartialEq)]
pub enum InputMode {
    /// Read fixtures/test.ts, pace at real-time, loop.
    File,
    /// SRT ingest from OBS (listener or caller mode).
    Srt,
}

#[derive(Debug, Clone, Copy, ValueEnum, PartialEq)]
pub enum SrtMode {
    /// Gateway listens; OBS calls in with ?mode=caller.
    Listener,
    /// Gateway dials OBS; OBS must be in ?mode=listener.
    Caller,
}

#[derive(Parser, Debug, Clone)]
#[command(name = "websrt-gateway", version, about = "SRT → WebTransport gateway")]
pub struct Cli {
    /// Skip the GUI and run in headless CLI mode (original behavior).
    #[arg(long)]
    pub no_gui: bool,

    /// Disable the built-in HTTPS web server (use Vite dev server instead).
    #[arg(long)]
    pub no_web: bool,

    /// HTTPS port for the built-in web server (0 to disable).
    #[arg(long, default_value_t = 5173u16)]
    pub web_port: u16,

    /// Bind address for the HTTPS web server.
    #[arg(long, default_value = "127.0.0.1")]
    pub web_bind: String,

    /// Root directory for web files (auto-detected: web/dist → web if unset).
    #[arg(long)]
    pub web_root: Option<PathBuf>,

    /// Input source.
    #[arg(long, value_enum, default_value_t = InputMode::File)]
    pub input: InputMode,

    /// Path to .ts fixture (when --input file).
    #[arg(long, default_value = "fixtures/test.ts")]
    pub fixture: PathBuf,

    /// Duration of the fixture in seconds (for real-time pacing).
    #[arg(long, default_value_t = 10.0)]
    pub fixture_duration: f64,

    /// SRT listen port (when --input srt --srt-mode listener).
    #[arg(long, default_value_t = 9000u16)]
    pub srt_port: u16,

    /// SRT connection mode.
    #[arg(long, value_enum, default_value_t = SrtMode::Listener)]
    pub srt_mode: SrtMode,

    /// Address to dial when --srt-mode caller (e.g. "192.168.1.3:1234").
    #[arg(long)]
    pub srt_call: Option<String>,

    /// SRT stream id. Listener mode: only accept connections matching this id.
    /// Caller mode: sent to OBS during connection.
    #[arg(long)]
    pub srt_streamid: Option<String>,

    /// WebTransport listen port.
    #[arg(long, default_value_t = 4433u16)]
    pub wt_port: u16,

    /// Bind address for WebTransport.
    #[arg(long, default_value = "127.0.0.1")]
    pub bind: String,

    /// Cert strategy.
    #[arg(long, value_enum, default_value_t = CertMode::Self_)]
    pub cert_mode: CertMode,

    /// PEM cert path (mkcert mode).
    #[arg(long)]
    pub cert_pem: Option<PathBuf>,

    /// PEM key path (mkcert mode).
    #[arg(long)]
    pub key_pem: Option<PathBuf>,

    /// Simulate N% random datagram loss (0-100). 0 disables.
    #[cfg(feature = "sim-loss")]
    #[arg(long, default_value_t = 0u8, value_parser = clap::value_parser!(u8).range(0..=100))]
    pub sim_loss: u8,

    /// RNG seed for sim-loss (deterministic by default).
    #[cfg(feature = "sim-loss")]
    #[arg(long, default_value_t = 42u64)]
    pub sim_seed: u64,

    /// SRT TSBPD latency for OBS input, in milliseconds.
    #[arg(long, default_value_t = 120u64)]
    pub latency: u64,

    /// SRT encryption passphrase for the OBS leg (10–79 chars).
    /// If set, AES encryption is negotiated on the SRT connection.
    #[arg(long)]
    pub srt_passphrase: Option<String>,

    /// Health/metrics HTTP port (0 to disable).
    #[arg(long, default_value_t = 0u16)]
    pub health_port: u16,

    /// Bind address for the HTTP health/metrics server (when --health-port > 0).
    #[arg(long, default_value = "127.0.0.1")]
    pub health_bind: String,

    /// Auth token for viewer connections. If set, browsers must pass ?token=<value>.
    /// If not set, authentication is disabled.
    #[arg(long)]
    pub auth_token: Option<String>,
}

fn main() -> Result<()> {
    // Install the ring crypto provider early — both wtransport (quinn) and
    // axum-server (rustls) need a provider; without this, axum-server panics
    // with "Could not automatically determine the process-level CryptoProvider".
    let _ = rustls::crypto::ring::default_provider().install_default();

    let log_buffer = LogBuffer::new(500);

    let filter = EnvFilter::from_default_env().add_directive("info".parse()?);
    tracing_subscriber::registry()
        .with(filter)
        .with(tracing_subscriber::fmt::layer())
        .with(
            tracing_subscriber::fmt::layer()
                .with_writer(BufferMaker {
                    buffer: log_buffer.clone(),
                })
                .with_ansi(false),
        )
        .init();

    let cli = Cli::parse();

    if cli.no_gui {
        run_headless(cli)
    } else {
        run_gui(cli, log_buffer)
    }
}

/// Headless CLI mode — the original behavior. Runs until Ctrl-C.
fn run_headless(cli: Cli) -> Result<()> {
    let runtime = tokio::runtime::Runtime::new()?;
    runtime.block_on(async move {
        let shutdown = Arc::new(Notify::new());
        let s = shutdown.clone();
        tokio::spawn(async move {
            let _ = tokio::signal::ctrl_c().await;
            tracing::info!("ctrl-c received, shutting down");
            s.notify_waiters();
        });
        let (_stats, task) = run_gateway(cli, shutdown).await?;
        let _ = task.await;
        Ok(())
    })
}

/// GUI mode — launches an eframe window. Falls back to CLI if no display.
fn run_gui(cli: Cli, log_buffer: Arc<LogBuffer>) -> Result<()> {
    let runtime = tokio::runtime::Runtime::new()?;
    let cli_fallback = cli.clone();

    let options = eframe::NativeOptions {
        viewport: eframe::egui::ViewportBuilder::default()
            .with_inner_size([520.0, 780.0])
            .with_min_inner_size([400.0, 500.0]),
        ..Default::default()
    };

    let result = eframe::run_native(
        "WebSRT Gateway",
        options,
        Box::new(move |cc| Ok(Box::new(gui::GuiApp::new(cli, runtime, log_buffer, cc)))),
    );

    match result {
        Ok(()) => Ok(()),
        Err(e) => {
            eprintln!("GUI unavailable ({e}); falling back to CLI mode.");
            eprintln!("Tip: pass --no-gui to start in CLI mode directly.");
            run_headless(cli_fallback)
        }
    }
}

fn config_dir() -> std::path::PathBuf {
    let home = std::env::var("HOME").unwrap_or_else(|_| ".".to_string());
    std::path::PathBuf::from(format!("{home}/.config/websrt"))
}

/// Build cert, write cert-hash.js, build gateway, spawn health server, wire
/// ingester, then spawn `gateway.run()` as a background task.
///
/// Returns the stats handle (for polling) and the gateway task join handle.
/// The caller triggers shutdown via `shutdown.notify_waiters()`.
pub(crate) async fn run_gateway(
    cli: Cli,
    shutdown: Arc<Notify>,
) -> Result<(GatewayStatsHandle, JoinHandle<Result<()>>)> {
    let cert_dir = config_dir();
    let _ = std::fs::create_dir_all(&cert_dir);
    let cert_path = cert_dir.join("gateway-cert.pem");
    let key_path = cert_dir.join("gateway-key.pem");

    let cert_src = match cli.cert_mode {
        CertMode::Self_ => {
            // Try to reuse a previously-generated self-signed cert so the
            // browser's cert exception / cert-hash pinning stays stable.
            if cert_path.exists() && key_path.exists() {
                tracing::info!("reusing persisted self-signed cert");
                CertSource::Mkcert {
                    cert: cert_path.clone(),
                    key: key_path.clone(),
                }
            } else {
                CertSource::SelfSigned {
                    sans: vec![
                        "localhost".to_string(),
                        "127.0.0.1".to_string(),
                        "::1".to_string(),
                    ],
                }
            }
        }
        CertMode::Mkcert => {
            let cert_pem = cli
                .cert_pem
                .clone()
                .ok_or_else(|| anyhow::anyhow!("--cert-pem required for --cert-mode mkcert"))?;
            let key_pem = cli
                .key_pem
                .clone()
                .ok_or_else(|| anyhow::anyhow!("--key-pem required for --cert-mode mkcert"))?;
            CertSource::Mkcert {
                cert: cert_pem,
                key: key_pem,
            }
        }
    };

    let mut cert = Cert::build(cert_src).await?;

    // When the cert was loaded from persisted PEM (mkcert path), the hash
    // isn't set by the builder — recompute it from the leaf DER so the
    // browser's cert-hash pinning works.
    if cert.der_sha256.is_none() {
        if let Some(leaf) = cert.identity.certificate_chain().as_slice().first() {
            cert.der_sha256 = Some(*leaf.hash().as_ref());
        }
    }

    // Persist newly-generated self-signed cert for reuse across restarts.
    if cli.cert_mode == CertMode::Self_
        && !cert_path.exists()
    {
        if let Some(leaf) = cert.identity.certificate_chain().as_slice().first() {
            let cert_pem = leaf.to_pem();
            let key_pem = cert.identity.private_key().to_secret_pem();
            if let Err(e) = std::fs::write(&cert_path, &cert_pem)
                .and_then(|()| std::fs::write(&key_path, &key_pem))
            {
                tracing::warn!(?e, "failed to persist self-signed cert; will regenerate next start");
            } else {
                tracing::info!("persisted self-signed cert to {} + {}", cert_path.display(), key_path.display());
            }
        }
    }

    // Write cert-hash.js so the browser knows which mode we're in.
    let hash_file = {
        let cwd = std::env::current_dir().context("failed to get current directory")?;
        let candidate = cwd.join("web/public/cert-hash.js");
        if cwd.join("web/public").exists() {
            candidate
        } else {
            let exe = std::env::current_exe().unwrap_or_default();
            exe.parent()
                .and_then(|p| p.parent())
                .map(|root| root.join("web/public/cert-hash.js"))
                .unwrap_or(candidate)
        }
    };
    let _ = std::fs::create_dir_all(hash_file.parent().unwrap());

    if let Some(hash) = cert.der_sha256 {
        let hex = hex::encode(hash);
        tracing::info!("WebTransport cert DER SHA-256: {}", hex);
        let js = format!("window.CERT_HASH = \"{}\";", hex);
        std::fs::write(&hash_file, &js)
            .with_context(|| format!("failed to write cert hash to {}", hash_file.display()))?;
        tracing::info!("Wrote cert hash to {}", hash_file.display());
    } else {
        tracing::info!("mkcert identity loaded; browser uses normal PKI");
        let js = "window.CERT_HASH = null;";
        std::fs::write(&hash_file, js)
            .with_context(|| format!("failed to write cert hash to {}", hash_file.display()))?;
        tracing::info!("Wrote cert-hash.js (null for mkcert mode) to {}", hash_file.display());
    }

    // Spawn the HTTPS web server (unless --no-web or --web-port 0)
    if !cli.no_web && cli.web_port > 0 {
        let cert_hash_js = if let Some(ref hash) = cert.der_sha256 {
            format!("window.CERT_HASH = \"{}\";", hex::encode(hash))
        } else {
            "window.CERT_HASH = null;".to_string()
        };
        let cert_pem = cert
            .identity
            .certificate_chain()
            .as_slice()
            .first()
            .map(|c| c.to_pem().into_bytes())
            .unwrap_or_default();
        let key_pem = cert.identity.private_key().to_secret_pem().into_bytes();
        let web_bind = cli.web_bind.clone();
        let web_port = cli.web_port;
        let shutdown = shutdown.clone();
        tokio::spawn(async move {
            if let Err(e) = web_server::run_web_server(web_bind, web_port, cert_hash_js, cert_pem, key_pem, shutdown).await {
                tracing::error!(?e, "web server failed");
            }
        });
    }

    // Build gateway
    #[cfg_attr(not(feature = "sim-loss"), allow(unused_mut))]
    let mut builder = Gateway::builder()
        .bind_addr(format!("{}:{}", cli.bind, cli.wt_port).parse::<std::net::SocketAddr>()?)
        .identity(cert.identity.clone_identity());

    #[cfg(feature = "sim-loss")]
    {
        builder = builder.sim_loss(cli.sim_loss, cli.sim_seed);
    }

    if let Some(ref token) = cli.auth_token {
        builder = builder.auth_token(token);
    }

    let gateway = builder.build()?;

    // Shared slot for the ingester's CC-probe counters. The TsContinuityChecker
    // is moved into the broadcaster pipeline; this handle lets the health
    // endpoint keep reading its live counters. Populated when the ingester is
    // wired (synchronously for --input file, inside the SRT setup task otherwise).
    let ts_stats: Arc<Mutex<HashMap<String, TsStatsHandle>>> = Arc::new(Mutex::new(HashMap::new()));

    // Spawn the health/metrics server. The library no longer owns this;
    // each embedding application is responsible for its own exposition format.
    if cli.health_port > 0 {
        let bind_addr: Option<std::net::IpAddr> = cli.health_bind.parse().ok();
        if let Some(ip) = bind_addr {
            if !ip.is_loopback() {
                tracing::warn!(
                    bind = %cli.health_bind,
                    "health server binding to non-loopback address; metrics will be visible on the network"
                );
            }
        }
        let stats_handle = gateway.stats_handle();
        let bind = cli.health_bind.clone();
        let port = cli.health_port;
        let ts_stats = ts_stats.clone();
        let health_shutdown = shutdown.clone();
        tokio::spawn(async move {
            let listener = match tokio::net::TcpListener::bind((bind.as_str(), port)).await {
                Ok(l) => l,
                Err(e) => {
                    tracing::warn!(?e, port, "health server bind failed");
                    return;
                }
            };
            tracing::info!(port, "health server listening");
            loop {
                tokio::select! {
                    biased;
                    _ = health_shutdown.notified() => {
                        tracing::info!("health server shutting down");
                        break;
                    }
                    accept_result = listener.accept() => {
                        match accept_result {
                            Ok((mut stream, _addr)) => {
                        let stats = stats_handle.stats();
                        let per_stream: String = stats
                            .per_stream
                            .iter()
                            .map(|s| {
                                format!(
                                    r#"{{"name":"{}","alive":{},"viewers":{},"messages_sent":{},"send_failures":{}}}"#,
                                    json_escape(&s.name),
                                    s.alive,
                                    s.viewers,
                                    s.messages_sent,
                                    s.send_failures,
                                )
                            })
                            .collect::<Vec<_>>()
                            .join(",");
                        let ingester_json = {
                            let stats_map = ts_stats.lock().unwrap();
                            if stats_map.is_empty() {
                                "null".to_string()
                            } else {
                                let entries: Vec<String> = stats_map
                                    .iter()
                                    .map(|(name, h)| {
                                        format!(
                                            r#""{}":{{"cc_gaps":{},"cc_checks":{},"messages_seen":{}}}"#,
                                            json_escape(name),
                                            h.cc_gaps(),
                                            h.cc_checks(),
                                            h.messages_seen(),
                                        )
                                    })
                                    .collect();
                                format!("{{{}}}", entries.join(","))
                            }
                        };
                        let json = format!(
                            r#"{{"status":"{}","streams":{},"alive_streams":{},"viewers":{},"max_viewers":{},"per_stream":[{}],"ingester":{}}}"#,
                            if stats.alive_streams > 0 { "ok" } else { "no_source" },
                            stats.streams,
                            stats.alive_streams,
                            stats.total_viewers,
                            stats.max_viewers,
                            per_stream,
                            ingester_json,
                        );
                        let response = format!(
                            "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nX-Content-Type-Options: nosniff\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                            json.len(),
                            json,
                        );
                        use tokio::io::AsyncWriteExt;
                        let _ = stream.write_all(response.as_bytes()).await;
                        let _ = stream.flush().await;
                    }
                            Err(e) => {
                                tracing::warn!(?e, "health accept error");
                                continue;
                            }
                        }
                    }
                }
            }
        });
    }

    // Setup ingester
    match cli.input {
        InputMode::File => {
            let ingester = FileIngester::new(&cli.fixture, cli.fixture_duration).map_err(|e| {
                tracing::error!(?e, "failed to open fixture; pass --fixture <path>");
                e
            })?;
            tracing::info!(fixture = ?cli.fixture, "file ingester ready");
            let checker = TsContinuityChecker::new(ingester);
            ts_stats.lock().unwrap().insert("default".to_string(), checker.stats_handle());
            gateway.source_handle().publish_stream("default", checker);
        }
        InputMode::Srt => {
            let source = gateway.source_handle();
            let srt_mode = cli.srt_mode;
            let srt_port = cli.srt_port;
            let call_addr = cli.srt_call.clone();
            let streamid = cli.srt_streamid.clone();
            let latency_ms = cli.latency;
            let srt_passphrase = cli.srt_passphrase.clone();
            let ts_stats = ts_stats.clone();
            let srt_shutdown = shutdown.clone();
            tokio::spawn(async move {
                match srt_mode {
                    SrtMode::Listener => {
                        tracing::info!(port = srt_port, "binding SRT multi-publisher listener");
                        let listener = match SrtListenerService::bind(
                            format!("0.0.0.0:{srt_port}"),
                            std::time::Duration::from_millis(latency_ms),
                            srt_passphrase,
                        )
                        .await
                        {
                            Ok(l) => l,
                            Err(e) => {
                                tracing::error!(?e, "SRT listener bind failed");
                                return;
                            }
                        };
                        let registry = source.registry();
                        tracing::info!("SRT multi-publisher listener ready, awaiting OBS connections");
                        listener
                            .serve(
                                registry,
                                srt_shutdown,
                                move |name, conn| {
                                    let checker = TsContinuityChecker::new(conn);
                                    ts_stats
                                        .lock()
                                        .unwrap()
                                        .insert(name.to_string(), checker.stats_handle());
                                    checker
                                },
                            )
                            .await;
                    }
                    SrtMode::Caller => {
                        let result = match call_addr {
                            Some(addr) => {
                                tracing::info!(%addr, "SRT caller mode: dialing OBS");
                                SrtIngester::call(
                                    &addr,
                                    streamid,
                                    std::time::Duration::from_millis(latency_ms),
                                    srt_passphrase,
                                )
                                .await
                            }
                            None => Err(anyhow::anyhow!(
                                "--srt-call <addr> required when --srt-mode caller"
                            )),
                        };
                        match result {
                            Ok(ingester) => {
                                let stream_name = ingester
                                    .accepted_stream_id()
                                    .map(|s| {
                                        tracing::info!(
                                            stream = %s,
                                            "publishing SRT stream under OBS streamid"
                                        );
                                        s.to_string()
                                    })
                                    .unwrap_or_else(|| "default".to_string());
                                tracing::info!("OBS connected; starting broadcaster");
                                let checker = TsContinuityChecker::new(ingester);
                                ts_stats
                                    .lock()
                                    .unwrap()
                                    .insert(stream_name.clone(), checker.stats_handle());
                                source.publish_stream(&stream_name, checker);
                            }
                            Err(e) => {
                                tracing::error!(?e, "SRT ingester setup failed");
                            }
                        }
                    }
                }
            });
        }
    }

    // Spawn the gateway run loop as a background task. The caller controls
    // shutdown via the Notify.
    let stats_handle = gateway.stats_handle();
    let task = tokio::spawn(async move {
        gateway.run(shutdown.notified()).await
    });

    Ok((stats_handle, task))
}

fn json_escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if (c as u32) < 0x20 => out.push_str(&format!("\\u{:04x}", c as u32)),
            c => out.push(c),
        }
    }
    out
}
