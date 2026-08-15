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
cargo fmt --check                 # verify Rust formatting (run `cargo fmt --all` to fix)

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

1. Forked `srt-protocol` (`maxolgi/srt-rs`) changes → `cargo update -p srt-protocol -p srt-tokio` (pull new commit) then `./build.sh srt-protocol` (rebuilds BOTH the gateway binary AND srt-wasm + copies pkg to web/wasm/)
2. Changing only `web/src/*.ts` / `*.tsx` → Vite hot-reloads, no rebuild needed
3. Changing `crates/srt-wasm/src/lib.rs` → `./build.sh wasm srt` + browser reload
4. Changing `crates/mpeg2ts-wasm/` or `crates/ts-muxer-wasm/` → `./build.sh wasm <crate>` + browser reload
5. Changing `crates/websrt/` (library) → `./build.sh gateway` + `./build.sh restart` (production only)
6. Changing `crates/websrt-gateway/src/gui.rs` or `log_buffer.rs` → `./build.sh gateway` (no WASM or web rebuild needed)

## Workspace structure

- `crates/websrt/` — **library crate**: SRT-over-WebTransport gateway core. Exposes `Gateway` builder, `BrowserSession`, `Broadcaster`, `SrtInitiator`, `Cert`, `Ingester`. Sim-loss behind `sim-loss` feature.
- `crates/websrt-gateway/` — **reference implementation**: CLI binary built on the library. Runs in production under supervisord.
- `crates/srt-wasm/` — browser-side SRT receiver + sender (WASM). Used by both viewer and publisher pages.
- `crates/mpeg2ts-wasm/` — browser-side TS demuxer (WASM). Viewer side.
- `crates/ts-muxer-wasm/` — browser-side TS muxer (WASM). Publisher side (browser→gateway publishing).

## Architecture layers

- **`crates/websrt/` (library)** — the embeddable product. `Gateway` builder,
  sessions, SRT state machine, broadcast fanout, security hooks.
- **`crates/websrt-gateway/` (reference implementation)** — the canonical binary.
  CLI parse, cert setup, ingester wiring, health endpoint. Runs under supervisord.
  Launches an eframe (egui) GUI by default; `--no-gui` for headless CLI mode.
- **`web/` (demo UI)** — player page and stream page. Browser-side demos that
  exercise the gateway end-to-end, plus the debug overlay.

## Forked crates (patched, not upstream)

