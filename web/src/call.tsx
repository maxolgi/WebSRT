import { render } from 'preact';
import { DebugStore } from './debug/store';
import { DebugPanel } from './debug/components/Panel';
import { attachConsoleErrorCapture } from './debug/sampler';
import { mountPlayer } from './player';
import type { PlayerHandle, PlayerState, PlayerErrorDetail, PlayerResizeDetail } from './player';
import type { PublishCmd, PublishMsg, EncodeStats } from './stream-worker';
import type { StatsMsg } from './worker';

// 1:1 video call page. Same building blocks as stream.html, re-targeted:
//   - publishes webcam+mic via stream-worker.ts (unchanged)
//   - plays the peer back via mountPlayer (unchanged)
//   - seat protocol: room "<name>" → directional streams "<name>-1" / "<name>-2".
//     Joining claims seat 1; if the name is held (WT closes before the SRT
//     handshake completes), fall back to seat 2. Seat 1 plays seat 2's
//     stream and vice versa. Both seats taken → room full.

// ─── Call defaults ────────────────────────────────────────────────

const CALL_FPS = 30;
const CALL_BITRATE_MBPS = 2;
const CALL_LATENCY_MS = 100;
const AUDIO_BITRATE = 128000;
const AUDIO_CHANNELS = 2;

// ─── DOM refs ─────────────────────────────────────────────────────

const prejoinEl = document.getElementById('prejoin') as HTMLDivElement;
const callEl = document.getElementById('call') as HTMLDivElement;
const previewEl = document.getElementById('preview') as HTMLVideoElement;
const pipEl = document.getElementById('pip') as HTMLDivElement;
const roomNameInput = document.getElementById('room-name') as HTMLInputElement;
const cameraSelect = document.getElementById('camera-select') as HTMLSelectElement;
const audioSourceSelect = document.getElementById('audio-source') as HTMLSelectElement;
const joinBtn = document.getElementById('join-btn') as HTMLButtonElement;
const prejoinError = document.getElementById('prejoin-error') as HTMLDivElement;
const remoteCanvas = document.getElementById('remote-canvas') as HTMLCanvasElement;
const callStatusEl = document.getElementById('call-status') as HTMLDivElement;
const pubStatsText = document.getElementById('pub-stats-text') as HTMLDivElement;
const micBtn = document.getElementById('mic-btn') as HTMLButtonElement;
const camBtn = document.getElementById('cam-btn') as HTMLButtonElement;
const speakerBtn = document.getElementById('speaker-btn') as HTMLButtonElement;
const hangupBtn = document.getElementById('hangup-btn') as HTMLButtonElement;

const debugRoot = document.getElementById('debug-root') as HTMLDivElement;

// ─── Debug panel (reused from viewer/stream) ──────────────────────

const PANEL_MIN_W = 320;
const PANEL_MAX_W_RATIO = 0.85;
const resizer = document.createElement('div');
resizer.className = 'debug-resizer visible';
document.body.appendChild(resizer);

function syncResizerPosition() {
  const w = debugRoot.offsetWidth;
  resizer.style.right = `${w}px`;
  document.body.style.paddingRight = `${w + 16}px`;
  // Fixed-position call view ignores body padding — inset it explicitly.
  callEl.style.right = `${w}px`;
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
    callEl.style.right = `${w}px`;
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
function callStatus(s: string) { callStatusEl.textContent = s; }

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
    callEl.style.right = '';
    localStorage.removeItem('websrt-debug-open');
  }
}

// ─── State ────────────────────────────────────────────────────────

let worker: Worker | null = null;
let captureStream: MediaStream | null = null;
let publishing = false;
let capturing = false;
let credits = 0;
let rafId: number | null = null;
let bgWorker: Worker | null = null;

// Seat-claim state
let room = '';
let claimedSeat = 0;        // 0 = not claimed yet, 1|2 once handshake completes
let tryingSeat = 0;         // seat the current publish attempt targets
let claimGen = 0;           // cancels in-flight attempts on stop
let seatRetryTimer: ReturnType<typeof setTimeout> | null = null;

// Remote playback
let pbHandle: PlayerHandle | null = null;
let pbStream = '';

// Audio
let audioCtx: AudioContext | null = null;
let workletNode: AudioWorkletNode | null = null;
let micMuted = false;
let camOff = false;

