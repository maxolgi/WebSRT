//! `srt-wasm` — wasm32 wrapper around `srt_protocol` for the browser side of the
//! SRT-over-WebTransport gateway.
//!
//! The browser is the **listener** (in SRT terminology): it accepts an HSv5
//! handshake initiated by the gateway, then receives data packets. This crate
//! exposes a minimal wasm-bindgen API shaped around the natural polling style
//! of `srt-protocol`:
//!
//! ```text
//!   JS calls handle_datagram(bytes, now_us)  →  Vec<SrtAction>
//!   JS calls poll(now_us)                    →  Vec<SrtAction>
//! ```
//!
//! `SrtAction` covers everything JS needs to do: send a WT datagram, deliver a
//! reassembled TS message to the demuxer, signal handshake completion, or wait.

use srt_protocol::connection::{Action, DuplexConnection, Input};
use srt_protocol::packet::Packet;
use srt_protocol::protocol::pending_connection::listen::Listen;
use srt_protocol::protocol::pending_connection::{ConnectionResult};
use srt_protocol::settings::ConnInitSettings;
use srt_protocol::statistics::SocketStatistics;
use std::cell::{Cell, RefCell};
use std::net::SocketAddr;
use std::str::FromStr;
use std::time::Duration;
use wasm_bindgen::prelude::*;

/// SRT payload MTU we assume. The gateway must match.
pub const PAYLOAD_SIZE: u64 = 1100;

/// Dummy peer address (we carry it vestigially; srt-protocol needs a SocketAddr
/// for bookkeeping but it's never used on the WT path).
const PEER: &str = "127.0.0.1:0";

/// An action JS must take.
#[wasm_bindgen]
pub struct SrtAction {
    kind: u8,
    data: Vec<u8>,
    text: String,
    wait_ms: f64,
}

#[wasm_bindgen]
impl SrtAction {
    /// 0 = SendDatagram, 1 = DeliverMessage, 2 = HandshakeComplete,
    /// 3 = WaitForData, 4 = Close, 5 = Log.
    #[wasm_bindgen(getter)]
    pub fn kind(&self) -> u8 { self.kind }

    #[wasm_bindgen(getter)]
    pub fn data(&self) -> Vec<u8> { self.data.clone() }

    #[wasm_bindgen(js_name = takeData)]
    pub fn take_data(&mut self) -> Vec<u8> {
        std::mem::take(&mut self.data)
    }

    #[wasm_bindgen(getter)]
    pub fn text(&self) -> String { self.text.clone() }

    #[wasm_bindgen(getter)]
    pub fn wait_ms(&self) -> f64 { self.wait_ms }

    fn send(bytes: Vec<u8>) -> Self {
        Self { kind: 0, data: bytes, text: String::new(), wait_ms: 0.0 }
    }
    fn deliver(bytes: Vec<u8>) -> Self {
        Self { kind: 1, data: bytes, text: String::new(), wait_ms: 0.0 }
    }
    fn hs_done() -> Self {
        Self { kind: 2, data: Vec::new(), text: String::new(), wait_ms: 0.0 }
    }
    fn wait(d: Duration) -> Self {
        Self { kind: 3, data: Vec::new(), text: String::new(), wait_ms: d.as_secs_f64() * 1000.0 }
    }
    fn close() -> Self {
        Self { kind: 4, data: Vec::new(), text: String::new(), wait_ms: 0.0 }
    }
    fn log(s: impl Into<String>) -> Self {
        Self { kind: 5, data: Vec::new(), text: s.into(), wait_ms: 0.0 }
    }
}

#[wasm_bindgen]
pub struct SrtStats {
    elapsed_ms: f64,
    rx_data: u64,
    rx_bytes: u64,
    rx_loss: u64,
    rx_retransmit: u64,
    rx_dropped: u64,
    rx_ack: u64,
    rx_nak: u64,
    rtt_ms: f64,
    bandwidth_bps: u64,
    rx_buffered: u64,
    rx_belated: u64,
    tx_data: u64,
    tx_bytes: u64,
    tx_retransmit: u64,
    tx_loss: u64,
    tx_buffered: u64,
}

