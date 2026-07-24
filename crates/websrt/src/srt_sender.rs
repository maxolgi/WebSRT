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

/// Configurable SRT protocol parameters.
#[derive(Debug, Clone)]
pub struct SrtConfig {
    /// SRT payload size in bytes (MTU minus headers).
    pub payload_size: u64,
    /// Send buffer depth in packets.
    pub send_buffer_size: u32,
    /// Receive buffer depth in packets.
    pub recv_buffer_size: u32,
    /// Peer idle timeout.
    pub peer_idle_timeout: std::time::Duration,
    /// TSBPD send latency.
    pub send_latency: std::time::Duration,
    /// TSBPD receive latency.
    pub recv_latency: std::time::Duration,
}

impl Default for SrtConfig {
    fn default() -> Self {
        Self {
            payload_size: PAYLOAD_SIZE,
            send_buffer_size: 8192,
            recv_buffer_size: 8192,
            peer_idle_timeout: std::time::Duration::from_secs(30),
            send_latency: std::time::Duration::from_millis(10),
            recv_latency: std::time::Duration::from_millis(10),
        }
    }
}

impl SrtConfig {
    /// Validate all fields. Returns an error with a descriptive message if any
    /// field is out of its safe range.
    pub fn validate(&self) -> anyhow::Result<()> {
        if self.payload_size < 100 {
            anyhow::bail!("payload_size must be >= 100, got {}", self.payload_size);
        }
        if self.payload_size > 1456 {
            anyhow::bail!("payload_size must be <= 1456, got {}", self.payload_size);
        }
        if self.send_buffer_size == 0 {
            anyhow::bail!("send_buffer_size must be >= 1");
        }
        if self.recv_buffer_size == 0 {
            anyhow::bail!("recv_buffer_size must be >= 1");
        }
        if self.peer_idle_timeout < std::time::Duration::from_secs(1) {
            anyhow::bail!(
                "peer_idle_timeout must be >= 1s, got {:?}",
                self.peer_idle_timeout
            );
        }
        if self.send_latency < std::time::Duration::from_millis(1) {
            anyhow::bail!(
                "send_latency must be >= 1ms, got {:?}",
                self.send_latency
            );
        }
        if self.recv_latency < std::time::Duration::from_millis(1) {
            anyhow::bail!(
                "recv_latency must be >= 1ms, got {:?}",
                self.recv_latency
            );
        }
        Ok(())
    }
}

/// Outgoing actions the gateway must take when driving the sender.
#[derive(Debug)]
pub enum SenderAction {
    /// Send these bytes as a WT datagram to the browser.
    SendDatagram(Vec<u8>),
    /// Handshake completed; we are now in the data plane.
    HandshakeComplete,
    /// Connection is closed (peer rejected, errored, or shut down).
    Close,
}

/// The gateway-side SRT state machine. Owns Connect during handshake and
/// DuplexConnection after.
pub struct SrtInitiator {
    state: InitiatorState,
    remote: SocketAddr,
    last_stats: Option<SocketStatistics>,
}

enum InitiatorState {
    /// CONCLUSION has not yet been sent (or is in-flight and may be re-sent).
    Handshaking(Connect),
    /// HSv5 complete; data plane running.
    Connected(DuplexConnection),
    Closed,
}

impl SrtInitiator {
    pub fn new(
        local_addr: IpAddr,
        remote: SocketAddr,
        config: &SrtConfig,
        initial_rtt: std::time::Duration,
    ) -> Self {
        let mut settings = ConnInitSettings::default();
        settings.max_packet_size = srt_protocol::options::PacketSize(config.payload_size);
        settings.send_buffer_size =
            srt_protocol::options::PacketCount(config.send_buffer_size.into());
        settings.recv_buffer_size =
            srt_protocol::options::PacketCount(config.recv_buffer_size.into());
        settings.peer_idle_timeout = config.peer_idle_timeout;
        settings.send_latency = config.send_latency;
        settings.recv_latency = config.recv_latency;
        settings.too_late_packet_drop = true;
        settings.initial_rtt = Some(initial_rtt);
        let connect = Connect::new_skip_induction(
            remote,
            local_addr,
            settings,
            None,
            SeqNumber::new(0).expect("seq 0"),
            Instant::now(),
        );
        Self {
            state: InitiatorState::Handshaking(connect),
            remote,
            last_stats: None,
        }
    }

