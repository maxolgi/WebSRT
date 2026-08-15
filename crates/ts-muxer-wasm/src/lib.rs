//! `ts-muxer-wasm` — wasm32 MPEG-TS muxer for the browser.
//!
//! Takes H.264/HEVC/AV1 NAL units (Annex B) and audio packets and produces
//! 188-byte ISO/IEC 13818-1 MPEG-TS packets. JS drives it by pushing encoded
//! chunks via `push_video` / `push_audio` / `push_pcm` and draining finished
//! packets via `poll`.

use mpeg2ts::crc::Crc32;
use mpeg2ts::time::{ClockReference, Timestamp};
use mpeg2ts_wasm::aes3;
use wasm_bindgen::prelude::*;

const TS_PACKET_SIZE: usize = 188;
const PAT_PID: u16 = 0x0000;
const DEFAULT_VIDEO_PID: u16 = 0x100;
const DEFAULT_AUDIO_PID: u16 = 0x101;
const PMT_PID: u16 = 0x1000;

const STREAM_TYPE_H264: u8 = 0x1B;
const STREAM_TYPE_HEVC: u8 = 0x24;
const STREAM_TYPE_PRIVATE: u8 = 0x06;

const AV1_DESCRIPTOR: &[u8] = &[0x05, 0x04, 0x41, 0x56, 0x30, 0x31];
const OPUS_DESCRIPTOR: &[u8] = &[0x05, 0x04, 0x4F, 0x70, 0x75, 0x73, 0x7F, 0x02, 0x80, 0x02];
const S302M_DESCRIPTOR: &[u8] = &[0x05, 0x04, 0x42, 0x53, 0x53, 0x44];

const SYNC_BYTE: u8 = 0x47;

const SILENCE_THRESHOLD: f32 = 1e-6;
const PMT_RESEND_COUNT: u32 = 10;
/// Minimum PTS interval between periodic PAT/PMT emissions in `push_pcm`
/// (9000 ticks at 90 kHz = 100 ms).
const PSI_INTERVAL_TICKS: u64 = 9_000;

#[derive(Clone, Copy, PartialEq, Eq)]
enum AudioKind {
    Opus,
    Smpte302m,
}

#[derive(Clone)]
struct AudioStream {
    pid: u16,
    cc: u8,
    kind: AudioKind,
    channel_count: u8,
    silence_ms: f64,
    suppressed: bool,
}

impl AudioStream {
    fn stream_type(&self) -> u8 {
        STREAM_TYPE_PRIVATE
    }

    fn descriptor(&self) -> Vec<u8> {
        match self.kind {
            AudioKind::Opus => OPUS_DESCRIPTOR.to_vec(),
            AudioKind::Smpte302m => S302M_DESCRIPTOR.to_vec(),
        }
    }
}

#[wasm_bindgen]
pub struct TsMuxer {
    video_enabled: bool,
    video_pid: u16,
    video_cc: u8,
    video_stream_type: u8,
    video_descriptor: Vec<u8>,

    audio_streams: Vec<AudioStream>,

    pmt_pid: u16,
    pat_cc: u8,
    pmt_cc: u8,
    pcr: u64,
    output: Vec<u8>,
    pat_pmt_emitted: bool,
    last_psi_pts_90k: u64,
    /// Sparse (silence) suppression is opt-in. When enabled, sustained
    /// silence drops a PID from the PMT and resets its CC on resume —
    /// the path has never been exercised against a real receiver chain.
    sparse_enabled: bool,
    sparse_threshold_ms: f64,
    pmt_dirty: bool,
    pmt_resend_count: u32,
}

#[wasm_bindgen]
impl TsMuxer {
    #[wasm_bindgen(constructor)]
    pub fn new() -> TsMuxer {
        TsMuxer {
            video_enabled: true,
            video_pid: DEFAULT_VIDEO_PID,
            video_cc: 0,
            video_stream_type: STREAM_TYPE_H264,
            video_descriptor: Vec::new(),
            audio_streams: vec![AudioStream {
                pid: DEFAULT_AUDIO_PID,
                cc: 0,
                kind: AudioKind::Opus,
                channel_count: 2,
                silence_ms: 0.0,
                suppressed: false,
            }],
            pmt_pid: PMT_PID,
            pat_cc: 0,
            pmt_cc: 0,
            pcr: 0,
            output: Vec::new(),
            pat_pmt_emitted: false,
            last_psi_pts_90k: 0,
            sparse_enabled: false,
            sparse_threshold_ms: 300.0,
            pmt_dirty: false,
            pmt_resend_count: 0,
        }
    }

    #[wasm_bindgen(js_name = setVideoCodec)]
    pub fn set_video_codec(&mut self, codec: &str) {
        match codec {
            "av1" => {
                self.video_stream_type = STREAM_TYPE_PRIVATE;
                self.video_descriptor = AV1_DESCRIPTOR.to_vec();
            }
            "hevc" => {
                self.video_stream_type = STREAM_TYPE_HEVC;
                self.video_descriptor.clear();
            }
            _ => {
                self.video_stream_type = STREAM_TYPE_H264;
                self.video_descriptor.clear();
            }
        }
    }

    #[wasm_bindgen(js_name = setVideoEnabled)]
    pub fn set_video_enabled(&mut self, enabled: bool) {
        self.video_enabled = enabled;
    }

    #[wasm_bindgen(js_name = setAudioCodec)]
    pub fn set_audio_codec(&mut self, codec: &str, channel_count: u8) {
        let kind = match codec {
            "s302m" => AudioKind::Smpte302m,
            _ => AudioKind::Opus,
        };
        if let Some(stream) = self.audio_streams.get_mut(0) {
            stream.kind = kind;
            stream.channel_count = channel_count;
        }
    }

