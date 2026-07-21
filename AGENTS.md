# AGENTS.md

## Build commands

Fresh-clone toolchain setup (installs rustup, wasm-pack, Node ≥ 18, ffmpeg,
build tools — Debian/Ubuntu, Fedora, Arch, macOS):

    ./install-prereqs.sh           # idempotent; --check to verify only
    # or remotely, before cloning:
    curl -sSf https://raw.githubusercontent.com/maxolgi/WebSRT/master/install-prereqs.sh | bash

`./build.sh` wraps every common build step (`./build.sh --help` for the full menu):

# One-time after a fresh clone: build all 3 WASM crates + copy to web/wasm/ + npm install
./build.sh setup

# Per-crate WASM rebuild (hot loop)
./build.sh wasm srt          # or: mpeg2ts | ts-muxer | (no arg = all three)
./build.sh wasm srt --debug  # dev profile instead of release

# Native builds
./build.sh gateway                # cargo build --release -p websrt-gateway
./build.sh gateway --sim-loss     # add the sim-loss feature
./build.sh lib                    # cargo build --release -p websrt (the library)

# Web
./build.sh web                    # vite dev server (alias for: ./build.sh web dev)
./build.sh web build              # production build → web/dist/

# Checks + tests
./build.sh check                  # cargo check --workspace + tsc --noEmit
./build.sh test                   # cargo test --workspace + node web/smoke.mjs

# Combined workflows (AGENTS.md "Critical build order")
./build.sh srt-protocol           # rule 1: gateway + srt-wasm after editing forked srt-rs
./build.sh restart                # rule 5: sudo supervisorctl restart websrt
./build.sh all                    # clean -y → setup → gateway → web build
./build.sh clean                  # rm -rf web/wasm web/dist target

Raw form (what the script wraps — useful when debugging the script itself):

# Demo gateway binary (release, with sim-loss feature)
cargo build --release -p websrt-gateway --features sim-loss

# Library only (release)
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

# Live test publisher (needs ffmpeg; h264 uses NVENC, av1 uses VAAPI)
./fixtures/stream.sh h264|av1

## Critical build order

1. Forked `srt-protocol` (`maxolgi/srt-rs`) changes → `./build.sh srt-protocol` (rebuilds BOTH the gateway binary AND srt-wasm + copies pkg to web/wasm/)
2. Changing only `web/src/*.ts` / `*.tsx` → Vite hot-reloads, no rebuild needed
3. Changing `crates/srt-wasm/src/lib.rs` → `./build.sh wasm srt` + browser reload
4. Changing `crates/mpeg2ts-wasm/` or `crates/ts-muxer-wasm/` → `./build.sh wasm <crate>` + browser reload
5. Changing `crates/websrt/` (library) → `./build.sh gateway` + `./build.sh restart` (production only)

## Workspace structure

- `crates/websrt/` — **library crate**: SRT-over-WebTransport gateway core. Exposes `Gateway` builder, `BrowserSession`, `Broadcaster`, `SrtInitiator`, `Cert`, `Ingester`. Sim-loss behind `sim-loss` feature.
- `crates/websrt-gateway/` — **demo binary**: CLI wrapper around the library. This is what runs in production.
- `crates/srt-wasm/` — browser-side SRT receiver + sender (WASM). Used by both viewer and publisher pages.
- `crates/mpeg2ts-wasm/` — browser-side TS demuxer (WASM). Viewer side.
- `crates/ts-muxer-wasm/` — browser-side TS muxer (WASM). Publisher side (browser→gateway publishing).

## Production readiness scope

**The library (`crates/websrt/`) is the product.** Everything else is a demo or dev tool.

When reviewing, hardening, or auditing code, prioritize by layer:

1. **`crates/websrt/` (library) — production-critical.** This is what downstream developers embed. It must have: correct resource cleanup (no leaked tasks, dropped senders close channels), proper error propagation, no panics on bad input, security primitives exposed as builder options (auth callbacks, origin allowlists, constant-time token comparison, configurable health bind address), input validation on builder methods, and no blocking calls inside async paths. Every public API should be usable in a hardened deployment.

2. **`crates/websrt-gateway/` (demo binary) — should work, doesn't need to be bulletproof.** It demonstrates the library. Loose defaults (auth token in query string, health on 0.0.0.0, no origin check) are acceptable here as long as the *library* exposes the APIs to do better. If the library lacks a capability the demo needs, add it to the library.

