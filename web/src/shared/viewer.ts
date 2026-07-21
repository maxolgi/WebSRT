// Shared viewer lifecycle used by both main.ts (simple page) and advanced.tsx
// (page with the Preact debug panel). Both entrypoints become thin wrappers
// that inject their UI sinks (log/status/state/stats) via ViewerUi.
//
// All ~300 lines of duplicated state and logic that used to live at module
// scope in each entrypoint now live as closures inside createViewer().

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
}

export interface ViewerConfig {
  /** Canvas element to render into. */
  canvas: HTMLCanvasElement;
  /** Latency input element (millisecond slider/input). */
  latencyInput: HTMLInputElement;
  /** Mute button (text + disabled state managed by the viewer). */
  muteBtn: HTMLButtonElement;
  /** UI sinks for log/status/state/etc. */
  ui: ViewerUi;
  /** Base reconnect backoff (ms). Defaults to 2000. */
  baseReconnectDelayMs?: number;
  /** Max reconnect backoff (ms). Defaults to 30000. */
  maxReconnectDelayMs?: number;
}

export interface ViewerHandle {
  /** Initiate a connection. Teardown any existing connection first. */
  connect(): void;
  /** Disconnect and stop. Marks as manual so no reconnect fires. */
  disconnect(): void;
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
    ui,
    baseReconnectDelayMs = 2000,
    maxReconnectDelayMs = 30000,
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
  let connState: ConnectionState = 'idle';
  let driftTimer: ReturnType<typeof setInterval> | null = null;
  let latestDriftMs: number | null = null;

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
    muteBtn.disabled = true;
    muteBtn.textContent = 'muted';
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
      if (Math.abs(driftMs) > 40) {
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
      audioEl.muted = true;
      log('audio ready (muted — click to unmute)', 'info');
      muteBtn.disabled = false;
      muteBtn.textContent = 'muted';
    } else {
      muteBtn.disabled = false;
      muteBtn.textContent = 'muted';
    }
  }

  // Mute toggle. Identical in both entrypoints; lives here since the viewer
  // owns audioEl and manages muteBtn text/disabled state.
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
          ui.onCertMode?.(newHash ? 'self' : 'mkcert');
        }
      }
    } catch { /* ignore — will use cached value */ }
  }

  function formatLatency(ms: number): string {
    return ms >= 1000 ? `${(ms / 1000).toFixed(ms % 1000 === 0 ? 0 : 1)}s` : `${ms}ms`;
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

    renderer = new CanvasRenderer(canvas);
    let firstFrame = true;

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

    const pageHost = location.hostname || '127.0.0.1';
    const wtHost = pageHost === 'localhost' ? '127.0.0.1' : pageHost;
    const urlParams = new URLSearchParams(location.search);
    const wtPort = urlParams.get('port') || '4433';
    const authToken = urlParams.get('token');
    const streamName = urlParams.get('stream') || 'default';
    const qp = new URLSearchParams({ stream: streamName });
    if (authToken) qp.set('token', authToken);
    const wtUrl = `https://${wtHost}:${wtPort}/wt?${qp}`;

    const latencyMs = +latencyInput.value;
    log(`TSBPD latency: ${formatLatency(latencyMs)}`, 'info');

    const certHash = hashHex ? hexToBytes(hashHex) : null;
    const hashLabel = hashHex ? `self-signed, hash ${hashHex.slice(0, 8)}…` : 'mkcert/PKI';
    log(`connecting to ${wtUrl} (${hashLabel}) …`, 'info');

    if (!worker) {
      worker = new Worker(new URL('../worker.ts', import.meta.url), { type: 'module' });
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
    ui.onWorkerReady?.(worker);

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
        ui.onStats?.(msg.stats, msg.demux ?? null);
        break;
      case 'close':
        log('SRT closed', 'err');
        setStatus('closed');
        if (!manualDisconnect) scheduleReconnect();
        break;
    }
  }

  return {
    connect: doConnect,
    disconnect() {
      manualDisconnect = true;
      reconnectAttempts = 0;
      teardown();
    },
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