    #[wasm_bindgen(js_name = addAudioPid)]
    pub fn add_audio_pid(&mut self, pid: u16, codec: &str, channel_count: u8) {
        let kind = match codec {
            "s302m" => AudioKind::Smpte302m,
            _ => AudioKind::Opus,
        };
        if !self.audio_streams.iter().any(|s| s.pid == pid) {
            self.audio_streams.push(AudioStream {
                pid,
                cc: 0,
                kind,
                channel_count,
                silence_ms: 0.0,
                suppressed: false,
            });
        }
    }

    /// Opt in to sparse (silence) suppression. Off by default: suppression
    /// mutates the PMT (drops silent PIDs) and resets CC on resume, and has
    /// never run against a real muxer source or receiver chain.
    #[wasm_bindgen(js_name = setSparseEnabled)]
    pub fn set_sparse_enabled(&mut self, enabled: bool) {
        self.sparse_enabled = enabled;
    }

    #[wasm_bindgen(js_name = setSparseThreshold)]
    pub fn set_sparse_threshold(&mut self, ms: f64) {
        self.sparse_threshold_ms = ms;
    }

    fn pcr_pid(&self) -> u16 {
        if self.video_enabled {
            self.video_pid
        } else {
            let sparse = self.sparse_enabled;
            self.audio_streams
                .iter()
                .find(|s| !(sparse && s.suppressed))
                .map(|s| s.pid)
                .unwrap_or(DEFAULT_AUDIO_PID)
        }
    }

    #[wasm_bindgen(js_name = push_video)]
    pub fn push_video(&mut self, data: &[u8], pts_us: f64, dts_us: f64, is_keyframe: bool) {
        if !self.video_enabled {
            return;
        }
        let pts_90k = us_to_90k(pts_us);
        let dts_90k = us_to_90k(dts_us);
        self.pcr = pts_90k.wrapping_mul(300);

        if is_keyframe {
            self.write_pat();
            self.write_pmt();
            self.pat_pmt_emitted = true;
            self.last_psi_pts_90k = pts_90k;
        }

        let pes = build_pes_video(data, pts_90k, dts_90k);
        packetize(
            &mut self.output,
            self.video_pid,
            &mut self.video_cc,
            &pes,
            Some(pts_90k),
            is_keyframe,
        );
    }

    #[wasm_bindgen(js_name = push_audio)]
    pub fn push_audio(&mut self, data: &[u8], pts_us: f64) {
        let pts_90k = us_to_90k(pts_us);
        let idx = self
            .audio_streams
            .iter()
            .position(|s| s.kind == AudioKind::Opus)
            .unwrap_or(0);
        let pid = self.audio_streams[idx].pid;
        let pcr_pid = self.pcr_pid();
        let is_pcr = pid == pcr_pid;
        let mut payload = Vec::with_capacity(2 + data.len());
        payload.extend_from_slice(&[0x7F, 0xE0]);
        payload.extend_from_slice(data);
        let pes = build_pes_audio(&payload, pts_90k);
        let cc = &mut self.audio_streams[idx].cc;
        packetize(
            &mut self.output,
            pid,
            cc,
            &pes,
            if is_pcr { Some(pts_90k) } else { None },
            false,
        );
    }

    #[wasm_bindgen(js_name = push_pcm)]
    pub fn push_pcm(&mut self, samples: &[f32], pts_us: f64) {
        let idx = self
            .audio_streams
            .iter()
            .position(|s| s.kind == AudioKind::Smpte302m)
            .unwrap_or(0);
        self.push_pcm_stream(idx, samples, pts_us);
    }

    /// Like `push_pcm`, but targets the s302m stream carrying `pid`
    /// (registered via `add_audioPid`). Unknown pids and non-s302m streams
    /// are a no-op.
    #[wasm_bindgen(js_name = push_pcm_pid)]
    pub fn push_pcm_pid(&mut self, pid: u16, samples: &[f32], pts_us: f64) {
        let Some(idx) = self.audio_streams.iter().position(|s| s.pid == pid) else {
            return;
        };
        if self.audio_streams[idx].kind != AudioKind::Smpte302m {
            return;
        }
        self.push_pcm_stream(idx, samples, pts_us);
    }

    #[wasm_bindgen(js_name = poll)]
    pub fn poll(&mut self) -> Vec<u8> {
        std::mem::take(&mut self.output)
    }
}

