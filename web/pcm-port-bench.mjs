// PCM port bench: relay (worker → main → consumer) vs direct MessagePort.
//
// Node cannot run the real receiver worker (needs WebTransport), so this is
// the synthetic comparison the embedding task specifies: a paced producer
// (nominal s302m PES cadence, ~23 ms per pcm batch) feeding
//   (A) relay: producer worker → main thread (postMessage) → setTimeout(0)
//       reschedule → consumer worker (postMessage)
//   (B) direct: producer → consumer over a transferred MessagePort
// measuring arrival jitter at the consumer. ArrayBuffers are TRANSFERRED on
// every hop exactly as production does, so copy semantics match.
//
// Honesty notes:
//  (i)  The relay sim UNDERSTATES the real relay path's jitter — no renderer
//       or GC competes on this main thread. The gate is therefore RELATIVE:
//       direct jitter must be <= relay jitter, ideally decisively. Absolute
//       numbers are informational only.
//  (ii) Node's worker_threads MessagePort is not bit-identical to the
//       browser's, but both are structured-clone + transfer; the relative
//       comparison carries.
//
// Run: node web/pcm-port-bench.mjs [seconds]

import { Worker, MessageChannel } from 'node:worker_threads';

const SECONDS = Number(process.argv[2] ?? 20);
const BATCH_MS = 23.2; // one s302m PES per pcm batch (~43/s, matches the fixture)
const SAMPLES = 1114 * 2; // ~23.2 ms of stereo 48 kHz f32

const PRODUCER_SRC = `
const { parentPort } = require('node:worker_threads');
let out = parentPort;
let seq = 0;
let nextAt = performance.now();
function arm() {
  const delay = Math.max(0, nextAt - performance.now());
  setTimeout(() => {
    const now = performance.now();
    while (nextAt <= now) nextAt += ${BATCH_MS}; // never fall behind (drift-corrected)
    nextAt += ${BATCH_MS};
    const buf = new ArrayBuffer(${SAMPLES * 4});
    const samples = new Float32Array(buf);
    for (let i = 0; i < samples.length; i++) samples[i] = Math.sin(i * 0.01 + seq);
    out.postMessage({ seq: seq++, sched: now, samples }, [buf]);
    arm();
  }, delay);
}
arm();
parentPort.on('message', (m) => {
  if (m.cmd === 'use-port') { out = m.port; out.ref(); }
});
`;

const CONSUMER_SRC = `
const { parentPort } = require('node:worker_threads');
const samples = [];
const sink = (m) => samples.push([m.seq, performance.now() - m.sched]);
// relay mode: data arrives on parentPort; direct mode: via 'use-port'.
parentPort.on('message', (m) => {
  if (m.cmd === 'use-port') {
    m.port.on('message', sink);
    m.port.ref();
  } else if (m.cmd === 'dump') {
    samples.sort((a, b) => a[0] - b[0]);
    parentPort.postMessage(samples);
  } else {
    sink(m);
  }
});
`;

function stats(errs) {
  const sorted = [...errs].sort((a, b) => a - b);
  const pct = (p) => sorted[Math.min(sorted.length - 1, Math.ceil((p / 100) * sorted.length) - 1)];
  const mean = errs.reduce((a, b) => a + b, 0) / (errs.length || 1);
  return { n: errs.length, meanMs: +mean.toFixed(3), p50Ms: +pct(50).toFixed(3), p95Ms: +pct(95).toFixed(3), p99Ms: +pct(99).toFixed(3), maxMs: +sorted[sorted.length - 1].toFixed(3) };
}

async function runPath(name, build) {
  const { producer, consumer } = build();
  await new Promise((r) => setTimeout(r, SECONDS * 1000));
  // settle: let in-flight main-thread reschedules (relay path) land so both
  // paths are dumped from a quiesced state
  await new Promise((r) => setTimeout(r, 300));
  await producer.terminate();
  const samples = await new Promise((r) => consumer.once('message', r).postMessage({ cmd: 'dump' }));
  await consumer.terminate();
  return { name, ...stats(samples.slice(10).map((s) => s[1])) }; // drop warmup
}

// (A) relay: producer → main → setTimeout(0) → consumer
const relay = await runPath('relay (worker→main→worker)', () => {
  const consumer = new Worker(CONSUMER_SRC, { eval: true });
  const producer = new Worker(PRODUCER_SRC, { eval: true });
  producer.on('message', (m) => {
    // main-thread reschedule hop, then re-transfer to the consumer
    setTimeout(() => consumer.postMessage(m, [m.samples.buffer]), 0);
  });
  return { producer, consumer };
});

// (B) direct: producer writes into a transferred MessagePort
const direct = await runPath('direct MessagePort', () => {
  const ch = new MessageChannel();
  const consumer = new Worker(CONSUMER_SRC, { eval: true });
  const producer = new Worker(PRODUCER_SRC, { eval: true });
  consumer.postMessage({ cmd: 'use-port', port: ch.port1 }, [ch.port1]);
  producer.postMessage({ cmd: 'use-port', port: ch.port2 }, [ch.port2]);
  return { producer, consumer };
});

console.log(`pcm port bench — ${SECONDS}s, batch every ${BATCH_MS}ms, ${SAMPLES} f32 frames, buffers transferred every hop`);
console.log('NOTE: relay sim understates real-world relay jitter (no renderer/GC on this main thread); gate is RELATIVE (direct <= relay).\n');
console.table([relay, direct]);

const gateMean = direct.meanMs <= relay.meanMs;
const gateP99 = direct.p99Ms <= relay.p99Ms;
console.log(`gate: direct mean ${direct.meanMs}ms <= relay ${relay.meanMs}ms → ${gateMean ? 'PASS' : 'FAIL'}`);
console.log(`gate: direct p99  ${direct.p99Ms}ms <= relay ${relay.p99Ms}ms → ${gateP99 ? 'PASS' : 'FAIL'}`);
if (!gateMean || !gateP99) process.exit(1);
