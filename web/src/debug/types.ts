// Shared type contract for the debug panel. All agents and components agree
// on these interfaces. Do not change field names without updating all
// consumers (sampler, components, diagnostics export).
//
// The five core stat interfaces live in ../shared/types so the core modules
// (worker, decode, render) can typecheck without depending on debug/**.
// Debug-specific types (GpuInfo, MediaCapResult, etc.) are defined here.
import type { VideoStats, AudioStats, RenderStats, DemuxStatsSerialized } from '../shared/types';

export interface GpuInfo {
  vendor: string | null;
  renderer: string | null;
  available: boolean;
}

export interface MediaCapResult {
  codec: string;
  width: number;
  height: number;
  framerate: number;
  bitrate: number;
  supported: boolean;
  powerEfficient: boolean;
  smooth: boolean;
  hwAcceleration: string | undefined;
}

export interface TimeSeriesBucket {
  t: number;
  rttMs: number;
  bandwidthMbps: number;
  lossRate: number;
  videoQueueDepth: number;
  audioQueueDepth: number;
  fps: number;
  rafDeltaMs: number;
  videoMbps: number;
  audioMbps: number;
  ccErrors: number;
  srtLoss: number;
  srtDropped: number;
  pollMaxMs: number;
  wasmHandleAvgUs: number;
  wasmPollAvgUs: number;
  loopIterAvgMs: number;
}

export interface DebugDiagnostics {
  timestamp: string;
  browser: {
    userAgent: string;
    platform: string;
    language: string;
    hardwareConcurrency: number;
    deviceMemory: number | null;
  };
  gpu: GpuInfo | null;
  capabilities: MediaCapResult[];
  video: VideoStats | null;
  audio: AudioStats | null;
  render: RenderStats | null;
  srt: unknown | null;
  demux: DemuxStatsSerialized | null;
  latencyMs: number;
  certMode: string;
  history: TimeSeriesBucket[];
  consoleErrors: string[];
}

export interface TestActions {
  resetDecoder: () => void;
  reconnect: () => void;
  cycleLatency: () => void;
  setHwMode: (mode: 'prefer-hardware' | 'prefer-software') => void;
  setDecodePacing: (enabled: boolean) => void;
  setRenderPacing: (enabled: boolean) => void;
}
