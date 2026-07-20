// H.264 NAL parsing + WebCodecs VideoDecoder driving.
//
// The browser receives PES packets from the demuxer. Each PES payload (for a
// H.264 elementary stream) is in MPEG-TS Annex B format:
//   [00 00 00 01] [nal header byte] [nal payload] [00 00 00 01] ...
// We split into NALUs, find SPS/PPS for avcC construction, and feed the rest
// (length-prefixed) to a VideoDecoder as EncodedVideoChunks.
//
// WebCodecs wants an `avcC` box for configuration, which encodes the SPS/PPS
// in the AVC decoder config record. After configure, every IDR slice (and
// every non-IDR slice) is fed as its own EncodedVideoChunk with the NALU
// payload length-prefixed (4-byte big-endian) instead of Annex B start codes.

export interface DecoderCallbacks {
  onFrame: (frame: VideoFrame) => void;
  onError: (e: unknown) => void;
  onConfigured: (info: { width: number; height: number; profile: number; level: number }) => void;
}

// H.264 NAL types we care about.
const NAL_UNSPECIFIED = 0;
const NAL_SLICE = 1;
const NAL_IDR = 5;
const NAL_SEI = 6;
const NAL_SPS = 7;
const NAL_PPS = 8;
const NAL_AUD = 9;

// HEVC NAL types.
const HEVC_TRAIL_N = 0;
const HEVC_IDR_W_RADL = 19;
const HEVC_IDR_N_LP = 20;
const HEVC_CRA = 21;
const HEVC_VPS = 32;
const HEVC_SPS = 33;
const HEVC_PPS = 34;

interface NalUnit {
  type: number;     // H.264 type: data[0] & 0x1f
  hevcType: number; // HEVC type: (data[0] >> 1) & 0x3f
  data: Uint8Array; // includes the 1-byte header (H.264) or 2-byte header (HEVC)
}

/** Split an Annex-B byte stream into individual NALUs (no start codes). */
export function parseAnnexB(payload: Uint8Array): NalUnit[] {
  const out: NalUnit[] = [];
  // Track each start code's first byte (00 of 00 00 01 / 00 00 00 01) so each
  // NALU spans from just after its start code to the start of the next one.
  const scBegin: number[] = [];
  for (let i = 0; i + 2 < payload.length; ) {
    if (
      i + 3 < payload.length &&
      payload[i] === 0 &&
      payload[i + 1] === 0 &&
      payload[i + 2] === 0 &&
      payload[i + 3] === 1
    ) {
      scBegin.push(i);
      i += 4;
    } else if (payload[i] === 0 && payload[i + 1] === 0 && payload[i + 2] === 1) {
      scBegin.push(i);
      i += 3;
    } else {
      i++;
    }
  }
  for (let s = 0; s < scBegin.length; s++) {
    const begin = scBegin[s];
    const scLen = payload[begin + 2] === 1 ? 3 : 4;
    const start = begin + scLen;
    const end = s + 1 < scBegin.length ? scBegin[s + 1] : payload.length;
    if (end > start) {
      const data = payload.subarray(start, end);
      if (data.length > 0) {
        const type = data[0] & 0x1f;
        const hevcType = (data[0] >> 1) & 0x3f;
        out.push({ type, hevcType, data });
      }
    }
  }
  return out;
}

/** Minimal bit-stream reader for SPS exp-Golomb parsing. */
class BitReader {
  private data: Uint8Array;
  private bytePos: number;
  private bitPos = 0; // 0=MSB .. 7=LSB

  constructor(data: Uint8Array, startByte: number) {
    this.data = data;
    this.bytePos = startByte;
  }

  readBit(): number {
    if (this.bytePos >= this.data.length) return 0;
    const bit = (this.data[this.bytePos] >> (7 - this.bitPos)) & 1;
    this.bitPos++;
    if (this.bitPos === 8) { this.bitPos = 0; this.bytePos++; }
    return bit;
  }

  readBits(n: number): number {
    let v = 0;
    for (let i = 0; i < n; i++) v = (v << 1) | this.readBit();
    return v;
  }

  /** Unsigned exp-Golomb (ITU-T H.262 §9.1). */
  readUe(): number {
    let zeros = 0;
    while (this.readBit() === 0 && zeros < 32) zeros++;
    if (zeros === 0) return 0;
    return ((1 << zeros) - 1) + this.readBits(zeros);
  }

  /** Signed exp-Golomb. */
  readSe(): number {
    const v = this.readUe();
    return v & 1 ? (v + 1) >> 1 : -(v >> 1);
  }
}

function skipScalingList(r: BitReader, size: number) {
  let lastScale = 8;
  let nextScale = 8;
  for (let j = 0; j < size; j++) {
    if (nextScale !== 0) {
      const delta = r.readSe();
      nextScale = (lastScale + delta + 256) % 256;
    }
    if (nextScale !== 0) lastScale = nextScale;
  }
}

/** SPS profiles that have chroma_format_idc and related extensions. */
function isHighProfile(profileIdc: number): boolean {
  return [100, 110, 122, 244, 44, 83, 86, 118, 128, 138, 139, 134].includes(profileIdc);
}

/**
 * Parse an H.264 SPS NAL unit to extract profile, level, and coded dimensions.
 * Implements ITU-T H.264 §7.3.2.1 up through frame_cropping.
 * `sps` includes the NAL header byte (e.g. 0x67).
 */
function parseSps(sps: Uint8Array): {
  profile: number;
  constraint: number;
  level: number;
  width: number;
  height: number;
} | null {
  if (sps.length < 4) return null;
  const profile = sps[1];
  const constraint = sps[2];
  const level = sps[3];

  const r = new BitReader(sps, 4);
  r.readUe(); // seq_parameter_set_id

  let chromaFormatIdc = 1; // default 4:2:0
  if (isHighProfile(profile)) {
    chromaFormatIdc = r.readUe();
    if (chromaFormatIdc === 3) r.readBit(); // separate_colour_plane_flag
    r.readUe(); // bit_depth_luma_minus8
    r.readUe(); // bit_depth_chroma_minus8
    r.readBit(); // qpprime_y_zero_transform_bypass_flag
    if (r.readBit()) { // seq_scaling_matrix_present_flag
      const n = chromaFormatIdc === 3 ? 12 : 8;
      for (let i = 0; i < n; i++) {
        if (r.readBit()) skipScalingList(r, i < 6 ? 16 : 64);
      }
    }
  }

  r.readUe(); // log2_max_frame_num_minus4
  const pocType = r.readUe(); // pic_order_cnt_type
  if (pocType === 0) {
    r.readUe(); // log2_max_pic_order_cnt_lsb_minus4
  } else if (pocType === 1) {
    r.readBit();  // delta_pic_order_always_zero_flag
    r.readSe();   // offset_for_non_ref_pic
    r.readSe();   // offset_for_top_to_bottom_field
    const n = r.readUe(); // num_ref_frames_in_pic_order_cnt_cycle
    for (let i = 0; i < n; i++) r.readSe();
  }

  r.readUe(); // max_num_ref_frames
  r.readBit(); // gaps_in_frame_num_value_allowed_flag
  const picWidthInMbsMinus1 = r.readUe();
  const picHeightInMapUnitsMinus1 = r.readUe();
  const frameMbsOnlyFlag = r.readBit();
  if (!frameMbsOnlyFlag) r.readBit(); // mb_adaptive_frame_field_flag
  r.readBit(); // direct_8x8_inference_flag

  let width = (picWidthInMbsMinus1 + 1) * 16;
  let height = (2 - frameMbsOnlyFlag) * (picHeightInMapUnitsMinus1 + 1) * 16;

  // Frame cropping
  if (r.readBit()) { // frame_cropping_flag
    const cropLeft = r.readUe();
    const cropRight = r.readUe();
    const cropTop = r.readUe();
    const cropBottom = r.readUe();
    // subWidthC/subHeightC table (H.264 Table 6-1)
    let subWidthC: number, subHeightC: number;
    if (chromaFormatIdc === 1)      { subWidthC = 2; subHeightC = 2; }
    else if (chromaFormatIdc === 2) { subWidthC = 2; subHeightC = 1; }
    else                             { subWidthC = 1; subHeightC = 1; }
    const cropUnitX = subWidthC;
    const cropUnitY = subHeightC * (2 - frameMbsOnlyFlag);
    width  -= (cropLeft + cropRight) * cropUnitX;
    height -= (cropTop + cropBottom) * cropUnitY;
  }

  return { profile, constraint, level, width, height };
}

