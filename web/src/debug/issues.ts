// Derived health flags for the debug panel issues strip. Each check reads
// the current values of the store signals and emits an Issue when its
// threshold is exceeded; absent signals are skipped.

import type { DebugStore } from './store';

export interface Issue {
  id: string;
  label: string;
  severity: 'warn' | 'error';
  tab: string;
  demuxSub?: string;
}

function sum(arr: Float64Array): number {
  let total = 0;
  for (let i = 0; i < arr.length; i++) total += arr[i];
  return total;
}

function max(arr: Float64Array): number {
  let m = 0;
  for (let i = 0; i < arr.length; i++) if (arr[i] > m) m = arr[i];
  return m;
}

export function computeIssues(store: DebugStore): Issue[] {
  const issues: Issue[] = [];

  const srt = store.srtStats.value;
  if (srt) {
    const lossRate = srt.rxLoss / Math.max(1, srt.rxData + srt.rxLoss);
    if (lossRate > 0.05) {
      issues.push({ id: 'srt-loss', label: `SRT loss ${(lossRate * 100).toFixed(1)}%`, severity: 'error', tab: 'srt' });
    } else if (lossRate > 0.01) {
      issues.push({ id: 'srt-loss', label: `SRT loss ${(lossRate * 100).toFixed(1)}%`, severity: 'warn', tab: 'srt' });
    }
    if (srt.rxDropped > 0) {
      issues.push({ id: 'srt-dropped', label: `Dropped (too-late) ${srt.rxDropped}`, severity: 'warn', tab: 'srt' });
    }
    if (srt.rxRetransmit > 0) {
      issues.push({ id: 'srt-retransmit', label: `NAK retransmits ${srt.rxRetransmit}`, severity: 'warn', tab: 'srt' });
    }
  }

  const video = store.videoStats.value;
  if (video && video.decodeQueueSize > 8) {
    issues.push({ id: 'video-queue', label: `Video queue ${video.decodeQueueSize}`, severity: 'warn', tab: 'codec' });
  }

  const audio = store.audioStats.value;
  if (audio && audio.decodeQueueSize > 20) {
    issues.push({ id: 'audio-queue', label: `Audio queue ${audio.decodeQueueSize}`, severity: 'warn', tab: 'audio' });
  }

  const render = store.renderStats.value;
  if (render) {
    const dropped = render.droppedLate + render.droppedOverflow;
    if (dropped > 0) {
      issues.push({ id: 'render-drop', label: `Render dropped ${dropped}`, severity: 'warn', tab: 'codec' });
    }
    if (render.rafDeltaMs > 34) {
      issues.push({ id: 'raf-slow', label: `rAF ${Math.round(render.rafDeltaMs)}ms`, severity: 'warn', tab: 'stream' });
    }
  }

  const demux = store.demuxStats.value;
  if (demux) {
    const ccTotal = sum(demux.ccErrors);
    if (ccTotal > 0) {
      issues.push({ id: 'cc-errors', label: `CC errors ${ccTotal}`, severity: 'error', tab: 'demux', demuxSub: 'errors' });
    }
    if (demux.errorMsg.length > 0) {
      issues.push({ id: 'demux-errors', label: `Demux errors ${demux.errorMsg.length}`, severity: 'error', tab: 'demux', demuxSub: 'errors' });
    }
    const jumps = sum(demux.ptsJumps);
    if (jumps > 0) {
      issues.push({ id: 'pts-jumps', label: `PTS jumps ${jumps}`, severity: 'warn', tab: 'demux', demuxSub: 'streams' });
    }
    const pcrMax = max(demux.pcrIntervalsMs);
    if (pcrMax > 100) {
      issues.push({ id: 'pcr-interval', label: `PCR interval ${Math.round(pcrMax)}ms`, severity: 'warn', tab: 'demux', demuxSub: 'video' });
    }
  }

  issues.sort((a, b) => (a.severity === b.severity ? 0 : a.severity === 'error' ? -1 : 1));
  return issues;
}
