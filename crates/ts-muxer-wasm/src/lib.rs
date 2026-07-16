//! `ts-muxer-wasm` — wasm32 MPEG-TS muxer for the browser.
//!
//! Takes H.264 NAL units (Annex B) and optional AAC ADTS frames and produces
//! 188-byte ISO/IEC 13818-1 MPEG-TS packets. JS drives it by pushing encoded
//! chunks via `pushVideo` / `pushAudio` and draining finished packets via
//! `poll`.

use wasm_bindgen::prelude::*;

const TS_PACKET_SIZE: usize = 188;
const PAT_PID: u16 = 0x0000;
const VIDEO_PID: u16 = 0x100;
const AUDIO_PID: u16 = 0x101;
const PMT_PID: u16 = 0x1000;

const STREAM_TYPE_H264: u8 = 0x1B;
const STREAM_TYPE_AAC: u8 = 0x0F;

const SYNC_BYTE: u8 = 0x47;

#[wasm_bindgen]
pub struct TsMuxer {
    video_pid: u16,
    audio_pid: u16,
    pmt_pid: u16,
    video_cc: u8,
    audio_cc: u8,
    pat_cc: u8,
    pmt_cc: u8,
    pcr: u64,
    output: Vec<u8>,
    pat_pmt_emitted: bool,
    last_was_keyframe: bool,
}

#[wasm_bindgen]
impl TsMuxer {
    #[wasm_bindgen(constructor)]
    pub fn new() -> TsMuxer {
        TsMuxer {
            video_pid: VIDEO_PID,
            audio_pid: AUDIO_PID,
            pmt_pid: PMT_PID,
            video_cc: 0,
            audio_cc: 0,
            pat_cc: 0,
            pmt_cc: 0,
            pcr: 0,
            output: Vec::new(),
            pat_pmt_emitted: false,
            last_was_keyframe: false,
        }
    }

    #[wasm_bindgen(js_name = pushVideo)]
    pub fn push_video(&mut self, data: &[u8], pts_us: i64, dts_us: i64, is_keyframe: bool) {
        let pts_90k = us_to_90k(pts_us);
        let dts_90k = us_to_90k(dts_us);
        self.pcr = pts_90k.wrapping_mul(300);

        if is_keyframe {
            self.write_pat();
            self.write_pmt();
            self.pat_pmt_emitted = true;
        }
        self.last_was_keyframe = is_keyframe;

        let pes = build_pes_video(data, pts_90k, dts_90k);
        let pcr_base = if is_keyframe { Some(pts_90k) } else { None };
        packetize(
            &mut self.output,
            self.video_pid,
            &mut self.video_cc,
            &pes,
            pcr_base,
        );
    }

    #[wasm_bindgen(js_name = pushAudio)]
    pub fn push_audio(&mut self, data: &[u8], pts_us: i64) {
        let pts_90k = us_to_90k(pts_us);
        let pes = build_pes_audio(data, pts_90k);
        packetize(
            &mut self.output,
            self.audio_pid,
            &mut self.audio_cc,
            &pes,
            None,
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
            0x00,                       // table_id
            0xB0, 0x0D,                 // SSI + section_length = 13
            0x00, 0x01,                 // transport_stream_id = 1
            0xC1,                       // reserved(11) + version(0) + current_next(1)
            0x00, 0x00,                 // section_number, last_section_number
            0x00, 0x01,                 // program_number = 1
            0xF0, 0x00,                 // reserved(111) + PMT_PID = 0x1000
        ];
        let crc = crc32(&section);
        section.extend_from_slice(&crc.to_be_bytes());

        let mut pkt = [0u8; TS_PACKET_SIZE];
        pkt[0] = SYNC_BYTE;
        pkt[1] = 0x40 | ((PAT_PID >> 8) as u8 & 0x1F); // PUSI=1
        pkt[2] = (PAT_PID & 0xFF) as u8;
        pkt[3] = 0x10 | (self.pat_cc & 0x0F); // AFC=01 (payload only)
        self.pat_cc = (self.pat_cc + 1) & 0x0F;

        pkt[4] = 0x00; // pointer_field
        pkt[5..5 + section.len()].copy_from_slice(&section);
        for byte in &mut pkt[5 + section.len()..] {
            *byte = 0xFF;
        }
        self.output.extend_from_slice(&pkt);
    }

