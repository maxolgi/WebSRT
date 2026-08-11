//! `ts-muxer-wasm` — wasm32 MPEG-TS muxer for the browser.
//!
//! Takes H.264/HEVC/AV1 NAL units (Annex B) and audio packets and produces
//! 188-byte ISO/IEC 13818-1 MPEG-TS packets. JS drives it by pushing encoded
//! chunks via `push_video` / `push_audio` / `push_pcm` and draining finished
//! packets via `poll`.

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
            sparse_enabled: true,
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
        let pts_90k = us_to_90k(pts_us);
        self.pcr = pts_90k.wrapping_mul(300);

        let idx = self
            .audio_streams
            .iter()
            .position(|s| s.kind == AudioKind::Smpte302m)
            .unwrap_or(0);
        let (pid, channel_count) = {
            let stream = &self.audio_streams[idx];
            (stream.pid, stream.channel_count)
        };

        if !self.pat_pmt_emitted {
            self.write_pat();
            self.write_pmt();
            self.pat_pmt_emitted = true;
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
        }
        if self.pmt_resend_count > 0 {
            self.write_pat();
            self.write_pmt();
            self.pmt_resend_count -= 1;
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

    #[wasm_bindgen(js_name = poll)]
    pub fn poll(&mut self) -> Vec<u8> {
        std::mem::take(&mut self.output)
    }
}

impl TsMuxer {
    fn write_pat(&mut self) {
        let mut section: Vec<u8> = vec![
            0x00, 0xB0, 0x0D, 0x00, 0x01, 0xC1, 0x00, 0x00, 0x00, 0x01, 0xF0, 0x00,
        ];
        let crc = crc32(&section);
        section.extend_from_slice(&crc.to_be_bytes());

        let mut pkt = [0u8; TS_PACKET_SIZE];
        pkt[0] = SYNC_BYTE;
        pkt[1] = 0x40 | ((PAT_PID >> 8) as u8 & 0x1F);
        pkt[2] = (PAT_PID & 0xFF) as u8;
        pkt[3] = 0x10 | (self.pat_cc & 0x0F);
        self.pat_cc = (self.pat_cc + 1) & 0x0F;
        pkt[4] = 0x00;
        pkt[5..5 + section.len()].copy_from_slice(&section);
        for byte in &mut pkt[5 + section.len()..] {
            *byte = 0xFF;
        }
        self.output.extend_from_slice(&pkt);
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

        let crc = crc32(&s);
        s.extend_from_slice(&crc.to_be_bytes());

        let mut pkt = [0u8; TS_PACKET_SIZE];
        pkt[0] = SYNC_BYTE;
        pkt[1] = 0x40 | ((self.pmt_pid >> 8) as u8 & 0x1F);
        pkt[2] = (self.pmt_pid & 0xFF) as u8;
        pkt[3] = 0x10 | (self.pmt_cc & 0x0F);
        self.pmt_cc = (self.pmt_cc + 1) & 0x0F;
        pkt[4] = 0x00;
        pkt[5..5 + s.len()].copy_from_slice(&s);
        for byte in &mut pkt[5 + s.len()..] {
            *byte = 0xFF;
        }
        self.output.extend_from_slice(&pkt);
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
                    write_pcr(&mut pkt[pos..pos + 6], pcr_base.unwrap(), 0);
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
    pes.extend_from_slice(&encode_pts(pts_90k, pts_prefix));
    if has_dts {
        pes.extend_from_slice(&encode_pts(dts_90k, 0b0001));
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
    pes.extend_from_slice(&encode_pts(pts_90k, 0b0010));
    pes.extend_from_slice(data);
    pes
}

fn encode_pts(value: u64, prefix: u8) -> [u8; 5] {
    let v = value & 0x1FFFFFFFF;
    let b0 = (prefix << 4) | (((v >> 29) & 0x0E) as u8) | 0x01;
    let b1 = ((v >> 22) & 0xFF) as u8;
    let b2 = (((v >> 14) & 0xFE) as u8) | 0x01;
    let b3 = ((v >> 7) & 0xFF) as u8;
    let b4 = (((v << 1) & 0xFE) as u8) | 0x01;
    [b0, b1, b2, b3, b4]
}

fn write_pcr(buf: &mut [u8], base: u64, ext: u16) {
    buf[0] = ((base >> 25) & 0xFF) as u8;
    buf[1] = ((base >> 17) & 0xFF) as u8;
    buf[2] = ((base >> 9) & 0xFF) as u8;
    buf[3] = ((base >> 1) & 0xFF) as u8;
    buf[4] = (((base & 1) << 7) | 0x7E | ((ext >> 8) as u64 & 1)) as u8;
    buf[5] = (ext & 0xFF) as u8;
}

fn crc32(data: &[u8]) -> u32 {
    let mut crc: u32 = 0xFFFFFFFF;
    for &byte in data {
        crc ^= (byte as u32) << 24;
        for _ in 0..8 {
            if crc & 0x80000000 != 0 {
                crc = (crc << 1) ^ 0x04C11DB7;
            } else {
                crc <<= 1;
            }
        }
    }
    crc
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
}
