import { VideoPipeline, OpusAudioPipeline, AacAudioPipeline } from './decode';
import { CanvasRenderer } from './render';
import type { WorkerMsg, StatsMsg } from './worker';

const logEl = document.getElementById('log') as HTMLPreElement;
const statusEl = document.getElementById('status') as HTMLParagraphElement;
const connectBtn = document.getElementById('connect') as HTMLButtonElement;
const canvas = document.getElementById('video-canvas') as HTMLCanvasElement;
const latencyNum = document.getElementById('latency-num') as HTMLInputElement;
const statsEl = document.getElementById('stats') as HTMLPreElement;
const muteBtn = document.getElementById('mute') as HTMLButtonElement;

let audioEl: HTMLAudioElement | null = null;
let audioReady = false;
let reconnectTimer: number | null = null;
let reconnectAttempts = 0;
let manualDisconnect = false;
const MAX_RECONNECT_DELAY_MS = 30000;
const BASE_RECONNECT_DELAY_MS = 2000;

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
const savedLatency = localStorage.getItem('latency');
if (savedLatency) latencyNum.value = savedLatency;
latencyNum.addEventListener('input', () => {
  const v = Math.max(20, Math.min(8000, +latencyNum.value || 120));
  latencyNum.value = String(v);
  localStorage.setItem('latency', String(v));
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
    out[i] = parseInt(clean.substring(i * 2, i * 2 + 2), 16);
  }
  return out;
}

let worker: Worker | null = null;
let wt: WebTransport | null = null;
let datagramWriter: WritableStreamDefaultWriter<Uint8Array> | null = null;
let video: VideoPipeline | null = null;
let audio: OpusAudioPipeline | AacAudioPipeline | null = null;
let renderer: CanvasRenderer | null = null;

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
    reconnectAttempts = 0;
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

