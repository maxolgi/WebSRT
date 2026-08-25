import { render } from 'preact';
import { DebugStore } from './debug/store';
import { DebugPanel } from './debug/components/Panel';
import { attachConsoleErrorCapture } from './debug/sampler';
import { mountPlayer } from './player';
import type { PlayerHandle, PlayerState, PlayerErrorDetail, PlayerResizeDetail } from './player';
import type { PublishCmd, PublishMsg, EncodeStats } from './stream-worker';
import type { StatsMsg } from './worker';
import {
  CODEC_CANDIDATES,
  detectCodec,
  AudioGraphManager,
  FramePump,
  attachDebugResizer,
  formatPubStats,
  hexToBytes,
} from './shared/publish';

// ─── DOM refs ─────────────────────────────────────────────────────

const previewEl = document.getElementById('preview') as HTMLVideoElement;
const sourceSelect = document.getElementById('source-select') as HTMLSelectElement;
const cameraSelect = document.getElementById('camera-select') as HTMLSelectElement;
const captureBtn = document.getElementById('capture-btn') as HTMLButtonElement;
const publishBtn = document.getElementById('publish-btn') as HTMLButtonElement;
const stopBtn = document.getElementById('stop-btn') as HTMLButtonElement;
const streamNameInput = document.getElementById('stream-name') as HTMLInputElement;
const latencyNum = document.getElementById('latency-num') as HTMLInputElement;
const codecSelect = document.getElementById('codec-select') as HTMLSelectElement;
const bitrateNum = document.getElementById('bitrate-num') as HTMLInputElement;
const framerateSelect = document.getElementById('framerate-select') as HTMLSelectElement;
const pubStatsText = document.getElementById('pub-stats-text') as HTMLDivElement;
const viewerLink = document.getElementById('viewer-link') as HTMLAnchorElement;
const audioSourceSelect = document.getElementById('audio-source') as HTMLSelectElement;
const playbackCanvas = document.getElementById('playback-canvas') as HTMLCanvasElement;
const playbackMuteBtn = document.getElementById('playback-mute') as HTMLButtonElement;
const playbackToggleBtn = document.getElementById('playback-toggle') as HTMLButtonElement;
const playbackLatencyNum = document.getElementById('playback-latency') as HTMLInputElement;

const debugRoot = document.getElementById('debug-root') as HTMLDivElement;

// ─── Debug panel (reused from viewer) ─────────────────────────────

const debugResizer = attachDebugResizer({ debugRoot });

const store = new DebugStore();
let panelMounted = false;
let consoleCleanup: (() => void) | null = null;

function log(msg: string, cls = '') { store.pushLog(msg, cls); }
function setStatus(s: string) { store.status.value = s; }

// ─── Stream-back viewer (round-trip from gateway) ─────────────────

let pbHandle: PlayerHandle | null = null;
let pbStream = '';

function mountPlayback(stream: string): void {
  pbHandle?.destroy();
  const h = mountPlayer(playbackCanvas, {
    stream,
    latencyMs: +playbackLatencyNum.value || 120,
    muted: true,
  });
  pbHandle = h;
  pbStream = stream;
  h.addEventListener('statechange', (ev) => {
    const s = (ev as CustomEvent<PlayerState>).detail;
    playbackToggleBtn.textContent = (s === 'idle' || s === 'error') ? 'play' : 'stop';
    if (s === 'connected') playbackMuteBtn.disabled = false;
    else { playbackMuteBtn.disabled = true; playbackMuteBtn.classList.add('muted'); }
  });
  h.addEventListener('resize', (ev) => {
    const { width, height } = (ev as CustomEvent<PlayerResizeDetail>).detail;
    store.pushLog(`[playback] first frame ${width}x${height}`, 'ok');
  });
  h.addEventListener('playing', () => store.pushLog('[playback] playing', 'info'));
  h.addEventListener('error', (ev) => {
    const { message } = (ev as CustomEvent<PlayerErrorDetail>).detail;
    store.pushLog(`[playback] ${message}`, 'err');
  });
}

function playbackActive(): boolean {
  return pbHandle !== null && pbHandle.state !== 'idle' && pbHandle.state !== 'error';
}

playbackLatencyNum.addEventListener('change', () => {
  pbHandle?.setLatencyMs(+playbackLatencyNum.value);
});

playbackMuteBtn.addEventListener('click', () => {
  if (!pbHandle) return;
  pbHandle.setMuted(!pbHandle.muted);
  playbackMuteBtn.classList.toggle('muted', pbHandle.muted);
});

