# AGENTS.md

## Build commands

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

## Critical build order

1. Vendored crates (`vendor/srt-protocol`, `vendor/mpeg2ts`) change → rebuild BOTH the gateway binary AND the affected WASM crate + copy pkg to web/wasm/
2. Changing only `web/src/*.ts` → Vite hot-reloads, no rebuild needed
3. Changing `crates/srt-wasm/src/lib.rs` → wasm-pack build + copy pkg + browser reload

## Vendored crates (patched, not upstream)

- `vendor/srt-protocol` — patched to use `web_time::Instant` instead of `std::time::Instant` for WASM compatibility. Also contains a TLPKTL fix in `src/protocol/receiver/buffer.rs:485` (`checked_sub` instead of panicking `Sub<Duration>`).
- `vendor/mpeg2ts` — unmodified vendoring for stability.
- Both are patched via `[patch.crates-io]` in root `Cargo.toml`.

## Architecture

Gateway is a **dumb SRT repeater**: terminates OBS's SRT/UDP connection, re-originates TS bytes as a new SRT sender to each browser over WebTransport datagrams. TS bytes are never inspected server-side.

Each browser gets its own independent `SrtInitiator` (independent seq numbers, retransmit buffer). Fanout via `tokio::sync::broadcast` in `broadcaster.rs`.

Browser runs the **same** `srt-protocol` + `mpeg2ts` Rust crates compiled to WASM. JS is glue only (WT datagram I/O, WebCodecs, canvas/audio routing).

## Key files

- `crates/gateway/src/session.rs` — per-browser session: dual-task split (recv_pump + sender_pump) sharing `SrtInitiator` via `Arc<Mutex<_>>`. Sim-loss injector lives here.
- `crates/gateway/src/srt_sender.rs` — wraps `srt_protocol::Connect` → `DuplexConnection`. `drain()` captures `Action::UpdateStatistics` into `last_stats`.
- `crates/srt-wasm/src/lib.rs` — `SrtReceiver` wraps `Listen` → `DuplexConnection`. State in `RefCell`. `handle_datagram(bytes, now_us)` + `poll(now_us)` return `Vec<SrtAction>`.
- `web/src/decode.ts` — H.264 SPS parser (exp-Golomb, High profile), avcC builder, `VideoPipeline`, `OpusAudioPipeline`, `AacAudioPipeline`. AudioWorklet fallback when `MediaStreamTrackGenerator` unavailable.
- `web/src/main.ts` — WT connect, PMT codec detection (AAC 0x0F vs Opus 0x06), connect/stop button state, auto-reconnect with backoff.

## Runtime

Gateway runs under supervisord:
- Config: `srtsocket.conf` → deployed to `/etc/supervisor/conf.d/srtsocket.conf`
- Logs: `logs/gateway.out.log` + `logs/gateway.err.log`
- Restart: `sudo supervisorctl reread && sudo supervisorctl update && sudo supervisorctl restart srtsocket`
- **After rebuilding the binary**, must restart supervisord to pick it up
- On boot, gateway writes `web/public/cert-hash.js` (hash for self-signed, null for mkcert)

## Cert modes

- `--cert-mode self` (default): self-signed ECDSA, browser connects with `serverCertificateHashes` (Chrome only)
- `--cert-mode mkcert`: loads PEM files, browser uses normal PKI (Firefox compatible)
- The cert hash changes on every restart — browser must reload page to pick up new hash

## Gotchas

- `web/public/cert-hash.js` is **runtime-generated** (gitignored). Don't commit it.
- WASM camelCase warnings in `srt-wasm` are **required** by wasm-bindgen — don't "fix" them.
- `SrtIngester.kind` field stores `SrtListener` to keep it alive (drop = close listener). The "never read" warning is intentional.
- The first decoded video frame logs "(0x0)" dimensions — Chrome resolves actual dimensions from the avcC on subsequent frames. Cosmetic only.
- `performance.now()` epoch mismatch: browser uses `web_time::Instant` (Performance API), gateway uses `std::time::Instant`. SRT protocol handles this via timestamp fields in packets + clock sync during handshake.
- TSBPD latency negotiation: `max(sender_latency, receiver_latency)` during HSv5. Browser slider and `--latency` CLI both matter.

## Testing

- `web/smoke.mjs` — Node smoke test for both WASM modules (no browser needed)
- `cargo run --bin wt_hs_probe` — SRT handshake + TS continuity-counter probe (tests NAK/retransmit under sim-loss)
- `cargo run --bin mock_obs` — Sends fixture over SRT to test ingester without real OBS
- `cargo run --bin wt_echo_client` — WT datagram round-trip test

## Behavioral guidelines

Think before coding, keep changes surgical, define success criteria. Match existing code style.

### 1. Think Before Coding

**Don't assume. Don't hide confusion. Surface tradeoffs.**

Before implementing:
- State your assumptions explicitly. If uncertain, ask.
- If multiple interpretations exist, present them - don't pick silently.
- If a simpler approach exists, say so. Push back when warranted.
- If something is unclear, stop. Name what's confusing. Ask.

### 2. Simplicity First

**Minimum code that solves the problem. Nothing speculative.**

- No features beyond what was asked.
- No abstractions for single-use code.
- No "flexibility" or "configurability" that wasn't requested.
- No error handling for impossible scenarios.
- If you write 200 lines and it could be 50, rewrite it.

Ask yourself: "Would a senior engineer say this is overcomplicated?" If yes, simplify.

### 3. Surgical Changes

**Touch only what you must. Clean up only your own mess.**

When editing existing code:
- Don't "improve" adjacent code, comments, or formatting.
- Don't refactor things that aren't broken.
- Match existing style, even if you'd do it differently.
- If you notice unrelated dead code, mention it - don't delete it.

When your changes create orphans:
- Remove imports/variables/functions that YOUR changes made unused.
- Don't remove pre-existing dead code unless asked.

The test: Every changed line should trace directly to the user's request.

### 4. Goal-Driven Execution

**Define success criteria. Loop until verified.**

Transform tasks into verifiable goals:
- "Add validation" → "Write tests for invalid inputs, then make them pass"
- "Fix the bug" → "Write a test that reproduces it, then make it pass"
- "Refactor X" → "Ensure tests pass before and after"

For multi-step tasks, state a brief plan:
```
1. [Step] → verify: [check]
2. [Step] → verify: [check]
3. [Step] → verify: [check]
```

Strong success criteria let you loop independently. Weak criteria ("make it work") require constant clarification.