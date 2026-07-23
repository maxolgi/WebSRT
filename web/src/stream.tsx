import { render } from 'preact';
import { DebugStore } from './debug/store';
import { DebugPanel } from './debug/components/Panel';
import { attachConsoleErrorCapture } from './debug/sampler';
import type { PublishCmd, PublishMsg, EncodeStats } from './stream-worker';
import type { StatsMsg } from './worker';

// ─── DOM refs ─────────────────────────────────────────────────────

const previewEl = document.getElementById('preview') as HTMLVideoElement;
const shareBtn = document.getElementById('share-btn') as HTMLButtonElement;
const publishBtn = document.getElementById('publish-btn') as HTMLButtonElement;
const stopBtn = document.getElementById('stop-btn') as HTMLButtonElement;
const streamNameInput = document.getElementById('stream-name') as HTMLInputElement;
const latencyNum = document.getElementById('latency-num') as HTMLInputElement;
const codecSelect = document.getElementById('codec-select') as HTMLSelectElement;
const bitrateNum = document.getElementById('bitrate-num') as HTMLInputElement;
const framerateSelect = document.getElementById('framerate-select') as HTMLSelectElement;
const fullscreenBtn = document.getElementById('fullscreen-btn') as HTMLButtonElement;
const pubStatsText = document.getElementById('pub-stats-text') as HTMLDivElement;
const audioSourceSelect = document.getElementById('audio-source') as HTMLSelectElement;

const debugRoot = document.getElementById('debug-root') as HTMLDivElement;

// ─── Debug panel (reused from viewer) ─────────────────────────────

const PANEL_MIN_W = 320;
const PANEL_MAX_W_RATIO = 0.85;
const resizer = document.createElement('div');
resizer.className = 'debug-resizer visible';
document.body.appendChild(resizer);

function syncResizerPosition() {
  const w = debugRoot.offsetWidth;
  resizer.style.right = `${w}px`;
  document.body.style.paddingRight = `${w + 16}px`;
}

{
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
}

const store = new DebugStore();
let panelMounted = false;
let consoleCleanup: (() => void) | null = null;

function log(msg: string, cls = '') { store.pushLog(msg, cls); }
function setStatus(s: string) { store.status.value = s; }

function setPanelVisible(visible: boolean) {
  store.panelVisible.value = visible;
  debugRoot.classList.toggle('visible', visible);
  document.body.classList.toggle('debug-open', visible);
  if (visible) {
    syncResizerPosition();
    localStorage.setItem('websrt-debug-open', '1');
    if (!panelMounted) {
      render(<DebugPanel store={store} />, debugRoot);
      panelMounted = true;
      consoleCleanup = attachConsoleErrorCapture(store);
    }
  } else {
    document.body.style.paddingRight = '';
    localStorage.removeItem('websrt-debug-open');
  }
}

// ─── State ────────────────────────────────────────────────────────

let worker: Worker | null = null;
let captureStream: MediaStream | null = null;
let publishing = false;
let credits = 0;
let rafId: number | null = null;
let detectedCodec: string | null = null;
let detectedCodecLabel = '';

// Audio
let audioCtx: AudioContext | null = null;
let workletNode: AudioWorkletNode | null = null;
let workletReady = false;

// ─── Codec auto-detection ─────────────────────────────────────────

const CODEC_CANDIDATES = [
  { label: 'AV1', codec: 'av01.0.08M.08' },
  { label: 'H.264', codec: 'avc1.640028' },
];

async function detectCodec(width: number, height: number, framerate: number, bitrate: number): Promise<string | null> {
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
        detectedCodec = c.codec;
        detectedCodecLabel = c.label;
        return c.codec;
      }
    } catch { /* try next */ }
  }
  return null;
}

function populateCodecSelect() {
  codecSelect.innerHTML = '';
  const autoOpt = document.createElement('option');
  autoOpt.value = 'auto';
  autoOpt.textContent = 'auto';
  codecSelect.appendChild(autoOpt);
  for (const c of CODEC_CANDIDATES) {
    const opt = document.createElement('option');
    opt.value = c.codec;
    opt.textContent = c.label;
    codecSelect.appendChild(opt);
  }
  codecSelect.value = 'auto';
}

// ─── Audio source population ──────────────────────────────────────