#[wasm_bindgen]
impl SrtStats {
    // All numeric getters return f64 so JS sees plain Numbers, never BigInt.
    // (u64 getters surface as BigInt in wasm-bindgen, which throws on
    // `bigint / number` — the latent bandwidthBps bug.)
    #[wasm_bindgen(getter)] pub fn elapsedMs(&self) -> f64 { self.elapsed_ms }
    #[wasm_bindgen(getter)] pub fn rxData(&self) -> f64 { self.rx_data as f64 }
    #[wasm_bindgen(getter)] pub fn rxBytes(&self) -> f64 { self.rx_bytes as f64 }
    #[wasm_bindgen(getter)] pub fn rxLoss(&self) -> f64 { self.rx_loss as f64 }
    #[wasm_bindgen(getter)] pub fn rxRetransmit(&self) -> f64 { self.rx_retransmit as f64 }
    #[wasm_bindgen(getter)] pub fn rxDropped(&self) -> f64 { self.rx_dropped as f64 }
    #[wasm_bindgen(getter)] pub fn rxAck(&self) -> f64 { self.rx_ack as f64 }
    #[wasm_bindgen(getter)] pub fn rxNak(&self) -> f64 { self.rx_nak as f64 }
    #[wasm_bindgen(getter)] pub fn rttMs(&self) -> f64 { self.rtt_ms }
    #[wasm_bindgen(getter)] pub fn bandwidthBps(&self) -> f64 { self.bandwidth_bps as f64 }
    #[wasm_bindgen(getter)] pub fn rxBuffered(&self) -> f64 { self.rx_buffered as f64 }
    #[wasm_bindgen(getter)] pub fn rxBelated(&self) -> f64 { self.rx_belated as f64 }
    #[wasm_bindgen(getter)] pub fn txData(&self) -> f64 { self.tx_data as f64 }
    #[wasm_bindgen(getter)] pub fn txBytes(&self) -> f64 { self.tx_bytes as f64 }
    #[wasm_bindgen(getter)] pub fn txRetransmit(&self) -> f64 { self.tx_retransmit as f64 }
    #[wasm_bindgen(getter)] pub fn txLoss(&self) -> f64 { self.tx_loss as f64 }
    #[wasm_bindgen(getter)] pub fn txBuffered(&self) -> f64 { self.tx_buffered as f64 }
}

enum State {
    /// HSv5 handshake in progress.
    Handshaking(Listen),
    /// Handshake complete; data plane running.
    Connected(DuplexConnection),
    /// Never started.
    New,
    /// Closed (peer shutdown or error).
    Closed,
}

/// Browser-side SRT receiver.
#[wasm_bindgen]
pub struct SrtReceiver {
    state: RefCell<State>,
    epoch: web_time::Instant,
    remote: SocketAddr,
    stats: RefCell<SocketStatistics>,
    prev_tx_bytes: Cell<u64>,
    prev_stats_time: Cell<web_time::Instant>,
}

fn now_from_us(epoch: web_time::Instant, now_us: f64) -> web_time::Instant {
    if !now_us.is_finite() || now_us < 0.0 {
        return epoch;
    }
    epoch + Duration::from_micros(now_us as u64)
}

#[wasm_bindgen]
impl SrtReceiver {
    /// Construct a fresh receiver. Local socket id is randomized internally.
    #[wasm_bindgen(constructor)]
    pub fn new() -> SrtReceiver {
        Self::new_inner(120, None)
    }

    /// Construct with a custom TSBPD latency (milliseconds).
    #[wasm_bindgen(js_name = newWithLatency)]
    pub fn new_with_latency(latency_ms: u32) -> SrtReceiver {
        Self::new_inner(latency_ms, None)
    }

    /// Construct with TSBPD latency and initial RTT estimate (milliseconds).
    /// The RTT seeds SRT's EWMA for accurate cold-start retransmit timing.
    #[wasm_bindgen(js_name = newWithLatencyAndRtt)]
    pub fn new_with_latency_and_rtt(latency_ms: u32, initial_rtt_ms: f64) -> SrtReceiver {
        Self::new_inner(latency_ms, Some(initial_rtt_ms))
    }

