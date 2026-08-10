// Viewer lifecycle factory. Consumed by the framework-agnostic player SDK
// (web/src/player/index.ts → mountPlayer), which wraps it in a PlayerHandle
// (EventTarget) and translates the ViewerUi callbacks into standard media
// events. Host pages wire their own UI through the SDK; ViewerUi is the
// internal hook surface.

import { VideoPipeline, OpusAudioPipeline, AacAudioPipeline } from '../decode';
import { CanvasRenderer } from '../render';
import type { WorkerMsg, StatsMsg, DemuxStatsMsg } from '../worker';

export type ConnectionState = 'idle' | 'connecting' | 'connected';

export interface ViewerUi {
  /** Push a log line. `cls` is optional CSS class / severity. */
  log(msg: string, cls?: string): void;
  /** Set the one-line status text. */
  setStatus(s: string): void;
  /** Connection state transitioned. */
  onStateChange(s: ConnectionState): void;
  /** A video frame decoded for the first time. */
  onFirstFrame(width: number, height: number): void;
  /** VideoDecoder configured. */
  onVideoConfigured(info: { profile: number; level: number }): void;
  /** Audio pipeline became ready. */
  onAudioReady(): void;
  /** Periodic stats (worker → main). Optional — entrypoints that don't
   *  render stats in this path can omit. */
  onStats?(stats: StatsMsg, demux: DemuxStatsMsg | null): void;
  /** A/V drift sample (or null when either side has no PTS yet). */
  onDrift?(driftMs: number | null): void;
  /** Cert mode determined from cert-hash.js. Optional. */
  onCertMode?(mode: 'self' | 'mkcert'): void;
  /** Called after the `init` message is posted to a (fresh or reused) worker.
    *  Useful for posting follow-up commands like `debug-rate`. */
  onWorkerReady?(worker: Worker): void;
  /** Decoded video frame (decodeInWorker mode only). Host owns lifecycle — must call frame.close(). */
  onDecodedFrame?(frame: VideoFrame): void;
  /** Decoded audio data (decodeInWorker mode only). Host owns lifecycle — must call data.close(). */
  onDecodedAudio?(data: AudioData): void;
}

export interface ViewerConfig {
  /** Canvas element to render into. Required when decodeInWorker is false;
   *  unused (may be omitted) when decodeInWorker is true. */
  canvas?: HTMLCanvasElement;
  /** Latency input element (millisecond slider/input). Optional for embedders
   *  that drive latency via `setLatencyMs()` instead. */
  latencyInput?: HTMLInputElement;
  /** Mute button (text + disabled state managed by the viewer). Optional for
   *  embedders that drive mute via `setMuted()` instead. */
  muteBtn?: HTMLButtonElement;
  /** Initial TSBPD latency in ms. Used when `latencyInput` is absent; defaults
   *  to 120 when neither `latencyInput` nor `latencyMs` is supplied. */
  latencyMs?: number;
  /** Cert hash override. `string` = DER SHA-256 hex (self-signed, pinned via
   *  WebTransport `serverCertificateHashes`); `null` = PKI/mkcert (no pinning);
   *  omitted = fall back to `(window as any).CERT_HASH` (set by cert-hash.js). */
  certHash?: string | null;
  /** WebTransport host. Overrides `?host=`; defaults to the page host. */
  host?: string;
  /** WebTransport port. Overrides `?port=`; defaults to '4433'. */
  port?: string | number;
  /** Stream name. Overrides `?stream=` (and is overridden by `getStreamName`). */
  stream?: string;
  /** Auth token. Overrides `?token=`. */
  token?: string;
  /** UI sinks for log/status/state/etc. */
  ui: ViewerUi;
  /** Base reconnect backoff (ms). Defaults to 2000. */
  baseReconnectDelayMs?: number;
  /** Max reconnect backoff (ms). Defaults to 30000. */
  maxReconnectDelayMs?: number;
  /** Stream name resolver. When provided, overrides the URL `?stream=` param. */
  getStreamName?(): string;
  /** When true, the worker handles decode and transfers decoded VideoFrame/AudioData
   *  to main. The SDK does NOT create a CanvasRenderer, VideoPipeline, or <audio> element.
   *  The host receives frames via the ViewerUi.onDecodedFrame / onDecodedAudio callbacks. */
  decodeInWorker?: boolean;
  /** When true (default), auto-reconnect with exponential backoff on disconnect.
   *  Set to false for apps that manage their own reconnection lifecycle. */
  autoReconnect?: boolean;
  /** Override the worker script URL. Defaults to the vite-resolved
   *  '../worker.ts'. Set this when embedding outside Vite (point it at
   *  your bundled worker output, e.g. '/vendor/worker.js'). */
  workerUrl?: string;
  /** Full WebTransport endpoint URL (e.g. 'https://host:4433/wt'). When set,
   *  overrides host/port/stream/token construction. Stream name and token
   *  are appended as query params if not already present in the URL. */
  url?: string;
}