function populateAudioSources() {
  const current = audioSourceSelect.value;
  audioSourceSelect.innerHTML = '';
  const none = document.createElement('option');
  none.value = '';
  none.textContent = '(none)';
  audioSourceSelect.appendChild(none);
  if (captureStream && captureStream.getAudioTracks().length > 0) {
    const opt = document.createElement('option');
    opt.value = '__tab__';
    opt.textContent = 'Tab / System Audio';
    audioSourceSelect.appendChild(opt);
  }
  for (const opt of micOptions) {
    audioSourceSelect.appendChild(opt.cloneNode(true) as HTMLOptionElement);
  }
  audioSourceSelect.value = current || '';
}

let micOptions: HTMLOptionElement[] = [];

async function enumerateMics() {
  try {
    const devices = await navigator.mediaDevices.enumerateDevices();
    const mics = devices.filter((d) => d.kind === 'audioinput');
    micOptions = mics.map((m) => {
      const opt = document.createElement('option');
      opt.value = m.deviceId;
      opt.textContent = m.label || `Device ${m.deviceId.slice(0, 8)}`;
      return opt;
    });
    populateAudioSources();
  } catch { /* ignore */ }
}

// ─── AudioWorklet (inline, matching decode.ts pattern) ────────────

const CAPTURE_WORKLET = `
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

async function setupAudioGraph(): Promise<void> {
  if (audioCtx) return;

  const Ctx = window.AudioContext || (window as unknown as Window & { webkitAudioContext: typeof AudioContext }).webkitAudioContext;
  audioCtx = new Ctx({ sampleRate: 48000 });

  const blob = new Blob([CAPTURE_WORKLET], { type: 'application/javascript' });
  await audioCtx.audioWorklet.addModule(URL.createObjectURL(blob));
  workletReady = true;

  workletNode = new AudioWorkletNode(audioCtx, 'capture-processor', {
    numberOfInputs: 1,
    numberOfOutputs: 1,
    outputChannelCount: [2],
    channelCount: 2,
    channelCountMode: 'explicit',
  });
  const silence = audioCtx.createGain();
  silence.gain.value = 0;
  workletNode.connect(silence);
  silence.connect(audioCtx.destination);
}

let audioSourceNode: MediaStreamAudioSourceNode | null = null;
let micStream: MediaStream | null = null;

async function connectAudioSource() {
  if (!audioCtx || !workletNode) return;
  const src = audioSourceSelect.value;
  if (!src) return;
  try {
    if (src === '__tab__') {
      if (!captureStream) return;
      const tracks = captureStream.getAudioTracks();
      if (tracks.length === 0) return;
      audioSourceNode = audioCtx.createMediaStreamSource(new MediaStream(tracks));
    } else {
      micStream = await navigator.mediaDevices.getUserMedia({
        audio: { deviceId: { exact: src } },
      });
      audioSourceNode = audioCtx.createMediaStreamSource(micStream);
    }
    audioSourceNode.connect(workletNode);
  } catch (e) {
    log(`Audio source failed: ${e}`, 'err');
  }
}

function disconnectAudioSource() {
  try { audioSourceNode?.disconnect(); } catch {}
  audioSourceNode = null;
  if (micStream) {
    micStream.getTracks().forEach((t) => t.stop());
    micStream = null;
  }
}

// ─── Screen capture ───────────────────────────────────────────────

shareBtn.addEventListener('click', async () => {
  try {
    const stream = await navigator.mediaDevices.getDisplayMedia({
      video: { frameRate: { ideal: +framerateSelect.value } },
      audio: true,
    });
    captureStream = stream;
    previewEl.srcObject = stream;
    await previewEl.play();

    // Tab/system audio now appears in the dropdown
    await enumerateMics();

    shareBtn.disabled = true;
    publishBtn.disabled = false;
    setStatus('screen captured — ready to publish');
    log(`Captured ${stream.getVideoTracks()[0]?.getSettings().width}x${stream.getVideoTracks()[0]?.getSettings().height}`, 'info');

    // Auto-detect codec
    const vTrack = stream.getVideoTracks()[0];
    const settings = vTrack?.getSettings();
    const w = settings?.width ?? 1280;
    const h = settings?.height ?? 720;
    const fps = +framerateSelect.value;
    const br = +bitrateNum.value;
    const codec = await detectCodec(w, h, fps, br);
    if (codec) {
      log(`Codec auto-detected: ${detectedCodecLabel} (${codec})`, 'info');
    } else {
      log('No supported codec found!', 'err');
    }

    // Handle user clicking browser's "Stop sharing"
    stream.getVideoTracks()[0]?.addEventListener('ended', () => {
      stopAll();
      shareBtn.disabled = false;
      publishBtn.disabled = true;
      setStatus('screen sharing ended');
    });
  } catch (e) {
    log(`Screen capture failed: ${e}`, 'err');
    setStatus('capture failed');
  }
});

// ─── Frame pump ───────────────────────────────────────────────────

function startFramePump() {
  credits = 4;
  const pump = () => {
    if (!publishing || !worker) return;
    if (credits > 0 && previewEl.readyState >= 2) {
      const frame = new VideoFrame(previewEl);
      if (frame.format === null) {
        frame.close();
      } else {
        credits--;
        const cmd: PublishCmd = { cmd: 'frame', frame };
        worker.postMessage(cmd, [frame]);
      }
    }
    rafId = 'requestVideoFrameCallback' in previewEl
      ? (previewEl as unknown as { requestVideoFrameCallback: (cb: FrameRequestCallback) => number })
        .requestVideoFrameCallback(pump as FrameRequestCallback)
      : requestAnimationFrame(pump);
  };
  pump();
}

function stopFramePump() {
  if (rafId !== null) {
    cancelAnimationFrame(rafId);
    rafId = null;
  }
}

// ─── Publishing ───────────────────────────────────────────────────

publishBtn.addEventListener('click', async () => {
  if (publishing) return;
  if (!captureStream) { log('Capture a screen first', 'err'); return; }

  publishing = true;
  publishBtn.disabled = true;
  stopBtn.disabled = false;
  setStatus('starting\u2026');

  // Determine codec
  const vTrack = captureStream.getVideoTracks()[0];
  const settings = vTrack?.getSettings();
  const width = settings?.width ?? 1280;
  const height = settings?.height ?? 720;
  const framerate = +framerateSelect.value;
  const bitrate = +bitrateNum.value;
  const chosenCodec = codecSelect.value === 'auto' ? (detectedCodec ?? 'avc1.640028') : codecSelect.value;
  const isAv1 = chosenCodec.startsWith('av01');

  // Determine audio config
  const audioSource = audioSourceSelect.value;
  const audioCfg = audioSource ? { bitrate: 128000, channels: 2 } : null;

  // Setup audio graph if needed
  if (audioSource) {
    await setupAudioGraph();
    await connectAudioSource();
    if (audioCtx?.state === 'suspended') await audioCtx.resume();
  }

  // Cert hash
  const hashHex = (window as any).CERT_HASH as string | null | undefined;
  let certHash: Uint8Array | null = null;
  if (hashHex) certHash = hexToBytes(hashHex);

  // Build WT URL
  const pageHost = location.hostname || '127.0.0.1';
  const wtHost = pageHost === 'localhost' ? '127.0.0.1' : pageHost;
  const urlParams = new URLSearchParams(location.search);
  const wtPort = urlParams.get('port') || '4433';
  const authToken = urlParams.get('token');
  const streamName = streamNameInput.value || 'default';
  const qp = new URLSearchParams({ publish: streamName });
  if (authToken) qp.set('token', authToken);
  const wtUrl = `https://${wtHost}:${wtPort}/wt?${qp}`;

  const latencyMs = +latencyNum.value;

  // Create worker
  worker = new Worker(new URL('./stream-worker.ts', import.meta.url), { type: 'module' });
  worker.onmessage = (e: MessageEvent) => handleWorkerMsg(e.data as PublishMsg);
  worker.onerror = (e) => { log(`worker error: ${e.message}`, 'err'); };

  const cmd: PublishCmd = {
    cmd: 'init',
    url: wtUrl,
    certHash,
    latencyMs,
    video: { codec: chosenCodec, width, height, bitrate, framerate },
    audio: audioCfg,
  };
  const transfer: ArrayBuffer[] = [];
  if (certHash) transfer.push(certHash.buffer as ArrayBuffer);
  worker.postMessage(cmd, transfer);

  // Transfer audio port
  if (audioSource && workletNode) {
    const port = workletNode.port;
    worker.postMessage({ cmd: 'audio-port', port } as PublishCmd, [port]);
  }

  log(`Publishing to ${wtUrl} (${isAv1 ? 'AV1' : 'H.264'})`, 'info');
  log(`latency: ${latencyMs}ms, bitrate: ${bitrate}Mbps, fps: ${framerate}`, 'info');
});

