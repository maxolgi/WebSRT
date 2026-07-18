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
use std::collections::{HashMap, VecDeque};
use std::io::{self, Read};
use std::rc::Rc;
use wasm_bindgen::prelude::*;

mod nal;

const TS_PACKET_SIZE: usize = 188;

const PACKET_RING_CAP: usize = 500;
const RECENT_ERRORS_CAP: usize = 20;
const BYTES_WINDOW_BUCKETS: usize = 100; // 100 × ~100ms = 10s rolling window
const BYTES_WINDOW_BUCKET_MS: f64 = 100.0;
const PTS_JUMP_90K: i64 = 90_000; // >1s delta in 90kHz units flags a jump

// PacketEntry.kind values mirrored into DebugSnapshot.ring_kind.
const KIND_PAT: u8 = 0;
const KIND_PMT: u8 = 1;
const KIND_PES: u8 = 2;
const KIND_RA: u8 = 3;
const KIND_ERROR: u8 = 4;
const KIND_OTHER: u8 = 255;

// One PMT entry snapshot (pid + stream_type + descriptor format id).
#[derive(Default, Clone)]
struct PmtEntry {
    pid: u16,
    stream_type: u8,
    format_id: String,
}

// Per-PID rolling statistics. `bytes_window` is a 100-bucket ring where each
// bucket holds the byte total for a ~100ms slice — the snapshot sums them to
// recover an approximate 10s rolling bitrate.
#[derive(Clone)]
struct PidStats {
    pes_count: u64,
    bytes_total: u64,
    bytes_window: VecDeque<u64>,
    bytes_window_current: u64,
    bytes_window_start: Option<web_time::Instant>,
    ra_count: u64,
    last_ra_ms: f64,
    last_pts: i64, // -1 = none seen yet
    last_dts: i64,
    max_pts_delta: i64,
    pts_jumps: u32,
    cc_errors: u32,
    tei_count: u32,
    pusi_count: u32,
    scrambling_counts: [u32; 4], // indexed by transport_scrambling_control discriminant
    af_control_counts: [u32; 4], // indexed by derived adaptation_field_control
}

impl Default for PidStats {
    fn default() -> Self {
        PidStats {
            pes_count: 0,
            bytes_total: 0,
            bytes_window: VecDeque::new(),
            bytes_window_current: 0,
            bytes_window_start: None,
            ra_count: 0,
            last_ra_ms: 0.0,
            last_pts: -1,
            last_dts: -1,
            max_pts_delta: 0,
            pts_jumps: 0,
            cc_errors: 0,
            tei_count: 0,
            pusi_count: 0,
            scrambling_counts: [0; 4],
            af_control_counts: [0; 4],
        }
    }
}

#[derive(Default, Clone)]
struct PcrStats {
    last_27mhz: u64, // 0 = no PCR seen yet on this PID
    interval_ms: f64,
    jitter_ms_ema: f64,
}

// One row in the rolling packet timeline (Commit 3 renders this). The ring is
// capped at PACKET_RING_CAP entries (~30s of PES at typical OBS rates).
#[derive(Clone)]
struct PacketEntry {
    t_ms: f64,
    pid: u16,
    kind: u8,
    pts: i64,
    dts: i64,
    size: u32,
    ra: bool,
    nal_summary: Vec<u8>,
    cc_error: bool,
    tei: bool,
    pusi: bool,
}

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

    // PSI snapshot.
    program_num: Option<u16>,
    pmt_pid: Option<u16>,
    pmt_entries: Vec<PmtEntry>,

    // Per-PID analysis.
    pid_stats: HashMap<u16, PidStats>,
    pcr_stats: HashMap<u16, PcrStats>,
    nal_stats: HashMap<u16, nal::NalStats>,
    last_cc: HashMap<u16, u8>,

    // Rings for the snapshot.
    recent_errors: VecDeque<(f64, String)>,
    packet_ring: VecDeque<PacketEntry>,

    // Clock anchor for t_ms in rings (ms since TsDemux construction).
    start_time: web_time::Instant,
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
            program_num: None,
            pmt_pid: None,
            pmt_entries: Vec::new(),
            pid_stats: HashMap::new(),
            pcr_stats: HashMap::new(),
            nal_stats: HashMap::new(),
            last_cc: HashMap::new(),
            recent_errors: VecDeque::new(),
            packet_ring: VecDeque::new(),
            start_time: web_time::Instant::now(),
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
                Err(e) => {
                    let msg = format!("ts parse: {e:?}");
                    let t_ms = self.elapsed_ms();
                    self.push_error(t_ms, msg.clone());
                    events.push(TsEvent::error(msg));
                }
            }
        }
        events
    }
}

