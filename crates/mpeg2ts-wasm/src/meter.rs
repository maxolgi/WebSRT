//! Audio metering: per-channel peak/RMS, K-weighted LUFS, phase correlation,
//! and FFT spectrum — computed in Rust from decoded PCM samples.
//!
//! Owned by TsDemuxer, updated on every SMPTE 302M PES packet. JS polls
//! `meter_snapshot()` at ~30fps for compact results (no raw sample marshaling).

use std::collections::HashMap;

const SAMPLE_RATE: usize = 48000;
const LUFS_BLOCK_SAMPLES: usize = SAMPLE_RATE / 10;
const LUFS_BLOCKS: usize = 4;
const FFT_SIZE: usize = 1024;
const SCOPE_SIZE: usize = 256;

const KW1_A0: f64 = 1.53512485958697;
const KW1_A1: f64 = -2.69169618940638;
const KW1_A2: f64 = 1.19839281085285;
const KW1_B1: f64 = -1.69065929318241;
const KW1_B2: f64 = 0.73248077421585;

const KW2_A0: f64 = 1.0;
const KW2_A1: f64 = -2.0;
const KW2_A2: f64 = 1.0;
const KW2_B1: f64 = -1.99004745483398;
const KW2_B2: f64 = 0.99007225036621;

pub struct ChannelMeter {
    pub peak: f32,
    pub sum_sq: f64,
    pub sample_count: u64,
    pub clip_count: u32,
    /// Values latched at the end of the last window that had samples.
    /// Snapshot windows with no new audio (bursty PES arrival vs the fixed
    /// ~50ms poll) re-emit these instead of 0, so meters hold rather than flash.
    held_peak: f32,
    held_rms: f32,
    kw1_x1: f64,
    kw1_x2: f64,
    kw1_y1: f64,
    kw1_y2: f64,
    kw2_x1: f64,
    kw2_x2: f64,
    kw2_y1: f64,
    kw2_y2: f64,
    lufs_blocks: [f64; LUFS_BLOCKS],
    lufs_block_idx: usize,
    lufs_block_samples: usize,
    lufs_block_sum: f64,
}

impl Default for ChannelMeter {
    fn default() -> Self {
        Self {
            peak: 0.0,
            sum_sq: 0.0,
            sample_count: 0,
            clip_count: 0,
            held_peak: 0.0,
            held_rms: 0.0,
            kw1_x1: 0.0,
            kw1_x2: 0.0,
            kw1_y1: 0.0,
            kw1_y2: 0.0,
            kw2_x1: 0.0,
            kw2_x2: 0.0,
            kw2_y1: 0.0,
            kw2_y2: 0.0,
            lufs_blocks: [0.0; LUFS_BLOCKS],
            lufs_block_idx: 0,
            lufs_block_samples: 0,
            lufs_block_sum: 0.0,
        }
    }
}

impl ChannelMeter {
    fn update(&mut self, sample: f32) {
        let abs_s = sample.abs();
        if abs_s > self.peak {
            self.peak = abs_s;
        }
        self.sum_sq += (sample as f64) * (sample as f64);
        self.sample_count += 1;
        if abs_s >= 0.999 {
            self.clip_count += 1;
        }

        let x = sample as f64;
        let y1 = KW1_A0 * x + KW1_A1 * self.kw1_x1 + KW1_A2 * self.kw1_x2
            - KW1_B1 * self.kw1_y1
            - KW1_B2 * self.kw1_y2;
        self.kw1_x2 = self.kw1_x1;
        self.kw1_x1 = x;
        self.kw1_y2 = self.kw1_y1;
        self.kw1_y1 = y1;

        let y2 = KW2_A0 * y1 + KW2_A1 * self.kw2_x1 + KW2_A2 * self.kw2_x2
            - KW2_B1 * self.kw2_y1
            - KW2_B2 * self.kw2_y2;
        self.kw2_x2 = self.kw2_x1;
        self.kw2_x1 = y1;
        self.kw2_y2 = self.kw2_y1;
        self.kw2_y1 = y2;

        let weighted = y2 * y2;
        self.lufs_block_sum += weighted;
        self.lufs_block_samples += 1;
        if self.lufs_block_samples >= LUFS_BLOCK_SAMPLES {
            self.lufs_blocks[self.lufs_block_idx] = self.lufs_block_sum;
            self.lufs_block_idx = (self.lufs_block_idx + 1) % LUFS_BLOCKS;
            self.lufs_block_sum = 0.0;
            self.lufs_block_samples = 0;
        }
    }

