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
    // PMT events: per-entry registration-descriptor format identifier
    // (4-char ASCII e.g. "AV01"/"Opus"/"HEVC"), or empty string when absent.
    pmt_format_ids: Vec<String>,
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

    /// For PMT events: per-entry registration-descriptor format identifier
    /// (4-char ASCII, e.g. "AV01"/"Opus"/"HEVC"). Empty string when the entry
    /// had no registration descriptor (ffmpeg/OBS AV1 + most private streams).
    #[wasm_bindgen(js_name = pmtFormatIds)]
    pub fn pmt_format_ids(&self) -> Vec<String> {
        self.pmt_format_ids.clone()
    }
}

impl TsEvent {
    fn pat(program_num: u16, pmt_pid: u16) -> Self {
        Self {
            kind: 0, pid: pmt_pid, pts: -1, dts: -1, stream_type: 0,
            data: Vec::new(), text: String::new(),
            program_num, random_access: false, pmt_format_ids: Vec::new(),
        }
    }
    fn pmt(entries: &[(Pid, StreamType)], format_ids: &[String]) -> Self {
        let mut data = Vec::with_capacity(entries.len() * 4);
        for (pid, st) in entries {
            data.extend_from_slice(&pid.as_u16().to_le_bytes());
            data.extend_from_slice(&(st.clone() as u16).to_le_bytes());
        }
        Self {
            kind: 1, pid: 0, pts: -1, dts: -1, stream_type: 0,
            data, text: String::new(),
            program_num: 0, random_access: false,
            pmt_format_ids: format_ids.to_vec(),
        }
    }
    fn pes(pid: Pid, header: &PesHeader, payload: Vec<u8>, random_access: bool) -> Self {
        let pts = header.pts.map(|t| t.as_u64() as i64).unwrap_or(-1);
        let dts = header.dts.map(|t| t.as_u64() as i64).unwrap_or(-1);
        Self {
            kind: 2, pid: pid.as_u16(), pts, dts, stream_type: 0,
            data: payload, text: String::new(),
            program_num: 0, random_access, pmt_format_ids: Vec::new(),
        }
    }
    fn random_access(pid: Pid) -> Self {
        Self {
            kind: 3, pid: pid.as_u16(), pts: -1, dts: -1, stream_type: 0,
            data: Vec::new(), text: String::new(),
            program_num: 0, random_access: true, pmt_format_ids: Vec::new(),
        }
    }
    fn error(s: impl Into<String>) -> Self {
        Self {
            kind: 4, pid: 0, pts: -1, dts: -1, stream_type: 0,
            data: Vec::new(), text: s.into(),
            program_num: 0, random_access: false, pmt_format_ids: Vec::new(),
        }
    }
}

#[derive(Default)]
struct PartialPes {
    header: Option<PesHeader>,
    declared_len: Option<usize>,
    buf: Vec<u8>,
    // Keyframe hint: true if the adaptation_field.random_access_indicator was
    // set on the TS packet containing this PES's PesStart. Captured at start
    // time (not read at flush) so a keyframe's RA flag isn't mistakenly
    // attached to the preceding delta PES flushed at the same PesStart.
    ra: bool,
}

/// Browser-facing demuxer.
#[wasm_bindgen]
pub struct TsDemuxer {
    feed: SharedFeedBuf,
    reader: TsPacketReader<SharedFeedBuf>,
    partials: HashMap<Pid, PartialPes>,
    // Most recent adaptation_field.random_access_indicator seen per PID.
    // Consumed (reset to false) when the next PES event for that PID is
    // emitted, so the keyframe hint reaches the pipeline instead of being
    // dropped as a separate kind-3 event.
    last_ra: HashMap<Pid, bool>,
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
            last_ra: HashMap::new(),
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
                // The RA indicator belongs to the PES starting in THIS packet
                // (the next PesStart for this PID). Stash it as a pending flag;
                // PesStart consumes it into the new partial's `ra` field so it
                // isn't mistakenly attached to the preceding delta PES flushed
                // at the same PesStart.
                self.last_ra.insert(pid, true);
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
                // Surface registration-descriptor format IDs (tag 0x05,
                // 4-byte ASCII) so JS can disambiguate 0x06 streams
                // (AV01 → video, Opus → audio). Empty when absent.
                let format_ids: Vec<String> = pmt
                    .es_info
                    .iter()
                    .map(|e| {
                        for d in &e.descriptors {
                            if d.tag == 0x05 && d.data.len() >= 4 {
                                if let Ok(s) = std::str::from_utf8(&d.data[..4]) {
                                    return s.to_string();
                                }
                            }
                        }
                        String::new()
                    })
                    .collect();
                events.push(TsEvent::pmt(&entries, &format_ids));
            }
            TsPayload::PesStart(pes) => {
                let entry = self.partials.entry(pid).or_default();
                if entry.header.is_some() && !entry.buf.is_empty() {
                    let hdr = entry.header.take().unwrap();
                    let payload = std::mem::take(&mut entry.buf);
                    // The flushed partial carries the RA flag captured when IT
                    // started, not the one set by this packet.
                    let ra = std::mem::take(&mut entry.ra);
                    events.push(TsEvent::pes(pid, &hdr, payload, ra));
                }
                entry.declared_len = if pes.pes_packet_len > 0 {
                    Some(pes.pes_packet_len as usize)
                } else {
                    None
                };
                entry.header = Some(pes.header.clone());
                // Attach this packet's pending RA flag to the NEW partial, then
                // consume it so it doesn't leak onto a later PES.
                entry.ra = self.last_ra.get(&pid).copied().unwrap_or(false);
                self.last_ra.insert(pid, false);
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
            let ra = std::mem::take(&mut p.ra);
            events.push(TsEvent::pes(pid, &hdr, payload, ra));
        }
    }
}

impl Default for TsDemuxer {
    fn default() -> Self {
        Self::new()
    }
}