impl TsDemuxer {
    fn elapsed_ms(&self) -> f64 {
        self.start_time.elapsed().as_secs_f64() * 1000.0
    }

    fn push_packet(&mut self, entry: PacketEntry) {
        self.packet_ring.push_back(entry);
        while self.packet_ring.len() > PACKET_RING_CAP {
            self.packet_ring.pop_front();
        }
    }

    fn push_error(&mut self, t_ms: f64, msg: String) {
        self.recent_errors.push_back((t_ms, msg));
        while self.recent_errors.len() > RECENT_ERRORS_CAP {
            self.recent_errors.pop_front();
        }
        self.push_packet(PacketEntry {
            t_ms,
            pid: 0,
            kind: KIND_ERROR,
            pts: -1,
            dts: -1,
            size: 0,
            ra: false,
            nal_summary: Vec::new(),
            cc_error: false,
            tei: false,
            pusi: false,
        });
    }

    fn stream_type_for_pid(&self, pid: u16) -> Option<u8> {
        self.pmt_entries
            .iter()
            .find(|e| e.pid == pid)
            .map(|e| e.stream_type)
    }

    // Roll the per-PID bitrate ring forward so it covers [now-10s, now], then
    // accumulate `n` bytes into the current (partial) bucket.
    fn accumulate_bytes(&mut self, pid: u16, n: u64, now: web_time::Instant) {
        let stats = self.pid_stats.entry(pid).or_default();
        stats.bytes_total = stats.bytes_total.saturating_add(n);
        let start = stats.bytes_window_start.get_or_insert(now);
        let elapsed_ms = now.duration_since(*start).as_secs_f64() * 1000.0;
        if elapsed_ms >= BYTES_WINDOW_BUCKET_MS {
            // Cap rotations to avoid a pathological spin if the stream stalled.
            let buckets_to_rotate =
                ((elapsed_ms / BYTES_WINDOW_BUCKET_MS) as usize).min(2 * BYTES_WINDOW_BUCKETS);
            for _ in 0..buckets_to_rotate {
                stats.bytes_window.push_back(stats.bytes_window_current);
                stats.bytes_window_current = 0;
                while stats.bytes_window.len() > BYTES_WINDOW_BUCKETS {
                    stats.bytes_window.pop_front();
                }
            }
            *start = now;
        }
        stats.bytes_window_current = stats.bytes_window_current.saturating_add(n);
    }

    // Update pid_stats + nal_stats + packet_ring for a fully-reassembled PES.
    // The existing TsEvent::pes is still emitted (unchanged flow).
    fn finalize_pes(
        &mut self,
        pid: Pid,
        hdr: &PesHeader,
        payload: &[u8],
        ra: bool,
        t_ms: f64,
        tei: bool,
        pusi: bool,
        events: &mut Vec<TsEvent>,
    ) {
        let pid_u16 = pid.as_u16();
        events.push(TsEvent::pes(pid, hdr, payload.to_vec(), ra));

        let pts = hdr.pts.map(|t| t.as_u64() as i64).unwrap_or(-1);
        let dts = hdr.dts.map(|t| t.as_u64() as i64).unwrap_or(-1);
        {
            let stats = self.pid_stats.entry(pid_u16).or_default();
            stats.pes_count = stats.pes_count.saturating_add(1);
            if pts >= 0 {
                if stats.last_pts >= 0 {
                    let delta = (pts - stats.last_pts).abs();
                    if delta > stats.max_pts_delta {
                        stats.max_pts_delta = delta;
                    }
                    if delta > PTS_JUMP_90K {
                        stats.pts_jumps += 1;
                    }
                }
                stats.last_pts = pts;
            }
            if dts >= 0 {
                stats.last_dts = dts;
            }
        }

        // NAL classification for video PIDs only (H.264 0x1b / HEVC 0x24).
        // AV1 (OBU syntax) and audio PIDs carry no Annex-B NAL units.
        let mut nal_summary: Vec<u8> = Vec::new();
        if let Some(st) = self.stream_type_for_pid(pid_u16) {
            let is_hevc = st == 0x24;
            let is_h264 = st == 0x1B;
            if is_hevc || is_h264 {
                let nstats = nal::parse_nal_stats(payload, is_hevc);
                nal_summary = nal::nal_summary(payload, is_hevc);
                let entry = self.nal_stats.entry(pid_u16).or_default();
                entry.aud = entry.aud.saturating_add(nstats.aud);
                entry.sps = entry.sps.saturating_add(nstats.sps);
                entry.pps = entry.pps.saturating_add(nstats.pps);
                entry.sei = entry.sei.saturating_add(nstats.sei);
                entry.idr = entry.idr.saturating_add(nstats.idr);
                entry.non_idr_slice = entry
                    .non_idr_slice
                    .saturating_add(nstats.non_idr_slice);
                entry.i_slices = entry.i_slices.saturating_add(nstats.i_slices);
                entry.p_slices = entry.p_slices.saturating_add(nstats.p_slices);
                entry.b_slices = entry.b_slices.saturating_add(nstats.b_slices);
            }
        }

        self.push_packet(PacketEntry {
            t_ms,
            pid: pid_u16,
            kind: KIND_PES,
            pts,
            dts,
            size: payload.len() as u32,
            ra,
            nal_summary,
            cc_error: false,
            tei,
            pusi,
        });
    }