    fn new_inner(latency_ms: u32, initial_rtt_ms: Option<f64>) -> SrtReceiver {
        console_error_panic_hook::set_once();
        let mut init = ConnInitSettings::default();
        init.send_buffer_size = srt_protocol::options::PacketCount(8192);
        init.max_packet_size = srt_protocol::options::PacketSize(PAYLOAD_SIZE);
        init.recv_buffer_size = srt_protocol::options::PacketCount(8192);
        init.send_latency = std::time::Duration::from_millis(latency_ms as u64);
        init.recv_latency = std::time::Duration::from_millis(latency_ms as u64);
        init.too_late_packet_drop = true;
        if let Some(rtt) = initial_rtt_ms {
            init.initial_rtt = Some(std::time::Duration::from_millis(rtt as u64));
        }
        let mut listen = Listen::new(init, false);
        listen.allow_skip_induction(true);
        let now = web_time::Instant::now();
        SrtReceiver {
            state: RefCell::new(State::Handshaking(listen)),
            epoch: now,
            remote: SocketAddr::from_str(PEER).expect("hardcoded addr"),
            stats: RefCell::new(SocketStatistics::new()),
            prev_tx_bytes: Cell::new(0),
            prev_stats_time: Cell::new(now),
        }
    }

    /// Feed an incoming WebTransport datagram (raw SRT packet bytes).
    /// Returns actions JS should perform: send datagrams, deliver messages, etc.
    pub fn handle_datagram(&self, bytes: &[u8], now_us: f64) -> Vec<SrtAction> {
        let now = now_from_us(self.epoch, now_us);
        let mut out: Vec<SrtAction> = Vec::new();

        // Parse the packet first; bail out cleanly on parse errors (the gateway
        // might send handshake packets that look odd before HS completes).
        let mut buf = bytes;
        let packet = match Packet::parse(&mut buf, false) {
            Ok(p) => p,
            Err(e) => {
                out.push(SrtAction::log(format!("parse error: {e:?}")));
                return out;
            }
        };

        let mut state = self.state.borrow_mut();
        match &mut *state {
            State::New => {
                out.push(SrtAction::log("received datagram before start"));
            }
            State::Closed => {
                // ignore
            }
            State::Handshaking(listen) => {
                let result = listen.handle_packet(now, Ok((packet, self.remote)));
                match result {
                    ConnectionResult::SendPacket((pkt, _addr)) => {
                        out.push(SrtAction::send(serialize_packet(&pkt)));
                    }
                    ConnectionResult::Connected(maybe_pkt, conn) => {
                        if let Some((pkt, _)) = maybe_pkt {
                            out.push(SrtAction::send(serialize_packet(&pkt)));
                        }
                        // graduate to data-plane
                        let mut duplex = DuplexConnection::new(conn);
                        out.push(SrtAction::hs_done());
                        // drain any post-handshake actions (e.g., initial ACK)
                        drain(&mut duplex, now, &mut out, &self.stats);
                        *state = State::Connected(duplex);
                    }
                    ConnectionResult::NoAction => {}
                    ConnectionResult::NotHandled(e) => {
                        out.push(SrtAction::log(format!("handshake not-handled: {e:?}")));
                    }
                    ConnectionResult::Reject(maybe_pkt, rej) => {
                        if let Some((pkt, _)) = maybe_pkt {
                            out.push(SrtAction::send(serialize_packet(&pkt)));
                        }
                        out.push(SrtAction::log(format!("rejected: {rej:?}")));
                        *state = State::Closed;
                        out.push(SrtAction::close());
                    }
                    ConnectionResult::Failure(e) => {
                        out.push(SrtAction::log(format!("handshake io: {e}")));
                        *state = State::Closed;
                        out.push(SrtAction::close());
                    }
                    ConnectionResult::RequestAccess(_) => {
                        out.push(SrtAction::log("access control requested (unsupported)"));
                    }
                }
            }
            State::Connected(duplex) => {
                duplex.handle_packet_input(now, Ok((packet, self.remote)));
                drain(duplex, now, &mut out, &self.stats);
                if !duplex.is_open() {
                    *state = State::Closed;
                }
            }
        }
        out
    }

    /// Feed upstream TS data into the SRT sender half of the DuplexConnection.
    /// Returns actions (SendDatagram with data packets, etc.) that JS must process.
    /// No-op if the handshake hasn't completed yet.
    #[wasm_bindgen(js_name = sendMessage)]
    pub fn send_message(&self, bytes: &[u8], now_us: f64) -> Vec<SrtAction> {
        let now = now_from_us(self.epoch, now_us);
        let mut state = self.state.borrow_mut();
        match &mut *state {
            State::Connected(duplex) => {
                duplex.handle_data_input(now, Some((now, bytes::Bytes::copy_from_slice(bytes))));
                let mut out: Vec<SrtAction> = Vec::new();
                drain(duplex, now, &mut out, &self.stats);
                if !duplex.is_open() {
                    *state = State::Closed;
                }
                out
            }
            _ => Vec::new(),
        }
    }

