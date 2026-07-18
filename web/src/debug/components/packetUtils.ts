// Helpers for the packet timeline (Demux tab). Pure functions, no Preact.

export type VideoCodec = 'h264' | 'hevc' | null;

const H264_NAL: Record<number, string> = {
  1: 'Slice', 2: 'DPA', 3: 'DPB', 4: 'DPC', 5: 'IDR',
  6: 'SEI', 7: 'SPS', 8: 'PPS', 9: 'AUD',
};

const HEVC_NAL: Record<number, string> = {
  0: 'TRAIL_N', 1: 'TRAIL_R', 19: 'IDR_W_RADL', 20: 'IDR_N_LP',
  32: 'VPS', 33: 'SPS', 34: 'PPS', 39: 'PREFIX_SEI', 40: 'SUFFIX_SEI',
};

const KIND_NAMES: Record<number, string> = {
  0: 'PAT', 1: 'PMT', 2: 'PES', 3: 'RA', 4: 'Error', 255: 'Other',
};

// `byte` is the extracted NAL unit type as stored in ringNal by the WASM.
// Mask H.264 defensively in case a future change stores the full header byte.
export function nalTypeName(byte: number, codec: VideoCodec): string {
  if (codec === 'hevc') return HEVC_NAL[byte] ?? `type${byte}`;
  return H264_NAL[byte & 0x1f] ?? `type${byte & 0x1f}`;
}

export function kindName(kind: number): string {
  return KIND_NAMES[kind] ?? `kind${kind}`;
}

export function formatBytes(n: number): string {
  if (n < 1024) return `${n} B`;
  if (n < 1024 * 1024) return `${(n / 1024).toFixed(1)} KiB`;
  return `${(n / (1024 * 1024)).toFixed(2)} MiB`;
}

// Parse "0x1010" or "4112" -> 4112. null for empty/unparseable input.
export function parsePidFilter(s: string): number | null {
  const t = s.trim();
  if (!t) return null;
  let n: number;
  if (/^0x[0-9a-fA-F]+$/.test(t)) n = parseInt(t.slice(2), 16);
  else if (/^\d+$/.test(t)) n = parseInt(t, 10);
  else return null;
  return Number.isNaN(n) ? null : n;
}

export function pidHex(pid: number): string {
  return `0x${pid.toString(16).padStart(4, '0')}`;
}
