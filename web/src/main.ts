import { VideoPipeline, OpusAudioPipeline, AacAudioPipeline } from './decode';
import { CanvasRenderer } from './render';
import type { WorkerMsg, StatsMsg, DemuxStatsMsg } from './worker';

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
  if (worker) {
    worker.postMessage({ cmd: 'stop' });
    worker.terminate();
    worker = null;
  }
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
  const streamName = urlParams.get('stream') || 'default';
  const qp = new URLSearchParams({ stream: streamName });
  if (authToken) qp.set('token', authToken);
  const wtUrl = `https://${wtHost}:${wtPort}/wt?${qp}`;

  const latencyMs = +latencyNum.value;
  log(`TSBPD latency: ${formatLatency(latencyMs)}`, 'info');

  const certHash = hashHex ? hexToBytes(hashHex) : null;
  const hashLabel = hashHex ? `self-signed, hash ${hashHex.slice(0, 8)}…` : 'mkcert/PKI';
  log(`connecting to ${wtUrl} (${hashLabel}) …`, 'info');

  if (!worker) {
    worker = new Worker(new URL('./worker.ts', import.meta.url), { type: 'module' });
    worker.onmessage = (e: MessageEvent) => handleWorkerMsg(e.data as WorkerMsg);
    worker.onerror = (e) => {
      log(`worker error: ${e.message}`, 'err');
      if (!manualDisconnect) scheduleReconnect();
    };
  }

  worker.postMessage(
    { cmd: 'init', url: wtUrl, certHash, latencyMs },
    certHash ? [certHash.buffer as ArrayBuffer] : [],
  );

  startDriftMonitor();
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
      // PMT always precedes PES — set the codec hint before any videoPes
      // arrives so VideoPipeline routes AV1 OBU payloads correctly.
      if (msg.videoPid >= 0) {
        video?.setCodecHint(msg.videoCodec);
      }
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
    case 'wtReady':
      log('WT ready ✓', 'ok');
      setStatus('WT ready; awaiting SRT handshake');
      setConnState('connected');
      break;
    case 'wtClosed':
      if (msg.error) log(`WT closed (err): ${msg.error}`, 'err');
      else log('WT closed', 'info');
      setStatus('closed');
      if (!manualDisconnect) scheduleReconnect();
      break;
    case 'stats':
      updateStats(msg.stats, msg.demux ?? null);
      break;
    case 'close':
      log('SRT closed', 'err');
      setStatus('closed');
      if (!manualDisconnect) scheduleReconnect();
      break;
  }
}

function updateStats(s: StatsMsg, demux: DemuxStatsMsg | null) {
  const lossRate = (s.rxData + s.rxLoss) > 0
    ? ((s.rxLoss / (s.rxData + s.rxLoss)) * 100).toFixed(2)
    : '0.00';
  const mbps = (s.bandwidthBps / 1e6).toFixed(1);
  const elapsed = (s.elapsedMs / 1000).toFixed(0);
  const dmx = formatDemuxLine(demux);
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
    (dmx ? `\ndemux    ${dmx}` : '') +
    (latestDriftMs !== null
      ? `\ndrift    ${latestDriftMs >= 0 ? '+' : ''}${latestDriftMs.toFixed(0)}ms (video vs audio)`
      : '');
}

const ST_H264_MAIN = 0x1b;
const ST_HEVC_MAIN = 0x24;
const ST_AAC_MAIN = 0x0f;
const ST_PRIVATE_MAIN = 0x06;

/** Condensed one-line demux summary for the simple stats panel. */
function formatDemuxLine(d: DemuxStatsMsg | null): string {
  if (!d || d.pids.length === 0) return '';
  let videoPid = -1;
  let audioPid = -1;
  for (let i = 0; i < d.pmtPids.length; i++) {
    const st = d.pmtStreamTypes[i];
    if (st === ST_H264_MAIN || st === ST_HEVC_MAIN) videoPid = d.pmtPids[i];
    else if (st === ST_AAC_MAIN) audioPid = d.pmtPids[i];
    else if (st === ST_PRIVATE_MAIN) {
      const fmt = d.pmtFormatIds[i];
      if (fmt === 'AV01') videoPid = d.pmtPids[i];
      else if (fmt === 'Opus') audioPid = d.pmtPids[i];
    }
  }
  let videoMbps = 0;
  let audioKbps = 0;
  let ccErrors = 0;
  for (let i = 0; i < d.pids.length; i++) {
    if (d.pids[i] === videoPid) videoMbps = d.bitratesMbps[i];
    else if (d.pids[i] === audioPid) audioKbps = d.bitratesMbps[i] * 1000;
    ccErrors += d.ccErrors[i];
  }
  const demuxErrs = d.errorMsg.length;
  return `video ${videoMbps.toFixed(1)} Mbps • audio ${audioKbps.toFixed(0)} kbps • CC errors ${ccErrors} • demux errors ${demuxErrs}`;
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