/** Build an `avcC` box (AVCDecoderConfigurationRecord) from SPS+PPS. */
function buildAvcC(sps: Uint8Array, pps: Uint8Array): Uint8Array {
  if (sps.length < 4) throw new Error('SPS too short');
  const profile = sps[1];
  const constraint = sps[2];
  const level = sps[3];
  // avcC layout:
  //   1 byte: configurationVersion = 1
  //   1 byte: AVCProfileIndication (SPS[1])
  //   1 byte: profile_compatibility (SPS[2])
  //   1 byte: AVCLevelIndication (SPS[3])
  //   1 byte: 0xFC | lengthSizeMinusOne (0x03 → 4-byte length)
  //   1 byte: 0xE0 | numOfSequenceParameterSets (0x01)
  //   2 bytes: SPS length
  //   N bytes: SPS
  //   1 byte: numOfPictureParameterSets (0x01)
  //   2 bytes: PPS length
  //   M bytes: PPS
  const buf = new Uint8Array(11 + sps.length + pps.length);
  let p = 0;
  buf[p++] = 0x01;
  buf[p++] = profile;
  buf[p++] = constraint;
  buf[p++] = level;
  buf[p++] = 0xfc | 0x03; // 4-byte length prefix
  buf[p++] = 0xe0 | 0x01; // 1 SPS
  buf[p++] = (sps.length >> 8) & 0xff;
  buf[p++] = sps.length & 0xff;
  buf.set(sps, p); p += sps.length;
  buf[p++] = 0x01; // 1 PPS
  buf[p++] = (pps.length >> 8) & 0xff;
  buf[p++] = pps.length & 0xff;
  buf.set(pps, p);
  return buf;
}

/** Convert a list of NALUs into a single length-prefixed byte stream. */
function nalusToLengthPrefixed(nalus: NalUnit[]): Uint8Array {
  let total = 0;
  for (const n of nalus) total += 4 + n.data.length;
  const out = new Uint8Array(total);
  let p = 0;
  for (const n of nalus) {
    const len = n.data.length;
    out[p++] = (len >>> 24) & 0xff;
    out[p++] = (len >>> 16) & 0xff;
    out[p++] = (len >>> 8) & 0xff;
    out[p++] = len & 0xff;
    out.set(n.data, p);
    p += len;
  }
  return out;
}

/** Parse an HEVC SPS NAL unit for profile, level, and coded dimensions. */
function parseHevcSps(sps: Uint8Array): {
  profileSpace: number;
  tierFlag: number;
  profileIdc: number;
  compatFlags: Uint8Array;
  constraintFlags: Uint8Array;
  levelIdc: number;
  chromaFormatIdc: number;
  bitDepthLumaMinus8: number;
  bitDepthChromaMinus8: number;
  width: number;
  height: number;
} | null {
  if (sps.length < 15) return null;

  const profileSpace = (sps[3] >> 6) & 0x03;
  const tierFlag = (sps[3] >> 5) & 0x01;
  const profileIdc = sps[3] & 0x1f;
  const compatFlags = new Uint8Array(sps.subarray(4, 8));
  const constraintFlags = new Uint8Array(sps.subarray(8, 14));
  const levelIdc = sps[14];

  const r = new BitReader(sps, 2);
  r.readBits(4); // sps_video_parameter_set_id
  const maxSubLayersMinus1 = r.readBits(3);
  r.readBit();   // sps_temporal_id_nesting_flag
  // Skip profile_tier_level (2+1+5 + 32 + 48 + 8 = 96 bits)
  r.readBits(8);   // profile_space + tier + profile_idc
  r.readBits(32);  // compat flags
  for (let i = 0; i < 6; i++) r.readBits(8); // constraint flags (48 bits)
  r.readBits(8);   // level_idc

  // Sub-layer presence flags + data
  if (maxSubLayersMinus1 > 0) {
    const subProfile: boolean[] = [];
    const subLevel: boolean[] = [];
    for (let i = 0; i < maxSubLayersMinus1; i++) {
      subProfile[i] = !!r.readBit();
      subLevel[i] = !!r.readBit();
    }
    for (let i = maxSubLayersMinus1; i < 8; i++) r.readBits(2);
    for (let i = 0; i < maxSubLayersMinus1; i++) {
      if (subProfile[i]) {
        r.readBits(5);  // profile_idc
        r.readBits(32); // compat
        for (let j = 0; j < 6; j++) r.readBits(8); // constraint (48 bits)
      }
      if (subLevel[i]) r.readBits(8);
    }
  }

  r.readUe(); // sps_seq_parameter_set_id
  const chromaFormatIdc = r.readUe();
  if (chromaFormatIdc === 3) r.readBit(); // separate_colour_plane_flag
  const picWidth = r.readUe();
  const picHeight = r.readUe();

  let cropLeft = 0, cropRight = 0, cropTop = 0, cropBottom = 0;
  if (r.readBit()) { // conformance_window_flag
    cropLeft = r.readUe();
    cropRight = r.readUe();
    cropTop = r.readUe();
    cropBottom = r.readUe();
  }
  const bitDepthLumaMinus8 = r.readUe();
  const bitDepthChromaMinus8 = r.readUe();

  const subWidthC = (chromaFormatIdc === 1 || chromaFormatIdc === 2) ? 2 : 1;
  const subHeightC = (chromaFormatIdc === 1) ? 2 : 1;
  const width = picWidth - (cropLeft + cropRight) * subWidthC;
  const height = picHeight - (cropTop + cropBottom) * subHeightC;

  return {
    profileSpace, tierFlag, profileIdc, compatFlags, constraintFlags,
    levelIdc, chromaFormatIdc, bitDepthLumaMinus8, bitDepthChromaMinus8,
    width, height,
  };
}