stopBtn.addEventListener('click', () => stopAll());

function stopAll() {
  publishing = false;
  stopFramePump();
  if (worker) {
    worker.postMessage({ cmd: 'stop' } as PublishCmd);
    worker.terminate();
    worker = null;
  }
  disconnectAudioSource();
  if (audioCtx) {
    try { audioCtx.close(); } catch {}
    audioCtx = null;
  }
  workletNode = null;
  workletReady = false;
  publishBtn.disabled = !captureStream;
  stopBtn.disabled = true;
  credits = 0;
  setStatus('stopped');
}

// ─── Worker message handler ───────────────────────────────────────

function handleWorkerMsg(msg: PublishMsg) {
  if (msg.type === 'batch') {
    for (const m of msg.msgs) handleWorkerMsg(m);
    return;
  }
  switch (msg.type) {
    case 'log':
      log(msg.msg, msg.cls);
      break;
    case 'credit':
      credits++;
      break;
    case 'wtReady':
      log('WT connected', 'ok');
      setStatus('WT ready; awaiting SRT handshake');
      break;
    case 'handshakeComplete':
      log('SRT handshake complete', 'ok');
      setStatus('LIVE');
      startFramePump();
      break;
    case 'close':
      log('SRT closed', 'err');
      setStatus('closed');
      stopFramePump();
      break;
    case 'wtClosed':
      if (msg.error) log(`WT closed: ${msg.error}`, 'err');
      else log('WT closed', 'info');
      setStatus('disconnected');
      stopFramePump();
      break;
    case 'stats':
      store.srtStats.value = msg.stats;
      if (msg.encode) updateEncodeStats(msg.stats, msg.encode);
      break;
  }
}

