// Advanced page entry point. Same SRT/WebTransport pipeline as main.ts, plus
// a Preact-based debug panel overlay with codec/GPU/SRT/devtools tabs.
//
// The debug panel is mounted when the page loads (setPanelVisible(true) at
// the bottom). All viewer lifecycle is delegated to createViewer(); this file
// only contains debug-panel-specific glue (resizer, latency-reconnect, test
// actions, store-backed UI sinks).

import { render } from 'preact';
import { DebugStore } from './debug/store';
import { DebugPanel } from './debug/components/Panel';
import { startSampler, attachConsoleErrorCapture } from './debug/sampler';
import { createViewer, type ConnectionState } from './shared/viewer';

const connectBtn = document.getElementById('connect') as HTMLButtonElement;
const canvas = document.getElementById('video-canvas') as HTMLCanvasElement;
const latencyNum = document.getElementById('latency-num') as HTMLInputElement;
const muteBtn = document.getElementById('mute') as HTMLButtonElement;
const debugRoot = document.getElementById('debug-root') as HTMLDivElement;
const fullscreenBtn = document.getElementById('fullscreen') as HTMLButtonElement;

// --- Debug panel resize handle (left edge, horizontal) ---
// Lives on document.body, NOT inside #debug-root, so Preact's diff never
// touches it. Positioned fixed at the panel's left border.
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

// --- Debug store + panel ---
const store = new DebugStore();
let samplerCleanup: (() => void) | null = null;
let panelMounted = false;

function log(msg: string, cls = '') {
  store.pushLog(msg, cls);
}

function setStatus(s: string) {
  store.status.value = s;
}

function onStateChange(s: ConnectionState) {
  if (s === 'connected') connectBtn.textContent = 'stop';
  else if (s === 'connecting') connectBtn.textContent = 'connecting…';
  else connectBtn.textContent = 'connect';
}

const savedLatency = localStorage.getItem('latency');
if (savedLatency) latencyNum.value = savedLatency;
store.latencyMs.value = +latencyNum.value;
latencyNum.addEventListener('change', () => {
  const v = Math.max(20, Math.min(8000, +latencyNum.value || 120));
  latencyNum.value = String(v);
  localStorage.setItem('latency', String(v));
  const prev = store.latencyMs.value;
  store.latencyMs.value = v;
  if (v !== prev && viewer.isActive()) {
    log(`Latency changed to ${v}ms — reconnecting…`, 'info');
    viewer.disconnect();
    setTimeout(() => viewer.connect(), 100);
  }
});

const viewer = createViewer({
  canvas,
  latencyInput: latencyNum,
  muteBtn,
  baseReconnectDelayMs: 500,
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
    onAudioReady: () => { /* no log line — advanced.tsx is the polished page */ },
    onStats: (stats, demux) => {
      store.srtStats.value = stats;
      if (demux) store.demuxStats.value = demux;
    },
    onDrift: (driftMs) => { store.driftMs.value = driftMs; },
    onCertMode: (mode) => { store.certMode.value = mode; },
    onWorkerReady: (worker) => {
      // If panel is open, request high-frequency stats from the worker.
      if (store.panelVisible.value) worker.postMessage({ cmd: 'debug-rate', ms: 250 });
    },
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

fullscreenBtn.addEventListener('click', () => {
  if (document.fullscreenElement) document.exitFullscreen();
  else canvas.requestFullscreen();
});

document.getElementById('debug-toggle')?.addEventListener('click', () => {
  setPanelVisible(!store.panelVisible.value);
});

document.addEventListener('visibilitychange', () => {
  viewer.onVisibilityChange(!document.hidden);
});

// --- Debug panel toggle ---
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
      samplerCleanup = startSampler(store, () => ({
        video: viewer.getVideo(),
        audio: viewer.getAudio(),
        renderer: viewer.getRenderer(),
      }));
      attachConsoleErrorCapture(store);
      viewer.getWorker()?.postMessage({ cmd: 'debug-rate', ms: 250 });
    }
  } else {
    document.body.style.paddingRight = '';
    localStorage.removeItem('websrt-debug-open');
    if (samplerCleanup) {
      samplerCleanup();
      samplerCleanup = null;
      viewer.getWorker()?.postMessage({ cmd: 'debug-rate', ms: 1000 });
    }
  }
}

// Register test actions for the debug panel's Test tab
store.testActions.value = {
  resetDecoder: () => {
    viewer.getVideo()?.reset();
    log('VideoDecoder reset — will re-sync on next keyframe', 'info');
  },
  reconnect: () => {
    log('Manual reconnect triggered', 'info');
    viewer.disconnect();
    setTimeout(() => viewer.connect(), 100);
  },
  cycleLatency: () => {
    const current = +latencyNum.value;
    const next = current >= 2000 ? 120 : current >= 500 ? 2000 : 500;
    latencyNum.value = String(next);
    localStorage.setItem('latency', String(next));
    store.latencyMs.value = next;
    log(`Latency cycled to ${next}ms (reconnect to apply)`, 'info');
  },
  setHwMode: (mode) => {
    const v = viewer.getVideo();
    if (!v) {
      log('Cannot switch hw mode — no active VideoPipeline', 'err');
      return;
    }
    v.setHwMode(mode);
    log(`VideoDecoder hw preference → ${mode} (applies on next feed())`, 'info');
  },
};

if ((window as any).CERT_HASH !== undefined) {
  log((window as any).CERT_HASH ? 'Cert hash loaded — auto-connecting…' : 'mkcert mode — auto-connecting…', 'info');
  store.certMode.value = (window as any).CERT_HASH ? 'self' : 'mkcert';
  setTimeout(() => viewer.connect(), 500);
} else {
  log('No cert-hash.js. Start the gateway first, then reload.', 'info');
}

setPanelVisible(true);