const fullscreenBtn = document.getElementById('fullscreen') as HTMLButtonElement;
fullscreenBtn.addEventListener('click', () => {
  if (document.fullscreenElement) {
    document.exitFullscreen();
  } else {
    canvas.requestFullscreen();
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
}

function teardown() {
  cancelReconnect();
  stopDriftMonitor();
  setConnState('idle');
  pendingSends = [];
  if (worker) {
    worker.postMessage({ cmd: 'stop' });
    worker.terminate();
    worker = null;
  }
  try { wt?.close({}); } catch {}
  wt = null;
  datagramWriter = null;
  video = null;
  audio = null;
  renderer?.destroy();
  renderer = null;
  if (audioEl) { try { audioEl.pause(); } catch {} audioEl.srcObject = null; audioEl.remove(); }
  audioEl = null;
  audioReady = false;
  muteBtn.disabled = true;
  muteBtn.textContent = 'muted';
  statsEl.textContent = '';
}

let driftTimer: ReturnType<typeof setInterval> | null = null;
let latestDriftMs: number | null = null;

function startDriftMonitor() {
  if (driftTimer !== null) clearInterval(driftTimer);
  driftTimer = setInterval(() => {
    const videoPts = renderer?.currentPtsUs() ?? null;
    const audioPts = audio?.audioPlayheadUs() ?? null;
    if (videoPts === null || audioPts === null) {
      latestDriftMs = null;
      return;
    }

    const driftMs = (videoPts - audioPts) / 1000;
    latestDriftMs = driftMs;
    const absDrift = Math.abs(driftMs);

    if (absDrift > 40) {
      const direction = driftMs > 0 ? 'ahead of' : 'behind';
      console.warn(`A/V drift: ${driftMs.toFixed(1)}ms (video ${direction} audio)`);
    }
  }, 2000);
}

function stopDriftMonitor() {
  if (driftTimer !== null) {
    clearInterval(driftTimer);
    driftTimer = null;
  }
  latestDriftMs = null;
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
    audioEl.muted = true;
    audioEl.play()
      .then(() => { log('audio ready (muted — click to unmute)', 'info'); })
      .catch((e) => log(`audio play failed: ${e}`, 'err'));
    muteBtn.disabled = false;
    muteBtn.textContent = 'muted';
  } else {
    muteBtn.disabled = false;
    muteBtn.textContent = 'muted';
  }
}

async function refreshCertHash(): Promise<void> {
  try {
    const resp = await fetch('/cert-hash.js', { cache: 'no-store' });
    const text = await resp.text();
    const match = text.match(/CERT_HASH\s*=\s*("(.*?)"|null)/);
    if (match) {
      const newHash = match[2] ?? null;
      const old = (window as any).CERT_HASH as string | null | undefined;
      if (newHash !== old) {
        (window as any).CERT_HASH = newHash;
        log(`Cert hash refreshed: ${newHash ? newHash.slice(0, 8) + '…' : '(mkcert)'}`, 'info');
      }
    }
  } catch { /* ignore — will use cached value */ }
}

async function doConnect() {
  teardown();
  manualDisconnect = false;
  setConnState('connecting');

  await refreshCertHash();
  const hashHex = (window as any).CERT_HASH as string | null | undefined;
  if (hashHex === undefined) {
    log('No cert-hash.js — is the gateway running?', 'err');
    return;
  }

  renderer = new CanvasRenderer(canvas, Math.min(150, Math.floor(+latencyNum.value / 2)));
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
  const urlParams = new URLSearchParams(location.search);
  const wtPort = urlParams.get('port') || '4433';
  const authToken = urlParams.get('token');
  const wtUrl = authToken
    ? `https://${wtHost}:${wtPort}/wt?token=${encodeURIComponent(authToken)}`
    : `https://${wtHost}:${wtPort}/wt`;

  const wtOpts: WebTransportOptions = {};
  if (hashHex) {
    const hash = hexToBytes(hashHex);
    wtOpts.serverCertificateHashes = [{ algorithm: 'sha-256', value: hash as BufferSource }];
    log(`connecting to ${wtUrl} (self-signed, hash ${hashHex.slice(0, 8)}…) …`, 'info');
  } else {
    log(`connecting to ${wtUrl} (mkcert/PKI) …`, 'info');
  }

  const latencyMs = +latencyNum.value;
  log(`TSBPD latency: ${formatLatency(latencyMs)}`, 'info');

  try {
    wt = new WebTransport(wtUrl, wtOpts);
    await wt.ready;
    log('WT ready ✓', 'ok');
    setStatus('WT ready; awaiting SRT handshake');
    setConnState('connected');
  } catch (e) {
    log(`connect failed: ${e}`, 'err');
    try { wt?.close({}); } catch {}
    wt = null;
    if (!manualDisconnect) scheduleReconnect();
    return;
  }

  datagramWriter = wt.datagrams.writable.getWriter();

  wt.closed
    .then(() => { log('WT closed', 'info'); if (!manualDisconnect) scheduleReconnect(); })
    .catch((e) => { log(`WT closed (err): ${e}`, 'err'); if (!manualDisconnect) scheduleReconnect(); });

  if (!worker) {
    worker = new Worker(new URL('./worker.ts', import.meta.url), { type: 'module' });
    worker.onmessage = (e: MessageEvent) => handleWorkerMsg(e.data as WorkerMsg);
    worker.onerror = (e) => {
      log(`worker error: ${e.message}`, 'err');
      if (!manualDisconnect) scheduleReconnect();
    };
  }

  worker.postMessage({ cmd: 'init', latencyMs });

  startDriftMonitor();

  let dgramBatch: Uint8Array[] = [];
  let flushPending = false;

  const flushDgrams = () => {
    flushPending = false;
    if (dgramBatch.length > 0 && worker) {
      const batch = dgramBatch;
      dgramBatch = [];
      worker.postMessage({ cmd: 'datagrams', batch });
    }
  };

  (async () => {
    const reader = wt.datagrams.readable.getReader();
    for (;;) {
      const { value, done } = await reader.read();
      if (done) {
        flushDgrams();
        log('datagram reader done', 'info');
        return;
      }
      dgramBatch.push(value);
      if (dgramBatch.length >= 16) {
        flushDgrams();
      } else if (!flushPending) {
        flushPending = true;
        setTimeout(flushDgrams, 0);
      }
    }
  })();
}

const MAX_PENDING_SENDS = 256;
let pendingSends: Uint8Array[] = [];
let draining = false;

function queueSend(data: Uint8Array) {
  pendingSends.push(data);
  if (pendingSends.length > MAX_PENDING_SENDS) {
    const dropped = pendingSends.splice(0, pendingSends.length - MAX_PENDING_SENDS);
    console.debug(`pendingSends overflow: dropped ${dropped.length} datagrams`);
  }
  if (!draining) drainSends();
}

async function drainSends() {
  draining = true;
  while (pendingSends.length > 0) {
    if (!datagramWriter) break;
    try { await datagramWriter.ready; } catch { break; }
    if (!datagramWriter) break;
    const data = pendingSends.shift()!;
    datagramWriter.write(data).then(
      () => {},
      (e) => log(`wt write failed: ${e}`, 'err'),
    );
  }
  draining = false;
}

function handleWorkerMsg(msg: WorkerMsg) {
  if (msg.type === 'batch') {
    for (const m of msg.msgs) handleWorkerMsg(m);
    return;
  }
  switch (msg.type) {
    case 'log':
      log(msg.msg, msg.cls);
      break;
    case 'handshakeComplete':
      log('SRT handshake complete ✓', 'ok');
      reconnectAttempts = 0;
      setStatus('SRT connected; awaiting video stream');
      break;
    case 'pmt':
      if (msg.audioPid >= 0 && msg.audioStreamType >= 0 && !audio) {
        const isOpus = msg.audioStreamType === 0x06;
        log(`audio PID ${msg.audioPid}: ${isOpus ? 'Opus' : 'AAC'} (stream type 0x${msg.audioStreamType.toString(16)})`, 'info');
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
    case 'send':
      queueSend(msg.data);
      break;
    case 'stats':
      updateStats(msg.stats);
      break;
    case 'close':
      log('SRT closed', 'err');
      setStatus('closed');
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
    `ACK/NAK  ${s.rxAck}/${s.rxNak}` +
    (latestDriftMs !== null
      ? `\ndrift    ${latestDriftMs >= 0 ? '+' : ''}${latestDriftMs.toFixed(0)}ms (video vs audio)`
      : '');
}

document.addEventListener('visibilitychange', () => {
  worker?.postMessage({ cmd: 'visibility', visible: !document.hidden });
});

if ((window as any).CERT_HASH !== undefined) {
  log((window as any).CERT_HASH ? 'Cert hash loaded — auto-connecting…' : 'mkcert mode — auto-connecting…', 'info');
  doConnect();
} else {
  log('No cert-hash.js. Start the gateway first, then reload.', 'info');
}
