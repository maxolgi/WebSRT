// Shared type contract for the debug panel. All agents and components agree
// on these interfaces. Do not change field names without updating all
// consumers (sampler, components, diagnostics export).

export interface VideoStats {
  codec: 'h264' | 'hevc' | 'av1' | null;
  codecString: string | null;
  decoderState: string;
  decodeQueueSize: number;
  decodedCount: number;
  droppedFrames: number;
  hwAcceleration: string | undefined;
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

export interface DemuxStats {
  pat: number;
  pmt: number;
  pes: number;
  ra: number;
  err: number;
  raw: number;
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
  demux: DemuxStats | null;
  latencyMs: number;
  certMode: string;
  history: TimeSeriesBucket[];
  consoleErrors: string[];
}

export interface TestActions {
  resetDecoder: () => void;
  reconnect: () => void;
  cycleLatency: () => void;
}
