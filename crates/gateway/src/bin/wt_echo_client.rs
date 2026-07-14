//! Standalone WT echo client for Phase 1 verification.
//! Run: cargo run -p gateway --bin wt_echo_client -- --url https://127.0.0.1:4433/wt --hash <hex>
//!
//! (This is a dev/test binary; not the production gateway entrypoint.)

use clap::Parser;
use wtransport::tls::Sha256Digest;
use wtransport::tls::Sha256DigestFmt;
use wtransport::ClientConfig;
use wtransport::Endpoint;

#[derive(Parser)]
struct Cli {
    #[arg(long, default_value = "https://127.0.0.1:4433/wt")]
    url: String,
    /// Cert DER SHA-256, hex (64 chars, optional separators).
    #[arg(long)]
    hash: String,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt::try_init().ok();

    let cli = Cli::parse();
    let hash_bytes = parse_hex(&cli.hash)?;
    let digest = Sha256Digest::new(hash_bytes);

    let config = ClientConfig::builder()
        .with_bind_default()
        .with_server_certificate_hashes([digest.clone()])
        .build();

    let conn = Endpoint::client(config)?
        .connect(&cli.url)
        .await?;

    println!("connected: session_id={:?}", conn.session_id());

    let payload = b"ping";
    conn.send_datagram(payload.to_vec())?;
    println!("sent {}B", payload.len());

    let dgram = conn.receive_datagram().await?;
    let echoed = dgram.payload();
    println!("recv {}B: {:?}", echoed.len(), std::str::from_utf8(&echoed)?);

    // Round-trip a few more to be sure.
    for i in 0u32..5 {
        let msg = format!("ping-{i}");
        conn.send_datagram(msg.as_bytes().to_vec())?;
        let r = conn.receive_datagram().await?;
        let p = r.payload();
        println!(
            "roundtrip {}: {:?}",
            i,
            std::str::from_utf8(&p).unwrap_or("<binary>")
        );
    }

    let _ = Sha256DigestFmt::DottedHex; // suppress unused-import if any
    Ok(())
}

fn parse_hex(s: &str) -> anyhow::Result<[u8; 32]> {
    let clean: String = s.chars().filter(|c| c.is_ascii_hexdigit()).collect();
    if clean.len() != 64 {
        anyhow::bail!("expected 64 hex chars, got {}", clean.len());
    }
    let mut out = [0u8; 32];
    for i in 0..32 {
        out[i] = u8::from_str_radix(&clean[i * 2..i * 2 + 2], 16)?;
    }
    Ok(out)
}
