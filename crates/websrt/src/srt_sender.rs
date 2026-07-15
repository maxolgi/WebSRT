//! `srt_sender` — gateway-side SRT initiator (caller) driver over WebTransport.
//!
//! For each browser session we instantiate a fresh SRT caller (the gateway is
//! always the initiator in this architecture: it has the data, the browser is
//! the listener). The HSv5 handshake is driven over WT datagrams; on success
//! the inner `Connect` state machine hands us a `Connection` which we wrap in
//! `DuplexConnection` for the data plane.

use bytes::Bytes;
use bytes::BytesMut;
use srt_protocol::connection::{Action, DuplexConnection, Input};
use srt_protocol::packet::Packet;
use srt_protocol::packet::SeqNumber;
use srt_protocol::protocol::pending_connection::connect::Connect;
use srt_protocol::protocol::pending_connection::ConnectionResult;
use srt_protocol::settings::ConnInitSettings;
use srt_protocol::statistics::SocketStatistics;
use std::net::{IpAddr, SocketAddr};
use std::time::Instant;

/// WebTransport datagram PMTU — must match the wasm-side constant.
pub const PAYLOAD_SIZE: u64 = 1100;

/// Outgoing actions the gateway must take when driving the sender.
#[derive(Debug)]
pub enum SenderAction {
    /// Send these bytes as a WT datagram to the browser.
    SendDatagram(Vec<u8>),
    /// Handshake completed; we are now in the data plane.
    HandshakeComplete,
    /// Connection is closed (peer rejected, errored, or shut down).
    Close,
    /// Informational log message.
    Log(String),
}

/// The gateway-side SRT state machine. Owns Connect during handshake and
/// DuplexConnection after.
pub struct SrtInitiator {
    state: InitiatorState,
    remote: SocketAddr,
    #[allow(dead_code)]
    local_addr: IpAddr,
    last_stats: Option<SocketStatistics>,
    pushed: u64,
}

enum InitiatorState {
    /// INDUCTION has not yet been sent (or is in-flight and may be re-sent).
    Handshaking(Connect),
    /// HSv5 complete; data plane running.
    Connected(DuplexConnection),
    Closed,
}

impl SrtInitiator {
    pub fn new(local_addr: IpAddr, remote: SocketAddr, latency_ms: u64) -> Self {
        let mut settings = ConnInitSettings::default();
        settings.max_packet_size = srt_protocol::options::PacketSize(PAYLOAD_SIZE);
        settings.send_buffer_size = srt_protocol::options::PacketCount(8192);
        settings.recv_buffer_size = srt_protocol::options::PacketCount(8192);
        settings.peer_idle_timeout = std::time::Duration::from_secs(30);
        settings.send_latency = std::time::Duration::from_millis(latency_ms);
        settings.recv_latency = std::time::Duration::from_millis(latency_ms);
        settings.too_late_packet_drop = true;
        Self::new_with_settings(local_addr, remote, settings)
    }

    pub fn new_with_settings(
        local_addr: IpAddr,
        remote: SocketAddr,
        settings: ConnInitSettings,
    ) -> Self {
        let connect = Connect::new(
            remote,
            local_addr,
            settings,
            None,
            SeqNumber::new(0).expect("seq 0"),
        );
        Self {
            state: InitiatorState::Handshaking(connect),
            remote,
            local_addr,
            last_stats: None,
            pushed: 0,
        }
    }

    /// Kick off (or retransmit) the handshake. Also drives the data plane when
    /// connected (drains pending output packets).
    pub fn tick(&mut self, now: Instant) -> Vec<SenderAction> {
        let mut out = Vec::new();
        match &mut self.state {
            InitiatorState::Handshaking(connect) => {
                let r = connect.handle_tick(now);
                process_hs_result(r, &mut self.state, &mut out);
            }
            InitiatorState::Connected(duplex) => {
                match duplex.handle_input(now, Input::Timer) {
                    Action::SendPacket((pkt, _addr)) => {
                        out.push(SenderAction::SendDatagram(serialize_packet(&pkt)));
                    }
                    Action::Close => {
                        out.push(SenderAction::Close);
                    }
                    Action::UpdateStatistics(s) => {
                        self.last_stats = Some(s.clone());
                    }
                    _ => {}
                }
                drain(duplex, now, &mut out, &mut self.last_stats);
            }
            InitiatorState::Closed => {}
        }
        out
    }