    /// Periodic tick. JS calls this every ~10ms (setTimeout) to advance the
    /// state machine even when no datagrams arrive.
    pub fn poll(&self, now_us: f64) -> Vec<SrtAction> {
        let now = now_from_us(self.epoch, now_us);
        let mut out: Vec<SrtAction> = Vec::new();
        let mut state = self.state.borrow_mut();
        match &mut *state {
            State::Connected(duplex) => {
                drain(duplex, now, &mut out, &self.stats);
                if !duplex.is_open() {
                    *state = State::Closed;
                }
            }
            State::Handshaking(_) => {
                // Listen::handle_timer is a no-op; nothing to do.
            }
            _ => {}
        }
        out
    }

    /// True once the HSv5 handshake has completed and data plane is running.
    #[wasm_bindgen(js_name = isHandshakeComplete)]
    pub fn is_handshake_complete(&self) -> bool {
        matches!(*self.state.borrow(), State::Connected(_))
    }

    /// True if the peer shut down or the connection errored out.
    #[wasm_bindgen(js_name = isClosed)]
    pub fn is_closed(&self) -> bool {
        matches!(*self.state.borrow(), State::Closed)
    }

    /// Latest SRT socket statistics.
    #[wasm_bindgen(js_name = getStats)]
    pub fn get_stats(&self) -> SrtStats {
        let s = self.stats.borrow();
        // srt-protocol computes rx_bandwidth but never tx_bandwidth. Derive
        // the send rate from the tx_bytes delta since the last getStats()
        // call so a publishing session reports real throughput.
        let now = web_time::Instant::now();
        let prev_bytes = self.prev_tx_bytes.get();
        let prev_time = self.prev_stats_time.get();
        let tx_bw = {
            let dt = (now - prev_time).as_secs_f64();
            if dt > 0.0 {
                let delta = s.tx_bytes.saturating_sub(prev_bytes);
                (delta as f64 * 8.0 / dt) as u64
            } else {
                0
            }
        };
        self.prev_tx_bytes.set(s.tx_bytes);
        self.prev_stats_time.set(now);
        SrtStats {
            elapsed_ms: s.elapsed_time.as_secs_f64() * 1000.0,
            rx_data: s.rx_data,
            rx_bytes: s.rx_bytes,
            rx_loss: s.rx_loss_data,
            rx_retransmit: s.rx_retransmit_data,
            rx_dropped: s.rx_dropped_data,
            rx_ack: s.rx_ack,
            rx_nak: s.rx_nak,
            rtt_ms: s.rx_average_rtt.max(s.tx_average_rtt).as_secs_f64() * 1000.0,
            bandwidth_bps: s.rx_bandwidth.max(tx_bw),
            rx_buffered: s.rx_acknowledged_data,
            rx_belated: s.rx_belated_data,
            tx_data: s.tx_data,
            tx_bytes: s.tx_bytes,
            tx_retransmit: s.tx_retransmit_data,
            tx_loss: s.tx_loss_data,
            tx_buffered: s.tx_buffered_data,
        }
    }
}

fn drain(
    conn: &mut DuplexConnection,
    now: web_time::Instant,
    out: &mut Vec<SrtAction>,
    stats: &RefCell<SocketStatistics>,
) {
    let mut tick = true;
    loop {
        let action = if tick {
            tick = false;
            conn.handle_input(now, Input::Timer)
        } else {
            conn.handle_input(now, Input::DataReleased)
        };
        match action {
            Action::SendPacket((pkt, _addr)) => {
                out.push(SrtAction::send(serialize_packet(&pkt)));
            }
            Action::ReleaseData((_ts, bytes)) => out.push(SrtAction::deliver(bytes.to_vec())),
            Action::UpdateStatistics(s) => { *stats.borrow_mut() = s.clone(); continue; }
            Action::WaitForData(d) => {
                if !d.is_zero() {
                    out.push(SrtAction::wait(d));
                }
                break;
            }
            Action::Close => {
                out.push(SrtAction::close());
                break;
            }
        }
    }
}

fn serialize_packet(pkt: &Packet) -> Vec<u8> {
    use bytes::BytesMut;
    let mut buf = BytesMut::with_capacity(pkt.wire_size());
    pkt.serialize(&mut buf);
    buf.to_vec()
}

impl Default for SrtReceiver {
    fn default() -> Self {
        Self::new()
    }
}
