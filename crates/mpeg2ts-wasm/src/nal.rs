//! NAL-unit analysis for H.264 (Annex B) and HEVC byte streams.
//!
//! Used by the demuxer to classify slice types (I/P/B), count NAL units
//! (AUD/SPS/PPS/SEI/IDR) and produce per-packet NAL-type summaries for the
//! debug snapshot. Operates directly on reassembled PES payloads.
//!
//! Emulation prevention bytes (`00 00 03`) are NOT stripped before RBSP
//! parsing — we only read the first 1-2 exp-Golomb codes of a slice header,
//! which sit well before any realistic emulation byte. This keeps the reader
//! allocation-free and panic-free on truncated input.

/// MSB-first bit reader over a borrowed byte slice. All methods return
/// `None` on end-of-input rather than panicking, so malformed NAL units are
/// silently skipped instead of aborting demux.
struct BitReader<'a> {
    bytes: &'a [u8],
    bit_pos: usize,
}

impl<'a> BitReader<'a> {
    fn new(bytes: &'a [u8]) -> Self {
        BitReader { bytes, bit_pos: 0 }
    }

    fn read_bit(&mut self) -> Option<u8> {
        let byte_idx = self.bit_pos / 8;
        if byte_idx >= self.bytes.len() {
            return None;
        }
        let bit_idx = 7 - (self.bit_pos % 8);
        let bit = (self.bytes[byte_idx] >> bit_idx) & 1;
        self.bit_pos += 1;
        Some(bit)
    }

    /// Unsigned exp-Golomb (`ue(v)` per ITU-T H.264 §9.1 / H.265 §9.1).
    fn read_ue(&mut self) -> Option<u32> {
        let mut leading_zeros = 0u32;
        loop {
            if leading_zeros >= 32 {
                return None;
            }
            let bit = self.read_bit()?;
            if bit == 1 {
                break;
            }
            leading_zeros += 1;
        }
        if leading_zeros == 0 {
            return Some(0);
        }
        let suffix = self.read_u(leading_zeros)?;
        Some((1u32 << leading_zeros) - 1 + suffix)
    }

    /// Fixed-length `n`-bit unsigned.
    fn read_u(&mut self, n: u32) -> Option<u32> {
        let mut val = 0u32;
        for _ in 0..n {
            val = (val << 1) | (self.read_bit()? as u32);
        }
        Some(val)
    }
}

/// Per-video-PID accumulated NAL counts. Mirrored into `DebugSnapshot.nal_stats`
/// as a flat 9-element block per PID in the order I/P/B/IDR/SPS/PPS/SEI/AUD/NonIDR.
#[derive(Default, Clone)]
pub struct NalStats {
    pub aud: u32,
    pub sps: u32,
    pub pps: u32,
    pub sei: u32,
    pub idr: u32,
    pub non_idr_slice: u32,
    pub i_slices: u32,
    pub p_slices: u32,
    pub b_slices: u32,
}

/// Scan Annex B start codes (`00 00 01` / `00 00 00 01`) and yield
/// `(data_start, data_end)` ranges for each NAL unit, excluding the start
/// code prefix. Consecutive NALs do not overlap and the last NAL runs to
/// the end of `payload`.
fn scan_nal_ranges(payload: &[u8]) -> Vec<(usize, usize)> {
    let n = payload.len();
    let mut code_begins: Vec<usize> = Vec::new();
    let mut code_lens: Vec<usize> = Vec::new();
    let mut i = 0;
    while i + 3 <= n {
        if payload[i] == 0 && payload[i + 1] == 0 && payload[i + 2] == 1 {
            code_begins.push(i);
            code_lens.push(3);
            i += 3;
            continue;
        }
        if i + 4 <= n
            && payload[i] == 0
            && payload[i + 1] == 0
            && payload[i + 2] == 0
            && payload[i + 3] == 1
        {
            code_begins.push(i);
            code_lens.push(4);
            i += 4;
            continue;
        }
        i += 1;
    }
    let mut ranges = Vec::with_capacity(code_begins.len());
    for j in 0..code_begins.len() {
        let data_start = code_begins[j] + code_lens[j];
        let data_end = if j + 1 < code_begins.len() {
            code_begins[j + 1]
        } else {
            n
        };
        if data_end > data_start {
            ranges.push((data_start, data_end));
        }
    }
    ranges
}

