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

interface NalUnit {
  type: number;
  data: Uint8Array; // includes the 1-byte header
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
        out.push({ type, data });
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

function bytesEqual(a: Uint8Array | null, b: Uint8Array | null): boolean {
  if (a === b) return true;
  if (!a || !b || a.length !== b.length) return false;
  for (let i = 0; i < a.length; i++) {
    if (a[i] !== b[i]) return false;
  }
  return true;
}

export class VideoPipeline {
  private decoder: VideoDecoder | null = null;
  private sps: Uint8Array | null = null;
  private pps: Uint8Array | null = null;
  private cb: DecoderCallbacks;
  // Pending NALU batches seen between SPS/PPS arrival and decoder.configure().
  // Each batch retains its original PTS so flushed chunks don't get stamped
  // with the wrong timestamp.
  private pendingNalus: { nalus: NalUnit[]; pts: number | null; isKeyframe: boolean }[] = [];
  private static readonly PENDING_CAP = 30;
  private configured = false;
  private decodedCount = 0;
  private seenKeyframe = false;

  constructor(cb: DecoderCallbacks) {
    this.cb = cb;
  }

  /** Feed a PES payload from the demuxer (Annex-B byte stream). */
  feed(payload: Uint8Array, pts: number | null, isKeyframe: boolean) {
    const nalus = parseAnnexB(payload);

    // Only re-configure if the SPS/PPS bytes actually changed. OBS emits
    // SPS/PPS in-band before every IDR (~2s); identical re-emissions must
    // not trigger a re-configure cycle.
    for (const n of nalus) {
      if (n.type === NAL_SPS) {
        if (!bytesEqual(this.sps, n.data)) {
          this.sps = n.data;
          this.configured = false;
        }
      } else if (n.type === NAL_PPS) {
        if (!bytesEqual(this.pps, n.data)) {
          this.pps = n.data;
          this.configured = false;
        }
      }
    }

    if (!this.configured) {
      if (this.sps && this.pps) {
        this.configure();
      }
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
    const hasIdr = nalus.some((n) => n.type === NAL_IDR);
    const decodeNalus = nalus.filter((n) => n.type >= 1 && n.type <= 5);
    if (decodeNalus.length === 0) return;
    if (!this.seenKeyframe && !hasIdr) return;
    if (hasIdr) this.seenKeyframe = true;
    const qDepth = this.decoder.decodeQueueSize;
    if (!hasIdr && qDepth > 8) {
      return;
    }
    const data = nalusToLengthPrefixed(decodeNalus);
    const tsUs = pts != null ? Math.floor(pts / 90) : undefined;
    const chunk = new EncodedVideoChunk({
      type: hasIdr ? 'key' : 'delta',
      timestamp: tsUs ?? 0,
      data,
    });
    try {
      this.decoder.decode(chunk);
      this.decodedCount++;
    } catch (e) {
      this.cb.onError(e);
      this.reset();
    }
  }

  private configure() {
    if (!this.sps || !this.pps) return;
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
      this.decoder.configure({
        codec: 'avc1.' + toHex(info.profile) + toHex(info.constraint) + toHex(info.level),
        description: avcc,
        codedWidth: info.width || undefined,
        codedHeight: info.height || undefined,
      } as VideoDecoderConfig);
      this.configured = true;
      this.seenKeyframe = false;
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

  reset() {
    this.configured = false;
    this.sps = null;
    this.pps = null;
    this.pendingNalus = [];
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
    const Ctx = (window.AudioContext || (window as any).webkitAudioContext) as typeof AudioContext;
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
  protected shouldFeed(channels: number): boolean {
    if (this.outputPending) return false;
    if (!this.configured || (this.decoder && this.channels !== channels)) {
      this.channels = channels;
      return false; // caller should call startInit
    }
    return this.configured;
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

    if (!this.shouldFeed(channels)) {
      if (!this.outputPending && !this.configured) {
        this.configureOpus();
      }
      return;
    }
    if (!this.decoder || this.decoder.state !== 'configured') return;

    const tsUs = pts != null ? Math.floor(pts / 90) : undefined;
    this.feedFrame(new EncodedAudioChunk({
      type: 'key',
      timestamp: tsUs ?? 0,
      data: opusData,
    }));
    if (this.packetsDecoded === 1) {
      this.cb.onError(`opus: first packet (${opusData.length}B, ${channels}ch, ${frameDurationUs / 1000}ms)`);
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

    if (!this.shouldFeed(ch)) {
      if (!this.outputPending && !this.configured) {
        this.sampleRate = sr;
        this.configureAac(profile, freqIndex, chanConfig);
      }
      return;
    }
    if (!this.decoder || this.decoder.state !== 'configured') return;

    const aacData = payload.subarray(headerSize);
    if (aacData.length === 0) return;

    const tsUs = pts != null ? Math.floor(pts / 90) : undefined;
    this.feedFrame(new EncodedAudioChunk({
      type: 'key',
      timestamp: tsUs ?? 0,
      data: aacData,
    }));
    if (this.packetsDecoded === 1) {
      this.cb.onError(`aac: first packet (${aacData.length}B, ${ch}ch, ${sr}Hz, profile ${profile + 1})`);
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
      this.startInit();
    } catch (e) {
      this.cb.onError(e);
      this.decoder = null;
    }
  }
}