function setPanelVisible(visible: boolean) {
  store.panelVisible.value = visible;
  debugRoot.classList.toggle('visible', visible);
  document.body.classList.toggle('debug-open', visible);
  if (visible) {
    debugResizer.sync();
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
let capturing = false;
let detectedCodec: string | null = null;
let detectedCodecLabel = '';

const audio = new AudioGraphManager((msg) => log(msg, 'err'));

const framePump = new FramePump({
  getPreviewEl: () => previewEl,
  isPublishing: () => publishing && worker !== null,
  postFrame: (frame) => {
    const cmd: PublishCmd = { cmd: 'frame', frame };
    worker!.postMessage(cmd, [frame]);
  },
  getFps: () => +framerateSelect.value,
});

// ─── Codec auto-detection ─────────────────────────────────────────

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
let cameraOptions: HTMLOptionElement[] = [];

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

async function enumerateCameras() {
  try {
    const devices = await navigator.mediaDevices.enumerateDevices();
    const cameras = devices.filter((d) => d.kind === 'videoinput');
    cameraOptions = cameras.map((c) => {
      const opt = document.createElement('option');
      opt.value = c.deviceId;
      opt.textContent = c.label || `Camera ${c.deviceId.slice(0, 8)}`;
      return opt;
    });
    populateCameraSelect();
  } catch { /* ignore */ }
}

function populateCameraSelect() {
  const current = cameraSelect.value;
  cameraSelect.innerHTML = '';
  for (const opt of cameraOptions) {
    cameraSelect.appendChild(opt.cloneNode(true) as HTMLOptionElement);
  }
  cameraSelect.value = current || cameraOptions[0]?.value || '';
}

// ─── Source capture (screen / webcam) ──────────────────────────────

sourceSelect.addEventListener('change', () => {
  const isWebcam = sourceSelect.value === 'webcam';
  cameraSelect.style.display = isWebcam ? '' : 'none';
  cameraSelect.disabled = !isWebcam;
  if (isWebcam && cameraOptions.length === 0) {
    enumerateCameras();
  }
});

async function startCapture(): Promise<void> {
  const source = sourceSelect.value;
  let stream: MediaStream;

  if (source === 'screen') {
    stream = await navigator.mediaDevices.getDisplayMedia({
      video: { frameRate: { ideal: +framerateSelect.value } },
      audio: true,
    });
    await enumerateMics();
  } else {
    const deviceId = cameraSelect.value || undefined;
    stream = await navigator.mediaDevices.getUserMedia({
      video: {
        ...(deviceId ? { deviceId: { exact: deviceId } } : {}),
        width: { ideal: 1920 },
        height: { ideal: 1080 },
        aspectRatio: { ideal: 16 / 9 },
        frameRate: { ideal: +framerateSelect.value },
      },
      audio: true,
    });
    // Re-enumerate cameras now that permission is granted (labels populate)
    await enumerateCameras();
    // Auto-pair the webcam's mic in the audio dropdown. The audio track
    // from this getUserMedia is stopped — connectAudioSource() will
    // re-acquire the mic at publish time via the existing deviceId path.
    const audioTrack = stream.getAudioTracks()[0];
    if (audioTrack) {
      const micDeviceId = audioTrack.getSettings().deviceId;
      audioTrack.stop();
      stream.removeTrack(audioTrack);
      if (micDeviceId) {
        await enumerateMics();
        audioSourceSelect.value = micDeviceId;
      }
    }
  }

  // Stop any previous capture before wiring the new one.
  if (captureStream) {
    captureStream.getTracks().forEach((t) => t.stop());
  }

  captureStream = stream;
  previewEl.srcObject = stream;
  await previewEl.play();

  stream.getVideoTracks()[0]?.addEventListener('ended', () => {
    stopCapture();
  });

  capturing = true;
  captureBtn.textContent = 'Stop Capture';
  publishBtn.disabled = false;

  const vTrack = stream.getVideoTracks()[0];
  const settings = vTrack?.getSettings();
  const w = settings?.width ?? 1280;
  const h = settings?.height ?? 720;
  log(`Captured ${w}x${h} (${source})`, 'info');
  setStatus(`${source === 'screen' ? 'screen' : 'webcam'} captured — ready to publish`);

  const fps = +framerateSelect.value;
  const br = +bitrateNum.value;
  const detected = await detectCodec(w, h, fps, br);
  if (detected) {
    detectedCodec = detected.codec;
    detectedCodecLabel = detected.label;
    log(`Codec auto-detected: ${detected.label} (${detected.codec})`, 'info');
  } else {
    log('No supported codec found!', 'err');
  }
}

function stopCapture(): void {
  if (publishing) stopAll();
  if (captureStream) {
    captureStream.getTracks().forEach((t) => t.stop());
  }
  captureStream = null;
  previewEl.srcObject = null;
  capturing = false;
  captureBtn.textContent = 'Capture';
  publishBtn.disabled = true;
  populateAudioSources();
  setStatus('capture stopped');
}

captureBtn.addEventListener('click', async () => {
  if (capturing) {
    stopCapture();
    return;
  }
  try {
    await startCapture();
  } catch (e) {
    log(`Capture failed: ${e}`, 'err');
    setStatus('capture failed');
  }
});

// ─── Publishing ───────────────────────────────────────────────────

publishBtn.addEventListener('click', async () => {
  if (publishing) return;
  if (!captureStream) { log('Capture a screen first', 'err'); return; }

  publishing = true;
  publishBtn.disabled = true;
  stopBtn.disabled = false;
  viewerLink.style.display = '';
  const sn = streamNameInput.value || 'default';
  const viewerUrl = `${location.origin}/?stream=${encodeURIComponent(sn)}`;
  viewerLink.href = viewerUrl;
  viewerLink.textContent = viewerUrl;
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
    await audio.setup();
    await audio.connect(audioSourceSelect.value, {
      allowTabAudio: true,
      getTabAudioTracks: () => captureStream?.getAudioTracks() ?? [],
    });
    await audio.resumeIfSuspended();
  }

  // Cert hash
  const hashHex = (window as any).CERT_HASH as string | null | undefined;
  let certHash: Uint8Array | null = null;
  if (hashHex) certHash = hexToBytes(hashHex);

  // Build WT URL
  const pageHost = location.hostname || '127.0.0.1';
  const urlParams = new URLSearchParams(location.search);
  const wtHost = urlParams.get('host') || (pageHost === 'localhost' ? '127.0.0.1' : pageHost);
  const wtPort = urlParams.get('port') || (window as any).WT_PORT || '4433';
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
  if (audioSource) {
    audio.transferPort(worker);
  }

  log(`Publishing to ${wtUrl} (${isAv1 ? 'AV1' : 'H.264'})`, 'info');
  log(`latency: ${latencyMs}ms, bitrate: ${bitrate}Mbps, fps: ${framerate}`, 'info');
});

stopBtn.addEventListener('click', () => stopAll());

function stopAll() {
  publishing = false;
  framePump.stop();
  pbHandle?.disconnect();
  viewerLink.style.display = 'none';
  if (worker) {
    worker.postMessage({ cmd: 'stop' } as PublishCmd);
    worker.terminate();
    worker = null;
  }
  audio.close();
  publishBtn.disabled = !captureStream;
  stopBtn.disabled = true;
  framePump.resetCredits();
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
      framePump.credit();
      break;
    case 'wtReady':
      log('WT connected', 'ok');
      setStatus('WT ready; awaiting SRT handshake');
      break;
    case 'handshakeComplete':
      log('SRT handshake complete', 'ok');
      setStatus('LIVE');
      framePump.start();
      if (!playbackActive()) {
        const sn = streamNameInput.value || 'default';
        if (!pbHandle || pbStream !== sn) mountPlayback(sn);
        store.pushLog('[playback] auto-connecting stream-back…', 'info');
        pbHandle?.connect().catch(() => {});
      }
      break;
    case 'close':
      log('SRT closed', 'err');
      setStatus('closed');
      framePump.stop();
      break;
    case 'wtClosed':
      if (msg.error) log(`WT closed: ${msg.error}`, 'err');
      else log('WT closed', 'info');
      setStatus('disconnected');
      framePump.stop();
      break;
    case 'stats':
      store.srtStats.value = msg.stats;
      if (msg.encode) updateEncodeStats(msg.stats, msg.encode);
      break;
  }
}

