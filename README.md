# WebSRT

> **NOTE: THIS IS NOT PRODUCTION READY.** This is an experimental project that
> implements SRT over WebTransport datagrams. The protocol works, streams play,
> and NAK/retransmit recovers from packet loss, but it has not been hardened,
> audited, or tested at scale.

Pure-Rust gateway that bridges native SRT (from OBS or any SRT sender) to
browsers running **the real SRT protocol over WebTransport datagrams** — same
wire format, same NAK/ACK/retransmit semantics.

The browser runs `srt-protocol` and `mpeg2ts` compiled to WASM; JS is glue only.

The gateway supports **both directions**: OBS → viewersand browser → viewers
(browser publishes via WebTransport, gateway re-originates to other browsers).
A browser can even do both simultaneously.

```
                  ┌─────── Rust gateway ───────────────┐
 [OBS] --SRT/UDP─▶│ srt-tokio listener (ingest)        │       ┌── Browser (viewer) ──────────────┐
                  │   ↓ (Instant, Bytes)               │       │ JS: WebTransport datagram I/O    │
                  │ broadcaster (broadcast channel)    │       │   ↓ bytes                        │
 [Browser] ─WT───▶│ SrtInitiator (ingest, publish)     │──WT──▶│ WASM: srt-protocol::receiver     │
  (publisher)     │   ↓ (Instant, Bytes)               │       │   ↓ (Instant, Bytes) messages    │
                  │   ↓                                │       │ WASM: mpeg2ts demux              │
                  │ srt-protocol::sender (per viewer)  │       │   ↓ PES / NAL / Opus             │
                  │   ↓ SRT packets (bytes)            │       │ JS: WebCodecs decode + render    │
                  │ wtransport datagram driver         │       └──────────────────────────────────┘
                  │   ↑ ACK/NAK (bytes)                │
                  └────────────────────────────────────┘
```

## Key design points

- **Gateway is a dumb SRT repeater.** It terminates OBS's SRT connection, takes
  the resulting `(Instant, Bytes)` messages, and re-originates them as a new
  SRT sender to each browser. TS bytes are never inspected server-side.
- **Browser publishing.** A browser can publish upstream via WebTransport.
  Published streams are fanned out to viewers exactly like OBS streams
  same broadcaster, same per-viewer SRT sender. The publisher muxes TS
  locally (`ts-muxer-wasm`), sends SRT over WT datagrams to the gateway.
- **Each browser gets its own SRT sender instance** (independent seq numbers,
  independent retransmit buffer) via `tokio::sync::broadcast` fanout.
- **Browser runs the same Rust state machines** the gateway does, compiled to
  WASM.
- **SRT crypto disabled** between gateway and browser. WebTransport TLS
  replaces it. The OBS-to-gateway link supports optional AES encryption via
  `--srt-passphrase`.
- **Multi-codec support.** H.264 (avcC + SPS parsing), HEVC/H.265 (hvcC +
  VPS/SPS/PPS), and AV1 (OBU Sequence Header parsing) video; Opus and AAC/ADTS
  audio. The browser publisher supports H.264 and AV1 encode via WebCodecs
  `VideoEncoder`.
- **Web Worker architecture.** The SRT receiver and TS demuxer run in a Web
  Worker off the main thread. Only WebCodecs decode and canvas rendering happen
  on the main thread.
- **Desktop GUI.** The gateway launches an eframe/egui config window by
  default with Start/Stop, live stats, and a scrolling log panel. Pass
  `--no-gui` for headless CLI mode (required under supervisord). On headless
  servers with no display, the GUI auto-falls-back to CLI.
- **Single-binary deployment.** The built-in HTTPS web server embeds
  `web/dist/` at compile time via `rust-embed`, so the gateway binary serves
  the viewer UI with no external file server. Pass `--no-web` to use the Vite
  dev server instead.
- **PTS-paced video presentation.** Decoded frames are queued in a small
  bounded ring and drawn on `requestAnimationFrame` when their PTS is due,
  measured against a wall-clock ↔ PTS mapping that resets on large gaps
  (seek, stream restart, backgrounded tab). SRT's TSBPD paces delivery; PTS
  pacing absorbs downstream bursts (decoder reorder, worker→main batching) so
  the canvas updates at the source frame rate. Opt out via the player SDK's
  `setRenderPacing(false)` to fall back to "latest frame only" skip-ahead.
