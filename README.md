# WebSRT

> **NOTE: THIS IS NOT PRODUCTION READY.** This is an experimental project that
> implements SRT over WebTransport datagrams. The protocol works, streams play,
> and NAK/retransmit recovers from packet loss, but it has not been hardened,
> audited, or tested at scale. Use at your own risk.

Pure-Rust gateway that bridges native SRT (from OBS or any SRT sender) to
browsers running **the real SRT protocol over WebTransport datagrams** — same
wire format, same NAK/ACK/retransmit semantics, no stream-per-frame remuxing,
no codec-specific server logic.

The browser runs `srt-protocol` and `mpeg2ts` compiled to WASM; JS is glue only.

```
                ┌─────── Rust gateway ──────┐
[OBS] --SRT/UDP─▶│ srt-tokio listener         │       ┌── Browser ──────────────────────┐
                │   ↓ (Instant, Bytes)       │       │ JS: WebTransport datagram I/O    │
                │ srt-protocol::sender       │       │   ↓ bytes                       │
                │   ↓ SRT packets (bytes)    │──WT──▶│ WASM: srt-protocol::receiver     │
                │ wtransport datagram driver │       │   ↓ (Instant, Bytes) messages    │
                │   ↑ ACK/NAK (bytes)        │◀─WT───│ WASM: mpeg2ts demux              │
                └────────────────────────────┘       │   ↓ PES / NAL / Opus             │
                                                     │ JS: WebCodecs decode + render    │
                                                     └──────────────────────────────────┘
```

## Key design points

- **Gateway is a dumb SRT repeater.** It terminates OBS's SRT connection, takes
  the resulting `(Instant, Bytes)` messages, and re-originates them as a new
  SRT sender to each browser. TS bytes are never inspected server-side.
- **Each browser gets its own SRT sender instance** (independent seq numbers,
  independent retransmit buffer) via `tokio::sync::broadcast` fanout.
- **Browser runs the same Rust state machines** the gateway does, compiled to
  WASM. No JS-side SRT logic; no wire-format drift risk.
- **SRT crypto disabled** between gateway and browser. WebTransport TLS
  replaces it.
- **Web Worker architecture.** The SRT receiver and TS demuxer run in a Web
  Worker off the main thread. Only WebCodecs decode and canvas rendering happen
  on the main thread.
- **Automatic OBS reconnect.** If OBS disconnects, the gateway waits for a new
  connection — no restart needed.
- **rAF-gated video presentation.** Decoded frames are buffered and presented
  at their PTS-aligned wall-clock time via `requestAnimationFrame`, preventing
  decode-ahead latency drift.
- **Bounded audio buffering.** The AudioWorklet path uses a fixed-size ring
  buffer with drop-oldest and skip-ahead to prevent latency accumulation.

## Quick start

### Prerequisites

- Rust stable (>=1.75), with `wasm32-unknown-unknown` target and `wasm-pack`
- Node.js >=18 (for the Vite dev server)
- ffmpeg (only to generate the test fixture)

```bash
rustup target add wasm32-unknown-unknown
cargo install wasm-pack

# one-time: generate the test fixture (~350 KB)
./fixtures/make-fixture.sh

# one-time: build the WASM modules and copy to web/
mkdir -p web/wasm/srt-wasm web/wasm/mpeg2ts-wasm
(cd crates/srt-wasm     && wasm-pack build --target web --release)
cp crates/srt-wasm/pkg/* web/wasm/srt-wasm/
(cd crates/mpeg2ts-wasm && wasm-pack build --target web --release)
cp crates/mpeg2ts-wasm/pkg/* web/wasm/mpeg2ts-wasm/

# install web deps
(cd web && npm install)
```

### Run with the test fixture (no OBS required)

Terminal A:

```bash
cargo run -p gateway
```

The gateway writes `web/public/cert-hash.js` on startup (the self-signed cert
DER hash for the browser). Open the dev server before loading the page so the
file is served.

Terminal B:

```bash
cd web && npm run dev
# Vite at https://localhost:5173 (self-signed; click through Chrome's warning)
```

Open the page. The cert hash is auto-loaded from `cert-hash.js` — no manual
entry needed. Click **connect**.

### Run with OBS

```bash
cargo run -p gateway -- --input srt --srt-port 9000
```

In OBS, add a Media Source (or your camera), then add an SRT output:

- Mode: `Call`
- IP: `127.0.0.1`
- Port: `9000`
- No passphrase (crypto is auto-disabled on the OBS link)

