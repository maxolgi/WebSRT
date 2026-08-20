//! Dump synthetic MPEG-TS streams from the muxer for offline conformance
//! analysis (TSDuck `tsanalyze`, `ffprobe`).
//!
//! Usage: `cargo run -p ts-muxer-wasm --example dump_ts -- <scenario> <out.ts>`
//!
//! Scenarios:
//! - `h264_opus` — 30 s H.264 + Opus, 60 fps, 2 s GOP (default publisher path)
//! - `s302m_64`  — 1 s audio-only, 64 s302m stereo PIDs 0x101..0x140
//!                 (the layout that once panicked long-PMT packetization)
//! - `sparse`    — s302m with sparse suppression: loud / silent / loud
//!
//! Payload bytes are deterministic (xorshift64*); NAL units are structurally
//! valid Annex B (start codes + correct nal_unit_type) but do not decode.
//! Analysis is container-level: PSI, CC continuity, PCR, PES, adaptation.

use std::collections::BTreeMap;
use std::env;
use std::fs;

use ts_muxer_wasm::TsMuxer;

const TS_PACKET_SIZE: usize = 188;

struct Rng(u64);

impl Rng {
    fn next_u64(&mut self) -> u64 {
        let mut x = self.0;
        x ^= x >> 12;
        x ^= x << 25;
        x ^= x >> 27;
        self.0 = x;
        x.wrapping_mul(0x2545_F491_4F6C_DD1D)
    }

    fn fill(&mut self, len: usize) -> Vec<u8> {
        (0..len).map(|_| (self.next_u64() >> 33) as u8).collect()
    }
}

fn annex_b(nal_header: u8, payload: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(5 + payload.len());
    out.extend_from_slice(&[0x00, 0x00, 0x00, 0x01, nal_header]);
    out.extend_from_slice(payload);
    out
}

fn stereo_samples(frames: usize, amp: f32) -> Vec<f32> {
    let mut out = Vec::with_capacity(frames * 2);
    for s in 0..frames {
        let v = if (s / 8) % 2 == 0 { amp } else { -amp };
        out.push(v);
        out.push(v);
    }
    out
}

fn drain(m: &mut TsMuxer, stream: &mut Vec<u8>) {
    loop {
        let chunk = m.poll();
        if chunk.is_empty() {
            break;
        }
        assert_eq!(
            chunk.len() % TS_PACKET_SIZE,
            0,
            "poll() must return whole TS packets"
        );
        stream.extend_from_slice(&chunk);
    }
}

/// 30 s H.264 + Opus, 60 fps, 2 s GOP. DTS == PTS (no B-frames), matching
/// the browser publisher (`stream-worker.ts` pushes chunk.timestamp twice).
fn scenario_h264_opus(stream: &mut Vec<u8>) {
    const FPS: u64 = 60;
    const GOP: u64 = 120;
    const FRAMES: u64 = FPS * 30;

    let mut m = TsMuxer::new();
    let mut rng = Rng(0x00C0_FFEE_1234_5678);
    let mut audio_idx: u64 = 0;

    for i in 0..FRAMES {
        let pts_us = (i * 1_000_000 / FPS) as f64;
        let is_key = i % GOP == 0;
        let mut frame = Vec::new();
        if is_key {
            frame.extend(annex_b(0x67, &rng.fill(12))); // SPS
            frame.extend(annex_b(0x68, &rng.fill(4))); // PPS
            frame.extend(annex_b(0x65, &rng.fill(12_000))); // IDR
        } else {
            frame.extend(annex_b(0x41, &rng.fill(2_000))); // non-IDR
        }
        m.push_video(&frame, pts_us, pts_us, is_key);

        while (audio_idx * 20_000) <= i * 1_000_000 / FPS {
            m.push_audio(&rng.fill(280), (audio_idx * 20_000) as f64);
            audio_idx += 1;
        }
        drain(&mut m, stream);
    }
}

/// 1 s audio-only, 64 s302m stereo PIDs (0x101 via push_pcm, rest via
/// push_pcm_pid) — exercises the multi-packet PMT path.
fn scenario_s302m_64(stream: &mut Vec<u8>) {
    let mut m = TsMuxer::new();
    m.set_video_enabled(false);
    m.set_audio_codec("s302m", 2);
    for pid in 0x102..=0x140u16 {
        m.add_audio_pid(pid, "s302m", 2);
    }
    let samples = stereo_samples(960, 0.25); // 20 ms @ 48 kHz
    for t in 0..50u64 {
        let pts = (t * 20_000) as f64;
        m.push_pcm(&samples, pts);
        for pid in 0x102..=0x140u16 {
            m.push_pcm_pid(pid, &samples, pts);
        }
        drain(&mut m, stream);
    }
}

/// s302m with sparse suppression: 500 ms loud, 400 ms silent, 200 ms loud —
/// captures the PMT drop / re-add / resend burst in the file.
fn scenario_sparse(stream: &mut Vec<u8>) {
    let mut m = TsMuxer::new();
    m.set_video_enabled(false);
    m.set_audio_codec("s302m", 2);
    m.set_sparse_enabled(true);
    m.set_sparse_threshold(100.0);
    let loud = stereo_samples(960, 0.5);
    let silent = stereo_samples(960, 0.0);
    for t in 0..55u64 {
        let pts = (t * 20_000) as f64;
        let samples = if t < 25 {
            &loud
        } else if t < 45 {
            &silent
        } else {
            &loud
        };
        m.push_pcm(samples, pts);
        drain(&mut m, stream);
    }
}

fn main() {
    let args: Vec<String> = env::args().collect();
    let scenario = args.get(1).map(String::as_str).unwrap_or("h264_opus");
    let out_path = args
        .get(2)
        .cloned()
        .unwrap_or_else(|| format!("/tmp/opencode/dump_{scenario}.ts"));

    let mut stream = Vec::new();
    match scenario {
        "h264_opus" => scenario_h264_opus(&mut stream),
        "s302m_64" => scenario_s302m_64(&mut stream),
        "sparse" => scenario_sparse(&mut stream),
        other => {
            eprintln!("unknown scenario: {other} (h264_opus | s302m_64 | sparse)");
            std::process::exit(2);
        }
    };

    let mut pid_counts: BTreeMap<u16, u64> = BTreeMap::new();
    for chunk in stream.chunks(TS_PACKET_SIZE) {
        let pid = ((chunk[1] as u16 & 0x1F) << 8) | chunk[2] as u16;
        *pid_counts.entry(pid).or_default() += 1;
    }

    fs::write(&out_path, &stream).expect("write output");
    eprintln!(
        "wrote {out_path}: {} packets, {} bytes",
        stream.len() / TS_PACKET_SIZE,
        stream.len()
    );
    for (pid, count) in &pid_counts {
        eprintln!("  PID 0x{pid:04X}: {count} packets");
    }
}
