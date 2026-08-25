// PCM player AudioWorklet sources, co-located for comparison.
//
// IMPORTANT: these two worklets have DIFFERENT message contracts and must NOT
// be merged without first unifying their wire protocol (explicitly out of
// scope). Both register the same processor name ('pcm-player'), but each page
// loads its own blob URL, so there is no collision at runtime.
//
// - PCM_PLAYER_WORKLET_PLANES: consumes postMessage `{ planes: Float32Array[] }`
//   — one pre-deinterleaved plane per channel, sent by decode.ts routeFrame().
//   Fixed CAP=24000 rings allocated lazily per channel index.
//
// - PCM_PLAYER_WORKLET_FRAMES: consumes postMessage
//   `{ samples: Float32Array, channelCount }` — interleaved samples sent by
//   PcmPlayer.feed(). Rebuilds the per-channel rings whenever the channel
//   count changes.

/** Worklet variant fed deinterleaved planes (`{ planes: Float32Array[] }`). */
export const PCM_PLAYER_WORKLET_PLANES = `
class PcmPlayerProcessor extends AudioWorkletProcessor {
  constructor() {
    super();
    this.queues = [];
    this.heads = [];
    this.tails = [];
    this.counts = [];
    this.CAP = 24000;
    this.port.onmessage = (e) => {
      const planes = e.data.planes;
      for (let ch = 0; ch < planes.length; ch++) {
        if (!this.queues[ch]) {
          this.queues[ch] = new Float32Array(this.CAP);
          this.heads[ch] = 0;
          this.tails[ch] = 0;
          this.counts[ch] = 0;
        }
        const incoming = planes[ch];
        const q = this.queues[ch];
        let tail = this.tails[ch];
        let count = this.counts[ch];
        for (let i = 0; i < incoming.length; i++) {
          if (count >= this.CAP) {
            this.heads[ch] = (this.heads[ch] + 1) % this.CAP;
            count--;
          }
          q[tail] = incoming[i];
          tail = (tail + 1) % this.CAP;
          count++;
        }
        this.tails[ch] = tail;
        this.counts[ch] = count;
      }
    };
  }
  process(inputs, outputs) {
    const output = outputs[0];
    const framesNeeded = output[0].length;
    for (let ch = 0; ch < output.length; ch++) {
      if (!this.queues[ch]) {
        for (let i = 0; i < framesNeeded; i++) output[ch][i] = 0;
        continue;
      }
      let head = this.heads[ch];
      let count = this.counts[ch];
      if (count > framesNeeded + 2400) {
        const skip = count - framesNeeded - 2400;
        head = (head + skip) % this.CAP;
        count -= skip;
      }
      const q = this.queues[ch];
      const toRead = Math.min(framesNeeded, count);
      for (let i = 0; i < framesNeeded; i++) {
        if (i < toRead) {
          output[ch][i] = q[head];
          head = (head + 1) % this.CAP;
        } else {
          output[ch][i] = 0;
        }
      }
      this.heads[ch] = head;
      this.counts[ch] = count - toRead;
    }
    return true;
  }
}
registerProcessor('pcm-player', PcmPlayerProcessor);
`;

/** Worklet variant fed interleaved samples (`{ samples, channelCount }`). */
export const PCM_PLAYER_WORKLET_FRAMES = `
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
