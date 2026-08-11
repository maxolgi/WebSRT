//! SMPTE 302M (AES3-in-MPEG-2-TS) unwrapper.
//!
//! Extracts PCM samples from AES3 frames packed in PES payloads per SMPTE 302M.
//! Matches FFmpeg's s302m decoder byte layout (bit-reversed, 7/6/5 bytes per
//! stereo pair for 24/20/16-bit). Used by the demuxer to emit PCM events.

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Aes3Header {
    pub frame_size: u16,
    pub channel_count: u8,
    pub bits_per_sample: u8,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct S302mInfo {
    pub channel_count: u8,
    pub sample_rate: u32,
    pub bit_depth: u8,
}

pub const S302M_STREAM_TYPE: u8 = 0x06;
pub const REGISTRATION_DESC_TAG: u8 = 0x05;
/// SMPTE 302M registration identifier (4-byte body of registration_descriptor).
/// Per SMPTE ST 302M and ffmpeg's s302m muxer: "BSSD" (Broadcast Serial Sound Data).
pub const S302M_REGISTRATION_ID: [u8; 4] = *b"BSSD";

pub fn is_s302m_registration(desc_body: &[u8]) -> bool {
    desc_body == S302M_REGISTRATION_ID.as_slice()
}

pub fn parse_aes3_header(payload: &[u8]) -> Option<Aes3Header> {
    if payload.len() < 4 {
        return None;
    }
    let h = u32::from_be_bytes([payload[0], payload[1], payload[2], payload[3]]);
    let frame_size = ((h >> 16) & 0xFFFF) as u16;
    let channel_count = (((h >> 14) & 0x3) * 2 + 2) as u8;
    let bits_per_sample = (((h >> 4) & 0x3) * 4 + 16) as u8;
    if matches!(bits_per_sample, 16 | 20 | 24) && matches!(channel_count, 2 | 4 | 6 | 8) {
        Some(Aes3Header {
            frame_size,
            channel_count,
            bits_per_sample,
        })
    } else {
        None
    }
}

pub fn s302m_info_from_header(payload: &[u8]) -> Option<S302mInfo> {
    let h = parse_aes3_header(payload)?;
    Some(S302mInfo {
        channel_count: h.channel_count,
        sample_rate: 48_000,
        bit_depth: h.bits_per_sample,
    })
}

fn reverse_byte(mut b: u8) -> u8 {
    b = ((b & 0xF0) >> 4) | ((b & 0x0F) << 4);
    b = ((b & 0xCC) >> 2) | ((b & 0x33) << 2);
    b = ((b & 0xAA) >> 1) | ((b & 0x55) << 1);
    b
}

const SCALE_24: f32 = 1.0 / 8_388_608.0;
const SCALE_20: f32 = 1.0 / 524_288.0;
const SCALE_16: f32 = 1.0 / 32_768.0;

pub fn unwrap_smpte302m_pes(payload: &[u8]) -> Vec<f32> {
    let Some(header) = parse_aes3_header(payload) else {
        return Vec::new();
    };
    let data = &payload[4..];
    let mut out: Vec<f32> = Vec::with_capacity(data.len() / 3);
    match header.bits_per_sample {
        24 => {
            for chunk in data.chunks_exact(7) {
                let s1_u32: u32 = ((reverse_byte(chunk[2]) as u32) << 24)
                    | ((reverse_byte(chunk[1]) as u32) << 16)
                    | ((reverse_byte(chunk[0]) as u32) << 8);
                let s2_u32: u32 = ((reverse_byte(chunk[6] & 0xF0) as u32) << 28)
                    | ((reverse_byte(chunk[5]) as u32) << 20)
                    | ((reverse_byte(chunk[4]) as u32) << 12)
                    | ((reverse_byte(chunk[3] & 0x0F) as u32) << 4);
                out.push(((s1_u32 as i32) >> 8) as f32 * SCALE_24);
                out.push(((s2_u32 as i32) >> 8) as f32 * SCALE_24);
            }
        }
        20 => {
            for chunk in data.chunks_exact(6) {
                let s1_u32: u32 = ((reverse_byte(chunk[2] & 0xF0) as u32) << 28)
                    | ((reverse_byte(chunk[1]) as u32) << 20)
                    | ((reverse_byte(chunk[0]) as u32) << 12);
                let s2_u32: u32 = ((reverse_byte(chunk[5] & 0xF0) as u32) << 28)
                    | ((reverse_byte(chunk[4]) as u32) << 20)
                    | ((reverse_byte(chunk[3]) as u32) << 12);
                out.push(((s1_u32 as i32) >> 12) as f32 * SCALE_20);
                out.push(((s2_u32 as i32) >> 12) as f32 * SCALE_20);
            }
        }
        16 => {
            for chunk in data.chunks_exact(5) {
                let s1_u16: u16 = ((reverse_byte(chunk[1]) as u16) << 8)
                    | (reverse_byte(chunk[0]) as u16);
                let s2_u16: u16 = ((reverse_byte(chunk[4] & 0xF0) as u16) << 12)
                    | ((reverse_byte(chunk[3]) as u16) << 4)
                    | ((reverse_byte(chunk[2]) as u16) >> 4);
                out.push((s1_u16 as i16) as f32 * SCALE_16);
                out.push((s2_u16 as i16) as f32 * SCALE_16);
            }
        }
        _ => {}
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_reverse_byte() {
        assert_eq!(reverse_byte(0x00), 0x00);
        assert_eq!(reverse_byte(0xFF), 0xFF);
        assert_eq!(reverse_byte(0x01), 0x80);
        assert_eq!(reverse_byte(0x80), 0x01);
        assert_eq!(reverse_byte(0xF0), 0x0F);
        assert_eq!(reverse_byte(0x0F), 0xF0);
        assert_eq!(reverse_byte(0xAA), 0x55);
        assert_eq!(reverse_byte(0x55), 0xAA);
        assert_eq!(reverse_byte(0x7F), 0xFE);
        assert_eq!(reverse_byte(0x39), 0x9C);
    }

    #[test]
    fn test_reverse_byte_self_inverse() {
        for b in [0x00u8, 0x01, 0x12, 0x55, 0x7F, 0x80, 0xAA, 0xFF, 0x39, 0xDE] {
            assert_eq!(reverse_byte(reverse_byte(b)), b, "not self-inverse for {b:#04x}");
        }
    }

    #[test]
    fn test_header_parse_stereo_24bit() {
        let frame_size: u32 = 1024;
        let h: u32 = (frame_size << 16) | (0u32 << 14) | (2u32 << 4);
        let payload = h.to_be_bytes();
        let parsed = parse_aes3_header(&payload).expect("header should parse");
        assert_eq!(parsed.frame_size, 1024);
        assert_eq!(parsed.channel_count, 2);
        assert_eq!(parsed.bits_per_sample, 24);
    }

    #[test]
    fn test_header_parse_six_channels_20bit() {
        let h: u32 = (512u32 << 16) | (2u32 << 14) | (1u32 << 4);
        let payload = h.to_be_bytes();
        let parsed = parse_aes3_header(&payload).expect("header should parse");
        assert_eq!(parsed.frame_size, 512);
        assert_eq!(parsed.channel_count, 6);
        assert_eq!(parsed.bits_per_sample, 20);
    }

    #[test]
    fn test_header_parse_eight_channels_16bit() {
        let h: u32 = (256u32 << 16) | (3u32 << 14) | (0u32 << 4);
        let payload = h.to_be_bytes();
        let parsed = parse_aes3_header(&payload).expect("header should parse");
        assert_eq!(parsed.frame_size, 256);
        assert_eq!(parsed.channel_count, 8);
        assert_eq!(parsed.bits_per_sample, 16);
    }

    #[test]
    fn test_header_too_short() {
        assert!(parse_aes3_header(&[0u8, 0, 0]).is_none());
        assert!(parse_aes3_header(&[]).is_none());
    }

    #[test]
    fn test_info_from_header() {
        let h: u32 = (8u32 << 16) | (0u32 << 14) | (2u32 << 4);
        let payload = h.to_be_bytes();
        let info = s302m_info_from_header(&payload).expect("info should parse");
        assert_eq!(
            info,
            S302mInfo {
                channel_count: 2,
                sample_rate: 48_000,
                bit_depth: 24,
            }
        );
    }

    fn make_header(channel_pair_index: u8, bits_index: u8, frame_size: u16) -> Vec<u8> {
        let h: u32 = ((frame_size as u32) << 16)
            | ((channel_pair_index as u32 & 0x3) << 14)
            | ((bits_index as u32 & 0x3) << 4);
        h.to_be_bytes().to_vec()
    }

    fn encode_chunk_24(s1: i32, s2: i32) -> [u8; 7] {
        let s1_u32 = (s1 as u32).wrapping_shl(8);
        let s2_u32 = (s2 as u32).wrapping_shl(8);
        let c0 = reverse_byte((s1_u32 >> 8) as u8);
        let c1 = reverse_byte((s1_u32 >> 16) as u8);
        let c2 = reverse_byte((s1_u32 >> 24) as u8);
        let s2_top = ((s2_u32 >> 28) & 0x0F) as u8;
        let s2_b = ((s2_u32 >> 20) & 0xFF) as u8;
        let s2_c = ((s2_u32 >> 12) & 0xFF) as u8;
        let s2_bot = ((s2_u32 >> 8) & 0x0F) as u8;
        let c6 = reverse_byte(s2_top);
        let c5 = reverse_byte(s2_b);
        let c4 = reverse_byte(s2_c);
        let c3 = reverse_byte(s2_bot) >> 4;
        [c0, c1, c2, c3, c4, c5, c6]
    }

    #[test]
    fn test_unwrap_zero_payload_24bit() {
        let mut payload = make_header(0, 2, 7);
        payload.extend_from_slice(&encode_chunk_24(0, 0));
        let out = unwrap_smpte302m_pes(&payload);
        assert_eq!(out.len(), 2);
        assert_eq!(out, vec![0.0, 0.0]);
    }

    #[test]
    fn test_unwrap_full_scale_positive_24bit() {
        let max_pos = 0x7FFFFF_i32;
        let mut payload = make_header(0, 2, 7);
        payload.extend_from_slice(&encode_chunk_24(max_pos, max_pos));
        let out = unwrap_smpte302m_pes(&payload);
        assert_eq!(out.len(), 2);
        assert!((out[0] - 0.99999988).abs() < 1e-6, "got {}", out[0]);
        assert!((out[1] - 0.99999988).abs() < 1e-6, "got {}", out[1]);
    }

    #[test]
    fn test_unwrap_full_scale_negative_24bit() {
        let max_neg = -0x800000_i32;
        let mut payload = make_header(0, 2, 7);
        payload.extend_from_slice(&encode_chunk_24(max_neg, max_neg));
        let out = unwrap_smpte302m_pes(&payload);
        assert_eq!(out.len(), 2);
        assert!((out[0] - (-1.0)).abs() < 1e-6, "got {}", out[0]);
        assert!((out[1] - (-1.0)).abs() < 1e-6, "got {}", out[1]);
    }

    #[test]
    fn test_unwrap_half_scale_positive_24bit() {
        let half = 0x400000_i32;
        let mut payload = make_header(0, 2, 7);
        payload.extend_from_slice(&encode_chunk_24(half, -half));
        let out = unwrap_smpte302m_pes(&payload);
        assert_eq!(out.len(), 2);
        assert!((out[0] - 0.5).abs() < 1e-6, "got {}", out[0]);
        assert!((out[1] - (-0.5)).abs() < 1e-6, "got {}", out[1]);
    }

    #[test]
    fn test_unwrap_two_stereo_pairs_24bit() {
        let mut payload = make_header(0, 2, 14);
        payload.extend_from_slice(&encode_chunk_24(0x123456, 0x789ABC));
        payload.extend_from_slice(&encode_chunk_24(-0x100000, 0x200000));
        let out = unwrap_smpte302m_pes(&payload);
        assert_eq!(out.len(), 4);
        assert!((out[0] - (0x123456 as f32 / 8_388_608.0)).abs() < 1e-6);
        assert!((out[1] - (0x789ABC as f32 / 8_388_608.0)).abs() < 1e-6);
        assert!((out[2] - (-0x100000 as f32 / 8_388_608.0)).abs() < 1e-6);
        assert!((out[3] - (0x200000 as f32 / 8_388_608.0)).abs() < 1e-6);
    }

    #[test]
    fn test_unwrap_truncated_data_24bit() {
        let mut payload = make_header(0, 2, 7);
        payload.extend_from_slice(&[0u8, 0, 0]);
        let out = unwrap_smpte302m_pes(&payload);
        assert!(out.is_empty(), "trailing partial chunk should be dropped");
    }

    #[test]
    fn test_unwrap_bad_header_returns_empty() {
        let h: u32 = (8u32 << 16) | (0u32 << 14) | (3u32 << 4);
        let mut payload = h.to_be_bytes().to_vec();
        payload.extend_from_slice(&[0u8; 7]);
        let out = unwrap_smpte302m_pes(&payload);
        assert!(out.is_empty(), "invalid bits_per_sample should yield no samples");
    }

    #[test]
    fn test_unwrap_empty_payload() {
        let out = unwrap_smpte302m_pes(&[]);
        assert!(out.is_empty());
    }

    #[test]
    fn test_round_trip_random_samples_24bit() {
        let samples = [
            0x000000_i32, 0x7FFFFF, -0x800000, 0x400000, -0x400000, 0x123456,
            -0x123456, 0x7F0000, -0x010000, 0x00ABCD, -0x7FABCD, 0x111111,
        ];
        let mut payload = make_header(0, 2, (samples.len() / 2 * 7) as u16);
        for pair in samples.chunks_exact(2) {
            payload.extend_from_slice(&encode_chunk_24(pair[0], pair[1]));
        }
        let out = unwrap_smpte302m_pes(&payload);
        assert_eq!(out.len(), samples.len());
        for (i, (&orig, &decoded)) in samples.iter().zip(out.iter()).enumerate() {
            let expected = orig as f32 / 8_388_608.0;
            assert!(
                (decoded - expected).abs() < 1e-6,
                "sample {i}: expected {expected}, got {decoded}"
            );
        }
    }

    #[test]
    fn test_is_s302m_registration() {
        assert!(is_s302m_registration(b"BSSD"));
        assert!(!is_s302m_registration(b"OPUS"));
        assert!(!is_s302m_registration(b"BSS"));
        assert!(!is_s302m_registration(b"BSSD!"));
    }
}
