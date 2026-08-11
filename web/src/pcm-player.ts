// Phase 0 spike PCM player.
//
// Receives interleaved f32 PCM from the worker (kind=5 events emitted by the
// mpeg2ts-wasm demuxer for SMPTE 302M PIDs), plays it through an AudioWorklet.
//
// Single-PID for the spike. Phase 2 will generalize to a per-PID Map feeding
// the WASM mixer (see audioplan.md).
//
// The worklet also computes a full audio metering suite (peak/RMS/clip,
// K-weighted LUFS, stereo phase correlation, 1024-point FFT spectrum,
// vectorscope) and posts it back at ~30fps for the AudioTab debug panel.

import type { AudioMeterData } from './shared/types';

const PCM_PLAYER_WORKLET = `
class PcmPlayerProcessor extends AudioWorkletProcessor {
  constructor() {
    super();
    this.CAP = 24000;
    this.rings = [];
    this.heads = [];
    this.tails = [];
    this.counts = [];
    this.channelCount = 2;
    this.selectedChannel = 0;

    // Tier 1: peak / RMS / clip
    this.peakHold = new Float32Array(0);
    this.sumSq = new Float32Array(0);
    this.sampleCount = new Int32Array(0);
    this.clipCount = new Uint32Array(0);
    this.underrunCount = 0;

    // Tier 2: K-weighting (ITU-R BS.1770-4 at 48kHz), two biquads per channel
    this.kFilter1 = null;   // [{x1,x2,y1,y2} per channel]
    this.kFilter2 = null;
    this.LUFS_WIN = 19200;  // 400ms at 48kHz
    this.lufsRing = null;   // [Float32Array(LUFS_WIN) per channel]
    this.lufsRingIdx = null;
    this.lufsSumSq = null;
    this.lufsRingFill = null;

    // Tier 2: phase correlation per stereo pair
    this.sumLR = null;
    this.sumLL = null;
    this.sumRR = null;

    // Tier 3: 1024-point FFT (Cooley-Tukey, twiddles precomputed)
    this.FFT_N = 1024;
    this.fftRing = new Float32Array(this.FFT_N);
    this.fftRingIdx = 0;
    this.fftReal = new Float32Array(this.FFT_N);
    this.fftImag = new Float32Array(this.FFT_N);
    this.twiddleCos = new Float32Array(this.FFT_N / 2);
    this.twiddleSin = new Float32Array(this.FFT_N / 2);
    for (let i = 0; i < this.FFT_N / 2; i++) {
      const angle = -2 * Math.PI * i / this.FFT_N;
      this.twiddleCos[i] = Math.cos(angle);
      this.twiddleSin[i] = Math.sin(angle);
    }
    // Precompute 64 log-spaced output bin edges over FFT bins [1, 511].
    this.fftBinEdges = new Int32Array(65);
    this.fftBinEdges[0] = 1;
    for (let i = 1; i <= 64; i++) {
      const edge = Math.floor(Math.pow(512, i / 64));
      this.fftBinEdges[i] = Math.max(this.fftBinEdges[i - 1] + 1, edge);
    }
    this.fftBinEdges[64] = 512;

    // Tier 3: vectorscope (256 samples of selected channel + partner)
    this.SCOPE_N = 256;
    this.scopeLRing = new Float32Array(this.SCOPE_N);
    this.scopeRRing = new Float32Array(this.SCOPE_N);
    this.scopeIdx = 0;

    // ~30fps meter post: process() runs at 128 samples/call = 375 calls/sec.
    // 375 / 30 ~= 12.5 -> post every 12 calls.
    this.frameCounter = 0;
    this.METER_FRAMES = 12;

    this.port.onmessage = (e) => {
      const msg = e.data;
      if (msg && msg.type === 'selectChannel') {
        this.selectedChannel = msg.channel | 0;
        // Flush stale scope/FFT data from the previously-selected channel.
        this.fftRing.fill(0);
        this.fftRingIdx = 0;
        this.scopeLRing.fill(0);
        this.scopeRRing.fill(0);
        this.scopeIdx = 0;
        return;
      }
      const samples = msg.samples;
      const ch = msg.channelCount || 2;
      if (this.rings.length !== ch) {
        this.channelCount = ch;
        this.rings = [];
        this.heads = [];
        this.tails = [];
        this.counts = [];
        for (let i = 0; i < ch; i++) {
          this.rings.push(new Float32Array(this.CAP));
          this.heads.push(0);
          this.tails.push(0);
          this.counts.push(0);
        }
        this.allocateMetering(ch);
      }
      const frames = (samples.length / ch) | 0;
      for (let i = 0; i < frames; i++) {
        for (let c = 0; c < ch; c++) {
          const tail = this.tails[c];
          const ring = this.rings[c];
          if (this.counts[c] >= this.CAP) {
            this.heads[c] = (this.heads[c] + 1) % this.CAP;
            this.counts[c]--;
          }
          ring[tail] = samples[i * ch + c];
          this.tails[c] = (tail + 1) % this.CAP;
          this.counts[c]++;
        }
      }
    };
  }

  allocateMetering(ch) {
    this.peakHold = new Float32Array(ch);
    this.sumSq = new Float32Array(ch);
    this.sampleCount = new Int32Array(ch);
    this.clipCount = new Uint32Array(ch);

    this.kFilter1 = new Array(ch);
    this.kFilter2 = new Array(ch);
    for (let i = 0; i < ch; i++) {
      this.kFilter1[i] = { x1: 0, x2: 0, y1: 0, y2: 0 };
      this.kFilter2[i] = { x1: 0, x2: 0, y1: 0, y2: 0 };
    }

    this.lufsRing = new Array(ch);
    for (let i = 0; i < ch; i++) {
      this.lufsRing[i] = new Float32Array(this.LUFS_WIN);
    }
    this.lufsRingIdx = new Int32Array(ch);
    this.lufsSumSq = new Float64Array(ch);
    this.lufsRingFill = new Int32Array(ch);

    const pairs = (ch + 1) >> 1;
    this.sumLR = new Float64Array(pairs);
    this.sumLL = new Float64Array(pairs);
    this.sumRR = new Float64Array(pairs);
  }

  process(inputs, outputs) {
    const output = outputs[0];
    const framesNeeded = output[0].length;
    const ch = output.length;

    // Defensive: metering not allocated yet (no audio received). Just zero
    // output. Once the first feed arrives, allocateMetering() sized everything
    // to ch and this branch stops firing.
    if (this.peakHold.length !== ch) {
      for (let c = 0; c < ch; c++) {
        for (let i = 0; i < framesNeeded; i++) output[c][i] = 0;
      }
      return true;
    }

    // K-weighting biquad coefficients (ITU-R BS.1770-4 at 48kHz).
    // y[n] = a0*x[n] + a1*x[n-1] + a2*x[n-2] - b1*y[n-1] - b2*y[n-2]
    const F1_A0 = 1.53512485958697;
    const F1_A1 = -2.69169618940638;
    const F1_A2 = 1.19839281085285;
    const F1_B1 = -1.69065929318241;
    const F1_B2 = 0.73248077421585;
    const F2_A0 = 1.0;
    const F2_A1 = -2.0;
    const F2_A2 = 1.0;
    const F2_B1 = -1.99004745483398;
    const F2_B2 = 0.99007225036621;
    const LUFS_WIN = this.LUFS_WIN;

    let hadUnderrun = false;

    for (let c = 0; c < ch; c++) {
      const ring = this.rings[c];
      let head = this.heads[c];
      let count = this.counts[c];
      if (count > framesNeeded + 2400) {
        const skip = count - framesNeeded - 2400;
        head = (head + skip) % this.CAP;
        count -= skip;
      }
      const toRead = Math.min(framesNeeded, count);
      if (toRead < framesNeeded) hadUnderrun = true;

      const out = output[c];
      const kFilter1 = this.kFilter1[c];
      const kFilter2 = this.kFilter2[c];
      const lufsRing = this.lufsRing[c];
      let lufsRingIdx = this.lufsRingIdx[c];
      let lufsSumSq = this.lufsSumSq[c];
      let lufsRingFill = this.lufsRingFill[c];
      let sCount = this.sampleCount[c];
      let sumSq = this.sumSq[c];
      let pk = this.peakHold[c];
      let clips = this.clipCount[c];

      let f1x1 = kFilter1.x1, f1x2 = kFilter1.x2, f1y1 = kFilter1.y1, f1y2 = kFilter1.y2;
      let f2x1 = kFilter2.x1, f2x2 = kFilter2.x2, f2y1 = kFilter2.y1, f2y2 = kFilter2.y2;

      for (let i = 0; i < framesNeeded; i++) {
        const played = i < toRead ? ring[head] : 0;
        out[i] = played;
        if (i < toRead) head = (head + 1) % this.CAP;

        // Tier 1: peak / RMS / clip
        const absVal = played >= 0 ? played : -played;
        if (absVal > pk) pk = absVal;
        sumSq += played * played;
        sCount++;
        if (absVal >= 0.999) clips++;

        // Tier 2 filter 1 (high shelf) — input is the played sample
        const y1_new = F1_A0 * played + F1_A1 * f1x1 + F1_A2 * f1x2 - F1_B1 * f1y1 - F1_B2 * f1y2;
        f1x2 = f1x1; f1x1 = played;
        f1y2 = f1y1; f1y1 = y1_new;

        // Tier 2 filter 2 (high pass) — input is output of filter 1
        const y2_new = F2_A0 * y1_new + F2_A1 * f2x1 + F2_A2 * f2x2 - F2_B1 * f2y1 - F2_B2 * f2y2;
        f2x2 = f2x1; f2x1 = y1_new;
        f2y2 = f2y1; f2y1 = y2_new;

        // Tier 2 LUFS sliding window — track sum of squares of K-weighted
        // output. Subtract evicted sample, add new sample (running sum avoids
        // rescanning the 19200-sample ring on every post).
        const oldVal = lufsRing[lufsRingIdx];
        lufsSumSq += y2_new * y2_new - oldVal * oldVal;
        lufsRing[lufsRingIdx] = y2_new;
        lufsRingIdx++;
        if (lufsRingIdx >= LUFS_WIN) lufsRingIdx = 0;
        if (lufsRingFill < LUFS_WIN) lufsRingFill++;
      }

      kFilter1.x1 = f1x1; kFilter1.x2 = f1x2; kFilter1.y1 = f1y1; kFilter1.y2 = f1y2;
      kFilter2.x1 = f2x1; kFilter2.x2 = f2x2; kFilter2.y1 = f2y1; kFilter2.y2 = f2y2;

      this.sampleCount[c] = sCount;
      this.sumSq[c] = sumSq;
      this.peakHold[c] = pk;
      this.clipCount[c] = clips;
      this.lufsRingIdx[c] = lufsRingIdx;
      this.lufsSumSq[c] = lufsSumSq;
      this.lufsRingFill[c] = lufsRingFill;
      this.heads[c] = head;
      this.counts[c] = count - toRead;
    }

    if (hadUnderrun) this.underrunCount++;

    // Tier 2 phase correlation per stereo pair (0+1, 2+3, ...).
    const pairs = (ch + 1) >> 1;
    for (let p = 0; p < pairs; p++) {
      const cL = p * 2;
      const cR = p * 2 + 1;
      if (cR >= ch) break;
      const outL = output[cL];
      const outR = output[cR];
      let lr = 0, ll = 0, rr = 0;
      for (let i = 0; i < framesNeeded; i++) {
        const l = outL[i];
        const r = outR[i];
        lr += l * r;
        ll += l * l;
        rr += r * r;
      }
      this.sumLR[p] += lr;
      this.sumLL[p] += ll;
      this.sumRR[p] += rr;
    }

    // Tier 3 FFT/scope rings for the selected channel + its stereo partner.
    const selCh = (this.selectedChannel >= 0 && this.selectedChannel < ch)
      ? this.selectedChannel : 0;
    const partnerCh = ((selCh & 1) === 0) ? selCh + 1 : selCh - 1;
    const selOut = output[selCh];
    const partnerOut = (partnerCh >= 0 && partnerCh < ch) ? output[partnerCh] : selOut;
    const N = this.FFT_N;
    const SC = this.SCOPE_N;
    let fftIdx = this.fftRingIdx;
    let scopeIdx = this.scopeIdx;
    for (let i = 0; i < framesNeeded; i++) {
      this.fftRing[fftIdx] = selOut[i];
      fftIdx++;
      if (fftIdx >= N) fftIdx = 0;
      this.scopeLRing[scopeIdx] = selOut[i];
      this.scopeRRing[scopeIdx] = partnerOut[i];
      scopeIdx++;
      if (scopeIdx >= SC) scopeIdx = 0;
    }
    this.fftRingIdx = fftIdx;
    this.scopeIdx = scopeIdx;

    // ~30fps meter post.
    this.frameCounter++;
    if (this.frameCounter >= this.METER_FRAMES) {
      this.frameCounter = 0;
      this.postMeter(ch);
      // Reset windowed accumulators (peak/RMS/phase). Clip count, underrun
      // count, LUFS ring, and FFT/scope rings are cumulative or sliding.
      this.peakHold.fill(0);
      this.sumSq.fill(0);
      this.sampleCount.fill(0);
      this.sumLR.fill(0);
      this.sumLL.fill(0);
      this.sumRR.fill(0);
    }

    return true;
  }

  postMeter(ch) {
    // Per-channel RMS and momentary LUFS.
    const rms = new Float32Array(ch);
    const lufs = new Float32Array(ch);
    for (let c = 0; c < ch; c++) {
      const n = this.sampleCount[c];
      rms[c] = n > 0 ? Math.sqrt(this.sumSq[c] / n) : 0;
      const fill = this.lufsRingFill[c];
      const ms = fill > 0 ? this.lufsSumSq[c] / fill : 0;
      lufs[c] = -0.691 + 10 * Math.log10(ms + 1e-12);
    }

    // Per-pair phase correlation, clamped to [-1, 1].
    const pairs = (ch + 1) >> 1;
    const phase = new Float32Array(pairs);
    for (let p = 0; p < pairs; p++) {
      const cR = p * 2 + 1;
      if (cR >= ch) {
        phase[p] = 1;
        continue;
      }
      let corr = this.sumLR[p] / Math.sqrt(this.sumLL[p] * this.sumRR[p] + 1e-12);
      if (corr > 1) corr = 1;
      if (corr < -1) corr = -1;
      phase[p] = corr;
    }

    // Buffer fill ratio across all channel rings.
    let totalQueued = 0;
    for (let c = 0; c < ch; c++) totalQueued += this.counts[c];
    const cap = ch * this.CAP;
    const bufferFill = cap > 0 ? totalQueued / cap : 0;

    // Scope: oldest-to-newest 256 samples from the per-channel 256 rings.
    const scopeL = new Float32Array(this.SCOPE_N);
    const scopeR = new Float32Array(this.SCOPE_N);
    let startIdx = this.scopeIdx;
    for (let i = 0; i < this.SCOPE_N; i++) {
      scopeL[i] = this.scopeLRing[startIdx];
      scopeR[i] = this.scopeRRing[startIdx];
      startIdx++;
      if (startIdx >= this.SCOPE_N) startIdx = 0;
    }

    const spectrum = this.computeSpectrum();

    const msg = {
      type: 'meter',
      peaks: new Float32Array(this.peakHold),
      rms: rms,
      clips: new Uint32Array(this.clipCount),
      bufferFill: bufferFill,
      underruns: this.underrunCount,
      channelCount: ch,
      sampleRate: 48000,
      lufs: lufs,
      phase: phase,
      scopeL: scopeL,
      scopeR: scopeR,
      spectrum: spectrum,
      selectedChannel: this.selectedChannel,
    };
    // Transfer ArrayBuffers to avoid structured-clone copies of typed arrays.
    this.port.postMessage(msg, [
      msg.peaks.buffer, msg.rms.buffer, msg.clips.buffer,
      msg.lufs.buffer, msg.phase.buffer, msg.scopeL.buffer,
      msg.scopeR.buffer, msg.spectrum.buffer,
    ]);
  }

  computeSpectrum() {
    const N = this.FFT_N;
    const real = this.fftReal;
    const imag = this.fftImag;
    // Read FFT ring in chronological order, apply Hann window.
    let readIdx = this.fftRingIdx;
    for (let i = 0; i < N; i++) {
      const w = 0.5 - 0.5 * Math.cos(2 * Math.PI * i / (N - 1));
      real[i] = this.fftRing[readIdx] * w;
      imag[i] = 0;
      readIdx++;
      if (readIdx >= N) readIdx = 0;
    }
    this.fftInPlace(real, imag);
    // Bin 512 useful FFT magnitudes into 64 log-spaced output bins (max per bin).
    const spectrum = new Float32Array(64);
    for (let i = 0; i < 64; i++) {
      const lo = this.fftBinEdges[i];
      const hi = this.fftBinEdges[i + 1];
      let maxMag = 0;
      for (let b = lo; b < hi; b++) {
        const mag = Math.sqrt(real[b] * real[b] + imag[b] * imag[b]);
        if (mag > maxMag) maxMag = mag;
      }
      spectrum[i] = 20 * Math.log10(maxMag + 1e-12);
    }
    return spectrum;
  }

  // In-place radix-2 Cooley-Tukey FFT. N must be a power of 2. Twiddle factors
  // are looked up from this.twiddleCos/Sin (indexed by k * N/L per stage L).
  fftInPlace(real, imag) {
    const N = real.length;
    // Bit-reversal permutation.
    for (let i = 1, j = 0; i < N; i++) {
      let bit = N >> 1;
      for (; j & bit; bit >>= 1) {
        j ^= bit;
      }
      j ^= bit;
      if (i < j) {
        const tr = real[i]; real[i] = real[j]; real[j] = tr;
        const ti = imag[i]; imag[i] = imag[j]; imag[j] = ti;
      }
    }
    // Butterfly stages: L = 2, 4, 8, ..., N.
    for (let len = 2; len <= N; len <<= 1) {
      const halfLen = len >> 1;
      const step = N / len;
      for (let i = 0; i < N; i += len) {
        for (let k = 0; k < halfLen; k++) {
          const wIdx = k * step;
          const wReal = this.twiddleCos[wIdx];
          const wImag = this.twiddleSin[wIdx];
          const idx1 = i + k;
          const idx2 = i + k + halfLen;
          const aReal = real[idx1];
          const aImag = imag[idx1];
          const bReal = real[idx2] * wReal - imag[idx2] * wImag;
          const bImag = real[idx2] * wImag + imag[idx2] * wReal;
          real[idx1] = aReal + bReal;
          imag[idx1] = aImag + bImag;
          real[idx2] = aReal - bReal;
          imag[idx2] = aImag - bImag;
        }
      }
    }
  }
}
registerProcessor('pcm-player', PcmPlayerProcessor);
`;