export interface ViewerHandle {
  /** Initiate a connection. Teardown any existing connection first. */
  connect(): void;
  /** Disconnect and stop. Marks as manual so no reconnect fires. */
  disconnect(): void;
  /** Set TSBPD latency (ms). Reconnects if active and the value changed,
   *  mirroring the `latencyInput` change handler. Works without any DOM. */
  setLatencyMs(ms: number): void;
  /** Current TSBPD latency in milliseconds. */
  getLatencyMs(): number;
  /** Mute or unmute audio. Works without any DOM element. */
  setMuted(muted: boolean): void;
  /** Tab visibility changed (drives worker visibility message). */
  onVisibilityChange(visible: boolean): void;
  /** Get the current video pipeline (or null). */
  getVideo(): VideoPipeline | null;
  /** Get the current audio pipeline (or null). */
  getAudio(): OpusAudioPipeline | AacAudioPipeline | null;
  /** Get the current renderer (or null). */
  getRenderer(): CanvasRenderer | null;
  /** Get the current worker (or null). */
  getWorker(): Worker | null;
  /** True if connected or connecting. */
  isActive(): boolean;
}

export function createViewer(config: ViewerConfig): ViewerHandle {
  const {
    canvas,
    latencyInput,
    muteBtn,
    latencyMs: initialLatencyMs,
    certHash,
    host,
    port,
    stream,
    token,
    ui,
    baseReconnectDelayMs = 2000,
    maxReconnectDelayMs = 30000,
    getStreamName,
    decodeInWorker = false,
    autoReconnect = true,
    workerUrl,
    url: configUrl,
  } = config;

  // --- closure state (was module-level `let` in the entrypoints) ---
  let worker: Worker | null = null;
  let video: VideoPipeline | null = null;
  let audio: OpusAudioPipeline | AacAudioPipeline | null = null;
  let renderer: CanvasRenderer | null = null;
  let audioEl: HTMLAudioElement | null = null;
  let audioReady = false;
  let reconnectTimer: number | null = null;
  let reconnectAttempts = 0;
  let manualDisconnect = false;
  let firstFrame = true;
  let connState: ConnectionState = 'idle';
  let driftTimer: ReturnType<typeof setInterval> | null = null;
  let latestDriftMs: number | null = null;
  let currentLatencyMs: number = initialLatencyMs ?? (latencyInput ? (+latencyInput.value || 120) : 120);
  let appliedLatencyMs = currentLatencyMs;
  let mutedState = true;

  function log(msg: string, cls = '') {
    ui.log(msg, cls);
  }

  function setStatus(s: string) {
    ui.setStatus(s);
  }

  function setConnState(s: ConnectionState) {
    connState = s;
    ui.onStateChange(s);
  }

  const audioCb = {
    onError: (e: unknown) => log(`audio err: ${e}`, 'err'),
    onReady: () => {
      audioReady = true;
      wireAudio();
      ui.onAudioReady();
    },
  };

  function scheduleReconnect() {
    if (reconnectTimer !== null) return;
    const delay = Math.min(baseReconnectDelayMs * 2 ** reconnectAttempts, maxReconnectDelayMs);
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
    if (muteBtn) { muteBtn.disabled = true; muteBtn.textContent = 'muted'; }
  }

  function startDriftMonitor() {
    if (driftTimer !== null) clearInterval(driftTimer);
    driftTimer = setInterval(() => {
      const videoPts = renderer?.currentPtsUs() ?? null;
      const audioPts = audio?.audioPlayheadUs() ?? null;
      if (videoPts === null || audioPts === null) {
        latestDriftMs = null;
        ui.onDrift?.(null);
        return;
      }
      const driftMs = (videoPts - audioPts) / 1000;
      latestDriftMs = driftMs;
      ui.onDrift?.(driftMs);
    }, 2000);
  }

  function stopDriftMonitor() {
    if (driftTimer !== null) {
      clearInterval(driftTimer);
      driftTimer = null;
    }
    latestDriftMs = null;
    ui.onDrift?.(null);
  }

  function wireAudio() {
    if (!audio || !audioReady) return;
    const track = audio.track;
    if (track) {
      if (!audioEl) {
        audioEl = document.createElement('audio');
        document.body.appendChild(audioEl);
      }
      audioEl.srcObject = new MediaStream([track]);
      audioEl.muted = mutedState;
      log(mutedState ? 'audio ready (muted — click to unmute)' : 'audio ready', 'info');
      if (muteBtn) { muteBtn.disabled = false; muteBtn.textContent = mutedState ? 'muted' : 'mute'; }
    } else {
      if (muteBtn) { muteBtn.disabled = false; muteBtn.textContent = mutedState ? 'muted' : 'mute'; }
    }
  }

  // Mute toggle. Identical in both entrypoints; lives here since the viewer
  // owns audioEl and manages muteBtn text/disabled state. Only wired when an
  // embedder supplies a muteBtn; otherwise mute is driven via setMuted().
  if (muteBtn) {
    muteBtn.addEventListener('click', () => {
      if (!audioEl) return;
      setMuted(!audioEl.muted);
    });
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

  function formatLatency(ms: number): string {
    return ms >= 1000 ? `${(ms / 1000).toFixed(ms % 1000 === 0 ? 0 : 1)}s` : `${ms}ms`;
  }

  function doConnect() {
    teardown();
    manualDisconnect = false;
    setConnState('connecting');

    if (!decodeInWorker && !canvas) {
      log('canvas element required when decodeInWorker is false', 'err');
      return;
    }

    const hashHex = certHash !== undefined
      ? certHash
      : (window as any).CERT_HASH as string | null | undefined;
    if (hashHex === undefined) {
      log('No cert-hash.js — is the gateway running?', 'err');
      return;
    }
    ui.onCertMode?.(hashHex ? 'self' : 'mkcert');

    firstFrame = true;
    if (!decodeInWorker) {
      // The guard above guarantees canvas is present whenever !decodeInWorker;
      // TS cannot carry that relational narrowing here, hence the assertion.
      renderer = new CanvasRenderer(canvas!);
      video = new VideoPipeline({
        onFrame: (frame) => {
          renderer?.draw(frame);
          if (firstFrame) {
            firstFrame = false;
            ui.onFirstFrame(frame.displayWidth, frame.displayHeight);
          }
        },
        onError: (e) => log(`video err: ${e}`, 'err'),
        onConfigured: (info) => ui.onVideoConfigured(info),
      });
    } else {
      // Frame pump mode: no renderer, no main-side pipeline.
      // The worker transfers decoded frames; we emit them via ui callbacks.
    }

    const authToken = token ?? new URLSearchParams(location.search).get('token') ?? undefined;
    const streamName = getStreamName?.() ?? stream ?? new URLSearchParams(location.search).get('stream') ?? 'default';
    let wtUrl: string;
    if (configUrl) {
      const parsed = new URL(configUrl);
      if (!parsed.searchParams.has('stream')) parsed.searchParams.set('stream', streamName);
      if (authToken && !parsed.searchParams.has('token')) parsed.searchParams.set('token', authToken);
      wtUrl = parsed.toString();
    } else {
      const pageHost = location.hostname || '127.0.0.1';
      const urlParams = new URLSearchParams(location.search);
      const wtHost = host ?? urlParams.get('host') ?? (pageHost === 'localhost' ? '127.0.0.1' : pageHost);
      const wtPort = (port ?? urlParams.get('port') ?? (window as any).WT_PORT ?? '4433').toString();
      const qp = new URLSearchParams({ stream: streamName });
      if (authToken) qp.set('token', authToken);
      wtUrl = `https://${wtHost}:${wtPort}/wt?${qp}`;
    }

    const latencyMs = latencyInput ? +latencyInput.value : currentLatencyMs;
    appliedLatencyMs = latencyMs;
    log(`TSBPD latency: ${formatLatency(latencyMs)}`, 'info');

    const hashBytes = hashHex ? hexToBytes(hashHex) : null;
    const hashLabel = hashHex ? `self-signed, hash ${hashHex.slice(0, 8)}…` : 'mkcert/PKI';
    log(`connecting to ${wtUrl} (${hashLabel}) …`, 'info');

    if (!worker) {
      if (workerUrl) {
        worker = new Worker(workerUrl, { type: 'module' });
      } else {
        worker = new Worker(new URL('../worker.ts', import.meta.url), { type: 'module' });
      }
      worker.onmessage = (e: MessageEvent) => handleWorkerMsg(e.data as WorkerMsg);
      worker.onerror = (e) => {
        log(`worker error: ${e.message}`, 'err');
        if (!manualDisconnect && autoReconnect) scheduleReconnect();
      };
    }

    worker.postMessage(
      { cmd: 'init', url: wtUrl, certHash: hashBytes, latencyMs, decodeInWorker },
      hashBytes ? [hashBytes.buffer as ArrayBuffer] : [],
    );
    ui.onWorkerReady?.(worker);

    if (!decodeInWorker) startDriftMonitor();
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
        if (!decodeInWorker) {
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
        }
        break;
      case 'videoFrame':
        if (firstFrame) {
          firstFrame = false;
          ui.onFirstFrame(msg.frame.displayWidth, msg.frame.displayHeight);
        }
        ui.onDecodedFrame?.(msg.frame);
        break;
      case 'audioData':
        ui.onDecodedAudio?.(msg.data);
        break;
      case 'videoPes':
        video?.feed(msg.data, msg.pts, msg.isKeyframe, msg.dts, msg.nalOffsets, msg.nalTypes);
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
        if (!manualDisconnect && autoReconnect) scheduleReconnect();
        break;
      case 'stats':
        ui.onStats?.(msg.stats, msg.demux ?? null);
        break;
      case 'close':
        log('SRT closed', 'err');
        setStatus('closed');
        if (!manualDisconnect && autoReconnect) scheduleReconnect();
        break;
    }
  }

  function doDisconnect() {
    manualDisconnect = true;
    reconnectAttempts = 0;
    teardown();
  }

  function setLatencyMs(ms: number) {
    currentLatencyMs = ms;
    if (latencyInput) latencyInput.value = String(ms);
    if (connState !== 'idle' && ms !== appliedLatencyMs) {
      doDisconnect();
      setTimeout(() => doConnect(), 100);
    }
  }

  function setMuted(muted: boolean) {
    mutedState = muted;
    if (audioEl) {
      audioEl.muted = muted;
      if (!muted) audioEl.play().catch((e) => log(`audio play failed: ${e}`, 'err'));
    }
    if (muteBtn) muteBtn.textContent = muted ? 'muted' : 'mute';
  }

  return {
    connect: doConnect,
    disconnect: doDisconnect,
    setLatencyMs,
    getLatencyMs: () => currentLatencyMs,
    setMuted,
    onVisibilityChange(visible: boolean) {
      worker?.postMessage({ cmd: 'visibility', visible });
    },
    getVideo: () => video,
    getAudio: () => audio,
    getRenderer: () => renderer,
    getWorker: () => worker,
    isActive: () => connState !== 'idle',
  };
}
