# SRT → WebTransport Gateway — Implementation Plan

## Goal

Pure-Rust gateway that bridges native SRT (from OBS) to a browser running the
**real SRT protocol over WebTransport datagrams** — same wire format, same
NAK/ACK/retransmit semantics, no stream-per-frame remuxing, no codec-specific
server logic. The browser runs `srt-protocol` and `mpeg2ts` compiled to WASM;
JS is glue only.

- Chrome via `serverCertificateHashes`; Firefox/all browsers via `mkcert` fallback.
- **Wire-compatible with native SRT** at the data-plane level (a native SRT
  sender's bytes look identical to ours modulo socket IDs and timestamps).
- Maximum code reuse: `srt-protocol` and `mpeg2ts` are the shared core on both
  sides.

## Decisions (locked)

| # | Decision | Choice |
|---|---|---|
| 1 | Server transport | `wtransport` (H3/QUIC + datagrams) |
| 2 | SRT ingest | `srt-tokio` v0.4.4 to terminate OBS connection |
| 3 | SRT state machines | `srt-protocol` v0.4.4 (no UDP/tokio deps — designed for this) |
| 4 | Logging | `tracing` (Rust), `console` (JS) |
| 5 | Configuration | `clap` CLI now; TOML later if needed |
| 6 | Browser SRT receiver | **Full WASM port of `srt-protocol`** — single source of truth |
| 7 | Browser TS demux | **WASM port of `mpeg2ts`** — single source of truth |
| 8 | SRT payload unit | **Raw TS packets (N×188 B per SRT message)** — gateway is codec-agnostic |
| 9 | SRT handshake scope | Full HSv5 (INDUCTION/CONCLUSION) — wire-compatible with native SRT |
| 10 | SRT crypto | Disabled on gateway↔browser link (`DataEncryption::None`); WebTransport TLS suffices |
| 11 | SRT payload size | ≤1100 B (fits QUIC datagram PMTU; = 5×188 B TS packets) |
| 12 | Repo init | `git init` at Phase 0 |
| 13 | Wire timestamp units | SRT-native µs (handled entirely inside `srt-protocol`) |

## Pure-Rust stack

| Concern | Crate | Notes |
|---|---|---|
| SRT ingest (listener for OBS) | `srt-tokio` v0.4.4 | Terminates OBS's native SRT/UDP connection |
| SRT sender (gateway→browser) | `srt-protocol` v0.4.4 | Driven over WebTransport datagrams; we write the driver |
| Browser SRT receiver | `srt-protocol` v0.4.4 → WASM | Same crate compiled to wasm32-unknown-unknown |
| Browser TS demux | `mpeg2ts` v0.6.0 → WASM | Same crate compiled to wasm |
| WASM bindings | `wasm-bindgen` + `wasm-pack` | Generate JS API for Rust state machines |
| WebTransport server | `wtransport` | Built on `quinn` + `rustls` + `h3` |
| Cert (ECDSA P-256, <2wk) | `rcgen` | Generated on every boot |
| Cert fingerprint | `sha2` | DER → SHA-256 → client |
| Runtime / utilities | `tokio`, `bytes`, `tracing`, `clap` | — |
| Browser bundler | Vite | Dev server + wasm loading |

No `ffmpeg-next`, no subprocess, no C deps at runtime. ffmpeg only generates the
test fixture.

## Architecture

```
                  ┌─────── Rust gateway ──────┐
[OBS] --SRT/UDP-->│ srt-tokio listener         │
                  │   ↓ (Instant, Bytes)       │       ┌── Browser ──────────────────────┐
                  │ srt-protocol::sender       │       │ JS: WebTransport datagram I/O    │
                  │   ↓ SRT packets (bytes)    │──WT──→│   ↓ bytes                       │
                  │ wtransport datagram driver │       │ WASM: srt-protocol::receiver     │
                  │   ↑ ACK/NAK (bytes)        │←─WT───│   ↓ (Instant, Bytes) messages    │
                  │ srt-protocol::sender.handle│       │ WASM: mpeg2ts demux              │
                  └────────────────────────────┘       │   ↓ PES / NAL / Opus             │
                                                       │ JS: WebCodecs decode + render    │
                                                       └──────────────────────────────────┘
```

- **Gateway is a dumb SRT repeater.** It terminates OBS's SRT connection, takes
  the resulting `(Instant, Bytes)` messages, and re-originates them as a new
  SRT sender to the browser. TS bytes are never inspected.
- **Each browser gets its own SRT sender instance** (independent seq numbers,
  independent retransmit buffer). One OBS connection fans out to N browsers by
  spawning N senders fed from the same `srt-tokio` receiver stream.
- **Browser runs the *same* Rust state machines** the gateway does, compiled to
  WASM. No TS-side SRT logic; no wire-format drift risk.
- **SRT crypto disabled** between gateway and browser. Wire-compatible with
  native SRT (which treats crypto as optional). WebTransport TLS replaces it.
- **No `CodecHandler` server-side, no avcC construction server-side, no control
  stream.** All codec logic moves to the browser alongside the TS demux.

## WASM bridge design

`srt-protocol` and `mpeg2ts` are both sync libraries designed for polling. We
compile each to WASM with a `wasm-bindgen` API shaped around their natural
polling style:

```rust
// crates/srt-wasm/src/lib.rs
#[wasm_bindgen]
pub struct SrtReceiver { inner: srt_protocol::protocol::receiver::Receiver }

#[wasm_bindgen]
impl SrtReceiver {
    #[wasm_bindgen(constructor)]
    pub fn new(config: JsValue) -> SrtReceiver { ... }

    /// Feed an incoming WT datagram (SRT packet bytes).
    pub fn handle_datagram(&mut self, bytes: &[u8], now_us: f64) -> Vec<SrtAction>;

    /// Periodic tick (JS calls every ~10ms via setTimeout).
    pub fn poll(&mut self, now_us: f64) -> Vec<SrtAction>;
}

#[wasm_bindgen]
pub enum SrtAction {
    SendDatagram(Vec<u8>),       // → wt.maxDatagramSize() datagram TX
    DeliverMessage(Vec<u8>),     // → mpeg2ts-wasm feed
    HandshakeComplete,
    DropLateMessage(u32),        // for stats
}
```

`mpeg2ts-wasm` mirrors this: `TsDemuxer::feed(bytes) -> Vec<TsEvent>` where
`TsEvent` covers PMT updates and PES packets.

**API survey is the first Phase 2 task** — the exact signatures above are
illustrative; `srt-protocol::protocol::receiver`'s actual public API needs to be
mapped before locking the wasm boundary.

## Cert strategy

- **Mode A (Chrome, zero-install):** `rcgen` ECDSA P-256, 13-day validity,
  self-signed. Server prints DER SHA-256 hex; client connects with
  `serverCertificateHashes`.
- **Mode B (Firefox / prod):** `mkcert localhost 127.0.0.1 ::1`. Client connects
  with normal PKI.
- Switch: `--cert-mode self|mkcert`.

## Test-vector strategy

- `--input srt :9000` → production (OBS calls in).
- `--input file fixtures/test.ts` → dev/test (read fixture, pace at real-time,
  loop, feed into SRT sender).
- Same fixture command: baseline H.264 + Opus + MPEG-TS, guaranteed-supported
  WebCodecs profile. ~1-2 MB, committed.

```
ffmpeg -f lavfi -i testsrc=duration=10:size=640x360:rate=30 \
       -f lavfi -i sine=frequency=440:duration=10 \
       -c:v libx264 -profile:v baseline -level 3.0 -g 30 -bf 0 \
       -c:a libopus -b:a 64k -f mpegts \
       fixtures/test.ts
```

New for this design: a **loss-injection mode** in the WT datagram driver
(`--sim-loss 5` for 5% random drop) to exercise NAK/retransmit in Phase 5
onward without OBS.

## Repo layout

```
WebSRT/
  Cargo.toml                           # workspace
  crates/
    gateway/
      src/main.rs                      # arg parse, --input srt|file, --cert-mode, --sim-loss
      src/cert.rs                      # rcgen ECDSA P-256 (13d) + DER sha256
      src/ingest/
        mod.rs                         # Ingester trait, FileIngester
        srt.rs                         # SrtIngester: srt-tokio listener (Phase 8)
        file.rs                        # FileIngester: read .ts, pace, loop
      src/srt_sender.rs                # srt-protocol::sender driver over WT datagrams
      src/server.rs                    # wtransport endpoint + accept loop
      src/session.rs                   # per-browser: Ingester→sender→datagram pump + ACK RX
    srt-wasm/                          # → pkg/ via wasm-pack
      Cargo.toml                       # crate-type cdylib, deps srt-protocol + wasm-bindgen
      src/lib.rs                       # wasm-bindgen API over srt-protocol::receiver
    mpeg2ts-wasm/                      # → pkg/ via wasm-pack
      Cargo.toml                       # crate-type cdylib, deps mpeg2ts + wasm-bindgen
      src/lib.rs                       # wasm-bindgen API over mpeg2ts::TsDemuxer
  web/
    index.html
    package.json                       # vite + wasm-pack plugin
    src/
      main.ts                          # WT connect, datagram shuttle, glue
      srt.ts                           # wrap srt-wasm pkg, drive via setTimeout
      demux.ts                         # wrap mpeg2ts-wasm pkg, route events
      decode.ts                        # VideoDecoder/AudioDecoder dispatch, avcC, WebCodecs
      render.ts                        # canvas + MediaStreamTrackGenerator
  fixtures/
    test.ts                            # generated once
    make-fixture.sh
  certs/README.md                      # mkcert instructions
  README.md
  plan.md
```

## Phased delivery

Each phase has a hard exit criterion.

### Phase 0 — Skeleton + cert bootstrap
- `git init`.
- Cargo workspace, deps, `tracing` setup.
- `cert.rs`: generate ECDSA P-256 cert (13d), build `wtransport::Identity`,
  print DER SHA-256 hex.
- **Exit:** binary boots, prints a 32-byte hash.

### Phase 1 — WebTransport datagram echo (de-risks cert + datagram plumbing)
- `server.rs`: accept one WT session at `/wt`, echo datagrams.
- `web/`: minimal TS page that connects with `serverCertificateHashes`, sends
  "ping" datagram, logs "pong" datagram.
- **Exit:** Chrome ↔ Rust round-trips a datagram over WT. ⭐ Most important
  milestone.

### Phase 2 — WASM bridge: `srt-wasm` + `mpeg2ts-wasm` build
- **API survey of `srt-protocol::protocol::receiver`** (read source, map state
  machine to wasm boundary).
- Set up `wasm-pack build -t web` for both crates.
- Implement minimal `wasm-bindgen` API on each: instantiate, feed bytes, return
  actions/events.
- Vite config to load wasm modules.
- Smoke test: JS round-trips a hardcoded SRT packet through wasm and back;
  round-trips a TS packet through mpeg2ts-wasm.
- **Exit:** `npm run dev` loads both wasm modules, smoke tests pass in browser
  console.

### Phase 3 — SRT handshake round-trip
- `srt_sender.rs`: minimal driver — instantiate `srt-protocol::sender`, send
  HSv5 handshake via WT datagrams, handle browser's handshake responses.
- `srt.ts`: drive `srt-wasm` receiver through INDUCTION/CONCLUSION, exchange
  Socket IDs.
- **Exit:** log shows completed SRT handshake (both sides) over WT. No data
  plane yet.

### Phase 4 — SRT data plane: one-way TS passthrough (no NAK yet)
- FileIngester reads `fixtures/test.ts`, paces, feeds `(Instant, Bytes)` into
  sender driver.
- Sender packetizes into SRT data packets (5×188 B per datagram), sends via WT.
- Browser wasm receiver ingests data packets, emits messages.
- Browser feeds message bytes into `mpeg2ts-wasm`.
- ACKs flow browser→gateway (gateway processes them, no retransmit yet).
- **Exit:** browser console shows PMT seen, PIDs identified, PES packets at
  correct PTS.

### Phase 5 — SRT reliability: NAK + retransmit + TLPKTL
- Wire up `--sim-loss N` in the gateway datagram driver (random drop with seed).
- Verify browser receiver detects seq gaps, generates NAKs.
- Verify gateway sender retransmits from buffer on NAK receipt.
- Verify TLPKTL drop kicks in under heavy loss.
- **Exit:** under 5% random loss, browser receives complete message stream (no
  missing TS packets); under 20% loss, gracefully degrades (frames dropped but
  no stall beyond next keyframe).

### Phase 6 — WebCodecs video decode
- `decode.ts`: avcC construction (port `build_avcc` logic — either TS or expose
  via `srt-wasm` companion), `VideoDecoder.configure`, feed NALs as
  `EncodedVideoChunk`, draw `VideoFrame` to `<canvas>`.
- Handle decoder reset on PMT/SPS change.
- **Exit:** video plays in Chrome from fixture file. **No fMP4 anywhere.** ⭐
  Main proof of concept.

### Phase 7 — Audio (Opus / AAC)
- `mpeg2ts-wasm` exposes audio PID PES packets.
- `decode.ts`: Opus PES triangle-header strip, `AudioDecoder`,
  `MediaStreamTrackGenerator` → `<audio>` via `AudioContext`. AudioWorklet
  fallback.
- Handle autoplay-gesture requirement.
- **Exit:** audio + video synced from fixture.

### Phase 8 — SRT ingest from OBS
- `ingest/srt.rs`: `srt-tokio` listener, OBS calls in,
  `Stream<Item=(Instant, Bytes)>`.
- Session: clone each OBS message to all browser senders
  (`tokio::sync::broadcast` or per-sender mpsc).
- **Exit:** OBS → Chrome live, sub-second latency. The original goal.

### Phase 9 — Firefox compatibility + hardening
- `--cert-mode mkcert`: load PEM files; client skips
  `serverCertificateHashes`.
- Multi-viewer fanout with session limits + backpressure.
- RTT-aware TLPKTL (use `wt.transport().stats().smoothedRtt` to tune).
- Keyframe-request backchannel: browser detects decoder stall → SRT sender
  triggers OBS keyframe (via `srt-tokio`'s sideband or upstream libSRT API).
- Reconnect, draining, graceful shutdown.
- **Exit:** Firefox plays same stream; multi-viewer works; long-running session
  stable.

## Risks and mitigations

| Risk | Mitigation |
|---|---|
| `srt-protocol` API not wasm-friendly (sync state machine assumptions, `Instant` usage, private fields) | Phase 2 starts with an API survey before writing any glue; if interior types aren't public, wrap at a higher level or fork the crate locally. |
| `srt-protocol` pulls crypto deps into wasm (~300-500KB binary) | Acceptable for a streaming app; or fork Cargo.toml to feature-gate crypto out (we don't use it). |
| `srt-tokio` last released May 2024 | API stable, swap to `shiguredo_srt` if needed. `Ingester` trait isolates the swap. Phase 8 risk only. |
| WT datagram PMTU < SRT default payload (1316 B) | Cap SRT payload at ~1100 B (= 5×188 B TS). Set via `srt-protocol::settings`. Negligible throughput impact. |
| `mpeg2ts` crate API not wasm-friendly | Same mitigation as srt-protocol: Phase 2 API survey first. If unusable, hand-roll (~500 LOC) — fallback documented. |
| Firefox `serverCertificateHashes` unsupported | Phase 9 mkcert path works in all browsers. |
| 2-week cert expiry | Server regenerates on boot; dev workflow "restart, paste hash". |
| SRT-MPEG-TS PTS/PCR clock from OBS | Browser inherits these via the TS bytes; `mpeg2ts` handles parsing. TSBPD uses gateway's send clock, not OBS's — separate concern, surfaced in Phase 8. |
| Audio autoplay blocked | Phase 7 documents gesture requirement; video-only works without. |
| `MediaStreamTrackGenerator` browser support | Phase 7 caniuse check; AudioWorklet fallback specced. |
| Opus-in-MPEG-TS has no standard stream type | Browser-side problem now: `mpeg2ts` may not detect Opus PID. Mitigation: PES-payload sniff or `--assume-audio=opus` flag passed to wasm. |
| This is novel territory (no prior SRT-over-WT OSS art) | Phase 5 sim-loss test gates OBS integration; bugs surface in deterministic environment before real network. |
| Multi-wasm-module init race in browser | Load both wasm modules before WT connect; document in Phase 2. |

## Security note (for README)

Anyone with the cert hash can connect. Default `--cert-mode self` is
**localhost only**. Bind `127.0.0.1`, not `0.0.0.0`, unless an auth layer is
added in Phase 9+. README says so explicitly so no one exposes `:4433` to the
LAN expecting privacy.

## Open questions to resolve in early phases (non-blocking)

1. **Phase 2 API survey** of `srt-protocol::protocol::receiver` — confirm the
   state machine is callable as `handle_packet(bytes, now) -> actions`. If the
   public API is incomplete, decide between (a) upstream PR to
   russelltg/srt-rs, (b) local fork, or (c) wrap-and-reexport pattern in
   `srt-wasm`.
2. **Datagram size negotiation** — `wtransport` exposes
   `Session::max_datagram_size()`. Confirm at runtime and assert it's ≥ ~1100 B;
   bail with a clear error otherwise.
3. **OBS keyframe trigger mechanism** — does `srt-tokio` expose a way to send a
   keyframe request upstream to OBS? If not, Phase 9's keyframe backchannel
   needs a different mechanism (or skips it).
4. **Single viewer ↔ multi-viewer boundary** — Phase 4 implements single viewer
   for simplicity; multi-viewer in Phase 9 reuses the SRT sender per session.
   Confirm no shared mutable state between senders.
