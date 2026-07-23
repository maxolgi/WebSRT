// Main-thread sampler: reads decoder/renderer stats at 10fps and pushes to
// the DebugStore. Only active when the debug panel is visible.
// Also captures console.error into a ring buffer for diagnostics export.

import type { DebugStore } from './store';
import type { VideoPipeline } from '../decode';
import type { OpusAudioPipeline, AacAudioPipeline } from '../decode';
import type { CanvasRenderer } from '../render';
import type { DemuxStats, TimeSeriesBucket } from './types';
import { summarizePmt, type PmtEntry } from '../shared/pmt';

export interface PipelineRefs {
  video: VideoPipeline | null;
  audio: OpusAudioPipeline | AacAudioPipeline | null;
  renderer: CanvasRenderer | null;
}

const SAMPLE_INTERVAL_MS = 100;

let prevRxLoss = 0;
let prevRxDropped = 0;

export function startSampler(
  store: DebugStore,
  getRefs: () => PipelineRefs,
): () => void {
  const id = setInterval(() => {
    const { video, audio, renderer } = getRefs();

    if (video) {
      store.videoStats.value = video.getStats();
    }
    if (audio) {
      store.audioStats.value = audio.getStats();
    }
    if (renderer) {
      store.renderStats.value = renderer.getStats();
    }

    // Push history bucket
    const srt = store.srtStats.value;
    const v = store.videoStats.value;
    const a = store.audioStats.value;
    const r = store.renderStats.value;
    const demux = store.demuxStats.value;
    if (srt) {
      const totalPkts = srt.rxData + srt.rxLoss;
      const { videoMbps, audioMbps, ccErrors } = summarizeDemux(demux);
      const srtLoss = Math.max(0, srt.rxLoss - prevRxLoss);
      const srtDropped = Math.max(0, srt.rxDropped - prevRxDropped);
      prevRxLoss = srt.rxLoss;
      prevRxDropped = srt.rxDropped;
      const bucket: TimeSeriesBucket = {
        t: performance.now(),
        rttMs: srt.rttMs,
        bandwidthMbps: srt.bandwidthBps / 1e6,
        lossRate: totalPkts > 0 ? srt.rxLoss / totalPkts : 0,
        videoQueueDepth: v?.decodeQueueSize ?? 0,
        audioQueueDepth: a?.decodeQueueSize ?? 0,
        fps: r?.fps ?? 0,
        rafDeltaMs: r?.rafDeltaMs ?? 0,
        videoMbps,
        audioMbps,
        ccErrors,
        srtLoss,
        srtDropped,
        pollMaxMs: srt.pollMaxMs,
      };
      store.pushHistory(bucket);
    }
  }, SAMPLE_INTERVAL_MS);

  return () => clearInterval(id);
}

const originalConsoleError = console.error;

/**
 * Identify video/audio PIDs from the PMT and read their current bitrates plus
 * total CC-error count from the WASM snapshot. Bitrate/jump logic itself now
 * lives in Rust; this is a pure read for the history bucket.
 */
function summarizeDemux(d: DemuxStats | null): {
  videoMbps: number;
  audioMbps: number;
  ccErrors: number;
} {
  if (!d || d.pids.length === 0) return { videoMbps: 0, audioMbps: 0, ccErrors: 0 };
  const pmtEntries: PmtEntry[] = [];
  for (let i = 0; i < d.pmtPids.length; i++) {
    pmtEntries.push({
      pid: d.pmtPids[i],
      streamType: d.pmtStreamTypes[i],
      formatId: d.pmtFormatIds[i] || null,
    });
  }
  const summary = summarizePmt(pmtEntries);
  const videoPid = summary.videoPid;
  const audioPid = summary.audioPid;
  let videoMbps = 0;
  let audioMbps = 0;
  let ccErrors = 0;
  for (let i = 0; i < d.pids.length; i++) {
    if (d.pids[i] === videoPid) videoMbps = d.bitratesMbps[i];
    else if (d.pids[i] === audioPid) audioMbps = d.bitratesMbps[i];
    ccErrors += d.ccErrors[i];
  }
  return { videoMbps, audioMbps, ccErrors };
}

export function attachConsoleErrorCapture(store: DebugStore): () => void {
  console.error = function (...args: unknown[]) {
    const msg = args.map((a) =>
      typeof a === 'string' ? a : (() => { try { return JSON.stringify(a); } catch { return String(a); } })()
    ).join(' ');
    store.pushConsoleError(msg);
    originalConsoleError.apply(console, args as any);
  };
  return () => { console.error = originalConsoleError; };
}
