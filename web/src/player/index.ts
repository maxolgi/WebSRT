// Framework-agnostic player SDK. Wraps createViewer() in an EventTarget-based
// handle that exposes HTMLMediaElement-like readyState/state plus media events
// (loadstart, canplay, playing, waiting, error, …). Intentionally free of any
// UI-framework or diagnostics imports — hosts wire those separately.

import { createViewer } from '../shared/viewer';
import type { ViewerHandle, ConnectionState } from '../shared/viewer';
import type { CanvasRenderer } from '../render';
import type { VideoPipeline, OpusAudioPipeline, AacAudioPipeline } from '../decode';
import type { StatsMsg, DemuxStatsMsg } from '../worker';

export interface PlayerOptions {
  host?: string;
  port?: string | number;
  stream?: string;
  token?: string;
  /** DER SHA-256 hex (self-signed pin) | null (PKI/mkcert) | omit = CERT_HASH. */
  certHash?: string | null;
  /** TSBPD latency in ms. Default 120. */
  latencyMs?: number;
  /** PTS-paced canvas presentation. Default true. */
  renderPacing?: boolean;
  /** DTS-paced decode submission. Default false. */
  decodePacing?: boolean;
  /** Initial mute state. Default true (browser autoplay policy). */
  muted?: boolean;
  /** Decode in the Web Worker; emit decodedframe/decodedaudio events instead of
   *  owning canvas/audio sinks. Default false. */
  decodeInWorker?: boolean;
  /** Auto-reconnect on disconnect. Default true. Set false for custom reconnection. */
  autoReconnect?: boolean;
  /** Override worker URL for non-Vite embedding. See ViewerConfig.workerUrl. */
  workerUrl?: string;
  /** Full WebTransport endpoint URL. Overrides host/port/stream/token. */
  url?: string;
}

export type PlayerState = 'idle' | 'connecting' | 'connected' | 'reconnecting' | 'error';

export interface PlayerStatsDetail {
  stats: StatsMsg;
  demux: DemuxStatsMsg | null;
}

export interface PlayerErrorDetail {
  message: string;
}

export interface PlayerResizeDetail {
  width: number;
  height: number;
}

export interface PlayerHandle extends EventTarget {
  // Events (via addEventListener):
  //   'decodedframe'  → CustomEvent<VideoFrame>   (decodeInWorker mode only; host owns lifecycle)
  //   'decodedaudio'  → CustomEvent<AudioData>    (decodeInWorker mode only; host owns lifecycle)
  /** Connect; resolves on first decoded frame, rejects on a terminal error
   *  (or if disconnect()/destroy() interrupts the initial connect). */
  connect(): Promise<void>;
  disconnect(): void;
  destroy(): void;
  readonly readyState: number;
  readonly state: PlayerState;
  readonly muted: boolean;
  volume: number;
  readonly videoWidth: number;
  readonly videoHeight: number;
  setMuted(muted: boolean): void;
  setLatencyMs(ms: number): void;
  readonly latencyMs: number;
  setRenderPacing(enabled: boolean): void;
  setDecodePacing(enabled: boolean): void;
  getRenderer(): CanvasRenderer | null;
  getVideo(): VideoPipeline | null;
  getAudio(): OpusAudioPipeline | AacAudioPipeline | null;
  getWorker(): Worker | null;
}

export function mountPlayer(canvas: HTMLCanvasElement | null, opts: PlayerOptions = {}): PlayerHandle {
  return new Player(canvas, opts);
}

class Player extends EventTarget implements PlayerHandle {
  private viewer: ViewerHandle;

  private _state: PlayerState = 'idle';
  private _readyState = 0;
  private _muted: boolean;
  private _volume = 1;
  private _videoWidth = 0;
  private _videoHeight = 0;

  private renderPacingPref: boolean;
  private decodePacingPref: boolean;

  private everConnected = false;
  private manualDisconnect = false;
  private destroyed = false;

  private connectPromise: Promise<void> | null = null;
  private resolveConnect: (() => void) | null = null;
  private rejectConnect: ((e: Error) => void) | null = null;

  constructor(canvas: HTMLCanvasElement | null, opts: PlayerOptions) {
    super();
    this._muted = opts.muted ?? true;
    this.renderPacingPref = opts.renderPacing ?? true;
    this.decodePacingPref = opts.decodePacing ?? false;

    this.viewer = createViewer({
      canvas: canvas ?? undefined,
      host: opts.host,
      port: opts.port,
      stream: opts.stream,
      token: opts.token,
      certHash: opts.certHash,
      latencyMs: opts.latencyMs ?? 120,
      decodeInWorker: opts.decodeInWorker ?? false,
      autoReconnect: opts.autoReconnect ?? true,
      workerUrl: opts.workerUrl,
      url: opts.url,
      ui: {
        log: (msg, cls) => this.onLog(msg, cls),
        setStatus: () => {},
        onStateChange: (s) => this.onViewerState(s),
        onFirstFrame: (w, h) => this.onFirstFrame(w, h),
        onVideoConfigured: () => {},
        onAudioReady: () => {},
        onStats: (stats, demux) => this.emit('stats', { stats, demux } satisfies PlayerStatsDetail),
        onDrift: (driftMs) => { if (driftMs !== null) this.emit('drift', driftMs); },
        onDecodedFrame: (frame) => this.emit('decodedframe', frame),
        onDecodedAudio: (data) => this.emit('decodedaudio', data),
        onAudioMeter: (data) => this.emit('audiometer', data),
      },
    });

    if (opts.muted !== undefined) this.viewer.setMuted(opts.muted);
  }