impl TsMuxer {
    /// Shared PCM push path: s302m AES3 wrapping, PES on the target stream's
    /// PID, per-stream CC + sparse accounting, PSI emission.
    fn push_pcm_stream(&mut self, idx: usize, samples: &[f32], pts_us: f64) {
        let pts_90k = us_to_90k(pts_us);
        self.pcr = pts_90k.wrapping_mul(300);

        let (pid, channel_count) = {
            let stream = &self.audio_streams[idx];
            (stream.pid, stream.channel_count)
        };

        if !self.pat_pmt_emitted {
            self.write_pat();
            self.write_pmt();
            self.pat_pmt_emitted = true;
            self.last_psi_pts_90k = pts_90k;
        }

        if self.sparse_enabled {
            let silent = is_silent(samples);
            let frame_ms = (samples.len() / channel_count as usize) as f64 / 48.0;
            let stream = &mut self.audio_streams[idx];
            if silent {
                stream.silence_ms += frame_ms;
            } else {
                stream.silence_ms = 0.0;
            }

            if stream.silence_ms > self.sparse_threshold_ms && !stream.suppressed {
                stream.suppressed = true;
                self.pmt_dirty = true;
                self.pmt_resend_count = PMT_RESEND_COUNT;
            } else if !silent && stream.suppressed {
                stream.suppressed = false;
                stream.cc = 0;
                self.pmt_dirty = true;
                self.pmt_resend_count = PMT_RESEND_COUNT;
            }
        }

        if self.pmt_dirty {
            self.write_pat();
            self.write_pmt();
            self.pmt_dirty = false;
            self.last_psi_pts_90k = pts_90k;
        }
        if self.pmt_resend_count > 0 {
            self.write_pat();
            self.write_pmt();
            self.pmt_resend_count -= 1;
            self.last_psi_pts_90k = pts_90k;
        }

        // Periodic PAT/PMT so mid-stream joiners can tune without waiting
        // for a sparse-suppression transition. Runs before the suppression
        // early-return to keep PSI flowing during silent gaps.
        if pts_90k.saturating_sub(self.last_psi_pts_90k) >= PSI_INTERVAL_TICKS {
            self.write_pat();
            self.write_pmt();
            self.last_psi_pts_90k = pts_90k;
        }

        let suppressed = self.audio_streams[idx].suppressed;
        if self.sparse_enabled && suppressed {
            return;
        }

        let pcr_pid = self.pcr_pid();
        let is_pcr = pid == pcr_pid;
        let aes3_payload = aes3::wrap_smpte302m_pes(samples, channel_count, 24);
        let pes = build_pes_audio(&aes3_payload, pts_90k);
        let cc = &mut self.audio_streams[idx].cc;
        packetize(
            &mut self.output,
            pid,
            cc,
            &pes,
            if is_pcr { Some(pts_90k) } else { None },
            false,
        );
    }

    fn write_pat(&mut self) {
        let mut section: Vec<u8> = vec![
            0x00, 0xB0, 0x0D, 0x00, 0x01, 0xC1, 0x00, 0x00, 0x00, 0x01, 0xF0, 0x00,
        ];
        let mut crc = Crc32::new();
        crc.update(&section);
        section.extend_from_slice(&crc.value().to_be_bytes());
        packetize_psi(&mut self.output, PAT_PID, &mut self.pat_cc, &section);
    }

    fn write_pmt(&mut self) {
        let pcr_pid = self.pcr_pid();

        let mut entries: Vec<(u8, u16, &[u8])> = Vec::new();
        if self.video_enabled {
            entries.push((
                self.video_stream_type,
                self.video_pid,
                &self.video_descriptor,
            ));
        }
        let sparse = self.sparse_enabled;
        let active_audio: Vec<&AudioStream> = self
            .audio_streams
            .iter()
            .filter(|s| !(sparse && s.suppressed))
            .collect();
        let audio_descriptors: Vec<Vec<u8>> = active_audio.iter().map(|s| s.descriptor()).collect();
        for (i, s) in active_audio.iter().enumerate() {
            entries.push((s.stream_type(), s.pid, &audio_descriptors[i]));
        }

        let entries_esil: usize = entries.iter().map(|(_, _, d)| d.len()).sum();
        // section_length counts the bytes after this field through CRC32
        // inclusive: program_number(2) + version(1) + section_number(1) +
        // last_section_number(1) + PCR_PID(2) + program_info_length(2) +
        // entries + CRC32(4) = 13 + entries + descriptors.
        let section_length: u16 = (13 + entries.len() * 5 + entries_esil) as u16;

        let mut s: Vec<u8> = Vec::with_capacity(32 + section_length as usize);
        s.push(0x02);
        s.push(0xB0 | ((section_length >> 8) as u8 & 0x0F));
        s.push((section_length & 0xFF) as u8);
        s.extend_from_slice(&[0x00, 0x01]);
        s.push(0xC1);
        s.extend_from_slice(&[0x00, 0x00]);
        s.push(((pcr_pid >> 8) as u8) | 0xE0);
        s.push((pcr_pid & 0xFF) as u8);
        s.extend_from_slice(&[0xF0, 0x00]);

        for (st, pid, desc) in &entries {
            s.push(*st);
            s.push(((pid >> 8) as u8) | 0xE0);
            s.push((pid & 0xFF) as u8);
            let esil = desc.len() as u16;
            s.push(0xF0 | ((esil >> 8) as u8 & 0x0F));
            s.push((esil & 0xFF) as u8);
            s.extend_from_slice(desc);
        }

        let mut crc = Crc32::new();
        crc.update(&s);
        s.extend_from_slice(&crc.value().to_be_bytes());
        packetize_psi(&mut self.output, self.pmt_pid, &mut self.pmt_cc, &s);
    }
}

/// Packetize one PSI section (PAT or PMT) across N TS packets per
/// ISO/IEC 13818-1 §2.4.4. The first packet sets PUSI and carries
/// pointer_field=0x00 before the section start; continuation packets
/// carry raw section bytes with PUSI clear; the final packet 0xFF-stuffs
/// the remainder. Only one section is emitted per call, so a new section
/// never starts mid-packet. For sections that fit a single packet the
/// output is byte-identical to the historical one-packet writers.
fn packetize_psi(output: &mut Vec<u8>, pid: u16, cc: &mut u8, section: &[u8]) {
    let mut offset = 0usize;
    let mut first = true;
    while offset < section.len() {
        // First packet reserves one payload byte for the pointer_field.
        let overhead = if first { 1 } else { 0 };
        let payload_room = TS_PACKET_SIZE - 4 - overhead;

        let chunk = (section.len() - offset).min(payload_room);

        let mut pkt = [0u8; TS_PACKET_SIZE];
        pkt[0] = SYNC_BYTE;
        let pusi = if first { 0x40u8 } else { 0x00u8 };
        pkt[1] = pusi | ((pid >> 8) as u8 & 0x1F);
        pkt[2] = (pid & 0xFF) as u8;
        pkt[3] = 0x10 | (*cc & 0x0F);
        *cc = (*cc + 1) & 0x0F;

        let mut pos = 4usize;
        if first {
            pkt[pos] = 0x00; // pointer_field: section starts right here
            pos += 1;
        }
        pkt[pos..pos + chunk].copy_from_slice(&section[offset..offset + chunk]);
        for byte in &mut pkt[pos + chunk..] {
            *byte = 0xFF;
        }
        output.extend_from_slice(&pkt);

        offset += chunk;
        first = false;
    }
}