const TIMER_WORKER_SRC = `
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
let detectedCodec: string | null = null;
let detectedCodecLabel = '';

// ─── Codec auto-detection (from stream.tsx) ───────────────────────

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

// ─── Device enumeration ───────────────────────────────────────────

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
    populateMicSelect();
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

function populateMicSelect() {
  const current = audioSourceSelect.value;
  audioSourceSelect.innerHTML = '';
  for (const opt of micOptions) {
    audioSourceSelect.appendChild(opt.cloneNode(true) as HTMLOptionElement);
  }
  audioSourceSelect.value = current || micOptions[0]?.value || '';
}

function populateCameraSelect() {
  const current = cameraSelect.value;
  cameraSelect.innerHTML = '';
  for (const opt of cameraOptions) {
    cameraSelect.appendChild(opt.cloneNode(true) as HTMLOptionElement);
  }
  cameraSelect.value = current || cameraOptions[0]?.value || '';
}

// ─── AudioWorklet capture (from stream.tsx) ───────────────────────

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
    micStream = await navigator.mediaDevices.getUserMedia({
      audio: { deviceId: { exact: src } },
    });
    // Apply pending mute state to the freshly acquired mic.
    applyMicMute();
    audioSourceNode = audioCtx.createMediaStreamSource(micStream);
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

// ─── Webcam capture ───────────────────────────────────────────────

async function startCapture(pairMic = true): Promise<void> {
  const deviceId = cameraSelect.value || undefined;
  const stream = await navigator.mediaDevices.getUserMedia({
    video: {
      ...(deviceId ? { deviceId: { exact: deviceId } } : {}),
      width: { ideal: 1920 },
      height: { ideal: 1080 },
      aspectRatio: { ideal: 16 / 9 },
      frameRate: { ideal: CALL_FPS },
    },
    audio: pairMic,
  });

  // Re-enumerate now that permission is granted (labels populate).
  await enumerateCameras();

  // Auto-pair the webcam's mic on the initial capture. The audio track from
  // this getUserMedia is stopped — connectAudioSource() re-acquires the mic
  // at join time via the deviceId path (same pattern as stream.tsx).
  if (pairMic) {
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
    if (capturing) stopCapture();
  });

  capturing = true;
  camOff = false;
  camBtn.textContent = 'cam on';

  const vTrack = stream.getVideoTracks()[0];
  const settings = vTrack?.getSettings();
  const w = settings?.width ?? 1280;
  const h = settings?.height ?? 720;
  log(`Camera captured ${w}x${h}`, 'info');

  const codec = await detectCodec(w, h, CALL_FPS, CALL_BITRATE_MBPS);
  if (codec) {
    log(`Codec auto-detected: ${detectedCodecLabel} (${codec})`, 'info');
  } else {
    log('No supported codec found!', 'err');
  }
}

function stopCapture(): void {
  if (capturing) stopAll();
  if (captureStream) {
    captureStream.getTracks().forEach((t) => t.stop());
  }
  captureStream = null;
  previewEl.srcObject = null;
  capturing = false;
}

cameraSelect.addEventListener('change', async () => {
  if (!capturing || publishing) return;
  try {
    await startCapture(false);
  } catch (e) {
    log(`Camera switch failed: ${e}`, 'err');
  }
});

// ─── Frame pump (from stream.tsx) ─────────────────────────────────

function startFramePump() {
  credits = 4;
  pumpFrame();
}

function pumpFrame() {
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
  schedulePump();
}

function schedulePump() {
  if (!publishing) return;
  if (document.hidden) {
    if (rafId !== null) { cancelAnimationFrame(rafId); rafId = null; }
    if (bgWorker === null) {
      bgWorker = new Worker(URL.createObjectURL(new Blob([TIMER_WORKER_SRC], { type: 'application/javascript' })));
      bgWorker.onmessage = () => pumpFrame();
      bgWorker.postMessage(1000 / CALL_FPS);
    }
  } else {
    if (bgWorker !== null) { bgWorker.terminate(); bgWorker = null; }
    if (rafId !== null) { cancelAnimationFrame(rafId); rafId = null; }
    rafId = 'requestVideoFrameCallback' in previewEl
      ? (previewEl as unknown as { requestVideoFrameCallback: (cb: FrameRequestCallback) => number })
        .requestVideoFrameCallback(pumpFrame as FrameRequestCallback)
      : requestAnimationFrame(pumpFrame);
  }
}

function stopFramePump() {
  if (rafId !== null) {
    cancelAnimationFrame(rafId);
    rafId = null;
  }
  if (bgWorker !== null) {
    bgWorker.terminate();
    bgWorker = null;
  }
}

// ─── Seat-claim publish loop ──────────────────────────────────────

function seatStreamName(seat: number): string {
  return `${room}-${seat}`;
}

function buildWtUrl(publishStream: string): string {
  const pageHost = location.hostname || '127.0.0.1';
  const urlParams = new URLSearchParams(location.search);
  const wtHost = urlParams.get('host') || (pageHost === 'localhost' ? '127.0.0.1' : pageHost);
  const wtPort = urlParams.get('port') || (window as any).WT_PORT || '4433';
  const authToken = urlParams.get('token');
  const qp = new URLSearchParams({ publish: publishStream });
  if (authToken) qp.set('token', authToken);
  return `https://${wtHost}:${wtPort}/wt?${qp}`;
}