/** Build an `hvcC` box (HEVCDecoderConfigurationRecord) from VPS+SPS+PPS. */
function buildHvcC(vps: Uint8Array, sps: Uint8Array, pps: Uint8Array): Uint8Array {
  const parsed = parseHevcSps(sps);

  const headerSize = 23;
  const totalSize = headerSize + 3 * 5 + vps.length + sps.length + pps.length;
  const buf = new Uint8Array(totalSize);
  let p = 0;

  buf[p++] = 0x01; // configurationVersion

  if (parsed) {
    buf[p++] = (parsed.profileSpace << 6) | (parsed.tierFlag << 5) | parsed.profileIdc;
    buf.set(parsed.compatFlags, p); p += 4;
    buf.set(parsed.constraintFlags, p); p += 6;
    buf[p++] = parsed.levelIdc;
  } else {
    buf[p++] = 0x01;
    buf.set([0x60, 0x00, 0x00, 0x00], p); p += 4;
    buf.set([0x90, 0x00, 0x00, 0x00, 0x00, 0x00], p); p += 6;
    buf[p++] = 0x00;
  }

  buf[p++] = 0xF0; buf[p++] = 0x00; // min_spatial_segmentation_idc = 0
  buf[p++] = 0xFC; // parallelismType = 0
  buf[p++] = 0xFC | ((parsed?.chromaFormatIdc ?? 1) & 0x03);
  buf[p++] = 0xF8 | ((parsed?.bitDepthLumaMinus8 ?? 0) & 0x07);
  buf[p++] = 0xF8 | ((parsed?.bitDepthChromaMinus8 ?? 0) & 0x07);
  buf[p++] = 0x00; buf[p++] = 0x00; // avgFrameRate = 0
  buf[p++] = 0x0F; // constantFrameRate=0, numTemporalLayers=1, temporalIdNested=1, lengthSizeMinusOne=3

  buf[p++] = 0x03; // numOfArrays (VPS, SPS, PPS)

  // VPS
  buf[p++] = 0xA0; // completeness=1, type=32
  buf[p++] = 0x00; buf[p++] = 0x01; // 1 NALU
  buf[p++] = (vps.length >> 8) & 0xff; buf[p++] = vps.length & 0xff;
  buf.set(vps, p); p += vps.length;

  // SPS
  buf[p++] = 0xA1; // completeness=1, type=33
  buf[p++] = 0x00; buf[p++] = 0x01;
  buf[p++] = (sps.length >> 8) & 0xff; buf[p++] = sps.length & 0xff;
  buf.set(sps, p); p += sps.length;

  // PPS
  buf[p++] = 0xA2; // completeness=1, type=34
  buf[p++] = 0x00; buf[p++] = 0x01;
  buf[p++] = (pps.length >> 8) & 0xff; buf[p++] = pps.length & 0xff;
  buf.set(pps, p); p += pps.length;

  return buf;
}

/** Build a WebCodecs codec string for HEVC (e.g. "hev1.1.6.L120.B0"). */
function buildHevcCodecString(parsed: NonNullable<ReturnType<typeof parseHevcSps>>): string {
  const profileStr = parsed.profileSpace === 0
    ? String(parsed.profileIdc)
    : `${['e', 'm', 'x'][parsed.profileSpace - 1]}${parsed.profileIdc}`;
  let compatHex = '';
  for (const b of parsed.compatFlags) compatHex += b.toString(16).padStart(2, '0');
  compatHex = compatHex.replace(/^0+/, '') || '0';
  const cb = parsed.constraintFlags[0];
  const constraintStr = cb > 0 ? `.B${cb.toString(16).padStart(2, '0')}` : '';
  return `hev1.${profileStr}.${compatHex}.L${parsed.levelIdc}${constraintStr}`;
}

function bytesEqual(a: Uint8Array | null, b: Uint8Array | null): boolean {
  if (a === b) return true;
  if (!a || !b || a.length !== b.length) return false;
  for (let i = 0; i < a.length; i++) {
    if (a[i] !== b[i]) return false;
  }
  return true;
}

// ---------------------------------------------------------------------------
// AV1 OBU parsing (low-overhead bitstream format as emitted by libsvtav1/
// libaom/WebCodecs VideoEncoder). PES payload = concatenated length-delimited
// OBUs. A keyframe access unit typically begins TD(2) → SH(1) → Frame(6).

const OBU_SEQUENCE_HEADER = 1;
const OBU_TEMPORAL_DELIMITER = 2;
const OBU_FRAME_HEADER = 3;
const OBU_TILE_GROUP = 4;
const OBU_METADATA = 5;
const OBU_FRAME = 6;

interface Av1Obu {
  type: number;
  /** Full raw OBU bytes (header + optional ext + LEB128 size + payload). */
  raw: Uint8Array;
  /** OBU payload (after header + optional ext + LEB128 size). */
  data: Uint8Array;
}

/** Parse a low-overhead AV1 OBU stream into individual OBUs. */
function parseObus(payload: Uint8Array): Av1Obu[] {
  const out: Av1Obu[] = [];
  let p = 0;
  while (p < payload.length) {
    const start = p;
    const b = payload[p];
    if ((b & 0x80) !== 0) break; // forbidden_bit set → malformed
    const type = (b >> 3) & 0x0f;
    const extFlag = (b >> 2) & 0x01;
    const hasSize = (b >> 1) & 0x01;
    let q = p + 1 + extFlag;
    let size: number;
    if (hasSize) {
      size = 0;
      let shift = 0;
      while (q < payload.length) {
        const byte = payload[q++];
        size |= (byte & 0x7f) << shift;
        if ((byte & 0x80) === 0) break;
        shift += 7;
        if (shift > 35) break;
      }
    } else {
      size = payload.length - q;
    }
    const dataEnd = Math.min(payload.length, q + size);
    out.push({
      type,
      raw: payload.subarray(start, dataEnd),
      data: payload.subarray(q, dataEnd),
    });
    if (!hasSize) break;
    p = dataEnd;
  }
  return out;
}

interface Av1SeqInfo {
  profile: number;
  levelIdx: number;
  tier: number;
  bitDepth: number;
  width: number;
  height: number;
  monoChrome: number;
}

/**
 * Parse an AV1 Sequence Header OBU payload (the bytes AFTER the OBU header +
 * LEB128 size) for the fields needed to build a codec string + decoder config.
 * Implements AV1 spec §5.5.2 up through color_config(). Best-effort for the
 * rare timing_info/decoder_model paths; returns null on desync so the caller
 * can fall back to a generic codec string.
 */