    /// Drive the SRT state machine: kick off (or retransmit) the handshake,
    /// and when connected, drain pending output packets.
    ///
    /// Returns the actions to take and any upstream data the state machine
    /// released (browser→gateway publish path).
    pub fn tick(&mut self, now: Instant) -> (Vec<SenderAction>, Vec<(Instant, Bytes)>) {
        let mut out = Vec::new();
        let mut data: Vec<(Instant, Bytes)> = Vec::new();
        let mut new_stats: Option<SocketStatistics> = None;
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
                    Action::UpdateStatistics(s) => { new_stats = Some(s.clone()); }
                    Action::WaitForData(_) => {}
                    Action::ReleaseData((ts, bytes)) => {
                        data.push((ts, bytes));
                    }
                }
                drain(duplex, now, &mut out, &mut data, &mut new_stats);
            }
            InitiatorState::Closed => {}
        }
        if new_stats.is_some() { self.last_stats = new_stats; }
        (out, data)
    }

    /// Feed a WT datagram received from the browser (handshake reply or ACK/NAK).
    ///
    /// Returns the actions to take and any upstream data the state machine
    /// released (browser→gateway publish path).
    pub fn handle_datagram(
        &mut self,
        bytes: &[u8],
        now: Instant,
    ) -> (Vec<SenderAction>, Vec<(Instant, Bytes)>) {
        let mut out = Vec::new();
        let mut data: Vec<(Instant, Bytes)> = Vec::new();
        let mut new_stats: Option<SocketStatistics> = None;
        let packet = match parse_packet(bytes) {
            Ok(p) => p,
            Err(e) => {
                tracing::warn!(?e, "parse error");
                return (out, data);
            }
        };
        match &mut self.state {
            InitiatorState::Handshaking(connect) => {
                let r = connect.handle_packet(Ok((packet, self.remote)), now);
                process_hs_result(r, &mut self.state, &mut out);
            }
            InitiatorState::Connected(duplex) => {
                duplex.handle_packet_input(now, Ok((packet, self.remote)));
                drain(duplex, now, &mut out, &mut data, &mut new_stats);
            }
            InitiatorState::Closed => {}
        }
        if new_stats.is_some() { self.last_stats = new_stats; }
        (out, data)
    }

    /// Push a TS message into the sender's queue. No-op before handshake
    /// completes; the message is dropped (call sites should check
    /// `is_connected()` first or accept the drop).
    ///
    /// The gateway→browser SRT session has its own TSBPD timeline, independent
    /// of when upstream (OBS or browser publisher) released these bytes. We
    /// therefore stamp the outgoing packet with `now` — not `msg.0` (the
    /// upstream release instant, which is already in the past by the time the
    /// packet traverses the broadcaster + ticker). Using `msg.0` causes the
    /// browser receiver to drop the packet as PacketTooLate.
    pub fn push_message(
        &mut self,
        msg: (Instant, Bytes),
        now: Instant,
    ) -> (Vec<SenderAction>, Vec<(Instant, Bytes)>) {
        let mut out = Vec::new();
        let mut data: Vec<(Instant, Bytes)> = Vec::new();
        let mut new_stats: Option<SocketStatistics> = None;
        if let InitiatorState::Connected(duplex) = &mut self.state {
            let (_, bytes) = msg;
            duplex.handle_data_input(now, Some((now, bytes)));
            drain(duplex, now, &mut out, &mut data, &mut new_stats);
        }
        if new_stats.is_some() { self.last_stats = new_stats; }
        (out, data)
    }

    pub fn is_connected(&self) -> bool {
        matches!(self.state, InitiatorState::Connected(_))
    }

    pub fn stats(&self) -> Option<&SocketStatistics> {
        self.last_stats.as_ref()
    }
}

fn drain(
    duplex: &mut DuplexConnection,
    now: Instant,
    out: &mut Vec<SenderAction>,
    data: &mut Vec<(Instant, Bytes)>,
    stats: &mut Option<SocketStatistics>,
) {
    loop {
        let action = duplex.handle_input(now, Input::DataReleased);
        match action {
            Action::SendPacket((pkt, _addr)) => {
                out.push(SenderAction::SendDatagram(serialize_packet(&pkt)));
            }
            Action::ReleaseData((ts, bytes)) => {
                data.push((ts, bytes));
            }
            Action::UpdateStatistics(s) => {
                *stats = Some(s.clone());
                continue;
            }
            Action::WaitForData(_) => {
                return;
            }
            Action::Close => {
                out.push(SenderAction::Close);
                return;
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
            tracing::warn!(%e, "hs not-handled");
        }
        ConnectionResult::Reject(maybe_pkt, rej) => {
            if let Some((pkt, _)) = maybe_pkt {
                out.push(SenderAction::SendDatagram(serialize_packet(&pkt)));
            }
            tracing::warn!(?rej, "hs rejected");
            *state = InitiatorState::Closed;
            out.push(SenderAction::Close);
        }
        ConnectionResult::Failure(e) => {
            tracing::warn!(%e, "hs io failure");
            *state = InitiatorState::Closed;
            out.push(SenderAction::Close);
        }
        ConnectionResult::RequestAccess(_) => {
            tracing::warn!("access control (unsupported)");
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_config_is_valid() {
        assert!(SrtConfig::default().validate().is_ok());
    }

    #[test]
    fn payload_size_too_small() {
        let mut c = SrtConfig::default();
        c.payload_size = 50;
        assert!(c.validate().is_err());
    }

    #[test]
    fn payload_size_too_large() {
        let mut c = SrtConfig::default();
        c.payload_size = 2000;
        assert!(c.validate().is_err());
    }

    #[test]
    fn send_buffer_zero_rejected() {
        let mut c = SrtConfig::default();
        c.send_buffer_size = 0;
        assert!(c.validate().is_err());
    }

    #[test]
    fn recv_buffer_zero_rejected() {
        let mut c = SrtConfig::default();
        c.recv_buffer_size = 0;
        assert!(c.validate().is_err());
    }

    #[test]
    fn idle_timeout_below_1s_rejected() {
        let mut c = SrtConfig::default();
        c.peer_idle_timeout = std::time::Duration::from_millis(500);
        assert!(c.validate().is_err());
    }

    #[test]
    fn send_latency_zero_rejected() {
        let mut c = SrtConfig::default();
        c.send_latency = std::time::Duration::from_millis(0);
        assert!(c.validate().is_err());
    }

    #[test]
    fn recv_latency_zero_rejected() {
        let mut c = SrtConfig::default();
        c.recv_latency = std::time::Duration::from_millis(0);
        assert!(c.validate().is_err());
    }

    #[test]
    fn payload_size_boundary_100_ok() {
        let mut c = SrtConfig::default();
        c.payload_size = 100;
        assert!(c.validate().is_ok());
    }

    #[test]
    fn payload_size_boundary_1456_ok() {
        let mut c = SrtConfig::default();
        c.payload_size = 1456;
        assert!(c.validate().is_ok());
    }
}