    fn handle_packet(&mut self, packet: &TsPacket, events: &mut Vec<TsEvent>) {
        let now = web_time::Instant::now();
        let elapsed_ms = self.start_time.elapsed().as_secs_f64() * 1000.0;
        let pid = packet.header.pid;
        let pid_u16 = pid.as_u16();
        let tei = packet.header.transport_error_indicator;
        let cc = packet.header.continuity_counter.as_u8();
        // TransportScramblingControl discriminant: 0=NotScrambled, 2=Even, 3=Odd.
        let scrambling = packet.header.transport_scrambling_control.clone() as u8;
        let pusi = matches!(
            packet.payload,
            Some(TsPayload::PesStart(_))
                | Some(TsPayload::Pat(_))
                | Some(TsPayload::Pmt(_))
                | Some(TsPayload::Section(_))
        );
        let has_af = packet.adaptation_field.is_some();
        let has_payload = packet.payload.is_some();
        let af_control: u8 = match (has_af, has_payload) {
            (true, true) => 0b11,
            (true, false) => 0b10,
            (false, true) => 0b01,
            (false, false) => 0b00,
        };
        let discontinuity = packet
            .adaptation_field
            .as_ref()
            .map(|af| af.discontinuity_indicator)
            .unwrap_or(false);

        // TS header flag counters apply to EVERY packet (incl. adaptation-only),
        // so they live above the payload early-return.
        {
            let stats = self.pid_stats.entry(pid_u16).or_default();
            if tei {
                stats.tei_count += 1;
            }
            if pusi {
                stats.pusi_count += 1;
            }
            if (scrambling as usize) < stats.scrambling_counts.len() {
                stats.scrambling_counts[scrambling as usize] += 1;
            }
            if (af_control as usize) < stats.af_control_counts.len() {
                stats.af_control_counts[af_control as usize] += 1;
            }
        }

        // PCR + random_access come from the adaptation field.
        if let Some(af) = &packet.adaptation_field {
            if let Some(pcr) = &af.pcr {
                let pcr_27mhz = pcr.as_u64();
                let pcr_stats = self.pcr_stats.entry(pid_u16).or_default();
                if pcr_stats.last_27mhz != 0 {
                    let delta_27mhz = pcr_27mhz.wrapping_sub(pcr_stats.last_27mhz);
                    let interval_ms = delta_27mhz as f64 / 27_000.0;
                    if pcr_stats.interval_ms > 0.0 {
                        let jitter = (interval_ms - pcr_stats.interval_ms).abs();
                        pcr_stats.jitter_ms_ema = pcr_stats.jitter_ms_ema * 0.9 + jitter * 0.1;
                    }
                    pcr_stats.interval_ms = interval_ms;
                }
                pcr_stats.last_27mhz = pcr_27mhz;
            }
            if af.random_access_indicator {
                // The RA indicator belongs to the PES starting in THIS packet
                // (the next PesStart for this PID). Stash it as a pending flag;
                // PesStart consumes it into the new partial's `ra` field so it
                // isn't mistakenly attached to the preceding delta PES flushed
                // at the same PesStart.
                self.last_ra.insert(pid, true);
                {
                    let stats = self.pid_stats.entry(pid_u16).or_default();
                    stats.ra_count += 1;
                    stats.last_ra_ms = elapsed_ms;
                }
                events.push(TsEvent::random_access(pid));
                self.push_packet(PacketEntry {
                    t_ms: elapsed_ms,
                    pid: pid_u16,
                    kind: KIND_RA,
                    pts: -1,
                    dts: -1,
                    size: 0,
                    ra: true,
                    nal_summary: Vec::new(),
                    cc_error: false,
                    tei,
                    pusi,
                });
            }
        }

        let payload = match &packet.payload {
            Some(p) => p,
            None => return,
        };

        // CC continuity check (ISO/IEC 13818-1 §2.4.3.3): only on payload-bearing
        // packets. A set discontinuity_indicator legitimately resets the counter.
        if !discontinuity {
            let expected_cc_opt = self.last_cc.get(&pid_u16).copied().map(|p| (p + 1) & 0x0F);
            if let Some(expected) = expected_cc_opt {
                if cc != expected {
                    let stats = self.pid_stats.entry(pid_u16).or_default();
                    stats.cc_errors += 1;
                }
            }
        }
        self.last_cc.insert(pid_u16, cc);

        match payload {
            TsPayload::Pat(pat) => {
                for pa in &pat.table {
                    self.program_num = Some(pa.program_num);
                    self.pmt_pid = Some(pa.program_map_pid.as_u16());
                    events.push(TsEvent::pat(pa.program_num, pa.program_map_pid.as_u16()));
                    self.push_packet(PacketEntry {
                        t_ms: elapsed_ms,
                        pid: pid_u16,
                        kind: KIND_PAT,
                        pts: -1,
                        dts: -1,
                        size: 0,
                        ra: false,
                        nal_summary: Vec::new(),
                        cc_error: false,
                        tei,
                        pusi,
                    });
                }
            }
            TsPayload::Pmt(pmt) => {
                // Refresh the snapshot of elementary streams. Registration
                // descriptor (tag 0x05) format id disambiguates 0x06 streams
                // (AV01 → video, Opus → audio); empty when absent.
                let mut new_entries: Vec<PmtEntry> = Vec::with_capacity(pmt.es_info.len());
                for e in &pmt.es_info {
                    let format_id = e
                        .descriptors
                        .iter()
                        .find_map(|d| {
                            if d.tag == 0x05 && d.data.len() >= 4 {
                                std::str::from_utf8(&d.data[..4]).ok().map(String::from)
                            } else {
                                None
                            }
                        })
                        .unwrap_or_default();
                    new_entries.push(PmtEntry {
                        pid: e.elementary_pid.as_u16(),
                        stream_type: e.stream_type.clone() as u8,
                        format_id,
                    });
                }
                self.pmt_entries = new_entries;

                let entries: Vec<(Pid, StreamType)> = pmt
                    .es_info
                    .iter()
                    .map(|e| (e.elementary_pid, e.stream_type.clone()))
                    .collect();
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
                self.push_packet(PacketEntry {
                    t_ms: elapsed_ms,
                    pid: pid_u16,
                    kind: KIND_PMT,
                    pts: -1,
                    dts: -1,
                    size: 0,
                    ra: false,
                    nal_summary: Vec::new(),
                    cc_error: false,
                    tei,
                    pusi,
                });
            }
            TsPayload::PesStart(pes) => {
                let pes_payload_len = pes.data.len();
                let entry = self.partials.entry(pid).or_default();
                let flushed = if entry.header.is_some() && !entry.buf.is_empty() {
                    let hdr = entry.header.take().unwrap();
                    let payload = std::mem::take(&mut entry.buf);
                    let ra = std::mem::take(&mut entry.ra);
                    Some((hdr, payload, ra))
                } else {
                    None
                };
                entry.declared_len = if pes.pes_packet_len > 0 {
                    Some(pes.pes_packet_len as usize)
                } else {
                    None
                };
                entry.header = Some(pes.header.clone());
                entry.ra = self.last_ra.get(&pid).copied().unwrap_or(false);
                self.last_ra.insert(pid, false);
                entry.buf.extend_from_slice(&pes.data);
                // Bytes for the NEW partial count toward this PID's bitrate
                // immediately (not deferred to flush) so the ring tracks live
                // arrival rate.
                self.accumulate_bytes(pid_u16, pes_payload_len as u64, now);

                if let Some((hdr, payload, ra)) = flushed {
                    self.finalize_pes(pid, &hdr, &payload, ra, elapsed_ms, tei, pusi, events);
                }
                self.maybe_flush(pid, elapsed_ms, tei, pusi, now, events);
            }
            TsPayload::PesContinuation(bytes) => {
                let n = bytes.len() as u64;
                if let Some(p) = self.partials.get_mut(&pid) {
                    p.buf.extend_from_slice(bytes);
                }
                self.accumulate_bytes(pid_u16, n, now);
                self.maybe_flush(pid, elapsed_ms, tei, pusi, now, events);
            }
            TsPayload::Null(_) | TsPayload::Raw(_) | TsPayload::Section(_) => {
                // ignore payload, but still record a timeline row for visibility.
                self.push_packet(PacketEntry {
                    t_ms: elapsed_ms,
                    pid: pid_u16,
                    kind: KIND_OTHER,
                    pts: -1,
                    dts: -1,
                    size: 0,
                    ra: false,
                    nal_summary: Vec::new(),
                    cc_error: false,
                    tei,
                    pusi,
                });
            }
        }
    }

