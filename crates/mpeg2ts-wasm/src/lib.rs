//! `mpeg2ts-wasm` — wasm32 wrapper around `mpeg2ts` for the browser.
//!
//! JS calls `feed(bytes)` with chunks of TS data. Internally, we accumulate
//! bytes and parse 188-byte TS packets, emitting events back as `TsEvent`s:
//!   - `pat` — Program Association Table seen (program_map_pid list)
//!   - `pmt` — Program Map Table seen (elementary PIDs + stream types)
//!   - `pes` — Reassembled PES packet (pid, pts?, dts?, payload bytes)
//!   - `random_access` — adaptation_field.random_access_indicator set on a PID
//!   - `error` — recoverable parse error (e.g., unknown PID before PMT)
//!
//! PES reassembly is implemented per-PID here (we drive `TsPacketReader`
//! directly so we keep PSI events). The logic mirrors `PesPacketReader` in the
//! upstream crate.

use mpeg2ts::es::StreamType;
use mpeg2ts::pes::PesHeader;
use mpeg2ts::ts::{Pid, ReadTsPacket, TsPacket, TsPacketReader, TsPayload};
use std::cell::RefCell;
use std::collections::HashMap;
use std::io::{self, Read};
use std::rc::Rc;
use wasm_bindgen::prelude::*;

const TS_PACKET_SIZE: usize = 188;

/// Shared append-only buffer with read cursor, so the long-lived
/// `TsPacketReader` inside our struct can be advanced by JS calls to `feed`
/// without rebuilding the reader (which would lose its PID state).
#[derive(Default)]
struct FeedBuf {
    data: Vec<u8>,
    pos: usize,
}

impl FeedBuf {
    fn append(&mut self, bytes: &[u8]) {
        // Compact when caught up so the buffer doesn't grow forever.
        if self.pos >= self.data.len() {
            self.data.clear();
            self.pos = 0;
        }
        self.data.extend_from_slice(bytes);
    }

    fn has_packet(&self) -> bool {
        self.data.len() - self.pos >= TS_PACKET_SIZE
    }
}

impl Read for FeedBuf {
    fn read(&mut self, out: &mut [u8]) -> io::Result<usize> {
        let avail = self.data.len() - self.pos;
        if avail == 0 {
            return Ok(0);
        }
        let n = avail.min(out.len());
        out[..n].copy_from_slice(&self.data[self.pos..self.pos + n]);
        self.pos += n;
        Ok(n)
    }
}

/// `Rc<RefCell<…>>` wrapper around `FeedBuf` so the same backing buffer is
/// shared between the wasm-glue (which appends) and the `TsPacketReader` (which
/// reads). `Read` impl borrows the inner cell.
#[derive(Clone)]
struct SharedFeedBuf(Rc<RefCell<FeedBuf>>);

impl SharedFeedBuf {
    fn new() -> Self {
        Self(Rc::new(RefCell::new(FeedBuf::default())))
    }
    fn append(&self, bytes: &[u8]) {
        self.0.borrow_mut().append(bytes);
    }
    fn has_packet(&self) -> bool {
        self.0.borrow().has_packet()
    }
}

impl Read for SharedFeedBuf {
    fn read(&mut self, out: &mut [u8]) -> io::Result<usize> {
        self.0.borrow_mut().read(out)
    }
}

/// A TS event JS consumes.
#[wasm_bindgen]
pub struct TsEvent {
    kind: u8,
    pid: u16,
    pts: i64, // -1 if absent
    dts: i64,
    stream_type: u8,
    data: Vec<u8>,
    text: String,
    program_num: u16,
    random_access: bool,
}

#[wasm_bindgen]
impl TsEvent {
    /// 0 = pat, 1 = pmt, 2 = pes, 3 = random_access, 4 = error
    #[wasm_bindgen(getter)]
    pub fn kind(&self) -> u8 { self.kind }
    #[wasm_bindgen(getter)]
    pub fn pid(&self) -> u16 { self.pid }
    #[wasm_bindgen(getter)]
    pub fn pts(&self) -> f64 {
        if self.pts < 0 { -1.0 } else { self.pts as f64 }
    }
    #[wasm_bindgen(getter)]
    pub fn dts(&self) -> f64 {
        if self.dts < 0 { -1.0 } else { self.dts as f64 }
    }
    #[wasm_bindgen(getter)]
    pub fn stream_type(&self) -> u8 { self.stream_type }
    #[wasm_bindgen(getter)]
    pub fn data(&self) -> Vec<u8> { self.data.clone() }
    #[wasm_bindgen(getter)]
    pub fn text(&self) -> String { self.text.clone() }
    #[wasm_bindgen(getter)]
    pub fn program_num(&self) -> u16 { self.program_num }
    #[wasm_bindgen(getter, js_name = randomAccess)]
    pub fn random_access_get(&self) -> bool { self.random_access }

    /// For PMT events: flat array of [pid0, stream_type0, pid1, stream_type1, ...].
    #[wasm_bindgen(js_name = pmtEntries)]
    pub fn pmt_entries(&self) -> Vec<u16> {
        let mut out = Vec::with_capacity(self.data.len() / 4);
        let mut i = 0;
        while i + 4 <= self.data.len() {
            let pid = u16::from_le_bytes([self.data[i], self.data[i + 1]]);
            let st = u16::from_le_bytes([self.data[i + 2], self.data[i + 3]]);
            out.push(pid);
            out.push(st);
            i += 4;
        }
        out
    }
}

