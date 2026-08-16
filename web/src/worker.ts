import init, { SrtReceiver, type SrtAction, type SrtStats } from '../wasm/srt-wasm/srt_wasm.js';
import { Demuxer } from './demux';
import { looksLikeAv1 } from './shared/av1';
import type { DemuxStats, VideoStats, AudioStats, AudioMeterData } from './shared/types';
import { summarizePmt, ST_PRIVATE, type PmtEntry } from './shared/pmt';
import { VideoPipeline, OpusAudioPipeline, AacAudioPipeline } from './decode';

export interface PcmReleaseStats {
  pid: number;
  count: number;
  /** Mean |relUs − schedUs| over the stats window (µs). */
  meanErrUs: number;
  /** Max |relUs − schedUs| over the stats window (µs). */
  maxErrUs: number;
  /** Max inter-pcm release gap over the stats window (µs). */
  maxGapUs: number;
}

export interface StatsMsg {
  elapsedMs: number;
  rttMs: number;
  bandwidthBps: number;
  rxData: number;
  rxBytes: number;
  rxLoss: number;
  rxRetransmit: number;
  rxDropped: number;
  rxBelated: number;
  rxBuffered: number;
  rxAck: number;
  rxNak: number;
  txData: number;
  txBytes: number;
  txRetransmit: number;
  txLoss: number;
  txBuffered: number;
  pollMaxMs: number;
  wasmHandleAvgUs: number;
  wasmPollAvgUs: number;
  loopIterAvgMs: number;
  videoStats?: VideoStats;
  audioStats?: AudioStats;
  /** PCM release pacing summary per audio PID over the stats window. */
  pcmRelease?: PcmReleaseStats[];
}

export type DemuxStatsMsg = DemuxStats;

export type WorkerCmd =
  | { cmd: 'init'; url: string; certHash: Uint8Array | null; latencyMs: number; decodeInWorker?: boolean }
  | { cmd: 'visibility'; visible: boolean }
  | { cmd: 'stop' }
  | { cmd: 'debug-rate'; ms: number }
  | { cmd: 'meter-select'; pid: number; channel: number };

export type WorkerMsg =
  | { type: 'log'; msg: string; cls?: string }
  | { type: 'handshakeComplete' }
  | { type: 'pmt'; videoPid: number; audioPid: number; audioStreamType: number; videoCodec: 'av1' | 'h264' | 'hevc' | null }
  | { type: 'videoPes'; data: Uint8Array; pts: number | null; dts: number | null; isKeyframe: boolean; nalOffsets: Uint32Array; nalTypes: Uint8Array }
  | { type: 'audioPes'; data: Uint8Array; pts: number | null }
  | { type: 'pcm'; pid: number; channelCount: number; samples: Float32Array; pts: number | null; schedUs: number | null; relUs: number | null }
  | { type: 'meter'; meter: AudioMeterData }
  | { type: 'videoFrame'; frame: VideoFrame }
  | { type: 'audioData'; data: AudioData }
  | { type: 'wtReady' }
  | { type: 'wtClosed'; error?: string }
  | { type: 'stats'; stats: StatsMsg; demux?: DemuxStatsMsg }
  | { type: 'close' }
  | { type: 'batch'; msgs: WorkerMsg[] };

const VERBOSE = typeof localStorage !== 'undefined' && localStorage.getItem('websrt-debug') === '1';

let rx: SrtReceiver | null = null;
let demux: Demuxer | null = null;
let wt: WebTransport | null = null;
let reader: ReadableStreamDefaultReader<Uint8Array> | null = null;
let writer: WritableStreamDefaultWriter<Uint8Array> | null = null;
let gen = 0;
let epoch = 0;
let pollMaxMs = 0;
let wasmHandleTotalUs = 0;
let wasmHandleCount = 0;
let wasmPollTotalUs = 0;
let wasmPollCount = 0;
let loopIterTotalMs = 0;
let loopIterCount = 0;
let prevRxLoss = 0;
let prevRxDropped = 0;
let prevRxRetransmit = 0;
let hsRelUs: number | null = null;