    fn lufs(&self) -> f32 {
        let total: f64 = self.lufs_blocks.iter().sum();
        let window = (LUFS_BLOCKS * LUFS_BLOCK_SAMPLES) as f64;
        let mean_sq = total / window;
        if mean_sq < 1e-12 {
            return -70.0;
        }
        (-0.691 + 10.0 * mean_sq.log10()) as f32
    }

    fn rms(&self) -> f32 {
        if self.sample_count == 0 {
            return 0.0;
        }
        ((self.sum_sq / self.sample_count as f64).sqrt()) as f32
    }

    fn reset_window(&mut self) {
        self.peak = 0.0;
        self.sum_sq = 0.0;
        self.sample_count = 0;
    }
}

pub struct PhaseState {
    sum_lr: f64,
    sum_ll: f64,
    sum_rr: f64,
    count: u64,
    /// Correlation latched at the end of the last window that had samples.
    held: f32,
}

impl Default for PhaseState {
    fn default() -> Self {
        Self {
            sum_lr: 0.0,
            sum_ll: 0.0,
            sum_rr: 0.0,
            count: 0,
            held: 0.0,
        }
    }
}

impl PhaseState {
    fn update(&mut self, l: f32, r: f32) {
        let l = l as f64;
        let r = r as f64;
        self.sum_lr += l * r;
        self.sum_ll += l * l;
        self.sum_rr += r * r;
        self.count += 1;
    }

    fn correlation(&self) -> f32 {
        if self.count == 0 {
            return 0.0;
        }
        let denom = (self.sum_ll * self.sum_rr).sqrt() + 1e-12;
        let c = self.sum_lr / denom;
        c.clamp(-1.0, 1.0) as f32
    }

    fn reset(&mut self) {
        self.sum_lr = 0.0;
        self.sum_ll = 0.0;
        self.sum_rr = 0.0;
        self.count = 0;
    }
}

pub struct PidMeter {
    pub channel_count: u8,
    pub channels: Vec<ChannelMeter>,
    pub pes_count: u64,
    pub last_pts: i64,
    phase_pairs: Vec<PhaseState>,
    scope_ring: Vec<f32>,
    scope_idx: usize,
    fft_ring: Vec<f32>,
    fft_idx: usize,
}

impl PidMeter {
    fn new(channel_count: u8) -> Self {
        let pairs = (channel_count / 2) as usize;
        Self {
            channel_count,
            channels: (0..channel_count)
                .map(|_| ChannelMeter::default())
                .collect(),
            pes_count: 0,
            last_pts: -1,
            phase_pairs: (0..pairs).map(|_| PhaseState::default()).collect(),
            scope_ring: vec![0.0; SCOPE_SIZE],
            scope_idx: 0,
            fft_ring: vec![0.0; FFT_SIZE],
            fft_idx: 0,
        }
    }

    pub fn update(&mut self, samples: &[f32], pts: i64, selected: bool, sel_ch: u8) {
        self.pes_count += 1;
        self.last_pts = pts;
        let ch = self.channel_count as usize;
        if ch == 0 || samples.len() < ch {
            return;
        }
        let frames = samples.len() / ch;

        for i in 0..frames {
            let base = i * ch;
            for c in 0..ch {
                self.channels[c].update(samples[base + c]);
            }
            for (p, pair) in self.phase_pairs.iter_mut().enumerate() {
                let li = base + p * 2;
                let ri = li + 1;
                if ri < base + ch {
                    pair.update(samples[li], samples[ri]);
                }
            }
        }

        if selected && (sel_ch as usize) < ch {
            for i in 0..frames {
                let s = samples[i * ch + sel_ch as usize];
                self.scope_ring[self.scope_idx] = s;
                self.scope_idx = (self.scope_idx + 1) % SCOPE_SIZE;
                self.fft_ring[self.fft_idx] = s;
                self.fft_idx = (self.fft_idx + 1) % FFT_SIZE;
            }
        }
    }