fn packetize(
    output: &mut Vec<u8>,
    pid: u16,
    cc: &mut u8,
    data: &[u8],
    pcr_base: Option<u64>,
    random_access: bool,
) {
    let total = data.len();
    if total == 0 {
        return;
    }

    let mut offset = 0usize;
    let mut first = true;
    while offset < total {
        let pcr_on_this = first && pcr_base.is_some();
        let pcr_overhead = if pcr_on_this { 8 } else { 0 };
        let payload_room = TS_PACKET_SIZE - 4 - pcr_overhead;

        let remaining = total - offset;
        let chunk = remaining.min(payload_room);
        let is_last = offset + chunk == total;
        let needs_stuffing = is_last && chunk < payload_room;

        let mut pkt = [0u8; TS_PACKET_SIZE];
        pkt[0] = SYNC_BYTE;
        let pusi = if first { 0x40u8 } else { 0x00u8 };
        pkt[1] = pusi | ((pid >> 8) as u8 & 0x1F);
        pkt[2] = (pid & 0xFF) as u8;

        let mut pos = 4usize;
        if pcr_overhead > 0 || needs_stuffing {
            pkt[3] = 0x30 | (*cc & 0x0F);
            *cc = (*cc + 1) & 0x0F;

            let af_total = TS_PACKET_SIZE - 4 - chunk;
            let af_length = af_total - 1;
            pkt[pos] = af_length as u8;
            pos += 1;

            if af_length >= 1 {
                let mut flags = 0u8;
                if pcr_overhead > 0 {
                    flags |= 0x10;
                }
                if first && random_access {
                    flags |= 0x40;
                }
                pkt[pos] = flags;
                pos += 1;

                if pcr_overhead > 0 {
                    let pcr =
                        ClockReference::new((pcr_base.unwrap() & Timestamp::MAX) * 300).unwrap();
                    pkt[pos..pos + 6].copy_from_slice(&pcr.pcr_to_bytes());
                    pos += 6;
                }
                for byte in &mut pkt[pos..TS_PACKET_SIZE - chunk] {
                    *byte = 0xFF;
                }
            }
            pos = TS_PACKET_SIZE - chunk;
        } else {
            pkt[3] = 0x10 | (*cc & 0x0F);
            *cc = (*cc + 1) & 0x0F;
        }

        pkt[pos..pos + chunk].copy_from_slice(&data[offset..offset + chunk]);
        output.extend_from_slice(&pkt);

        offset += chunk;
        first = false;
    }
}

fn build_pes_video(data: &[u8], pts_90k: u64, dts_90k: u64) -> Vec<u8> {
    let has_dts = dts_90k != pts_90k;
    let (flags2, header_len) = if has_dts {
        (0xC0u8, 10u8)
    } else {
        (0x80u8, 5u8)
    };
    let pts_prefix = if has_dts { 0b0011 } else { 0b0010 };

    let mut pes = Vec::with_capacity(9 + header_len as usize + data.len());
    pes.extend_from_slice(&[0x00, 0x00, 0x01, 0xE0]);
    pes.extend_from_slice(&[0x00, 0x00]);
    pes.push(0x80);
    pes.push(flags2);
    pes.push(header_len);
    let pts = Timestamp::new(pts_90k & Timestamp::MAX).unwrap();
    pes.extend_from_slice(&pts.to_bytes(pts_prefix));
    if has_dts {
        let dts = Timestamp::new(dts_90k & Timestamp::MAX).unwrap();
        pes.extend_from_slice(&dts.to_bytes(0b0001));
    }
    pes.extend_from_slice(data);
    pes
}

fn build_pes_audio(data: &[u8], pts_90k: u64) -> Vec<u8> {
    let pes_packet_length = (3 + 5 + data.len()) as u16;
    let mut pes = Vec::with_capacity(9 + 5 + data.len());
    pes.extend_from_slice(&[0x00, 0x00, 0x01, 0xC0]);
    pes.extend_from_slice(&pes_packet_length.to_be_bytes());
    pes.push(0x80);
    pes.push(0x80);
    pes.push(0x05);
    let pts = Timestamp::new(pts_90k & Timestamp::MAX).unwrap();
    pes.extend_from_slice(&pts.to_bytes(0b0010));
    pes.extend_from_slice(data);
    pes
}

fn us_to_90k(us: f64) -> u64 {
    (us.max(0.0) * 9.0 / 100.0) as u64
}

fn is_silent(samples: &[f32]) -> bool {
    for &s in samples {
        if s.abs() > SILENCE_THRESHOLD {
            return false;
        }
    }
    true
}