- **Bounded audio buffering.** The AudioWorklet path uses a fixed-size ring
  buffer with drop-oldest and skip-ahead to prevent latency accumulation.

## Quick start

### Prerequisites

- Rust stable (>=1.75), with `wasm32-unknown-unknown` target and `wasm-pack`
- Node.js >=18 (for the Vite dev server)
- ffmpeg (only for the live publisher script, `fixtures/stream.sh`)
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

# one-time: build all 3 WASM modules, copy to web/wasm/, install web deps
./build.sh setup
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
library, web, check, test, clean, etc.).

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

# Web deps + dev server
npm.cmd install --prefix web
npm.cmd run dev --prefix web    # https://localhost:5173

# Gateway binary (release) → target\release\websrt-gateway.exe
cargo build --release -p websrt-gateway
.\target\release\websrt-gateway.exe             # GUI: only the egui window, no console window
.\target\release\websrt-gateway.exe --no-gui    # headless: reattaches to the terminal for log output
```

Everything else in this README (`cargo run`, CLI flags, cert modes, browser
flow) works identically on Windows. The only Linux-only steps are the
supervisord deployment (`./build.sh restart`, `websrt.conf`) and the
`fixtures/stream.sh` publisher (a bash+ffmpeg script — adapt it or run OBS
instead).

The test fixture (`fixtures/test.ts`, ~45 KB, H.264+Opus, 10 s loop) is committed
to the repo — no generation step needed. The replacement for the old
`make-fixture.sh` is `fixtures/stream.sh`, a live ffmpeg publisher (NVENC/VAAPI)
that streams to the gateway's SRT listener instead of writing a file.

### Run with the test fixture (no OBS required)

Terminal A:

```bash
cargo run -p websrt-gateway
```

The gateway launches a GUI window by default (config form + live stats + logs).
Add `--no-gui` for headless mode. The gateway writes `web/public/cert-hash.js`
on startup (the self-signed cert DER hash for the browser).

If `web/dist/` exists (from `./build.sh web build`), the gateway also serves
the UI on `https://127.0.0.1:5173` via its built-in HTTPS server — no separate
Vite server needed. For development with hot-reload, pass `--no-web` and use
the Vite dev server in Terminal B.

Terminal B (development only — skip if using the built-in server):

```bash
cd web && npm run dev
# Vite at https://localhost:5173 (self-signed; click through Chrome's warning)
```

Open the page. The cert hash is auto-loaded from `cert-hash.js` — no manual
entry needed. Click **connect**.

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

## Library usage

The `websrt` crate is the reusable core. The reference binary (`websrt-gateway`) is
a thin CLI wrapper around it. To embed in your own application:

For embedding the **browser-side player** (canvas + WebCodecs SDK), see
[`docs/embedding.md`](docs/embedding.md) — a 294-line guide covering the
`mountPlayer()` SDK, config options, and event surface.

```rust
use websrt::Gateway;
use websrt::cert::{Cert, CertSource};
use websrt::ingest::srt::SrtIngester;

# async fn run() -> anyhow::Result<()> {
let cert = Cert::build(CertSource::SelfSigned {
    sans: vec!["localhost".into()],
}).await?;

let gateway = Gateway::builder()
    .bind_addr("127.0.0.1:4433".parse::<std::net::SocketAddr>()?)
    .identity(cert.identity.clone_identity())
    .latency_ms(1000)
    .max_viewers(16)
    .build()?;

// Deferred ingester: connect OBS in background
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

### Simulated packet loss (feature-gated)

The `sim-loss` feature enables a probabilistic datagram dropper for testing
NAK/retransmit. Without the feature, the `rand` dependency is excluded.

```toml
[dependencies]
websrt = { path = "...", features = ["sim-loss"] }
```

## CLI reference

```
websrt-gateway [OPTIONS]

