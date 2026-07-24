#![cfg(feature = "e2e")]
//! End-to-end integration test: real WebTransport over loopback.
//!
//! Exercises the full `Gateway` lifecycle against a real `wtransport` client —
//! accept loop, TLS/WebTransport handshake, PMTU check, skip-induction SRT
//! handshake driven over real WT datagrams, publish → broadcaster → ticker →
//! SRT sender data path, and graceful drain. Gated behind the `e2e` feature so
//! it does not add latency to the default `cargo test` run.
//!
//! Run with:
//! ```text
//! cargo test -p websrt --features e2e --test e2e -- --nocapture --test-threads=1
//! ```
//!
//! Client-side SRT driving mirrors `crates/websrt-gateway/src/bin/wt_hs_probe.rs`.

use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::time::{Duration, Instant};

use bytes::{Bytes, BytesMut};

use srt_protocol::packet::Packet;
use srt_protocol::protocol::pending_connection::listen::Listen;
use srt_protocol::protocol::pending_connection::ConnectionResult;
use srt_protocol::settings::ConnInitSettings;

use tokio::sync::{mpsc, oneshot};
use wtransport::tls::Sha256Digest;
use wtransport::{ClientConfig, Connection, Endpoint};

use websrt::cert::{Cert, CertSource};
use websrt::ingest::TsMessage;
use websrt::Gateway;

const LOCAL: IpAddr = IpAddr::V4(Ipv4Addr::LOCALHOST);
/// Dummy peer address passed to the client-side SRT state machine. srt-protocol
/// only needs it for internal bookkeeping (socket IDs); it is never used on the
/// WebTransport data path. Matches the constant used by `wt_hs_probe`.
const DUMMY_REMOTE: SocketAddr = SocketAddr::new(LOCAL, 0);

/// Grab an ephemeral loopback port by briefly binding a UDP socket.
///
/// `Gateway::run` consumes `self` and binds internally, so there is no way to
/// read back the OS-assigned port. This reserves one ahead of time. There is a
/// small TOCTOU window between the drop and the gateway's bind, but it is
/// acceptable for a single-threaded test run.
fn ephemeral_port() -> u16 {
    let sock = std::net::UdpSocket::bind("127.0.0.1:0").expect("bind udp probe");
    let port = sock.local_addr().expect("local_addr").port();
    drop(sock);
    port
}

fn init_tracing() {
    let _ = tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("warn")),
        )
        .try_init();
}

fn serialize(pkt: &Packet) -> Vec<u8> {
    let mut buf = BytesMut::with_capacity(pkt.wire_size());
    pkt.serialize(&mut buf);
    buf.to_vec()
}

/// Build a self-signed cert identity + the DER SHA-256 the client needs for
/// `serverCertificateHashes`.
async fn make_cert() -> (wtransport::Identity, [u8; 32]) {
    let cert = Cert::build(CertSource::SelfSigned {
        sans: vec!["localhost".to_string()],
    })
    .await
    .expect("cert build");
    let hash = cert.der_sha256.expect("self-signed cert exposes der hash");
    (cert.identity, hash)
}

/// Wire up a gateway on `port`, publishing the `test` stream so viewer sessions
/// pass the pre-accept "stream alive" check. Returns the shutdown sender, the
/// spawned run-loop join handle, the publish sender, and the cert hash.
async fn start_gateway(
    port: u16,
) -> (
    oneshot::Sender<()>,
    tokio::task::JoinHandle<anyhow::Result<()>>,
    mpsc::Sender<TsMessage>,
    [u8; 32],
) {
    let (identity, hash) = make_cert().await;
    let bind: SocketAddr = format!("127.0.0.1:{port}").parse().unwrap();
    let gateway = Gateway::builder()
        .bind_addr(bind)
        .identity(identity)
        .max_idle_timeout(Duration::from_secs(10))
        .handshake_timeout(Duration::from_secs(5))
        .build()
        .expect("gateway build");

    let source = gateway.source_handle();
    let publish_tx = source.publish("test");

    let (shutdown_tx, shutdown_rx) = oneshot::channel::<()>();
    let handle = tokio::spawn(async move {
        gateway
            .run(async {
                let _ = shutdown_rx.await;
            })
            .await
    });
    (shutdown_tx, handle, publish_tx, hash)
}