  private emit(type: string, detail?: unknown): void {
    this.dispatchEvent(detail !== undefined ? new CustomEvent(type, { detail }) : new CustomEvent(type));
  }

  private setState(s: PlayerState): void {
    if (this._state === s) return;
    this._state = s;
    this.emit('statechange', s);
  }

  private onLog(msg: string, cls?: string): void {
    // Reconnect is scheduled with a delay — surface it immediately so the
    // host can show a buffering indicator during the wait.
    if (msg.startsWith('reconnecting in')) {
      this.setState('reconnecting');
      this.emit('waiting');
      return;
    }
    if (cls === 'err') {
      // Missing cert-hash aborts doConnect() before any worker is created:
      // terminal — reject the pending connect promise, if any.
      if (msg.startsWith('No cert-hash.js')) {
        this.emit('error', { message: msg } satisfies PlayerErrorDetail);
        this.setState('error');
        this.failConnect(new Error(msg));
      } else {
        // Recoverable (worker / WT / SRT) — the viewer will retry.
        this.emit('error', { message: msg } satisfies PlayerErrorDetail);
      }
    }
  }

  private onViewerState(s: ConnectionState): void {
    if (s === 'connecting') {
      if (this.everConnected && !this.manualDisconnect) {
        this.setState('reconnecting');
        this.emit('waiting');
      } else {
        this.setState('connecting');
        this.emit('loadstart');
        this.emit('connecting');
      }
    } else if (s === 'connected') {
      this.everConnected = true;
      if (this._readyState < 1) this._readyState = 1;
      this.setState('connected');
      this.emit('open');
    } else {
      // idle (viewer teardown)
      if (this.manualDisconnect) {
        this._readyState = 0;
        this.everConnected = false;
        this.setState('idle');
        this.emit('close');
      } else if (this.everConnected) {
        this.setState('reconnecting');
        this.emit('waiting');
      } else {
        this.setState('idle');
      }
    }
  }

  private onFirstFrame(w: number, h: number): void {
    const changed = w !== this._videoWidth || h !== this._videoHeight;
    this._videoWidth = w;
    this._videoHeight = h;
    this._readyState = 4;
    if (changed) this.emit('resize', { width: w, height: h } satisfies PlayerResizeDetail);
    // Re-apply pacing preferences to the freshly built pipelines (pacing knobs
    // attach to renderer / decoder instances, which are recreated on connect).
    this.viewer.getRenderer()?.setRenderPacing(this.renderPacingPref);
    this.viewer.getVideo()?.setDecodePacing(this.decodePacingPref);
    this.emit('canplay');
    this.emit('playing');
    if (this.resolveConnect) {
      this.resolveConnect();
      this.connectPromise = null;
      this.resolveConnect = null;
      this.rejectConnect = null;
    }
  }

  private failConnect(e: Error): void {
    if (!this.rejectConnect) return;
    this.rejectConnect(e);
    this.connectPromise = null;
    this.resolveConnect = null;
    this.rejectConnect = null;
  }

  connect(): Promise<void> {
    if (this.destroyed) return Promise.reject(new Error('player destroyed'));
    if (this.connectPromise) return this.connectPromise;
    this.manualDisconnect = false;
    this.connectPromise = new Promise<void>((resolve, reject) => {
      this.resolveConnect = resolve;
      this.rejectConnect = reject;
    });
    this.viewer.connect();
    return this.connectPromise;
  }

  disconnect(): void {
    if (this.destroyed) return;
    this.manualDisconnect = true;
    this.failConnect(new Error('disconnected'));
    this.viewer.disconnect();
  }

  destroy(): void {
    if (this.destroyed) return;
    this.destroyed = true;
    this.manualDisconnect = true;
    this.failConnect(new Error('destroyed'));
    this.viewer.disconnect();
  }

  get readyState(): number { return this._readyState; }
  get state(): PlayerState { return this._state; }
  get muted(): boolean { return this._muted; }
  get latencyMs(): number { return this.viewer.getLatencyMs(); }
  get volume(): number { return this._volume; }
  set volume(v: number) {
    // Stored only — the audio element is owned privately by the viewer and
    // not exposed for volume control. Reserved for a future viewer hook.
    this._volume = v;
  }
  get videoWidth(): number { return this._videoWidth; }
  get videoHeight(): number { return this._videoHeight; }

  setMuted(muted: boolean): void {
    this._muted = muted;
    this.viewer.setMuted(muted);
  }

  setLatencyMs(ms: number): void { this.viewer.setLatencyMs(ms); }

  setRenderPacing(enabled: boolean): void {
    // No-op in decodeInWorker mode (no renderer on main thread).
    this.renderPacingPref = enabled;
    this.viewer.getRenderer()?.setRenderPacing(enabled);
  }

  setDecodePacing(enabled: boolean): void {
    // No-op in decodeInWorker mode (decoder lives in worker).
    this.decodePacingPref = enabled;
    this.viewer.getVideo()?.setDecodePacing(enabled);
  }

  getRenderer(): CanvasRenderer | null { return this.viewer.getRenderer(); }
  getVideo(): VideoPipeline | null { return this.viewer.getVideo(); }
  getAudio(): OpusAudioPipeline | AacAudioPipeline | null { return this.viewer.getAudio(); }
  getWorker(): Worker | null { return this.viewer.getWorker(); }
}