    fn write_pmt(&mut self) {
        let mut section: Vec<u8> = vec![
            0x02,                       // table_id
            0xB0, 0x17,                 // SSI + section_length = 23
            0x00, 0x01,                 // program_number = 1
            0xC1,                       // reserved(11) + version(0) + current_next(1)
            0x00, 0x00,                 // section_number, last_section_number
            0xE1, 0x00,                 // reserved(111) + PCR_PID = 0x100
            0xF0, 0x00,                 // reserved(1111) + program_info_length = 0
            STREAM_TYPE_H264, 0xE1, 0x00, 0xF0, 0x00, // video: PID 0x100, ES_info_length 0
            STREAM_TYPE_AAC,  0xE1, 0x01, 0xF0, 0x00, // audio: PID 0x101, ES_info_length 0
        ];
        let crc = crc32(&section);
        section.extend_from_slice(&crc.to_be_bytes());

        let mut pkt = [0u8; TS_PACKET_SIZE];
        pkt[0] = SYNC_BYTE;
        pkt[1] = 0x40 | ((self.pmt_pid >> 8) as u8 & 0x1F); // PUSI=1
        pkt[2] = (self.pmt_pid & 0xFF) as u8;
        pkt[3] = 0x10 | (self.pmt_cc & 0x0F); // AFC=01 (payload only)
        self.pmt_cc = (self.pmt_cc + 1) & 0x0F;

        pkt[4] = 0x00; // pointer_field
        pkt[5..5 + section.len()].copy_from_slice(&section);
        for byte in &mut pkt[5 + section.len()..] {
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
) {
    let total = data.len();
    if total == 0 {
        return;
    }

    let mut offset = 0usize;
    let mut first = true;
    while offset < total {
        let pcr_on_this = first && pcr_base.is_some();
        // adaptation field overhead for PCR: 1 length byte + 1 flags byte + 6 PCR bytes
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
            // AFC = 11 (adaptation field + payload)
            pkt[3] = 0x30 | (*cc & 0x0F);
            *cc = (*cc + 1) & 0x0F;

            let af_total = TS_PACKET_SIZE - 4 - chunk;
            let af_length = af_total - 1;
            pkt[pos] = af_length as u8;
            pos += 1;

            if af_length >= 1 {
                let mut flags = 0u8;
                if pcr_overhead > 0 {
                    flags |= 0x40; // random_access_indicator
                    flags |= 0x10; // PCR flag
                }
                pkt[pos] = flags;
                pos += 1;

                if pcr_overhead > 0 {
                    write_pcr(&mut pkt[pos..pos + 6], pcr_base.unwrap(), 0);
                    pos += 6;
                }
                // stuffing
                for byte in &mut pkt[pos..TS_PACKET_SIZE - chunk] {
                    *byte = 0xFF;
                }
            }
            pos = TS_PACKET_SIZE - chunk;
        } else {
            // AFC = 01 (payload only)
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
    pes.extend_from_slice(&[0x00, 0x00, 0x01, 0xE0]); // start code + stream_id (video)
    pes.extend_from_slice(&[0x00, 0x00]); // PES_packet_length = 0 (unbounded)
    pes.push(0x80); // flags1: MPEG-2
    pes.push(flags2); // flags2: PTS only / PTS+DTS
    pes.push(header_len); // PES_header_data_length
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
    pes.extend_from_slice(&[0x00, 0x00, 0x01, 0xC0]); // start code + stream_id (audio)
    pes.extend_from_slice(&pes_packet_length.to_be_bytes());
    pes.push(0x80); // flags1: MPEG-2
    pes.push(0x80); // flags2: PTS only
    pes.push(0x05); // PES_header_data_length
    pes.extend_from_slice(&encode_pts(pts_90k, 0b0010));
    pes.extend_from_slice(data);
    pes
}

fn encode_pts(value: u64, prefix: u8) -> [u8; 5] {
    let v = value & 0x1FFFFFFFF; // 33 bits
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

fn us_to_90k(us: i64) -> u64 {
    if us <= 0 {
        0
    } else {
        (us as u64) * 9 / 100
    }
}

impl Default for TsMuxer {
    fn default() -> Self {
        Self::new()
    }
}
