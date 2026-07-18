// MPEG-TS stream_type → human name. See ISO/IEC 13818-1 table 2-34.
// 0x06 (Private) is disambiguated via the PMT registration-descriptor
// format ID elsewhere (AV01 → AV1, Opus → Opus).

const NAMES: Record<number, string> = {
  0x01: 'MPEG-1 Video',
  0x02: 'MPEG-2 Video',
  0x03: 'MPEG-1 Audio',
  0x04: 'MPEG-2 Audio',
  0x0f: 'AAC',
  0x1b: 'H.264',
  0x24: 'HEVC',
  0x06: 'Private',
};

export function streamTypeName(streamType: number): string {
  return NAMES[streamType] ?? `0x${streamType.toString(16)}`;
}
