//! Mock OBS sender: connect to the gateway's SRT listener and stream the
//! fixture .ts over SRT/UDP. Used to verify the SRT ingester without real OBS.
//!
//! Usage:
//!   1. cargo run -p websrt-gateway -- --input srt --srt-port 9000
//!   2. cargo run -p websrt-gateway --bin mock_obs -- --dst 127.0.0.1:9000

use bytes::Bytes;
use clap::Parser;
use srt_tokio::SrtSocket;
use std::path::PathBuf;
use std::time::{Duration, Instant};
use tokio::time::sleep;

#[derive(Parser)]
struct Cli {
    #[arg(long, default_value = "127.0.0.1:9000")]
    dst: String,
    #[arg(long, default_value = "fixtures/test.ts")]
    fixture: PathBuf,
    /// How many seconds to stream for.
    #[arg(long, default_value_t = 10u64)]
    seconds: u64,
}

const TS_PACKET: usize = 188;
const PAYLOAD_BYTES: usize = 1100;
const PACKETS_PER_MESSAGE: usize = PAYLOAD_BYTES / TS_PACKET;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt::try_init().ok();
    let cli = Cli::parse();
    let data = std::fs::read(&cli.fixture)?;
    let chunk = PACKETS_PER_MESSAGE * TS_PACKET;
    if data.len() < chunk {
        anyhow::bail!("fixture too small");
    }
    let bytes_per_sec = (data.len() as f64) / 10.0;

    let dst: srt_protocol::options::SocketAddress = cli.dst.as_str().try_into()?;
    let mut socket = SrtSocket::builder().call(dst, None).await?;
    println!("connected to {}", cli.dst);

    let start = Instant::now();
    let deadline = start + Duration::from_secs(cli.seconds);
    let mut cursor = 0usize;
    let mut emitted = 0u64;
    let mut loop_count = 0u64;
    loop {
        if Instant::now() > deadline { break; }
        if cursor + chunk > data.len() {
            cursor = 0;
            loop_count += 1;
            println!("fixture looped (#{loop_count})");
        }
        // Pace.
        let target_elapsed = (emitted as f64) / bytes_per_sec;
        let actual = start.elapsed().as_secs_f64();
        if target_elapsed > actual {
            sleep(Duration::from_secs_f64(target_elapsed - actual)).await;
        }

        let msg = (Instant::now(), Bytes::copy_from_slice(&data[cursor..cursor + chunk]));
        use futures::SinkExt;
        socket.send(msg).await?;
        cursor += chunk;
        emitted += chunk as u64;
    }
    println!("done; emitted {emitted} bytes over {loop_count}+ loops");
    Ok(())
}
