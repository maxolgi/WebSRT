// Phase 6: WebCodecs video decode.
//
// SRT-over-WebTransport → srt-wasm → mpeg2ts-wasm → VideoPipeline → canvas.

import { SrtController } from './srt';
import { Demuxer } from './demux';
import { VideoPipeline, OpusAudioPipeline, AacAudioPipeline } from './decode';
import { CanvasRenderer } from './render';

const logEl = document.getElementById('log') as HTMLPreElement;
const statusEl = document.getElementById('status') as HTMLParagraphElement;
const connectBtn = document.getElementById('connect') as HTMLButtonElement;
const canvas = document.getElementById('video-canvas') as HTMLCanvasElement;
const latencySlider = document.getElementById('latency') as HTMLInputElement;
const latencyNum = document.getElementById('latency-num') as HTMLInputElement;
const statsEl = document.getElementById('stats') as HTMLPreElement;
const muteBtn = document.getElementById('mute') as HTMLButtonElement;

let audioEl: HTMLAudioElement | null = null;
let audioReady = false;
let reconnectTimer: number | null = null;
let reconnectAttempts = 0;
const MAX_RECONNECT_DELAY_MS = 30000;
const BASE_RECONNECT_DELAY_MS = 1000;

function setStatus(s: string) { statusEl.textContent = s; }

function formatLatency(ms: number): string {
  return ms >= 1000 ? `${(ms / 1000).toFixed(ms % 1000 === 0 ? 0 : 1)}s` : `${ms}ms`;
}
latencySlider.addEventListener('input', () => { latencyNum.value = latencySlider.value; });
latencyNum.addEventListener('input', () => {
  const v = Math.max(20, Math.min(8000, +latencyNum.value || 120));
  latencySlider.value = String(v);
});

function log(msg: string, cls = '') {
  const lines = logEl.children;
  if (lines.length > 50) logEl.removeChild(lines[0]);
  const span = document.createElement('span');
  span.className = cls;
  span.textContent = msg + '\n';
  logEl.appendChild(span);
  logEl.scrollTop = logEl.scrollHeight;
}

function hexToBytes(hex: string): Uint8Array {
  const clean = hex.replace(/[:\s]/g, '');
  if (clean.length !== 64) {
    throw new Error(`expected 32-byte (64 hex char) hash, got ${clean.length} hex chars`);
  }
  const out = new Uint8Array(32);
  for (let i = 0; i < 32; i++) {
    out[i] = parseInt(clean.substr(i * 2, 2), 16);
  }
  return out;
}

let wt: WebTransport | null = null;
let srt: SrtController | null = null;
let demux: Demuxer | null = null;
let video: VideoPipeline | null = null;
let audio: OpusAudioPipeline | AacAudioPipeline | null = null;
let renderer: CanvasRenderer | null = null;

// Stream-type IDs from MPEG-TS we recognize.
const ST_H264 = 0x1B; // AVC
const ST_AAC = 0x0F;
const ST_OPUS_FFMPEG = 0x06; // private data (Opus-in-TS via ffmpeg)

const stats = { pat: 0, pmt: 0, pesVideo: 0, pesAudio: 0, ra: 0, decodedFrames: 0 };
let videoPid: number | null = null;
let audioPid: number | null = null;
let audioStreamType: number | null = null;

const audioCb = {
  onError: (e: unknown) => log(`audio err: ${e}`, 'err'),
  onReady: () => {
    log('AudioDecoder ready', 'info');
    audioReady = true;
    wireAudio();
  },
};

connectBtn.addEventListener('click', () => doConnect());

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

function scheduleReconnect() {
  if (reconnectTimer !== null) return;
  const delay = Math.min(BASE_RECONNECT_DELAY_MS * 2 ** reconnectAttempts, MAX_RECONNECT_DELAY_MS);
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
  reconnectAttempts = 0;
}

function teardown() {
  cancelReconnect();
  srt?.stop();
  srt = null;
  try { wt?.close({}); } catch {}
  wt = null;
  demux = null;
  video = null;
  audio = null;
  renderer = null;
  if (audioEl) { try { audioEl.pause(); } catch {} audioEl.srcObject = null; }
  audioEl = null;
  audioReady = false;
  muteBtn.disabled = true;
  muteBtn.textContent = 'muted';
  videoPid = null;
  audioPid = null;
  audioStreamType = null;
  stats.pat = 0; stats.pmt = 0; stats.pesVideo = 0; stats.pesAudio = 0; stats.ra = 0; stats.decodedFrames = 0;
}

function wireAudio() {
  if (!audio || !audioReady) return;
  const track = audio.track;
  if (track) {
    // MediaStreamTrackGenerator path (Chrome).
    if (!audioEl) {
      audioEl = document.createElement('audio');
      audioEl.autoplay = true;
      document.body.appendChild(audioEl);
    }
    audioEl.srcObject = new MediaStream([track]);
    audioEl.muted = false;
    audioEl.play()
      .then(() => {
        log('audio playing ✓', 'ok');
        muteBtn.disabled = false;
        muteBtn.textContent = 'mute';
      })
      .catch((e) => {
        log(`audio autoplay blocked — click "muted" to enable: ${e}`, 'err');
        muteBtn.disabled = false;
        muteBtn.textContent = 'muted';
      });
  } else {
    // AudioWorklet path (Firefox). AudioContext needs a user gesture.
    audio!.resume()
      .then(() => {
        log('audio playing ✓ (AudioWorklet)', 'ok');
        muteBtn.disabled = false;
        muteBtn.textContent = 'mute';
      })
      .catch((e) => log(`audio resume failed: ${e}`, 'err'));
  }
}