    fn scope(&self) -> (Vec<f32>, Vec<f32>) {
        let mut l = Vec::with_capacity(SCOPE_SIZE);
        let mut r = Vec::with_capacity(SCOPE_SIZE);
        let ch = self.channel_count as usize;
        for i in 0..SCOPE_SIZE {
            let idx = (self.scope_idx + i) % SCOPE_SIZE;
            l.push(self.scope_ring[idx]);
            let partner = if ch > 1 { 1 } else { 0 };
            r.push(self.scope_ring[idx]);
            let _ = partner;
        }
        (l, r)
    }

    fn spectrum(&self) -> Vec<f32> {
        let mut re = [0.0f64; FFT_SIZE];
        let mut im = [0.0f64; FFT_SIZE];
        for i in 0..FFT_SIZE {
            let idx = (self.fft_idx + i) % FFT_SIZE;
            let w =
                0.5 - 0.5 * (2.0 * std::f64::consts::PI * i as f64 / (FFT_SIZE as f64 - 1.0)).cos();
            re[i] = self.fft_ring[idx] as f64 * w;
        }
        fft_inplace(&mut re, &mut im);

        let half = FFT_SIZE / 2;
        let mut mags = [0.0f64; 512];
        for i in 0..half {
            mags[i] = (re[i] * re[i] + im[i] * im[i]).sqrt();
        }

        let mut edges = [0usize; 65];
        edges[0] = 1;
        for b in 1..=64usize {
            let edge = (512f64.powf(b as f64 / 64.0)).floor() as usize;
            edges[b] = (edges[b - 1] + 1).max(edge);
        }
        edges[64] = 512;

        let mut bins = vec![-80.0f32; 64];
        for b in 0..64 {
            let lo = edges[b];
            let hi = edges[b + 1].max(lo + 1).min(half);
            let mut max_mag = 0.0f64;
            for i in lo..hi {
                if mags[i] > max_mag {
                    max_mag = mags[i];
                }
            }
            let db = 20.0 * (max_mag + 1e-12).log10();
            bins[b] = db as f32;
        }
        bins
    }
}

pub struct MeterSnapshot {
    pub pids: Vec<u16>,
    pub channel_counts: Vec<u8>,
    pub peaks: Vec<f32>,
    pub rms: Vec<f32>,
    pub clips: Vec<u32>,
    pub lufs: Vec<f32>,
    pub phase: Vec<f32>,
    pub scope_l: Vec<f32>,
    pub scope_r: Vec<f32>,
    pub spectrum: Vec<f32>,
    pub selected_pid: u16,
    pub selected_channel: u8,
}

pub struct MeterState {
    pub pids: HashMap<u16, PidMeter>,
    pub selected_pid: u16,
    pub selected_channel: u8,
}

impl Default for MeterState {
    fn default() -> Self {
        Self {
            pids: HashMap::new(),
            selected_pid: 0,
            selected_channel: 0,
        }
    }
}

impl MeterState {
    pub fn update(&mut self, pid: u16, samples: &[f32], channel_count: u8, pts: i64) {
        if self.selected_pid == 0 || !self.pids.contains_key(&self.selected_pid) {
            self.selected_pid = pid;
        }
        let selected = pid == self.selected_pid;
        let sel_ch = self.selected_channel;
        let meter = self
            .pids
            .entry(pid)
            .or_insert_with(|| PidMeter::new(channel_count));
        if meter.channel_count != channel_count {
            *meter = PidMeter::new(channel_count);
        }
        meter.update(samples, pts, selected, sel_ch);
    }

