import init, { SrtReceiver, type SrtAction, type SrtStats } from '../wasm/srt-wasm/srt_wasm.js';
import { Demuxer } from './demux';

export type WorkerCmd =
  | { cmd: 'init'; latencyMs: number }
  | { cmd: 'datagrams'; batch: Uint8Array[] }
  | { cmd: 'visibility'; visible: boolean }
  | { cmd: 'stop' };

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

export type WorkerMsg =
  | { type: 'log'; msg: string; cls?: string }
  | { type: 'handshakeComplete' }
  | { type: 'pmt'; videoPid: number; audioPid: number; audioStreamType: number }
  | { type: 'videoPes'; data: Uint8Array; pts: number | null; isKeyframe: boolean }
  | { type: 'audioPes'; data: Uint8Array; pts: number | null }
  | { type: 'send'; data: Uint8Array }
  | { type: 'stats'; stats: StatsMsg }
  | { type: 'close' }
  | { type: 'batch'; msgs: WorkerMsg[] };

const ST_H264 = 0x1b;
const ST_HEVC = 0x24;
const ST_AAC = 0x0f;
const ST_OPUS_FFMPEG = 0x06;

let rx: SrtReceiver | null = null;
let demux: Demuxer | null = null;
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
      await doInit(cmd.latencyMs);
      break;
    case 'datagrams':
      if (!rx || !inited) return;
      for (const data of cmd.batch) {
        const nowUs = (performance.now() - epoch) * 1000;
        const actions = rx.handle_datagram(data, nowUs);
        processActions(actions);
      }
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
      doStop();
      break;
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
      (m.type === 'videoPes' || m.type === 'audioPes' || m.type === 'send') &&
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

async function doInit(latencyMs: number) {
  try {
    doStop();
    await init();
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
      queue({ type: 'stats', stats: serializeStats(s) });
      flushOutgoing();
    }, 1000);
  } catch (e) {
    queue({ type: 'log', msg: `worker init failed: ${e}`, cls: 'err' });
    flushOutgoing();
  }
}

function doStop() {
  if (pollTimer) { clearInterval(pollTimer); pollTimer = null; }
  if (statsTimer) { clearInterval(statsTimer); statsTimer = null; }
  rx = null;
  demux = null;
  inited = false;
}

function processActions(actions: SrtAction[]) {
  for (const a of actions) {
    try {
      switch (a.kind) {
        case 0:
          queue({ type: 'send', data: a.takeData() });
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
