# srtsocket

Pure-Rust gateway that bridges native SRT (from OBS or any SRT sender) to a
browser running **the real SRT protocol over WebTransport datagrams** — same
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

- **Gateway is a dumb SRT repeater.** It terminates OBS's SRT connection, takes
  the resulting `(Instant, Bytes)` messages, and re-originates them as a new
  SRT sender to the browser. TS bytes are never inspected.
- **Each browser gets its own SRT sender instance** (independent seq numbers,
  independent retransmit buffer).
- **Browser runs the *same* Rust state machines** the gateway does, compiled to
  WASM. No TS-side SRT logic; no wire-format drift risk.
- **SRT crypto disabled** between gateway and browser. Wire-compatible with
  native SRT (which treats crypto as optional). WebTransport TLS replaces it.

## Quick start

### Prerequisites

- Rust stable (≥1.75), with `wasm32-unknown-unknown` target and `wasm-pack`
- Node.js ≥18 (for the Vite dev server)
- ffmpeg (only to generate the test fixture)

```bash
rustup target add wasm32-unknown-unknown
cargo install wasm-pack

# one-time: generate the test fixture (~350 KB)
./fixtures/make-fixture.sh

# one-time: build the wasm modules
(cd crates/srt-wasm     && wasm-pack build --target web --release)
(cd crates/mpeg2ts-wasm && wasm-pack build --target web --release)

# install web deps
(cd web && npm install)
```

### Run with the test fixture (no OBS required)

Terminal A:

```bash
cargo run -p gateway
# prints: INFO gateway: WebTransport cert DER SHA-256: <64-hex>
```

Copy the hash.

Terminal B:

```bash
cd web && npm run dev
# Vite at http://localhost:5173
```

Open the page, paste the hash, click "connect". You should see PAT/PMT/PES
events in the log and (Phase 6+) video frames decoded onto the canvas.

### Run with OBS

```bash
cargo run -p gateway -- --input srt --srt-port 9000
```

In OBS, add a "Media Source" (or your camera) → Filters → "SRT Output":

- Mode: `Call`
- IP: `127.0.0.1`
- Port: `9000`
- (no passphrase; gateway-side crypto is auto-disabled on the OBS link too)

Open the browser, paste the cert hash, click connect.

### Simulated loss

```bash
cargo run -p gateway -- --sim-loss 5    # 5% random drop of data datagrams
cargo run -p gateway -- --sim-loss 20   # 20% — still 0 gaps thanks to NAK/retransmit
```

### Multi-viewer

Just open more browser tabs — each gets its own SRT sender instance. Viewer
cap defaults to 16 (see `Broadcaster`).

## CLI

```
gateway [OPTIONS]

Options:
      --input <INPUT>          [default: file] [possible values: file, srt]
      --fixture <FIXTURE>      [default: fixtures/test.ts]
      --srt-port <SRT_PORT>    [default: 9000]
      --wt-port <WT_PORT>      [default: 4433]
      --bind <BIND>            [default: 127.0.0.1]
      --cert-mode <CERT_MODE>  [default: self] [possible values: self, mkcert]
      --cert-pem <CERT_PEM>    mkcert cert path
      --key-pem <KEY_PEM>      mkcert key path
      --sim-loss <SIM_LOSS>    [default: 0]
      --sim-seed <SIM_SEED>    [default: 42]
```

## Repo layout

```
srtsocket/
  Cargo.toml                 # workspace
  crates/
    gateway/                 # the binary
    srt-wasm/                # wasm-bindgen wrapper around srt-protocol::receiver
    mpeg2ts-wasm/            # wasm-bindgen wrapper around mpeg2ts
  vendor/
    srt-protocol/            # patched srt-protocol 0.4.4 (std::time::Instant → web_time::Instant)
  web/                       # Vite + TS browser app
  fixtures/test.ts           # ffmpeg-generated test stream
  certs/README.md            # mkcert instructions
  plan.md                    # implementation plan (read this!)
```

## Status

| Phase | Status | Exit criterion |
|---|---|---|
| 0 — Skeleton + cert bootstrap | ✅ | Binary boots, prints 32-byte hash. |
| 1 — WT datagram echo | ✅ | Chrome ↔ Rust round-trips a datagram over WT. |
| 2 — WASM bridge | ✅ | srt-wasm + mpeg2ts-wasm build; node smoke tests pass. |
| 3 — SRT HSv5 handshake | ✅ | Handshake completes on both sides over WT datagrams. |
| 4 — SRT data plane | ✅ | FileIngester → TS passthrough; 137+ messages with 0x47 sync bytes received intact. |
| 5 — NAK + retransmit + sim-loss | ✅ | TLPKTL enabled on both sides. SRT sender stats (tx_dropped, tx_retransmit, rx_nak) logged every 5s. 0 TS-CC gaps at 5% and 20% random loss. |
| 6 — WebCodecs video decode | ✅ | Full H.264 SPS parser (exp-Golomb, frame cropping, High profile). avcC + VideoDecoder + canvas. Verified with OBS High profile 4.2. |
| 7 — Audio (Opus / AAC) | ✅ | AAC (ADTS) and Opus both supported. Auto-detect from PMT stream type. AudioSpecificConfig for AAC. Opus TOC stereo detection. MediaStreamTrackGenerator + AudioWorklet fallback. Verified with live OBS AAC audio. |
| 8 — SRT ingest from OBS | ✅ | srt-tokio listener + caller modes. mock_obs verified end-to-end. Live OBS confirmed. |
| 9 — Firefox/mkcert + hardening | ✅ | Multi-viewer broadcast fanout (cap 16). mkcert mode writes null cert-hash.js. Runtime datagram-size assertion. RTT-aware latency warning (conn.rtt() × 4 vs --latency). Graceful Ctrl-C drain (2s GOAWAY). Browser auto-reconnect with exponential backoff. AudioWorklet fallback for Firefox. |

## Known limitations / future work

- **Opus-in-MPEG-TS** — supported via 2-byte control header strip. Each PES
  payload is treated as one Opus packet (ffmpeg's default). AAC/ADTS is the
  default for OBS and is fully supported (AudioSpecificConfig extracted from
  ADTS header, `mp4a.40.2` codec).
- **2-week cert expiry** — server regenerates on boot; dev workflow is
  "restart, reload page". The hash is auto-written to `web/public/cert-hash.js`.

## Security note

Anyone with the cert hash can connect. Default `--cert-mode self` is
**localhost only** (bind `127.0.0.1`). Do not bind `0.0.0.0` unless you also
add an auth layer.
