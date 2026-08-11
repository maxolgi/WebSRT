# 128-channel PCM audio over WebSRT

**Goal:** Transport up to 128 channels of uncompressed PCM audio (mixed mono/stereo/surround) from ffmpeg through the WebSRT gateway to a browser-based WASM audio mixer, using MPEG-2 TS with SMPTE 302M encapsulation.

**Immediate scope:** ffmpeg → gateway → browser WASM mixer.
**Future scope:** replace ffmpeg source with [MXL](https://github.com/dmf-mxl/mxl) shared-memory Float32 audio.

---

## Architecture

```
[ffmpeg s302m]                         [Gateway]                    [Browser mixer]
  multi-PID TS       SRT push            pass-through        ┌─► mpeg2ts-wasm (parse)
  mono/stereo/  ──SRT ingest──►  ──bytes──►  ──WebTransport──►  ├─► AES3 unwrap → f32 (per PID)
  surround PIDs    ?streamid=X                              └─► WASM mixer (AudioWorklet out)
```

- **One TS program, N audio PIDs** (mono / stereo / 5.1 / 7.1 per PID, per SMPTE 302M channel-pair semantics)
- **Zero gateway changes** — it's already a byte-for-byte pass-through (`crates/websrt/src/ingest/mod.rs:54-57`, README:409-412)
- **No WebCodecs** — PCM is unpacked from AES3 frames in Rust, no decoder needed
- **ffmpeg `s302m` encoder** produces standard SMPTE 302M multi-PID TS directly
- **Browser-to-browser later** (MXL bridge is a separate native binary, see Phase 4)

## Wire math (sanity check)

| Metric | Value |
|---|---|
| Raw PCM (128ch × 48 kHz × 24-bit) | 18.4 Mbps |
| + AES3 / TS overhead | ~20 Mbps |
| Datagrams/sec aggregate @ 1100 B | ~2,300 |
| Per-session SRT send buffer (8192 pkts) | ~3.5 s headroom at full rate |
| Gateway broadcast capacity default (4096 msgs) | **must raise** to ~16384 |
| Per-PID datagram rate (8ch @ 48 kHz) | ~17 datagrams/sec |

Mono-bandwidth caveat: AES3 packs 1 mono channel in 8 bytes (uses 1 subframe). 128 mono feeds ≈ 37 Mbps instead of 18.4. Acceptable; pack stereo pairs where possible.

---

## Stable interface: PCM handoff to mixer

The mixer is being built in parallel. **This is the contract** the worker implements and the mixer consumes. Agree before Phase 2.

### PCM packet (Worker → Mixer), zero-copy transfer

```ts
type PcmPacket = {
  type: 'pcm';
  pid: number;                       // TS PID (0x101, 0x102, …)
  channelCount: 1 | 2 | 6 | 8;       // mono / stereo / 5.1 / 7.1
  sampleRate: 48000;
  samples: Float32Array;             // interleaved, length = channelCount × frames
  ptsMs: number;                     // PES PTS in ms, for sync
};
postMessage(pcmPacket, [pcmPacket.samples.buffer]);  // transferable, zero-copy
```

### PID announcement (Worker → Mixer, on PMT update)

```ts
type PidMap = {
  type: 'pidmap';
  streams: Array<{
    pid: number;
    channelCount: number;
    languageCode: string;  // MPEG-2 audio component descriptor, 3-char ISO 639: "mic", "gtr", "vox"
    label?: string;        // optional, longer name
  }>;
};
```

### Control (Mixer → Worker)

```ts
{ type: 'subscribe'; pids: number[] }  // mute/unmute per PID (default: all)
```

### Notes for the mixer team

- PCM arrives as **Float32 interleaved** — i32→f32 conversion is done in the Rust demuxer at AES3-unpack time (cheaper than in JS).
- **48 kHz fixed.** Resampling is the mixer's concern if it needs another rate.
- **`ptsMs`** comes from PES PTS (ffmpeg populates correctly for s302m).
- **PID map can change** mid-stream if the source reconfigures; handle `pidmap` events idempotently.
- **Best case at maturity:** mixer WASM loads in the **same Worker** as `mpeg2ts-wasm` for direct wasm-bindgen calls (zero-copy, no JS boundary). Until then, `postMessage` + `Transferable` works.

---

## Phased plan

### Phase 0 — Spike (1 mono PID end-to-end)  ·  ~1-2 days

**Goal:** ffmpeg SRT push → gateway → browser viewer hears PCM audio. No codec work.

1. Create `fixtures/stream_pcm.sh` — ffmpeg sine source → `s302m` encoder → SRT push to gateway listener (mirror existing `fixtures/stream.sh` pattern).
2. Add SMPTE 302M recognition in `crates/mpeg2ts-wasm/src/lib.rs`: stream type `0x06` + registration descriptor tag `0x43554553` ("CUES"). Currently the demuxer only flags Opus/AAC in `stream_type_for_pid` (lines 511-516).
3. AES3 frame unwrapper in Rust: each PES payload is N × 8-byte AES3 frames (4 bytes/subframe × 2 subframes). Extract 24-bit samples, right-justified. Output `Vec<f32>` (i32→f32 conversion at this boundary).
4. Expose unwrapper via wasm-bindgen: `TsDemuxer` already emits PES events; add a `pcm_samples(pid)` accessor or extend the `TsEvent::Pes` payload for SMPTE 302M PIDs.
5. `web/src/worker.ts` — branch on SMPTE 302M stream type (currently only Opus/AAC at lines 337-347). Emit `pcm` messages per the handoff contract above.
6. `web/src/shared/viewer.ts` — single `AudioWorkletNode` for the spike: receives Float32Array, plays via `AudioWorkletProcessor.process()`.

**Verify:**
- `ffprobe` on the wire shows `Audio: s302m`.
- Audible 440 Hz tone end-to-end: ffmpeg → gateway → browser.
- Wireshark confirms stream type `0x06` + SMPTE 302M descriptor in PMT.

---

### Phase 1 — Multi-PID demuxer + descriptor parse  ·  ~2-3 days

**Goal:** all N audio PIDs deliver PCM concurrently.

1. `crates/mpeg2ts-wasm/src/lib.rs` — convert scalar state to per-program / per-PID:
   - Replace `program_num`/`pmt_pid`/`pmt_entries` scalars (lines 403-405) with:
     ```rust
     audio_pids: HashMap<u16, AudioPidInfo>;
     struct AudioPidInfo {
         channel_count: u8,
         sample_rate: u32,
         bit_depth: u8,
         language_code: [u8; 3],   // from MPEG-2 component descriptor
         stream_type: u8,
     }
     ```
   - Drop the last-program-wins overwrite at lines 747-750 (PAT) and 783 (PMT).
2. Parse SMPTE 302M descriptor per PID for channel count + sample rate.
3. Parse MPEG-2 audio component descriptor `language_code` (3-char ISO 639) per PID — upstream `mpeg2ts` crate's `EsInfo` already exposes this.
4. Emit a `pidmap` event (handoff contract above) on every PMT update.
5. Extend `DebugSnapshot` (lines 1245+) to expose the audio PID map as an array. Follow the POJO mirror pattern in `worker.ts:getDemuxStats()`.

**Verify:**
- ffmpeg generates 4-PID TS: mono + stereo + 5.1 + mono, each with distinct `language_code` (set via ffmpeg `-metadata:s:a:N language=XXX`).
- Browser logs all 4 PIDs with correct channel counts + language tags.
- Demux debug tab shows all 4 audio PIDs.

---

### Phase 2 — Worker routing + mixer handoff  ·  ~2-3 days

**Goal:** route N PIDs to the WASM mixer.

1. `web/src/worker.ts`:
   - Replace scalar `audioPid`/`audioPipeline` (lines 68-69, 77) with `Map<number, PcmRoute>`.
   - Drop the `pid === audioPid` early-return at lines 236-241 — route every known SMPTE 302M PID.
2. Implement the three message types on the worker↔mixer boundary: `pcm`, `pidmap`, `subscribe`.
3. **If mixer WASM is ready** and loadable in the same Worker: swap `postMessage` for a direct wasm-bindgen call. Keep the message shape as the stable contract so the path can switch back if needed.
4. **Output path** (if mixer is not yet ready): single `AudioWorkletNode` per PID, or one shared `SharedArrayBuffer` ring buffer driven by a single worklet — decide based on measured CPU.

**Verify:**
- 8-PID stream → 8 PCM outputs visible in mixer meters (or 8 AudioWorklet outputs audible).
- `subscribe { pids: [0x101, 0x103] }` correctly mutes the other 6 PIDs.

---

### Phase 3 — Scale to 128 channels  ·  ~2-3 days

**Goal:** 128-channel stability.

1. **Gateway:** raise `--broadcast-capacity` from default 4096 to ~16384 (at ~2,300 datagrams/sec you want >2 s buffer).
2. **Browser publisher CPU:** batch PES processing in worker — process ≥8 PES packets per wasm-bindgen round-trip to amortize boundary cost. Current per-packet call pattern in `worker.ts` will not scale to 128 PIDs.
3. **Verify SRT `PAYLOAD_SIZE = 1100`** (`crates/websrt/src/srt_sender.rs:21`) doesn't fragment AES3 PES packets — at 8 ch/PID, ~5.7 ms audio/datagram fits cleanly.
4. **Verify per-session send buffer** (8192 pkts in `srt_sender.rs:41-52`) is sufficient at 128-channel rate.

**Verify:**
- 128ch 48 kHz/24-bit runs 10 minutes without drops.
- Gateway logs show no broadcast lag warnings.
- Viewer SRT stats show <0.1% loss.
- Mixer output glitch-free under load.

---

### Phase 4 (future) — MXL bridge

**Goal:** replace ffmpeg with [MXL](https://github.com/dmf-mxl/mxl) shared-memory source. Separate design exercise.

- MXL is EBU's Dynamic Media Facility "Media eXchange Layer" — C++ SDK with Rust bindings, shared-memory zero-copy, **Float32 audio**.
- Likely a **native** `websrt-mxl-bridge` binary using `crates/websrt/` library directly + MXL Rust bindings. Bypasses the browser entirely.
- Bridge reads Float32 frames from MXL shared memory → converts to 24-bit PCM → packs AES3 → publishes into the gateway's `Broadcaster` (or via SRT self-loop).
- Native publisher preferred over browser: lower latency, no WASM constraint, no SharedArrayBuffer hassles.
- Defer detailed design until the demux/mixer path (Phases 0-3) is proven.

---

## Risk register

| # | Risk | Mitigation |
|---|---|---|
| 1 | AES3 mono overhead (50% bandwidth waste) | Pack stereo pairs where possible; accept for mono-only feeds |
| 2 | ffmpeg `s302m` encoder multi-channel limits unknown | Phase 0 verification step — if ffmpeg caps channels per PID, pack as multiple PIDs |
| 3 | 128 PCM routes in one browser tab may hit CPU ceiling | Split across multiple Web Workers communicating via `MessageChannel`; or move mixer WASM into the demuxer worker for zero-copy direct calls |
| 4 | Gateway broadcast capacity too small at high datagram rate | Phase 3 step 1 raises the cap; tune empirically |
| 5 | SMPTE 302M descriptor parsing edge cases (variable-length channel assignment) | Validate against real ffmpeg output in Phase 1; fall back to defaults if descriptor absent |
| 6 | PES PTS discontinuity when ffmpeg source reconfigures | Mixer must handle `pidmap` events and reset PTS baseline |

## Decision log

| Decision | Choice | Rationale |
|---|---|---|
| Transport shape | Single SPTS with N audio PIDs | Avoids MPTS complexity, simpler demuxer state, SMPTE 302M native model |
| PCM encapsulation | SMPTE 302M | ffmpeg has native `s302m` codec for easy test source; broadcast-grade standard |
| Channel naming | MPEG-2 component descriptor `language_code` (3-char) | Carries in-band; no side-channel dependency |
| Source path (Phase 0) | ffmpeg → SRT ingester | Reuses existing gateway SRT ingest path |
| Mixer output target | Browser WASM (in-page) | User requirement; aligns with future MXL browser work |
| PCM format at handoff | Float32 interleaved | MXL native; cheap int24→f32 conversion at AES3-unpack time |

## Files touched (per phase)

**Phase 0**
- `fixtures/stream_pcm.sh` (new)
- `crates/mpeg2ts-wasm/src/lib.rs` (SMPTE 302M recognition + AES3 unwrap)
- `web/src/worker.ts` (PCM branch in pipeline selection)
- `web/src/shared/viewer.ts` (single AudioWorkletPlayer for spike)

**Phase 1**
- `crates/mpeg2ts-wasm/src/lib.rs` (per-PID state, descriptor parse, snapshot extension)
- `web/src/shared/pmt.ts` (multi-PID summarize, currently single-audio at lines 44-71)
- `web/src/worker.ts` (pidmap emission)
- `web/src/debug/components/` (display audio PID array — DemuxTab)

**Phase 2**
- `web/src/worker.ts` (PID map, routing, handoff messages)
- `web/src/shared/viewer.ts` (multi-PID output routing)
- Mixer-side consumer (out of scope for this repo, but contract documented above)

**Phase 3**
- Gateway config (`--broadcast-capacity`)
- `web/src/worker.ts` (batched PES processing)
- Possibly `crates/websrt/src/broadcaster.rs` (if capacity tuning needs code change beyond config)

**Phase 4 (future)**
- New crate `crates/websrt-mxl-bridge/` (native binary)
- Depends on MXL Rust bindings availability

## Build commands reference

```bash
# After mpeg2ts-wasm changes:
./build.sh wasm mpeg2ts

# After gateway/library changes:
./build.sh gateway && ./build.sh restart

# TypeScript typecheck:
cd web && npx tsc --noEmit

# Smoke test (no browser):
node web/smoke.mjs

# Run ffmpeg source (Phase 0):
./fixtures/stream_pcm.sh
```
