import { SrtController, type SrtStats } from './srt';
import { Demuxer } from './demux';

export type WorkerCmd =
  | { cmd: 'connect'; url: string; certHash: Uint8Array | null; latencyMs: number }
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
  | { type: 'wtReady' }
  | { type: 'handshakeComplete' }
  | { type: 'pmt'; videoPid: number; audioPid: number; audioStreamType: number }
  | { type: 'videoPes'; data: Uint8Array; pts: number | null; isKeyframe: boolean }
  | { type: 'audioPes'; data: Uint8Array; pts: number | null }
  | { type: 'stats'; stats: StatsMsg }
  | { type: 'close' }
  | { type: 'wtClosed' }
  | { type: 'wtError'; error: string };

const ST_H264 = 0x1b;
const ST_AAC = 0x0f;
const ST_OPUS_FFMPEG = 0x06;

let srt: SrtController | null = null;
let demux: Demuxer | null = null;
let wt: WebTransport | null = null;
let statsTimer: ReturnType<typeof setInterval> | null = null;
let videoPid: number | null = null;
let audioPid: number | null = null;
let audioStreamType: number | null = null;

self.onmessage = async (e: MessageEvent) => {
  const cmd = e.data as WorkerCmd;
  switch (cmd.cmd) {
    case 'connect':
      await doConnect(cmd.url, cmd.certHash, cmd.latencyMs);
      break;
    case 'stop':
      doTeardown();
      break;
  }
};

function post(msg: WorkerMsg) {
  (self as unknown as Worker).postMessage(msg);
}

async function doConnect(url: string, certHash: Uint8Array | null, latencyMs: number) {
  doTeardown();
  videoPid = null;
  audioPid = null;
  audioStreamType = null;

  const wtOpts: WebTransportOptions = {};
  if (certHash) {
    wtOpts.serverCertificateHashes = [{ algorithm: 'sha-256', value: certHash as BufferSource }];
  }

  try {
    wt = new WebTransport(url, wtOpts);
    await wt.ready;
    post({ type: 'wtReady' });
  } catch (e) {
    post({ type: 'wtError', error: String(e) });
    return;
  }

  wt.closed
    .then(() => post({ type: 'wtClosed' }))
    .catch((err) => post({ type: 'wtError', error: String(err) }));

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
    onError: (msg) => post({ type: 'log', msg: `demux err: ${msg}`, cls: 'err' }),
  });

  srt = await SrtController.start(wt, latencyMs, {
    onLog: (msg, cls) => post({ type: 'log', msg, cls }),
    onHandshakeComplete: () => post({ type: 'handshakeComplete' }),
    onDeliverMessage: (bytes) => demux?.feed(bytes),
    onClose: () => post({ type: 'close' }),
  });

  statsTimer = setInterval(() => {
    if (!srt) return;
    const s = srt.getStats();
    if (!s) return;
    post({ type: 'stats', stats: serializeStats(s) });
  }, 1000);
}

function doTeardown() {
  if (statsTimer) { clearInterval(statsTimer); statsTimer = null; }
  srt?.stop();
  srt = null;
  try { wt?.close({}); } catch {}
  wt = null;
  demux = null;
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
