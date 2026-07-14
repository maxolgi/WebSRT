//! Phase 5 verification: same as Phase 4 probe, but additionally tracks per-PID
//! TS continuity counter gaps to confirm NAK/retransmit keeps the stream
//! complete under --sim-loss.
//!
//! Usage:
//!   1. Start gateway with --sim-loss N
//!   2. cargo run -p websrt-gateway --bin wt_hs_probe -- --hash <hash> --seconds 10

use clap::Parser;
use srt_protocol::connection::{Action, DuplexConnection, Input};
use srt_protocol::packet::Packet;
use srt_protocol::protocol::pending_connection::listen::Listen;
use srt_protocol::protocol::pending_connection::ConnectionResult;
use srt_protocol::settings::ConnInitSettings;
use wtransport::tls::Sha256Digest;
use std::collections::HashMap;
use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::time::{Duration, Instant};
use wtransport::ClientConfig;
use wtransport::Endpoint;

#[derive(Parser)]
struct Cli {
    #[arg(long, default_value = "https://127.0.0.1:4433/wt")]
    url: String,
    #[arg(long)]
    hash: String,
    #[arg(long, default_value_t = 5u64)]
    seconds: u64,
}

const GATEWAY: SocketAddr = SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 0);

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt::try_init().ok();
    let cli = Cli::parse();
    let hash = parse_hex(&cli.hash)?;
    let digest = Sha256Digest::new(hash);

    let config = ClientConfig::builder()
        .with_bind_default()
        .with_server_certificate_hashes([digest])
        .build();
    let conn = Endpoint::client(config)?.connect(&cli.url).await?;
    println!("connected: session_id={:?}", conn.session_id());

    let settings = ConnInitSettings::default();
    let mut listen = Listen::new(settings, false);
    let mut duplex: Option<DuplexConnection> = None;

    let mut msgs: u64 = 0;
    let mut bytes: u64 = 0;
    let mut ts_msgs: u64 = 0;
    // Per-PID continuity-counter tracking: detect gaps (= missing TS packets
    // not recovered by NAK/retransmit).
    let mut last_cc: HashMap<u16, u8> = HashMap::new();
    let mut cc_gaps: u64 = 0;
    let mut cc_advances: u64 = 0;

    let deadline = Instant::now() + Duration::from_secs(cli.seconds);

    loop {
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() { break; }

        let dgram = tokio::time::timeout(Duration::from_millis(500), conn.receive_datagram()).await;
        let payload = match dgram {
            Ok(Ok(d)) => d.payload(),
            Ok(Err(e)) => {
                println!("recv err: {e}");
                break;
            }
            Err(_) => {
                if let Some(d) = &mut duplex {
                    let now = Instant::now();
                    d.handle_input(now, Input::Timer);
                    drain(d, &conn, &mut msgs, &mut bytes, &mut ts_msgs, &mut last_cc, &mut cc_gaps, &mut cc_advances)?;
                }
                continue;
            }
        };
        let now = Instant::now();
        let mut buf: &[u8] = &payload[..];
        let packet = match Packet::parse(&mut buf, false) {
            Ok(p) => p,
            Err(e) => { println!("parse err: {e:?}"); continue; }
        };

        if let Some(d) = &mut duplex {
            d.handle_packet_input(now, Ok((packet, GATEWAY)));
            drain(d, &conn, &mut msgs, &mut bytes, &mut ts_msgs, &mut last_cc, &mut cc_gaps, &mut cc_advances)?;
        } else {
            match listen.handle_packet(now, Ok((packet, GATEWAY))) {
                ConnectionResult::SendPacket((pkt, _)) => conn.send_datagram(serialize(&pkt))?,
                ConnectionResult::Connected(maybe, conn_settings) => {
                    if let Some((pkt, _)) = maybe { conn.send_datagram(serialize(&pkt))?; }
                    duplex = Some(DuplexConnection::new(conn_settings));
                    println!("✓ handshake complete");
                }
                ConnectionResult::NoAction => {}
                ConnectionResult::NotHandled(e) => println!("not-handled: {e}"),
                ConnectionResult::Reject(_, r) => { println!("rejected: {r:?}"); break; }
                ConnectionResult::Failure(e) => { println!("io: {e}"); break; }
                ConnectionResult::RequestAccess(_) => println!("access-control"),
            }
        }
    }

    println!("\n--- stats ---");
    println!("received messages: {msgs} ({ts_msgs} started with 0x47)");
    println!("received bytes:    {bytes}");
    println!("TS packet advances: {cc_advances}, gaps detected: {cc_gaps}");
    let gap_pct = if cc_advances > 0 {
        (cc_gaps as f64) / (cc_advances + cc_gaps) as f64 * 100.0
    } else { 0.0 };
    println!("gap rate: {gap_pct:.2}%");
    Ok(())
}

fn drain(
    d: &mut DuplexConnection,
    conn: &wtransport::Connection,
    msgs: &mut u64,
    bytes: &mut u64,
    ts_msgs: &mut u64,
    last_cc: &mut HashMap<u16, u8>,
    cc_gaps: &mut u64,
    cc_advances: &mut u64,
) -> anyhow::Result<()> {
    let now = Instant::now();
    loop {
        let a = d.handle_input(now, Input::DataReleased);
        match a {
            Action::SendPacket((p, _)) => {
                conn.send_datagram(serialize(&p))?;
            }
            Action::ReleaseData((_ts, b)) => {
                *msgs += 1;
                *bytes += b.len() as u64;
                if b.first() == Some(&0x47) {
                    *ts_msgs += 1;
                    // Walk 188-byte TS packets, check continuity counters.
                    for chunk in b.chunks(188) {
                        if chunk.len() < 4 { continue; }
                        let pid = (((chunk[1] as u16) & 0x1F) << 8) | (chunk[2] as u16);
                        let cc = chunk[3] & 0x0F;
                        // adaptation_field_control determines if CC advances.
                        // For simplicity, we assume every packet increments.
                        if let Some(&prev) = last_cc.get(&pid) {
                            let expected = (prev.wrapping_add(1)) & 0x0F;
                            if cc != expected {
                                *cc_gaps += 1;
                            } else {
                                *cc_advances += 1;
                            }
                        }
                        last_cc.insert(pid, cc);
                    }
                }
            }
            Action::WaitForData(_) => break,
            Action::Close => break,
            Action::UpdateStatistics(_) => {}
        }
    }
    Ok(())
}

fn serialize(pkt: &Packet) -> Vec<u8> {
    let mut buf = bytes::BytesMut::with_capacity(pkt.wire_size());
    pkt.serialize(&mut buf);
    buf.to_vec()
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