    fn maybe_flush(
        &mut self,
        pid: Pid,
        elapsed_ms: f64,
        tei: bool,
        pusi: bool,
        now: web_time::Instant,
        events: &mut Vec<TsEvent>,
    ) {
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
            // Bytes already counted incrementally as they arrived; the bitrate
            // ring uses `now` only to gate bucket rotation, which finalize_pes
            // doesn't need, so we pass `elapsed_ms` for the timeline stamp.
            let _ = now;
            self.finalize_pes(pid, &hdr, &payload, ra, elapsed_ms, tei, pusi, events);
        }
    }
}

impl Default for TsDemuxer {
    fn default() -> Self {
        Self::new()
    }
}

/// Flat struct-of-arrays snapshot of the demuxer's analysis state. wasm-bindgen
/// serializes parallel `Vec`s far more cheaply than nested objects, so every
/// per-PID table is laid out as `[field0_pid0, field0_pid1, …]` etc. The JS
/// consumer zips them by index.
///
/// Field layout conventions:
///   - `pids` is the key array for `pid_stats`; all per-PID scalar vectors
///     (`pesCounts`, `byteTotals`, `bitratesMbps`, …) are parallel to it.
///   - `scramblingCounts` / `afControlCounts` are flat 4×N (4 values per PID).
///   - `nalStats` is flat 9×M (9 values per video PID: I/P/B/IDR/SPS/PPS/SEI/AUD/NonIDR).
///   - `ringNalOffsets` has N+1 entries; packet i's NAL types are
///     `ringNal[offsets[i] .. offsets[i+1]]`.
#[wasm_bindgen]
pub struct DebugSnapshot {
    program_num: i32,
    pmt_pid: i32,
    pmt_pids: Vec<u16>,
    pmt_stream_types: Vec<u8>,
    pmt_format_ids: Vec<String>,

