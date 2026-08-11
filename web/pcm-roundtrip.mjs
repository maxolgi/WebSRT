// Node test for SMPTE 302M (AES3) PCM round-trip through WASM.
//
// Encodes f32 samples with ts-muxer-wasm (push_pcm → AES3/SMPTE 302M TS
// packets), then decodes them back with mpeg2ts-wasm's TsDemuxer (kind=5
// PCM events). Verifies encode → TS → decode fidelity end-to-end with no
// network, no gateway, and no ffmpeg.
//
// Run: node web/pcm-roundtrip.mjs

import fs from 'node:fs';
import path from 'node:path';
import { fileURLToPath } from 'node:url';

const __dirname = path.dirname(fileURLToPath(import.meta.url));

// wasm-pack output dirs
const MPEG2TS_PKG = path.join(__dirname, '..', 'crates', 'mpeg2ts-wasm', 'pkg');
const MUXER_PKG = path.join(__dirname, '..', 'crates', 'ts-muxer-wasm', 'pkg');

// wasm-pack -t web produces ESM that imports the .wasm file via fetch + URL.
// In node we shim fetch + URL to read from disk.
async function loadPkg(pkgDir, name) {
  const mod = await import('file://' + path.join(pkgDir, `${name}.js`));
  const wasmPath = path.join(pkgDir, `${name}_bg.wasm`);
  const wasmBytes = fs.readFileSync(wasmPath);
  // `default` export is the init function that takes { module_or_path }.
  if (typeof mod.default === 'function') {
    await mod.default({ module_or_path: wasmBytes });
  } else if (mod.init) {
    await mod.init(wasmBytes);
  }
  return mod;
}

const mpeg2ts = await loadPkg(MPEG2TS_PKG, 'mpeg2ts_wasm');
const muxerMod = await loadPkg(MUXER_PKG, 'ts_muxer_wasm');

console.log('loaded:', {
  mpeg2ts: Object.keys(mpeg2ts),
  tsMuxer: Object.keys(muxerMod),
});

const TS_PACKET_SIZE = 188;

// TsEvent.kind values (mirror mpeg2ts-wasm).
const KIND_PAT = 0;
const KIND_PMT = 1;
const KIND_PES = 2;
const KIND_RA = 3;
const KIND_ERROR = 4;
const KIND_PCM = 5;

// 24-bit quantization step ≈ 1.19e-7; 1e-5 leaves ample headroom.
const EPS_24BIT = 1e-5;

// Run the muxer → demuxer pipeline and return all demuxer events.
//
// The demuxer only flushes a reassembled PES when the *next* PesStart
// arrives on the same PID, so we push a tiny trailing batch (one frame of
// zeros) solely to flush the primary PES via its next PesStart.
function pcmRoundTrip(primarySamples, opts = {}) {
  const channels = opts.channels ?? 2;
  const m = new muxerMod.TsMuxer();
  m.setVideoEnabled(false);
  m.setAudioCodec('s302m', channels);
  if (opts.extraPid) {
    m.addAudioPid(opts.extraPid, 's302m', opts.extraPidChannels ?? 2);
  }
  m.push_pcm(primarySamples, 0.0);
  // Trailing nudge: flushes the primary PES via the next PesStart.
  m.push_pcm(new Float32Array(channels), 1000.0);
  const tsBytes = m.poll();

  const demux = new mpeg2ts.TsDemuxer();
  const events = [];
  for (let i = 0; i < tsBytes.length; i += TS_PACKET_SIZE) {
    const slice = tsBytes.subarray(i, Math.min(i + TS_PACKET_SIZE, tsBytes.length));
    const batch = demux.feed(slice);
    for (const e of batch) events.push(e);
  }
  return events;
}

// Build a 440Hz sine of N samples at 48kHz.
function sineSamples(n) {
  const out = new Float32Array(n);
  for (let i = 0; i < n; i++) {
    out[i] = Math.sin((2 * Math.PI * 440 * i) / 48000);
  }
  return out;
}

// Test 1: Stereo s302m round-trip.
{
  const samples = sineSamples(480);
  const events = pcmRoundTrip(samples, { channels: 2 });

  const pcmEvents = events.filter((e) => e.kind === KIND_PCM);
  const errors = events.filter((e) => e.kind === KIND_ERROR);

  if (errors.length > 0) {
    console.error(
      `FAIL: Test 1 stereo round-trip: ${errors.length} demux errors (first: ${errors[0].text})`
    );
    process.exit(1);
  }
  if (pcmEvents.length === 0) {
    console.error('FAIL: Test 1 stereo round-trip: no kind=5 PCM events emitted');
    process.exit(1);
  }
  const pcm = pcmEvents[0];
  if (pcm.pid !== 0x101) {
    console.error(
      `FAIL: Test 1 stereo round-trip: expected pid 0x101, got 0x${pcm.pid.toString(16)}`
    );
    process.exit(1);
  }
  if (pcm.program_num !== 2) {
    console.error(
      `FAIL: Test 1 stereo round-trip: expected program_num (channel_count) 2, got ${pcm.program_num}`
    );
    process.exit(1);
  }
  const decoded = pcm.samples;
  if (decoded.length === 0) {
    console.error('FAIL: Test 1 stereo round-trip: decoded sample count is 0');
    process.exit(1);
  }
  const firstDiff = Math.abs(decoded[0] - samples[0]);
  if (firstDiff > EPS_24BIT) {
    console.error(
      `FAIL: Test 1 stereo round-trip: first sample mismatch (input ${samples[0]}, decoded ${decoded[0]}, diff ${firstDiff})`
    );
    process.exit(1);
  }
  console.log(
    `Test 1 stereo round-trip: ${pcmEvents.length} PCM event(s), ${decoded.length}/${samples.length} samples, first diff ${firstDiff.toExponential(2)}`
  );
  console.log('Test 1 stereo round-trip: PASS');
}