function parseAv1SeqHeader(sh: Uint8Array): Av1SeqInfo | null {
  if (sh.length === 0) return null;
  const r = new BitReader(sh, 0);
  const profile = r.readBits(3);
  r.readBit(); // still_picture
  const reduced = r.readBit();
  let levelIdx = 0;
  let tier = 0;
  if (reduced) {
    levelIdx = r.readBits(5);
  } else {
    const timingPresent = r.readBit();
    if (timingPresent) {
      r.readBits(32); // num_units_in_display_tick
      r.readBits(32); // time_scale
      if (r.readBit()) r.readUe(); // equal_picture_interval → delta_frame_id
      const decoderModelPresent = r.readBit();
      if (decoderModelPresent) {
        // decoder_model_info: 5 + 32 + 5 + 5 bits
        r.readBits(5);
        r.readBits(32);
        r.readBits(5);
        r.readBits(5);
      }
    }
    const initialDisplayPresent = r.readBit();
    const opCntMinus1 = r.readBits(5);
    for (let i = 0; i <= opCntMinus1; i++) {
      r.readBits(12); // operating_point_idc
      const lvl = r.readBits(5);
      if (i === 0) levelIdx = lvl;
      const t = lvl > 7 ? r.readBit() : 0;
      if (i === 0) tier = t;
      // NOTE: operating_parameters_info (per-op decoder model) is not parsed;
      // live encoders (libsvtav1/libaom) don't emit it. If present and skipped,
      // the parse desyncs and later fields are wrong → caller falls back.
      if (initialDisplayPresent) {
        if (r.readBit()) r.readBits(4); // initial_display_delay
      }
    }
  }
  const widthBits = r.readBits(4);
  const heightBits = r.readBits(4);
  const width = r.readBits(widthBits + 1) + 1;
  const height = r.readBits(heightBits + 1) + 1;
  if (!reduced) r.readBit(); // frame_id_numbers_present_flag
  r.readBit(); // use_128x128_superblock
  r.readBit(); // enable_filter_intra
  r.readBit(); // enable_intra_edge_filter
  if (!reduced) {
    r.readBit(); // enable_interintra_compound
    r.readBit(); // enable_masked_compound
    r.readBit(); // enable_warped_motion
    r.readBit(); // enable_dual_filter
    const enableOrderHint = r.readBit();
    if (enableOrderHint) {
      r.readBit(); // enable_jnt_comp
      r.readBit(); // enable_ref_frame_mvs
    }
    const chooseScreen = r.readBit();
    let forceScreen = 2; // SELECT_SCREEN_CONTENT_TOOLS
    if (!chooseScreen) forceScreen = r.readBit();
    if (forceScreen > 0) {
      const chooseMv = r.readBit();
      if (!chooseMv) r.readBit();
    }
    if (enableOrderHint) r.readBits(3); // order_hint_bits_minus_1
  }
  r.readBit(); // enable_superres
  r.readBit(); // enable_cdef
  r.readBit(); // enable_restoration
  const highBitdepth = r.readBit();
  let twelveBit = 0;
  if (profile === 2 && highBitdepth) twelveBit = r.readBit();
  const bitDepth = twelveBit ? 12 : highBitdepth ? 10 : 8;
  let monoChrome = 0;
  if (profile !== 1) monoChrome = r.readBit();
  return { profile, levelIdx, tier, bitDepth, width, height, monoChrome };
}

/**
 * Build a WebCodecs AV1 codec string: `av01.<profile>.<LL><tier>.<DD>`.
 *
 * NOTE: the level (seq_level_idx) and bit-depth fields are 2-digit DECIMAL,
 * not hex. Hex-encoding values >= 10 (e.g. level 12 -> "0c", 10-bit -> "0a")
 * produces strings Chrome rejects as "Unknown or ambiguous codec name",
 * leaving the decoder in a configure -> error loop with no decoded frames.
 */
function buildAv1CodecString(info: Av1SeqInfo): string {
  const ll = info.levelIdx.toString().padStart(2, '0');
  const tierCh = info.tier === 0 ? 'M' : 'H';
  const dd = info.bitDepth.toString().padStart(2, '0');
  return `av01.${info.profile}.${ll}${tierCh}.${dd}`;
}

type VideoCodec = 'h264' | 'hevc' | 'av1';

export class VideoPipeline {
  private decoder: VideoDecoder | null = null;
  private codec: VideoCodec | null = null;
  private codecHint: VideoCodec | null = null;
  private sps: Uint8Array | null = null;
  private pps: Uint8Array | null = null;
  private vps: Uint8Array | null = null; // HEVC only
  private av1ShRaw: Uint8Array | null = null; // AV1: raw Sequence Header OBU
  private av1ShInfo: Av1SeqInfo | null = null; // AV1: parsed SH fields
  private cb: DecoderCallbacks;
  // Pending NALU batches seen between SPS/PPS arrival and decoder.configure().
  // Each batch retains its original PTS so flushed chunks don't get stamped
  // with the wrong timestamp.
  private pendingNalus: { nalus: NalUnit[]; pts: number | null; isKeyframe: boolean }[] = [];
  // Pending AV1 access units seen between SH capture and decoder.configure().
  private pendingAv1: { payload: Uint8Array; pts: number | null; isKey: boolean }[] = [];
  private static readonly PENDING_CAP = 30;
  private configured = false;
  private configuring = false;
  private decodedCount = 0;
  private seenKeyframe = false;
  private droppedVideo = 0;
  private reconfigureCount = 0;
  // Hardware-acceleration preference. Exposed via setHwMode() so the debug
  // panel can A/B test hardware vs software decode paths without reconnecting.
  private hwMode: 'prefer-hardware' | 'prefer-software' = 'prefer-hardware';
  private lastCodecString: string | null = null;
  private lastHwAccel: string | undefined = undefined;
  private lastProfile = 0;
  private lastLevel = 0;
  private lastWidth = 0;
  private lastHeight = 0;

  constructor(cb: DecoderCallbacks) {
    this.cb = cb;
  }

  /**
   * Set the expected video codec from the PMT (viewer-side routing decision).
   * When set to 'av1', feed() routes payloads to the OBU path instead of the
   * Annex-B NAL path. Must be called before the first videoPes arrives (PMT
   * always precedes PES). Changing the hint resets any in-progress decode.
   */
  setCodecHint(codec: VideoCodec | null) {
    if (this.codecHint === codec) return;
    if (codec !== null && this.codec !== null && this.codec !== codec) {
      this.reset();
    }
    this.codecHint = codec;
  }

  /**
   * Switch the hardware-acceleration preference. Takes effect on the next
   * configure() — kept SPS/PPS/VPS/SH mean that happens immediately on the
   * next feed() rather than waiting for a fresh keyframe. Callers should
   * follow up with resetDecoder() if they want a clean slate instead.
   */
  setHwMode(mode: 'prefer-hardware' | 'prefer-software') {
    if (this.hwMode === mode) return;
    this.hwMode = mode;
    this.configured = false;
    this.seenKeyframe = false;
    if (this.decoder) {
      try { this.decoder.close(); } catch {}
      this.decoder = null;
    }
  }

  /** Feed a PES payload from the demuxer (Annex-B byte stream, or AV1 OBUs). */
  feed(payload: Uint8Array, pts: number | null, isKeyframe: boolean) {
    // AV1 path: low-overhead OBU bitstream, not Annex B. Honor an explicit
    // PMT hint, or a previously-pinned av1 detection.
    if (this.codecHint === 'av1' || this.codec === 'av1') {
      this.feedAv1(payload, pts, isKeyframe);
      return;
    }

    const nalus = parseAnnexB(payload);

    // Auto-detect codec on first recognizable NAL.
    if (this.codec === null) {
      for (const n of nalus) {
        if (n.hevcType === HEVC_VPS || n.hevcType === HEVC_SPS || n.hevcType === HEVC_PPS) {
          this.codec = 'hevc';
          break;
        }
        if (n.type === NAL_SPS || n.type === NAL_PPS || n.type === NAL_IDR) {
          this.codec = 'h264';
          break;
        }
      }
      if (this.codec === null) {
        // No NAL recognized — try AV1 OBU sniff (fallback when no PMT hint).
        if (this.sniffAv1(payload)) {
          this.codec = 'av1';
          this.feedAv1(payload, pts, isKeyframe);
        }
        return;
      }
    }

    const isHevc = this.codec === 'hevc';
    const nt = (n: NalUnit) => isHevc ? n.hevcType : n.type;

    // Collect parameter sets. HEVC needs VPS+SPS+PPS; H.264 needs SPS+PPS.
    for (const n of nalus) {
      if (isHevc && nt(n) === HEVC_VPS) {
        if (!bytesEqual(this.vps, n.data)) { this.vps = n.data; this.configured = false; }
      } else if (nt(n) === (isHevc ? HEVC_SPS : NAL_SPS)) {
        if (!bytesEqual(this.sps, n.data)) { this.sps = n.data; this.configured = false; }
      } else if (nt(n) === (isHevc ? HEVC_PPS : NAL_PPS)) {
        if (!bytesEqual(this.pps, n.data)) { this.pps = n.data; this.configured = false; }
      }
    }

    if (!this.configured) {
      const ready = isHevc ? (this.vps && this.sps && this.pps) : (this.sps && this.pps);
      if (ready) this.configure();
      this.pendingNalus.push({ nalus, pts, isKeyframe });
      if (this.pendingNalus.length > VideoPipeline.PENDING_CAP) {
        this.pendingNalus.splice(0, this.pendingNalus.length - VideoPipeline.PENDING_CAP);
      }
      return;
    }

    if (!this.decoder || this.decoder.state !== 'configured') {
      this.pendingNalus.push({ nalus, pts, isKeyframe });
      if (this.pendingNalus.length > VideoPipeline.PENDING_CAP) {
        this.pendingNalus.splice(0, this.pendingNalus.length - VideoPipeline.PENDING_CAP);
      }
      return;
    }

    // Flush pending batches with their original PTS, then the current batch.
    const batches = this.pendingNalus;
    this.pendingNalus = [];
    for (const b of batches) {
      this.emitAu(b.nalus, b.pts, b.isKeyframe);
    }
    this.emitAu(nalus, pts, isKeyframe);
  }