impl TsEvent {
    fn pat(program_num: u16, pmt_pid: u16) -> Self {
        Self {
            kind: 0, pid: pmt_pid, pts: -1, dts: -1, stream_type: 0,
            data: Vec::new(), text: String::new(),
            program_num, random_access: false,
        }
    }
    fn pmt(entries: &[(Pid, StreamType)]) -> Self {
        let mut data = Vec::with_capacity(entries.len() * 4);
        for (pid, st) in entries {
            data.extend_from_slice(&pid.as_u16().to_le_bytes());
            data.extend_from_slice(&(st.clone() as u16).to_le_bytes());
        }
        Self {
            kind: 1, pid: 0, pts: -1, dts: -1, stream_type: 0,
            data, text: String::new(),
            program_num: 0, random_access: false,
        }
    }
    fn pes(pid: Pid, header: &PesHeader, payload: Vec<u8>, random_access: bool) -> Self {
        let pts = header.pts.map(|t| t.as_u64() as i64).unwrap_or(-1);
        let dts = header.dts.map(|t| t.as_u64() as i64).unwrap_or(-1);
        Self {
            kind: 2, pid: pid.as_u16(), pts, dts, stream_type: 0,
            data: payload, text: String::new(),
            program_num: 0, random_access,
        }
    }
    fn random_access(pid: Pid) -> Self {
        Self {
            kind: 3, pid: pid.as_u16(), pts: -1, dts: -1, stream_type: 0,
            data: Vec::new(), text: String::new(),
            program_num: 0, random_access: true,
        }
    }
    fn error(s: impl Into<String>) -> Self {
        Self {
            kind: 4, pid: 0, pts: -1, dts: -1, stream_type: 0,
            data: Vec::new(), text: s.into(),
            program_num: 0, random_access: false,
        }
    }
}

#[derive(Default)]
struct PartialPes {
    header: Option<PesHeader>,
    declared_len: Option<usize>,
    buf: Vec<u8>,
}

/// Browser-facing demuxer.
#[wasm_bindgen]
pub struct TsDemuxer {
    feed: SharedFeedBuf,
    reader: TsPacketReader<SharedFeedBuf>,
    partials: HashMap<Pid, PartialPes>,
}

#[wasm_bindgen]
impl TsDemuxer {
    #[wasm_bindgen(constructor)]
    pub fn new() -> TsDemuxer {
        let feed = SharedFeedBuf::new();
        let reader = TsPacketReader::new(feed.clone());
        TsDemuxer {
            feed,
            reader,
            partials: HashMap::new(),
        }
    }

    /// Feed raw TS bytes (any length). Returns events emitted during parsing.
    pub fn feed(&mut self, bytes: &[u8]) -> Vec<TsEvent> {
        self.feed.append(bytes);
        let mut events = Vec::new();
        while self.feed.has_packet() {
            match self.reader.read_ts_packet() {
                Ok(Some(packet)) => self.handle_packet(&packet, &mut events),
                Ok(None) => break,
                Err(e) => events.push(TsEvent::error(format!("ts parse: {e:?}"))),
            }
        }
        events
    }
}

impl TsDemuxer {
    fn handle_packet(&mut self, packet: &TsPacket, events: &mut Vec<TsEvent>) {
        let pid = packet.header.pid;

        if let Some(af) = &packet.adaptation_field {
            if af.random_access_indicator {
                events.push(TsEvent::random_access(pid));
            }
        }

        let payload = match &packet.payload {
            Some(p) => p,
            None => return,
        };

        match payload {
            TsPayload::Pat(pat) => {
                for pa in &pat.table {
                    events.push(TsEvent::pat(pa.program_num, pa.program_map_pid.as_u16()));
                }
            }
            TsPayload::Pmt(pmt) => {
                let entries: Vec<(Pid, StreamType)> = pmt
                    .es_info
                    .iter()
                    .map(|e| (e.elementary_pid, e.stream_type.clone()))
                    .collect();
                events.push(TsEvent::pmt(&entries));
            }
            TsPayload::PesStart(pes) => {
                let entry = self.partials.entry(pid).or_default();
                if entry.header.is_some() && !entry.buf.is_empty() {
                    let hdr = entry.header.take().unwrap();
                    let payload = std::mem::take(&mut entry.buf);
                    events.push(TsEvent::pes(pid, &hdr, payload, false));
                }
                entry.declared_len = if pes.pes_packet_len > 0 {
                    Some(pes.pes_packet_len as usize)
                } else {
                    None
                };
                entry.header = Some(pes.header.clone());
                entry.buf.extend_from_slice(&pes.data);
                self.maybe_flush(pid, events);
            }
            TsPayload::PesContinuation(bytes) => {
                if let Some(p) = self.partials.get_mut(&pid) {
                    p.buf.extend_from_slice(bytes);
                    self.maybe_flush(pid, events);
                }
            }
            TsPayload::Null(_) | TsPayload::Raw(_) | TsPayload::Section(_) => {
                // ignore
            }
        }
    }

    fn maybe_flush(&mut self, pid: Pid, events: &mut Vec<TsEvent>) {
        let flush = match self.partials.get(&pid) {
            Some(p) => match p.declared_len {
                Some(n) => p.buf.len() >= n,
                None => false,
            },
            None => false,
        };
        if flush {
            let p = self.partials.get_mut(&pid).unwrap();
            let hdr = p.header.take().unwrap_or(PesHeader {
                stream_id: mpeg2ts::es::StreamId::new(0xBC),
                priority: false,
                data_alignment_indicator: false,
                copyright: false,
                original_or_copy: false,
                pts: None,
                dts: None,
                escr: None,
            });
            let payload = std::mem::take(&mut p.buf);
            p.declared_len = None;
            events.push(TsEvent::pes(pid, &hdr, payload, false));
        }
    }
}

impl Default for TsDemuxer {
    fn default() -> Self {
        Self::new()
    }
}