// --- PCM release pacing telemetry -----------------------------------------
// schedUs comes from the WASM deliver action (TSBPD deadline, epoch-relative
// µs); relUs is the actual release time (performance.now()-based, same
// timebase). demux.feed() fires onPcm synchronously, so a module-level
// "current feed context" set immediately before each feed attaches per-batch
// timing to every pcm message it produces.
function nowRelUs(): number {
  return (performance.now() - epoch) * 1000;
}

let feedSchedUs: number | null = null;
let feedRelUs: number | null = null;

interface PcmWindow {
  count: number;
  sumErrUs: number;
  maxErrUs: number;
  lastRelUs: number;
  maxGapUs: number;
}
const pcmReleaseWindows = new Map<number, PcmWindow>();

function recordPcmRelease(pid: number, relUs: number, schedUs: number | null): void {
  const errUs = schedUs !== null && schedUs > 0 ? Math.abs(relUs - schedUs) : -1;
  const validErr = errUs >= 0 && errUs < 10_000_000; // guard stale-clock artifacts

  let w = pcmReleaseWindows.get(pid);
  if (!w) {
    w = { count: 0, sumErrUs: 0, maxErrUs: 0, lastRelUs: relUs, maxGapUs: 0 };
    pcmReleaseWindows.set(pid, w);
  }
  if (validErr) {
    w.count++;
    w.sumErrUs += errUs;
    if (errUs > w.maxErrUs) w.maxErrUs = errUs;
  }
  const gapUs = relUs - w.lastRelUs;
  if (gapUs > 0 && gapUs < 1_000_000 && gapUs > w.maxGapUs) w.maxGapUs = gapUs;
  w.lastRelUs = relUs;

  if (VERBOSE) pcmLogSample(pid, relUs, validErr ? errUs : -1);
}

function drainPcmRelease(): PcmReleaseStats[] | undefined {
  if (pcmReleaseWindows.size === 0) return undefined;
  const out: PcmReleaseStats[] = [];
  for (const [pid, w] of pcmReleaseWindows) {
    if (w.count > 0) {
      out.push({
        pid,
        count: w.count,
        meanErrUs: w.sumErrUs / w.count,
        maxErrUs: w.maxErrUs,
        maxGapUs: w.maxGapUs,
      });
    }
  }
  pcmReleaseWindows.clear();
  return out.length > 0 ? out : undefined;
}

// --- VERBOSE-only pacing fidelity logger (dev acceptance tooling) ----------
// Keeps a ~60 s ring of (errUs, gapUs) per PID, marks seconds that had
// retransmit arrivals (excluded from the gate per the acceptance criteria),
// and dumps percentiles every 10 s to the console.
interface PcmLogSample { sec: number; errUs: number; gapUs: number }
const PCM_LOG_SPAN_SEC = 60;
const PCM_LOG_DUMP_SEC = 10;
const pcmLogs = new Map<number, PcmLogSample[]>();
const pcmLogLastRel = new Map<number, number>();
const pcmRetxSecs = new Set<number>();
let pcmLogDumpedAtSec = 0;

function pcmLogSample(pid: number, relUs: number, errUs: number): void {
  if (hsRelUs !== null && relUs - hsRelUs < 1_000_000) return; // TSBPD fill second
  const sec = Math.floor(relUs / 1_000_000);
  const lastRel = pcmLogLastRel.get(pid);
  const gapUs = lastRel !== undefined ? relUs - lastRel : -1;
  pcmLogLastRel.set(pid, relUs);
  let ring = pcmLogs.get(pid);
  if (!ring) { ring = []; pcmLogs.set(pid, ring); }
  ring.push({
    sec,
    errUs,
    gapUs: gapUs > 0 && gapUs < 1_000_000 ? gapUs : -1,
  });
  while (ring.length > 0 && ring[0].sec < sec - PCM_LOG_SPAN_SEC) ring.shift();
}

function pct(sorted: number[], p: number): number {
  if (sorted.length === 0) return 0;
  const idx = Math.min(sorted.length - 1, Math.max(0, Math.ceil((p / 100) * sorted.length) - 1));
  return sorted[idx];
}