function updateEncodeStats(srt: StatsMsg, enc: EncodeStats) {
  pubStatsText.innerHTML = formatPubStats(srt, enc, framerateVal());
}

function framerateVal(): number { return +framerateSelect.value; }

// ─── Misc handlers ────────────────────────────────────────────────

document.getElementById('debug-toggle')?.addEventListener('click', () => {
  setPanelVisible(!store.panelVisible.value);
});

document.addEventListener('visibilitychange', () => {
  if (publishing) framePump.schedule();
});

playbackToggleBtn.addEventListener('click', () => {
  if (playbackActive()) {
    pbHandle?.disconnect();
  } else {
    const sn = streamNameInput.value || 'default';
    if (!pbHandle || pbStream !== sn) mountPlayback(sn);
    pbHandle?.connect().catch(() => {});
  }
});

// ─── Init ─────────────────────────────────────────────────────────

// Persistent stream key: load from localStorage or generate a random 8-char one.
const STREAM_KEY_KEY = 'websrt-stream-name';
const savedName = localStorage.getItem(STREAM_KEY_KEY);
streamNameInput.value = savedName ?? Array.from(
  { length: 8 },
  () => Math.floor(Math.random() * 36).toString(36),
).join('');
localStorage.setItem(STREAM_KEY_KEY, streamNameInput.value);
streamNameInput.addEventListener('change', () => {
  localStorage.setItem(STREAM_KEY_KEY, streamNameInput.value);
});

populateCodecSelect();
enumerateMics();
enumerateCameras();
navigator.mediaDevices?.addEventListener('devicechange', () => {
  enumerateMics();
  enumerateCameras();
});

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
