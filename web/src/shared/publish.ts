// Shared helpers extracted from stream.tsx / call.tsx (behavior-identical).
import type { PublishCmd, EncodeStats } from '../stream-worker';
import type { StatsMsg } from '../worker';

// ─── Hex helper ───────────────────────────────────────────────────

export function hexToBytes(hex: string): Uint8Array {
  const clean = hex.replace(/[:\s]/g, '');
  const out = new Uint8Array(clean.length / 2);
  for (let i = 0; i < out.length; i++) {
    out[i] = parseInt(clean.substring(i * 2, i * 2 + 2), 16);
  }
  return out;
}

// ─── Codec auto-detection ─────────────────────────────────────────

export const CODEC_CANDIDATES = [
  { label: 'AV1', codec: 'av01.0.08M.08' },
  { label: 'H.264', codec: 'avc1.640028' },
];

export async function detectCodec(
  width: number,
  height: number,
  framerate: number,
  bitrate: number,
): Promise<{ codec: string; label: string } | null> {
  for (const c of CODEC_CANDIDATES) {
    try {
      const cfg: VideoEncoderConfig = {
        codec: c.codec,
        width,
        height,
        bitrate: bitrate * 1_000_000,
        framerate,
        hardwareAcceleration: 'prefer-hardware',
      };
      if (!c.codec.startsWith('av01')) {
        (cfg as unknown as Record<string, unknown>).avc = { format: 'annexb' };
      }
      const probe = await VideoEncoder.isConfigSupported(cfg);
      if (probe.supported) {
        return { codec: c.codec, label: c.label };
      }
    } catch { /* try next */ }
  }
  return null;
}

// ─── Hidden-tab timer worker (frame pump fallback) ────────────────

export const TIMER_WORKER_SRC = `
let id = null;
onmessage = (e) => {
  if (typeof e.data === 'number') {
    if (id) clearInterval(id);
    id = setInterval(() => postMessage('tick'), e.data);
  } else if (e.data === 'stop') {
    if (id) { clearInterval(id); id = null; }
  }
};
`;

// ─── Audio capture graph ──────────────────────────────────────────

export const CAPTURE_WORKLET = `
class CaptureProcessor extends AudioWorkletProcessor {
  constructor() {
    super();
    this.frameSize = 960;
    this.buffers = [];
  }
  process(inputs) {
    const input = inputs[0];
    if (!input || input.length === 0) return true;
    const ch = input.length;
    for (let c = 0; c < ch; c++) {
      if (!this.buffers[c]) this.buffers[c] = [];
      for (let i = 0; i < input[c].length; i++) {
        this.buffers[c].push(input[c][i]);
      }
    }
    while (this.buffers[0] && this.buffers[0].length >= this.frameSize) {
      const numCh = this.buffers.length;
      const out = new Float32Array(numCh * this.frameSize);
      for (let c = 0; c < numCh; c++) {
        const slice = this.buffers[c].splice(0, this.frameSize);
        out.set(slice, c * this.frameSize);
      }
      this.port.postMessage(
        { data: out, channels: numCh, time: currentTime },
        [out.buffer]
      );
    }
    return true;
  }
}
registerProcessor('capture-processor', CaptureProcessor);
`;

export interface ConnectAudioOpts {
  allowTabAudio?: boolean;
  getTabAudioTracks?: () => readonly MediaStreamTrack[];
  onMicAcquired?: () => void;
}

export class AudioGraphManager {
  private audioCtx: AudioContext | null = null;
  private workletNode: AudioWorkletNode | null = null;
  private audioSourceNode: MediaStreamAudioSourceNode | null = null;
  private micStream: MediaStream | null = null;
  private workletReady = false;

  constructor(private onError: (msg: string) => void) {}