Options:
      --no-gui                   Skip the GUI, run in headless CLI mode
      --no-web                   Disable the built-in HTTPS web server
      --web-port <WEB_PORT>      HTTPS port for built-in web server [default: 5173]
      --web-bind <WEB_BIND>      Bind address for HTTPS web server [default: 127.0.0.1]
      --web-root <WEB_ROOT>      Root directory for web files (auto-detected if unset)
      --input <INPUT>            Input source [default: file] [possible values: file, srt]
      --fixture <FIXTURE>        Path to .ts fixture file [default: fixtures/test.ts]
      --fixture-duration <DUR>   Fixture duration in seconds (for real-time pacing) [default: 10.0]
      --srt-mode <SRT_MODE>      SRT mode [default: listener] [possible values: listener, caller]
      --srt-port <SRT_PORT>      SRT listen port (listener mode) [default: 9000]
      --srt-call <SRT_CALL>      Address to dial (caller mode, e.g. 192.168.1.50:9000)
      --srt-streamid <STREAMID> SRT stream id (listener: filter, caller: sent to OBS)
      --srt-passphrase <PASS>    SRT encryption passphrase for OBS leg (10-79 chars)
      --wt-port <WT_PORT>        WebTransport listen port [default: 4433]
      --bind <BIND>              WT bind address [default: 127.0.0.1]
      --cert-mode <CERT_MODE>    Certificate mode [default: self] [possible values: self, mkcert]
      --cert-pem <CERT_PEM>      PEM cert path (mkcert mode)
      --key-pem <KEY_PEM>        PEM key path (mkcert mode)
      --sim-loss <SIM_LOSS>      Simulated data-packet loss percentage (0-100) [default: 0]
      --sim-seed <SIM_SEED>      RNG seed for sim-loss (deterministic) [default: 42]
      --latency <LATENCY>        OBS↔gateway SRT TSBPD latency in milliseconds [default: 120]
      --health-port <HEALTH_PORT> HTTP health/metrics port (0 = disabled) [default: 0]
      --health-bind <HEALTH_BIND> Bind address for the HTTP health/metrics server [default: 127.0.0.1]
      --auth-token <AUTH_TOKEN>  Viewer auth token; browsers must pass ?token=<value>
      --max-viewers <N>          Max concurrent viewers per stream [default: 16]
```

## Certificate modes

### Self-signed (default, `--cert-mode self`)

Self-signed ECDSA P-256 certificate with SANs `localhost`, `127.0.0.1`, `::1`.
Generated once, then persisted to `~/.config/websrt/gateway-cert.pem` +
`gateway-key.pem` and reused across restarts so the browser's cert exception /
hash pinning stays stable. Delete those files to force regeneration. The DER
SHA-256 hash is written to `web/public/cert-hash.js` at startup. The browser
passes it to `serverCertificateHashes` in the WebTransport options, bypassing
the normal PKI validation. Chrome/Edge only (Firefox does not support
`serverCertificateHashes`).

Delete the persisted cert files and restart to rotate the hash.

### mkcert (`--cert-mode mkcert`)

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
OBS ──SRT/UDP──► SrtIngester ──► Broadcaster (broadcast channel, depth 4096)
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
  (one per read; 5 ms tick coalesce)                  │   ↓ TSBPD-paced
                                                      │ SrtAction::DeliverMessage
                                                      │   ↓ raw TS bytes
                                                      │ Demuxer (WASM, mpeg2ts)
                                                      │   ↓ PES packets
  ◄────────────────── postMessage ────────────────────│ (pid, pts, payload)
  │                                                   │
  ├── VideoPipeline                                   │
  │   H.264 SPS parse → avcC → VideoDecoder           │
  │   ↓ VideoFrame                                    │
  │   CanvasRenderer (PTS-paced, rAF-driven)            │
  │                                                   │
  ├── OpusAudioPipeline / AacAudioPipeline            │
  │   AudioDecoder → MediaStreamTrackGenerator        │
  │   or AudioWorklet (Firefox fallback)              │
  │                                                   │
  └── datagramWriter (ACK/NAK → WT)                   │
```