function updateEncodeStats(srt: StatsMsg, enc: EncodeStats) {
  const txMbps = (srt.bandwidthBps / 1e6).toFixed(1);
  const txMB = (srt.txBytes / 1e6).toFixed(1);
  pubStatsText.innerHTML =
    `<span class="${enc.fps >= framerateVal() - 5 ? 'ok' : 'err'}">${enc.fps} fps</span>` +
    ` | encode: ${enc.encodeMs.toFixed(1)}ms` +
    ` | queue: ${enc.queueDepth}` +
    ` | <span class="info">\u2191${txMbps} Mbps</span>` +
    ` | sent: ${txMB} MB` +
    ` | RTT: ${srt.rttMs.toFixed(0)}ms` +
    ` | loss: ${srt.txLoss}`;
}

function framerateVal(): number { return +framerateSelect.value; }

// ─── Helpers ──────────────────────────────────────────────────────

function hexToBytes(hex: string): Uint8Array {
  const clean = hex.replace(/[:\s]/g, '');
  const out = new Uint8Array(clean.length / 2);
  for (let i = 0; i < out.length; i++) {
    out[i] = parseInt(clean.substring(i * 2, i * 2 + 2), 16);
  }
  return out;
}

// ─── Misc handlers ────────────────────────────────────────────────

fullscreenBtn.addEventListener('click', () => {
  if (document.fullscreenElement) document.exitFullscreen();
  else previewEl.requestFullscreen();
});

document.getElementById('debug-toggle')?.addEventListener('click', () => {
  setPanelVisible(!store.panelVisible.value);
});

// ─── Init ─────────────────────────────────────────────────────────

populateCodecSelect();
enumerateMics();
navigator.mediaDevices?.addEventListener('devicechange', enumerateMics);

setPanelVisible(false);

if ((window as any).CERT_HASH !== undefined) {
  if ((window as any).CERT_HASH) {
    log('Cert hash loaded', 'info');
  } else {
    log('mkcert mode', 'info');
  }
} else {
  log('No cert-hash.js. Start the gateway first, then reload.', 'info');
}