// Test 2: Audio-only PMT has no video entry.
{
  const samples = sineSamples(480);
  const events = pcmRoundTrip(samples, { channels: 2 });

  const pmtEvents = events.filter((e) => e.kind === KIND_PMT);
  if (pmtEvents.length === 0) {
    console.error('FAIL: Test 2 audio-only PMT: no kind=1 PMT events');
    process.exit(1);
  }
  // pmtEntries() → flat [pid0, stream_type0, pid1, stream_type1, ...].
  const entries = pmtEvents[pmtEvents.length - 1].pmtEntries();
  for (let i = 0; i + 1 < entries.length; i += 2) {
    const pid = entries[i];
    const st = entries[i + 1];
    if (st === 0x1b) {
      console.error(
        `FAIL: Test 2 audio-only PMT: H.264 entry present (pid 0x${pid.toString(16)}, stream_type 0x1B)`
      );
      process.exit(1);
    }
  }
  const count = entries.length / 2;
  console.log(
    `Test 2 audio-only PMT: ${count} entr${count === 1 ? 'y' : 'ies'}, no 0x1B video`
  );
  console.log('Test 2 audio-only PMT: PASS');
}

// Test 3: Multi-PID round-trip.
//
// push_pcm only targets the first SMPTE 302M stream (PID 0x101), so we
// cannot directly address PID 0x102. We verify the PMT structure contains
// both configured audio PIDs with stream_type 0x06.
{
  const samples = sineSamples(480);
  const events = pcmRoundTrip(samples, {
    channels: 2,
    extraPid: 0x102,
    extraPidChannels: 2,
  });

  const pmtEvents = events.filter((e) => e.kind === KIND_PMT);
  if (pmtEvents.length === 0) {
    console.error('FAIL: Test 3 multi-PID: no PMT event');
    process.exit(1);
  }
  const entries = pmtEvents[pmtEvents.length - 1].pmtEntries();
  const pids = [];
  for (let i = 0; i + 1 < entries.length; i += 2) pids.push(entries[i]);

  if (!pids.includes(0x101)) {
    console.error('FAIL: Test 3 multi-PID: PID 0x101 missing from PMT');
    process.exit(1);
  }
  if (!pids.includes(0x102)) {
    console.error('FAIL: Test 3 multi-PID: PID 0x102 missing from PMT');
    process.exit(1);
  }
  for (let i = 0; i + 1 < entries.length; i += 2) {
    if (entries[i + 1] !== 0x06) {
      console.error(
        `FAIL: Test 3 multi-PID: pid 0x${entries[i].toString(16)} has stream_type 0x${entries[i + 1].toString(16)}, expected 0x06`
      );
      process.exit(1);
    }
  }
  console.log(
    `Test 3 multi-PID: PMT audio PIDs ${pids.map((p) => '0x' + p.toString(16)).join(', ')} (all stream_type 0x06)`
  );
  console.log('Test 3 multi-PID: PASS');
}

// Test 4: Bit-depth fidelity (24-bit) on full-scale samples.
{
  const samples = new Float32Array([1.0, -1.0, 0.5, -0.5]);
  const events = pcmRoundTrip(samples, { channels: 2 });

  const pcmEvents = events.filter((e) => e.kind === KIND_PCM);
  if (pcmEvents.length === 0) {
    console.error('FAIL: Test 4 bit-depth fidelity: no PCM event');
    process.exit(1);
  }
  const decoded = pcmEvents[0].samples;
  if (decoded.length < samples.length) {
    console.error(
      `FAIL: Test 4 bit-depth fidelity: decoded ${decoded.length} samples, expected >= ${samples.length}`
    );
    process.exit(1);
  }
  let maxDiff = 0;
  for (let i = 0; i < samples.length; i++) {
    const diff = Math.abs(decoded[i] - samples[i]);
    if (diff > maxDiff) maxDiff = diff;
    if (diff > EPS_24BIT) {
      console.error(
        `FAIL: Test 4 bit-depth fidelity: sample ${i} input ${samples[i]} decoded ${decoded[i]} diff ${diff} > ${EPS_24BIT}`
      );
      process.exit(1);
    }
  }
  console.log(
    `Test 4 bit-depth fidelity: ${samples.length} full-scale samples verified, max diff ${maxDiff.toExponential(2)}`
  );
  console.log('Test 4 bit-depth fidelity: PASS');
}

console.log('\nAll PCM round-trip tests passed.');