    pids: Vec<u16>,
    pes_counts: Vec<f64>,
    byte_totals: Vec<f64>,
    bitrates_mbps: Vec<f64>,
    ra_counts: Vec<f64>,
    last_pts: Vec<f64>,
    last_dts: Vec<f64>,
    pts_jumps: Vec<f64>,
    cc_errors: Vec<f64>,
    tei_counts: Vec<f64>,
    pusi_counts: Vec<f64>,
    scrambling_counts: Vec<f64>,
    af_control_counts: Vec<f64>,

    pcr_pids: Vec<u16>,
    pcr_intervals_ms: Vec<f64>,
    pcr_jitter_ms: Vec<f64>,

    nal_pids: Vec<u16>,
    nal_stats: Vec<f64>,

    error_t: Vec<f64>,
    error_msg: Vec<String>,

    ring_t: Vec<f64>,
    ring_pid: Vec<u16>,
    ring_kind: Vec<u8>,
    ring_pts: Vec<f64>,
    ring_dts: Vec<f64>,
    ring_size: Vec<f64>,
    ring_ra: Vec<u8>,
    ring_cc_err: Vec<u8>,
    ring_tei: Vec<u8>,
    ring_pusi: Vec<u8>,
    ring_nal: Vec<u8>,
    ring_nal_offsets: Vec<u32>,
}

#[wasm_bindgen]
impl DebugSnapshot {
    #[wasm_bindgen(getter, js_name = programNum)]
    pub fn program_num(&self) -> i32 {
        self.program_num
    }
    #[wasm_bindgen(getter, js_name = pmtPid)]
    pub fn pmt_pid(&self) -> i32 {
        self.pmt_pid
    }
    #[wasm_bindgen(getter, js_name = pmtPids)]
    pub fn pmt_pids(&self) -> Vec<u16> {
        self.pmt_pids.clone()
    }
    #[wasm_bindgen(getter, js_name = pmtStreamTypes)]
    pub fn pmt_stream_types(&self) -> Vec<u8> {
        self.pmt_stream_types.clone()
    }
    #[wasm_bindgen(getter, js_name = pmtFormatIds)]
    pub fn pmt_format_ids(&self) -> Vec<String> {
        self.pmt_format_ids.clone()
    }