async function trySeat(seat: number, attemptGen: number): Promise<void> {
  if (attemptGen !== claimGen) return;

  tryingSeat = seat;
  const publishStream = seatStreamName(seat);
  log(`Claiming seat ${seat} (${publishStream})…`, 'info');
  callStatus(`joining as seat ${seat}…`);

  const vTrack = captureStream?.getVideoTracks()[0];
  const settings = vTrack?.getSettings();
  const width = settings?.width ?? 1280;
  const height = settings?.height ?? 720;
  const chosenCodec = detectedCodec ?? 'avc1.640028';

  const url = buildWtUrl(publishStream);

  worker = new Worker(new URL('./stream-worker.ts', import.meta.url), { type: 'module' });
  worker.onmessage = (e: MessageEvent) => handlePublishMsg(e.data as PublishMsg, attemptGen);
  worker.onerror = (e) => { log(`worker error: ${e.message}`, 'err'); };

  const cmd: PublishCmd = {
    cmd: 'init',
    url,
    certHash: certHashForUrl(),
    latencyMs: CALL_LATENCY_MS,
    video: { codec: chosenCodec, width, height, bitrate: CALL_BITRATE_MBPS, framerate: CALL_FPS },
    audio: { bitrate: AUDIO_BITRATE, channels: AUDIO_CHANNELS },
  };
  const transfer: ArrayBuffer[] = [];
  const ch = cmd.certHash;
  if (ch) transfer.push(ch.buffer as ArrayBuffer);
  worker.postMessage(cmd, transfer);

  if (workletNode) {
    const port = workletNode.port;
    worker.postMessage({ cmd: 'audio-port', port } as PublishCmd, [port]);
  }

  log(`Publishing to ${publishStream} (${chosenCodec.startsWith('av01') ? 'AV1' : 'H.264'} ${width}x${height} @${CALL_FPS}fps ${CALL_BITRATE_MBPS}Mbps)`, 'info');
}

// certHash parsed once per attempt (the buffer is transferred to the worker).
function certHashForUrl(): Uint8Array | null {
  const hashHex = (window as any).CERT_HASH as string | null | undefined;
  return hashHex ? hexToBytes(hashHex) : null;
}

function handleClaimFailure(attemptGen: number) {
  if (attemptGen !== claimGen) return;
  if (worker) {
    worker.postMessage({ cmd: 'stop' } as PublishCmd);
    worker.terminate();
    worker = null;
  }
  if (tryingSeat === 1) {
    const delay = 200 + Math.random() * 300;
    log(`Seat 1 taken — retrying as seat 2 in ${Math.round(delay)}ms`, 'info');
    seatRetryTimer = setTimeout(() => {
      seatRetryTimer = null;
      trySeat(2, attemptGen);
    }, delay);
  } else {
    log('Room full (both seats taken)', 'err');
    prejoinError.textContent = 'Room is full (both seats taken).';
    stopAll();
  }
}

function handlePublishMsg(msg: PublishMsg, attemptGen: number) {
  if (attemptGen !== claimGen) return;
  if (msg.type === 'batch') {
    for (const m of msg.msgs) handlePublishMsg(m, attemptGen);
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
      claimedSeat = tryingSeat;
      log(`SRT handshake complete — seat ${claimedSeat} claimed`, 'ok');
      setStatus('LIVE');
      callStatus(`seat ${claimedSeat} — waiting for peer…`);
      startFramePump();
      mountRemote(seatStreamName(claimedSeat === 1 ? 2 : 1));
      break;
    case 'close':
      log('SRT closed', 'err');
      setStatus('closed');
      callStatus('connection lost');
      stopFramePump();
      break;
    case 'wtClosed':
      if (claimedSeat === 0) {
        // Publish claim rejected before the handshake — seat is taken.
        handleClaimFailure(attemptGen);
      } else {
        if (msg.error) log(`WT closed: ${msg.error}`, 'err');
        else log('WT closed', 'info');
        setStatus('disconnected');
        callStatus('disconnected');
        stopFramePump();
      }
      break;
    case 'stats':
      store.srtStats.value = msg.stats;
      if (msg.encode) updateEncodeStats(msg.stats, msg.encode);
      break;
  }
}

