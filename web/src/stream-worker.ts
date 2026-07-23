import initSrt, { SrtReceiver, type SrtAction, type SrtStats } from '../wasm/srt-wasm/srt_wasm.js';
import initMux, { TsMuxer } from '../wasm/ts-muxer-wasm/ts_muxer_wasm.js';
import type { StatsMsg } from './worker';

export type { StatsMsg };

export interface EncodeStats {
  fps: number;
  encodeMs: number;
  queueDepth: number;
  chunksEncoded: number;
  kbEncoded: number;
}

export interface VideoConfig {
  codec: string;
  width: number;
  height: number;
  bitrate: number;
  framerate: number;
}

export interface AudioConfig {
  bitrate: number;
  channels: number;
}

export type PublishCmd =
  | { cmd: 'init'; url: string; certHash: Uint8Array | null; latencyMs: number; video: VideoConfig; audio: AudioConfig | null }
  | { cmd: 'frame'; frame: VideoFrame }
  | { cmd: 'audio-port'; port: MessagePort }
  | { cmd: 'stop' }
  | { cmd: 'visibility'; visible: boolean }
  | { cmd: 'debug-rate'; ms: number };

export type PublishMsg =
  | { type: 'log'; msg: string; cls?: string }
  | { type: 'credit' }
  | { type: 'wtReady' }
  | { type: 'handshakeComplete' }
  | { type: 'wtClosed'; error?: string }
  | { type: 'close' }
  | { type: 'stats'; stats: StatsMsg; encode?: EncodeStats }
  | { type: 'batch'; msgs: PublishMsg[] };

const VERBOSE = typeof localStorage !== 'undefined' && localStorage.getItem('websrt-debug') === '1';

let rx: SrtReceiver | null = null;
let muxer: TsMuxer | null = null;
let wt: WebTransport | null = null;
let reader: ReadableStreamDefaultReader<Uint8Array> | null = null;
let writer: WritableStreamDefaultWriter<Uint8Array> | null = null;
let gen = 0;
let epoch = 0;
let pollMaxMs = 0;
let prevTxLoss = 0;
let statsTimer: ReturnType<typeof setInterval> | null = null;

let videoEncoder: VideoEncoder | null = null;
let audioEncoder: AudioEncoder | null = null;
let audioPort: MessagePort | null = null;
let frameCount = 0;
let keyframeInterval = 60;

let chunksEncoded = 0;
let bytesEncoded = 0;
let encodeDurations: number[] = [];
let lastEncodeFinish = 0;
let encodeFps = 0;

let outgoing: PublishMsg[] = [];
let inited = false;
let srtWasmReady = false;
let muxWasmReady = false;

// ─── Message handler ──────────────────────────────────────────────

self.onmessage = async (e: MessageEvent) => {
  const cmd = e.data as PublishCmd;
  switch (cmd.cmd) {
    case 'init':
      await doInit(cmd.url, cmd.certHash, cmd.latencyMs, cmd.video, cmd.audio);
      break;
    case 'frame':
      handleFrame(cmd.frame);
      break;
    case 'audio-port':
      setupAudioPort(cmd.port);
      break;
    case 'visibility':
      if (cmd.visible && rx && inited) {
        for (let i = 0; i < 10; i++) {
          const nowUs = (performance.now() - epoch) * 1000;
          processActions(rx.poll(nowUs));
        }
        flushOutgoing();
      }
      break;
    case 'stop':
      gen++;
      doStop();
      break;
    case 'debug-rate': {
      if (statsTimer) { clearInterval(statsTimer); statsTimer = null; }
      const rate = Math.max(100, cmd.ms);
      if (inited) {
        statsTimer = setInterval(() => {
          if (!rx || !inited) return;
          const s = rx.getStats();
          if (!s) return;
          emitLossEvents(s);
          queue({ type: 'stats', stats: serializeStats(s), encode: getEncodeStats() });
          flushOutgoing();
        }, rate);
      }
      break;
    }
  }
  flushOutgoing();
};

// ─── Queue / flush ────────────────────────────────────────────────

function queue(msg: PublishMsg) {
  outgoing.push(msg);
}

function flushOutgoing() {
  if (outgoing.length === 0) return;
  (self as unknown as Worker).postMessage({ type: 'batch', msgs: outgoing });
  outgoing = [];
}

// ─── Init ─────────────────────────────────────────────────────────