function maybeDumpPcmLog(): void {
  const nowSec = Math.floor(nowRelUs() / 1_000_000);
  if (nowSec - pcmLogDumpedAtSec < PCM_LOG_DUMP_SEC) return;
  pcmLogDumpedAtSec = nowSec;
  for (const [pid, ring] of pcmLogs) {
    const samples = ring.filter((s) => !pcmRetxSecs.has(s.sec));
    const errs = samples.filter((s) => s.errUs >= 0).map((s) => s.errUs).sort((a, b) => a - b);
    const gaps = samples.filter((s) => s.gapUs >= 0).map((s) => s.gapUs).sort((a, b) => a - b);
    const fmt = (arr: number[]) => arr.length === 0 ? 'n/a' :
      `p50=${(pct(arr, 50) / 1000).toFixed(2)}ms p95=${(pct(arr, 95) / 1000).toFixed(2)}ms p99=${(pct(arr, 99) / 1000).toFixed(2)}ms max=${(arr[arr.length - 1] / 1000).toFixed(2)}ms`;
    console.log(
      `[pcm-pacing] pid ${pid}: n=${samples.length}/${ring.length} (retx-excluded ${ring.length - samples.length})` +
      ` |err-sched| ${fmt(errs)} · gap ${fmt(gaps)}`,
    );
  }
}

let statsTimer: ReturnType<typeof setInterval> | null = null;
let meterTimer: ReturnType<typeof setInterval> | null = null;
let videoPid: number | null = null;
let videoCodecResolved: 'av1' | 'h264' | 'hevc' | null = null;
let audioPid: number | null = null;
let audioStreamType: number | null = null;
// 0x06 PIDs with no registration descriptor (ffmpeg/OBS AV1 + Opus) awaiting
// content-probe on their first PES before being pinned as video or audio.
const probePids: Set<number> = new Set();
let inited = false;
let outgoing: WorkerMsg[] = [];
let decodeInWorker = false;
let videoPipeline: VideoPipeline | null = null;
let audioPipeline: OpusAudioPipeline | AacAudioPipeline | null = null;

self.onmessage = async (e: MessageEvent) => {
  const cmd = e.data as WorkerCmd;
  switch (cmd.cmd) {
    case 'init':
      await doInit(cmd.url, cmd.certHash, cmd.latencyMs, cmd.decodeInWorker);
      break;
    case 'visibility':
      if (cmd.visible) {
        if (rx && inited) {
          // Tab returned to foreground — catch up on missed ticks
          for (let i = 0; i < 10; i++) {
            const nowUs = (performance.now() - epoch) * 1000;
            const actions = rx.poll(nowUs);
            processActions(actions);
          }
          flushOutgoing();
        }
      } else {
        queue({ type: 'log', msg: 'tab backgrounded — SRT ticks may be throttled', cls: 'info' });
        flushOutgoing();
      }
      break;
    case 'stop':
      gen++;
      doStop();
      break;
    case 'debug-rate': {
      if (statsTimer) { clearInterval(statsTimer); statsTimer = null; }
      if (meterTimer) { clearInterval(meterTimer); meterTimer = null; }
      const rate = Math.max(100, cmd.ms);
      if (rx && inited) {
        statsTimer = setInterval(() => {
          if (!rx || !inited) return;
          const s = rx.getStats();
          if (!s) return;
          emitLossEvents(s);
          if (VERBOSE) console.debug('srt stats', serializeStats(s));
          const statsMsg = serializeStats(s);
          if (videoPipeline) statsMsg.videoStats = videoPipeline.getStats();
          if (audioPipeline) statsMsg.audioStats = audioPipeline.getStats();
          queue({ type: 'stats', stats: statsMsg, demux: getDemuxStats() });
          flushOutgoing();
        }, rate);
        meterTimer = setInterval(() => {
          if (!demux || !inited) return;
          const meter = demux.meterSnapshot();
          if (meter) {
            queue({ type: 'meter', meter });
            flushOutgoing();
          }
        }, 50);
      }
      break;
    }
    case 'meter-select': {
      demux?.setMeterSelection(cmd.pid, cmd.channel);
      break;
    }
  }
  flushOutgoing();
};

function queue(msg: WorkerMsg) {
  outgoing.push(msg);
}