    #[wasm_bindgen(getter)]
    pub fn pids(&self) -> Vec<u16> {
        self.pids.clone()
    }
    #[wasm_bindgen(getter, js_name = pesCounts)]
    pub fn pes_counts(&self) -> Vec<f64> {
        self.pes_counts.clone()
    }
    #[wasm_bindgen(getter, js_name = byteTotals)]
    pub fn byte_totals(&self) -> Vec<f64> {
        self.byte_totals.clone()
    }
    #[wasm_bindgen(getter, js_name = bitratesMbps)]
    pub fn bitrates_mbps(&self) -> Vec<f64> {
        self.bitrates_mbps.clone()
    }
    #[wasm_bindgen(getter, js_name = raCounts)]
    pub fn ra_counts(&self) -> Vec<f64> {
        self.ra_counts.clone()
    }
    #[wasm_bindgen(getter, js_name = lastPts)]
    pub fn last_pts(&self) -> Vec<f64> {
        self.last_pts.clone()
    }
    #[wasm_bindgen(getter, js_name = lastDts)]
    pub fn last_dts(&self) -> Vec<f64> {
        self.last_dts.clone()
    }
    #[wasm_bindgen(getter, js_name = ptsJumps)]
    pub fn pts_jumps(&self) -> Vec<f64> {
        self.pts_jumps.clone()
    }
    #[wasm_bindgen(getter, js_name = ccErrors)]
    pub fn cc_errors(&self) -> Vec<f64> {
        self.cc_errors.clone()
    }
    #[wasm_bindgen(getter, js_name = teiCounts)]
    pub fn tei_counts(&self) -> Vec<f64> {
        self.tei_counts.clone()
    }
    #[wasm_bindgen(getter, js_name = pusiCounts)]
    pub fn pusi_counts(&self) -> Vec<f64> {
        self.pusi_counts.clone()
    }
    #[wasm_bindgen(getter, js_name = scramblingCounts)]
    pub fn scrambling_counts(&self) -> Vec<f64> {
        self.scrambling_counts.clone()
    }
    #[wasm_bindgen(getter, js_name = afControlCounts)]
    pub fn af_control_counts(&self) -> Vec<f64> {
        self.af_control_counts.clone()
    }

    #[wasm_bindgen(getter, js_name = pcrPids)]
    pub fn pcr_pids(&self) -> Vec<u16> {
        self.pcr_pids.clone()
    }
    #[wasm_bindgen(getter, js_name = pcrIntervalsMs)]
    pub fn pcr_intervals_ms(&self) -> Vec<f64> {
        self.pcr_intervals_ms.clone()
    }
    #[wasm_bindgen(getter, js_name = pcrJitterMs)]
    pub fn pcr_jitter_ms(&self) -> Vec<f64> {
        self.pcr_jitter_ms.clone()
    }

    #[wasm_bindgen(getter, js_name = nalPids)]
    pub fn nal_pids(&self) -> Vec<u16> {
        self.nal_pids.clone()
    }
    #[wasm_bindgen(getter, js_name = nalStats)]
    pub fn nal_stats(&self) -> Vec<f64> {
        self.nal_stats.clone()
    }

    #[wasm_bindgen(getter, js_name = errorT)]
    pub fn error_t(&self) -> Vec<f64> {
        self.error_t.clone()
    }
    #[wasm_bindgen(getter, js_name = errorMsg)]
    pub fn error_msg(&self) -> Vec<String> {
        self.error_msg.clone()
    }