async function doInit(url: string, certHash: Uint8Array | null, latencyMs: number, videoCfg: VideoConfig, audioCfg: AudioConfig | null) {
  const myGen = ++gen;
  try {
    doStop();

    if (!srtWasmReady) { await initSrt(); srtWasmReady = true; }
    if (!muxWasmReady) { await initMux(); muxWasmReady = true; }
    if (myGen !== gen) return;

    epoch = performance.now();
    frameCount = 0;
    chunksEncoded = 0;
    bytesEncoded = 0;
    encodeDurations = [];
    keyframeInterval = Math.max(1, Math.round(videoCfg.framerate * 2));

    // TsMuxer
    muxer = new TsMuxer();
    const isAv1 = videoCfg.codec.startsWith('av01');
    muxer.setVideoCodec(isAv1 ? 'av1' : 'h264');

    // VideoEncoder
    const vCfg: VideoEncoderConfig = {
      codec: videoCfg.codec,
      width: videoCfg.width,
      height: videoCfg.height,
      bitrate: videoCfg.bitrate * 1_000_000,
      framerate: videoCfg.framerate,
      latencyMode: 'realtime',
      hardwareAcceleration: 'prefer-hardware',
    };
    if (!isAv1) {
      (vCfg as unknown as Record<string, unknown>).avc = { format: 'annexb' };
    }

    let probe = await VideoEncoder.isConfigSupported(vCfg);
    if (!probe.supported) {
      (vCfg as unknown as Record<string, unknown>).hardwareAcceleration = 'prefer-software';
      probe = await VideoEncoder.isConfigSupported(vCfg);
    }
    if (!probe.supported) {
      throw new Error(`VideoEncoder does not support codec ${videoCfg.codec}`);
    }

    videoEncoder = new VideoEncoder({
      output: onVideoOutput,
      error: (e: unknown) => {
        queue({ type: 'log', msg: `VideoEncoder error: ${e}`, cls: 'err' });
        flushOutgoing();
      },
    });
    videoEncoder.configure(vCfg);
    queue({ type: 'log', msg: `VideoEncoder configured: ${videoCfg.codec} ${videoCfg.width}x${videoCfg.height} @ ${videoCfg.framerate}fps ${videoCfg.bitrate}Mbps`, cls: 'info' });

    // AudioEncoder
    if (audioCfg) {
      const aCfg: AudioEncoderConfig = {
        codec: 'opus',
        sampleRate: 48000,
        numberOfChannels: audioCfg.channels,
        bitrate: audioCfg.bitrate,
      };
      const aProbe = await AudioEncoder.isConfigSupported(aCfg);
      if (!aProbe.supported) {
        throw new Error('AudioEncoder does not support Opus');
      }
      audioEncoder = new AudioEncoder({
        output: onAudioOutput,
        error: (e: unknown) => {
          queue({ type: 'log', msg: `AudioEncoder error: ${e}`, cls: 'err' });
          flushOutgoing();
        },
      });
      audioEncoder.configure(aCfg);
      queue({ type: 'log', msg: `AudioEncoder configured: Opus ${audioCfg.channels}ch ${audioCfg.bitrate}bps`, cls: 'info' });
    }

    inited = true;

    // WebTransport
    const opts: WebTransportOptions = {};
    if (certHash) {
      opts.serverCertificateHashes = [{ algorithm: 'sha-256', value: certHash as BufferSource }];
    }
    wt = new WebTransport(url, opts);
    await wt.ready;
    if (myGen !== gen) { try { wt.close({}); } catch {} return; }

    let initialRttMs: number | undefined;
    try {
      const stats = await (wt as any).getStats();
      if (stats && typeof stats.smoothedRtt === 'number' && stats.smoothedRtt > 0) {
        initialRttMs = stats.smoothedRtt;
      }
    } catch { /* getStats not supported */ }

    rx = initialRttMs !== undefined
      ? SrtReceiver.newWithLatencyAndRtt(latencyMs, initialRttMs)
      : SrtReceiver.newWithLatency(latencyMs);
    reader = wt.datagrams.readable.getReader();
    writer = wt.datagrams.writable.getWriter();

    wt.closed
      .then(() => { if (myGen === gen) { queue({ type: 'wtClosed' }); flushOutgoing(); } })
      .catch((e) => { if (myGen === gen) { queue({ type: 'wtClosed', error: String(e) }); flushOutgoing(); } });

    queue({ type: 'wtReady' });
    flushOutgoing();
    runSrtLoop(myGen);

    statsTimer = setInterval(() => {
      if (!rx || !inited) return;
      const s = rx.getStats();
      if (!s) return;
      emitLossEvents(s);
      queue({ type: 'stats', stats: serializeStats(s), encode: getEncodeStats() });
      flushOutgoing();
    }, 1000);
  } catch (e) {
    if (myGen === gen) {
      doStop();
      queue({ type: 'log', msg: `worker init failed: ${e}`, cls: 'err' });
      queue({ type: 'wtClosed', error: String(e) });
      flushOutgoing();
    }
  }
}

