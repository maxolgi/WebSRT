// Node smoke test for srt-wasm + mpeg2ts-wasm.
//
// Loads both wasm packages, constructs a TsDemuxer, feeds the fixture .ts file,
// and reports events. Smoke test exit criterion: tests pass.
//
// Run: node --experimental-fetch web/smoke.mjs (or use a static import fs).

import fs from 'node:fs';
import path from 'node:path';
import { fileURLToPath } from 'node:url';

const __dirname = path.dirname(fileURLToPath(import.meta.url));

// wasm-pack output dirs
const MPEG2TS_PKG = path.join(__dirname, '..', 'crates', 'mpeg2ts-wasm', 'pkg');
const SRT_PKG = path.join(__dirname, '..', 'crates', 'srt-wasm', 'pkg');
const FIXTURE = path.join(__dirname, '..', 'fixtures', 'test.ts');

async function loadPkg(pkgDir, name) {
  // wasm-pack -t web produces ESM that imports the .wasm file via fetch + URL.
  // In node we shim fetch + URL to read from disk.
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
const srt = await loadPkg(SRT_PKG, 'srt_wasm');

console.log('loaded:', {
  mpeg2ts: Object.keys(mpeg2ts),
  srt: Object.keys(srt),
});

// Test 1: mpeg2ts-wasm — feed the fixture, count events.
{
  const demux = new mpeg2ts.TsDemuxer();
  const bytes = fs.readFileSync(FIXTURE);
  console.log(`feeding ${bytes.byteLength} bytes of fixture to TsDemuxer…`);

  const stats = { pat: 0, pmt: 0, pes: 0, ra: 0, error: 0 };
  // Feed in 1KB chunks (will accumulate internally).
  const CHUNK = 1024;
  for (let i = 0; i < bytes.byteLength; i += CHUNK) {
    const slice = bytes.subarray(i, Math.min(i + CHUNK, bytes.byteLength));
    const events = demux.feed(slice);
    for (const e of events) {
      switch (e.kind) {
        case 0: stats.pat++; break;
        case 1: stats.pmt++; break;
        case 2: stats.pes++; break;
        case 3: stats.ra++; break;
        case 4: stats.error++; console.log('err:', e.text); break;
      }
    }
  }
  console.log('mpeg2ts-wasm stats:', stats);
  if (stats.pat === 0 || stats.pmt === 0 || stats.pes === 0) {
    console.error('FAIL: expected at least 1 PAT, 1 PMT, and 1 PES event');
    process.exit(1);
  }
  if (stats.error > 0) {
    console.error(`FAIL: ${stats.error} parse errors during demux`);
    process.exit(1);
  }
  console.log('mpeg2ts-wasm: PASS');
}

// Test 1b: debug_snapshot() — exercise the demux analysis surface.
{
  const demux = new mpeg2ts.TsDemuxer();
  const bytes = fs.readFileSync(FIXTURE);
  const CHUNK = 1024;
  for (let i = 0; i < bytes.byteLength; i += CHUNK) {
    const slice = bytes.subarray(i, Math.min(i + CHUNK, bytes.byteLength));
    demux.feed(slice);
  }
  const snap = demux.debugSnapshot();
  const totalPes = snap.pesCounts.reduce((a, b) => a + b, 0);
  console.log('debug_snapshot:', {
    programNum: snap.programNum,
    pmtPid: snap.pmtPid,
    pmtEntries: snap.pmtPids.length,
    pids: snap.pids.length,
    pcrPids: snap.pcrPids.length,
    nalPids: snap.nalPids.length,
    totalPes,
    ringLen: snap.ringT.length,
    errorCount: snap.errorMsg.length,
  });
  if (snap.programNum < 0 || snap.pmtPid < 0) {
    console.error('FAIL: debug_snapshot missing PAT/PMT (programNum/pmtPid)');
    process.exit(1);
  }
  if (snap.pids.length === 0) {
    console.error('FAIL: debug_snapshot has no pids');
    process.exit(1);
  }
  if (snap.ringT.length === 0) {
    console.error('FAIL: debug_snapshot packet ring empty');
    process.exit(1);
  }
  // Sanity: scrambling/af-control flat arrays are 4× the per-PID count.
  if (snap.scramblingCounts.length !== snap.pids.length * 4) {
    console.error(`FAIL: scramblingCounts len ${snap.scramblingCounts.length} != pids*4`);
    process.exit(1);
  }
  if (snap.afControlCounts.length !== snap.pids.length * 4) {
    console.error(`FAIL: afControlCounts len ${snap.afControlCounts.length} != pids*4`);
    process.exit(1);
  }
  console.log('debug_snapshot: PASS');
}

// Test 2: srt-wasm — construct, poll, verify no crash.
{
  const rx = new srt.SrtReceiver();
  console.log('constructed SrtReceiver; hs_complete=' + rx.isHandshakeComplete() + ' closed=' + rx.isClosed());

  // No datagrams to feed yet (gateway side isn't running). Just exercise poll()
  // a few times to confirm we don't panic.
  const nowUs = performance.now() * 1000;
  for (let i = 0; i < 5; i++) {
    const actions = rx.poll(nowUs + i * 10000);
    if (actions.length > 0) {
      console.log(`poll ${i}: ${actions.length} actions (unexpected)`);
    }
  }
  if (rx.isHandshakeComplete()) {
    console.error('FAIL: receiver should not be handshake-complete without any gateway traffic');
    process.exit(1);
  }
  console.log('srt-wasm: PASS');
}

console.log('\nAll smoke tests passed.');