  async setup(): Promise<void> {
    if (this.audioCtx) return;

    const Ctx = window.AudioContext || (window as unknown as Window & { webkitAudioContext: typeof AudioContext }).webkitAudioContext;
    this.audioCtx = new Ctx({ sampleRate: 48000 });

    const blob = new Blob([CAPTURE_WORKLET], { type: 'application/javascript' });
    await this.audioCtx.audioWorklet.addModule(URL.createObjectURL(blob));
    this.workletReady = true;

    this.workletNode = new AudioWorkletNode(this.audioCtx, 'capture-processor', {
      numberOfInputs: 1,
      numberOfOutputs: 1,
      outputChannelCount: [2],
      channelCount: 2,
      channelCountMode: 'explicit',
    });
    const silence = this.audioCtx.createGain();
    silence.gain.value = 0;
    this.workletNode.connect(silence);
    silence.connect(this.audioCtx.destination);
  }

  async connect(source: string, opts: ConnectAudioOpts = {}): Promise<void> {
    const audioCtx = this.audioCtx;
    const workletNode = this.workletNode;
    if (!audioCtx || !workletNode) return;
    if (!source) return;
    try {
      if (source === '__tab__') {
        if (!opts.allowTabAudio) return;
        const tracks = opts.getTabAudioTracks?.() ?? [];
        if (tracks.length === 0) return;
        this.audioSourceNode = audioCtx.createMediaStreamSource(new MediaStream([...tracks]));
      } else {
        this.micStream = await navigator.mediaDevices.getUserMedia({
          audio: { deviceId: { exact: source } },
        });
        opts.onMicAcquired?.();
        this.audioSourceNode = audioCtx.createMediaStreamSource(this.micStream);
      }
      this.audioSourceNode.connect(workletNode);
    } catch (e) {
      this.onError(`Audio source failed: ${e}`);
    }
  }

  disconnect(): void {
    try { this.audioSourceNode?.disconnect(); } catch {}
    this.audioSourceNode = null;
    if (this.micStream) {
      this.micStream.getTracks().forEach((t) => t.stop());
      this.micStream = null;
    }
  }

  getMicStream(): MediaStream | null {
    return this.micStream;
  }

  async resumeIfSuspended(): Promise<void> {
    if (this.audioCtx?.state === 'suspended') await this.audioCtx.resume();
  }

  transferPort(worker: Worker): void {
    if (!this.workletNode) return;
    const port = this.workletNode.port;
    worker.postMessage({ cmd: 'audio-port', port } as PublishCmd, [port]);
  }

  close(): void {
    this.disconnect();
    if (this.audioCtx) {
      try { this.audioCtx.close(); } catch {}
      this.audioCtx = null;
    }
    this.workletNode = null;
    this.workletReady = false;
  }
}

// ─── Frame pump ───────────────────────────────────────────────────

export interface FramePumpDeps {
  getPreviewEl: () => HTMLVideoElement;
  isPublishing: () => boolean;
  postFrame: (frame: VideoFrame) => void;
  getFps: () => number;
}

export class FramePump {
  private credits = 0;
  private rafId: number | null = null;
  private bgWorker: Worker | null = null;

  constructor(private deps: FramePumpDeps) {}

  start(): void {
    this.credits = 4;
    this.pumpFrame();
  }

  credit(): void {
    this.credits++;
  }

  resetCredits(): void {
    this.credits = 0;
  }

  schedule(): void {
    const publishing = this.deps.isPublishing();
    if (!publishing) return;
    if (document.hidden) {
      if (this.rafId !== null) { cancelAnimationFrame(this.rafId); this.rafId = null; }
      if (this.bgWorker === null) {
        this.bgWorker = new Worker(URL.createObjectURL(new Blob([TIMER_WORKER_SRC], { type: 'application/javascript' })));
        this.bgWorker.onmessage = () => this.pumpFrame();
        this.bgWorker.postMessage(1000 / this.deps.getFps());
      }
    } else {
      if (this.bgWorker !== null) { this.bgWorker.terminate(); this.bgWorker = null; }
      if (this.rafId !== null) { cancelAnimationFrame(this.rafId); this.rafId = null; }
      const previewEl = this.deps.getPreviewEl();
      this.rafId = 'requestVideoFrameCallback' in previewEl
        ? (previewEl as unknown as { requestVideoFrameCallback: (cb: FrameRequestCallback) => number })
          .requestVideoFrameCallback(() => this.pumpFrame())
        : requestAnimationFrame(() => this.pumpFrame());
    }
  }