function flushOutgoing() {
  if (outgoing.length === 0) return;
  const transfer: ArrayBuffer[] = [];
  for (const m of outgoing) {
    if (
      (m.type === 'videoPes' || m.type === 'audioPes') &&
      m.data?.buffer instanceof ArrayBuffer
    ) {
      transfer.push(m.data.buffer);
    }
    if (m.type === 'pcm' && m.samples?.buffer instanceof ArrayBuffer) {
      transfer.push(m.samples.buffer);
    }
    if (m.type === 'videoPes') {
      if (m.nalOffsets?.buffer instanceof ArrayBuffer) transfer.push(m.nalOffsets.buffer);
      if (m.nalTypes?.buffer instanceof ArrayBuffer) transfer.push(m.nalTypes.buffer);
    }
    if (m.type === 'videoFrame' && m.frame) transfer.push(m.frame as unknown as ArrayBuffer);
    if (m.type === 'audioData' && m.data) transfer.push(m.data as unknown as ArrayBuffer);
  }
  (self as unknown as Worker).postMessage(
    { type: 'batch', msgs: outgoing },
    transfer,
  );
  outgoing = [];
}

async function doInit(url: string, certHash: Uint8Array | null, latencyMs: number, decodeInWorkerFlag?: boolean) {
  const myGen = ++gen;
  decodeInWorker = decodeInWorkerFlag ?? false;
  try {
    doStop();
    await init();
    if (myGen !== gen) return;
    epoch = performance.now();
    videoPid = null;
    videoCodecResolved = null;
    audioPid = null;
    audioStreamType = null;
    probePids.clear();

    demux = await Demuxer.create({
      onPmt: (entries) => {
        // Entries with PRIVATE stream type and no recognized format id are
        // pending content-probe — don't pass them to summarizePmt yet because
        // they'd be silently dropped.
        const summary = summarizePmt(entries as PmtEntry[]);
        videoPid = summary.videoPid >= 0 ? summary.videoPid : null;
        videoCodecResolved = summary.videoCodec;
        audioPid = summary.audioPid >= 0 ? summary.audioPid : null;
        audioStreamType = summary.audioStreamType >= 0 ? summary.audioStreamType : null;
        // Collect probe-pending PIDs (PRIVATE with no AV01/Opus descriptor).
        for (const e of entries) {
          if (e.streamType === ST_PRIVATE && !e.formatId) {
            probePids.add(e.pid);
          }
        }
        // Emit PMT once video or audio is resolved. Probe-pending-only PMTs wait
        // until the probe completes (first PES on the probe PID).
        if (videoPid !== null || audioPid !== null) {
          queue({
            type: 'pmt',
            videoPid: videoPid ?? -1,
            audioPid: audioPid ?? -1,
            audioStreamType: audioStreamType ?? -1,
            videoCodec: videoCodecResolved,
          });
        }
        if (decodeInWorker) {
          ensureVideoPipeline();
          ensureAudioPipeline();
        }
      },
      onPes: (pid, pts, dts, bytes, ra, nalOffsets, nalTypes) => {
        // 1. Content-probe descriptor-less 0x06 PIDs first (may resolve codec)
        if (probePids.has(pid)) {
          probePids.delete(pid);
          if (looksLikeAv1(bytes)) {
            videoPid = pid;
            videoCodecResolved = 'av1';
          } else {
            audioPid = pid;
            audioStreamType = ST_PRIVATE;
          }
          // Emit updated PMT so main thread knows the resolved codec
          if (videoPid !== null || audioPid !== null) {
            queue({
              type: 'pmt',
              videoPid: videoPid ?? -1,
              audioPid: audioPid ?? -1,
              audioStreamType: audioStreamType ?? -1,
              videoCodec: videoCodecResolved,
            });
          }
        }

        if (decodeInWorker) {
          // Construct pipeline if codec resolved but pipeline not yet built, then feed
          if (pid === videoPid) {
            ensureVideoPipeline();
            videoPipeline?.feed(bytes, pts, ra, dts, nalOffsets, nalTypes);
          } else if (pid === audioPid) {
            ensureAudioPipeline();
            audioPipeline?.feed(bytes, pts);
          }
        } else {
          // Current path: queue PES bytes to main thread
          if (pid === videoPid) {
            queue({ type: 'videoPes', data: bytes, pts, dts, isKeyframe: ra, nalOffsets, nalTypes });
          } else if (pid === audioPid) {
            queue({ type: 'audioPes', data: bytes, pts });
          }
        }
      },
      onPcm: (pid, pts, channelCount, samples) => {
        const schedUs = feedSchedUs;
        const relUs = feedRelUs ?? nowRelUs();
        queue({ type: 'pcm', pid, channelCount, samples, pts, schedUs, relUs });
        recordPcmRelease(pid, relUs, schedUs);
      },
      onError: (msg_) => queue({ type: 'log', msg: `demux err: ${msg_}`, cls: 'err' }),
    });

    inited = true;

    // WebTransport lives in the worker so the SRT control loop
    // (datagram -> handle_datagram -> ACK write) never touches the main thread.
    const opts: WebTransportOptions = {};
    if (certHash) {
      opts.serverCertificateHashes = [{ algorithm: 'sha-256', value: certHash as BufferSource }];
    }
    wt = new WebTransport(url, opts);
    await wt.ready;
    if (myGen !== gen) { try { wt.close({}); } catch {} return; }

    // Seed SRT's RTT from QUIC's smoothed RTT for accurate cold-start
    // retransmit timing (draft-sharabayko-srt-over-quic §4.5).
    let initialRttMs: number | undefined;
    try {
      const stats = await (wt as any).getStats();
      if (stats && typeof stats.smoothedRtt === 'number' && stats.smoothedRtt > 0) {
        initialRttMs = stats.smoothedRtt;
      }
    } catch { /* getStats not supported — proceed with default RTT */ }
    rx = initialRttMs !== undefined
      ? SrtReceiver.newWithLatencyAndRtt(latencyMs, initialRttMs)
      : SrtReceiver.newWithLatency(latencyMs);
    const dg = wt.datagrams as any;
    const datagrams = typeof dg === 'function' ? dg() : dg;
    const readableStream = typeof datagrams.createReadable === 'function'
      ? datagrams.createReadable()
      : datagrams.readable;
    const writableStream = typeof datagrams.createWritable === 'function'
      ? datagrams.createWritable()
      : datagrams.writable;
    reader = readableStream.getReader();
    writer = writableStream.getWriter();
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
      if (VERBOSE) console.debug('srt stats', serializeStats(s));
      const statsMsg = serializeStats(s);
      if (videoPipeline) statsMsg.videoStats = videoPipeline.getStats();
      if (audioPipeline) statsMsg.audioStats = audioPipeline.getStats();
      queue({ type: 'stats', stats: statsMsg, demux: getDemuxStats() });
      flushOutgoing();
    }, 1000);
    meterTimer = setInterval(() => {
      if (!demux || !inited) return;
      const meter = demux.meterSnapshot();
      if (meter) {
        queue({ type: 'meter', meter });
        flushOutgoing();
      }
    }, 50);
  } catch (e) {
    if (myGen === gen) {
      doStop();
      queue({ type: 'log', msg: `worker init failed: ${e}`, cls: 'err' });
      queue({ type: 'wtClosed', error: String(e) });
      flushOutgoing();
    }
  }
}

