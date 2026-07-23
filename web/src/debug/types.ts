// Shared type contract for the debug panel. All agents and components agree
// on these interfaces. Do not change field names without updating all
// consumers (sampler, components, diagnostics export).

export interface VideoStats {
  codec: 'h264' | 'hevc' | 'av1' | null;
  codecString: string | null;
  decoderState: string;
  decodeQueueSize: number;
  decodedCount: number;
  decodeFps: number;
  droppedFrames: number;
  hwAcceleration: string | undefined;
  hwModePreference: 'prefer-hardware' | 'prefer-software';
  reconfigureCount: number;
  profile: number;
  level: number;
  codedWidth: number;
  codedHeight: number;
}

export interface AudioStats {
  codec: string | null;
  decoderState: string;
  decodeQueueSize: number;
  packetsDecoded: number;
  droppedPackets: number;
  sampleRate: number;
  channels: number;
  outputMode: 'MediaStreamTrackGenerator' | 'AudioWorklet' | null;
}

export interface RenderStats {
  frameCount: number;
  droppedLate: number;
  droppedOverflow: number;
  ringLength: number;
  ringCap: number;
  currentPtsUs: number | null;
  fps: number;
  rafDeltaMs: number;
}

// Mirrors the WASM `DebugSnapshot` (crates/mpeg2ts-wasm/src/lib.rs).
// Every typed array is a fresh JS-owned copy; the snapshot struct is GC'd.
// Per-PID arrays are parallel to `pids`; `scramblingCounts`/`afControlCounts`
// are flat 4×N, `nalStats` is flat 9×M (see WASM doc comment).
export interface DemuxStats {
  programNum: number;
  pmtPid: number;
  pmtPids: Uint16Array;
  pmtStreamTypes: Uint8Array;
  pmtFormatIds: string[];
  pids: Uint16Array;
  pesCounts: Float64Array;
  byteTotals: Float64Array;
  bitratesMbps: Float64Array;
  raCounts: Float64Array;
  lastPts: Float64Array;
  lastDts: Float64Array;
  ptsJumps: Float64Array;
  ccErrors: Float64Array;
  teiCounts: Float64Array;
  pusiCounts: Float64Array;
  scramblingCounts: Float64Array;
  afControlCounts: Float64Array;
  pcrPids: Uint16Array;
  pcrIntervalsMs: Float64Array;
  pcrJitterMs: Float64Array;
  nalPids: Uint16Array;
  nalStats: Float64Array;
  errorT: Float64Array;
  errorMsg: string[];
  // Packet ring — populated by WASM, rendered by the packet-timeline commit.
  ringT: Float64Array;
  ringPid: Uint16Array;
  ringKind: Uint8Array;
  ringPts: Float64Array;
  ringDts: Float64Array;
  ringSize: Float64Array;
  ringRa: Uint8Array;
  ringTei: Uint8Array;
  ringPusi: Uint8Array;
  ringNal: Uint8Array;
  ringNalOffsets: Uint32Array;
}

// Plain-JSON form of DemuxStats (typed arrays → number[]) for the
// diagnostics export, so the downloaded JSON reads as arrays not index-objects.
export interface DemuxStatsSerialized {
  programNum: number;
  pmtPid: number;
  pmtPids: number[];
  pmtStreamTypes: number[];
  pmtFormatIds: string[];
  pids: number[];
  pesCounts: number[];
  byteTotals: number[];
  bitratesMbps: number[];
  raCounts: number[];
  lastPts: number[];
  lastDts: number[];
  ptsJumps: number[];
  ccErrors: number[];
  teiCounts: number[];
  pusiCounts: number[];
  scramblingCounts: number[];
  afControlCounts: number[];
  pcrPids: number[];
  pcrIntervalsMs: number[];
  pcrJitterMs: number[];
  nalPids: number[];
  nalStats: number[];
  errorT: number[];
  errorMsg: string[];
}

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
}
