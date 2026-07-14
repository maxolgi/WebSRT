import { VideoPipeline, OpusAudioPipeline, AacAudioPipeline } from './decode';
import { CanvasRenderer } from './render';
import type { WorkerMsg, StatsMsg } from './worker';

const logEl = document.getElementById('log') as HTMLPreElement;
const statusEl = document.getElementById('status') as HTMLParagraphElement;
const connectBtn = document.getElementById('connect') as HTMLButtonElement;
const canvas = document.getElementById('video-canvas') as HTMLCanvasElement;
const latencySlider = document.getElementById('latency') as HTMLInputElement;
const latencyNum = document.getElementById('latency-num') as HTMLInputElement;
const statsEl = document.getElementById('stats') as HTMLPreElement;
const muteBtn = document.getElementById('mute') as HTMLButtonElement;

let audioEl: HTMLAudioElement | null = null;
let audioReady = false;
let reconnectTimer: number | null = null;
let reconnectAttempts = 0;
let manualDisconnect = false;
const MAX_RECONNECT_DELAY_MS = 30000;
const BASE_RECONNECT_DELAY_MS = 1000;

type ConnectionState = 'idle' | 'connecting' | 'connected';
let connState: ConnectionState = 'idle';

function setConnState(s: ConnectionState) {
  connState = s;
  if (s === 'connected') {
    connectBtn.textContent = 'stop';
  } else if (s === 'connecting') {
    connectBtn.textContent = 'connecting…';
  } else {
    connectBtn.textContent = 'connect';
  }
}

function setStatus(s: string) { statusEl.textContent = s; }

function formatLatency(ms: number): string {
  return ms >= 1000 ? `${(ms / 1000).toFixed(ms % 1000 === 0 ? 0 : 1)}s` : `${ms}ms`;
}
latencySlider.addEventListener('input', () => { latencyNum.value = latencySlider.value; });
latencyNum.addEventListener('input', () => {
  const v = Math.max(20, Math.min(8000, +latencyNum.value || 120));
  latencySlider.value = String(v);
});

function log(msg: string, cls = '') {
  const lines = logEl.children;
  if (lines.length > 50) logEl.removeChild(lines[0]);
  const span = document.createElement('span');
  span.className = cls;
  span.textContent = msg + '\n';
  logEl.appendChild(span);
  logEl.scrollTop = logEl.scrollHeight;
}

function hexToBytes(hex: string): Uint8Array {
  const clean = hex.replace(/[:\s]/g, '');
  if (clean.length !== 64) {
    throw new Error(`expected 32-byte (64 hex char) hash, got ${clean.length} hex chars`);
  }
  const out = new Uint8Array(32);
  for (let i = 0; i < 32; i++) {
    out[i] = parseInt(clean.substr(i * 2, 2), 16);
  }
  return out;
}

let worker: Worker | null = null;
let video: VideoPipeline | null = null;
let audio: OpusAudioPipeline | AacAudioPipeline | null = null;
let renderer: CanvasRenderer | null = null;
let audioStreamType: number | null = null;
let lastStats: StatsMsg | null = null;

const audioCb = {
  onError: (e: unknown) => log(`audio err: ${e}`, 'err'),
  onReady: () => {
    log('AudioDecoder ready', 'info');
    audioReady = true;
    wireAudio();
  },
};

connectBtn.addEventListener('click', () => {
  if (connState === 'connected' || connState === 'connecting') {
    manualDisconnect = true;
    teardown();
    setStatus('disconnected');
  } else {
    manualDisconnect = false;
    doConnect();
  }
});

muteBtn.addEventListener('click', () => {
  if (!audioEl) return;
  if (audioEl.muted) {
    audioEl.muted = false;
    muteBtn.textContent = 'mute';
    audioEl.play().catch((e) => log(`audio play failed: ${e}`, 'err'));
  } else {
    audioEl.muted = true;
    muteBtn.textContent = 'muted';
  }
});

function scheduleReconnect() {
  if (reconnectTimer !== null) return;
  const delay = Math.min(BASE_RECONNECT_DELAY_MS * 2 ** reconnectAttempts, MAX_RECONNECT_DELAY_MS);
  reconnectAttempts++;
  log(`reconnecting in ${(delay / 1000).toFixed(0)}s (attempt ${reconnectAttempts})…`, 'info');
  setStatus(`reconnecting in ${(delay / 1000).toFixed(0)}s`);
  reconnectTimer = window.setTimeout(() => {
    reconnectTimer = null;
    doConnect();
  }, delay);
}

function cancelReconnect() {
  if (reconnectTimer !== null) {
    window.clearTimeout(reconnectTimer);
    reconnectTimer = null;
  }
  reconnectAttempts = 0;
}

function teardown() {
  cancelReconnect();
  setConnState('idle');
  if (worker) {
    worker.postMessage({ cmd: 'stop' });
  }
  video = null;
  audio = null;
  renderer?.destroy();
  renderer = null;
  if (audioEl) { try { audioEl.pause(); } catch {} audioEl.srcObject = null; }
  audioEl = null;
  audioReady = false;
  audioStreamType = null;
  muteBtn.disabled = true;
  muteBtn.textContent = 'muted';
  lastStats = null;
  statsEl.textContent = '';
}

