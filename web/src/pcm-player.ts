// Phase 0 spike PCM player.
//
// Receives interleaved f32 PCM from the worker (kind=5 events emitted by the
// mpeg2ts-wasm demuxer for SMPTE 302M PIDs), plays it through an AudioWorklet.
//
// Single-PID for the spike. Phase 2 will generalize to a per-PID Map feeding
// the WASM mixer (see audioplan.md).

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
      const samples = e.data.samples;
      const ch = e.data.channelCount || 2;
      if (this.rings.length !== ch) {
        this.channelCount = ch;
        this.rings = [];
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
    for (let c = 0; c < ch; c++) {
      if (!this.rings[c]) {
        for (let i = 0; i < framesNeeded; i++) output[c][i] = 0;
        continue;
      }
      let head = this.heads[c];
      let count = this.counts[c];
      if (count > framesNeeded + 2400) {
        const skip = count - framesNeeded - 2400;
        head = (head + skip) % this.CAP;
        count -= skip;
      }
      const ring = this.rings[c];
      const toRead = Math.min(framesNeeded, count);
      for (let i = 0; i < framesNeeded; i++) {
        output[c][i] = i < toRead ? ring[head] : 0;
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
  private initPromise: Promise<void> | null = null;
  private sampleRate = 48000;
  private droppedPackets = 0;

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
    this.workletNode.connect(this.audioCtx.destination);
  }

  feed(samples: Float32Array, channelCount: number): void {
    if (!this.workletNode) {
      this.droppedPackets++;
      return;
    }
    this.workletNode.port.postMessage({ samples, channelCount });
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
  }
}