    pub fn snapshot(&mut self) -> MeterSnapshot {
        let mut pids: Vec<u16> = self.pids.keys().copied().collect();
        pids.sort_unstable();

        let mut channel_counts = Vec::new();
        let mut peaks = Vec::new();
        let mut rms = Vec::new();
        let mut clips = Vec::new();
        let mut lufs = Vec::new();
        let mut phase = Vec::new();

        for &pid in &pids {
            if let Some(meter) = self.pids.get_mut(&pid) {
                channel_counts.push(meter.channel_count);
                for ch in &mut meter.channels {
                    if ch.sample_count > 0 {
                        ch.held_peak = ch.peak;
                        ch.held_rms = ch.rms();
                        ch.reset_window();
                    }
                    peaks.push(ch.held_peak);
                    rms.push(ch.held_rms);
                    clips.push(ch.clip_count);
                    lufs.push(ch.lufs());
                }
                for pair in &mut meter.phase_pairs {
                    if pair.count > 0 {
                        pair.held = pair.correlation();
                        pair.reset();
                    }
                    phase.push(pair.held);
                }
            }
        }

        let scope_pid = if self.pids.contains_key(&self.selected_pid) {
            self.selected_pid
        } else {
            pids.first().copied().unwrap_or(0)
        };
        let (scope_l, scope_r, spectrum) = if let Some(meter) = self.pids.get(&scope_pid) {
            let (l, r) = meter.scope();
            (l, r, meter.spectrum())
        } else {
            (
                vec![0.0; SCOPE_SIZE],
                vec![0.0; SCOPE_SIZE],
                vec![-80.0; 64],
            )
        };

        MeterSnapshot {
            pids,
            channel_counts,
            peaks,
            rms,
            clips,
            lufs,
            phase,
            scope_l,
            scope_r,
            spectrum,
            selected_pid: self.selected_pid,
            selected_channel: self.selected_channel,
        }
    }
}