impl Default for TsMuxer {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn full_section(m: &mut TsMuxer) -> Vec<u8> {
        m.push_video(&[0; 8], 0.0, 0.0, true);
        let out = m.poll();
        for chunk in out.chunks(TS_PACKET_SIZE) {
            let pid = ((chunk[1] as u16 & 0x1F) << 8) | chunk[2] as u16;
            if pid == PMT_PID {
                let sl = (((chunk[6] as u16 & 0x0F) << 8) | chunk[7] as u16) as usize;
                return chunk[5..5 + 3 + sl].to_vec();
            }
        }
        unreachable!("no PMT");
    }

    fn full_section_audio_only(m: &mut TsMuxer) -> Vec<u8> {
        m.push_pcm(&[0.0_f32; 4], 0.0);
        let out = m.poll();
        for chunk in out.chunks(TS_PACKET_SIZE) {
            let pid = ((chunk[1] as u16 & 0x1F) << 8) | chunk[2] as u16;
            if pid == PMT_PID {
                let sl = (((chunk[6] as u16 & 0x0F) << 8) | chunk[7] as u16) as usize;
                return chunk[5..5 + 3 + sl].to_vec();
            }
        }
        unreachable!("no PMT");
    }

    #[test]
    fn h264_default_has_opus_descriptor() {
        let mut m = TsMuxer::new();
        let s = full_section(&mut m);
        assert_eq!(s[12], 0x1B);
        assert_eq!(s[17], 0x06);
        assert_eq!(&s[22..28], &[0x05, 0x04, 0x4F, 0x70, 0x75, 0x73]);
    }

    #[test]
    fn av1_has_av01_and_opus_descriptors() {
        let mut m = TsMuxer::new();
        m.set_video_codec("av1");
        let s = full_section(&mut m);
        assert_eq!(s[12], 0x06);
        assert_eq!(&s[17..23], &[0x05, 0x04, 0x41, 0x56, 0x30, 0x31]);
    }

    #[test]
    fn s302m_audio_descriptor() {
        let mut m = TsMuxer::new();
        m.set_audio_codec("s302m", 2);
        let s = full_section(&mut m);
        let sl = ((s[1] as u16 & 0x0F) << 8) | s[2] as u16;
        let section = &s[..3 + sl as usize];
        let bssd = [0x05, 0x04, 0x42, 0x53, 0x53, 0x44];
        assert!(
            section.windows(bssd.len()).any(|w| w == bssd),
            "BSSD descriptor not found in PMT"
        );
    }

    #[test]
    fn audio_only_no_video_entry() {
        let mut m = TsMuxer::new();
        m.set_video_enabled(false);
        m.set_audio_codec("s302m", 2);
        let s = full_section_audio_only(&mut m);
        let sl = ((s[1] as u16 & 0x0F) << 8) | s[2] as u16;
        let section = &s[..3 + sl as usize];
        assert!(
            !section.windows(3).any(|w| w == &[0x1B, 0xE1, 0x00]),
            "H.264 video entry should be absent"
        );
    }

    #[test]
    fn multi_audio_pids_in_pmt() {
        let mut m = TsMuxer::new();
        m.set_video_enabled(false);
        m.set_audio_codec("s302m", 2);
        m.add_audio_pid(0x102, "s302m", 6);
        let s = full_section_audio_only(&mut m);
        let sl = ((s[1] as u16 & 0x0F) << 8) | s[2] as u16;
        let section = &s[..3 + sl as usize];
        let pid_101 = [0xE1, 0x01];
        let pid_102 = [0xE1, 0x02];
        assert!(
            section.windows(2).any(|w| w == pid_101),
            "PID 0x101 missing"
        );
        assert!(
            section.windows(2).any(|w| w == pid_102),
            "PID 0x102 missing"
        );
    }

    #[test]
    fn pcr_pid_is_audio_in_audio_only() {
        let mut m = TsMuxer::new();
        m.set_video_enabled(false);
        m.set_audio_codec("s302m", 2);
        let s = full_section_audio_only(&mut m);
        assert_eq!(
            &s[8..10],
            &[0xE1, 0x01],
            "PCR_PID should be 0x101 (first audio)"
        );
    }

    #[test]
    fn pcr_pid_is_video_when_video_enabled() {
        let mut m = TsMuxer::new();
        let s = full_section(&mut m);
        assert_eq!(&s[8..10], &[0xE1, 0x00], "PCR_PID should be 0x100 (video)");
    }

    #[test]
    fn push_pcm_emits_pat_pmt_and_pes() {
        let mut m = TsMuxer::new();
        m.set_video_enabled(false);
        m.set_audio_codec("s302m", 2);
        let samples = vec![0.0_f32, 0.5, -0.5, 0.25];
        m.push_pcm(&samples, 1000.0);
        let out = m.poll();
        assert!(
            out.len() >= TS_PACKET_SIZE * 3,
            "should have PAT + PMT + PES"
        );
        let pids: Vec<u16> = out
            .chunks(TS_PACKET_SIZE)
            .map(|c| ((c[1] as u16 & 0x1F) << 8) | c[2] as u16)
            .collect();
        assert!(pids.contains(&PAT_PID), "PAT missing");
        assert!(pids.contains(&PMT_PID), "PMT missing");
        assert!(pids.contains(&0x101), "audio PES missing");
    }

    fn first_video_pusi_af_flags(out: &[u8]) -> u8 {
        for chunk in out.chunks(TS_PACKET_SIZE) {
            let pid = ((chunk[1] as u16 & 0x1F) << 8) | chunk[2] as u16;
            if pid == DEFAULT_VIDEO_PID && (chunk[1] & 0x40 != 0) {
                assert_eq!((chunk[3] >> 4) & 0x03, 0b11);
                return chunk[5];
            }
        }
        unreachable!("no video PUSI packet found");
    }

