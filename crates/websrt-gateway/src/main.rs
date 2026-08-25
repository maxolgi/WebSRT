//! Demo gateway binary: CLI parse → cert setup → Gateway::run().
//!
//! By default launches a GUI (eframe/egui) with a config form and Start/Stop
//! buttons. Use `--no-gui` for the original headless CLI behavior.
//!
//! This is the application built on the `websrt` library.
//! For embedding, use the library crate directly.

// Use the Windows GUI subsystem for release builds so no background console
// window appears alongside the egui window. `--no-gui` mode reattaches to the
// parent terminal's console (see `reattach_parent_console`) so CLI output still
// works. Debug builds keep the console for `println!`-based development.
#![cfg_attr(
    all(target_os = "windows", not(debug_assertions)),
    windows_subsystem = "windows"
)]

mod gui;
mod log_buffer;
mod web_server;

use anyhow::{Context, Result};
use axum::extract::State;
use axum::routing::get;
use axum::{Json, Router};
use clap::{Parser, ValueEnum};
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use tokio::sync::Notify;
use tokio::task::JoinHandle;
use tracing_subscriber::layer::SubscriberExt as _;
use tracing_subscriber::util::SubscriberInitExt as _;
use tracing_subscriber::EnvFilter;
use websrt::cert::{Cert, CertSource};
use websrt::ingest::file::FileIngester;
use websrt::ingest::srt::SrtIngester;
use websrt::ingest::{SrtListenerService, TsContinuityChecker, TsStatsHandle};
use websrt::{Gateway, GatewayStatsHandle};

use log_buffer::{BufferMaker, LogBuffer};
use serde_json::json;

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

    /// Max SRT send bandwidth in kbps (0 = unlimited). Set to ~125% of stream bitrate.
    /// Example: --max-bandwidth 250000 for a 200 Mbps stream.
    #[arg(long, default_value_t = 0u64)]
    pub max_bandwidth: u64,

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

    /// Maximum concurrent viewers per stream.
    #[arg(long, default_value_t = 16)]
    pub max_viewers: usize,
}

fn main() -> Result<()> {
    // Install the ring crypto provider early — both wtransport (quinn) and
    // axum-server (rustls) need a provider; without this, axum-server panics
    // with "Could not automatically determine the process-level CryptoProvider".
    let _ = rustls::crypto::ring::default_provider().install_default();

    // Parse CLI early so `--no-gui` can reattach to the parent terminal's
    // console before logging is initialized (the windows_subsystem="windows"
    // attribute otherwise leaves stdout/stderr disconnected).
    let cli = Cli::parse();

    #[cfg(all(target_os = "windows", not(debug_assertions)))]
    if cli.no_gui {
        reattach_parent_console();
    }

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

    if cli.no_gui {
        run_headless(cli)
    } else {
        run_gui(cli, log_buffer)
    }
}