    #[wasm_bindgen(getter, js_name = ringT)]
    pub fn ring_t(&self) -> Vec<f64> {
        self.ring_t.clone()
    }
    #[wasm_bindgen(getter, js_name = ringPid)]
    pub fn ring_pid(&self) -> Vec<u16> {
        self.ring_pid.clone()
    }
    #[wasm_bindgen(getter, js_name = ringKind)]
    pub fn ring_kind(&self) -> Vec<u8> {
        self.ring_kind.clone()
    }
    #[wasm_bindgen(getter, js_name = ringPts)]
    pub fn ring_pts(&self) -> Vec<f64> {
        self.ring_pts.clone()
    }
    #[wasm_bindgen(getter, js_name = ringDts)]
    pub fn ring_dts(&self) -> Vec<f64> {
        self.ring_dts.clone()
    }
    #[wasm_bindgen(getter, js_name = ringSize)]
    pub fn ring_size(&self) -> Vec<f64> {
        self.ring_size.clone()
    }
    #[wasm_bindgen(getter, js_name = ringRa)]
    pub fn ring_ra(&self) -> Vec<u8> {
        self.ring_ra.clone()
    }
    #[wasm_bindgen(getter, js_name = ringCcErr)]
    pub fn ring_cc_err(&self) -> Vec<u8> {
        self.ring_cc_err.clone()
    }
    #[wasm_bindgen(getter, js_name = ringTei)]
    pub fn ring_tei(&self) -> Vec<u8> {
        self.ring_tei.clone()
    }
    #[wasm_bindgen(getter, js_name = ringPusi)]
    pub fn ring_pusi(&self) -> Vec<u8> {
        self.ring_pusi.clone()
    }
    #[wasm_bindgen(getter, js_name = ringNal)]
    pub fn ring_nal(&self) -> Vec<u8> {
        self.ring_nal.clone()
    }
    #[wasm_bindgen(getter, js_name = ringNalOffsets)]
    pub fn ring_nal_offsets(&self) -> Vec<u32> {
        self.ring_nal_offsets.clone()
    }
}

impl TsDemuxer {
    fn bitrate_mbps(s: &PidStats) -> f64 {
        let window_sum: u64 = s.bytes_window.iter().sum();
        let total = window_sum + s.bytes_window_current;
        let buckets = (s.bytes_window.len() + 1).max(1) as f64;
        // Each bucket represents BYTES_WINDOW_BUCKET_MS of stream time.
        let window_secs = buckets * (BYTES_WINDOW_BUCKET_MS / 1000.0);
        if window_secs <= 0.0 {
            0.0
        } else {
            (total as f64 * 8.0) / window_secs / 1_000_000.0
        }
    }
}

