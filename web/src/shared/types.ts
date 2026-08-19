// Shared type contracts between the core playback modules (worker, decode,
// render) and the debug panel. These interfaces are part of the public core
// surface: an embedder that excludes debug/** must still be able to typecheck
// against the core, so the definitions live here rather than under debug/.

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

export interface AudioMeterData {
  pids: number[];
  channelCounts: number[];
  peaks: number[];
  rms: number[];
  clips: number[];
  lufs: number[];
  phase: number[];
  scopeL: number[];
  scopeR: number[];
  spectrum: number[];
  selectedPid: number;
  selectedChannel: number;
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
  /**
   * Rolling (~8.5 s at 60 Hz) histogram of decoded-frame arrivals per rAF
   * interval: index = arrivals (capped at 4+), value = number of intervals.
   * With the cap-1 baseline ring, index 1 is the only lossless slot; 0 =
   * stall slot, ≥2 = burst slot (forces a drop).
   */
  arrivalHistogram: number[];
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