/// Connect a real WT client using `serverCertificateHashes` (the same path a
/// browser takes with a self-signed cert). `path` must include the leading `/`
/// and any query string, e.g. `/wt?stream=test`.
async fn connect_client(port: u16, hash: [u8; 32], path: &str) -> Connection {
    let config = ClientConfig::builder()
        .with_bind_default()
        .with_server_certificate_hashes([Sha256Digest::new(hash)])
        .build();
    let client = Endpoint::client(config).expect("client endpoint");
    let url = format!("https://localhost:{port}{path}");
    client.connect(&url).await.expect("client connect")
}

/// Drive the browser-side skip-induction SRT handshake on a real WT connection
/// until `Listen` returns `Connected`, then return the established `Connection`.
/// All gateway → client datagrams before handshake completion are fed to
/// `Listen`; any `SendPacket` reply is sent back over WT.
async fn drive_handshake(conn: &Connection, deadline: Instant) {
    let mut listen = Listen::new(ConnInitSettings::default(), false);
    listen.allow_skip_induction(true);
    loop {
        if Instant::now() >= deadline {
            panic!("SRT handshake did not complete within the deadline");
        }
        let wait = deadline
            .saturating_duration_since(Instant::now())
            .min(Duration::from_millis(500));
        let dgram = tokio::time::timeout(wait, conn.receive_datagram()).await;
        let now = Instant::now();
        match dgram {
            Ok(Ok(d)) => {
                let payload = d.payload();
                let mut buf: &[u8] = &payload[..];
                let packet = match Packet::parse(&mut buf, false) {
                    Ok(p) => p,
                    Err(_) => continue,
                };
                match listen.handle_packet(now, Ok((packet, DUMMY_REMOTE))) {
                    ConnectionResult::SendPacket((pkt, _)) => {
                        let _ = conn.send_datagram(serialize(&pkt));
                    }
                    ConnectionResult::Connected(maybe, _settings) => {
                        if let Some((pkt, _)) = maybe {
                            let _ = conn.send_datagram(serialize(&pkt));
                        }
                        return;
                    }
                    ConnectionResult::NoAction
                    | ConnectionResult::NotHandled(_)
                    | ConnectionResult::RequestAccess(_) => {}
                    ConnectionResult::Reject(_, r) => panic!("handshake rejected: {r:?}"),
                    ConnectionResult::Failure(e) => panic!("handshake failure: {e}"),
                }
            }
            Ok(Err(e)) => panic!("WT datagram receive error during handshake: {e}"),
            Err(_) => {}
        }
    }
}

/// Receive WT datagrams until one carries an SRT **data** packet, then return
/// its payload. Control packets (ACK/KeepAlive/handshake retransmits) are
/// skipped. Times out at `deadline`.
async fn recv_first_data_payload(conn: &Connection, deadline: Instant) -> Bytes {
    loop {
        if Instant::now() >= deadline {
            panic!("no SRT data packet received within the deadline");
        }
        let wait = deadline
            .saturating_duration_since(Instant::now())
            .min(Duration::from_millis(500));
        let dgram = tokio::time::timeout(wait, conn.receive_datagram()).await;
        match dgram {
            Ok(Ok(d)) => {
                let payload = d.payload();
                // SRT wire format: bit 7 of the first byte = 0 ⇒ DATA packet.
                let is_data = payload.first().map(|b| b & 0x80 == 0).unwrap_or(false);
                if !is_data {
                    continue;
                }
                let mut buf: &[u8] = &payload[..];
                match Packet::parse(&mut buf, false) {
                    Ok(Packet::Data(data)) => return data.payload,
                    _ => continue,
                }
            }
            Ok(Err(e)) => panic!("WT datagram receive error: {e}"),
            Err(_) => {}
        }
    }
}