// ─── Stop ─────────────────────────────────────────────────────────

function doStop() {
  if (statsTimer) { clearInterval(statsTimer); statsTimer = null; }
  pollMaxMs = 0;
  prevTxLoss = 0;

  if (videoEncoder) {
    try { videoEncoder.flush(); } catch {}
    try { videoEncoder.close(); } catch {}
    videoEncoder = null;
  }
  if (audioEncoder) {
    try { audioEncoder.flush(); } catch {}
    try { audioEncoder.close(); } catch {}
    audioEncoder = null;
  }
  if (audioPort) {
    try { audioPort.close(); } catch {}
    audioPort = null;
  }
  if (muxer) {
    try { muxer.free(); } catch {}
    muxer = null;
  }

  const w = wt;
  wt = null;
  reader = null;
  writer = null;
  rx = null;
  inited = false;

  if (w) { try { w.close({}); } catch {} }
}

// ─── Video frame handling ─────────────────────────────────────────

function handleFrame(frame: VideoFrame) {
  if (!videoEncoder || videoEncoder.state !== 'configured') {
    frame.close();
    queue({ type: 'credit' });
    return;
  }

  const enc = videoEncoder;
  if (enc.encodeQueueSize > 8) {
    frame.close();
    queue({ type: 'log', msg: `encode queue full (${enc.encodeQueueSize}), dropping frame`, cls: 'err' });
    queue({ type: 'credit' });
    flushOutgoing();
    return;
  }

  const forceKey = frameCount === 0 || frameCount % keyframeInterval === 0;
  frameCount++;

  const encodeStart = performance.now();
  try {
    enc.encode(frame, { keyFrame: forceKey });
  } catch (e) {
    queue({ type: 'log', msg: `encode() threw: ${e}`, cls: 'err' });
  }
  frame.close();

  queue({ type: 'credit' });
  flushOutgoing();

  const encodeMs = performance.now() - encodeStart;
  encodeDurations.push(encodeMs);
  if (encodeDurations.length > 60) encodeDurations.shift();
}

function onVideoOutput(chunk: EncodedVideoChunk, _metadata: EncodedVideoChunkMetadata | undefined) {
  if (!muxer) return;

  const data = new Uint8Array(chunk.byteLength);
  chunk.copyTo(data);

  muxer.push_video(data, chunk.timestamp, chunk.timestamp, chunk.type === 'key');

  chunksEncoded++;
  bytesEncoded += data.byteLength;

  const now = performance.now();
  if (lastEncodeFinish > 0) {
    const dt = now - lastEncodeFinish;
    if (dt > 0 && dt < 5000) {
      encodeFps = encodeFps * 0.8 + (1000 / dt) * 0.2;
    }
  }
  lastEncodeFinish = now;

  flushTsToSrt();
}

// ─── Audio handling ───────────────────────────────────────────────

function setupAudioPort(port: MessagePort) {
  if (audioPort) {
    try { audioPort.close(); } catch {}
  }
  audioPort = port;
  audioPort.onmessage = (e: MessageEvent) => {
    if (!audioEncoder || audioEncoder.state !== 'configured') return;
    const { data, channels, time } = e.data as { data: Float32Array; channels: number; time: number };

    try {
      const audioData = new AudioData({
        format: 'f32-planar',
        sampleRate: 48000,
        numberOfFrames: 960,
        numberOfChannels: channels,
        timestamp: Math.round(time * 1_000_000),
        data: data as BufferSource,
      });
      audioEncoder.encode(audioData);
      audioData.close();
    } catch (err) {
      queue({ type: 'log', msg: `audio encode error: ${err}`, cls: 'err' });
      flushOutgoing();
    }
  };
}

function onAudioOutput(chunk: EncodedAudioChunk) {
  if (!muxer) return;

  const data = new Uint8Array(chunk.byteLength);
  chunk.copyTo(data);

  muxer.push_audio(data, chunk.timestamp);
  flushTsToSrt();
}

// ─── TS → SRT flush ───────────────────────────────────────────────

function flushTsToSrt() {
  if (!muxer || !rx || !inited) return;

  const tsBytes = muxer.poll();
  if (tsBytes.length === 0) return;

  const nowUs = (performance.now() - epoch) * 1000;
  const actions = rx.sendMessage(tsBytes, nowUs);
  processActions(actions);
  flushOutgoing();
}

