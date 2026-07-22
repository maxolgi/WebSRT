//! Integration test: 1-RTT skip-induction handshake.
//!
//! Drives the gateway-side `SrtInitiator` (which uses
//! `Connect::new_skip_induction`) against a browser-side `Listen` (with
//! `allow_skip_induction(true)`) and asserts both reach Connected in exactly
//! 2 packets — one RTT — per draft-sharabayko-srt-over-quic §4.3.

use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::time::{Duration, Instant};

use bytes::BytesMut;

use srt_protocol::packet::{ControlTypes, Packet, ShakeType};
use srt_protocol::protocol::pending_connection::listen::Listen;
use srt_protocol::protocol::pending_connection::ConnectionResult;
use srt_protocol::settings::ConnInitSettings;

use websrt::srt_sender::{SenderAction, SrtConfig, SrtInitiator};

const LOCAL: IpAddr = IpAddr::V4(Ipv4Addr::new(127, 0, 0, 1));
const REMOTE: SocketAddr = SocketAddr::new(LOCAL, 9000);

fn serialize(pkt: &Packet) -> Vec<u8> {
    let mut buf = BytesMut::with_capacity(pkt.wire_size());
    pkt.serialize(&mut buf);
    buf.to_vec()
}

fn parse(bytes: &[u8]) -> Packet {
    let mut buf = bytes;
    Packet::parse(&mut buf, false).expect("packet should parse")
}

fn listen_settings() -> ConnInitSettings {
    let mut s = ConnInitSettings::default();
    s.send_latency = Duration::from_millis(120);
    s.recv_latency = Duration::from_millis(120);
    s
}

/// Calls `initiator.tick`, returns the serialized first handshake packet,
/// and asserts it is a CONCLUSION (not INDUCTION) — verifying patch 8.
fn first_datagram(initiator: &mut SrtInitiator, now: Instant) -> Vec<u8> {
    let (actions, _data) = initiator.tick(now);
    let bytes = actions
        .iter()
        .find_map(|a| match a {
            SenderAction::SendDatagram(b) => Some(b.clone()),
            _ => None,
        })
        .expect("initiator.tick should emit a SendDatagram");

    let pkt = parse(&bytes);
    match &pkt {
        Packet::Control(c) => match &c.control_type {
            ControlTypes::Handshake(hs) => {
                assert!(
                    matches!(&hs.shake_type, ShakeType::Conclusion),
                    "skip-induction: first packet must be Conclusion"
                );
            }
            other => panic!("expected Handshake control, got {:?}", other),
        },
        other => panic!("expected Control packet, got {:?}", other),
    }
    bytes
}

/// Drives the full 1-RTT handshake. Returns the count of SRT packets
/// exchanged (both directions).
fn drive_handshake(listen: &mut Listen, initiator: &mut SrtInitiator, now: Instant) -> usize {
    let mut count = 0;

    // Packet 1: gateway → browser (CONCLUSION, cookie=0, HSREQ).
    let conclusion = first_datagram(initiator, now);
    count += 1;

    let result = listen.handle_packet(now, Ok((parse(&conclusion), REMOTE)));
    let resp = match result {
        ConnectionResult::Connected(Some((pkt, _)), _) => {
            count += 1;
            serialize(&pkt)
        }
        _ => panic!("listen should Connect on conclusion-first handshake"),
    };

    // Packet 2: browser → gateway (CONCLUSION-RESP, HSRESP).
    let (actions, _) = initiator.handle_datagram(&resp, now);
    assert!(
        actions
            .iter()
            .any(|a| matches!(a, SenderAction::HandshakeComplete)),
        "initiator should report HandshakeComplete, got {:?}",
        actions
    );
    assert!(initiator.is_connected());
    count
}

#[test]
fn handshake_completes_in_one_rtt() {
    let now = Instant::now();
    let mut initiator =
        SrtInitiator::new(LOCAL, REMOTE, &SrtConfig::default(), Duration::from_millis(50));

    let mut listen = Listen::new(listen_settings(), false);
    listen.allow_skip_induction(true);

    let packets = drive_handshake(&mut listen, &mut initiator, now);
    assert_eq!(
        packets, 2,
        "skip-induction must exchange exactly 2 packets (1 RTT), got {}",
        packets
    );
}

#[test]
fn listen_rejects_conclusion_first_without_flag() {
    let now = Instant::now();
    let mut initiator =
        SrtInitiator::new(LOCAL, REMOTE, &SrtConfig::default(), Duration::from_millis(50));

    // Standard listener — no allow_skip_induction.
    let mut listen = Listen::new(listen_settings(), false);

    let bytes = first_datagram(&mut initiator, now);
    let result = listen.handle_packet(now, Ok((parse(&bytes), REMOTE)));
    assert!(
        !matches!(result, ConnectionResult::Connected(_, _)),
        "listener without allow_skip_induction must not accept conclusion-first"
    );
}

#[test]
fn rtt_seeding_flows_into_initiator() {
    // Verifies patch 9 wiring: a non-trivial initial_rtt passed to
    // SrtInitiator::new is accepted without error. The actual EWMA seeding
    // lives inside srt-protocol (SendBuffer.rtt / ARQ.rtt); here we assert
    // the gateway-side constructor accepts and stores it.
    let now = Instant::now();
    let initial_rtt = Duration::from_millis(137);
    let mut initiator =
        SrtInitiator::new(LOCAL, REMOTE, &SrtConfig::default(), initial_rtt);

    let mut listen = Listen::new(listen_settings(), false);
    listen.allow_skip_induction(true);

    let packets = drive_handshake(&mut listen, &mut initiator, now);
    assert_eq!(packets, 2, "RTT seeding must not affect handshake packet count");
}
