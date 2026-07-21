// Shared PMT-summarization helper. Extracts video/audio PID + codec from a
// flat list of PMT entries. Used by the worker (session setup), the main
// viewer (one-line demux summary), and the debug sampler (history bucket).

export type VideoCodec = 'av1' | 'h264' | 'hevc';

export interface PmtEntry {
  pid: number;
  streamType: number;
  formatId: string | null;
}

export interface PmtSummary {
  /** Video PID, or -1 if no video stream identified. */
  videoPid: number;
  /** Detected video codec, or null if no video stream identified. */
  videoCodec: VideoCodec | null;
  /** Audio PID, or -1 if no audio stream identified. */
  audioPid: number;
  /** Numeric audio stream_type (0x0f for AAC, 0x06 for Opus), or -1. */
  audioStreamType: number;
}

export const ST_H264 = 0x1b;
export const ST_HEVC = 0x24;
export const ST_AAC = 0x0f;
export const ST_PRIVATE = 0x06;

const NO_STREAM: PmtSummary = {
  videoPid: -1,
  videoCodec: null,
  audioPid: -1,
  audioStreamType: -1,
};

/**
 * Walk a list of PMT entries and identify the video and audio elementary
 * stream PIDs. AV1 and Opus ride on the same PRIVATE (0x06) stream type and
 * are disambiguated by the registration-descriptor format id ("AV01" vs
 * "Opus"). A 0x06 entry with no recognized format id is left unresolved here
 * — callers that need content-probing (worker.ts) handle that path
 * separately.
 */
export function summarizePmt(entries: PmtEntry[]): PmtSummary {
  let videoPid = -1;
  let videoCodec: VideoCodec | null = null;
  let audioPid = -1;
  let audioStreamType = -1;
  for (const e of entries) {
    if (e.streamType === ST_H264) {
      videoPid = e.pid;
      videoCodec = 'h264';
    } else if (e.streamType === ST_HEVC) {
      videoPid = e.pid;
      videoCodec = 'hevc';
    } else if (e.streamType === ST_AAC) {
      audioPid = e.pid;
      audioStreamType = e.streamType;
    } else if (e.streamType === ST_PRIVATE) {
      if (e.formatId === 'AV01') {
        videoPid = e.pid;
        videoCodec = 'av1';
      } else if (e.formatId === 'Opus') {
        audioPid = e.pid;
        audioStreamType = e.streamType;
      }
      // Unknown formatId on PRIVATE: skip — caller may content-probe.
    }
  }
  return { videoPid, videoCodec, audioPid, audioStreamType };
}

/** Empty summary sentinel for "no PMT seen yet". */
export const NO_PMT_SUMMARY: PmtSummary = NO_STREAM;