// ─── SRT loop (same structure as viewer worker) ───────────────────

async function runSrtLoop(myGen: number) {
  const r = reader;
  if (!r) return;
  let readPromise = r.read();
  let lastCycle = performance.now();

  for (;;) {
    if (myGen !== gen || !rx || !inited) break;

    const POLL_MS = 5;
    let timeoutId: ReturnType<typeof setTimeout> | undefined;
    const readWithLabel = readPromise.then(
      (res) => ({ kind: 'dgram' as const, res }),
      (err: unknown) => ({ kind: 'read_error' as const, err }),
    );
    const tickPromise = new Promise<{ kind: 'tick' }>((resolve) => {
      timeoutId = setTimeout(() => resolve({ kind: 'tick' }), POLL_MS);
    });

    const winner = await Promise.race([readWithLabel, tickPromise]);
    if (timeoutId !== undefined) clearTimeout(timeoutId);

    if (myGen !== gen || !rx || !inited) break;

    const nowUs = (performance.now() - epoch) * 1000;

    if (winner.kind === 'dgram') {
      if (winner.res.done) break;
      const value = winner.res.value;
      if (!value) break;
      processActions(rx.handle_datagram(value, nowUs));
      readPromise = r.read();
    } else if (winner.kind === 'read_error') {
      if (myGen === gen) {
        queue({ type: 'log', msg: `wt read: ${winner.err}`, cls: 'err' });
        flushOutgoing();
      }
      break;
    }

    processActions(rx.poll(nowUs));
    flushOutgoing();

    const cycleMs = performance.now() - lastCycle;
    lastCycle = performance.now();
    if (cycleMs > pollMaxMs) pollMaxMs = cycleMs;
  }
}

function processActions(actions: SrtAction[]) {
  for (const a of actions) {
    try {
      switch (a.kind) {
        case 0:
          writeDatagram(a.takeData());
          break;
        case 1:
          break;
        case 2:
          queue({ type: 'handshakeComplete' });
          break;
        case 3:
          break;
        case 4:
          queue({ type: 'close' });
          break;
        case 5:
          queue({ type: 'log', msg: `srt: ${a.text}`, cls: 'info' });
          break;
        default:
          break;
      }
    } finally {
      a.free();
    }
  }
}

function writeDatagram(bytes: Uint8Array) {
  const w = writer;
  if (!w) return;
  try {
    w.write(bytes).catch((e) => {
      queue({ type: 'log', msg: `wt write: ${e}`, cls: 'err' });
      flushOutgoing();
    });
  } catch (e) {
    queue({ type: 'log', msg: `wt write: ${e}`, cls: 'err' });
    flushOutgoing();
  }
}

// ─── Stats ────────────────────────────────────────────────────────

function emitLossEvents(s: SrtStats) {
  const newLoss = s.txLoss - prevTxLoss;
  if (newLoss > 0) {
    queue({ type: 'log', msg: `SRT tx loss: ${newLoss} packets (total ${s.txLoss})`, cls: 'err' });
  }
  prevTxLoss = s.txLoss;
}

function getEncodeStats(): EncodeStats {
  const avgEncodeMs = encodeDurations.length > 0
    ? encodeDurations.reduce((a, b) => a + b, 0) / encodeDurations.length
    : 0;
  return {
    fps: Math.round(encodeFps),
    encodeMs: avgEncodeMs,
    queueDepth: videoEncoder?.encodeQueueSize ?? 0,
    chunksEncoded,
    kbEncoded: Math.round(bytesEncoded / 1024),
  };
}

function serializeStats(s: SrtStats): StatsMsg {
  const msg: StatsMsg = {
    elapsedMs: s.elapsedMs,
    rttMs: s.rttMs,
    bandwidthBps: s.bandwidthBps,
    rxData: s.rxData,
    rxBytes: s.rxBytes,
    rxLoss: s.rxLoss,
    rxRetransmit: s.rxRetransmit,
    rxDropped: s.rxDropped,
    rxBelated: s.rxBelated,
    rxBuffered: s.rxBuffered,
    rxAck: s.rxAck,
    rxNak: s.rxNak,
    txData: s.txData,
    txBytes: s.txBytes,
    txRetransmit: s.txRetransmit,
    txLoss: s.txLoss,
    txBuffered: s.txBuffered,
    pollMaxMs: pollMaxMs,
  };
  pollMaxMs = 0;
  return msg;
}