  private emitAu(nalus: NalUnit[], pts: number | null, _hint: boolean) {
    if (!this.decoder || this.decoder.state !== 'configured') return;
    const isHevc = this.codec === 'hevc';
    const nt = (n: NalUnit) => isHevc ? n.hevcType : n.type;
    const isKey = (n: NalUnit) => isHevc
      ? (nt(n) === HEVC_IDR_W_RADL || nt(n) === HEVC_IDR_N_LP)
      : (nt(n) === NAL_IDR);
    const hasIdr = nalus.some(isKey);
    const decodeNalus = isHevc
      ? nalus.filter((n) => nt(n) < 32)
      : nalus.filter((n) => nt(n) >= 1 && nt(n) <= 5);
    if (decodeNalus.length === 0) return;
    if (!this.seenKeyframe && !hasIdr) return;
    if (hasIdr) this.seenKeyframe = true;
    const qDepth = this.decoder.decodeQueueSize;
    if (!hasIdr && qDepth > 8) {
      this.droppedVideo++;
      if (this.droppedVideo % 30 === 0) {
        console.debug(`VideoPipeline: dropped ${this.droppedVideo} frames (queue full, qDepth=${qDepth})`);
      }
      return;
    }
    const data = nalusToLengthPrefixed(decodeNalus);
    // PTS from the demuxer is in 90 kHz units. Convert to microseconds:
    //   1 unit @ 90 kHz = 1_000_000 / 90_000 µs = 100/9 µs ≈ 11.111 µs.
    // The previous `pts / 90` produced milliseconds (off by 1000×), which
    // confused WebCodecs's internal reference-frame / output ordering.
    const tsUs = pts != null ? Math.floor((pts * 100) / 9) : undefined;
    const chunk = new EncodedVideoChunk({
      type: hasIdr ? 'key' : 'delta',
      timestamp: tsUs ?? 0,
      data,
    });
    try {
      this.decoder.decode(chunk);
    } catch (e) {
      this.cb.onError(e);
      this.reset();
    }
  }

  private configure() {
    if (this.codec === 'hevc') {
      this.configureHevc();
    } else if (this.codec === 'av1') {
      this.configureAv1();
    } else {
      this.configureAvc();
    }
  }

  /** Quick structural sniff: does this payload begin with a valid AV1 OBU? */
  private sniffAv1(payload: Uint8Array): boolean {
    if (payload.length < 2) return false;
    const b = payload[0];
    if ((b & 0x80) !== 0) return false; // forbidden
    if ((b & 0x01) !== 0) return false; // reserved
    const type = (b >> 3) & 0x0f;
    if (type !== OBU_SEQUENCE_HEADER && type !== OBU_TEMPORAL_DELIMITER && type !== OBU_FRAME) return false;
    return ((b >> 1) & 0x01) === 1; // has_size (low-overhead format)
  }

  /** AV1 feed: parse OBUs, (re)configure on Sequence Header, emit access units. */
  private feedAv1(payload: Uint8Array, pts: number | null, isKey: boolean) {
    if (this.codec === null) this.codec = 'av1';
    const obus = parseObus(payload);
    // Capture Sequence Header. SH rides inside keyframes; refresh config when
    // it changes (new spatial/SNR layer, resolution change, etc.).
    for (const o of obus) {
      if (o.type === OBU_SEQUENCE_HEADER) {
        if (!bytesEqual(this.av1ShRaw, o.raw)) {
          this.av1ShRaw = o.raw;
          this.av1ShInfo = parseAv1SeqHeader(o.data);
          this.configured = false;
        }
      }
    }

    if (!this.configured) {
      if (this.av1ShRaw && !this.configuring) this.configureAv1();
      this.pendingAv1.push({ payload, pts, isKey });
      if (this.pendingAv1.length > VideoPipeline.PENDING_CAP) {
        this.pendingAv1.splice(0, this.pendingAv1.length - VideoPipeline.PENDING_CAP);
      }
      return;
    }

    if (!this.decoder || this.decoder.state !== 'configured') {
      this.pendingAv1.push({ payload, pts, isKey });
      if (this.pendingAv1.length > VideoPipeline.PENDING_CAP) {
        this.pendingAv1.splice(0, this.pendingAv1.length - VideoPipeline.PENDING_CAP);
      }
      return;
    }

    // Flush pending access units with their original PTS, then the current one.
    const batches = this.pendingAv1;
    this.pendingAv1 = [];
    for (const b of batches) this.emitAv1(b.payload, b.pts, b.isKey);
    this.emitAv1(payload, pts, isKey);
  }

  private emitAv1(payload: Uint8Array, pts: number | null, isKey: boolean) {
    if (!this.decoder || this.decoder.state !== 'configured') return;
    if (!this.seenKeyframe && !isKey) return;
    if (isKey) this.seenKeyframe = true;
    const qDepth = this.decoder.decodeQueueSize;
    if (!isKey && qDepth > 8) {
      this.droppedVideo++;
      if (this.droppedVideo % 30 === 0) {
        console.debug(`VideoPipeline: dropped ${this.droppedVideo} AV1 frames (queue full, qDepth=${qDepth})`);
      }
      return;
    }
    // PTS from the demuxer is in 90 kHz units — convert to µs (100/9 per unit).
    // See emitAu for the rationale; the old `pts / 90` was off by 1000×.
    const tsUs = pts != null ? Math.floor((pts * 100) / 9) : undefined;
    const chunk = new EncodedVideoChunk({
      type: isKey ? 'key' : 'delta',
      timestamp: tsUs ?? 0,
      data: payload,
    });
    try {
      this.decoder.decode(chunk);
    } catch (e) {
      this.cb.onError(e);
      this.reset();
    }
  }