The SRT receiver and TS demuxer run in a **Web Worker** (`worker.ts`) to keep
the main thread free for decoding and rendering. The worker races a datagram
read against a 5 ms tick so the SRT state machine advances even when no data
arrives; outgoing messages (PES packets, stats, logs) coalesce into a single
batched `postMessage` to the main thread.

**Video presentation:** Decoded frames are queued in a small bounded ring and
drawn on `requestAnimationFrame` when their PTS is due (PTS-paced presentation,
on by default). A wall-clock ↔ PTS mapping established on the first frame and
reset on large gaps (seek, restart, backgrounded tab) gates each draw. Frames
that missed their slot are dropped as late (the newest is always kept so the
canvas never freezes). Opt out via `setRenderPacing(false)` to fall back to
"latest frame only" skip-ahead drawing.

**Audio output:** On Chrome, `MediaStreamTrackGenerator` provides implicit
pacing. On Firefox, the AudioWorklet path uses a bounded `Float32Array` ring
buffer (24,000 samples, ~0.5s) with drop-oldest and skip-ahead when buffered
data exceeds the playout target by more than 50ms.

**Backpressure:** Both video and audio decoders check `decodeQueueSize` before
submitting new chunks. Video skips delta frames when queue depth > 8; audio
skips when queue depth > 20. Keyframes always pass to allow resync.

### Forked crates

This project depends on two forked crates, wired via `[patch.crates-io]` in the
root `Cargo.toml`. Cargo fetches them automatically at build time — they are
**not** submodules or vendored copies.

- **[`maxolgi/srt-rs`](https://github.com/maxolgi/srt-rs)** (main) — fork of
  `srt-protocol` 0.4.4. Twelve patches (see `AGENTS.md` for implementation
  details and coupling notes):
  1. Uses `web_time::Instant` instead of `std::time::Instant` (WASM compat).
     Also adds `getrandom` with the `js` feature for `cfg(target_arch = "wasm32")`.
  2. **`TimeBase::adjust()` sign flip** (coupled with patch 6): upstream
     applies `-drift` via the broken `Sub<TimeSpan>` (patch 6 — `Sub` is a
     copy-paste of `Add`, so it actually computes `+drift`). Upstream is
     correct *by accident*. The fork fixes `Sub` to actually subtract, so
     `adjust` must change `-drift` → `+drift` to preserve the same runtime
     behavior. These two patches must be applied or reverted together.
  3. TLPKTL `checked_sub` in `protocol/receiver/buffer.rs`: `now - (tsbpd_latency
     + tsbpd_tolerance)` panics via underflow early in page life (before latency
     has elapsed). Changed to `checked_sub` → `Option<Instant>`, returning "no
     too-late packets" when the subtraction underflows.
  4. **Stats population + accessors**: upstream declares `rx_loss_data`,
     `rx_loss_bytes`, and `rx_bandwidth` in `SocketStatistics` but never assigns
     them — they are always 0. Fork populates `rx_loss_data`/`rx_loss_bytes` in
     the `ReceivedWithLoss` path, computes `rx_bandwidth` via byte-delta, and
     adds accessor methods (`rtt()`, `bandwidth_bps()`, `buffered_packets()`,
     `buffer_available_packets()`) on ARQ/Receiver/Sender.
  5. Sender buffer `SeqNumber` wrapping-subtraction underflows (3 fixes):
     `send_next_packet` clamps `next_send` to `front_packet`,
     `send_packet` bounds-checks and returns `None` instead of panicking,
     `number_of_unacked_packets` returns 0 instead of wrapping.
  6. **`packet/time.rs` `Sub<TimeSpan>` for `Instant`** (coupled with patch 2):
     upstream `Sub<TimeSpan>` is byte-for-byte identical to `Add<TimeSpan>` — a
     copy-paste bug where both branches add. Fork swaps the branches so `Sub`
     actually subtracts, and changes `unwrap()` → `unwrap_or(self)` to avoid
     panics on `Instant` underflow.
  7. `protocol/pending_connection/listen.rs`: `Listen::allow_skip_induction`
     flag + branch in `wait_for_induction` that accepts a Conclusion-first
     handshake (skips Induction phase for 1-RTT over WebTransport).
  8. `protocol/pending_connection/connect.rs`: `Connect::new_skip_induction`
     constructor that starts in `ConclusionResponseWait` with a pre-built
     Conclusion packet (cookie=0, HSREQ extensions).
  9. `ConnInitSettings.initial_rtt: Option<Duration>` field that seeds
     `SendBuffer.rtt` and `ARQ.rtt` via `Rtt::from_mean_duration`
     (variance = mean/4). Upstream populates `ConnectionSettings.rtt` during
     the handshake but never feeds it to `SendBuffer` or `ARQ` (both use
     `Rtt::default()`). The fork makes both consumers read the setting, and
     adds a way to override it from QUIC's smoothed RTT (needed because
     skip-induction has no Induction round-trip to measure RTT).
  10. `protocol/sender/buffer.rs`: CC-aware retransmit skip in
       `send_next_lost_packet` — if `now + rtt.mean()` exceeds the packet's
       TSBPD deadline, the retransmit is skipped (receiver will drop it as
       too-late anyway). The packet is popped from the lost list before the
       check, so the skip doesn't block subsequent retransmits.
  11. `protocol/sender/buffer.rs` + `protocol/sender/mod.rs` +
       `connection/mod.rs`: Populate `SocketStatistics.tx_average_rtt` from
       `SendBuffer.rtt` in `update_statistics`. The field was declared but
       never assigned by upstream — only `rx_average_rtt` was populated.
       Without this, publisher-side stats show RTT=0.
  12. `connection/mod.rs` + `packet/control/srt.rs`: Handle `CongestionWarning`,
       `PeerError`, and unknown SRT control packets without panicking (was
       `todo!()`/`unimplemented!()`). Advertise `TLPKTDROP` + `NAKREPORT` in
       `SrtShakeFlags::SUPPORTED` (both are enabled by default but were not
       advertised to the peer).