function doStop() {
  if (statsTimer) { clearInterval(statsTimer); statsTimer = null; }
  if (meterTimer) { clearInterval(meterTimer); meterTimer = null; }
  pollMaxMs = 0;
  wasmHandleTotalUs = 0;
  wasmHandleCount = 0;
  wasmPollTotalUs = 0;
  wasmPollCount = 0;
  loopIterTotalMs = 0;
  loopIterCount = 0;
  prevRxLoss = 0;
  prevRxDropped = 0;
  prevRxRetransmit = 0;
  hsRelUs = null;
  feedSchedUs = null;
  feedRelUs = null;
  pcmReleaseWindows.clear();
  pcmLogs.clear();
  pcmLogLastRel.clear();
  pcmRetxSecs.clear();
  pcmLogDumpedAtSec = 0;
  const w = wt;
  wt = null;
  reader = null;
  writer = null;
  rx = null;
  demux = null;
  inited = false;
  if (videoPipeline) { try { videoPipeline.reset(); } catch {} videoPipeline = null; }
  if (audioPipeline) { try { audioPipeline.reset(); } catch {} audioPipeline = null; }
  if (w) { try { w.close({}); } catch {} }
}

function ensureVideoPipeline(): void {
  if (videoPipeline) return;
  if (videoCodecResolved === null) return;
  videoPipeline = new VideoPipeline({
    onFrame: (frame) => { queue({ type: 'videoFrame', frame }); },
    onError: (e) => { queue({ type: 'log', msg: `video err: ${e}`, cls: 'err' }); flushOutgoing(); },
    onConfigured: () => {},
  });
  videoPipeline.setCodecHint(videoCodecResolved);
}