  private async configureAv1() {
    if (!this.av1ShRaw || this.configuring) return;
    this.configuring = true;
    this.reconfigureCount++;
    try {
      const info = this.av1ShInfo;
      // Chrome accepts the raw Sequence Header OBU (header + size + payload) as
      // the decoder `description`; it parses bitdepth/chroma/subsampling from it.
      const description = this.av1ShRaw;

      // Validate the parsed SH: AV1 profiles are 0/1/2, bitdepths 8/10/12. If the
      // parse desynced (e.g. a misaligned OBU captured as type-1, or an SH with
      // features we don't fully parse), don't trust the extracted fields — pass
      // only the raw `description` + a generic codec string and let Chrome parse
      // the authoritative values from the SH OBU itself.
      const infoValid = info !== null
        && (info.profile === 0 || info.profile === 1 || info.profile === 2)
        && (info.bitDepth === 8 || info.bitDepth === 10 || info.bitDepth === 12)
        && info.width > 0 && info.height > 0;
      const preferred = infoValid ? buildAv1CodecString(info!) : null;
      const codedWidth = infoValid ? info!.width : undefined;
      const codedHeight = infoValid ? info!.height : undefined;

      if (this.decoder) {
        try { this.decoder.close(); } catch {}
      }
      this.decoder = new VideoDecoder({
        output: (frame) => {
          this.decodedCount++;
          this.cb.onFrame(frame);
        },
        // On an async decode error Chrome closes the decoder. Drop configured
        // so the next keyframe (which carries a fresh SH) reconfigures a new
        // decoder instead of feeding chunks into a dead one forever.
        error: (e) => {
          this.configured = false;
          this.seenKeyframe = false;
          this.cb.onError(e);
        },
      });

      // MUST validate via isConfigSupported: VideoDecoder.configure() does NOT
      // throw synchronously for an unsupported codec — it resolves and then
      // fails later via the error callback. A sync try/catch therefore cannot
      // detect rejection, which is how the decoder previously got stuck looping
      // on a bogus codec string. Probe each candidate; first supported wins.
      // The raw SH `description` carries the authoritative config, so even a
      // generic fallback string decodes the real bitstream correctly.
      const candidates: string[] = [];
      if (preferred) candidates.push(preferred);
      for (const fb of ['av01.0.08M.08', 'av01.0.08M.10']) {
        if (!candidates.includes(fb)) candidates.push(fb);
      }

      const buildCfg = (codecStr: string) => ({
        codec: codecStr,
        description,
        codedWidth,
        codedHeight,
        hardwareAcceleration: this.hwMode,
      } as VideoDecoderConfig);

      let chosen: string | null = null;
      const tried: string[] = [];
      for (const c of candidates) {
        try {
          const r = await VideoDecoder.isConfigSupported(buildCfg(c));
          const ok = !!(r && r.supported);
          tried.push(`${c}->${ok}`);
          if (ok) { chosen = c; break; }
        } catch {
          tried.push(`${c}->err`);
        }
      }
      if (!chosen) {
        console.warn(`VideoPipeline AV1: no supported codec string (tried ${tried.join(', ')})`);
        this.cb.onError(new Error(`AV1 VideoDecoder not supported (tried ${tried.join(', ')})`));
        this.decoder = null;
        return;
      }

      this.decoder.configure(buildCfg(chosen));
      this.configured = true;
      this.seenKeyframe = false;
      this.lastCodecString = chosen;
      this.lastHwAccel = this.hwMode;
      this.lastProfile = infoValid ? info!.profile : 0;
      this.lastLevel = infoValid ? info!.levelIdx : 0;
      this.lastWidth = infoValid ? info!.width : 0;
      this.lastHeight = infoValid ? info!.height : 0;
      this.cb.onConfigured({
        width: infoValid ? info!.width : 0,
        height: infoValid ? info!.height : 0,
        profile: infoValid ? info!.profile : 0,
        level: infoValid ? info!.levelIdx : 0,
      });
    } finally {
      this.configuring = false;
    }
  }

  private configureAvc() {
    if (!this.sps || !this.pps) return;
    this.reconfigureCount++;
    const info = parseSps(this.sps);
    if (!info) return;
    const avcc = buildAvcC(this.sps, this.pps);

    // Reset if previously configured.
    if (this.decoder) {
      try {
        this.decoder.close();
      } catch {
        // ignore
      }
    }
    this.decoder = new VideoDecoder({
      output: (frame) => {
        this.decodedCount++;
        this.cb.onFrame(frame);
      },
      error: (e) => this.cb.onError(e),
    });

    try {
      const codecStr = 'avc1.' + toHex(info.profile) + toHex(info.constraint) + toHex(info.level);
      this.decoder.configure({
        codec: codecStr,
        description: avcc,
        codedWidth: info.width || undefined,
        codedHeight: info.height || undefined,
        hardwareAcceleration: this.hwMode,
      } as VideoDecoderConfig);
      this.configured = true;
      this.seenKeyframe = false;
      this.lastCodecString = codecStr;
      this.lastHwAccel = this.hwMode;
      this.lastProfile = info.profile;
      this.lastLevel = info.level;
      this.lastWidth = info.width;
      this.lastHeight = info.height;
      this.cb.onConfigured({
        width: info.width,
        height: info.height,
        profile: info.profile,
        level: info.level,
      });
    } catch (e) {
      this.cb.onError(e);
      this.decoder = null;
    }
  }

  private configureHevc() {
    if (!this.vps || !this.sps || !this.pps) return;
    this.reconfigureCount++;
    const info = parseHevcSps(this.sps);
    const hvcc = buildHvcC(this.vps, this.sps, this.pps);

    if (this.decoder) {
      try { this.decoder.close(); } catch {}
    }
    this.decoder = new VideoDecoder({
      output: (frame) => {
        this.decodedCount++;
        this.cb.onFrame(frame);
      },
      error: (e) => this.cb.onError(e),
    });

    const codec = info ? buildHevcCodecString(info) : 'hev1.1.6.L0';
    try {
      this.decoder.configure({
        codec,
        description: hvcc,
        hardwareAcceleration: this.hwMode,
      } as VideoDecoderConfig);
      this.configured = true;
      this.seenKeyframe = false;
      this.lastCodecString = codec;
      this.lastHwAccel = this.hwMode;
      this.lastProfile = info?.profileIdc ?? 0;
      this.lastLevel = info?.levelIdc ?? 0;
      this.lastWidth = info?.width ?? 0;
      this.lastHeight = info?.height ?? 0;
      this.cb.onConfigured({
        width: info?.width ?? 0,
        height: info?.height ?? 0,
        profile: info?.profileIdc ?? 0,
        level: info?.levelIdc ?? 0,
      });
    } catch (e) {
      this.cb.onError(e);
      this.decoder = null;
    }
  }

  getStats(): import('./debug/types').VideoStats {
    return {
      codec: this.codec,
      codecString: this.lastCodecString ?? null,
      decoderState: this.decoder?.state ?? 'unconfigured',
      decodeQueueSize: this.decoder?.decodeQueueSize ?? 0,
      decodedCount: this.decodedCount,
      droppedFrames: this.droppedVideo,
      hwAcceleration: this.lastHwAccel,
      hwModePreference: this.hwMode,
      reconfigureCount: this.reconfigureCount,
      profile: this.lastProfile ?? 0,
      level: this.lastLevel ?? 0,
      codedWidth: this.lastWidth ?? 0,
      codedHeight: this.lastHeight ?? 0,
    };
  }

  reset() {
    this.configured = false;
    this.configuring = false;
    this.codec = null;
    this.sps = null;
    this.pps = null;
    this.vps = null;
    this.av1ShRaw = null;
    this.av1ShInfo = null;
    this.pendingNalus = [];
    this.pendingAv1 = [];
    if (this.decoder) {
      try { this.decoder.close(); } catch {}
      this.decoder = null;
    }
  }
}

function toHex(n: number): string {
  return n.toString(16).padStart(2, '0');
}