If OBS disconnects (crash, restart, network drop), the gateway automatically
waits for a reconnection — no restart required. Existing browser viewers will
auto-reconnect via the exponential-backoff logic.

### Caller mode (OBS is the listener)

```bash
cargo run -p gateway -- --input srt --srt-mode caller --srt-call 192.168.1.50:9000
```

### Simulated packet loss

```bash
cargo run -p gateway -- --sim-loss 5    # 5% random drop of data datagrams
cargo run -p gateway -- --sim-loss 20   # 20% — NAK/retransmit recovers
```

Only data packets are dropped; control packets (handshake, ACK, NAK, KeepAlive)
always pass through so the SRT reliability machinery stays functional.

### Multi-viewer

Open more browser tabs — each gets its own independent SRT sender. Viewer cap
defaults to 16 (enforced in `Broadcaster::subscribe`).

## CLI reference

```
gateway [OPTIONS]

Options:
      --input <INPUT>            Input source [default: file] [possible values: file, srt]
      --fixture <FIXTURE>        Path to .ts fixture file [default: fixtures/test.ts]
      --fixture-duration <DUR>   Fixture duration in seconds (for real-time pacing) [default: 10.0]
      --srt-mode <SRT_MODE>      SRT mode [default: listener] [possible values: listener, caller]
      --srt-port <SRT_PORT>      SRT listen port (listener mode) [default: 9000]
      --srt-call <SRT_CALL>      Address to dial (caller mode, e.g. 192.168.1.50:9000)
      --wt-port <WT_PORT>        WebTransport listen port [default: 4433]
      --bind <BIND>              WT bind address [default: 127.0.0.1]
      --cert-mode <CERT_MODE>    Certificate mode [default: self] [possible values: self, mkcert]
      --cert-pem <CERT_PEM>      PEM cert path (mkcert mode)
      --key-pem <KEY_PEM>        PEM key path (mkcert mode)
      --sim-loss <SIM_LOSS>      Simulated data-packet loss percentage (0-100) [default: 0]
      --sim-seed <SIM_SEED>      RNG seed for sim-loss (deterministic) [default: 42]
      --latency <LATENCY>        SRT TSBPD latency in milliseconds [default: 300]
```

## Certificate modes

### Self-signed (default, `--cert-mode self`)

Self-signed ECDSA certificate with SANs `localhost`, `127.0.0.1`, `::1`.
Regenerated on every boot. The DER SHA-256 hash is written to
`web/public/cert-hash.js` at startup. The browser passes it to
`serverCertificateHashes` in the WebTransport options, bypassing the normal
PKI validation. Chrome/Edge only (Firefox does not support
`serverCertificateHashes`).

The hash changes on every restart — reload the page to pick up the new one.

### mkcert (`--cert-mode mkcert`)