    #[test]
    fn pcr_on_every_video_frame_random_access_only_on_keyframe() {
        let mut m = TsMuxer::new();
        m.push_video(&[0xAA; 8], 1_000_000.0, 1_000_000.0, false);
        let af_flags = first_video_pusi_af_flags(&m.poll());
        assert_eq!(af_flags & 0x10, 0x10);
        assert_eq!(af_flags & 0x40, 0x00);

        let mut m = TsMuxer::new();
        m.push_video(&[0xBB; 8], 1_000_000.0, 1_000_000.0, true);
        let af_flags = first_video_pusi_af_flags(&m.poll());
        assert_eq!(af_flags & 0x10, 0x10);
        assert_eq!(af_flags & 0x40, 0x40);
    }

    fn last_pmt_section(out: &[u8]) -> Vec<u8> {
        let mut result: Option<Vec<u8>> = None;
        for chunk in out.chunks(TS_PACKET_SIZE) {
            let pid = ((chunk[1] as u16 & 0x1F) << 8) | chunk[2] as u16;
            if pid == PMT_PID {
                let sl = (((chunk[6] as u16 & 0x0F) << 8) | chunk[7] as u16) as usize;
                result = Some(chunk[5..5 + 3 + sl].to_vec());
            }
        }
        result.expect("no PMT found in output")
    }

    fn pkt_pid(chunk: &[u8]) -> u16 {
        ((chunk[1] as u16 & 0x1F) << 8) | chunk[2] as u16
    }

    #[test]
    fn sparse_drops_silent_pid_from_pmt() {
        let mut m = TsMuxer::new();
        m.set_video_enabled(false);
        m.set_audio_codec("s302m", 2);
        m.set_sparse_enabled(true);
        m.set_sparse_threshold(10.0);

        m.push_pcm(&[0.5_f32; 480], 0.0);
        let out = m.poll();
        let pmt = last_pmt_section(&out);
        let entry = [0x06, 0xE1, 0x01];
        assert!(
            pmt.windows(3).any(|w| w == entry),
            "PID 0x101 ES entry should be in PMT when audio is active"
        );

        for _ in 0..3 {
            m.push_pcm(&[0.0_f32; 480], 0.0);
        }
        let out = m.poll();
        let pmt = last_pmt_section(&out);
        assert!(
            !pmt.windows(3).any(|w| w == entry),
            "PID 0x101 ES entry should be absent from PMT after sustained silence"
        );
    }

    #[test]
    fn sparse_readds_pid_on_signal() {
        let mut m = TsMuxer::new();
        m.set_video_enabled(false);
        m.set_audio_codec("s302m", 2);
        m.set_sparse_enabled(true);
        m.set_sparse_threshold(10.0);

        for _ in 0..3 {
            m.push_pcm(&[0.0_f32; 480], 0.0);
        }
        m.poll();

        m.push_pcm(&[0.5_f32; 480], 0.0);
        let out = m.poll();
        let pmt = last_pmt_section(&out);
        let entry = [0x06, 0xE1, 0x01];
        assert!(
            pmt.windows(3).any(|w| w == entry),
            "PID 0x101 ES entry should reappear in PMT when audio resumes"
        );
    }

    #[test]
    fn sparse_disabled_emits_all_pids() {
        let mut m = TsMuxer::new();
        m.set_video_enabled(false);
        m.set_audio_codec("s302m", 2);
        m.set_sparse_enabled(false);

        for _ in 0..100 {
            m.push_pcm(&[0.0_f32; 480], 0.0);
        }
        let out = m.poll();
        let pmt = last_pmt_section(&out);
        let entry = [0x06, 0xE1, 0x01];
        assert!(
            pmt.windows(3).any(|w| w == entry),
            "PID 0x101 ES entry should remain in PMT when sparse is disabled"
        );
    }

    #[test]
    fn sparse_disabled_by_default() {
        let mut m = TsMuxer::new();
        m.set_video_enabled(false);
        m.set_audio_codec("s302m", 2);
        // No set_sparse_enabled call: a fresh muxer must not suppress.

        // Sustained silence far past the default 300 ms threshold.
        let mut all = Vec::new();
        for i in 0..100usize {
            m.push_pcm(&[0.0_f32; 480], (i as f64) * 5_000.0);
            all.extend(m.poll());
        }
        let pmt = last_pmt_section(&all);
        assert!(
            pmt.windows(3).any(|w| w == [0x06, 0xE1, 0x01]),
            "PID 0x101 ES entry must remain in the PMT by default"
        );

        // A silent push long after the threshold must still emit PES.
        m.push_pcm(&[0.0_f32; 480], 500_000.0);
        let out = m.poll();
        assert!(
            out.chunks(TS_PACKET_SIZE).any(|c| pkt_pid(c) == 0x101),
            "silent audio must still emit PES when sparse is off by default"
        );
    }

    #[test]
    fn sparse_pmt_resend_after_change() {
        let mut m = TsMuxer::new();
        m.set_video_enabled(false);
        m.set_audio_codec("s302m", 2);
        m.set_sparse_enabled(true);
        m.set_sparse_threshold(10.0);

        let mut pmt_count = 0u32;
        for _ in 0..15 {
            m.push_pcm(&[0.0_f32; 480], 0.0);
            let out = m.poll();
            for chunk in out.chunks(TS_PACKET_SIZE) {
                let pid = ((chunk[1] as u16 & 0x1F) << 8) | chunk[2] as u16;
                if pid == PMT_PID {
                    pmt_count += 1;
                }
            }
        }
        assert!(
            pmt_count >= 10,
            "expected multiple PMT re-emissions after suppression, got {}",
            pmt_count
        );
    }

