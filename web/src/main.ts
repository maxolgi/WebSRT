// Simple viewer page entrypoint. Thin wrapper around mountPlayer() that wires
// the DOM (log/status/stats/controls) to the HTMLMediaElement-like PlayerHandle.

import { mountPlayer } from './player';
import type {
  PlayerState,
  PlayerStatsDetail,
  PlayerErrorDetail,
  PlayerResizeDetail,
} from './player';
import type { StatsMsg, DemuxStatsMsg } from './worker';
import { summarizePmt, type PmtEntry } from './shared/pmt';

const logEl = document.getElementById('log') as HTMLPreElement;
const statusEl = document.getElementById('status') as HTMLParagraphElement;
const statsEl = document.getElementById('stats') as HTMLPreElement;
const connectBtn = document.getElementById('connect') as HTMLButtonElement;
const canvas = document.getElementById('video-canvas') as HTMLCanvasElement;
const latencyNum = document.getElementById('latency-num') as HTMLInputElement;
const muteBtn = document.getElementById('mute') as HTMLButtonElement;
const fullscreenBtn = document.getElementById('fullscreen') as HTMLButtonElement;

let latestDriftMs: number | null = null;
let firstFrameLogged = false;

function log(msg: string, cls = '') {
  const lines = logEl.children;
  if (lines.length > 50) logEl.removeChild(lines[0]);
  const span = document.createElement('span');
  span.className = cls;
  span.textContent = msg + '\n';
  logEl.appendChild(span);
  logEl.scrollTop = logEl.scrollHeight;
}

function setStatus(s: string) { statusEl.textContent = s; }

function setMuteBtn() {
  muteBtn.classList.toggle('muted', handle.muted);
}

const savedLatency = localStorage.getItem('latency');
if (savedLatency) latencyNum.value = savedLatency;

const handle = mountPlayer(canvas, { latencyMs: +latencyNum.value || 120 });

connectBtn.addEventListener('click', () => {
  if (handle.state !== 'idle') handle.disconnect();
  else handle.connect().catch(() => {});
});

latencyNum.addEventListener('change', () => {
  const v = Math.max(20, Math.min(8000, +latencyNum.value || 120));
  latencyNum.value = String(v);
  localStorage.setItem('latency', String(v));
  handle.setLatencyMs(v);
});

muteBtn.addEventListener('click', () => {
  handle.setMuted(!handle.muted);
  setMuteBtn();
});

fullscreenBtn.addEventListener('click', () => {
  if (document.fullscreenElement) document.exitFullscreen();
  else canvas.requestFullscreen();
});

document.addEventListener('visibilitychange', () => {
  handle.getWorker()?.postMessage({ cmd: 'visibility', visible: !document.hidden });
});

const stateLabel: Record<PlayerState, string> = {
  idle: 'idle',
  connecting: 'connecting…',
  connected: 'connected',
  reconnecting: 'reconnecting…',
  error: 'error',
};

handle.addEventListener('statechange', (ev) => {
  const s = (ev as CustomEvent<PlayerState>).detail;
  connectBtn.textContent = s === 'idle' || s === 'error' ? 'connect' : 'disconnect';
  setStatus(stateLabel[s]);
  if (s === 'idle') {
    latestDriftMs = null;
    muteBtn.disabled = true;
  }
  const cls = s === 'connected' ? 'ok' : s === 'error' ? 'err' : 'info';
  log(`state: ${s}`, cls);
});

handle.addEventListener('playing', () => {
  muteBtn.disabled = false;
  setMuteBtn();
  setStatus(`decoding ${handle.videoWidth}x${handle.videoHeight}`);
});

handle.addEventListener('error', (ev) => {
  const { message } = (ev as CustomEvent<PlayerErrorDetail>).detail;
  setStatus(`error: ${message}`);
  log(message, 'err');
});

handle.addEventListener('stats', (ev) => {
  const { stats, demux } = (ev as CustomEvent<PlayerStatsDetail>).detail;
  updateStats(stats, demux);
});

handle.addEventListener('drift', (ev) => {
  latestDriftMs = (ev as CustomEvent<number>).detail;
});

handle.addEventListener('resize', (ev) => {
  if (firstFrameLogged) return;
  firstFrameLogged = true;
  const { width, height } = (ev as CustomEvent<PlayerResizeDetail>).detail;
  log(`first frame ${width}x${height}`, 'ok');
});

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
    `\npoll max ${s.pollMaxMs.toFixed(1)}ms` +
    `\nWASM hndl ${s.wasmHandleAvgUs.toFixed(1)}µs/call` +
    `\nWASM poll  ${s.wasmPollAvgUs.toFixed(1)}µs/call` +
    `\nloop avg  ${s.loopIterAvgMs.toFixed(2)}ms/iter` +
    (dmx ? `\ndemux    ${dmx}` : '') +
    (latestDriftMs !== null
      ? `\ndrift    ${latestDriftMs >= 0 ? '+' : ''}${latestDriftMs.toFixed(0)}ms (video vs audio)`
      : '');
}

/** Condensed one-line demux summary for the simple stats panel. */
function formatDemuxLine(d: DemuxStatsMsg | null): string {
  if (!d || d.pids.length === 0) return '';
  const pmtEntries: PmtEntry[] = [];
  for (let i = 0; i < d.pmtPids.length; i++) {
    pmtEntries.push({
      pid: d.pmtPids[i],
      streamType: d.pmtStreamTypes[i],
      formatId: d.pmtFormatIds[i] || null,
    });
  }
  const summary = summarizePmt(pmtEntries);
  const videoPid = summary.videoPid;
  const audioPid = summary.audioPid;
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

if ((window as any).CERT_HASH !== undefined) {
  log((window as any).CERT_HASH ? 'Cert hash loaded — auto-connecting…' : 'mkcert mode — auto-connecting…', 'info');
  handle.connect().catch(() => {});
} else {
  log('No cert-hash.js. Start the gateway first, then reload.', 'info');
}
