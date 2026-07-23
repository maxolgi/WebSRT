// Simple viewer page entrypoint. Thin wrapper around createViewer() that
// injects DOM-backed UI sinks (log <pre>, status <p>, stats <pre>).

import type { StatsMsg, DemuxStatsMsg } from './worker';
import { summarizePmt, type PmtEntry } from './shared/pmt';
import { createViewer, type ConnectionState } from './shared/viewer';

const logEl = document.getElementById('log') as HTMLPreElement;
const statusEl = document.getElementById('status') as HTMLParagraphElement;
const connectBtn = document.getElementById('connect') as HTMLButtonElement;
const canvas = document.getElementById('video-canvas') as HTMLCanvasElement;
const latencyNum = document.getElementById('latency-num') as HTMLInputElement;
const statsEl = document.getElementById('stats') as HTMLPreElement;
const muteBtn = document.getElementById('mute') as HTMLButtonElement;
const fullscreenBtn = document.getElementById('fullscreen') as HTMLButtonElement;

// Tracks the most recent A/V drift sample (mirrors the closure state inside
// the viewer; surfaced via onDrift so updateStats() can render it).
let latestDriftMs: number | null = null;

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

function onStateChange(s: ConnectionState) {
  if (s === 'connected') connectBtn.textContent = 'stop';
  else if (s === 'connecting') connectBtn.textContent = 'connecting…';
  else connectBtn.textContent = 'connect';
}

const savedLatency = localStorage.getItem('latency');
if (savedLatency) latencyNum.value = savedLatency;
latencyNum.addEventListener('input', () => {
  const v = Math.max(20, Math.min(8000, +latencyNum.value || 120));
  latencyNum.value = String(v);
  localStorage.setItem('latency', String(v));
});

const viewer = createViewer({
  canvas,
  latencyInput: latencyNum,
  muteBtn,
  ui: {
    log,
    setStatus,
    onStateChange,
    onFirstFrame: (w, h) => {
      log(`first frame decoded ✓ (${w}x${h})`, 'ok');
      setStatus(`decoding ${w}x${h}`);
    },
    onVideoConfigured: (info) =>
      log(`VideoDecoder configured (profile ${info.profile}, level ${info.level})`, 'info'),
    onAudioReady: () => log('AudioDecoder ready', 'info'),
    onStats: (s, demux) => updateStats(s, demux),
    onDrift: (driftMs) => { latestDriftMs = driftMs; },
  },
});

connectBtn.addEventListener('click', () => {
  if (viewer.isActive()) {
    viewer.disconnect();
    setStatus('disconnected');
  } else {
    viewer.connect();
  }
});

// muteBtn click handler is registered inside the viewer (it owns audioEl).

fullscreenBtn.addEventListener('click', () => {
  if (document.fullscreenElement) document.exitFullscreen();
  else canvas.requestFullscreen();
});

document.addEventListener('visibilitychange', () => {
  viewer.onVisibilityChange(!document.hidden);
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
  viewer.connect();
} else {
  log('No cert-hash.js. Start the gateway first, then reload.', 'info');
}