export class PcmPlayer {
  private audioCtx: AudioContext | null = null;
  private workletNode: AudioWorkletNode | null = null;
  private initPromise: Promise<void> | null = null;
  private sampleRate = 48000;
  private droppedPackets = 0;
  // Named onMeterCb (not onMeter) so the public onMeter(cb) registrar method
  // can coexist with the stored callback in the same class.
  private onMeterCb: ((data: AudioMeterData) => void) | null = null;
  private meterCallback: ((e: MessageEvent) => void) | null = null;

  get ready(): boolean {
    return this.workletNode !== null;
  }

  async init(channelCount: number): Promise<void> {
    if (this.initPromise) return this.initPromise;
    this.initPromise = this.doInit(channelCount);
    return this.initPromise;
  }

  private async doInit(channelCount: number): Promise<void> {
    const Ctx = window.AudioContext
      || (window as unknown as Window & { webkitAudioContext: typeof AudioContext }).webkitAudioContext;
    this.audioCtx = new Ctx({ sampleRate: this.sampleRate });
    if (this.audioCtx.state === 'suspended') {
      await this.audioCtx.resume().catch(() => {});
    }
    const blob = new Blob([PCM_PLAYER_WORKLET], { type: 'application/javascript' });
    await this.audioCtx.audioWorklet.addModule(URL.createObjectURL(blob));
    this.workletNode = new AudioWorkletNode(this.audioCtx, 'pcm-player', {
      numberOfInputs: 0,
      numberOfOutputs: 1,
      outputChannelCount: [channelCount],
    });
    this.meterCallback = (e: MessageEvent) => {
      this.onMeterCb?.(e.data as AudioMeterData);
    };
    this.workletNode.port.onmessage = this.meterCallback;
    this.workletNode.connect(this.audioCtx.destination);
  }

  feed(samples: Float32Array, channelCount: number): void {
    if (!this.workletNode) {
      this.droppedPackets++;
      return;
    }
    this.workletNode.port.postMessage({ samples, channelCount });
  }

  /** Register a meter-data callback invoked at ~30fps from the worklet. */
  onMeter(cb: (data: AudioMeterData) => void): void {
    this.onMeterCb = cb;
  }

  /** Select which channel feeds the FFT spectrum + vectorscope (0-based). */
  setSelectedChannel(ch: number): void {
    if (!this.workletNode) return;
    this.workletNode.port.postMessage({ type: 'selectChannel', channel: ch });
  }

  async resume(): Promise<void> {
    if (this.audioCtx?.state === 'suspended') {
      await this.audioCtx.resume();
    }
  }

  dispose(): void {
    try { this.workletNode?.disconnect(); } catch {}
    try { this.audioCtx?.close(); } catch {}
    this.workletNode = null;
    this.audioCtx = null;
    this.initPromise = null;
    this.meterCallback = null;
  }
}
