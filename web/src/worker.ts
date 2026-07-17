import init, { SrtReceiver, type SrtAction, type SrtStats } from '../wasm/srt-wasm/srt_wasm.js';
import { Demuxer } from './demux';

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
}

export interface DemuxStatsMsg {
  pat: number;
  pmt: number;
  pes: number;
  ra: number;
  err: number;
  raw: number;
}

export type WorkerCmd =
  | { cmd: 'init'; url: string; certHash: Uint8Array | null; latencyMs: number }
  | { cmd: 'visibility'; visible: boolean }
  | { cmd: 'stop' }
  | { cmd: 'debug-rate'; ms: number };

export type WorkerMsg =
  | { type: 'log'; msg: string; cls?: string }
  | { type: 'handshakeComplete' }
  | { type: 'pmt'; videoPid: number; audioPid: number; audioStreamType: number }
  | { type: 'videoPes'; data: Uint8Array; pts: number | null; isKeyframe: boolean }
  | { type: 'audioPes'; data: Uint8Array; pts: number | null }
  | { type: 'wtReady' }
  | { type: 'wtClosed'; error?: string }
  | { type: 'stats'; stats: StatsMsg; demux?: DemuxStatsMsg }
  | { type: 'close' }
  | { type: 'batch'; msgs: WorkerMsg[] };

const ST_H264 = 0x1b;
const ST_HEVC = 0x24;
const ST_AAC = 0x0f;
const ST_OPUS_FFMPEG = 0x06;

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
let audioPid: number | null = null;
let audioStreamType: number | null = null;
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
    rx = SrtReceiver.newWithLatency(latencyMs);
    epoch = performance.now();
    videoPid = null;
    audioPid = null;
    audioStreamType = null;

    demux = await Demuxer.create({
      onPmt: (entries) => {
        let changed = false;
        for (const e of entries) {
          if ((e.streamType === ST_H264 || e.streamType === ST_HEVC) && videoPid !== e.pid) {
            videoPid = e.pid;
            changed = true;
          } else if ((e.streamType === ST_AAC || e.streamType === ST_OPUS_FFMPEG) && audioPid !== e.pid) {
            audioPid = e.pid;
            audioStreamType = e.streamType;
            changed = true;
          }
        }
        if (changed) {
          queue({
            type: 'pmt',
            videoPid: videoPid ?? -1,
            audioPid: audioPid ?? -1,
            audioStreamType: audioStreamType ?? -1,
          });
        }
      },
      onPes: (pid, pts, _dts, bytes, ra) => {
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
    const nowUs = (performance.now() - epoch) * 1000;
    processActions(rx.handle_datagram(value, nowUs));
    flushOutgoing();
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
    bandwidthBps: Number(s.bandwidthBps),
    rxData: Number(s.rxData),
    rxBytes: Number(s.rxBytes),
    rxLoss: Number(s.rxLoss),
    rxRetransmit: Number(s.rxRetransmit),
    rxDropped: Number(s.rxDropped),
    rxBelated: Number(s.rxBelated),
    rxBuffered: Number(s.rxBuffered),
    rxAck: Number(s.rxAck),
    rxNak: Number(s.rxNak),
  };
}

function getDemuxStats(): DemuxStatsMsg {
  const s = (globalThis as any).__demuxStats ?? { pat: 0, pmt: 0, pes: 0, ra: 0, err: 0, raw: 0 };
  return { pat: s.pat, pmt: s.pmt, pes: s.pes, ra: s.ra, err: s.err, raw: s.raw };
}