function wireAudio() {
  if (!audio || !audioReady) return;
  const track = audio.track;
  if (track) {
    if (!audioEl) {
      audioEl = document.createElement('audio');
      audioEl.autoplay = true;
      document.body.appendChild(audioEl);
    }
    audioEl.srcObject = new MediaStream([track]);
    audioEl.muted = false;
    audioEl.play()
      .then(() => {
        log('audio playing ✓', 'ok');
        muteBtn.disabled = false;
        muteBtn.textContent = 'mute';
      })
      .catch((e) => {
        log(`audio autoplay blocked — click "muted" to enable: ${e}`, 'err');
        muteBtn.disabled = false;
        muteBtn.textContent = 'muted';
      });
  } else {
    audio!.resume()
      .then(() => {
        log('audio playing ✓ (AudioWorklet)', 'ok');
        muteBtn.disabled = false;
        muteBtn.textContent = 'mute';
      })
      .catch((e) => log(`audio resume failed: ${e}`, 'err'));
  }
}

function doConnect() {
  teardown();
  manualDisconnect = false;
  setConnState('connecting');

  const hashHex = (window as any).CERT_HASH as string | null | undefined;
  if (hashHex === undefined) {
    log('No cert-hash.js — is the gateway running?', 'err');
    return;
  }

  renderer = new CanvasRenderer(canvas, Math.min(150, Math.floor(+latencySlider.value / 2)));
  let firstFrame = true;
  video = new VideoPipeline({
    onFrame: (frame) => {
      renderer?.draw(frame);
      if (firstFrame) {
        firstFrame = false;
        log(`first frame decoded ✓ (${frame.displayWidth}x${frame.displayHeight})`, 'ok');
        setStatus(`decoding ${frame.displayWidth}x${frame.displayHeight}`);
      }
    },
    onError: (e) => log(`video err: ${e}`, 'err'),
    onConfigured: (info) =>
      log(`VideoDecoder configured (profile ${info.profile}, level ${info.level})`, 'info'),
  });

  const pageHost = location.hostname || '127.0.0.1';
  const wtHost = pageHost === 'localhost' ? '127.0.0.1' : pageHost;
  const wtUrl = `https://${wtHost}:4433/wt`;

  let certHash: Uint8Array | null = null;
  if (hashHex) {
    certHash = hexToBytes(hashHex);
    log(`connecting to ${wtUrl} (self-signed, hash ${hashHex.slice(0, 8)}…) …`, 'info');
  } else {
    log(`connecting to ${wtUrl} (mkcert/PKI) …`, 'info');
  }

  if (!worker) {
    worker = new Worker(new URL('./worker.ts', import.meta.url), { type: 'module' });
    worker.onmessage = (e: MessageEvent) => handleWorkerMsg(e.data as WorkerMsg);
    worker.onerror = (e) => log(`worker error: ${e.message}`, 'err');
  }

  const latencyMs = +latencySlider.value;
  log(`TSBPD latency: ${formatLatency(latencyMs)}`, 'info');
  worker.postMessage({ cmd: 'connect', url: wtUrl, certHash, latencyMs });
}

function handleWorkerMsg(msg: WorkerMsg) {
  switch (msg.type) {
    case 'log':
      log(msg.msg, msg.cls);
      break;
    case 'wtReady':
      log('WT ready ✓', 'ok');
      setStatus('WT ready; awaiting SRT handshake');
      setConnState('connected');
      break;
    case 'handshakeComplete':
      reconnectAttempts = 0;
      setStatus('SRT connected; awaiting video stream');
      break;
    case 'pmt':
      if (msg.audioPid >= 0 && msg.audioStreamType >= 0 && !audio) {
        audioStreamType = msg.audioStreamType;
        const isOpus = audioStreamType === 0x06;
        log(`audio PID ${msg.audioPid}: ${isOpus ? 'Opus' : 'AAC'} (stream type 0x${audioStreamType.toString(16)})`, 'info');
        audio = isOpus
          ? new OpusAudioPipeline(audioCb)
          : new AacAudioPipeline(audioCb);
      }
      break;
    case 'videoPes':
      video?.feed(msg.data, msg.pts, msg.isKeyframe);
      break;
    case 'audioPes':
      audio?.feed(msg.data, msg.pts);
      break;
    case 'stats':
      lastStats = msg.stats;
      updateStats(msg.stats);
      break;
    case 'close':
      log('session closed', 'err');
      setStatus('closed');
      break;
    case 'wtClosed':
      log('WT closed', 'info');
      if (!manualDisconnect) scheduleReconnect();
      break;
    case 'wtError':
      log(`WT error: ${msg.error}`, 'err');
      if (!manualDisconnect) scheduleReconnect();
      break;
  }
}

function updateStats(s: StatsMsg) {
  const lossRate = (s.rxData + s.rxLoss) > 0
    ? ((s.rxLoss / (s.rxData + s.rxLoss)) * 100).toFixed(2)
    : '0.00';
  const mbps = (s.bandwidthBps / 1e6).toFixed(1);
  const elapsed = (s.elapsedMs / 1000).toFixed(0);
  statsEl.textContent =
    `uptime   ${elapsed}s\n` +
    `RTT      ${s.rttMs.toFixed(1)}ms\n` +
    `bw       ${mbps} Mbps\n` +
    `rx pkts  ${s.rxData}\n` +
    `rx bytes ${(s.rxBytes / 1e6).toFixed(1)} MB\n` +
    `loss     ${s.rxLoss} (${lossRate}%)\n` +
    `re-xmit  ${s.rxRetransmit}\n` +
    `dropped  ${s.rxDropped}\n` +
    `belated  ${s.rxBelated}\n` +
    `buf'd    ${s.rxBuffered}\n` +
    `ACK/NAK  ${s.rxAck}/${s.rxNak}`;
}

if ((window as any).CERT_HASH !== undefined) {
  log((window as any).CERT_HASH ? 'Cert hash loaded — auto-connecting…' : 'mkcert mode — auto-connecting…', 'info');
  doConnect();
} else {
  log('No cert-hash.js. Start the gateway first, then reload.', 'info');
}