// Suppress unused-export lint for these constants; kept for completeness.
export const NAL_TYPES = {
  UNSPECIFIED: NAL_UNSPECIFIED,
  SLICE: NAL_SLICE,
  IDR: NAL_IDR,
  SEI: NAL_SEI,
  SPS: NAL_SPS,
  PPS: NAL_PPS,
  AUD: NAL_AUD,
};

// ---------------------------------------------------------------------------
// Audio pipelines for MPEG-TS.
//
// Two codecs:
//   - Opus (stream type 0x06): 2-byte control header prefix per PES, then
//     one Opus packet. Detected via TOC byte stereo flag.
//   - AAC/ADTS (stream type 0x0F): 7-byte ADTS header per PES, then raw AAC
//     frame. Profile/sampleRate/channels extracted from ADTS.
//
// `MediaStreamTrackGenerator` (Chrome ≥94) routes to `<audio>`.

export interface AudioDecoderCallbacks {
  onError: (e: unknown) => void;
  onReady: () => void;
}

/** Opus TOC byte frame durations in microseconds, indexed by config (0-31). */
const OPUS_FRAME_DURATIONS_US = [
  10000, 20000, 40000, 60000,  // 0-3:  SILK NB
  10000, 20000, 40000, 60000,  // 4-7:  SILK MB
  10000, 20000, 40000, 60000,  // 8-11: SILK WB
  10000, 10000, 20000, 20000,  // 12-15: Hybrid SWB/FB
  2500, 5000, 10000, 20000,    // 16-19: CELT NB
  10000, 20000, 40000, 60000,  // 20-23: CELT WB
  10000, 20000, 40000, 60000,  // 24-27: CELT SWB
  10000, 20000, 40000, 60000,  // 28-31: CELT FB
];

function parseOpusToc(toc: number): { code: number; frameDurationUs: number } {
  const config = (toc >> 3) & 0x1f;
  const code = toc & 0x03;
  return { code, frameDurationUs: OPUS_FRAME_DURATIONS_US[config] ?? 20000 };
}

/** ADTS sampling rate table (index → Hz). */
const ADTS_SAMPLE_RATES = [
  96000, 88200, 64000, 48000, 44100, 32000, 24000, 22050,
  16000, 12000, 11025, 8000, 7350,
];

/** Shared base: owns the output path (MediaStreamTrackGenerator or AudioWorklet). */
abstract class AudioPipelineBase {
  protected decoder: AudioDecoder | null = null;
  protected generator: MediaStreamTrackGenerator | null = null;
  protected writer: WritableStreamDefaultWriter<AudioData> | null = null;
  protected audioCtx: AudioContext | null = null;
  protected workletNode: AudioWorkletNode | null = null;
  protected cb: AudioDecoderCallbacks;
  protected configured = false;
  protected outputPending = false;
  protected packetsDecoded = 0;
  protected sampleRate = 48000;
  protected channels = 2;
  protected droppedAudio = 0;
  protected lastWrittenPtsUs: number | null = null;
  protected lastWrittenWallMs = 0;
  protected codecString: string | null = null;

  constructor(cb: AudioDecoderCallbacks) {
    this.cb = cb;
  }

  get track(): MediaStreamTrack | null {
    return this.generator;
  }

  /** Resume AudioContext after user gesture (worklet path only). */
  async resume(): Promise<void> {
    if (this.audioCtx?.state === 'suspended') {
      await this.audioCtx.resume();
    }
  }

  abstract feed(payload: Uint8Array, pts: number | null): void;

  /** Set up the output path. Sync for MediaStreamTrackGenerator, async for AudioWorklet. */
  protected async initOutput(): Promise<void> {
    const MTG = (window as unknown as {
      MediaStreamTrackGenerator?: new (init: { kind: string }) => MediaStreamTrackGenerator;
    }).MediaStreamTrackGenerator;

    if (MTG) {
      this.generator = new MTG({ kind: 'audio' });
      this.writer = this.generator.writable?.getWriter() ?? null;
    } else {
      await this.initWorklet();
    }
    this.configured = true;
    this.cb.onReady();
  }

  /** AudioWorklet fallback for Firefox and other browsers without MediaStreamTrackGenerator. */
  private async initWorklet(): Promise<void> {
    const Ctx = (window.AudioContext || (window as unknown as Window & { webkitAudioContext: typeof AudioContext }).webkitAudioContext);
    this.audioCtx = new Ctx({ sampleRate: this.sampleRate });

    const blob = new Blob([PCM_PLAYER_WORKLET], { type: 'application/javascript' });
    await this.audioCtx.audioWorklet.addModule(URL.createObjectURL(blob));

    this.workletNode = new AudioWorkletNode(this.audioCtx!, 'pcm-player', {
      numberOfInputs: 0,
      numberOfOutputs: 1,
      outputChannelCount: [this.channels],
    });
    this.workletNode.connect(this.audioCtx!.destination);
  }

  /** Route a decoded AudioData frame to whichever output path is active. */
  protected routeFrame(frame: AudioData) {
    this.lastWrittenPtsUs = frame.timestamp;
    this.lastWrittenWallMs = performance.now();
    if (this.writer) {
      this.writer.write(frame).catch(() => frame.close());
    } else if (this.workletNode) {
      const planes: Float32Array[] = [];
      for (let ch = 0; ch < frame.numberOfChannels; ch++) {
        const buf = new Float32Array(frame.numberOfFrames);
        frame.copyTo(buf, { planeIndex: ch });
        planes.push(buf);
      }
      this.workletNode.port.postMessage({ planes });
      frame.close();
    } else {
      frame.close();
    }
  }

  protected feedFrame(chunk: EncodedAudioChunk) {
    if (this.decoder && this.decoder.decodeQueueSize > 20) {
      this.droppedAudio++;
      if (this.droppedAudio % 30 === 0) {
        console.debug(`${this.constructor.name}: dropped ${this.droppedAudio} packets (queue full)`);
      }
      return;
    }
    try {
      this.decoder?.decode(chunk);
      this.packetsDecoded++;
    } catch (e) {
      this.cb.onError(e);
    }
  }

  /** Start async output init. Guards against re-entry. */
  protected startInit() {
    this.outputPending = true;
    this.configured = false;
    this.initOutput()
      .catch((e) => this.cb.onError(e))
      .finally(() => { this.outputPending = false; });
  }

  /** Gate for feed(): returns true if packets should be processed now. */
  protected canFeed(channels: number): boolean {
    if (this.outputPending) return false;
    if (!this.configured) return false;
    if (this.decoder && this.channels !== channels) return false;
    return true;
  }

  /** Estimated audio playhead PTS in microseconds, or null if no audio written yet. */
  audioPlayheadUs(): number | null {
    if (this.lastWrittenPtsUs === null) return null;
    // Audio hardware consumes samples at real-time rate from the write point.
    // This approximation assumes the hardware clock runs at exactly real-time.
    return this.lastWrittenPtsUs + (performance.now() - this.lastWrittenWallMs) * 1000;
  }

  getStats(): import('./debug/types').AudioStats {
    const MTG = (typeof window !== 'undefined' && (window as any).MediaStreamTrackGenerator);
    return {
      codec: this.codecString ?? null,
      decoderState: this.decoder?.state ?? 'unconfigured',
      decodeQueueSize: this.decoder?.decodeQueueSize ?? 0,
      packetsDecoded: this.packetsDecoded,
      droppedPackets: this.droppedAudio,
      sampleRate: this.sampleRate,
      channels: this.channels,
      outputMode: this.generator ? 'MediaStreamTrackGenerator' : (this.workletNode ? 'AudioWorklet' : null),
    };
  }

