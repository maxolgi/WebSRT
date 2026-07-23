//! Demo gateway binary: CLI parse → cert setup → Gateway::run().
//!
//! This is the reference application built on the `websrt` library.
//! For embedding, use the library crate directly.

use anyhow::{Context, Result};
use clap::{Parser, ValueEnum};
use std::path::PathBuf;
use tracing_subscriber::EnvFilter;
use websrt::cert::{Cert, CertSource};
use websrt::ingest::file::FileIngester;
use websrt::ingest::srt::SrtIngester;
use websrt::ingest::TsContinuityChecker;
use websrt::Gateway;

#[derive(Debug, Clone, Copy, ValueEnum, PartialEq)]
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

#[derive(Parser, Debug)]
#[command(name = "websrt-gateway", version, about = "SRT → WebTransport gateway (demo)")]
pub struct Cli {
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

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::from_default_env().add_directive("info".parse()?))
        .init();

    let cli = Cli::parse();

    let cert_src = match cli.cert_mode {
        CertMode::Self_ => CertSource::SelfSigned {
            sans: vec![
                "localhost".to_string(),
                "127.0.0.1".to_string(),
                "::1".to_string(),
            ],
        },
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

    let cert = Cert::build(cert_src).await?;

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

    // Spawn the demo health/metrics server. The library no longer owns this;
    // each embedding application is responsible for its own exposition format.
    if cli.health_port > 0 {
        let stats_handle = gateway.stats_handle();
        let bind = cli.health_bind.clone();
        let port = cli.health_port;
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
                match listener.accept().await {
                    Ok((mut stream, _addr)) => {
                        let stats = stats_handle.stats();
                        let json = format!(
                            r#"{{"status":"{}","streams":{},"alive_streams":{},"viewers":{},"max_viewers":{}}}"#,
                            if stats.alive_streams > 0 { "ok" } else { "no_source" },
                            stats.streams,
                            stats.alive_streams,
                            stats.total_viewers,
                            stats.max_viewers,
                        );
                        let response = format!(
                            "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
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
            gateway
                .source_handle()
                .publish_stream("default", TsContinuityChecker::new(ingester));
        }
        InputMode::Srt => {
            let source = gateway.source_handle();
            let srt_mode = cli.srt_mode;
            let srt_port = cli.srt_port;
            let call_addr = cli.srt_call.clone();
            let streamid = cli.srt_streamid.clone();
            let latency_ms = cli.latency;
            tokio::spawn(async move {
                let result = match srt_mode {
                    SrtMode::Listener => {
                        tracing::info!(port = srt_port, "binding SRT listener for OBS");
                        SrtIngester::bind_with_latency(
                            format!("0.0.0.0:{srt_port}"),
                            streamid,
                            std::time::Duration::from_millis(latency_ms),
                        )
                        .await
                    }
                    SrtMode::Caller => {
                        match call_addr {
                            Some(addr) => {
                                tracing::info!(%addr, "SRT caller mode: dialing OBS");
                                SrtIngester::call_with_latency(
                                    &addr,
                                    streamid,
                                    std::time::Duration::from_millis(latency_ms),
                                )
                                .await
                            }
                            None => Err(anyhow::anyhow!("--srt-call <addr> required when --srt-mode caller")),
                        }
                    }
                };
                match result {
                    Ok(ingester) => {
                        // Route by OBS's ?streamid=... if present, else "default".
                        let stream_name = ingester
                            .accepted_stream_id()
                            .map(|s| {
                                tracing::info!(stream = %s, "publishing SRT stream under OBS streamid");
                                s.to_string()
                            })
                            .unwrap_or_else(|| "default".to_string());
                        tracing::info!("OBS connected; starting broadcaster");
                        source.publish_stream(
                            &stream_name,
                            TsContinuityChecker::new(ingester),
                        );
                    }
                    Err(e) => {
                        tracing::error!(?e, "SRT ingester setup failed");
                    }
                }
            });
        }
    }

    // Run until ctrl-c
    gateway
        .run(async {
            let _ = tokio::signal::ctrl_c().await;
        })
        .await
}