fn fft_inplace(re: &mut [f64; FFT_SIZE], im: &mut [f64; FFT_SIZE]) {
    let n = FFT_SIZE;
    let half = n / 2;

    let mut j = 0usize;
    for i in 1..n {
        let mut bit = half;
        while j & bit != 0 {
            j &= !bit;
            bit >>= 1;
        }
        j |= bit;
        if i < j {
            re.swap(i, j);
            im.swap(i, j);
        }
    }

    let mut len = 2;
    while len <= n {
        let half_len = len / 2;
        let angle = -2.0 * std::f64::consts::PI / len as f64;
        let w_re = angle.cos();
        let w_im = angle.sin();

        let mut i = 0;
        while i < n {
            let mut wr = 1.0f64;
            let mut wi = 0.0f64;
            for k in 0..half_len {
                let idx1 = i + k;
                let idx2 = i + k + half_len;
                let tr = wr * re[idx2] - wi * im[idx2];
                let ti = wr * im[idx2] + wi * re[idx2];
                re[idx2] = re[idx1] - tr;
                im[idx2] = im[idx1] - ti;
                re[idx1] += tr;
                im[idx1] += ti;
                let next_wr = wr * w_re - wi * w_im;
                wi = wr * w_im + wi * w_re;
                wr = next_wr;
            }
            i += len;
        }
        len <<= 1;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_peak_tracking() {
        let mut m = ChannelMeter::default();
        m.update(0.5);
        m.update(-0.8);
        m.update(0.3);
        assert!((m.peak - 0.8).abs() < 1e-6);
    }

    #[test]
    fn test_rms_of_sine() {
        let mut m = ChannelMeter::default();
        for i in 0..48000 {
            let s = (2.0 * std::f32::consts::PI * 1000.0 * i as f32 / 48000.0).sin() * 0.707;
            m.update(s);
        }
        let rms = m.rms();
        assert!(
            (rms - 0.5).abs() < 0.01,
            "RMS of 0.707 sine ≈ 0.5, got {rms}"
        );
    }

    #[test]
    fn test_clip_count() {
        let mut m = ChannelMeter::default();
        m.update(1.0);
        m.update(0.999);
        m.update(-1.0);
        m.update(0.5);
        assert_eq!(m.clip_count, 3);
    }

    #[test]
    fn test_kweight_lufs_nonnegligible() {
        let mut m = ChannelMeter::default();
        for i in 0..19200 {
            let s = (2.0 * std::f32::consts::PI * 1000.0 * i as f32 / 48000.0).sin() * 0.5;
            m.update(s);
        }
        let lufs = m.lufs();
        assert!(
            lufs > -40.0 && lufs < 0.0,
            "LUFS of 0.5 sine should be in a sane range, got {lufs}"
        );
    }

    #[test]
    fn test_phase_mono() {
        let mut p = PhaseState::default();
        for i in 0..1000 {
            let s = (i as f32 * 0.01).sin() * 0.5;
            p.update(s, s);
        }
        assert!((p.correlation() - 1.0).abs() < 1e-3, "mono → +1");
    }

    #[test]
    fn test_phase_antiphase() {
        let mut p = PhaseState::default();
        for i in 0..1000 {
            let s = (i as f32 * 0.01).sin() * 0.5;
            p.update(s, -s);
        }
        assert!((p.correlation() + 1.0).abs() < 1e-3, "anti-phase → -1");
    }

    #[test]
    fn test_scope_ring_order() {
        let mut m = PidMeter::new(2);
        let samples: Vec<f32> = (0..256).flat_map(|i| [i as f32 * 0.01, 0.0]).collect();
        m.update(&samples, 0, true, 0);
        let (l, _) = m.scope();
        for i in 0..255 {
            assert!(
                l[i] <= l[i + 1] + 1e-6,
                "scope should be in insertion order"
            );
        }
    }

    #[test]
    fn test_fft_peak_at_known_freq() {
        let mut m = PidMeter::new(2);
        let freq = 440.0f32;
        let mut samples = Vec::new();
        for i in 0..FFT_SIZE {
            let s = (2.0 * std::f32::consts::PI * freq * i as f32 / SAMPLE_RATE as f32).sin() * 0.8;
            samples.push(s);
            samples.push(0.0);
        }
        m.update(&samples, 0, true, 0);
        let bins = m.spectrum();
        let peak_bin = bins
            .iter()
            .enumerate()
            .max_by(|(_, a), (_, b)| a.partial_cmp(b).unwrap())
            .map(|(i, _)| i)
            .unwrap();
        assert!(
            peak_bin >= 1 && peak_bin <= 10,
            "440Hz peak should be in low bins (1-10), got bin {peak_bin}"
        );
    }

    #[test]
    fn test_snapshot_resets_peak() {
        let mut state = MeterState::default();
        state.update(0x100, &[0.5, -0.5, 0.3, -0.3], 2, 0);
        assert!((state.pids[&0x100].channels[0].peak - 0.5).abs() < 1e-6);
        let snap = state.snapshot();
        assert!(!snap.peaks.is_empty());
        assert!(
            (state.pids[&0x100].channels[0].peak - 0.0).abs() < 1e-6,
            "peak should reset after snapshot"
        );
    }

    #[test]
    fn test_snapshot_empty_window_holds_values() {
        let mut state = MeterState::default();
        state.update(0x100, &[0.5, -0.5, 0.3, -0.3], 2, 0);
        let first = state.snapshot();
        assert!((first.peaks[0] - 0.5).abs() < 1e-6);

        // No new PES since the last snapshot: values must hold, not drop to 0.
        let second = state.snapshot();
        assert!(
            (second.peaks[0] - 0.5).abs() < 1e-6,
            "empty window should hold peak, got {}",
            second.peaks[0]
        );
        assert!(second.rms[0] > 0.0, "empty window should hold rms");
        assert!(
            (second.phase[0] + 1.0).abs() < 1e-3,
            "empty window should hold phase correlation, got {}",
            second.phase[0]
        );

        // Genuine silence (samples arrive, all zero) must still read as 0.
        state.update(0x100, &[0.0, 0.0, 0.0, 0.0], 2, 0);
        let third = state.snapshot();
        assert!(third.peaks[0] == 0.0, "real silence should read 0 peak");
    }

    #[test]
    fn test_multi_pid_snapshot() {
        let mut state = MeterState::default();
        state.selected_pid = 0x101;
        state.update(0x100, &[0.1, 0.2, 0.3, 0.4], 2, 0);
        state.update(0x101, &[0.5, 0.6, 0.7, 0.8], 2, 0);
        let snap = state.snapshot();
        assert_eq!(snap.pids.len(), 2);
        assert_eq!(snap.peaks.len(), 4);
        assert_eq!(snap.phase.len(), 2);
        assert!(!snap.scope_l.is_empty());
        assert!(!snap.spectrum.is_empty());
    }
}