    #[test]
    fn push_pcm_periodic_pmt_every_100ms() {
        let mut m = TsMuxer::new();
        m.set_video_enabled(false);
        m.set_audio_codec("s302m", 2);

        // 960 frames stereo @ 48 kHz = 20 ms per push, pts stepping 20 ms.
        let frame = [0.5_f32; 1920];
        let mut pmt_pushes: Vec<usize> = Vec::new();
        for i in 0..50usize {
            m.push_pcm(&frame, (i * 20_000) as f64);
            let out = m.poll();
            if out.chunks(TS_PACKET_SIZE).any(|c| pkt_pid(c) == PMT_PID) {
                pmt_pushes.push(i);
            }
        }
        assert!(
            (8..=13).contains(&pmt_pushes.len()),
            "expected a PMT roughly every 5th push (10 of 50), got {} in {:?}",
            pmt_pushes.len(),
            pmt_pushes
        );
        for w in pmt_pushes.windows(2) {
            assert!(
                w[1] - w[0] <= 10,
                "gap longer than 200 ms between PSI emissions: {:?}",
                pmt_pushes
            );
        }
    }

    #[test]
    fn push_pcm_periodic_pmt_during_suppression() {
        let mut m = TsMuxer::new();
        m.set_video_enabled(false);
        m.set_audio_codec("s302m", 2);
        m.set_sparse_enabled(true);
        m.set_sparse_threshold(10.0);

        let silent = [0.0_f32; 1920]; // 20 ms stereo, over threshold instantly
        let mut pmt_pushes: Vec<usize> = Vec::new();
        for i in 0..40usize {
            m.push_pcm(&silent, (i * 20_000) as f64);
            let out = m.poll();
            if out.chunks(TS_PACKET_SIZE).any(|c| pkt_pid(c) == PMT_PID) {
                pmt_pushes.push(i);
            }
            for c in out.chunks(TS_PACKET_SIZE) {
                assert_ne!(pkt_pid(c), 0x101, "no audio PES expected while suppressed");
            }
        }
        // The dirty/resend burst covers roughly the first 11 pushes; periodic
        // emission must keep PSI flowing after it.
        let post_burst: Vec<usize> = pmt_pushes.iter().copied().filter(|&i| i > 11).collect();
        assert!(
            post_burst.len() >= 3,
            "periodic PMTs should continue after resend burst, got pushes {:?}",
            pmt_pushes
        );
        for w in pmt_pushes.windows(2) {
            assert!(
                w[1] - w[0] <= 10,
                "gap longer than 200 ms between PSI emissions: {:?}",
                pmt_pushes
            );
        }
    }

    #[test]
    fn push_pcm_pid_targets_registered_pid() {
        let mut m = TsMuxer::new();
        m.set_video_enabled(false);
        m.set_audio_codec("s302m", 2);
        m.add_audio_pid(0x102, "s302m", 2);

        m.push_pcm_pid(0x102, &[0.5_f32; 1920], 0.0);
        let out = m.poll();
        let pids: Vec<u16> = out.chunks(TS_PACKET_SIZE).map(pkt_pid).collect();
        assert!(pids.contains(&0x102), "audio PES missing on 0x102");
        assert!(
            !pids.contains(&0x101),
            "nothing should land on 0x101 when only 0x102 is pushed"
        );
        assert!(pids.contains(&PAT_PID), "PAT missing");
        let pmt = last_pmt_section(&out);
        assert!(
            pmt.windows(2).any(|w| w == [0xE1, 0x01]),
            "PMT should still list 0x101"
        );
        assert!(
            pmt.windows(2).any(|w| w == [0xE1, 0x02]),
            "PMT should list 0x102"
        );
    }

    #[test]
    fn push_pcm_pid_cc_advances_per_pid() {
        let mut m = TsMuxer::new();
        m.set_video_enabled(false);
        m.set_audio_codec("s302m", 2);
        m.add_audio_pid(0x102, "s302m", 2);

        // 2 stereo frames → a single TS packet per push, so the PUSI CC of
        // each PID advances by exactly 1 between its own pushes.
        let loud = [0.5_f32; 4];
        let mut outputs: Vec<Vec<u8>> = Vec::new();
        m.push_pcm(&loud, 0.0); // lands on 0x101
        outputs.push(m.poll());
        m.push_pcm_pid(0x102, &loud, 20_000.0);
        outputs.push(m.poll());
        m.push_pcm(&loud, 40_000.0); // 0x101 again
        outputs.push(m.poll());
        m.push_pcm_pid(0x102, &loud, 60_000.0);
        outputs.push(m.poll());

        let pusi_ccs = |pid: u16| -> Vec<u8> {
            outputs
                .iter()
                .flat_map(|o| o.chunks(TS_PACKET_SIZE))
                .filter(|c| pkt_pid(c) == pid && (c[1] & 0x40) != 0)
                .map(|c| c[3] & 0x0F)
                .collect()
        };
        assert_eq!(
            pusi_ccs(0x101),
            vec![0, 1],
            "push_pcm must land on 0x101 with its own CC"
        );
        assert_eq!(
            pusi_ccs(0x102),
            vec![0, 1],
            "push_pcm_pid must keep a CC independent of 0x101 pushes"
        );
    }