  stop(): void {
    if (this.rafId !== null) {
      cancelAnimationFrame(this.rafId);
      this.rafId = null;
    }
    if (this.bgWorker !== null) {
      this.bgWorker.terminate();
      this.bgWorker = null;
    }
  }

  private pumpFrame(): void {
    if (!this.deps.isPublishing()) return;
    const previewEl = this.deps.getPreviewEl();
    if (this.credits > 0 && previewEl.readyState >= 2) {
      const frame = new VideoFrame(previewEl);
      if (frame.format === null) {
        frame.close();
      } else {
        this.credits--;
        this.deps.postFrame(frame);
      }
    }
    this.schedule();
  }
}

// ─── Debug panel resizer ──────────────────────────────────────────

export function attachDebugResizer(opts: {
  debugRoot: HTMLElement;
  onResize?: () => void;
}): { sync: () => void } {
  const PANEL_MIN_W = 320;
  const PANEL_MAX_W_RATIO = 0.85;
  const debugRoot = opts.debugRoot;
  const onResize = opts.onResize;

  const resizer = document.createElement('div');
  resizer.className = 'debug-resizer visible';
  document.body.appendChild(resizer);

  function syncResizerPosition() {
    const w = debugRoot.offsetWidth;
    resizer.style.right = `${w}px`;
    document.body.style.paddingRight = `${w + 16}px`;
    onResize?.();
  }

  const savedW = localStorage.getItem('websrt-debug-width');
  if (savedW) debugRoot.style.width = `${savedW}px`;

  let dragging = false;
  resizer.addEventListener('mousedown', (e) => {
    e.preventDefault();
    dragging = true;
    resizer.classList.add('dragging');
    document.body.classList.add('resizing');
    document.body.style.cursor = 'col-resize';
    document.body.style.userSelect = 'none';
  });
  window.addEventListener('mousemove', (e) => {
    if (!dragging) return;
    const maxW = window.innerWidth * PANEL_MAX_W_RATIO;
    const w = Math.min(maxW, Math.max(PANEL_MIN_W, window.innerWidth - e.clientX));
    debugRoot.style.width = `${w}px`;
    resizer.style.right = `${w}px`;
    document.body.style.paddingRight = `${w + 16}px`;
    onResize?.();
  });
  window.addEventListener('mouseup', () => {
    if (!dragging) return;
    dragging = false;
    resizer.classList.remove('dragging');
    document.body.classList.remove('resizing');
    document.body.style.cursor = '';
    document.body.style.userSelect = '';
    localStorage.setItem('websrt-debug-width', String(debugRoot.offsetWidth));
  });

  return { sync: syncResizerPosition };
}

// ─── Publish stats line ───────────────────────────────────────────

export function formatPubStats(srt: StatsMsg, enc: EncodeStats, targetFps: number): string {
  const txMbps = (srt.bandwidthBps / 1e6).toFixed(1);
  const txMB = (srt.txBytes / 1e6).toFixed(1);
  return (
    `<span class="${enc.fps >= targetFps - 5 ? 'ok' : 'err'}">${enc.fps} fps</span>` +
    ` | encode submit: ${enc.encodeMs.toFixed(1)}ms` +
    ` | queue: ${enc.queueDepth}` +
    ` | <span class="info">\u2191${txMbps} Mbps</span>` +
    ` | sent: ${txMB} MB` +
    ` | RTT: ${srt.rttMs.toFixed(0)}ms` +
    ` | loss: ${srt.txLoss}`
  );
}