Two external crates are forked and patched. Both are wired via `[patch.crates-io]`
in root `Cargo.toml`. Cargo.lock pins each to a specific commit hash — **new
commits pushed to a fork are NOT pulled automatically**. You must run `cargo update`
(see [Updating forked crates](#updating-forked-crates) below).

**Local clones already exist — use them, don't re-clone:**

- `~/srt-rs` — branch `main` (pushes to `maxolgi/srt-rs`)
- `~/mpeg2ts` — branch `master` (pushes to `maxolgi/mpeg2ts`)

**Where fixes go:** `mpeg2ts` is parse-side only (the muxer is WebSRT-native in
`ts-muxer-wasm`). Any bug in TS/PES/PSI parsing semantics is fixed **in the fork**
so every consumer benefits; wrappers must not re-implement fork logic — if a
wrapper needs internal arithmetic, the fork exposes it publicly instead. Same
rule for srt-protocol/srt-tokio.

- **`maxolgi/srt-rs`** (branch: `main`) — forked from `russelltg/srt-rs` v0.4.4 (commit `d4c08ac`).
  Provides `srt-protocol` + `srt-tokio`. Twelve patches:
  1. `std::time::Instant` → `web_time::Instant` across all source files (WASM compat; no-op on native). Also adds `getrandom` with the `js` feature for `cfg(target_arch = "wasm32")`.
  2. **`TimeBase::adjust()` sign flip** (coupled with patch 6): upstream applies `-drift` via the **broken `Sub<TimeSpan>`** (patch 6 — `Sub` is a copy-paste of `Add`, so it actually computes `+drift`). Upstream is correct **by accident**. The fork fixes `Sub` to actually subtract, which means `adjust` must change `-drift` → `+drift` to preserve the same runtime behavior. These two patches must be applied (or reverted) together.
  3. TLPKTL `checked_sub` in `protocol/receiver/buffer.rs`: `now - (tsbpd_latency + tsbpd_tolerance)` panics via underflow early in page life (before latency has elapsed). Changed to `checked_sub` → `Option<Instant>`, returning "no too-late packets" when the subtraction underflows.
  4. **Stats population + accessors**: upstream declares `rx_loss_data`, `rx_loss_bytes`, and `rx_bandwidth` in `SocketStatistics` but **never assigns them anywhere** — they are always 0. Fork populates `rx_loss_data`/`rx_loss_bytes` in the `ReceivedWithLoss` path, computes `rx_bandwidth` via byte-delta, and adds accessor methods (`rtt()`, `bandwidth_bps()`, `buffered_packets()`, `buffer_available_packets()`) on ARQ/Receiver/Sender.
  5. Sender buffer `SeqNumber` wrapping-subtraction underflows (3 fixes): (a) `send_next_packet` clamps `next_send` to `front_packet` when `next_send < front`, (b) `send_packet` bounds-checks `seq_number - front_packet` and returns `None` instead of wrapping to a huge index + panicking, (c) `number_of_unacked_packets` returns 0 instead of wrapping when `next_send < front`.
  6. **`packet/time.rs` `Sub<TimeSpan>` for `Instant`** (coupled with patch 2): upstream `Sub<TimeSpan>` is **byte-for-byte identical to `Add<TimeSpan>`** — a copy-paste bug where both branches add. Fork swaps the branches so `Sub` actually subtracts, and changes `unwrap()` → `unwrap_or(self)` to avoid panics on `Instant` underflow.
  7. `protocol/pending_connection/listen.rs`: `Listen::allow_skip_induction` flag + branch in `wait_for_induction` that accepts a Conclusion-first handshake (skips Induction phase for 1-RTT over WebTransport).
  8. `protocol/pending_connection/connect.rs`: `Connect::new_skip_induction` constructor that starts in `ConclusionResponseWait` with a pre-built Conclusion packet (cookie=0, HSREQ extensions).
  9. `settings/connection.rs` + `protocol/time/rtt.rs` + `protocol/receiver/arq.rs` + `protocol/sender/buffer.rs`: `ConnInitSettings.initial_rtt: Option<Duration>` field that seeds `SendBuffer.rtt` and `ARQ.rtt` via `Rtt::from_mean_duration` (variance = mean/4). Upstream populates `ConnectionSettings.rtt` during the handshake but **never feeds it to `SendBuffer` or `ARQ`** (both use `Rtt::default()`). The fork makes both consumers read the setting, and adds a way to override it from QUIC's smoothed RTT (needed because skip-induction has no Induction round-trip to measure RTT).
  10. `protocol/sender/buffer.rs`: CC-aware retransmit skip in `send_next_lost_packet` — if `now + rtt.mean()` exceeds the packet's TSBPD deadline (`timestamp + latency_window`), the retransmit is skipped (receiver will drop it as too-late anyway). The packet is popped from the lost list before the check, so the skip doesn't block subsequent retransmits.
  11. `protocol/sender/buffer.rs` + `protocol/sender/mod.rs` + `connection/mod.rs`: Populate `SocketStatistics.tx_average_rtt` from `SendBuffer.rtt` in `update_statistics`. The field was declared but **never assigned** by upstream — only `rx_average_rtt` was populated. Without this, publisher-side stats show RTT=0.
  12. `connection/mod.rs` + `packet/control/srt.rs`: Handle `CongestionWarning`, `PeerError`, and unknown SRT control packets without panicking (was `todo!()`/`unimplemented!()`). Advertise `TLPKTDROP` + `NAKREPORT` in `SrtShakeFlags::SUPPORTED` (both are enabled by default but were not advertised to the peer).
- **`maxolgi/mpeg2ts`** (branch: `master`) — forked from `sile/mpeg2ts` v0.6.0 (commit `82e68d4`). One patch:
  1. `ts/reader.rs`: unknown PIDs return Raw bytes instead of erroring, preventing byte-stream misalignment when the receiver joins mid-stream.

### Updating forked crates

Cargo.lock pins each fork to a specific commit hash. **New commits pushed to a
fork are NOT pulled automatically.** After the human pushes changes to a fork, you must
explicitly update Cargo.lock:

    # After the human pushes to maxolgi/srt-rs (branch: main):
    cargo update -p srt-protocol -p srt-tokio

    # After the human pushes to maxolgi/mpeg2ts (branch: master):
    cargo update -p mpeg2ts

    # Or update both at once:
    ./build.sh update-forks

After updating `srt-protocol`, you must also rebuild srt-wasm (it depends on
srt-protocol). Run `./build.sh srt-protocol` for the full gateway + WASM rebuild.

**DO NOT edit files in `~/.cargo/git/checkouts/`** — cargo overwrites that
directory on `cargo clean` or `cargo update`. To edit fork source, use the
existing local clones (`~/srt-rs`, `~/mpeg2ts` — see above):

1. Work in the clone, on the correct branch (`main` for srt-rs, `master` for mpeg2ts)
2. Commit your changes, then ask the human to push to GitHub
3. After the human pushes, run `cargo update -p <crate>` in WebSRT to pull the new commit

### Inherited QUIC features (via WebTransport)

- **Connection migration (§4.7):** WebTransport inherits QUIC's connection migration. A browser that switches networks (cellular → WiFi) keeps the WebTransport session alive; the SRT layer pauses briefly while packets queue, then resumes. No code change required.
- **Pacing / TSBPD interaction (§4.6):** WebTransport's built-in pacing may delay packets past SRT's TSBPD latency under congestion. The browser's SRT receiver drops those packets as "too late" — correct behavior for live streaming, not a bug.
- **1-RTT handshake (§4.3):** WebSRT skips the SRT Induction phase entirely. WebTransport's TLS layer provides the DoS protection that the SRT cookie mechanism was designed for, so the gateway sends a Conclusion handshake directly. Both `SrtInitiator` (gateway) and `Listen` (browser WASM) use the skip-induction code paths. Saves one RTT (~50-200 ms) on every viewer join.
- **RTT seeding (§4.5):** The gateway reads QUIC's smoothed RTT via `wtransport::Connection::rtt()` and seeds SRT's EWMA via `ConnInitSettings.initial_rtt`. The browser reads `WebTransport.getStats().smoothedRtt`. Both seed SRT's `SendBuffer.rtt` and `ARQ.rtt` for accurate cold-start retransmit timing and congestion window estimation.
- **CC-aware retransmit (§4.5):** The SRT sender skips NAK-triggered retransmits whose predicted arrival time (`now + RTT`) exceeds the packet's TSBPD deadline. Prevents wasting bandwidth on retransmits the receiver will drop as too-late. Combined with RTT seeding, implements the "transport-aware retransmission decisions" the draft recommends.

## Architecture

Gateway is a **multi-stream SRT repeater**: a single `StreamRegistry` maps stream names to independent `Broadcaster`s, each fanning its stream out to N viewers via `tokio::sync::broadcast`. Routing is query-param based on one fixed `/wt` WebTransport path — viewers select a stream with `?stream=<name>` or `?subscribe=<name>`, browser publishers with `?publish=<name>`, and the SRT/OBS ingester derives the stream name from OBS's `?streamid=`. Each browser session gets its own independent `SrtInitiator` (independent seq numbers, retransmit buffer); a session can simultaneously view one stream and publish another. TS bytes are never modified server-side.

Browser runs the **same** `srt-protocol` + `mpeg2ts` Rust crates compiled to WASM. JS is glue only (WT datagram I/O, WebCodecs, canvas/audio routing).

## Key files

- `crates/websrt/src/gateway.rs` — high-level `Gateway` builder: WT accept loop, session spawn, viewer cap, graceful drain.
- `crates/websrt/src/session.rs` — per-browser session: dual-task split (recv_pump + sender_pump) sharing `SrtInitiator` via `Arc<Mutex<_>>`. LossInjector (sim-loss feature) lives here.
- `crates/websrt/src/srt_sender.rs` — wraps `srt_protocol::Connect` → `DuplexConnection`. `drain()` captures `Action::UpdateStatistics` into `last_stats`.
- `crates/websrt-gateway/src/main.rs` — reference binary: CLI parse, `--no-gui` branching, `run_gateway()` (cert, cert-hash.js, ingester, health server, gateway run task).
- `crates/websrt-gateway/src/gui.rs` — eframe (egui) GUI app: config form mirroring all CLI options, Start/Stop buttons, live stats from `GatewayStatsHandle`, scrolling log panel. Falls back to CLI if no display.
- `crates/websrt-gateway/src/log_buffer.rs` — `LogBuffer` ring buffer + `MakeWriter` impl for capturing tracing output into the GUI log panel (second `fmt` layer alongside stdout).
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

Gateway runs under supervisord (must use `--no-gui` in the supervisord config):
- Config: `websrt.conf` → deployed to `/etc/supervisor/conf.d/websrt.conf`
- Logs: `/var/log/websrt/gateway.out.log` + `/var/log/websrt/gateway.err.log`
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
- **QUIC stats** require the `quinn` feature on `wtransport` (enabled in root `Cargo.toml`). The gateway logs per-session QUIC stats (cwnd, rtt, lost_packets, congestion_events) at session start and close.
- **GUI** (eframe/egui) launches by default. `--no-gui` skips to CLI mode. On headless servers (no DISPLAY), eframe fails and falls back to CLI automatically. Linux requires X11/Wayland dev packages (installed by `install-prereqs.sh`). The gateway startup logic is shared between both modes via `run_gateway()`.

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

If you are not sure about something dont overthink it and use internet search