- **[`maxolgi/mpeg2ts`](https://github.com/maxolgi/mpeg2ts)** (master) — fork
  of `mpeg2ts` 0.6.0. One patch:
  1. `ts/reader.rs`: unknown PIDs return Raw bytes instead of erroring,
     preventing byte-stream misalignment when the receiver joins mid-stream.

## Standards alignment

WebSRT implements [draft-sharabayko-srt-over-quic](https://haivision.github.io/srt-rfc/draft-sharabayko-srt-over-quic.html):

- **§4.2 Packet integrity** — satisfied. Each SRT packet is sent as exactly one WebTransport datagram; the underlying transport preserves packet boundaries.
- **§4.3 Connection establishment** — 1-RTT handshake. Because WebTransport already provides TLS-level authentication and return-routability, the SRT induction phase (whose only purpose is DoS protection via cookie) is redundant. WebSRT skips induction and runs a 2-packet handshake: the gateway sends CONCLUSION directly, the browser responds with CONCLUSION-RESP. This saves one RTT (~50-200 ms) on every viewer join.
- **§4.5 Congestion control** — transport-aware retransmit decisions. SRT's RTT is seeded from QUIC's smoothed RTT (`wtransport::Connection::rtt()` on the gateway, `WebTransport.getStats().smoothedRtt` on the browser). The SRT sender skips NAK-triggered retransmits whose predicted arrival time exceeds the TSBPD deadline, preventing wasted bandwidth on packets the receiver will drop as too-late.
- **§4.7 Connection migration** — inherited from QUIC via WebTransport. Mobile viewers can hand off between networks (cellular → WiFi) without rejoining the stream.
- **§4.8 Datagram vs H3 Datagram** — WebTransport uses H3 Datagram semantics, the load-balancer-compatible choice the draft recommends.

## Build commands

`./build.sh` wraps every common build step. Run `./build.sh --help` for the
full list; the most-used subcommands:

```bash
./build.sh setup                # one-time: WASM + npm install (run after fresh clone)
./build.sh wasm                 # rebuild all 3 WASM crates + copy to web/wasm/
./build.sh wasm srt             # rebuild just srt-wasm + copy
./build.sh gateway              # cargo build --release -p websrt-gateway
./build.sh gateway --sim-loss   # add the sim-loss feature
./build.sh lib                  # cargo build --release -p websrt (the library)
./build.sh web                  # vite dev server (alias: ./build.sh web dev)
./build.sh web build            # vite production build → web/dist/
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

# Web dev server (hot-reloads TS, not WASM)
cd web && npm run dev

# TypeScript typecheck (no emit)
cd web && npx tsc --noEmit

# Live publisher (needs ffmpeg; h264 uses NVENC, av1 uses VAAPI)
./fixtures/stream.sh h264|av1
```

### Critical build order

1. Forked `srt-protocol` (`maxolgi/srt-rs`) change → run `./build.sh srt-protocol`
   (rebuilds BOTH the gateway binary AND srt-wasm + copies pkg to `web/wasm/`).
2. Changing only `web/src/*.ts` / `*.tsx` → Vite hot-reloads, no rebuild needed.
3. Changing `crates/srt-wasm/src/lib.rs` → `./build.sh wasm srt` + browser reload.
4. Changing `crates/mpeg2ts-wasm/` or `crates/ts-muxer-wasm/` →
   `./build.sh wasm mpeg2ts` (or `ts-muxer`) + browser reload.
5. Changing `crates/websrt/` (library) → `./build.sh gateway` +
   `./build.sh restart` (production only; dev just reruns the binary).

## Production deployment

The gateway runs under supervisord in production.

### Supervisord config

Config file `websrt.conf` is deployed to `/etc/supervisor/conf.d/`:

```ini
[program:websrt]
command=/opt/WebSRT/target/release/websrt-gateway --no-gui --input srt --srt-mode listener --srt-port 9000 --bind 0.0.0.0 --latency 1000 --health-port 9090
directory=/opt/WebSRT
autostart=true
autorestart=true
startretries=3
stdout_logfile=/opt/WebSRT/logs/gateway.out.log
stderr_logfile=/opt/WebSRT/logs/gateway.err.log
environment=RUST_LOG="debug"
```

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
script automatically.

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

### Node smoke test

```bash
# Tests both WASM modules without a browser
node web/smoke.mjs
```

### Manual OBS test

1. Start gateway: `cargo run -p websrt-gateway -- --input srt --srt-port 9000`
2. Start Vite: `cd web && npm run dev`
3. Open browser, click connect
4. In OBS: SRT output to `127.0.0.1:9000`, mode `caller`
5. Kill OBS (Ctrl-C) — gateway should log "waiting for reconnect"
6. Restart OBS — gateway reconnects, browser auto-reconnects

## Repo layout

```
WebSRT/
  Cargo.toml                  # workspace (5 crates, 2 forked deps via [patch.crates-io])
  Cargo.lock
  AGENTS.md                   # build commands, architecture, gotchas
  build.sh                    # build orchestrator (./build.sh --help for the menu)
  install-prereqs.sh          # toolchain installer (./install-prereqs.sh --check to verify)
  websrt.conf                 # supervisord config (production)
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
    websrt-gateway/           # reference binary: CLI wrapper around the websrt library
      src/
        main.rs               # CLI parsing, cert-hash.js writing, Gateway::run()
        bin/
          wt_hs_probe.rs      # SRT handshake + TS continuity probe
          mock_obs.rs         # Streams fixture over SRT
          wt_echo_client.rs   # WT datagram round-trip test
      tests/
        broadcaster.rs        # fanout / viewer cap / lag integration tests
        timebase_drift.rs     # diagnostic: confirms forked TimeBase::adjust sign-flip fix
    srt-wasm/                 # wasm-bindgen wrapper around srt-protocol (receiver + sender)
    mpeg2ts-wasm/             # wasm-bindgen wrapper around mpeg2ts::TsDemuxer (+ nal.rs + DebugSnapshot)
    ts-muxer-wasm/            # wasm-bindgen wrapper around the publisher-side TS muxer
  web/
    index.html                # default page — loads pages/viewer.ts (player SDK + lazy debug panel)
    simple.html               # stripped-down page — loads main.ts (no debug panel)
    stream.html               # publisher page — loads stream.tsx
    package.json              # vite + typescript + preact + chart.js
    vite.config.ts            # HTTPS dev server (basic-ssl), multi-page input
    tsconfig.json
    smoke.mjs                 # Node smoke test for WASM modules
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
      worker.ts               # viewer Web Worker: WebTransport + SrtReceiver + Demuxer
      demux.ts                # Demuxer: wraps mpeg2ts-wasm, dispatches PES events
      decode.ts               # H.264/HEVC/AV1 parsers, VideoPipeline, Opus/AAC audio pipelines
      render.ts               # CanvasRenderer: PTS-paced presentation (bounded ring, rAF-driven)
      wasm.d.ts               # Type declarations for MediaStreamTrackGenerator
      debug/                  # debug panel (Preact + signals)
        store.ts              # DebugStore: reactive signals consumed by all tabs
        sampler.ts            # main-thread sampler for decoder/renderer stats
        types.ts              # shared TS contracts (DemuxStats, VideoStats, etc.)
        diagnostics.ts        # "Download/Copy Info" JSON exporter
        gpu-info.ts, media-capabilities.ts
        components/           # Panel, StreamTab, CodecTab, GpuTab, SrtTab, DemuxTab,
                              # ConsoleTab, TestTab, PacketTimeline, packetUtils, streamTypes
        components/charts/    # BitrateChart, PidDonutChart, CcHeatmap, RaTimeline,
                              # PtsJumpSparkline, PcrChart, NalStackedBar, …
    public/
      cert-hash.js            # runtime-generated (gitignored)
      favicon.ico
    wasm/                     # pre-built wasm-pack pkg output (gitignored)
      srt-wasm/
      mpeg2ts-wasm/
      ts-muxer-wasm/
  fixtures/
    stream.sh                 # live ffmpeg publisher (h264 NVENC / av1 VAAPI) → SRT 9000
    test.ts                   # committed fixture (~45 KB, 10 s H.264+Opus loop)
  certs/
    README.md                 # mkcert setup instructions
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

The renderer does not add its own playout delay — SRT's TSBPD is the only
latency buffer.

## Gotchas

- `web/public/cert-hash.js` is **runtime-generated** (gitignored). Don't commit
  it. The gateway writes it on boot.
- `web/wasm/` contents are **gitignored**. Fresh clones must run the WASM build
  steps from Quick Start before the page will work.
- WASM camelCase warnings in `srt-wasm` are **required** by wasm-bindgen —
  don't "fix" them.
- `performance.now()` epoch mismatch: browser uses `web_time::Instant`
  (Performance API), gateway uses `std::time::Instant`. SRT protocol handles
  this via timestamp fields in packets + clock sync during handshake.
- Cert hash changes on every gateway restart — browser must reload the page.
- The Vite dev server uses HTTPS (self-signed). Click through Chrome's "not
  private" warning on first load.

## Security note

Anyone with the cert hash can connect. Default `--cert-mode self` binds to
`127.0.0.1` (localhost only). Do not bind `0.0.0.0` with self-signed mode
unless you add an auth layer. Use mkcert mode for LAN access (Firefox
compatible, PKI-validated).

## License

[MPL-2.0](LICENSE)

## Known limitations

- **Chrome/Edge only for self-signed mode** — Firefox lacks
  `serverCertificateHashes` support. Use mkcert mode for Firefox.
- **Codec support**: H.264, HEVC/H.265, and AV1 video; Opus and AAC/ADTS audio.
  Browser publishing supports H.264 and AV1 encode via WebCodecs.
- **Opus-in-MPEG-TS** — supported via 2-byte control header strip. Each PES
  payload is treated as one Opus packet (ffmpeg's default). AAC/ADTS is the
  default for OBS and is fully supported.
- **2-week cert validity** (self-signed mode) — `serverCertificateHashes`
  imposes a 14-day cap. The cert is persisted and reused across restarts;
  delete `~/.config/websrt/gateway-cert.pem` + `gateway-key.pem` to regenerate.
- **No SRT encryption** between gateway and browser (WebTransport TLS replaces
  it). The OBS-to-gateway link supports optional AES encryption via
  `--srt-passphrase` (10–79 chars); disabled by default.