#[wasm_bindgen]
impl TsDemuxer {
    /// Snapshot the full analysis state for the debug panel. Owned by JS —
    /// cheap to call every ~250ms. Iteration order is by ascending PID so the
    /// flat arrays are stable across calls for the same stream.
    #[wasm_bindgen(js_name = debugSnapshot)]
    pub fn debug_snapshot(&self) -> DebugSnapshot {
        let mut pids: Vec<u16> = self.pid_stats.keys().copied().collect();
        pids.sort_unstable();

        let mut pes_counts = Vec::with_capacity(pids.len());
        let mut byte_totals = Vec::with_capacity(pids.len());
        let mut bitrates_mbps = Vec::with_capacity(pids.len());
        let mut ra_counts = Vec::with_capacity(pids.len());
        let mut last_pts = Vec::with_capacity(pids.len());
        let mut last_dts = Vec::with_capacity(pids.len());
        let mut pts_jumps = Vec::with_capacity(pids.len());
        let mut cc_errors = Vec::with_capacity(pids.len());
        let mut tei_counts = Vec::with_capacity(pids.len());
        let mut pusi_counts = Vec::with_capacity(pids.len());
        let mut scrambling_counts = Vec::with_capacity(pids.len() * 4);
        let mut af_control_counts = Vec::with_capacity(pids.len() * 4);

        for &pid in &pids {
            let s = self
                .pid_stats
                .get(&pid)
                .expect("pid present from keys()");
            pes_counts.push(s.pes_count as f64);
            byte_totals.push(s.bytes_total as f64);
            bitrates_mbps.push(Self::bitrate_mbps(s));
            ra_counts.push(s.ra_count as f64);
            last_pts.push(if s.last_pts < 0 { -1.0 } else { s.last_pts as f64 });
            last_dts.push(if s.last_dts < 0 { -1.0 } else { s.last_dts as f64 });
            pts_jumps.push(s.pts_jumps as f64);
            cc_errors.push(s.cc_errors as f64);
            tei_counts.push(s.tei_count as f64);
            pusi_counts.push(s.pusi_count as f64);
            for i in 0..4 {
                scrambling_counts.push(s.scrambling_counts[i] as f64);
                af_control_counts.push(s.af_control_counts[i] as f64);
            }
        }

        let mut pcr_pids: Vec<u16> = self.pcr_stats.keys().copied().collect();
        pcr_pids.sort_unstable();
        let mut pcr_intervals_ms = Vec::with_capacity(pcr_pids.len());
        let mut pcr_jitter_ms = Vec::with_capacity(pcr_pids.len());
        for &pid in &pcr_pids {
            let p = self.pcr_stats.get(&pid).expect("pcr pid from keys()");
            pcr_intervals_ms.push(p.interval_ms);
            pcr_jitter_ms.push(p.jitter_ms_ema);
        }

        let mut nal_pids: Vec<u16> = self.nal_stats.keys().copied().collect();
        nal_pids.sort_unstable();
        let mut nal_stats_flat = Vec::with_capacity(nal_pids.len() * 9);
        for &pid in &nal_pids {
            let n = self.nal_stats.get(&pid).expect("nal pid from keys()");
            // Order: I, P, B, IDR, SPS, PPS, SEI, AUD, NonIDR.
            nal_stats_flat.push(n.i_slices as f64);
            nal_stats_flat.push(n.p_slices as f64);
            nal_stats_flat.push(n.b_slices as f64);
            nal_stats_flat.push(n.idr as f64);
            nal_stats_flat.push(n.sps as f64);
            nal_stats_flat.push(n.pps as f64);
            nal_stats_flat.push(n.sei as f64);
            nal_stats_flat.push(n.aud as f64);
            nal_stats_flat.push(n.non_idr_slice as f64);
        }

        let mut pmt_pids = Vec::with_capacity(self.pmt_entries.len());
        let mut pmt_stream_types = Vec::with_capacity(self.pmt_entries.len());
        let mut pmt_format_ids = Vec::with_capacity(self.pmt_entries.len());
        for e in &self.pmt_entries {
            pmt_pids.push(e.pid);
            pmt_stream_types.push(e.stream_type);
            pmt_format_ids.push(e.format_id.clone());
        }

        let mut error_t = Vec::with_capacity(self.recent_errors.len());
        let mut error_msg = Vec::with_capacity(self.recent_errors.len());
        for (t, msg) in &self.recent_errors {
            error_t.push(*t);
            error_msg.push(msg.clone());
        }

        let ring_len = self.packet_ring.len();
        let mut ring_t = Vec::with_capacity(ring_len);
        let mut ring_pid = Vec::with_capacity(ring_len);
        let mut ring_kind = Vec::with_capacity(ring_len);
        let mut ring_pts = Vec::with_capacity(ring_len);
        let mut ring_dts = Vec::with_capacity(ring_len);
        let mut ring_size = Vec::with_capacity(ring_len);
        let mut ring_ra = Vec::with_capacity(ring_len);
        let mut ring_cc_err = Vec::with_capacity(ring_len);
        let mut ring_tei = Vec::with_capacity(ring_len);
        let mut ring_pusi = Vec::with_capacity(ring_len);
        let mut ring_nal = Vec::new();
        let mut ring_nal_offsets = Vec::with_capacity(ring_len + 1);
        ring_nal_offsets.push(0);
        for e in &self.packet_ring {
            ring_t.push(e.t_ms);
            ring_pid.push(e.pid);
            ring_kind.push(e.kind);
            ring_pts.push(if e.pts < 0 { -1.0 } else { e.pts as f64 });
            ring_dts.push(if e.dts < 0 { -1.0 } else { e.dts as f64 });
            ring_size.push(e.size as f64);
            ring_ra.push(e.ra as u8);
            ring_cc_err.push(e.cc_error as u8);
            ring_tei.push(e.tei as u8);
            ring_pusi.push(e.pusi as u8);
            ring_nal.extend_from_slice(&e.nal_summary);
            ring_nal_offsets.push(ring_nal.len() as u32);
        }

        DebugSnapshot {
            program_num: self.program_num.map(|n| n as i32).unwrap_or(-1),
            pmt_pid: self.pmt_pid.map(|n| n as i32).unwrap_or(-1),
            pmt_pids,
            pmt_stream_types,
            pmt_format_ids,
            pids,
            pes_counts,
            byte_totals,
            bitrates_mbps,
            ra_counts,
            last_pts,
            last_dts,
            pts_jumps,
            cc_errors,
            tei_counts,
            pusi_counts,
            scrambling_counts,
            af_control_counts,
            pcr_pids,
            pcr_intervals_ms,
            pcr_jitter_ms,
            nal_pids,
            nal_stats: nal_stats_flat,
            error_t,
            error_msg,
            ring_t,
            ring_pid,
            ring_kind,
            ring_pts,
            ring_dts,
            ring_size,
            ring_ra,
            ring_cc_err,
            ring_tei,
            ring_pusi,
            ring_nal,
            ring_nal_offsets,
        }
    }
}
