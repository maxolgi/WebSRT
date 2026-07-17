// Main-thread sampler: reads decoder/renderer stats at 10fps and pushes to
// the DebugStore. Only active when the debug panel is visible.
// Also captures console.error into a ring buffer for diagnostics export.

import type { DebugStore } from './store';
import type { VideoPipeline } from '../decode';
import type { OpusAudioPipeline, AacAudioPipeline } from '../decode';
import type { CanvasRenderer } from '../render';
import type { TimeSeriesBucket } from './types';

export interface PipelineRefs {
  video: VideoPipeline | null;
  audio: OpusAudioPipeline | AacAudioPipeline | null;
  renderer: CanvasRenderer | null;
}

const SAMPLE_INTERVAL_MS = 100;

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
    if (srt) {
      const totalPkts = srt.rxData + srt.rxLoss;
      const bucket: TimeSeriesBucket = {
        t: performance.now(),
        rttMs: srt.rttMs,
        bandwidthMbps: srt.bandwidthBps / 1e6,
        lossRate: totalPkts > 0 ? srt.rxLoss / totalPkts : 0,
        videoQueueDepth: v?.decodeQueueSize ?? 0,
        audioQueueDepth: a?.decodeQueueSize ?? 0,
        fps: r?.fps ?? 0,
        rafDeltaMs: r?.rafDeltaMs ?? 0,
      };
      store.pushHistory(bucket);
    }
  }, SAMPLE_INTERVAL_MS);

  return () => clearInterval(id);
}

const originalConsoleError = console.error;

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