  reset() {
    this.configured = false;
    this.outputPending = false;
    if (this.decoder) { try { this.decoder.close(); } catch {} this.decoder = null; }
    if (this.writer) { try { this.writer.close(); } catch {} this.writer = null; }
    if (this.workletNode) { try { this.workletNode.disconnect(); } catch {} this.workletNode = null; }
    if (this.audioCtx) { try { this.audioCtx.close(); } catch {} this.audioCtx = null; }
    this.generator = null;
  }
}

/** AudioWorklet processor: receives Float32 planes via port, plays them out. */
const PCM_PLAYER_WORKLET = `
class PcmPlayerProcessor extends AudioWorkletProcessor {
  constructor() {
    super();
    this.queues = [];
    this.heads = [];
    this.tails = [];
    this.counts = [];
    this.CAP = 24000;
    this.port.onmessage = (e) => {
      const planes = e.data.planes;
      for (let ch = 0; ch < planes.length; ch++) {
        if (!this.queues[ch]) {
          this.queues[ch] = new Float32Array(this.CAP);
          this.heads[ch] = 0;
          this.tails[ch] = 0;
          this.counts[ch] = 0;
        }
        const incoming = planes[ch];
        const q = this.queues[ch];
        let tail = this.tails[ch];
        let count = this.counts[ch];
        for (let i = 0; i < incoming.length; i++) {
          if (count >= this.CAP) {
            this.heads[ch] = (this.heads[ch] + 1) % this.CAP;
            count--;
          }
          q[tail] = incoming[i];
          tail = (tail + 1) % this.CAP;
          count++;
        }
        this.tails[ch] = tail;
        this.counts[ch] = count;
      }
    };
  }
  process(inputs, outputs) {
    const output = outputs[0];
    const framesNeeded = output[0].length;
    for (let ch = 0; ch < output.length; ch++) {
      if (!this.queues[ch]) {
        for (let i = 0; i < framesNeeded; i++) output[ch][i] = 0;
        continue;
      }
      let head = this.heads[ch];
      let count = this.counts[ch];
      if (count > framesNeeded + 2400) {
        const skip = count - framesNeeded - 2400;
        head = (head + skip) % this.CAP;
        count -= skip;
      }
      const q = this.queues[ch];
      const toRead = Math.min(framesNeeded, count);
      for (let i = 0; i < framesNeeded; i++) {
        if (i < toRead) {
          output[ch][i] = q[head];
          head = (head + 1) % this.CAP;
        } else {
          output[ch][i] = 0;
        }
      }
      this.heads[ch] = head;
      this.counts[ch] = count - toRead;
    }
    return true;
  }
}
registerProcessor('pcm-player', PcmPlayerProcessor);
`;

export class OpusAudioPipeline extends AudioPipelineBase {

  feed(payload: Uint8Array, pts: number | null) {
    if (payload.length < 3) return;
    const opusData = payload.subarray(2);
    if (opusData.length === 0) return;

    const toc = opusData[0];
    const stereo = (toc >> 2) & 0x01;
    const channels = stereo ? 2 : 1;
    const { frameDurationUs } = parseOpusToc(toc);

    if (!this.canFeed(channels)) {
      if (!this.outputPending && !this.configured) {
        this.channels = channels;
        this.configureOpus();
      }
      return;
    }
    if (!this.decoder || this.decoder.state !== 'configured') return;

    // PTS from the demuxer is in 90 kHz units — convert to µs (100/9 per unit).
    // See VideoPipeline.emitAu for the rationale; old `pts / 90` was off by 1000×.
    const tsUs = pts != null ? Math.floor((pts * 100) / 9) : undefined;
    this.feedFrame(new EncodedAudioChunk({
      type: 'key',
      timestamp: tsUs ?? 0,
      data: opusData,
    }));
    if (this.packetsDecoded === 1) {
      console.info(`opus: first packet (${opusData.length}B, ${channels}ch, ${frameDurationUs / 1000}ms)`);
    }
  }

  private configureOpus() {
    this.decoder = new AudioDecoder({
      output: (frame) => this.routeFrame(frame),
      error: (e) => this.cb.onError(e),
    });
    try {
      this.decoder.configure({
        codec: 'opus',
        sampleRate: 48000,
        numberOfChannels: this.channels,
      } as AudioDecoderConfig);
      this.codecString = 'opus';
      this.startInit();
    } catch (e) {
      this.cb.onError(e);
      this.decoder = null;
    }
  }
}

export class AacAudioPipeline extends AudioPipelineBase {

  feed(payload: Uint8Array, pts: number | null) {
    if (payload.length < 7) return;

    const syncword = (payload[0] << 4) | (payload[1] >> 4);
    if (syncword !== 0xFFF) return;

    const protectionAbsent = payload[1] & 1;
    const headerSize = protectionAbsent ? 7 : 9;
    if (payload.length < headerSize + 1) return;

    const profile = (payload[2] >> 6) & 0x03;
    const freqIndex = (payload[2] >> 2) & 0x0F;
    const chanConfig = ((payload[2] & 1) << 2) | ((payload[3] >> 6) & 3);

    const sr = freqIndex < ADTS_SAMPLE_RATES.length ? ADTS_SAMPLE_RATES[freqIndex] : 48000;
    const ch = chanConfig > 0 ? chanConfig : 2;

    if (!this.canFeed(ch)) {
      if (!this.outputPending && !this.configured) {
        this.channels = ch;
        this.sampleRate = sr;
        this.configureAac(profile, freqIndex, chanConfig);
      }
      return;
    }
    if (!this.decoder || this.decoder.state !== 'configured') return;

    const aacData = payload.subarray(headerSize);
    if (aacData.length === 0) return;

    // PTS from the demuxer is in 90 kHz units — convert to µs (100/9 per unit).
    // See VideoPipeline.emitAu for the rationale; old `pts / 90` was off by 1000×.
    const tsUs = pts != null ? Math.floor((pts * 100) / 9) : undefined;
    this.feedFrame(new EncodedAudioChunk({
      type: 'key',
      timestamp: tsUs ?? 0,
      data: aacData,
    }));
    if (this.packetsDecoded === 1) {
      console.info(`aac: first packet (${aacData.length}B, ${ch}ch, ${sr}Hz, profile ${profile + 1})`);
    }
  }

  private configureAac(profile: number, freqIndex: number, chanConfig: number) {
    const aot = profile + 1;
    const asc = new Uint8Array(2);
    asc[0] = ((aot & 0x1f) << 3) | ((freqIndex >> 1) & 0x07);
    asc[1] = ((freqIndex & 1) << 7) | ((chanConfig & 0x0f) << 3);

    this.decoder = new AudioDecoder({
      output: (frame) => this.routeFrame(frame),
      error: (e) => this.cb.onError(e),
    });
    const codec = aot === 2 ? 'mp4a.40.2' : aot === 5 ? 'mp4a.40.5' : 'mp4a.40.2';
    try {
      this.decoder.configure({
        codec,
        sampleRate: this.sampleRate,
        numberOfChannels: this.channels,
        description: asc,
      } as AudioDecoderConfig);
      this.codecString = codec;
      this.startInit();
    } catch (e) {
      this.cb.onError(e);
      this.decoder = null;
    }
  }
}
