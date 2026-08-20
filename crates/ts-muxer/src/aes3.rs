//! SMPTE 302M (AES3-in-MPEG-2-TS) encoder.
//!
//! Encodes interleaved f32 PCM into AES3 frames packed in PES payloads per
//! SMPTE 302M, matching FFmpeg's s302m decoder byte layout (bit-reversed,
//! 7/6/5 bytes per stereo pair for 24/20/16-bit). Moved from `mpeg2ts-wasm`
//! so the native muxer (`ts-muxer`) and the browser share one encode
//! implementation; the decode direction still lives in `mpeg2ts-wasm`.

/// Build the 4-byte AES3 header for a SMPTE 302M PES payload.
fn build_aes3_header(frame_size: u16, channel_count: u8, bits_per_sample: u8) -> [u8; 4] {
    let ch_idx = ((channel_count as u32 - 2) / 2) & 0x3;
    let bits_idx = ((bits_per_sample as u32 - 16) / 4) & 0x3;
    let h: u32 = ((frame_size as u32) << 16) | (ch_idx << 14) | (bits_idx << 4);
    h.to_be_bytes()
}

/// Encode interleaved f32 samples into a SMPTE 302M PES payload.
///
/// `samples`: interleaved f32 in [-1.0, 1.0)
/// `channel_count`: 2, 4, 6, or 8
/// `bit_depth`: 16, 20, or 24 (default 24)
/// Returns: 4-byte AES3 header + bit-reversed AES3 data (ready for PES packaging).
/// Clips out-of-range samples to [-1.0, 1.0).
pub fn wrap_smpte302m_pes(samples: &[f32], channel_count: u8, bit_depth: u8) -> Vec<u8> {
    if !matches!(channel_count, 2 | 4 | 6 | 8) || !matches!(bit_depth, 16 | 20 | 24) {
        return Vec::new();
    }
    let block_size = match bit_depth {
        24 => 7usize,
        20 => 6,
        _ => 5,
    };
    let pairs = samples.len() / 2;
    let data_len = pairs * block_size;
    let mut out = Vec::with_capacity(4 + data_len);
    out.extend_from_slice(&build_aes3_header(
        data_len as u16,
        channel_count,
        bit_depth,
    ));

    match bit_depth {
        24 => {
            for pair in samples.chunks_exact(2) {
                let s1 = (clip_f32(pair[0]) * 8_388_608.0) as i32 & 0xFFFFFF;
                let s2 = (clip_f32(pair[1]) * 8_388_608.0) as i32 & 0xFFFFFF;
                let s1_u32 = (s1 as u32) << 8;
                let s2_u32 = (s2 as u32) << 8;
                out.push(reverse_byte((s1_u32 >> 8) as u8));
                out.push(reverse_byte((s1_u32 >> 16) as u8));
                out.push(reverse_byte((s1_u32 >> 24) as u8));
                let s2_top = ((s2_u32 >> 28) & 0x0F) as u8;
                let s2_b = ((s2_u32 >> 20) & 0xFF) as u8;
                let s2_c = ((s2_u32 >> 12) & 0xFF) as u8;
                let s2_bot = ((s2_u32 >> 8) & 0x0F) as u8;
                out.push(reverse_byte(s2_bot) >> 4);
                out.push(reverse_byte(s2_c));
                out.push(reverse_byte(s2_b));
                out.push(reverse_byte(s2_top));
            }
        }
        20 => {
            for pair in samples.chunks_exact(2) {
                let s1 = ((clip_f32(pair[0]) * 524_288.0) as i32 & 0xFFFFF) as u32;
                let s2 = ((clip_f32(pair[1]) * 524_288.0) as i32 & 0xFFFFF) as u32;
                let s1_u32 = s1 << 12;
                let s2_u32 = s2 << 12;
                out.push(reverse_byte((s1_u32 >> 12) as u8));
                out.push(reverse_byte((s1_u32 >> 20) as u8));
                out.push(reverse_byte(((s1_u32 >> 28) & 0x0F) as u8));
                out.push(reverse_byte((s2_u32 >> 12) as u8));
                out.push(reverse_byte((s2_u32 >> 20) as u8));
                out.push(reverse_byte(((s2_u32 >> 28) & 0x0F) as u8));
            }
        }
        _ => {
            for pair in samples.chunks_exact(2) {
                let s1 = (clip_f32(pair[0]) * 32768.0) as i16 as u16;
                let s2 = (clip_f32(pair[1]) * 32768.0) as i16 as u16;
                out.push(reverse_byte((s1 & 0xFF) as u8));
                out.push(reverse_byte((s1 >> 8) as u8));
                out.push(reverse_byte((s2 & 0x0F) as u8) >> 4);
                out.push(reverse_byte(((s2 >> 4) & 0xFF) as u8));
                out.push(reverse_byte((s2 >> 12) as u8));
            }
        }
    }
    out
}

fn clip_f32(s: f32) -> f32 {
    s.clamp(-1.0, 0.99999994)
}

fn reverse_byte(mut b: u8) -> u8 {
    b = ((b & 0xF0) >> 4) | ((b & 0x0F) << 4);
    b = ((b & 0xCC) >> 2) | ((b & 0x33) << 2);
    b = ((b & 0xAA) >> 1) | ((b & 0x55) << 1);
    b
}