3. **`web/` (demo client) — dev tool, lowest priority.** It exists to test against. Memory leaks, reconnect races, imprecise timers, and missing security checks are fine to note but should not block production readiness. The only exception: if a web-side issue reveals a library API gap (e.g., the library doesn't expose stats the client needs), fix the library.

**Rule of thumb:** If a security or robustness issue can be fixed in the library so that any consumer benefits, fix it there. If it's purely demo-app glue, leave it or mention it but don't prioritize it.

## Forked crates (patched, not upstream)

- `maxolgi/srt-rs` (main) — forked from `russelltg/srt-rs` v0.4.4 (commit `d4c08ac`). Eight patches:
  1. `std::time::Instant` → `web_time::Instant` across all source files (WASM compat; no-op on native).
  2. `TimeBase::adjust()` sign flip: upstream applies `-drift` which doubles TSBPD clock error every sync; changed to `+drift`.
  3. TLPKTL `checked_sub` in `protocol/receiver/buffer.rs` to prevent underflow panic early in page life.
  4. Stats tracking methods (`rtt()`, `bandwidth_bps()`, `buffered_packets()`, `buffer_available_packets()`) on ARQ/Receiver/Connection.
  5. Sender buffer edge-case fixes (`send_next_packet` front_packet clamping, `send_packet` bounds-checked index, simplified `number_of_unacked_packets`).
  6. `packet/time.rs` `Sub<TimeSpan>`: `unwrap_or(self)` → `unwrap()` to surface errors.
  7. `protocol/pending_connection/listen.rs`: `Listen::allow_skip_induction` flag + branch in `wait_for_induction` that accepts a Conclusion-first handshake (skips Induction phase for 1-RTT over WebTransport).
  8. `protocol/pending_connection/connect.rs`: `Connect::new_skip_induction` constructor that starts in `ConclusionResponseWait` with a pre-built Conclusion packet (cookie=0, HSREQ extensions).
- `maxolgi/mpeg2ts` (master) — forked from `sile/mpeg2ts` v0.6.0 (commit `82e68d4`). One patch:
  1. `ts/reader.rs`: unknown PIDs return Raw bytes instead of erroring, preventing byte-stream misalignment when the receiver joins mid-stream.
- Both wired via `[patch.crates-io]` in root `Cargo.toml`.

### Inherited QUIC features (via WebTransport)

- **Connection migration (§4.7):** WebTransport inherits QUIC's connection migration. A browser that switches networks (cellular → WiFi) keeps the WebTransport session alive; the SRT layer pauses briefly while packets queue, then resumes. No code change required.
- **Pacing / TSBPD interaction (§4.6):** WebTransport's built-in pacing may delay packets past SRT's TSBPD latency under congestion. The browser's SRT receiver drops those packets as "too late" — correct behavior for live streaming, not a bug.
- **1-RTT handshake (§4.3):** WebSRT skips the SRT Induction phase entirely. WebTransport's TLS layer provides the DoS protection that the SRT cookie mechanism was designed for, so the gateway sends a Conclusion handshake directly. Both `SrtInitiator` (gateway) and `Listen` (browser WASM) use the skip-induction code paths. Saves one RTT (~50-200 ms) on every viewer join.

## Architecture

Gateway is a **dumb SRT repeater**: terminates OBS's SRT/UDP connection, re-originates TS bytes as a new SRT sender to each browser over WebTransport datagrams. TS bytes are never inspected server-side.

Each browser gets its own independent `SrtInitiator` (independent seq numbers, retransmit buffer). Fanout via `tokio::sync::broadcast` in `broadcaster.rs`.

Browser runs the **same** `srt-protocol` + `mpeg2ts` Rust crates compiled to WASM. JS is glue only (WT datagram I/O, WebCodecs, canvas/audio routing).

## Key files

- `crates/websrt/src/gateway.rs` — high-level `Gateway` builder: WT accept loop, session spawn, viewer cap, graceful drain.
- `crates/websrt/src/session.rs` — per-browser session: dual-task split (recv_pump + sender_pump) sharing `SrtInitiator` via `Arc<Mutex<_>>`. LossInjector (sim-loss feature) lives here.
- `crates/websrt/src/srt_sender.rs` — wraps `srt_protocol::Connect` → `DuplexConnection`. `drain()` captures `Action::UpdateStatistics` into `last_stats`.
- `crates/websrt-gateway/src/main.rs` — demo binary: CLI parse, cert-hash.js writing, ingester setup, `Gateway::run()`.
- `crates/srt-wasm/src/lib.rs` — `SrtReceiver` wraps `Listen` → `DuplexConnection`. State in `RefCell`. `handle_datagram(bytes, now_us)` + `poll(now_us)` return `Vec<SrtAction>`.
- `web/src/decode.ts` — H.264 SPS parser (exp-Golomb, High profile), avcC builder, `VideoPipeline`, `OpusAudioPipeline`, `AacAudioPipeline`. AudioWorklet fallback when `MediaStreamTrackGenerator` unavailable.
- `web/src/worker.ts` — Web Worker: runs SrtReceiver + Demuxer off main thread. Datagrams batched (up to 16) before processing. Polls SRT state machine every 10ms.
- `web/src/main.ts` — WT connect, PMT codec detection (AAC 0x0F vs Opus 0x06), connect/stop button state, auto-reconnect with backoff.
- `crates/mpeg2ts-wasm/src/lib.rs` — browser-side TS demuxer (WASM). `TsDemuxer.feed(bytes)` emits `TsEvent`s (PAT/PMT/PES/RA/error). `debug_snapshot()` returns aggregated per-PID analysis: CC errors, TS header flags, PCR interval/jitter, NAL frame-type counts (I/P/B via exp-Golomb slice header parse), packet ring (500 events), error ring. All analysis in Rust; JS renders.
- `crates/mpeg2ts-wasm/src/nal.rs` — NAL parser: start-code scanner, H.264/HEVC nal_unit_type classification, exp-Golomb slice_type → I/P/B.
- `web/src/debug/components/DemuxTab.tsx` — 8th debug panel tab: program table, elementary streams, PTS/DTS, CC errors, TS header flags, PCR, NAL frame-type breakdown, error log. Driven by `store.demuxStats` (mirrors `DebugSnapshot`).
- `web/src/debug/components/PacketTimeline.tsx` — virtualized packet ring (CSS-only, 500-event cap) + click-to-inspect side panel. Color-coded rows (video/audio/PSI/error/other), filter bar, copy-JSON inline buttons.
- `web/src/debug/components/charts/` — 7 demux charts: BitrateChart (per-PID line), PidDonutChart (byte share), CcHeatmap, RaTimeline, PtsJumpSparkline, PcrChart (interval+jitter with 100ms target), NalStackedBar (I/P/B/IDR/etc.).

## Runtime

Gateway runs under supervisord:
- Config: `websrt.conf` → deployed to `/etc/supervisor/conf.d/websrt.conf`
- Logs: `logs/gateway.out.log` + `logs/gateway.err.log`
- Restart: `sudo supervisorctl reread && sudo supervisorctl update && sudo supervisorctl restart websrt`
- **After rebuilding the binary**, must restart supervisord to pick it up
- On boot, gateway writes `web/public/cert-hash.js` (hash for self-signed, null for mkcert)

## Cert modes

- `--cert-mode self` (default): self-signed ECDSA, browser connects with `serverCertificateHashes` (Chrome only)
- `--cert-mode mkcert`: loads PEM files, browser uses normal PKI (Firefox compatible)
- The cert hash changes on every restart — browser must reload page to pick up new hash

## Gotchas

- `web/public/cert-hash.js` is **runtime-generated** (gitignored). Don't commit it.
- `web/wasm/` contents are **gitignored**. Fresh clones must run the WASM build steps before the page works.
- WASM camelCase warnings in `srt-wasm` are **required** by wasm-bindgen — don't "fix" them.
- `SrtIngester.kind` field stores `SrtListener` to keep it alive (drop = close listener). The "never read" warning is intentional.
- `performance.now()` epoch mismatch: browser uses `web_time::Instant` (Performance API), gateway uses `std::time::Instant`. SRT protocol handles this via timestamp fields in packets + clock sync during handshake.
- TSBPD latency negotiation: `max(sender_latency, receiver_latency)` during HSv5. The browser slider solely controls the gateway→browser TSBPD (gateway-side floor is 10ms). `--latency` controls the OBS→gateway ingester SRT latency (default 120ms).
- **Demux debug tab** requires the `mpeg2ts-wasm` rebuild (it consumes `debug_snapshot()`). On stale WASM, the tab renders empty tables — no crash. The old 6-counter `__demuxStats` global is deleted; all demux analysis lives in the `TsDemuxer` WASM struct.
- **Packet inspector hex dump** is deferred — `debug_snapshot()` doesn't include raw packet bytes (memory cost). The inspector shows decoded fields + NAL summary but not a hex dump. Adding it requires a WASM change (`ringHex` field) + rebuild.
- `DebugSnapshot` is a wasm-bindgen struct — **cannot be structured-cloned** across the worker `postMessage` boundary. `worker.ts:getDemuxStats()` reads every field into a POJO and calls `snap.free()` in a `finally` block. Any new snapshot fields must follow this pattern.

## Testing

- `web/smoke.mjs` — Node smoke test for both WASM modules (no browser needed)
- `cargo run -p websrt-gateway --bin wt_hs_probe` — SRT handshake + TS continuity-counter probe (tests NAK/retransmit under sim-loss)
- `cargo run -p websrt-gateway --bin mock_obs` — Sends fixture over SRT to test ingester without real OBS
- `cargo run -p websrt-gateway --bin wt_echo_client` — WT datagram round-trip test

## Git workflow for agents

**Agents commit, humans push.**

- **Commit your work** as you complete each logical unit. Small, focused commits with clear messages. Stage only the files you changed — never `git add -A` blindly.
- **Never push.** Never run `git push`, `gh pr create`, or any remote command. The human reviews the commit log and pushes when ready.
- **One commit per logical change.** If you fix three issues, that's three commits. Write messages that match the repo's existing style (look at `git log --oneline -10`).
- **Before committing:** inspect `git status` and `git diff` to confirm only intended files are staged. Never commit secrets, `cert-hash.js`, or `web/wasm/` contents.
- **Rebase is fine** if you need to fix your own earlier commit (e.g., `git commit --amend`), but never force-push.

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