/// Gracefully tear down a gateway: signal shutdown and await its run-loop with a
/// bounded timeout so a hung drain surfaces as a test failure rather than a
/// hang.
async fn shutdown_gateway(
    shutdown_tx: oneshot::Sender<()>,
    handle: tokio::task::JoinHandle<anyhow::Result<()>>,
) {
    let _ = shutdown_tx.send(());
    let result = tokio::time::timeout(Duration::from_secs(10), handle).await;
    match result {
        Ok(Ok(Ok(()))) => {}
        Ok(Ok(Err(e))) => panic!("gateway run returned error: {e}"),
        Ok(Err(join_err)) => panic!("gateway task panicked: {join_err}"),
        Err(_) => panic!("gateway did not shut down within 10s"),
    }
}

/// Minimal "connect and disconnect" E2E test: the gateway must accept a real WT
/// session over loopback, and cleanly drain when the client drops. Covers the
/// accept loop, TLS handshake, path policy, stream-alive check, PMTU check,
/// recv_pump spawn, and drain path.
#[tokio::test]
async fn gateway_accepts_and_drains_wt_session() {
    init_tracing();
    let port = ephemeral_port();
    let (shutdown_tx, gateway_handle, _publish_tx, hash) = start_gateway(port).await;
    // Let the server endpoint bind before the client connects.
    tokio::time::sleep(Duration::from_millis(100)).await;

    let conn = connect_client(port, hash, "/wt?stream=test").await;
    // The WT session (including accept) is fully established here — proves the
    // accept loop, path policy, stream-alive check, and PMTU check all passed.
    assert!(
        conn.max_datagram_size().unwrap_or(0) >= 1200,
        "PMTU must be adequate for SRT payloads"
    );

    // Drop the client; the gateway's recv_pump must observe the closed datagram
    // stream, mark the session finished, and let the drain path clean up.
    drop(conn);

    shutdown_gateway(shutdown_tx, gateway_handle).await;
}

/// Full data-path E2E test: the gateway accepts a viewer, completes the
/// skip-induction SRT handshake over real WT datagrams, and a published TS
/// message is delivered to the client as an SRT data packet whose payload
/// matches byte-for-byte. Verifies the complete gateway data path
/// (publish channel → broadcaster fanout → centralized ticker → `SrtInitiator`
/// → WT datagram) without depending on the client-side TSBPD release clock.
#[tokio::test]
async fn gateway_delivers_published_data_over_wt() {
    init_tracing();
    let port = ephemeral_port();
    let (shutdown_tx, gateway_handle, publish_tx, hash) = start_gateway(port).await;
    tokio::time::sleep(Duration::from_millis(100)).await;

    let conn = connect_client(port, hash, "/wt?stream=test").await;

    let deadline = Instant::now() + Duration::from_secs(5);

    // Drive the SRT handshake to completion over real WT datagrams.
    drive_handshake(&conn, deadline).await;

    // Give the gateway's ticker a moment to register the connected initiator
    // before publishing, so the viewer subscription is wired up.
    tokio::time::sleep(Duration::from_millis(50)).await;

    // Publish a single 188-byte TS packet (sync byte 0x47) — fits one SRT
    // payload and is the unit the demuxer consumes.
    let mut ts = vec![0x47u8];
    ts.resize(188, 0);
    let published = Bytes::from(ts);
    publish_tx
        .send((Instant::now(), published.clone()))
        .await
        .expect("publish send");

    // The client must receive the published bytes, framed as an SRT data packet.
    let payload = recv_first_data_payload(&conn, deadline).await;
    assert_eq!(
        payload, published,
        "received SRT data payload must match the published TS message byte-for-byte"
    );

    drop(conn);
    shutdown_gateway(shutdown_tx, gateway_handle).await;
}
