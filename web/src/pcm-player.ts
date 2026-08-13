// Phase 0 spike PCM player.
//
// Receives interleaved f32 PCM from the worker (kind=5 events emitted by the
// mpeg2ts-wasm demuxer for SMPTE 302M PIDs), plays it through an AudioWorklet.
//
// Single-PID for the spike. Phase 2 will generalize to a per-PID Map feeding
// the WASM mixer (see audioplan.md).
//
// The worklet is playback-only: it deinterleaves incoming samples into
// per-channel ring buffers and copies them into the AudioWorklet outputs.
// Audio metering (peak/RMS/LUFS/phase/FFT) is computed in WASM (meter.rs) by
// the demuxer, not here.

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

    this.port.onmessage = (e) => {
      const msg = e.data;
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

  process(inputs, outputs) {
    const output = outputs[0];
    const framesNeeded = output[0].length;
    const ch = output.length;

    // Defensive: rings not allocated yet (no audio received). Zero output
    // until the first feed arrives and sizes the per-channel buffers.
    if (this.rings.length !== ch) {
      for (let c = 0; c < ch; c++) {
        for (let i = 0; i < framesNeeded; i++) output[c][i] = 0;
      }
      return true;
    }

    for (let c = 0; c < ch; c++) {
      const ring = this.rings[c];
      let head = this.heads[c];
      let count = this.counts[c];
      // Skip ahead if the ring has drifted far past one quantum + slack,
      // keeping latency bounded without dropping the whole buffer.
      if (count > framesNeeded + 2400) {
        const skip = count - framesNeeded - 2400;
        head = (head + skip) % this.CAP;
        count -= skip;
      }
      const toRead = Math.min(framesNeeded, count);
      const out = output[c];
      for (let i = 0; i < framesNeeded; i++) {
        out[i] = i < toRead ? ring[head] : 0;
        if (i < toRead) head = (head + 1) % this.CAP;
      }
      this.heads[c] = head;
      this.counts[c] = count - toRead;
    }

    return true;
  }
}
registerProcessor('pcm-player', PcmPlayerProcessor);
`;

export class PcmPlayer {
  private audioCtx: AudioContext | null = null;
  private workletNode: AudioWorkletNode | null = null;
  private gainNode: GainNode | null = null;
  private initPromise: Promise<void> | null = null;
  private sampleRate = 48000;
  private droppedPackets = 0;
  private _muted = true;

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
    this.gainNode = this.audioCtx.createGain();
    this.gainNode.gain.value = this._muted ? 0 : 1;
    this.workletNode.connect(this.gainNode);
    this.gainNode.connect(this.audioCtx.destination);
  }

  feed(samples: Float32Array, channelCount: number): void {
    if (!this.workletNode) {
      this.droppedPackets++;
      return;
    }
    this.workletNode.port.postMessage({ samples, channelCount });
  }

  /** No-op: metering moved to WASM. Stub kept until viewer.ts is updated. */
  onMeter(_: (data: AudioMeterData) => void): void {}

  setMuted(muted: boolean): void {
    this._muted = muted;
    if (this.gainNode) this.gainNode.gain.value = muted ? 0 : 1;
  }

  /** No-op: metering moved to WASM. Stub kept until AudioTab is updated. */
  setSelectedChannel(_: number): void {}

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
  }
}