async function doConnect() {
  teardown();
  const hashHex = (window as any).CERT_HASH as string | null | undefined;
  if (hashHex === undefined) {
    log('No cert-hash.js — is the gateway running?', 'err');
    return;
  }
  try {
    renderer = new CanvasRenderer(canvas);
    video = new VideoPipeline({
      onFrame: (frame) => {
        stats.decodedFrames++;
        renderer?.draw(frame);
        if (stats.decodedFrames === 1) {
          log(`first frame decoded ✓ (${frame.displayWidth}x${frame.displayHeight})`, 'ok');
          setStatus(`decoding ${frame.displayWidth}x${frame.displayHeight}`);
        }
      },
      onError: (e) => log(`video err: ${e}`, 'err'),
      onConfigured: (info) =>
        log(`VideoDecoder configured (profile ${info.profile}, level ${info.level})`, 'info'),
    });

    const pageHost = location.hostname || '127.0.0.1';
    const wtHost = pageHost === 'localhost' ? '127.0.0.1' : pageHost;
    const wtUrl = `https://${wtHost}:4433/wt`;
    const wtOpts: WebTransportOptions = {};
    if (hashHex) {
      const hash = hexToBytes(hashHex);
      wtOpts.serverCertificateHashes = [{ algorithm: 'sha-256', value: hash as BufferSource }];
      log(`connecting to ${wtUrl} (self-signed, hash ${hashHex.slice(0, 8)}…) …`, 'info');
    } else {
      log(`connecting to ${wtUrl} (mkcert/PKI) …`, 'info');
    }
    wt = new WebTransport(wtUrl, wtOpts);
    await wt.ready;
    log('WT ready ✓', 'ok');
    setStatus('WT ready; awaiting SRT handshake');

    demux = await Demuxer.create({
      onPat: (_prog, _pid) => {},
      onPmt: (entries) => {
        for (const e of entries) {
          if (e.streamType === ST_H264) {
            videoPid = e.pid;
          } else if (e.streamType === ST_AAC || e.streamType === ST_OPUS_FFMPEG) {
            audioPid = e.pid;
            audioStreamType = e.streamType;
          }
        }
        if (audioPid !== null && !audio) {
          const isOpus = audioStreamType === ST_OPUS_FFMPEG;
          log(`audio PID ${audioPid}: ${isOpus ? 'Opus' : 'AAC'} (stream type 0x${audioStreamType!.toString(16)})`, 'info');
          audio = isOpus
            ? new OpusAudioPipeline(audioCb)
            : new AacAudioPipeline(audioCb);
        }
      },
      onPes: (pid, pts, _dts, bytes, ra) => {
        if (pid === videoPid) {
          stats.pesVideo++;
          if (ra) stats.ra++;
          video?.feed(bytes, pts, ra);
        } else if (pid === audioPid) {
          stats.pesAudio++;
          audio?.feed(bytes, pts);
        }
      },
      onRandomAccess: () => {},
      onError: (msg) => log(`demux err: ${msg}`, 'err'),
    });

    const latencyMs = +latencySlider.value;
    log(`TSBPD latency: ${formatLatency(latencyMs)}`, 'info');
    srt = await SrtController.start(wt, latencyMs, {
      onLog: (m, c) => log(m, c),
      onHandshakeComplete: () => {
        reconnectAttempts = 0;
        setStatus('SRT connected; awaiting video stream');
      },
      onDeliverMessage: (b) => demux?.feed(b),
      onClose: () => {
        log('session closed', 'err');
        setStatus('closed');
      },
    });

    wt.closed
      .then(() => { log('WT closed', 'info'); scheduleReconnect(); })
      .catch((e) => { log(`WT closed (err): ${e}`, 'err'); scheduleReconnect(); });
  } catch (e) {
    log(`connect failed: ${e}`, 'err');
    scheduleReconnect();
  }
}

if ((window as any).CERT_HASH !== undefined) {
  log((window as any).CERT_HASH ? 'Cert hash loaded — auto-connecting…' : 'mkcert mode — auto-connecting…', 'info');
  doConnect();
} else {
  log('No cert-hash.js. Start the gateway first, then reload.', 'info');
}

setInterval(() => {
  if (!srt) { statsEl.textContent = ''; return; }
  const s = srt.getStats();
  if (!s) return;
  const rxData = Number(s.rxData);
  const rxLoss = Number(s.rxLoss);
  const lossRate = (rxData + rxLoss) > 0
    ? ((rxLoss / (rxData + rxLoss)) * 100).toFixed(2)
    : '0.00';
  const mbps = (Number(s.bandwidthBps) / 1e6).toFixed(1);
  const elapsed = (s.elapsedMs / 1000).toFixed(0);
  statsEl.textContent =
    `uptime   ${elapsed}s\n` +
    `RTT      ${s.rttMs.toFixed(1)}ms\n` +
    `bw       ${mbps} Mbps\n` +
    `rx pkts  ${rxData}\n` +
    `rx bytes ${(Number(s.rxBytes) / 1e6).toFixed(1)} MB\n` +
    `loss     ${rxLoss} (${lossRate}%)\n` +
    `re-xmit  ${Number(s.rxRetransmit)}\n` +
    `dropped  ${Number(s.rxDropped)}\n` +
    `belated  ${Number(s.rxBelated)}\n` +
    `buf'd    ${Number(s.rxBuffered)}\n` +
    `ACK/NAK  ${Number(s.rxAck)}/${Number(s.rxNak)}`;
}, 1000);
