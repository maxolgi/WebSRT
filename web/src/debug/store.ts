// Central reactive store using @preact/signals-core.
// All debug panel components read from these signals. Data sources write to
// them: the sampler (main-thread decoder/renderer stats), worker messages
// (SRT stats), and on-demand queries (MediaCapabilities, GPU info).

import { signal } from '@preact/signals-core';
import type {
  VideoStats,
  AudioStats,
  RenderStats,
  GpuInfo,
  MediaCapResult,
  TimeSeriesBucket,
  DemuxStats,
} from './types';
import type { StatsMsg } from '../worker';

const HISTORY_CAP = 300;

export class DebugStore {
  readonly srtStats = signal<StatsMsg | null>(null);
  readonly demuxStats = signal<DemuxStats | null>(null);
  readonly videoStats = signal<VideoStats | null>(null);
  readonly audioStats = signal<AudioStats | null>(null);
  readonly renderStats = signal<RenderStats | null>(null);
  readonly gpuInfo = signal<GpuInfo | null>(null);
  readonly mediaCaps = signal<MediaCapResult[]>([]);
  readonly mediaCapsLoading = signal(false);
  readonly history = signal<TimeSeriesBucket[]>([]);
  readonly panelVisible = signal(false);
  readonly latencyMs = signal(300);
  readonly certMode = signal('unknown');
  readonly consoleErrors = signal<string[]>([]);
  readonly activeTab = signal<string>('codec');

  pushHistory(bucket: TimeSeriesBucket) {
    const h = this.history.value;
    this.history.value =
      h.length >= HISTORY_CAP ? [...h.slice(1), bucket] : [...h, bucket];
  }

  pushConsoleError(msg: string) {
    const errs = this.consoleErrors.value;
    this.consoleErrors.value = [...errs.slice(-49), msg];
  }

  reset() {
    this.history.value = [];
    this.consoleErrors.value = [];
  }
}