/// On Windows release builds the binary uses `windows_subsystem = "windows"`,
/// which means stdout/stderr are not connected to a console. When `--no-gui` is
/// run from an existing terminal, reattach to that terminal's console and
/// rebind stdout/stderr so tracing output is visible. No-op when double-clicked
/// (no parent console exists) — redirect to a file in that case.
#[cfg(all(target_os = "windows", not(debug_assertions)))]
fn reattach_parent_console() {
    use windows_sys::Win32::System::Console::{
        AttachConsole, GetStdHandle, ATTACH_PARENT_PROCESS, STD_ERROR_HANDLE, STD_OUTPUT_HANDLE,
    };
    extern "C" {
        fn _open_osfhandle(osfhandle: isize, flags: i32) -> i32;
        fn _dup2(filedes1: i32, filedes2: i32) -> i32;
    }
    const O_TEXT: i32 = 0x4000;
    unsafe {
        if AttachConsole(ATTACH_PARENT_PROCESS) == 0 {
            return;
        }
        for (handle_id, fd) in [(STD_OUTPUT_HANDLE, 1), (STD_ERROR_HANDLE, 2)] {
            let handle = GetStdHandle(handle_id) as isize;
            if handle == 0 || handle == -1 {
                continue;
            }
            let new_fd = _open_osfhandle(handle, O_TEXT);
            if new_fd != -1 {
                _dup2(new_fd, fd);
            }
        }
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

/// Decode the embedded PNG into RGBA pixels for the eframe window icon.
fn load_icon() -> eframe::egui::IconData {
    let img = image::load_from_memory(include_bytes!("../assets/icon.png"))
        .expect("failed to decode embedded icon")
        .to_rgba8();
    let (width, height) = img.dimensions();
    eframe::egui::IconData {
        rgba: img.into_raw(),
        width,
        height,
    }
}

/// GUI mode — launches an eframe window. Falls back to CLI if no display.
fn run_gui(cli: Cli, log_buffer: Arc<LogBuffer>) -> Result<()> {
    let runtime = tokio::runtime::Runtime::new()?;
    let cli_fallback = cli.clone();

    let options = eframe::NativeOptions {
        viewport: eframe::egui::ViewportBuilder::default()
            .with_inner_size([520.0, 580.0])
            .with_min_inner_size([400.0, 500.0])
            .with_icon(std::sync::Arc::new(load_icon())),
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

/// Best-effort chmod 0600 on PEM files (cert + key contain private material).
/// Never fails the caller; logs a warning per file on unix if chmod fails.
#[cfg(unix)]
fn restrict_pem_perms(paths: &[&std::path::Path]) {
    use std::os::unix::fs::PermissionsExt;
    for path in paths {
        let perms = std::fs::Permissions::from_mode(0o600);
        if let Err(e) = std::fs::set_permissions(path, perms) {
            tracing::warn!(?e, path = %path.display(), "failed to restrict PEM file permissions to 0600");
        }
    }
}
#[cfg(not(unix))]
fn restrict_pem_perms(_paths: &[&std::path::Path]) {}

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
                restrict_pem_perms(&[&cert_path, &key_path]);
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

    // A persisted self-signed cert is loaded via the Mkcert code path
    // (CertSource::Mkcert), so the builder doesn't set der_sha256. Recompute
    // it so the browser can pin via serverCertificateHashes. Real mkcert /
    // letsencrypt certs (CertMode::Mkcert) must NOT get a hash — the browser
    // must use normal PKI validation, and serverCertificateHashes imposes a
    // 2-week validity cap that public-CA certs exceed.
    if cli.cert_mode == CertMode::Self_ && cert.der_sha256.is_none() {
        if let Some(leaf) = cert.identity.certificate_chain().as_slice().first() {
            cert.der_sha256 = Some(*leaf.hash().as_ref());
        }
    }

    // Persist newly-generated self-signed cert for reuse across restarts.
    if cli.cert_mode == CertMode::Self_ && !cert_path.exists() {
        if let Some(leaf) = cert.identity.certificate_chain().as_slice().first() {
            let cert_pem = leaf.to_pem();
            let key_pem = cert.identity.private_key().to_secret_pem();
            if let Err(e) = std::fs::write(&cert_path, &cert_pem)
                .and_then(|()| std::fs::write(&key_path, &key_pem))
            {
                tracing::warn!(
                    ?e,
                    "failed to persist self-signed cert; will regenerate next start"
                );
            } else {
                restrict_pem_perms(&[&cert_path, &key_path]);
                tracing::info!(
                    "persisted self-signed cert to {} + {}",
                    cert_path.display(),
                    key_path.display()
                );
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

    // cert-hash.js body, built once and reused for both the on-disk file
    // (Vite dev server / static serving) and the embedded web server's
    // /cert-hash.js route. Advertises the cert hash AND the WT port so a
    // consumer that knows only the web origin can discover both in one fetch.
    let cert_hash_js = match cert.der_sha256.as_ref() {
        Some(hash) => {
            let hex = hex::encode(hash);
            tracing::info!("WebTransport cert DER SHA-256: {}", hex);
            format!(
                "window.CERT_HASH = \"{}\";\nwindow.WT_PORT = {};",
                hex, cli.wt_port
            )
        }
        None => {
            tracing::info!("mkcert identity loaded; browser uses normal PKI");
            format!(
                "window.CERT_HASH = null;\nwindow.WT_PORT = {};",
                cli.wt_port
            )
        }
    };
    std::fs::write(&hash_file, &cert_hash_js)
        .with_context(|| format!("failed to write cert-hash.js to {}", hash_file.display()))?;
    tracing::info!("Wrote cert-hash.js to {}", hash_file.display());

    // Spawn the HTTPS web server (unless --no-web or --web-port 0)
    if !cli.no_web && cli.web_port > 0 {
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
            if let Err(e) = web_server::run_web_server(
                web_bind,
                web_port,
                cert_hash_js,
                cert_pem,
                key_pem,
                shutdown,
            )
            .await
            {
                tracing::error!(?e, "web server failed");
            }
        });
    }

    // Build gateway
    #[cfg_attr(not(feature = "sim-loss"), allow(unused_mut))]
    let mut builder = Gateway::builder()
        .bind_addr(format!("{}:{}", cli.bind, cli.wt_port).parse::<std::net::SocketAddr>()?)
        .identity(cert.identity.clone_identity())
        .max_viewers(cli.max_viewers)
        .max_bandwidth(if cli.max_bandwidth > 0 {
            Some(cli.max_bandwidth.saturating_mul(1000) / 8)
        } else {
            None
        });

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
        let ts_stats = ts_stats.clone();
        let app = Router::new()
            .route("/health", get(health_handler))
            .with_state((Arc::new(stats_handle), ts_stats));
        let bind = cli.health_bind.clone();
        let port = cli.health_port;
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
            if let Err(e) = axum::serve(listener, app)
                .with_graceful_shutdown(async move {
                    health_shutdown.notified().await;
                    tracing::info!("health server shutting down");
                })
                .await
            {
                tracing::warn!(?e, "health server failed");
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
            ts_stats
                .lock()
                .unwrap()
                .insert("default".to_string(), checker.stats_handle());
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
                        tracing::info!(
                            "SRT multi-publisher listener ready, awaiting OBS connections"
                        );
                        listener
                            .serve(registry, srt_shutdown, move |name, conn| {
                                let checker = TsContinuityChecker::new(conn);
                                ts_stats
                                    .lock()
                                    .unwrap()
                                    .insert(name.to_string(), checker.stats_handle());
                                checker
                            })
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
    let task = tokio::spawn(async move { gateway.run(shutdown.notified()).await });

    Ok((stats_handle, task))
}

type HealthState = (
    Arc<GatewayStatsHandle>,
    Arc<Mutex<HashMap<String, TsStatsHandle>>>,
);

/// GET /health — gateway + ingester stats as JSON.
async fn health_handler(
    State((stats_handle, ts_stats)): State<HealthState>,
) -> Json<serde_json::Value> {
    let stats = stats_handle.stats();
    let per_stream: Vec<serde_json::Value> = stats
        .per_stream
        .iter()
        .map(|s| {
            json!({
                "name": s.name,
                "alive": s.alive,
                "viewers": s.viewers,
                "messages_sent": s.messages_sent,
                "send_failures": s.send_failures,
            })
        })
        .collect();
    let per_session: Vec<serde_json::Value> = stats
        .per_session
        .iter()
        .map(|s| {
            let srt = s.srt.as_ref();
            json!({
                "session_id": s.session_id,
                "stream": s.stream_name,
                "tx_data": srt.map(|v| v.tx_data).unwrap_or(0),
                "tx_buffered": srt.map(|v| v.tx_buffered).unwrap_or(0),
                "tx_retransmit": srt.map(|v| v.tx_retransmit).unwrap_or(0),
                "tx_loss": srt.map(|v| v.tx_loss).unwrap_or(0),
                "rtt_ms": srt.and_then(|v| v.tx_rtt).map(|d| d.as_millis() as u64).unwrap_or(0),
                "messages_pushed": s.messages_pushed,
                "viewer_lag": s.viewer_lag_count,
            })
        })
        .collect();
    // Short std::sync::Mutex lock, no await while held.
    let ingester = {
        let stats_map = ts_stats.lock().unwrap();
        if stats_map.is_empty() {
            serde_json::Value::Null
        } else {
            let entries: serde_json::Map<String, serde_json::Value> = stats_map
                .iter()
                .map(|(name, h)| {
                    (
                        name.clone(),
                        json!({
                            "cc_gaps": h.cc_gaps(),
                            "cc_checks": h.cc_checks(),
                            "messages_seen": h.messages_seen(),
                        }),
                    )
                })
                .collect();
            serde_json::Value::Object(entries)
        }
    };
    Json(json!({
        "status": if stats.alive_streams > 0 { "ok" } else { "no_source" },
        "streams": stats.streams,
        "alive_streams": stats.alive_streams,
        "viewers": stats.total_viewers,
        "max_viewers": stats.max_viewers,
        "per_stream": per_stream,
        "per_session": per_session,
        "ticker": {
            "count": stats.ticker_count,
            "avg_us": stats.ticker_avg_us,
            "max_us": stats.ticker_max_us,
        },
        "ingester": ingester,
    }))
}