// ─── Remote playback ──────────────────────────────────────────────

function mountRemote(stream: string): void {
  pbHandle?.destroy();
  const h = mountPlayer(remoteCanvas, {
    stream,
    latencyMs: CALL_LATENCY_MS,
    muted: false,
  });
  pbHandle = h;
  pbStream = stream;
  h.addEventListener('statechange', (ev) => {
    const s = (ev as CustomEvent<PlayerState>).detail;
    if (s === 'reconnecting') {
      callStatus(`seat ${claimedSeat} — waiting for peer…`);
    } else if (s === 'connected') {
      callStatus(`seat ${claimedSeat} — connected`);
    }
  });
  h.addEventListener('resize', (ev) => {
    const { width, height } = (ev as CustomEvent<PlayerResizeDetail>).detail;
    log(`[remote] first frame ${width}x${height}`, 'ok');
    callStatus(`seat ${claimedSeat} — connected`);
  });
  h.addEventListener('playing', () => log('[remote] playing', 'info'));
  h.addEventListener('error', (ev) => {
    const { message } = (ev as CustomEvent<PlayerErrorDetail>).detail;
    log(`[remote] ${message}`, 'err');
  });
  h.connect().catch(() => { /* reconnect loop handles it */ });
}

speakerBtn.addEventListener('click', () => {
  if (!pbHandle) return;
  const muted = !pbHandle.muted;
  pbHandle.setMuted(muted);
  speakerBtn.textContent = muted ? 'audio off' : 'audio on';
  speakerBtn.classList.toggle('muted', muted);
});

// ─── Mic / camera toggles ─────────────────────────────────────────

function applyMicMute() {
  const tracks = micStream?.getAudioTracks() ?? [];
  for (const t of tracks) t.enabled = !micMuted;
  micBtn.textContent = micMuted ? 'mic off' : 'mic on';
  micBtn.classList.toggle('muted', micMuted);
}

micBtn.addEventListener('click', () => {
  micMuted = !micMuted;
  applyMicMute();
  log(micMuted ? 'mic muted' : 'mic unmuted', 'info');
});

camBtn.addEventListener('click', () => {
  camOff = !camOff;
  const tracks = captureStream?.getVideoTracks() ?? [];
  for (const t of tracks) t.enabled = !camOff;
  camBtn.textContent = camOff ? 'cam off' : 'cam on';
  camBtn.classList.toggle('muted', camOff);
  log(camOff ? 'camera off' : 'camera on', 'info');
});

// ─── View switching ───────────────────────────────────────────────

const prejoinCardPreviewSlot = previewEl.parentElement as HTMLElement;

function showCallView(): void {
  prejoinEl.style.display = 'none';
  callEl.classList.add('active');
  // Move the (playing) preview into the PiP slot — no re-acquisition.
  pipEl.appendChild(previewEl);
  placePipDefault();
}

function showPrejoinView(): void {
  callEl.classList.remove('active');
  prejoinEl.style.display = '';
  prejoinCardPreviewSlot.appendChild(previewEl);
}

// ─── PiP drag ─────────────────────────────────────────────────────

function placePipDefault(): void {
  const cw = callEl.clientWidth || window.innerWidth;
  pipEl.style.left = `${cw - pipEl.offsetWidth - 16}px`;
  pipEl.style.top = `${16}px`;
}

{
  let dragging = false;
  let offX = 0;
  let offY = 0;
  pipEl.addEventListener('mousedown', (e) => {
    e.preventDefault();
    dragging = true;
    offX = e.clientX - pipEl.offsetLeft;
    offY = e.clientY - pipEl.offsetTop;
  });
  window.addEventListener('mousemove', (e) => {
    if (!dragging) return;
    const cw = callEl.clientWidth;
    const chh = callEl.clientHeight;
    const x = Math.min(cw - pipEl.offsetWidth, Math.max(0, e.clientX - offX));
    const y = Math.min(chh - pipEl.offsetHeight, Math.max(0, e.clientY - offY));
    pipEl.style.left = `${x}px`;
    pipEl.style.top = `${y}px`;
  });
  window.addEventListener('mouseup', () => { dragging = false; });
}

