//! `ts-muxer-wasm` — wasm32 MPEG-TS muxer binding for the browser.
//!
//! Thin forwarding shim over the native `ts-muxer` crate, which owns all
//! muxing logic. Takes H.264/HEVC/AV1 NAL units (Annex B) and audio packets
//! and produces 188-byte ISO/IEC 13818-1 MPEG-TS packets. JS drives it by
//! pushing encoded chunks via `push_video` / `push_audio` / `push_pcm` and
//! draining finished packets via `poll`.

use ts_muxer::TsMuxer as NativeTsMuxer;
use wasm_bindgen::prelude::*;

#[wasm_bindgen]
pub struct TsMuxer {
    inner: NativeTsMuxer,
}

#[wasm_bindgen]
impl TsMuxer {
    #[wasm_bindgen(constructor)]
    pub fn new() -> TsMuxer {
        TsMuxer {
            inner: NativeTsMuxer::new(),
        }
    }

    #[wasm_bindgen(js_name = setVideoCodec)]
    pub fn set_video_codec(&mut self, codec: &str) {
        self.inner.set_video_codec(codec);
    }

    #[wasm_bindgen(js_name = setVideoEnabled)]
    pub fn set_video_enabled(&mut self, enabled: bool) {
        self.inner.set_video_enabled(enabled);
    }

    #[wasm_bindgen(js_name = setAudioCodec)]
    pub fn set_audio_codec(&mut self, codec: &str, channel_count: u8) {
        self.inner.set_audio_codec(codec, channel_count);
    }

    #[wasm_bindgen(js_name = addAudioPid)]
    pub fn add_audio_pid(&mut self, pid: u16, codec: &str, channel_count: u8) {
        self.inner.add_audio_pid(pid, codec, channel_count);
    }

    /// Opt in to sparse (silence) suppression. Off by default: suppression
    /// mutates the PMT (drops silent PIDs) and resets CC on resume, and has
    /// never run against a real muxer source or receiver chain.
    #[wasm_bindgen(js_name = setSparseEnabled)]
    pub fn set_sparse_enabled(&mut self, enabled: bool) {
        self.inner.set_sparse_enabled(enabled);
    }

    #[wasm_bindgen(js_name = setSparseThreshold)]
    pub fn set_sparse_threshold(&mut self, ms: f64) {
        self.inner.set_sparse_threshold(ms);
    }

    #[wasm_bindgen(js_name = push_video)]
    pub fn push_video(&mut self, data: &[u8], pts_us: f64, dts_us: f64, is_keyframe: bool) {
        self.inner.push_video(data, pts_us, dts_us, is_keyframe);
    }

    #[wasm_bindgen(js_name = push_audio)]
    pub fn push_audio(&mut self, data: &[u8], pts_us: f64) {
        self.inner.push_audio(data, pts_us);
    }

    #[wasm_bindgen(js_name = push_pcm)]
    pub fn push_pcm(&mut self, samples: &[f32], pts_us: f64) {
        self.inner.push_pcm(samples, pts_us);
    }

    /// Like `push_pcm`, but targets the s302m stream carrying `pid`
    /// (registered via `addAudioPid`). Unknown pids and non-s302m streams
    /// are a no-op.
    #[wasm_bindgen(js_name = push_pcm_pid)]
    pub fn push_pcm_pid(&mut self, pid: u16, samples: &[f32], pts_us: f64) {
        self.inner.push_pcm_pid(pid, samples, pts_us);
    }

    #[wasm_bindgen(js_name = poll)]
    pub fn poll(&mut self) -> Vec<u8> {
        self.inner.poll()
    }
}

#[cfg(test)]
mod tests {
    use mpeg2ts_wasm::aes3::{parse_aes3_header, unwrap_smpte302m_pes};
    use ts_muxer::aes3::wrap_smpte302m_pes;

    use super::*;

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

    // AES3 encode↔decode roundtrip tests. Encode (wrap_smpte302m_pes)
    // lives in ts-muxer, decode (unwrap_smpte302m_pes) in mpeg2ts-wasm;
    // this crate sees both, so the roundtrip tests live here.