    #[test]
    fn push_pcm_pid_unknown_or_opus_pid_is_noop() {
        let mut m = TsMuxer::new();
        m.set_video_enabled(false);
        m.set_audio_codec("s302m", 2);
        m.add_audio_pid(0x103, "opus", 2);

        m.push_pcm_pid(0x999, &[0.5_f32; 4], 0.0);
        assert!(m.poll().is_empty(), "unknown pid must emit nothing");
        m.push_pcm_pid(0x103, &[0.5_f32; 4], 0.0);
        assert!(m.poll().is_empty(), "non-s302m pid must emit nothing");

        // Muxer still fully functional via the default path afterwards.
        m.push_pcm(&[0.5_f32; 4], 0.0);
        let out = m.poll();
        let pids: Vec<u16> = out.chunks(TS_PACKET_SIZE).map(pkt_pid).collect();
        assert!(pids.contains(&PAT_PID) && pids.contains(&PMT_PID));
        assert!(pids.contains(&0x101), "push_pcm should land on 0x101");
    }

    /// 64 s302m streams: 0x101 via set_audio_codec + 0x102..=0x140 via
    /// add_audio_pid (CakeMix's 128-channel layout).
    fn muxer_with_64_audio_pids() -> TsMuxer {
        let mut m = TsMuxer::new();
        m.set_video_enabled(false);
        m.set_audio_codec("s302m", 2);
        for pid in 0x102..=0x140u16 {
            m.add_audio_pid(pid, "s302m", 2);
        }
        m
    }

    /// Reassemble complete PMT sections from a poll() buffer: a PUSI packet
    /// starts a section (skipping the pointer_field byte), non-PUSI packets
    /// on the PMT PID append, trailing 0xFF stuffing is trimmed via the
    /// section_length field.
    fn reassemble_pmt_sections(out: &[u8]) -> Vec<Vec<u8>> {
        let mut sections = Vec::new();
        let mut cur: Option<Vec<u8>> = None;
        for chunk in out.chunks(TS_PACKET_SIZE) {
            if pkt_pid(chunk) != PMT_PID {
                continue;
            }
            if chunk[1] & 0x40 != 0 {
                cur = Some(chunk[5..].to_vec()); // skip pointer_field
            } else if let Some(buf) = cur.as_mut() {
                buf.extend_from_slice(&chunk[4..]);
            } else {
                panic!("PMT continuation packet without a start packet");
            }
            let buf = cur.as_ref().expect("section in progress");
            let total = (((buf[1] as usize & 0x0F) << 8) | buf[2] as usize) + 3;
            if buf.len() >= total {
                let mut done = cur.take().expect("section in progress");
                done.truncate(total);
                sections.push(done);
            }
        }
        sections
    }

    #[test]
    fn pmt_64_audio_pids_spans_multiple_packets() {
        let mut m = muxer_with_64_audio_pids();
        m.push_pcm(&[0.5_f32; 4], 0.0);
        let out = m.poll();

        assert_eq!(
            out.len() % TS_PACKET_SIZE,
            0,
            "output must be a whole number of TS packets"
        );

        let pmt: Vec<&[u8]> = out
            .chunks(TS_PACKET_SIZE)
            .filter(|c| pkt_pid(c) == PMT_PID)
            .collect();
        assert!(
            pmt.len() >= 4,
            "64-entry PMT needs at least 4 TS packets, got {}",
            pmt.len()
        );
        for (i, c) in pmt.iter().enumerate() {
            assert_eq!(
                c[3] & 0x0F,
                (i as u8) & 0x0F,
                "continuity_counter must increment by 1 per PMT packet"
            );
            assert_eq!(
                c[1] & 0x40 != 0,
                i == 0,
                "PUSI must be set on the first PMT packet only"
            );
        }
    }

    #[test]
    fn pmt_64_audio_pids_section_reassembles() {
        let mut m = muxer_with_64_audio_pids();
        m.push_pcm(&[0.5_f32; 4], 0.0);
        let out = m.poll();

        let sections = reassemble_pmt_sections(&out);
        assert_eq!(sections.len(), 1, "expected exactly one PMT section");
        let s = &sections[0];
        let sl = ((s[1] as usize & 0x0F) << 8) | s[2] as usize;
        assert_eq!(
            s.len(),
            3 + sl,
            "section_length must match the reassembled byte count"
        );
        let crc = u32::from_be_bytes(s[s.len() - 4..].try_into().unwrap());
        let mut check = Crc32::new();
        check.update(&s[..s.len() - 4]);
        assert_eq!(
            check.value(),
            crc,
            "reassembled section CRC32 must validate"
        );
        for pid in 0x101..=0x140u16 {
            let entry = [0x06, (0xE0 | (pid >> 8)) as u8, (pid & 0xFF) as u8];
            assert!(
                s.windows(3).any(|w| w == entry),
                "PID 0x{:X} ES entry missing from PMT",
                pid
            );
        }
    }

    #[test]
    fn pmt_multi_packet_demuxer_roundtrip() {
        use mpeg2ts_wasm::TsDemuxer;

        let mut m = muxer_with_64_audio_pids();
        m.push_pcm(&[0.5_f32; 4], 0.0);
        let out = m.poll();

        let mut demux = TsDemuxer::new();
        let events = demux.feed(&out);
        let pmt_events: Vec<&mpeg2ts_wasm::TsEvent> =
            events.iter().filter(|e| e.kind() == 1).collect();
        assert!(!pmt_events.is_empty(), "demuxer should emit a pmt event");
        let flat = pmt_events[0].pmt_entries();
        let pids: Vec<u16> = flat.chunks(2).map(|c| c[0]).collect();
        assert_eq!(pids.len(), 64, "PMT should list all 64 audio PIDs");
        for pid in 0x101..=0x140u16 {
            assert!(
                pids.contains(&pid),
                "PID 0x{:X} missing from demuxed PMT",
                pid
            );
        }
    }
}