// ─── Join / hang up ───────────────────────────────────────────────

joinBtn.addEventListener('click', async () => {
  if (publishing) return;
  prejoinError.textContent = '';

  if (!capturing) {
    try {
      await startCapture();
    } catch (e) {
      prejoinError.textContent = `Camera failed: ${e}`;
      return;
    }
  }

  publishing = true;
  joinBtn.disabled = true;
  joinBtn.textContent = 'Joining…';
  room = roomNameInput.value || 'default';
  showCallView();
  setStatus('joining…');
  callStatus('joining…');

  // Audio graph + mic (Join click = user gesture → AudioContext runs).
  await setupAudioGraph();
  await connectAudioSource();
  if (audioCtx?.state === 'suspended') await audioCtx.resume();

  claimedSeat = 0;
  const attemptGen = ++claimGen;
  await trySeat(1, attemptGen);
});

hangupBtn.addEventListener('click', () => stopAll());

function stopAll() {
  publishing = false;
  stopFramePump();
  claimGen++;
  claimedSeat = 0;
  tryingSeat = 0;
  if (seatRetryTimer) {
    clearTimeout(seatRetryTimer);
    seatRetryTimer = null;
  }
  if (worker) {
    worker.postMessage({ cmd: 'stop' } as PublishCmd);
    worker.terminate();
    worker = null;
  }
  pbHandle?.destroy();
  pbHandle = null;
  pbStream = '';
  disconnectAudioSource();
  if (audioCtx) {
    try { audioCtx.close(); } catch {}
    audioCtx = null;
  }
  workletNode = null;
  credits = 0;
  // Reset device toggles for the next join.
  micMuted = false;
  micBtn.textContent = 'mic on';
  micBtn.classList.remove('muted');
  camOff = false;
  const vTracks = captureStream?.getVideoTracks() ?? [];
  for (const t of vTracks) t.enabled = true;
  camBtn.textContent = 'cam on';
  camBtn.classList.remove('muted');
  speakerBtn.textContent = 'audio on';
  speakerBtn.classList.remove('muted');
  joinBtn.disabled = false;
  joinBtn.textContent = 'Join Call';
  showPrejoinView();
  setStatus('hung up');
  callStatus('');
  pubStatsText.innerHTML = '';
}

// ─── Stats line (from stream.tsx) ─────────────────────────────────

function updateEncodeStats(srt: StatsMsg, enc: EncodeStats) {
  const txMbps = (srt.bandwidthBps / 1e6).toFixed(1);
  const txMB = (srt.txBytes / 1e6).toFixed(1);
  pubStatsText.innerHTML =
    `<span class="${enc.fps >= CALL_FPS - 5 ? 'ok' : 'err'}">${enc.fps} fps</span>` +
    ` | encode: ${enc.encodeMs.toFixed(1)}ms` +
    ` | queue: ${enc.queueDepth}` +
    ` | <span class="info">\u2191${txMbps} Mbps</span>` +
    ` | sent: ${txMB} MB` +
    ` | RTT: ${srt.rttMs.toFixed(0)}ms` +
    ` | loss: ${srt.txLoss}`;
}

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

document.getElementById('debug-toggle')?.addEventListener('click', () => {
  setPanelVisible(!store.panelVisible.value);
});

document.addEventListener('visibilitychange', () => {
  if (publishing) schedulePump();
});

// ─── Init ─────────────────────────────────────────────────────────

// Persistent room key: load from localStorage or generate a random 8-char one.
const ROOM_KEY_KEY = 'websrt-call-room';
const savedRoom = localStorage.getItem(ROOM_KEY_KEY);
roomNameInput.value = savedRoom ?? Array.from(
  { length: 8 },
  () => Math.floor(Math.random() * 36).toString(36),
).join('');
localStorage.setItem(ROOM_KEY_KEY, roomNameInput.value);
roomNameInput.addEventListener('change', () => {
  localStorage.setItem(ROOM_KEY_KEY, roomNameInput.value);
});

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

// Auto-start camera preview (call lobby pattern — permission prompt is
// expected on a call page).
startCapture().catch((e) => {
  log(`Camera unavailable: ${e}`, 'err');
  prejoinError.textContent = `Camera unavailable: ${e}`;
});
