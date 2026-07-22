import init, { SrtReceiver, type SrtAction, type SrtStats } from '../wasm/srt-wasm/srt_wasm.js';
import { Demuxer } from './demux';
import { looksLikeAv1 } from './shared/av1';
import type { DemuxStats } from './debug/types';
import { summarizePmt, ST_PRIVATE, type PmtEntry } from './shared/pmt';

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
}

export type DemuxStatsMsg = DemuxStats;

export type WorkerCmd =
  | { cmd: 'init'; url: string; certHash: Uint8Array | null; latencyMs: number }
  | { cmd: 'visibility'; visible: boolean }
  | { cmd: 'stop' }
  | { cmd: 'debug-rate'; ms: number };

export type WorkerMsg =
  | { type: 'log'; msg: string; cls?: string }
  | { type: 'handshakeComplete' }
  | { type: 'pmt'; videoPid: number; audioPid: number; audioStreamType: number; videoCodec: 'av1' | 'h264' | 'hevc' | null }
  | { type: 'videoPes'; data: Uint8Array; pts: number | null; isKeyframe: boolean }
  | { type: 'audioPes'; data: Uint8Array; pts: number | null }
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
let pollTimer: ReturnType<typeof setInterval> | null = null;
let statsTimer: ReturnType<typeof setInterval> | null = null;
let videoPid: number | null = null;
let videoCodecResolved: 'av1' | 'h264' | 'hevc' | null = null;
let audioPid: number | null = null;
let audioStreamType: number | null = null;
// 0x06 PIDs with no registration descriptor (ffmpeg/OBS AV1 + Opus) awaiting
// content-probe on their first PES before being pinned as video or audio.
const probePids: Set<number> = new Set();
let inited = false;
let outgoing: WorkerMsg[] = [];

self.onmessage = async (e: MessageEvent) => {
  const cmd = e.data as WorkerCmd;
  switch (cmd.cmd) {
    case 'init':
      await doInit(cmd.url, cmd.certHash, cmd.latencyMs);
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
      const rate = Math.max(100, cmd.ms);
      if (rx && inited) {
        statsTimer = setInterval(() => {
          if (!rx || !inited) return;
          const s = rx.getStats();
          if (!s) return;
          if (VERBOSE) console.debug('srt stats', serializeStats(s));
          queue({ type: 'stats', stats: serializeStats(s), demux: getDemuxStats() });
          flushOutgoing();
        }, rate);
      }
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
  }
  (self as unknown as Worker).postMessage(
    { type: 'batch', msgs: outgoing },
    transfer,
  );
  outgoing = [];
}

async function doInit(url: string, certHash: Uint8Array | null, latencyMs: number) {
  const myGen = ++gen;
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
      },
      onPes: (pid, pts, _dts, bytes, ra) => {
        if (probePids.has(pid)) {
          // Content-probe: distinguish AV1 video from Opus audio by sniffing
          // the first OBU header. Runs once per PID, then pins the decision.
          probePids.delete(pid);
          if (looksLikeAv1(bytes)) {
            videoPid = pid;
            videoCodecResolved = 'av1';
          } else {
            audioPid = pid;
            audioStreamType = ST_PRIVATE;
          }
          // PMT reaches main.ts (sets the codec hint) before the video/audio
          // Pes queued immediately after, since both land in the same batch.
          queue({
            type: 'pmt',
            videoPid: videoPid ?? -1,
            audioPid: audioPid ?? -1,
            audioStreamType: audioStreamType ?? -1,
            videoCodec: videoCodecResolved,
          });
        }
        if (pid === videoPid) {
          queue({ type: 'videoPes', data: bytes, pts, isKeyframe: ra });
        } else if (pid === audioPid) {
          queue({ type: 'audioPes', data: bytes, pts });
        }
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
    reader = wt.datagrams.readable.getReader();
    writer = wt.datagrams.writable.getWriter();
    wt.closed
      .then(() => { if (myGen === gen) { queue({ type: 'wtClosed' }); flushOutgoing(); } })
      .catch((e) => { if (myGen === gen) { queue({ type: 'wtClosed', error: String(e) }); flushOutgoing(); } });
    queue({ type: 'wtReady' });
    flushOutgoing();
    startReaderLoop(myGen);

    pollTimer = setInterval(() => {
      if (!rx || !inited) return;
      const nowUs = (performance.now() - epoch) * 1000;
      const actions = rx.poll(nowUs);
      processActions(actions);
      flushOutgoing();
    }, 10);

    statsTimer = setInterval(() => {
      if (!rx || !inited) return;
      const s = rx.getStats();
      if (!s) return;
      if (VERBOSE) console.debug('srt stats', serializeStats(s));
      queue({ type: 'stats', stats: serializeStats(s), demux: getDemuxStats() });
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

function doStop() {
  if (pollTimer) { clearInterval(pollTimer); pollTimer = null; }
  if (statsTimer) { clearInterval(statsTimer); statsTimer = null; }
  const w = wt;
  wt = null;
  reader = null;
  writer = null;
  rx = null;
  demux = null;
  inited = false;
  if (w) { try { w.close({}); } catch {} }
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

async function startReaderLoop(myGen: number) {
  const r = reader;
  if (!r) return;
  for (;;) {
    let value: Uint8Array | undefined;
    try {
      const res = await r.read();
      if (res.done) break;
      value = res.value;
    } catch (e) {
      if (myGen === gen) { queue({ type: 'log', msg: `wt read: ${e}`, cls: 'err' }); flushOutgoing(); }
      break;
    }
    if (myGen !== gen || !rx || !inited) break;
    if (VERBOSE) console.debug('wt datagram', value.byteLength, 'bytes');
    const nowUs = (performance.now() - epoch) * 1000;
    processActions(rx.handle_datagram(value, nowUs));
    flushOutgoing();
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
        case 1:
          demux?.feed(a.takeData());
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
          console.warn(`srt: unknown action kind ${a.kind}`);
          break;
      }
    } finally {
      a.free();
    }
  }
}

function serializeStats(s: SrtStats): StatsMsg {
  return {
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
  };
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
