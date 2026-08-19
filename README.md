# WebSRT

[What is WebSRT?](#what-is-websrt) | [The Library](#the-library) | [Features](#features) | [Getting Started](#getting-started) | [Build Instructions](#build-instructions) | [Reference Applications](#reference-applications) | [Architecture](#architecture) | [License](#license)

> **NOTE: NOT PRODUCTION READY.** Experimental: the protocol works, streams
> play, and NAK/retransmit recovers from packet loss — but it has not been
> hardened, audited, or tested at scale.

## What is WebSRT?

**WebSRT is a Rust + WebAssembly library that implements SRT over WebTransport
datagrams** — the same wire format, HSv5 handshake, NAK/ACK/retransmit
semantics, and TSBPD latency management as SRT/UDP, running natively on the
server and inside the browser via WASM. JS is glue only: the browser runs the
*same* `srt-protocol` and `mpeg2ts` Rust crates the gateway does, compiled to
WASM.

It bridges native SRT sources (OBS or any SRT sender) to browsers — and both
directions: browsers can publish too, and a browser can even view one stream
while publishing another.

|                |                                                        |
| -------------- | ------------------------------------------------------ |
| **S**RT        | The real protocol: same wire format, HSv5 handshake, NAK/ACK retransmit, TSBPD |
| **R**ust       | Native server and browser WASM run the same `srt-protocol` state machines |
| **T**ransport  | WebTransport (HTTP/3) datagrams carry the SRT packets — TLS and connection migration included |

```
                  ┌─────── Rust gateway ───────────────┐
 [OBS] --SRT/UDP─▶│ srt-tokio listener (ingest)        │       ┌── Browser (viewer) ──────────────┐
                  │   ↓ (Instant, Bytes)               │       │ JS: WebTransport datagram I/O    │
                  │ broadcaster (broadcast channel)    │       │   ↓ bytes                        │
 [Browser] ─WT───▶│ SrtInitiator (ingest, publish)     │──WT──▶│ WASM: srt-protocol::receiver     │
  (publisher)     │   ↓ (Instant, Bytes)               │       │   ↓ (Instant, Bytes) messages    │
                  │   ↓                                │       │ WASM: mpeg2ts demux              │
                  │ srt-protocol::sender (per viewer)  │       │   ↓ PES / NAL / Opus / PCM        │
                  │   ↓ SRT packets (bytes)            │       │ JS: WebCodecs decode + render    │
                  │ wtransport datagram driver         │       └──────────────────────────────────┘
                  │   ↑ ACK/NAK (bytes)                │
                  └────────────────────────────────────┘
```

This repository also ships **reference applications** built on the library — a
native gateway binary (desktop GUI or headless CLI) and web viewer/publisher
pages. They exercise every part of the library end-to-end and receive many
updates as the library matures, but they are examples, not the product: the
product is the crates. See [Reference applications](#reference-applications).

## The Library

Five artifacts: one native crate, three WASM crates, and a TypeScript player
SDK.

| Crate / package | Target | What it provides |
| --- | --- | --- |
| [`websrt`](crates/websrt) | native (tokio) | `Gateway` builder + run loop, `Broadcaster` fanout, `SrtInitiator`, `Ingester` trait (SRT / file / channel), `StreamRegistry`, `SessionPolicy` hooks, `GatewayLimits`, `Cert` / `CertSource` |
| [`srt-wasm`](crates/srt-wasm) | wasm32 | `SrtReceiver` — the browser-side SRT listener (recv **and** send), driven by `handle_datagram()` / `poll()` / `sendMessage()` |
| [`mpeg2ts-wasm`](crates/mpeg2ts-wasm) | wasm32 | `TsDemuxer` — TS demux emitting PES / PCM / PMT events, plus `DebugSnapshot` stream analysis (CC errors, PCR jitter, NAL breakdown) |
| [`ts-muxer-wasm`](crates/ts-muxer-wasm) | wasm32 | `TsMuxer` — publisher-side TS muxing (H.264/HEVC/AV1 video, Opus audio, raw PCM) |
| [`web/` player SDK](web/src/player) | TypeScript | `mountPlayer()` — framework-agnostic viewer SDK; see [docs/embedding.md](docs/embedding.md) |

### Using `websrt` (native side)

```toml
[dependencies]
websrt = { git = "https://github.com/maxolgi/WebSRT.git", branch = "master" }
```

`websrt` builds on two patched forks, wired via `[patch.crates-io]`:
[`maxolgi/srt-rs`](https://github.com/maxolgi/srt-rs) (12 patches: WASM
compat, skip-induction handshake, stats population, buffer-underflow fixes,
RTT seeding, CC-aware retransmit — all documented in `AGENTS.md`, drafted for
upstream in `PR.md`) and [`maxolgi/mpeg2ts`](https://github.com/maxolgi/mpeg2ts)
(1 patch: unknown-PID tolerance for mid-stream joins).

**Note:** cargo only honors `[patch]` sections from the *workspace root*, so a
git dependency does not carry WebSRT's patches with it — mirror them in your
own root `Cargo.toml`:

```toml
[patch.crates-io]
srt-protocol = { git = "https://github.com/maxolgi/srt-rs.git", branch = "main" }
srt-tokio = { git = "https://github.com/maxolgi/srt-rs.git", branch = "main" }
mpeg2ts = { git = "https://github.com/maxolgi/mpeg2ts.git", branch = "master" }
```

Embeddable gateway in ~30 lines:

```rust
use websrt::cert::{Cert, CertSource};
use websrt::hooks::{auth_token_policy, chain, path_policy};
use websrt::ingest::srt::SrtIngester;
use websrt::limits::GatewayLimits;
use websrt::Gateway;

# async fn run() -> anyhow::Result<()> {
let cert = Cert::build(CertSource::SelfSigned {
    sans: vec!["localhost".into()],
}).await?;

let gateway = Gateway::builder()
    .bind_addr("127.0.0.1:4433".parse::<std::net::SocketAddr>()?)
    .identity(cert.identity.clone_identity())
    .latency_ms(1000)
    .max_viewers(16)
    // Pluggable accept policy: chain built-ins (path / origin allowlist /
    // auth token) or implement SessionPolicy for per-stream tokens, JWTs,
    // rate limits… Replaces the built-in checks when set.
    .session_policy(chain(
        path_policy("/wt".into()),
        auth_token_policy("s3cret".into()),
    ))
    // Per-IP (IPv6 collapsed to /64) + global session caps, timeouts.
    .limits(GatewayLimits::builder().max_sessions_per_ip(5).build()?)
    .build()?;

// Live stats for your own metrics endpoint (cheap, no I/O):
let _stats = gateway.stats_handle();

// Deferred ingester: connect OBS in the background
let source = gateway.source_handle();
tokio::spawn(async move {
    let ingester = SrtIngester::bind(
        "0.0.0.0:9000",
        None,
        std::time::Duration::from_millis(120),
        None,
    ).await.unwrap();
    source.publish_stream("default", ingester);
});

gateway.run(async {
    let _ = tokio::signal::ctrl_c().await;
}).await?;
# Ok(())
# }
```

`Ingester` is the input abstraction: implement `next_message()` returning
`(Instant, Bytes)` TS messages and the library handles fanout, per-viewer SRT
senders, retransmit, and backpressure. Built-in: `SrtIngester` (OBS, listener
or caller, with reconnect), `SrtListenerService` (multi-publisher SRT accept
loop), `FileIngester` (fixture loop with real-time pacing), `ChannelIngester`
(mpsc-backed — this is what browser publishers feed).

### Browser side (WASM)

The WASM crates are not published; consume them by cloning this repo and
running `wasm-pack build --target web` (see
[Build Instructions](#build-instructions)), then importing the generated
`pkg` output. The browser is the SRT **listener**: it feeds raw WebTransport
datagrams into the state machine and executes the returned actions.

```js
import init, { SrtReceiver } from './wasm/srt-wasm/srt_wasm.js';

await init();                                   // wasm-pack --target web output
const epoch = performance.now();
const nowUs = () => (performance.now() - epoch) * 1000;

// TSBPD latency 120 ms; RTT seeded from QUIC's smoothed RTT
// (await wt.getStats()).smoothedRtt — accurate cold-start retransmit timing.
const rx = SrtReceiver.newWithLatencyAndRtt(120, quicSmoothedRttMs);

// Incoming WebTransport datagram → SRT state machine → actions
function onDatagram(bytes) {
  for (const a of rx.handle_datagram(bytes, nowUs())) dispatch(a);
}
// ~5 ms tick advances timers (TSBPD release, ACK, keepalive)
setInterval(() => { for (const a of rx.poll(nowUs())) dispatch(a); }, 5);

function dispatch(a) {
  switch (a.kind) {
    case 0: wtDatagramWriter.write(a.takeData()); break;   // ACK / NAK / handshake
    case 1: onTsMessage(a.takeData(), a.schedUs); break;   // TSBPD-released TS message
    case 2: onHandshakeComplete(); break;                  // data plane is live
    case 4: onClose(); break;
  }
  a.free();  // wasm-bindgen structs are freed manually
}
```

Demux and mux are equally small surfaces:

```js
import { TsDemuxer } from './wasm/mpeg2ts-wasm/mpeg2ts_wasm.js';
const demux = new TsDemuxer();
for (const ev of demux.feed(tsBytes)) { /* PES / PCM / PMT / RA events */ }

import { TsMuxer } from './wasm/ts-muxer-wasm/ts_muxer_wasm.js';
const muxer = new TsMuxer();                    // publisher side
muxer.push_video(nalBytes, ptsUs, dtsUs, isKeyframe);
muxer.push_pcm(f32Samples, ptsUs);              // SMPTE 302M audio
```

### Player SDK (TypeScript)

For embedding the **browser-side player** (canvas + WebCodecs SDK, including
the direct `pcmPort` PCM delivery API), see
[`docs/embedding.md`](docs/embedding.md) — a guide covering the `mountPlayer()`
SDK, config options, and event surface.

### Feature flags

The `sim-loss` feature enables a probabilistic datagram dropper for testing
NAK/retransmit (data packets only; control packets always pass). Without the
feature, the `rand` dependency is excluded.

```toml
[dependencies]
websrt = { git = "https://github.com/maxolgi/WebSRT.git", branch = "master", features = ["sim-loss"] }
```

## Features

- **Dumb SRT repeater.** The gateway terminates OBS's SRT connection, takes
  the resulting `(Instant, Bytes)` messages, and re-originates them as a new
  SRT sender to each browser. TS bytes are never modified server-side.
- **Browser publishing.** A browser can publish upstream via WebTransport.
  Published streams are fanned out to viewers exactly like OBS streams —
  same broadcaster, same per-viewer SRT sender. The publisher muxes TS
  locally (`ts-muxer-wasm`), sends SRT over WT datagrams to the gateway.
- **Each browser gets its own SRT sender instance** (independent seq numbers,
  independent retransmit buffer) via `tokio::sync::broadcast` fanout.
- **Browser runs the same Rust state machines** the gateway does, compiled to
  WASM.
- **First-wins stream occupancy.** A live stream name cannot be replaced by
  another publisher (browser or SRT); challengers are rejected with 403.
- **SRT crypto disabled** between gateway and browser. WebTransport TLS
  replaces it. The OBS-to-gateway link supports optional AES encryption via
  `--srt-passphrase`.
- **Multi-codec support.** H.264 (avcC + SPS parsing), HEVC/H.265 (hvcC +
  VPS/SPS/PPS), and AV1 (OBU Sequence Header parsing) video; Opus, AAC/ADTS,
  and uncompressed PCM (SMPTE 302M) audio. The browser publisher supports
  H.264 and AV1 encode via WebCodecs `VideoEncoder`, plus Opus and raw-PCM
  audio.
- **PCM / SMPTE 302M audio.** AES3 (s302m) PES payloads are decoded to f32
  samples inside the WASM demuxer and delivered as per-PID `pcm` messages
  tagged with channel count, PTS, and the SRT TSBPD release deadline.
  Audio-only streams (no video PID) and multi-PID stacks (broadcaster-style
  N×stereo) work in both directions. Embedders can receive raw PCM on a
  transferred `MessagePort` (`pcmPort` option) — samples flow
  worker → consumer directly, bypassing the main-thread hop.
- **Web Worker architecture.** The SRT receiver and TS demuxer run in a Web
  Worker off the main thread. The poll loop is adaptive: it sleeps until the
  protocol's next timed event (SRT `WaitForData`), clamped to sane bounds,
  with a dedicated timer worker for fine-grained ticks. Only WebCodecs decode
  and canvas rendering happen on the main thread.
- **PTS-paced video presentation.** Decoded frames are queued in a small
  bounded ring and drawn on `requestAnimationFrame` when their PTS is due,
  measured against a wall-clock ↔ PTS mapping that resets on large gaps
  (seek, stream restart, backgrounded tab). SRT's TSBPD paces delivery; PTS
  pacing absorbs downstream bursts (decoder reorder, worker→main batching) so
  the canvas updates at the source frame rate. Opt out via the player SDK's
  `setRenderPacing(false)` to fall back to "latest frame only" skip-ahead.
- **Bounded audio buffering.** The AudioWorklet paths use fixed-size ring
  buffers with drop-oldest and skip-ahead to prevent latency accumulation.

## Getting Started

| | | |
|:---:|:---:|:---:|
| [The Library](#the-library) | [Reference gateway quickstart](#reference-applications) | [Embedding the player](docs/embedding.md) |
| Crate overview + Rust and JS examples | Run the gateway app with OBS or the test fixture | `mountPlayer()` SDK: options, events, `pcmPort` |
| [Build Instructions](#build-instructions) | [Architecture](#architecture) | [SRT-over-QUIC draft](https://haivision.github.io/srt-rfc/draft-sharabayko-srt-over-quic.html) |
| Toolchain setup, `./build.sh` menu | Data flow, browser pipeline | The IETF draft WebSRT implements |

## Build Instructions

### Prerequisites

- Rust stable (>=1.75), with `wasm32-unknown-unknown` target and `wasm-pack`
- Node.js >=18 (build-time only — bundles the web UI that the gateway serves)
- ffmpeg (only for the live publisher scripts in `fixtures/`)
- System C/C++ build tools: `build-essential` / `cmake` / `pkg-config`
  (Linux/macOS), or **Visual Studio with the "Desktop development with C++"
  workload** (Windows; supplies the MSVC compiler + Windows SDK that `ring` /
  `aws-lc-sys` compile via `cmake`). The `x86_64-pc-windows-msvc` Rust toolchain
  (the default on Windows) auto-discovers it — no manual env setup.

#### One-command install (Debian/Ubuntu, Fedora, Arch, macOS)

```bash
./install-prereqs.sh
```

Detects what's already installed and only installs what's missing. Run
`./install-prereqs.sh --check` to verify without installing. Can also be
curl'd directly on a fresh machine before cloning the repo:

```bash
curl -sSf https://raw.githubusercontent.com/maxolgi/WebSRT/master/install-prereqs.sh | bash
```

#### Manual setup

```bash
# rustup + stable toolchain + wasm32 target + wasm-pack (user-local, ~/.cargo)
curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh -s -- -y --profile minimal
source "$HOME/.cargo/env"
rustup target add wasm32-unknown-unknown
curl https://rustwasm.github.io/wasm-pack/installer/init.sh -sSf | sh

# Node.js >= 18, ffmpeg, and C/C++ build tools via your system package manager.

# one-time: build all 3 WASM modules, copy to web/wasm/, install web-UI deps
./build.sh setup

# one-time: bundle the web UI → web/dist/ (served by the gateway binary)
./build.sh web build
```

`./build.sh setup` runs the equivalent of:

```bash
mkdir -p web/wasm/srt-wasm web/wasm/mpeg2ts-wasm web/wasm/ts-muxer-wasm
(cd crates/srt-wasm     && wasm-pack build --target web --release)
cp crates/srt-wasm/pkg/* web/wasm/srt-wasm/
(cd crates/mpeg2ts-wasm && wasm-pack build --target web --release)
cp crates/mpeg2ts-wasm/pkg/* web/wasm/mpeg2ts-wasm/
(cd crates/ts-muxer-wasm && wasm-pack build --target web --release)
cp crates/ts-muxer-wasm/pkg/* web/wasm/ts-muxer-wasm/
(cd web && npm install)
```

Run `./build.sh --help` for the full menu (per-crate WASM builds, gateway,
library, web bundle, check, test, clean, etc.).

#### Building on Windows

The project builds natively on Windows (verified on Rust 1.96 MSVC + VS 2026).
`build.sh` is bash, so either run it through **Git Bash** unchanged, or run the
raw commands below in PowerShell/cmd. Two Windows-specific notes:

- **Visual Studio C++ tools are required** — install Visual Studio (2022+) with
  the *"Desktop development with C++"* workload. The MSVC toolchain
  (`x86_64-pc-windows-msvc`, Rust's Windows default) needs it for `ring` /
  `aws-lc-sys`. Rust auto-discovers the linker; no `vcvarsall` setup needed.
- **PowerShell blocks `npm.ps1`** by default execution policy — use `npm.cmd`
  (and `npx.cmd`) instead of `npm` / `npx`.

```powershell
# Prerequisites (one-time)
rustup target add wasm32-unknown-unknown
# Install wasm-pack: https://rustwasm.github.io/wasm-pack/installer/
# Install Visual Studio with the C++ workload, then:

# Build all 3 WASM crates + copy to web/wasm/
foreach ($c in 'srt-wasm','mpeg2ts-wasm','ts-muxer-wasm') {
    wasm-pack build --target web --release "crates/$c"
    New-Item -ItemType Directory -Force "web/wasm/$c" | Out-Null
    Copy-Item "crates/$c/pkg/*" "web/wasm/$c/" -Force
}

# Web-UI deps + bundle
npm.cmd install --prefix web
npm.cmd run build --prefix web

# Gateway binary (release) → target\release\websrt-gateway.exe
cargo build --release -p websrt-gateway
.\target\release\websrt-gateway.exe             # GUI: only the egui window, no console window
.\target\release\websrt-gateway.exe --no-gui    # headless: reattaches to the terminal for log output
```

Everything else in this README (`cargo run`, CLI flags, cert modes, browser
flow) works identically on Windows. The only Linux-only steps are the
supervisord deployment (`./build.sh restart`, `websrt.conf`) and the
`fixtures/stream*.sh` publishers (bash+ffmpeg scripts — adapt them or run OBS
instead).

The test fixture (`fixtures/test.ts`, ~45 KB, H.264+Opus, 10 s loop) is committed
to the repo — no generation step needed. `fixtures/stream.sh` is a live ffmpeg
publisher (NVENC/VAAPI) that streams to the gateway's SRT listener instead of
writing a file.

### `./build.sh` menu

```bash
./build.sh setup                # one-time: WASM + web-UI deps (run after fresh clone)
./build.sh wasm                 # rebuild all 3 WASM crates + copy to web/wasm/
./build.sh wasm srt             # rebuild just srt-wasm + copy
./build.sh gateway              # cargo build --release -p websrt-gateway
./build.sh gateway --sim-loss   # add the sim-loss feature
./build.sh lib                  # cargo build --release -p websrt (the library)
./build.sh web build            # bundle the web UI → web/dist/
./build.sh check                # cargo check + tsc --noEmit
./build.sh test                 # cargo test --workspace + node web/smoke.mjs
./build.sh srt-protocol         # rule 1: rebuild gateway + srt-wasm after editing forked srt-rs
./build.sh restart              # sudo supervisorctl restart websrt (production only)
./build.sh clean                # rm -rf web/wasm web/dist target
./build.sh all                  # full clean rebuild: clean → setup → gateway → web build
```

Raw form (what the script runs):

```bash
# Demo gateway binary (release, with sim-loss feature)
cargo build --release -p websrt-gateway --features sim-loss

# Library only (for use as a dependency)
cargo build --release -p websrt

# WASM crates — must rebuild + copy to web/ after changes
(cd crates/srt-wasm && wasm-pack build --target web --release)
cp crates/srt-wasm/pkg/* web/wasm/srt-wasm/
(cd crates/mpeg2ts-wasm && wasm-pack build --target web --release)
cp crates/mpeg2ts-wasm/pkg/* web/wasm/mpeg2ts-wasm/
(cd crates/ts-muxer-wasm && wasm-pack build --target web --release)
cp crates/ts-muxer-wasm/pkg/* web/wasm/ts-muxer-wasm/

# Web UI bundle (debug builds of the gateway serve dist/ from disk;
# release builds embed it at compile time)
(cd web && npm run build)

# TypeScript typecheck (no emit)
cd web && npx tsc --noEmit

# Live publishers (need ffmpeg; h264 uses NVENC, av1 uses VAAPI)
./fixtures/stream.sh h264|av1
./fixtures/stream_pcm.sh [mono|stereo|surround|<channel_count>]
```

### Critical build order

1. Forked `srt-protocol` (`maxolgi/srt-rs`) change → run `./build.sh srt-protocol`
   (rebuilds BOTH the gateway binary AND srt-wasm + copies pkg to `web/wasm/`).
2. Changing only `web/src/*.ts` / `*.tsx` → rebuild the bundle
   (`./build.sh web build`). Debug `cargo run` picks it up on page reload;
   release builds re-embed at compile time, so rebuild the binary too.
3. Changing `crates/srt-wasm/src/lib.rs` → `./build.sh wasm srt` + browser reload.
4. Changing `crates/mpeg2ts-wasm/` or `crates/ts-muxer-wasm/` →
   `./build.sh wasm mpeg2ts` (or `ts-muxer`) + browser reload.
5. Changing `crates/websrt/` (library) → `./build.sh gateway` +
   `./build.sh restart` (production only; dev just reruns the binary).

## Reference Applications

The repo ships two reference applications built **entirely** on the library.
Like SRT's `srt-live-transmit`, they are provided for instructional purposes —
demonstrations of the crate surfaces, not ends in themselves.

- **`websrt-gateway`** — native binary wrapping `Gateway`: CLI parse, cert
  setup, ingester wiring, health endpoint. Launches a desktop GUI (eframe/egui)
  by default — config form (persisted to `~/.config/websrt/gateway-config.json`),
  Start/Stop, live stats, scrolling log panel. `--no-gui` for headless CLI mode
  (required under supervisord); auto-falls-back to CLI on headless servers.
  Single-binary deployment: the built-in HTTPS web server embeds the web UI at
  compile time (`rust-embed`), so the release binary serves the viewer UI with
  no external file server. Debug builds serve `web/dist/` from disk so UI
  iterations don't require recompiling.
- **`web/` pages** — viewer page (player SDK + optional debug overlay with
  8 analysis tabs), publisher page (screen capture → WebCodecs encode →
  `ts-muxer-wasm` → SRT over WT), and a stripped-down simple viewer.

### Run with the test fixture (no OBS required)

```bash
cargo run -p websrt-gateway
```

The gateway launches a GUI window by default (config form + live stats + logs).
Add `--no-gui` for headless mode. The gateway writes `web/public/cert-hash.js`
on startup (the self-signed cert DER hash for the browser).

Open `https://127.0.0.1:5173` — the gateway's built-in HTTPS server serves the
web UI from the embedded bundle (or from `web/dist/` on disk in debug builds).
The cert hash is auto-loaded from `cert-hash.js` — no manual entry needed.
Click **connect**.

If the page returns "web UI not built", run `./build.sh web build` once and
reload.

### Run with OBS

```bash
cargo run -p websrt-gateway -- --input srt --srt-port 9000
```

In OBS, add a Media Source (or your camera), then add an SRT output:

- Mode: `Call`
- IP: `127.0.0.1`
- Port: `9000`
- No passphrase needed by default. Add `--srt-passphrase <key>` to enable AES
  encryption on the OBS leg (10–79 chars).

If OBS disconnects (crash, restart, network drop), the gateway automatically
waits for a reconnection — no restart required. Existing browser viewers will
auto-reconnect via the exponential-backoff logic.

### Caller mode (OBS is the listener)

```bash
cargo run -p websrt-gateway -- --input srt --srt-mode caller --srt-call 192.168.1.50:9000
```

### Browser publishing (browser-to-browser streaming)

A browser can publish a stream to the gateway, which fans it out to other
browsers. The publisher encodes video (WebCodecs `VideoEncoder`) and audio,
muxes to MPEG-TS (`ts-muxer-wasm`), and sends via SRT-over-WebTransport.
Viewers connect to `?stream=<name>` as usual.

The publish URL pattern is `?publish=<name>` — the gateway creates a
broadcaster for that stream name, and viewers subscribe via `?stream=<name>`.
A live stream name cannot be taken over by another publisher (first-wins);
challengers are rejected with 403.

```bash
cargo run -p websrt-gateway  # listener mode, any SRT source
```

Publisher connects to `https://127.0.0.1:4433/wt?publish=mystream`.
Viewer connects to `https://127.0.0.1:5173/?stream=mystream`.

The publisher-side SRT uses the same forked `srt-protocol` compiled to WASM
(`srt-wasm`). The gateway's publish path runs an `SrtInitiator` (SRT receiver
over WT datagrams) and releases TSBPD-paced messages to the broadcaster.

**Note:** Browser publishing works best with low TSBPD latency (20–120 ms).
High latency (e.g., 300 ms) is fine for viewing but adds unnecessary buffering
on the publish side. The gateway services publish sessions on every ticker
cycle (every 2 ms) to keep the TSBPD release path responsive.

### PCM audio (SMPTE 302M)

Uncompressed AES3 audio rides MPEG-TS as SMPTE 302M (`s302m`). The WASM
demuxer decodes s302m PES payloads to interleaved f32 samples in Rust; the
worker posts per-PID `pcm` messages (`pid`, `channelCount`, `samples`, `pts`,
plus the SRT TSBPD deadline and actual release time for pacing telemetry).
The reference viewer plays them via an AudioWorklet (`PcmPlayer`); the debug
overlay's AudioTab adds per-PID meters (peak/LUFS/phase/scope/spectrum) and
release-pacing rows.

Publishing side: `ts-muxer-wasm` exposes `push_pcm` / `push_pcm_pid` so a
browser (or embedder) can originate PCM audio — audio-only (no video PID) and
multi-PID (N×stereo) layouts included.

Test source (needs an ffmpeg built with `--enable-gpl` for the s302m encoder):

```bash
./fixtures/stream_pcm.sh             # stereo two-tone sine
./fixtures/stream_pcm.sh surround    # 5.1 bed on one PID
./fixtures/stream_pcm.sh 128         # 64 stereo PIDs at different frequencies
```

### Simulated packet loss

```bash
cargo run -p websrt-gateway --features sim-loss -- --sim-loss 5    # 5% random drop of data datagrams
cargo run -p websrt-gateway --features sim-loss -- --sim-loss 20   # 20% — NAK/retransmit recovers
```

Only data packets are dropped; control packets (handshake, ACK, NAK, KeepAlive)
always pass through so the SRT reliability machinery stays functional.

### Multi-viewer

Open more browser tabs — each gets its own independent SRT sender. Viewer cap
defaults to 16 (enforced in `Broadcaster::subscribe`).

### CLI reference

```
websrt-gateway [OPTIONS]

Options:
      --no-gui                   Skip the GUI and run in headless CLI mode (original behavior)
      --no-web                   Disable the built-in HTTPS web server
      --web-port <WEB_PORT>      HTTPS port for the built-in web server (0 to disable) [default: 5173]
      --web-bind <WEB_BIND>      Bind address for the built-in HTTPS web server [default: 127.0.0.1]
      --web-root <WEB_ROOT>      Root directory for web files (auto-detected: web/dist → web if unset)
      --input <INPUT>            Input source [default: file] [possible values: file, srt]
      --fixture <FIXTURE>        Path to .ts fixture (when --input file) [default: fixtures/test.ts]
      --fixture-duration <DUR>   Duration of the fixture in seconds (for real-time pacing) [default: 10.0]
      --srt-mode <SRT_MODE>      SRT connection mode [default: listener] [possible values: listener, caller]
      --srt-port <SRT_PORT>      SRT listen port (when --input srt --srt-mode listener) [default: 9000]
      --srt-call <SRT_CALL>      Address to dial when --srt-mode caller (e.g. 192.168.1.3:1234)
      --srt-streamid <STREAMID>  SRT stream id. Listener mode: only accept connections matching this id.
                                 Caller mode: sent to OBS during connection
      --wt-port <WT_PORT>        WebTransport listen port [default: 4433]
      --bind <BIND>              Bind address for WebTransport [default: 127.0.0.1]
      --cert-mode <CERT_MODE>    Cert strategy [default: self] [possible values: self, mkcert]
      --cert-pem <CERT_PEM>      PEM cert path (mkcert mode)
      --key-pem <KEY_PEM>        PEM key path (mkcert mode)
      --latency <LATENCY>        SRT TSBPD latency for OBS input, in milliseconds [default: 120]
      --max-bandwidth <KBPS>     Max SRT send bandwidth in kbps (0 = unlimited). Set to ~125% of
                                 stream bitrate, e.g. --max-bandwidth 250000 for a 200 Mbps stream
                                 [default: 0]
      --srt-passphrase <PASS>    SRT encryption passphrase for the OBS leg (10–79 chars)
      --health-port <PORT>       Health/metrics HTTP port (0 to disable) [default: 0]
      --health-bind <BIND>       Bind address for the HTTP health/metrics server [default: 127.0.0.1]
      --auth-token <TOKEN>       Auth token for viewer connections. If set, browsers must pass
                                 ?token=<value>
      --max-viewers <N>          Maximum concurrent viewers per stream [default: 16]

Only with the `sim-loss` feature:
      --sim-loss <SIM_LOSS>      Simulate N% random datagram loss (0-100). 0 disables [default: 0]
      --sim-seed <SIM_SEED>      RNG seed for sim-loss (deterministic by default) [default: 42]
```

### Certificate modes

#### Self-signed (default, `--cert-mode self`)

Self-signed ECDSA P-256 certificate with SANs `localhost`, `127.0.0.1`, `::1`.
Generated once, then persisted to `~/.config/websrt/gateway-cert.pem` +
`gateway-key.pem` and reused across restarts so the browser's cert exception /
hash pinning stays stable. Delete those files to force regeneration. The DER
SHA-256 hash is written to `web/public/cert-hash.js` at startup. The browser
passes it to `serverCertificateHashes` in the WebTransport options, bypassing
the normal PKI validation. Chrome/Edge only (Firefox does not support
`serverCertificateHashes`).

Delete the persisted cert files and restart to rotate the hash.

#### mkcert (`--cert-mode mkcert`)

Uses PEM files generated by [mkcert](https://github.com/FiloSibille/mkcert).
The browser validates via normal PKI (mkcert installs a local CA). Works with
Firefox. `cert-hash.js` is set to `null`.

```bash
mkcert -install
mkcert -cert-file certs/cert.pem -key-file certs/key.pem localhost 127.0.0.1 ::1
cargo run -p websrt-gateway -- --cert-mode mkcert --cert-pem certs/cert.pem --key-pem certs/key.pem
```

See `certs/README.md` for details.

## Architecture

### Data flow

```
OBS ──SRT/UDP──► SrtIngester ──► Broadcaster (broadcast channel, depth 32768)
                                      ▲
Browser ──WT──► SrtInitiator ────────┘  (publish path: WT dgrams → SRT receiver → ReleaseData)
(publisher)     (recv_pump + ticker)

                                        │
                            ┌───────────┴───────────────┐
                            ▼                           ▼
                     BrowserSession A             BrowserSession B
                     ├── recv_pump                ├── recv_pump
                     │   (WT dgram → SrtInitiator)│   (WT dgram → SrtInitiator)
                     └── ticker (shared)          └── ticker (shared)
                         (viewer.recv →               (viewer.recv →
                          SrtInitiator → WT dgram)     SrtInitiator → WT dgram)
```

The gateway is a **dumb SRT repeater**: it terminates OBS's SRT/UDP connection
(or a browser's SRT-over-WT publish session), re-originates TS bytes as a new
SRT sender to each browser over WebTransport datagrams. TS bytes are never
inspected server-side.

Each browser session runs a **recv_pump** task (drains incoming WT datagrams —
ACK/NAK from browser — into the SRT initiator state machine) and is serviced by
a **single centralized ticker** (one task drives all sessions' SRT state
machines every ~2 ms, eliminating N separate timer tasks). The ticker pushes
TS messages from each viewer's broadcast subscriber into the initiator and
sends resulting SRT packets as WT datagrams.

### Browser pipeline

```
                        main thread                    │   Web Worker
                                                       │
WT datagram ──────────────────────────────────────────►│ SrtReceiver (WASM)
   (batched up to 16, fresh clock per datagram)        │   ↓ TSBPD-paced
                                                       │ SrtAction::DeliverMessage
                                                       │   ↓ raw TS bytes
                                                       │ Demuxer (WASM, mpeg2ts)
                                                       │   ↓ PES packets / PCM
  ◄────────────────── postMessage ────────────────────│ (pid, pts, payload)
  │                                                   │     (s302m decoded to f32
  │                                                   │      in the worker)
  ├── VideoPipeline                                   │
  │   H.264 SPS parse → avcC → VideoDecoder           │
  │   ↓ VideoFrame                                    │
  │   CanvasRenderer (PTS-paced, rAF-driven)          │
  │                                                   │
  ├── OpusAudioPipeline / AacAudioPipeline            │
  │   AudioDecoder → MediaStreamTrackGenerator        │
  │   or AudioWorklet (Firefox fallback)              │
  │                                                   │
  ├── PcmPlayer (s302m) — AudioWorklet ring           │
  │   or pcmPort: raw PCM → embedder's MessagePort    │
  │                                                   │
  └── datagramWriter (ACK/NAK → WT)                   │
```

The SRT receiver and TS demuxer run in a **Web Worker** (`worker.ts`) to keep
the main thread free for decoding and rendering. The worker's tick is
**adaptive**: the SRT state machine reports how long until its next timed
event (`WaitForData` actions) and the loop sleeps exactly that long (clamped),
using a dedicated timer worker for sub-millisecond ticks — no fixed 5 ms
quantization on audio release. Incoming datagrams are batched (up to 16) but
each gets a **fresh timestamp**, so packets processed late in a batch don't
see a stale clock. Outgoing messages (PES packets, PCM batches, stats, logs)
coalesce into a single batched `postMessage` to the main thread; PCM on a
`pcmPort` bypasses the main thread entirely.

**Video presentation:** Decoded frames are queued in a small bounded ring and
drawn on `requestAnimationFrame` when their PTS is due (PTS-paced presentation,
on by default). A wall-clock ↔ PTS mapping established on the first frame and
reset on large gaps (seek, restart, backgrounded tab) gates each draw. Frames
that missed their slot are dropped as late (the newest is always kept so the
canvas never freezes). Opt out via `setRenderPacing(false)` to fall back to
"latest frame only" skip-ahead drawing.

**Audio output:** On Chrome, `MediaStreamTrackGenerator` provides implicit
pacing for decoded codecs. The AudioWorklet paths (Firefox fallback, and the
s302m PCM player) use bounded ring buffers with drop-oldest and skip-ahead
when buffered data exceeds the playout target.

**Backpressure:** Both video and audio decoders check `decodeQueueSize` before
submitting new chunks. Video skips delta frames when queue depth > 8; audio
skips when queue depth > 20. Keyframes always pass to allow resync.

## Standards alignment

WebSRT implements [draft-sharabayko-srt-over-quic](https://haivision.github.io/srt-rfc/draft-sharabayko-srt-over-quic.html):

- **§4.2 Packet integrity** — satisfied. Each SRT packet is sent as exactly one WebTransport datagram; the underlying transport preserves packet boundaries.
- **§4.3 Connection establishment** — 1-RTT handshake. Because WebTransport already provides TLS-level authentication and return-routability, the SRT induction phase (whose only purpose is DoS protection via cookie) is redundant. WebSRT skips induction and runs a 2-packet handshake: the gateway sends CONCLUSION directly, the browser responds with CONCLUSION-RESP. This saves one RTT (~50-200 ms) on every viewer join.
- **§4.5 Congestion control** — transport-aware retransmit decisions. SRT's RTT is seeded from QUIC's smoothed RTT (`wtransport::Connection::rtt()` on the gateway, `WebTransport.getStats().smoothedRtt` on the browser). The SRT sender skips NAK-triggered retransmits whose predicted arrival time exceeds the TSBPD deadline, preventing wasted bandwidth on packets the receiver will drop as too-late.
- **§4.7 Connection migration** — inherited from QUIC via WebTransport. Mobile viewers can hand off between networks (cellular → WiFi) without rejoining the stream.
- **§4.8 Datagram vs H3 Datagram** — WebTransport uses H3 Datagram semantics, the load-balancer-compatible choice the draft recommends.

## Production deployment

The gateway runs under supervisord in production (`--no-gui` is required in
the supervisord config).

### Supervisord config

Config file `websrt.conf` (repo root) is deployed to
`/etc/supervisor/conf.d/`:

```ini
[program:websrt]
command=/opt/WebSRT/target/release/websrt-gateway --no-gui --input srt --srt-mode listener --srt-port 9000 --bind 0.0.0.0 --web-bind 0.0.0.0 --latency 1000 --health-port 9090
directory=/opt/WebSRT
autostart=true
autorestart=true
startretries=3
stdout_logfile=/var/log/websrt/gateway.out.log
stderr_logfile=/var/log/websrt/gateway.err.log
stdout_logfile_maxbytes=5MB
stdout_logfile_backups=3
stderr_logfile_maxbytes=5MB
stderr_logfile_backups=3
environment=RUST_LOG="debug"
```

A stock variant ships in `packaging/websrt.supervisor.conf` for packaged
installs (binary at `/usr/bin/websrt-gateway`, working dir `/var/lib/websrt`);
`packaging/debian/` holds the Debian package scaffolding.

### Managing the service

```bash
# After rebuilding the binary:
sudo supervisorctl reread && sudo supervisorctl update && sudo supervisorctl restart websrt

# Check status:
sudo supervisorctl status websrt

# Tail logs:
tail -f logs/gateway.err.log
```

On boot, the gateway writes `web/public/cert-hash.js` containing the cert hash
(for self-signed mode) or `null` (for mkcert mode). The browser page loads this
script automatically. In packaged deployments the web server serves
`cert-hash.js` dynamically, so the on-disk copy is only consumed by the
bundled UI in dev.

## Testing

### Dev binaries

```bash
# SRT handshake + TS continuity-counter probe (tests NAK/retransmit under sim-loss)
cargo run -p websrt-gateway --bin wt_hs_probe

# Sends fixture over SRT to test ingester without real OBS
cargo run -p websrt-gateway --bin mock_obs

# WT datagram round-trip test
cargo run -p websrt-gateway --bin wt_echo_client
```

### Node tests (no browser needed)

```bash
# Both WASM modules end-to-end
node web/smoke.mjs

# PCM round-trip + MessagePort handoff semantics
node web/pcm-roundtrip.mjs

# PCM delivery jitter: worker→main→consumer relay vs direct MessagePort
node web/pcm-port-bench.mjs
```

### Manual OBS test

1. Start gateway: `cargo run -p websrt-gateway -- --input srt --srt-port 9000`
2. Open `https://127.0.0.1:5173`, click connect
3. In OBS: SRT output to `127.0.0.1:9000`, mode `caller`
4. Kill OBS (Ctrl-C) — gateway should log "waiting for reconnect"
5. Restart OBS — gateway reconnects, browser auto-reconnects

## Repo layout

```
WebSRT/
  Cargo.toml                  # workspace (5 crates, 2 forked deps via [patch.crates-io])
  Cargo.lock
  AGENTS.md                   # build commands, architecture, gotchas
  build.sh                    # build orchestrator (./build.sh --help for the menu)
  install-prereqs.sh          # toolchain installer (./install-prereqs.sh --check to verify)
  websrt.conf                 # supervisord config (production)
  packaging/                  # Debian package scaffolding + packaged supervisord config
  docs/
    embedding.md              # mountPlayer() SDK: embedding the browser player, pcmPort API
  fixtures/
    stream.sh                 # live ffmpeg publisher (h264 NVENC / av1 VAAPI) → SRT 9000
    stream_pcm.sh             # live SMPTE 302M PCM publisher (mono…N×stereo PIDs) → SRT 9000
    test.ts                   # committed fixture (~45 KB, 10 s H.264+Opus loop)
  certs/
    README.md                 # mkcert setup instructions
  LICENSE                     # MPL-2.0
  crates/
    websrt/                   # library crate: SRT-over-WebTransport gateway core
      src/
        lib.rs                # pub re-exports + crate docs
        gateway.rs            # Gateway builder: WT accept loop, session spawn, fanout, health
        session.rs            # per-browser session: recv_pump (ticker drives the sender half)
        registry.rs           # centralized SessionRegistry + 2 ms ticker (replaces per-session sender_pump)
        stream_registry.rs    # multi-stream name → Broadcaster map (?stream= / ?publish=)
        srt_sender.rs         # SrtInitiator: wraps srt-protocol Connect → DuplexConnection
        broadcaster.rs        # broadcast fanout with alive-flag + per-stream viewer cap
        nocc.rs               # QUIC congestion-control bypass for the WT listener
        cert.rs               # self-signed / mkcert / in-memory PEM cert management
        hooks.rs              # pluggable SessionPolicy (path/origin/auth-token, chainable)
        limits.rs             # GatewayLimits: per-IP (/64) + global session caps, timeouts
        ingest/
          mod.rs              # Ingester trait + TsMessage type
          channel.rs          # ChannelIngester: mpsc-backed ingester (browser publish path)
          srt.rs              # SrtIngester: srt-tokio listener/caller with reconnect
          srt_listener.rs     # SrtListenerService: multi-publisher SRT accept loop
          file.rs             # FileIngester: fixture loop with real-time pacing
          continuity.rs       # TsContinuityChecker: read-only MPEG-TS CC gap probe
    websrt-gateway/           # reference application: native GUI + CLI wrapper around the library
      src/
        main.rs               # CLI parsing, cert persistence, cert-hash.js writing, Gateway::run()
        gui.rs                # eframe/egui app: config form (persisted), Start/Stop, stats, logs
        log_buffer.rs         # LogBuffer ring buffer + tracing writer for the GUI log panel
        web_server.rs         # built-in HTTPS server, web/dist/ embedded via rust-embed
        bin/
          wt_hs_probe.rs      # SRT handshake + TS continuity probe
          mock_obs.rs         # Streams fixture over SRT
          wt_echo_client.rs   # WT datagram round-trip test
      tests/
        broadcaster.rs        # fanout / viewer cap / lag integration tests
        timebase_drift.rs     # diagnostic: confirms forked TimeBase::adjust sign-flip fix
    srt-wasm/                 # wasm-bindgen wrapper around srt-protocol (receiver + sender)
    mpeg2ts-wasm/             # wasm-bindgen wrapper around mpeg2ts::TsDemuxer (+ nal.rs + aes3.rs + DebugSnapshot)
    ts-muxer-wasm/            # wasm-bindgen wrapper around the publisher-side TS muxer (video, Opus/AAC, PCM push)
  web/
    index.html                # default page — loads pages/viewer.ts (player SDK + lazy debug panel)
    simple.html               # stripped-down page — loads main.ts (no debug panel)
    stream.html               # publisher page — loads stream.tsx
    package.json              # web-UI toolchain (TypeScript, Preact, Chart.js)
    vite.config.ts            # multi-page HTTPS build config
    tsconfig.json
    smoke.mjs                 # Node smoke test for WASM modules
    pcm-roundtrip.mjs         # PCM round-trip + MessagePort semantics test
    pcm-port-bench.mjs        # PCM relay vs direct-MessagePort jitter benchmark
    src/
      main.ts                 # simple-page entry: thin UI wrapper around mountPlayer()
      pages/
        viewer.ts             # default-page entry: mountPlayer + opt-in debug panel (lazy-loaded)
      player/
        index.ts              # mountPlayer(): framework-agnostic player SDK → PlayerHandle (EventTarget)
      shared/
        viewer.ts             # viewer lifecycle: WT URL build, worker mgmt, backoff reconnect, audio wiring
        pmt.ts                # PMT summarization (video/audio PID + codec from stream types + descriptors)
        av1.ts                # AV1 OBU content-probe (disambiguate 0x06 AV1 vs Opus)
      stream.tsx              # publisher page: screen capture → VideoEncoder → TsMuxer → SRT
      stream-worker.ts        # publisher Web Worker: VideoEncoder/AudioEncoder → TsMuxer → SrtReceiver.sendMessage
      worker.ts               # viewer Web Worker: WebTransport + SrtReceiver + Demuxer (adaptive tick)
      demux.ts                # Demuxer: wraps mpeg2ts-wasm, dispatches PES/PCM events
      decode.ts               # H.264/HEVC/AV1 parsers, VideoPipeline, Opus/AAC audio pipelines
      pcm-player.ts           # PcmPlayer: AudioWorklet ring for s302m PCM
      render.ts               # CanvasRenderer: PTS-paced presentation (bounded ring, rAF-driven)
      wasm.d.ts               # Type declarations for MediaStreamTrackGenerator
      debug/                  # debug panel (Preact + signals)
        store.ts              # DebugStore: reactive signals consumed by all tabs
        sampler.ts            # main-thread sampler for decoder/renderer stats
        types.ts              # shared TS contracts (DemuxStats, VideoStats, etc.)
        diagnostics.ts        # "Download/Copy Info" JSON exporter
        gpu-info.ts, media-capabilities.ts
        components/           # Panel, StreamTab, CodecTab, AudioTab, GpuTab, SrtTab, DemuxTab,
                              # ConsoleTab, TestTab, PacketTimeline, packetUtils, streamTypes
        components/charts/    # BitrateChart, PidDonutChart, CcHeatmap, RaTimeline, PtsJumpSparkline,
                              # PcrChart, NalStackedBar, LossCorrelation, LossHeatmap, QueueSparkline, …
    public/
      cert-hash.js            # runtime-generated (gitignored)
      favicon.ico
    wasm/                     # pre-built wasm-pack pkg output (gitignored)
      srt-wasm/
      mpeg2ts-wasm/
      ts-muxer-wasm/
    dist/                     # built web-UI bundle (gitignored; embedded into the gateway binary)
```

## Latency tuning

There are two independent SRT TSBPD latencies in the reference binary:

- **`--latency` (default 120 ms)** — controls the **OBS → gateway** ingester
  link (passed to `SrtIngester::bind_with_latency`). Raise it if OBS is on a
  high-latency network.
- **Browser latency slider (default 120 ms in the UI)** — controls the
  **gateway → browser** link. The gateway-side floor is 10 ms
  (`SrtConfig::default().send_latency`); the browser's requested latency wins
  via `max(sender, receiver)` during HSv5 handshake.

For high-bitrate streams, cap the SRT send bandwidth with
`--max-bandwidth <kbps>` (~125% of the stream bitrate) so retransmit headroom
doesn't oversubscribe the uplink.

The renderer does not add its own playout delay — SRT's TSBPD is the only
latency buffer.

## Gotchas

- `web/public/cert-hash.js` is **runtime-generated** (gitignored). Don't commit
  it. The gateway writes it on boot.
- `web/wasm/` and `web/dist/` contents are **gitignored**. Fresh clones must
  run the build steps from Build Instructions before the page will work.
- WASM camelCase warnings in `srt-wasm` are **required** by wasm-bindgen —
  don't "fix" them.
- `performance.now()` epoch mismatch: browser uses `web_time::Instant`
  (Performance API), gateway uses `std::time::Instant`. SRT protocol handles
  this via timestamp fields in packets + clock sync during handshake.
- The self-signed cert is **persisted** (`~/.config/websrt/gateway-cert.pem` +
  `gateway-key.pem`), so the cert hash is **stable across restarts**. Delete
  both files and restart to rotate — then reload the browser page.
- **Don't raise `PAYLOAD_SIZE` (1128 = 6×188, TS-aligned).** Chrome silently
  drops browser→gateway datagrams larger than ~1200 bytes — the write resolves
  with no error, so it fails invisibly. 1128 + SRT header stays under the cap.
- The built-in web server uses the same self-signed cert as the WebTransport
  listener — click through the browser's "not private" warning on first load.

## Security note

Anyone with the cert hash can connect. Default `--cert-mode self` binds to
`127.0.0.1` (localhost only). Do not bind `0.0.0.0` with self-signed mode
unless you add an auth layer. Use mkcert mode for LAN access (Firefox
compatible, PKI-validated).

Stream-name occupancy is first-wins (a live stream cannot be replaced by
another publisher), but there is no per-publisher authentication beyond the
optional `?token=` gate — any session that passes the policy can claim a free
name.

## Known limitations

- **Chrome/Edge only for self-signed mode** — Firefox lacks
  `serverCertificateHashes` support. Use mkcert mode for Firefox.
- **Codec support**: H.264, HEVC/H.265, and AV1 video; Opus, AAC/ADTS, and
  PCM (SMPTE 302M) audio. Browser publishing supports H.264 and AV1 encode
  via WebCodecs; the muxer also accepts raw PCM (`push_pcm`/`push_pcm_pid`).
- **Opus-in-MPEG-TS** — supported via 2-byte control header strip. Each PES
  payload is treated as one Opus packet (ffmpeg's default). AAC/ADTS is the
  default for OBS and is fully supported.
- **2-week cert validity** (self-signed mode) — `serverCertificateHashes`
  imposes a 14-day cap. The cert is persisted and reused across restarts;
  delete `~/.config/websrt/gateway-cert.pem` + `gateway-key.pem` to regenerate.
- **No SRT encryption** between gateway and browser (WebTransport TLS replaces
  it). The OBS-to-gateway link supports optional AES encryption via
  `--srt-passphrase` (10–79 chars); disabled by default.

## License

[MPL-2.0](LICENSE)