Uses PEM files generated by [mkcert](https://github.com/FiloSibille/mkcert).
The browser validates via normal PKI (mkcert installs a local CA). Works with
Firefox. `cert-hash.js` is set to `null`.

```bash
mkcert -install
mkcert -cert-file certs/cert.pem -key-file certs/key.pem localhost 127.0.0.1 ::1
cargo run -p gateway -- --cert-mode mkcert --cert-pem certs/cert.pem --key-pem certs/key.pem
```

See `certs/README.md` for details.

## Architecture

### Data flow

```
OBS ──SRT/UDP──► SrtIngester ──► Broadcaster (broadcast channel, depth 4096)
                                      │
                          ┌───────────┴───────────────┐
                          ▼                           ▼
                   BrowserSession A             BrowserSession B
                   ├── recv_pump                ├── recv_pump
                   │   (WT dgram → SrtInitiator)│   (WT dgram → SrtInitiator)
                   └── sender_pump              └── sender_pump
                       (viewer.recv →             (viewer.recv →
                        SrtInitiator → WT dgram)   SrtInitiator → WT dgram)
```

The gateway is a **dumb SRT repeater**: it terminates OBS's SRT/UDP connection,
re-originates TS bytes as a new SRT sender to each browser over WebTransport
datagrams. TS bytes are never inspected server-side.

Each browser session runs two concurrent tokio tasks:
- **recv_pump** — drains incoming WT datagrams (ACK/NAK from browser) into the
  SRT initiator state machine.
- **sender_pump** — drives the 2ms SRT ticker, pushes TS messages from the
  broadcast subscriber into the initiator, and sends resulting SRT packets as
  WT datagrams.

The split prevents ACK traffic from starving the sender under load.

### Browser pipeline

```
                        main thread                    │   Web Worker
                                                       │
WT datagram ──────────────────────────────────────────►│ SrtReceiver (WASM)
  (batched, up to 16 per tick)                        │   ↓ TSBPD-paced
                                                      │ SrtAction::DeliverMessage
                                                      │   ↓ raw TS bytes
                                                      │ Demuxer (WASM, mpeg2ts)
                                                      │   ↓ PES packets
  ◄────────────────── postMessage ────────────────────│ (pid, pts, payload)
  │                                                   │
  ├── VideoPipeline                                   │
  │   H.264 SPS parse → avcC → VideoDecoder           │
  │   ↓ VideoFrame                                    │
  │   CanvasRenderer (rAF-gated PTS presentation)     │
  │                                                   │
  ├── OpusAudioPipeline / AacAudioPipeline            │
  │   AudioDecoder → MediaStreamTrackGenerator        │
  │   or AudioWorklet (Firefox fallback)              │
  │                                                   │
  └── datagramWriter (ACK/NAK → WT)                   │
```

The SRT receiver and TS demuxer run in a **Web Worker** (`worker.ts`) to keep
the main thread free for decoding and rendering. Datagrams are batched (up to
16 per tick) before posting to the worker. The worker polls the SRT state
machine every 10ms via `setInterval`.

**Video presentation:** Decoded `VideoFrame`s are buffered in a 4-frame ring.
A `requestAnimationFrame` loop presents the latest frame whose PTS-mapped
wall-clock time has arrived. Late frames are dropped; overflow frames are
dropped. This prevents the decoder from running ahead of realtime.

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
  `srt-protocol` 0.4.4. Six patches:
  1. Uses `web_time::Instant` instead of `std::time::Instant` (WASM compat).
  2. `TimeBase::adjust()` sign flip — upstream applies `-drift` which doubles
     TSBPD clock error every sync cycle; changed to `+drift`.
  3. TLPKTL fix at `protocol/receiver/buffer.rs` — `checked_sub` instead of
     panicking `Sub<Duration>` to prevent underflow panic early in page life.
  4. Stats tracking methods (`rtt()`, `bandwidth_bps()`, `buffered_packets()`,
     `buffer_available_packets()`).
  5. Sender buffer edge-case fixes (`send_next_packet`, `send_packet`, and
     `number_of_unacked_packets`).
  6. `packet/time.rs` `Sub<TimeSpan>`: `unwrap_or(self)` → `unwrap()` to
     surface errors.

- **[`maxolgi/mpeg2ts`](https://github.com/maxolgi/mpeg2ts)** (master) — fork
  of `mpeg2ts` 0.6.0. One patch:
  1. `ts/reader.rs`: unknown PIDs return Raw bytes instead of erroring,
     preventing byte-stream misalignment when the receiver joins mid-stream.

## Build commands

```bash
# Rust gateway (release)
cargo build --release -p gateway

# WASM crates — must rebuild + copy to web/ after changes
(cd crates/srt-wasm && wasm-pack build --target web --release)
cp crates/srt-wasm/pkg/* web/wasm/srt-wasm/
(cd crates/mpeg2ts-wasm && wasm-pack build --target web --release)
cp crates/mpeg2ts-wasm/pkg/* web/wasm/mpeg2ts-wasm/

# Web dev server (hot-reloads TS, not WASM)
cd web && npm run dev

# TypeScript typecheck (no emit)
cd web && npx tsc --noEmit

# Fixture generator (needs ffmpeg)
./fixtures/make-fixture.sh
```

### Critical build order

1. Forked `srt-protocol` (`maxolgi/srt-rs`) change → rebuild
   BOTH the gateway binary AND the srt-wasm crate + copy pkg to `web/wasm/`.
2. Changing only `web/src/*.ts` → Vite hot-reloads, no rebuild needed.
3. Changing `crates/srt-wasm/src/lib.rs` → wasm-pack build + copy pkg + browser
   reload.

## Production deployment

The gateway runs under supervisord in production.

### Supervisord config

Config file `websrt.conf` is deployed to `/etc/supervisor/conf.d/`:

```ini
[program:websrt]
command=/opt/WebSRT/target/release/gateway --input srt --srt-mode listener --srt-port 9000 --bind 0.0.0.0 --latency 1000
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
cargo run --bin wt_hs_probe

# Sends fixture over SRT to test ingester without real OBS
cargo run --bin mock_obs

# WT datagram round-trip test
cargo run --bin wt_echo_client
```

### Node smoke test

```bash
# Tests both WASM modules without a browser
node web/smoke.mjs
```

### Manual OBS test

1. Start gateway: `cargo run -p gateway -- --input srt --srt-port 9000`
2. Start Vite: `cd web && npm run dev`
3. Open browser, click connect
4. In OBS: SRT output to `127.0.0.1:9000`, mode `caller`
5. Kill OBS (Ctrl-C) — gateway should log "waiting for reconnect"
6. Restart OBS — gateway reconnects, browser auto-reconnects

## Repo layout

```
WebSRT/
  Cargo.toml                  # workspace (3 crates, 2 forked deps via [patch.crates-io])
  Cargo.lock                  # gitignored — regenerated on build
  AGENTS.md                   # build commands, architecture, gotchas
  websrt.conf                 # supervisord config (production)
  LICENSE                     # MPL-2.0
  crates/
    gateway/                  # the gateway binary + dev/test binaries
      src/
        main.rs               # CLI parsing, cert bootstrap
        server.rs             # WT accept loop, viewer cap, graceful shutdown
        session.rs            # per-browser session: recv_pump + sender_pump
        srt_sender.rs         # SrtInitiator: wraps srt-protocol Connect → DuplexConnection
        broadcaster.rs        # broadcast fanout with alive-flag + subscriber cap
        cert.rs               # self-signed / mkcert cert management
        ingest/
          mod.rs              # Ingester trait + TsMessage type
          srt.rs              # SrtIngester: srt-tokio listener/caller with reconnect
          file.rs             # FileIngester: fixture loop with real-time pacing
        bin/
          wt_hs_probe.rs      # SRT handshake + TS continuity probe
          mock_obs.rs         # Streams fixture over SRT
          wt_echo_client.rs   # WT datagram round-trip test
    srt-wasm/                 # wasm-bindgen wrapper around srt-protocol::receiver
    mpeg2ts-wasm/             # wasm-bindgen wrapper around mpeg2ts::TsDemuxer
  web/
    index.html                # page shell, loads cert-hash.js
    package.json              # vite + typescript
    vite.config.ts            # HTTPS dev server (basic-ssl plugin)
    tsconfig.json
    smoke.mjs                 # Node smoke test for WASM modules
    src/
      main.ts                 # WT connect, PMT codec detection, auto-reconnect
      worker.ts               # Web Worker: SrtReceiver + Demuxer (off main thread)
      srt.ts                  # SrtController (legacy main-thread path, unused)
      demux.ts                # Demuxer: wraps mpeg2ts-wasm, dispatches PES events
      decode.ts               # H.264 SPS parser, VideoPipeline, Opus/AAC audio pipelines
      render.ts               # CanvasRenderer: rAF-gated PTS-based presentation
      wasm.d.ts               # Type declarations for MediaStreamTrackGenerator
    public/
      cert-hash.js            # runtime-generated (gitignored)
      favicon.ico
    wasm/                     # pre-built wasm-pack pkg output (gitignored)
      srt-wasm/
      mpeg2ts-wasm/
  fixtures/
    make-fixture.sh           # ffmpeg generates 10s H.264+Opus test stream
    test.ts                   # generated fixture (committed)
  certs/
    README.md                 # mkcert setup instructions
```

## Latency tuning

The `--latency` flag sets the SRT TSBPD (Timestamp-Based Packet Delivery)
latency on the gateway side. The browser's latency slider does the same on the
receiver side. During HSv5 handshake, the effective latency is `max(sender,
receiver)`.

**Rule of thumb:** Set `--latency` to at least 4x the WT RTT. The gateway logs
a warning every 5 seconds if this is violated:

```
WARN session: WT RTT suggests latency is too low; consider --latency <recommended>
```

The browser's playout delay (video presentation buffering) is automatically set
to `min(150ms, latency/2)`.

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
- **Opus-in-MPEG-TS** — supported via 2-byte control header strip. Each PES
  payload is treated as one Opus packet (ffmpeg's default). AAC/ADTS is the
  default for OBS and is fully supported.
- **2-week cert expiry** (self-signed mode) — server regenerates on boot; dev
  workflow is "restart, reload page".
- **No SRT encryption** between gateway and browser (WebTransport TLS replaces
  it). The OBS-to-gateway link also has crypto disabled.
