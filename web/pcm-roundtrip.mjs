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

// Feed raw TS bytes through a fresh TsDemuxer and return every emitted event.
// Used by the sparse-mode tests, which drive the muxer directly.
function demuxTs(tsBytes) {
  const demux = new mpeg2ts.TsDemuxer();
  const events = [];
  for (let i = 0; i < tsBytes.length; i += TS_PACKET_SIZE) {
    const slice = tsBytes.subarray(i, Math.min(i + TS_PACKET_SIZE, tsBytes.length));
    const batch = demux.feed(slice);
    for (const e of batch) events.push(e);
  }
  return events;
}

// Extract just the PID values from a PMT event (every other entry is a
// stream_type). pmtEntries() → flat [pid0, stream_type0, pid1, stream_type1, ...].
function pmtPids(pmtEvent) {
  const entries = pmtEvent.pmtEntries();
  const pids = [];
  for (let i = 0; i + 1 < entries.length; i += 2) pids.push(entries[i]);
  return pids;
}

// Format a PID list for log/diagnostic output.
function fmtPids(pids) {
  return pids.length ? pids.map((p) => '0x' + p.toString(16)).join(', ') : 'none';
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

// ---------------------------------------------------------------------------
// Sparse-channel transport tests.
//
// The muxer exposes optional sparse-mode knobs (setSparseEnabled /
// setSparseThreshold) implemented by ts-muxer-wasm. When sparse is on, an
// audio PID that stays silent past the threshold is dropped from both the PES
// stream and the PMT; the first non-zero push revives it immediately.
//
// The WASM is rebuilt separately from this script, so we detect the API at
// runtime. If sparse isn't present in the build, these tests SKIP (not fail)
// to keep the suite green on stale builds; once the API lands they run for
// real and hard-fail on regression like the tests above.
const SPARSE_AVAILABLE =
  typeof muxerMod.TsMuxer.prototype.setSparseEnabled === 'function' &&
  typeof muxerMod.TsMuxer.prototype.setSparseThreshold === 'function';

// 10ms of audio per frame at 48kHz: the unit the muxer timestamps against.
const SILENT_FRAME = new Float32Array(480);
const SPARSE_THRESHOLD_MS = 10;
const SILENT_FRAMES = 20; // 200ms of silence — well past the 10ms threshold.

// Tests 5 & 6 share one muxer: Test 5 suppresses PID 0x101 with sustained
// silence, Test 6 revives it with non-zero audio on the same muxer.
{
  if (!SPARSE_AVAILABLE) {
    console.log('Test 5 sparse drops silent PID: SKIP (sparse API not in WASM build)');
    console.log('Test 6 sparse re-adds on signal: SKIP (sparse API not in WASM build)');
  } else {
    const m = new muxerMod.TsMuxer();
    m.setVideoEnabled(false);
    m.setAudioCodec('s302m', 2);
    m.setSparseEnabled(true);
    m.setSparseThreshold(SPARSE_THRESHOLD_MS);

    // --- Test 5: ~200ms of all-zero frames should drop 0x101 from the PMT. ---
    for (let i = 0; i < SILENT_FRAMES; i++) {
      m.push_pcm(SILENT_FRAME, i * (SPARSE_THRESHOLD_MS * 1000.0));
    }
    let events = demuxTs(m.poll());
    let pmtEvents = events.filter((e) => e.kind === KIND_PMT);
    if (pmtEvents.length === 0) {
      console.error('FAIL: Test 5 sparse drops silent PID: no PMT events emitted');
      process.exit(1);
    }
    let lastPids = pmtPids(pmtEvents[pmtEvents.length - 1]);
    if (lastPids.includes(0x101)) {
      console.error(
        `FAIL: Test 5 sparse drops silent PID: 0x101 still in last PMT (pids: ${fmtPids(lastPids)})`
      );
      process.exit(1);
    }
    console.log(
      `Test 5 sparse drops silent PID: 0x101 absent after ${SILENT_FRAMES} silent frames (last PMT pids: ${fmtPids(lastPids)})`
    );
    console.log('Test 5 sparse drops silent PID: PASS');

    // --- Test 6: non-zero audio must re-add 0x101 to the PMT and emit PCM. ---
    const signal = sineSamples(480);
    const revivePtsUs = SILENT_FRAMES * (SPARSE_THRESHOLD_MS * 1000.0);
    m.push_pcm(signal, revivePtsUs); // revives the PID
    // Trailing non-zero frame: its PesStart flushes the revived PES so the
    // demuxer emits a kind=5 PCM event (the demuxer only flushes on the next
    // PesStart for the same PID).
    m.push_pcm(signal, revivePtsUs + SPARSE_THRESHOLD_MS * 1000.0);
    events = demuxTs(m.poll());
    pmtEvents = events.filter((e) => e.kind === KIND_PMT);
    if (pmtEvents.length === 0) {
      console.error('FAIL: Test 6 sparse re-adds on signal: no PMT events after revive');
      process.exit(1);
    }
    lastPids = pmtPids(pmtEvents[pmtEvents.length - 1]);
    if (!lastPids.includes(0x101)) {
      console.error(
        `FAIL: Test 6 sparse re-adds on signal: 0x101 missing from PMT after non-zero audio (pids: ${fmtPids(lastPids)})`
      );
      process.exit(1);
    }
    const pcmEvents = events.filter((e) => e.kind === KIND_PCM && e.pid === 0x101);
    if (pcmEvents.length === 0) {
      console.error('FAIL: Test 6 sparse re-adds on signal: no PCM event for 0x101 after revive');
      process.exit(1);
    }
    console.log(
      `Test 6 sparse re-adds on signal: 0x101 back in PMT, ${pcmEvents.length} PCM event(s)`
    );
    console.log('Test 6 sparse re-adds on signal: PASS');
  }
}

// Test 7: with sparse disabled, sustained silence must NOT drop the PID.
{
  if (!SPARSE_AVAILABLE) {
    console.log('Test 7 sparse disabled keeps PID: SKIP (sparse API not in WASM build)');
  } else {
    const m = new muxerMod.TsMuxer();
    m.setVideoEnabled(false);
    m.setAudioCodec('s302m', 2);
    m.setSparseEnabled(false); // explicitly off
    for (let i = 0; i < SILENT_FRAMES; i++) {
      m.push_pcm(SILENT_FRAME, i * (SPARSE_THRESHOLD_MS * 1000.0));
    }
    const events = demuxTs(m.poll());
    const pmtEvents = events.filter((e) => e.kind === KIND_PMT);
    if (pmtEvents.length === 0) {
      console.error('FAIL: Test 7 sparse disabled keeps PID: no PMT events emitted');
      process.exit(1);
    }
    const pids = pmtPids(pmtEvents[pmtEvents.length - 1]);
    if (!pids.includes(0x101)) {
      console.error(
        `FAIL: Test 7 sparse disabled keeps PID: 0x101 dropped despite sparse disabled (pids: ${fmtPids(pids)})`
      );
      process.exit(1);
    }
    console.log(
      `Test 7 sparse disabled keeps PID: 0x101 retained while silent (pids: ${fmtPids(pids)})`
    );
    console.log('Test 7 sparse disabled keeps PID: PASS');
  }
}

// Test 8: sparse PMT reflects only active PIDs (multi-PID muxer).
//
// push_pcm only feeds the first SMPTE 302M stream (PID 0x101), so PID 0x102
// never receives data. We only assert on 0x101's behavior (dropped after
// silence); 0x102's presence is logged but not asserted either way.
{
  if (!SPARSE_AVAILABLE) {
    console.log('Test 8 sparse PMT active PIDs: SKIP (sparse API not in WASM build)');
  } else {
    const m = new muxerMod.TsMuxer();
    m.setVideoEnabled(false);
    m.setAudioCodec('s302m', 2);
    m.addAudioPid(0x102, 's302m', 2);
    m.setSparseEnabled(true);
    m.setSparseThreshold(SPARSE_THRESHOLD_MS);

    // Prime 0x101 with non-zero audio (only PID 0x101 is reachable).
    const signal = sineSamples(480);
    const primeFrames = 3;
    for (let i = 0; i < primeFrames; i++) {
      m.push_pcm(signal, i * (SPARSE_THRESHOLD_MS * 1000.0));
    }
    // Then drive 0x101 silent long enough to trigger the sparse drop.
    for (let i = 0; i < SILENT_FRAMES; i++) {
      m.push_pcm(SILENT_FRAME, (primeFrames + i) * (SPARSE_THRESHOLD_MS * 1000.0));
    }

    const events = demuxTs(m.poll());
    const pmtEvents = events.filter((e) => e.kind === KIND_PMT);
    if (pmtEvents.length === 0) {
      console.error('FAIL: Test 8 sparse PMT active PIDs: no PMT events emitted');
      process.exit(1);
    }
    const finalPids = pmtPids(pmtEvents[pmtEvents.length - 1]);
    const pid102Present = finalPids.includes(0x102);

    // Core assertion: 0x101 must be gone after sustained silence.
    if (finalPids.includes(0x101)) {
      console.error(
        `FAIL: Test 8 sparse PMT active PIDs: 0x101 still in PMT after silence (pids: ${fmtPids(finalPids)})`
      );
      process.exit(1);
    }
    console.log(
      `Test 8 sparse PMT active PIDs: 0x101 dropped; 0x102 ${pid102Present ? 'present' : 'absent'} (never pushed to — informational); final PMT pids: ${fmtPids(finalPids)}`
    );
    console.log('Test 8 sparse PMT active PIDs: PASS');
  }
}

if (SPARSE_AVAILABLE) {
  console.log('\nAll PCM round-trip + sparse-channel transport tests passed.');
} else {
  console.log('\nAll PCM round-trip tests passed.');
  console.log('Sparse-channel transport tests skipped (rebuild ts-muxer-wasm to enable).');
}
