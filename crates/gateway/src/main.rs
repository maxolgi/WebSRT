//! gateway entrypoint: arg parse, cert bootstrap, server dispatch.
//!
//! Phase 0: just generate the cert and print the hash.
//! Phase 1+: dispatch into server::run().

mod broadcaster;
mod cert;
mod ingest;
mod server;
mod session;
mod srt_sender;

use anyhow::Result;
use clap::{Parser, ValueEnum};
use std::path::PathBuf;
use tracing_subscriber::EnvFilter;

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
#[command(name = "gateway", version, about = "SRT → WebTransport gateway")]
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
    #[arg(long, default_value_t = 0u8)]
    pub sim_loss: u8,

    /// RNG seed for sim-loss (deterministic by default).
    #[arg(long, default_value_t = 42u64)]
    pub sim_seed: u64,

    /// SRT TSBPD latency in milliseconds.
    #[arg(long, default_value_t = 300u64)]
    pub latency: u64,
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::from_default_env().add_directive("info".parse()?))
        .init();

    let cli = Cli::parse();

    let cert_src = match cli.cert_mode {
        CertMode::Self_ => cert::CertSource::SelfSigned {
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
                .context_path("--cert-pem required for --cert-mode mkcert")?;
            let key_pem = cli
                .key_pem
                .clone()
                .context_path("--key-pem required for --cert-mode mkcert")?;
            cert::CertSource::Mkcert {
                cert: cert_pem,
                key: key_pem,
            }
        }
    };

    let cert = cert::Cert::build(cert_src).await?;

    // Always write cert-hash.js so the browser knows which mode we're in.
    // Self-signed: writes the hash hex. Mkcert: writes null so the browser
    // connects without serverCertificateHashes (uses system PKI trust).
    let hash_file = {
        let cwd = std::env::current_dir().unwrap_or_default();
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
        let _ = std::fs::write(&hash_file, &js);
        tracing::info!("Wrote cert hash to {}", hash_file.display());
        tracing::info!(
            "Browser: pass this to serverCertificateHashes: {{algorithm:'sha-256', value:new Uint8Array([{}])}}",
            hash.iter()
                .map(|b| format!("0x{:02x}", b))
                .collect::<Vec<_>>()
                .join(", ")
        );
    } else {
        tracing::info!("mkcert identity loaded; browser uses normal PKI");
        let js = "window.CERT_HASH = null;";
        let _ = std::fs::write(&hash_file, js);
        tracing::info!("Wrote cert-hash.js (null for mkcert mode) to {}", hash_file.display());
    }

    server::run(cert, cli).await
}

/// Small helper to attach a context to an Option<PathBuf>-unwrap idiom.
trait OptExt<T> {
    fn context_path(self, msg: &str) -> Result<T>;
}

impl<T> OptExt<T> for Option<T> {
    fn context_path(self, msg: &str) -> Result<T> {
        self.ok_or_else(|| anyhow::anyhow!("{}", msg))
    }
}