function ensureAudioPipeline(): void {
  if (audioPipeline) return;
  if (audioStreamType === null || audioStreamType < 0) return;
  const isOpus = audioStreamType === 0x06;
  const cb = {
    onError: (e: unknown) => { queue({ type: 'log', msg: `audio err: ${e}`, cls: 'err' }); flushOutgoing(); },
    onReady: () => {},
    onFrame: (data: AudioData) => { queue({ type: 'audioData', data }); },
  };
  audioPipeline = isOpus ? new OpusAudioPipeline(cb) : new AacAudioPipeline(cb);
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

async function runSrtLoop(myGen: number) {
  const r = reader;
  if (!r) return;
  let readPromise = r.read();
  let lastCycle = performance.now();

  const BATCH_SIZE = 16;

  mainLoop: for (;;) {
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

    if (winner.kind === 'dgram') {
      if (winner.res.done) break;
      const value = winner.res.value;
      if (!value) break;
      if (VERBOSE) console.debug('wt datagram', value.byteLength, 'bytes');
      let _t0 = performance.now();
      // Fresh clock per datagram: a single per-iteration timestamp goes stale
      // as the batch drains, so packets processed late in a batch see a clock
      // lagging by the batch's own processing time.
      processActions(rx.handle_datagram(value, nowRelUs()));
      wasmHandleTotalUs += (performance.now() - _t0) * 1000;
      wasmHandleCount++;
      readPromise = r.read();

      // Batch: drain up to BATCH_SIZE-1 more datagrams that are already
      // buffered in the WebTransport receive queue before yielding back to
      // poll+flush. Racing readPromise against an already-resolved sentinel
      // resolves in a single microtask when data is waiting — far cheaper
      // than a full event-loop turn. When the queue is empty the sentinel
      // wins and we fall through to poll.
      for (let i = 1; i < BATCH_SIZE; i++) {
        const next = await Promise.race([
          readPromise.then(
            (res) => ({ kind: 'dgram' as const, res }),
            (err: unknown) => ({ kind: 'read_error' as const, err }),
          ),
          Promise.resolve({ kind: 'idle' as const }),
        ]);
        if (next.kind === 'idle') break;
        if (next.kind === 'read_error') {
          if (myGen === gen) {
            queue({ type: 'log', msg: `wt read: ${next.err}`, cls: 'err' });
            flushOutgoing();
          }
          break mainLoop;
        }
        if (next.res.done || !next.res.value) break mainLoop;
        if (VERBOSE) console.debug('wt datagram', next.res.value.byteLength, 'bytes');
        _t0 = performance.now();
        processActions(rx.handle_datagram(next.res.value, nowRelUs()));
        wasmHandleTotalUs += (performance.now() - _t0) * 1000;
        wasmHandleCount++;
        readPromise = r.read();
      }
    } else if (winner.kind === 'read_error') {
      if (myGen === gen) {
        queue({ type: 'log', msg: `wt read: ${winner.err}`, cls: 'err' });
        flushOutgoing();
      }
      break;
    }

    const _pollT0 = performance.now();
    // Fresh clock for the poll too — the batch drain above may have taken time.
    processActions(rx.poll(nowRelUs()));
    wasmPollTotalUs += (performance.now() - _pollT0) * 1000;
    wasmPollCount++;
    flushOutgoing();

    const cycleMs = performance.now() - lastCycle;
    lastCycle = performance.now();
    if (cycleMs > pollMaxMs) pollMaxMs = cycleMs;
    loopIterTotalMs += cycleMs;
    loopIterCount++;
  }
}

function processActions(actions: SrtAction[]) {
  for (const a of actions) {
    if (VERBOSE) console.debug('srt action', a.kind, 'bytes', a.data.length);
    try {
      switch (a.kind) {
        case 0:
          writeDatagram(a.takeData());
          break;
        case 1: {
          // Stamp the release context the demuxer's synchronous onPcm callbacks
          // read: schedUs = TSBPD deadline for this deliver action, relUs = the
          // actual time we are feeding it downstream.
          feedSchedUs = typeof a.schedUs === 'number' && a.schedUs > 0 ? a.schedUs : null;
          feedRelUs = nowRelUs();
          demux?.feed(a.takeData());
          feedSchedUs = null;
          feedRelUs = null;
          break;
        }
        case 2:
          hsRelUs = nowRelUs();
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
          console.warn(`srt: unknown action kind ${a.kind}`);
          break;
      }
    } finally {
      a.free();
    }
  }
}

function emitLossEvents(s: SrtStats) {
  const newLoss = s.rxLoss - prevRxLoss;
  const newDropped = s.rxDropped - prevRxDropped;
  if (s.rxRetransmit > prevRxRetransmit) {
    // Tag the second that just elapsed as retransmit-active for the pacing
    // logger's exclusion rule.
    pcmRetxSecs.add(Math.floor(nowRelUs() / 1_000_000));
  }
  if (newLoss > 0) {
    queue({ type: 'log', msg: `SRT loss: ${newLoss} packets (total ${s.rxLoss})`, cls: 'err' });
  }
  if (newDropped > 0) {
    queue({ type: 'log', msg: `SRT dropped (too late): ${newDropped} packets (total ${s.rxDropped})`, cls: 'err' });
  }
  prevRxLoss = s.rxLoss;
  prevRxDropped = s.rxDropped;
  prevRxRetransmit = s.rxRetransmit;
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
    wasmHandleAvgUs: wasmHandleCount > 0 ? wasmHandleTotalUs / wasmHandleCount : 0,
    wasmPollAvgUs: wasmPollCount > 0 ? wasmPollTotalUs / wasmPollCount : 0,
    loopIterAvgMs: loopIterCount > 0 ? loopIterTotalMs / loopIterCount : 0,
    pcmRelease: drainPcmRelease(),
  };
  if (VERBOSE) maybeDumpPcmLog();
  pollMaxMs = 0;
  wasmHandleTotalUs = 0;
  wasmHandleCount = 0;
  wasmPollTotalUs = 0;
  wasmPollCount = 0;
  loopIterTotalMs = 0;
  loopIterCount = 0;
  return msg;
}