    #[test]
    fn test_wrap_zero_samples_24bit() {
        let samples = vec![0.0_f32; 4];
        let payload = wrap_smpte302m_pes(&samples, 2, 24);
        assert!(payload.len() > 4);
        let decoded = unwrap_smpte302m_pes(&payload);
        assert_eq!(decoded.len(), 4);
        for s in &decoded {
            assert!(s.abs() < 1e-6);
        }
    }

    #[test]
    fn test_wrap_round_trip_24bit() {
        let samples: Vec<f32> = vec![
            0.0, 0.5, -0.5, 0.99999988, -1.0, 0.25, -0.25, 0.0, 0.1, -0.1,
        ];
        let payload = wrap_smpte302m_pes(&samples, 2, 24);
        assert!(payload.len() > 4);
        let decoded = unwrap_smpte302m_pes(&payload);
        assert_eq!(decoded.len(), samples.len());
        for (i, (&orig, &got)) in samples.iter().zip(decoded.iter()).enumerate() {
            assert!(
                (orig - got).abs() < 1e-5,
                "sample {i}: orig {orig:.6}, got {got:.6}, diff {:.8}",
                (orig - got).abs()
            );
        }
    }

    #[test]
    fn test_wrap_round_trip_20bit() {
        let samples: Vec<f32> = vec![0.0, 0.5, -0.5, 0.999, -0.999, 0.1];
        let payload = wrap_smpte302m_pes(&samples, 2, 20);
        assert!(payload.len() > 4);
        let decoded = unwrap_smpte302m_pes(&payload);
        assert_eq!(decoded.len(), samples.len());
        for (i, (&orig, &got)) in samples.iter().zip(decoded.iter()).enumerate() {
            assert!(
                (orig - got).abs() < 1e-4,
                "20-bit sample {i}: orig {orig:.6}, got {got:.6}"
            );
        }
    }

    #[test]
    fn test_wrap_round_trip_16bit() {
        let samples: Vec<f32> = vec![0.0, 0.5, -0.5, 0.99, -0.99, 0.1, -0.1, 0.0];
        let payload = wrap_smpte302m_pes(&samples, 2, 16);
        assert!(payload.len() > 4);
        let decoded = unwrap_smpte302m_pes(&payload);
        assert_eq!(decoded.len(), samples.len());
        for (i, (&orig, &got)) in samples.iter().zip(decoded.iter()).enumerate() {
            assert!(
                (orig - got).abs() < 1e-3,
                "16-bit sample {i}: orig {orig:.6}, got {got:.6}"
            );
        }
    }

    #[test]
    fn test_wrap_clips_out_of_range() {
        let samples = vec![2.0_f32, -2.0, 10.0, -10.0];
        let payload = wrap_smpte302m_pes(&samples, 2, 24);
        let decoded = unwrap_smpte302m_pes(&payload);
        assert_eq!(decoded.len(), 4);
        assert!(
            (decoded[0] - 0.99999988).abs() < 1e-5,
            "should clip to max positive"
        );
        assert!(
            (decoded[1] - (-1.0)).abs() < 1e-5,
            "should clip to max negative"
        );
    }

    #[test]
    fn test_wrap_invalid_params() {
        assert!(wrap_smpte302m_pes(&[0.0_f32], 1, 24).is_empty());
        assert!(wrap_smpte302m_pes(&[0.0_f32], 3, 24).is_empty());
        assert!(wrap_smpte302m_pes(&[0.0_f32], 2, 32).is_empty());
    }

    #[test]
    fn test_wrap_header_fields() {
        let payload = wrap_smpte302m_pes(&[0.0_f32; 16], 2, 24);
        let header = parse_aes3_header(&payload).expect("header should parse");
        assert_eq!(header.channel_count, 2);
        assert_eq!(header.bits_per_sample, 24);
        assert_eq!(header.frame_size as usize, 8 * 7);
    }
}