    /// Feed a WT datagram received from the browser (handshake reply or ACK/NAK).
    pub fn handle_datagram(&mut self, bytes: &[u8], now: Instant) -> Vec<SenderAction> {
        let mut out = Vec::new();
        let packet = match parse_packet(bytes) {
            Ok(p) => p,
            Err(e) => {
                out.push(SenderAction::Log(format!("parse error: {e:?}")));
                return out;
            }
        };
        match &mut self.state {
            InitiatorState::Handshaking(connect) => {
                let r = connect.handle_packet(Ok((packet, self.remote)), now);
                process_hs_result(r, &mut self.state, &mut out);
            }
            InitiatorState::Connected(duplex) => {
                duplex.handle_packet_input(now, Ok((packet, self.remote)));
                drain(duplex, now, &mut out, &mut self.last_stats);
            }
            InitiatorState::Closed => {}
        }
        out
    }

    /// Push a TS message into the sender's queue. No-op before handshake
    /// completes; the message is dropped (call sites should check
    /// `is_connected()` first or accept the drop).
    pub fn push_message(&mut self, msg: (Instant, Bytes), now: Instant) -> Vec<SenderAction> {
        let mut out = Vec::new();
        if let InitiatorState::Connected(duplex) = &mut self.state {
            self.pushed += 1;
            if self.pushed <= 3 || self.pushed % 100 == 0 {
                tracing::debug!(pushed = self.pushed, bytes = msg.1.len(), "push_message: to sender");
            }
            duplex.handle_data_input(now, Some(msg));
            drain(duplex, now, &mut out, &mut self.last_stats);
        }
        out
    }

    pub fn is_connected(&self) -> bool {
        matches!(self.state, InitiatorState::Connected(_))
    }

    pub fn is_closed(&self) -> bool {
        matches!(self.state, InitiatorState::Closed)
    }

    pub fn stats(&self) -> Option<&SocketStatistics> {
        self.last_stats.as_ref()
    }
}

fn drain(
    duplex: &mut DuplexConnection,
    now: Instant,
    out: &mut Vec<SenderAction>,
    stats: &mut Option<SocketStatistics>,
) {
    loop {
        let action = duplex.handle_input(now, Input::DataReleased);
        match action {
            Action::SendPacket((pkt, _addr)) => {
                out.push(SenderAction::SendDatagram(serialize_packet(&pkt)));
            }
            Action::ReleaseData((_ts, _bytes)) => {
                // Receiver-side output — gateway sender shouldn't emit data to
                // the browser. Drop.
            }
            Action::UpdateStatistics(s) => { *stats = Some(s.clone()); continue; }
            Action::WaitForData(_) => break,
            Action::Close => {
                out.push(SenderAction::Close);
                break;
            }
        }
    }
}

fn process_hs_result(
    result: ConnectionResult,
    state: &mut InitiatorState,
    out: &mut Vec<SenderAction>,
) {
    match result {
        ConnectionResult::SendPacket((pkt, _addr)) => {
            out.push(SenderAction::SendDatagram(serialize_packet(&pkt)));
        }
        ConnectionResult::Connected(maybe_pkt, conn) => {
            if let Some((pkt, _)) = maybe_pkt {
                out.push(SenderAction::SendDatagram(serialize_packet(&pkt)));
            }
            *state = InitiatorState::Connected(DuplexConnection::new(conn));
            out.push(SenderAction::HandshakeComplete);
        }
        ConnectionResult::NoAction => {}
        ConnectionResult::NotHandled(e) => {
            out.push(SenderAction::Log(format!("hs not-handled: {e}")));
        }
        ConnectionResult::Reject(maybe_pkt, rej) => {
            if let Some((pkt, _)) = maybe_pkt {
                out.push(SenderAction::SendDatagram(serialize_packet(&pkt)));
            }
            out.push(SenderAction::Log(format!("rejected: {rej:?}")));
            *state = InitiatorState::Closed;
            out.push(SenderAction::Close);
        }
        ConnectionResult::Failure(e) => {
            out.push(SenderAction::Log(format!("hs io failure: {e}")));
            *state = InitiatorState::Closed;
            out.push(SenderAction::Close);
        }
        ConnectionResult::RequestAccess(_) => {
            out.push(SenderAction::Log("access control (unsupported)".into()));
        }
    }
}

fn parse_packet(bytes: &[u8]) -> Result<Packet, srt_protocol::packet::PacketParseError> {
    let mut buf: &[u8] = bytes;
    Packet::parse(&mut buf, false)
}

fn serialize_packet(pkt: &Packet) -> Vec<u8> {
    let mut buf = BytesMut::with_capacity(pkt.wire_size());
    pkt.serialize(&mut buf);
    buf.to_vec()
}