function getDemuxStats(): DemuxStatsMsg | undefined {
  if (!demux) return undefined;
  // The snapshot is a wasm-bindgen struct holding a WASM pointer — it cannot
  // be structured-cloned across the worker boundary, and it must be freed.
  // Each typed-array getter already `.slice()`s into a JS-owned buffer, so we
  // read every field into a plain POJO, then free the WASM struct.
  const snap = demux.debugSnapshot();
  try {
    return {
      programNum: snap.programNum,
      pmtPid: snap.pmtPid,
      pmtPids: snap.pmtPids,
      pmtStreamTypes: snap.pmtStreamTypes,
      pmtFormatIds: snap.pmtFormatIds,
      pids: snap.pids,
      pesCounts: snap.pesCounts,
      byteTotals: snap.byteTotals,
      bitratesMbps: snap.bitratesMbps,
      raCounts: snap.raCounts,
      lastPts: snap.lastPts,
      lastDts: snap.lastDts,
      ptsJumps: snap.ptsJumps,
      ccErrors: snap.ccErrors,
      teiCounts: snap.teiCounts,
      pusiCounts: snap.pusiCounts,
      scramblingCounts: snap.scramblingCounts,
      afControlCounts: snap.afControlCounts,
      pcrPids: snap.pcrPids,
      pcrIntervalsMs: snap.pcrIntervalsMs,
      pcrJitterMs: snap.pcrJitterMs,
      nalPids: snap.nalPids,
      nalStats: snap.nalStats,
      errorT: snap.errorT,
      errorMsg: snap.errorMsg,
      ringT: snap.ringT,
      ringPid: snap.ringPid,
      ringKind: snap.ringKind,
      ringPts: snap.ringPts,
      ringDts: snap.ringDts,
      ringSize: snap.ringSize,
      ringRa: snap.ringRa,
      ringTei: snap.ringTei,
      ringPusi: snap.ringPusi,
      ringNal: snap.ringNal,
      ringNalOffsets: snap.ringNalOffsets,
    };
  } finally {
    snap.free();
  }
}