/// Walk NAL units and return their raw type codes (H.264: low 5 bits of byte 0;
/// HEVC: bits 1..6 of byte 0). Compact per-packet summary, e.g. `[9, 7, 8, 5]`
/// for `AUD SPS PPS IDR`.
pub fn nal_summary(payload: &[u8], is_hevc: bool) -> Vec<u8> {
    let mut out = Vec::new();
    for (start, end) in scan_nal_ranges(payload) {
        if start >= end {
            continue;
        }
        let nal_type = if is_hevc {
            (payload[start] >> 1) & 0x3F
        } else {
            payload[start] & 0x1F
        };
        out.push(nal_type);
    }
    out
}

/// Parse a PES payload (Annex B byte stream) and accumulate NAL + slice-type
/// statistics. Never panics: malformed/truncated NALs are counted as
/// `non_idr_slice` without slice classification.
///
/// `is_hevc`: `false` for `stream_type 0x1b` (H.264), `true` for `0x24` (HEVC).
pub fn parse_nal_stats(payload: &[u8], is_hevc: bool) -> NalStats {
    let mut stats = NalStats::default();
    for (start, end) in scan_nal_ranges(payload) {
        let nal = &payload[start..end];
        if nal.is_empty() {
            continue;
        }
        if is_hevc {
            if nal.len() < 2 {
                continue;
            }
            // forbidden(1) | nal_type(6) | nuh_layer_id(6) | nuh_temporal_id_plus1(3)
            let nal_type = (nal[0] >> 1) & 0x3F;
            match nal_type {
                32 => stats.pps += 1, // VPS — folded into pps per spec
                33 => stats.sps += 1,
                34 => stats.pps += 1,
                39 | 40 => stats.sei += 1,
                19 | 20 => {
                    stats.idr += 1;
                    stats.i_slices += 1;
                }
                0 | 1 => {
                    stats.non_idr_slice += 1;
                    // slice_segment_header: pps_id(ue), then slice_type(ue).
                    // HEVC slice_type is 0=B, 1=P, 2=I (no mod).
                    let mut reader = BitReader::new(&nal[2..]);
                    if reader.read_ue().is_some() {
                        if let Some(st) = reader.read_ue() {
                            match st {
                                0 => stats.b_slices += 1,
                                1 => stats.p_slices += 1,
                                2 => stats.i_slices += 1,
                                _ => {}
                            }
                        }
                    }
                }
                _ => {}
            }
        } else {
            // forbidden(1) | nal_ref_idc(2) | nal_unit_type(5)
            let nal_type = nal[0] & 0x1F;
            match nal_type {
                1 => {
                    stats.non_idr_slice += 1;
                    // slice_header: first_mb_in_slice(ue), slice_type(ue).
                    // slice_type % 5: 0=P, 1=B, 2=I, 3=SP, 4=SI (ITU-T H.264 §7.4.3).
                    let mut reader = BitReader::new(&nal[1..]);
                    if reader.read_ue().is_some() {
                        if let Some(st) = reader.read_ue() {
                            match st % 5 {
                                0 => stats.p_slices += 1,
                                1 => stats.b_slices += 1,
                                2 => stats.i_slices += 1,
                                _ => {}
                            }
                        }
                    }
                }
                5 => {
                    stats.idr += 1;
                    stats.i_slices += 1;
                }
                6 => stats.sei += 1,
                7 => stats.sps += 1,
                8 => stats.pps += 1,
                9 => stats.aud += 1,
                _ => {}
            }
        }
    }
    stats
}
