import init, { SrtReceiver, type SrtAction, type SrtStats } from '../wasm/srt-wasm/srt_wasm.js';
import { Demuxer } from './demux';

export type WorkerCmd =
  | { cmd: 'init'; latencyMs: number }
  | { cmd: 'datagram'; data: Uint8Array }
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
  | { type: 'close' };

const ST_H264 = 0x1b;
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

self.onmessage = async (e: MessageEvent) => {
  const cmd = e.data as WorkerCmd;
  switch (cmd.cmd) {
    case 'init':
      await doInit(cmd.latencyMs);
      break;
    case 'datagram':
      if (!rx || !inited) return;
      {
        const nowUs = (performance.now() - epoch) * 1000;
        const actions = rx.handle_datagram(cmd.data, nowUs);
        processActions(actions);
      }
      break;
    case 'stop':
      doStop();
      break;
  }
};

function post(msg: WorkerMsg) {
  (self as unknown as Worker).postMessage(msg);
}

async function doInit(latencyMs: number) {
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
        if (e.streamType === ST_H264 && videoPid !== e.pid) {
          videoPid = e.pid;
          changed = true;
        } else if ((e.streamType === ST_AAC || e.streamType === ST_OPUS_FFMPEG) && audioPid !== e.pid) {
          audioPid = e.pid;
          audioStreamType = e.streamType;
          changed = true;
        }
      }
      if (changed) {
        post({
          type: 'pmt',
          videoPid: videoPid ?? -1,
          audioPid: audioPid ?? -1,
          audioStreamType: audioStreamType ?? -1,
        });
      }
    },
    onPes: (pid, pts, _dts, bytes, ra) => {
      if (pid === videoPid) {
        post({ type: 'videoPes', data: bytes, pts, isKeyframe: ra });
      } else if (pid === audioPid) {
        post({ type: 'audioPes', data: bytes, pts });
      }
    },
    onError: (msg_) => post({ type: 'log', msg: `demux err: ${msg_}`, cls: 'err' }),
  });

  inited = true;

  pollTimer = setInterval(() => {
    if (!rx || !inited) return;
    const nowUs = (performance.now() - epoch) * 1000;
    const actions = rx.poll(nowUs);
    processActions(actions);
  }, 10);

  statsTimer = setInterval(() => {
    if (!rx || !inited) return;
    const s = rx.getStats();
    if (!s) return;
    post({ type: 'stats', stats: serializeStats(s) });
  }, 1000);
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
    switch (a.kind) {
      case 0:
        post({ type: 'send', data: a.data });
        break;
      case 1:
        demux?.feed(a.data);
        break;
      case 2:
        post({ type: 'handshakeComplete' });
        break;
      case 3:
        break;
      case 4:
        post({ type: 'close' });
        break;
      case 5:
        post({ type: 'log', msg: `srt: ${a.text}`, cls: 'info' });
        break;
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
