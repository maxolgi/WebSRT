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
  TestActions,
} from './types';
import type { StatsMsg } from '../worker';

const HISTORY_CAP = 300;

export interface LogEntry {
  msg: string;
  cls: string;
}

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
  readonly activeTab = signal<string>('stream');
  readonly testActions = signal<TestActions | null>(null);
  readonly status = signal('idle');
  readonly logEntries = signal<LogEntry[]>([]);
  readonly driftMs = signal<number | null>(null);

  pushHistory(bucket: TimeSeriesBucket) {
    const h = this.history.value;
    this.history.value =
      h.length >= HISTORY_CAP ? [...h.slice(1), bucket] : [...h, bucket];
  }

  pushConsoleError(msg: string) {
    const errs = this.consoleErrors.value;
    this.consoleErrors.value = [...errs.slice(-49), msg];
  }

  pushLog(msg: string, cls = '') {
    const entries = this.logEntries.value;
    this.logEntries.value = [...entries.slice(-49), { msg, cls }];
  }

  reset() {
    this.history.value = [];
    this.consoleErrors.value = [];
    this.logEntries.value = [];
  }
}
