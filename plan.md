# Plan: Multi-publisher SRT listener

## Goal

Support multiple concurrent OBS/SRT publishers, each pushing a different stream identified by OBS's `?streamid=`. Viewers subscribe to a specific stream via `?stream=<name>`. Today the demo binary accepts only one SRT connection at a time.

## Why the current limit exists

It is **not** an `srt-rs` / `srt-tokio` limitation. `SrtListener::builder().bind()` returns `(SrtListener, SrtIncoming)` and `SrtIncoming::incoming()` yields a multi-connection request stream.

The constraint lives in WebSRT's `Ingester` trait boundary:
- `Ingester::next_message(&mut self) -> Result<Option<TsMessage>>` (`crates/websrt/src/ingest/mod.rs:32-34`) is a single-stream pull API.
- `SrtIngester` (`crates/websrt/src/ingest/srt.rs:19-28`) stores one `SrtSocket`, calls `accept_one` once at construction (`srt.rs:54`), and `reconnect()` accepts one replacement (`srt.rs:139`).
- `Broadcaster::spawn` runs a single `while let Some(msg) = ingester.next_message().await` loop per name (`broadcaster.rs:77-90`).
- `StreamRegistry` maps one name → one `Broadcaster` (`stream_registry.rs:33-37`).

So: one Ingester ↔ one Broadcaster ↔ one stream name. Everything **below** the `Ingester` trait (registry, fanout, viewer routing, per-stream caps, stats) already handles N concurrent streams. The missing piece is a component sitting **above** the trait that owns the listener, runs the accept loop, and registers each accepted connection under its streamid.

Per AGENTS.md ("if a capability can be added to the library so that any consumer benefits, add it there"), this belongs in `crates/websrt/`, not in demo glue.

## Design decisions

- **Missing `?streamid=`** → reject the connection.
- **Duplicate streamid** (name already alive) → reject the second publisher.
- **Name only** — no auth secret parsing in this round. Browser-side WT path already has `--auth-token`; SRT auth is a follow-up.
- **Caller mode untouched.** Existing `SrtIngester` stays for caller mode (gateway dials OBS). Only listener mode gets multi-accept.
- **Per-stream `TsContinuityChecker` preserved** via a wrap callback on the listener service, so CC error sources stay attributable during development.

## File-by-file changes

### 1. `crates/websrt/src/ingest/srt.rs` — extract `SrtConnectionIngester`

Pull the socket-reading half of `SrtIngester::next_message` (`srt.rs:182-224`) into a new public `SrtConnectionIngester { socket: SrtSocket }` implementing `Ingester`. No `kind`, no `reconnect`, no listener — just read until EOF/error then return `None`.

Keep `SrtIngester` intact for caller mode (it can optionally wrap `SrtConnectionIngester` internally to dedupe, but keep that surgical).

**Verify:** `cargo check -p websrt`.

### 2. `crates/websrt/src/ingest/srt_listener.rs` — new file, the accept loop

```rust
pub struct SrtListenerService {
    listener: SrtListener,
    incoming: SrtIncoming,
    latency: Duration,
}

impl SrtListenerService {
    pub async fn bind(addr, latency) -> Result<Self>

    /// Run the accept loop. `wrap` is invoked on each accepted connection's
    /// SrtConnectionIngester before publishing, so the caller can install a
    /// TsContinuityChecker (or any Ingester decorator) and capture its stats.
    pub async fn serve<I, F>(
        self,
        registry: Arc<StreamRegistry>,
        cancel: CancellationToken,
        wrap: F,
    )
    where
        F: Fn(&str, SrtConnectionIngester) -> I,
        I: Ingester + Send + 'static,
}
```

Loop body per `incoming.incoming().next().await`:

1. `stream_id = request.stream_id()`. If `None` or empty → `tracing::warn!`, continue (do not call `accept`).
2. `request.accept(None).await` → on error, warn + continue.
3. `name = stream_id.to_string()`.
4. `wrapped = wrap(&name, SrtConnectionIngester::new(socket))`.
5. `if !registry.try_publish_ingester(&name, wrapped) { warn "duplicate streamid"; }` — duplicate-reject happens *inside* `try_publish` (returns false if name alive). On false, the wrapped ingester is dropped → socket closes.
6. Loop until `cancel.cancelled()` or `incoming` ends.

Hold `SrtListener` inside the task; dropping it on cancel closes the listener.

### 3. `crates/websrt/src/stream_registry.rs` — add `try_publish_ingester`

```rust
/// Publish only if no live stream currently holds `name`. Returns false
/// (no insert) if a live broadcaster exists. Replaces a dead entry.
pub fn try_publish_ingester<I: Ingester + Send + 'static>(&self, name: &str, ingester: I) -> bool
```

- Lock `streams`; if `streams.get(name).map(|b| b.is_alive()).unwrap_or(false)` → return false.
- Else spawn `Broadcaster` + insert; return true.
- Leave existing `publish_ingester` (force-replace) for file-fixture and browser-publisher paths.

**Verify:** unit test asserting (a) first publish returns true, (b) second publish of same name while alive returns false, (c) publish after death returns true.

### 4. `crates/websrt/src/gateway.rs` — expose via builder

- Add `GatewayBuilder::srt_listener_addr(impl Into<String>)` and `srt_listener_latency(Duration)`. Stored as `Option<SrtListenerConfig>` on `Gateway`.
- Expose `Gateway::streams_handle()` returning `Arc<StreamRegistry>` (or pass via `source_handle()` — match whatever `main.rs:303`'s `gateway.source_handle()` already returns; it has `publish_stream`).
- In `Gateway::run()`: if `srt_listener` is configured, `tokio::spawn(listener.serve(streams, cancel_child, wrap_fn))` where `cancel_child` is tied to the existing drain token.

**Verify:** `cargo check -p websrt`.

### 5. `crates/websrt-gateway/src/main.rs` — switch listener-mode to multi-accept

Change `InputMode::Srt` + `SrtMode::Listener` branch (`main.rs:312-320`) to construct `SrtListenerService::bind(...)` and call `.serve(...)` with a `wrap` closure:

```rust
move |name, conn| {
    let checker = TsContinuityChecker::new(conn);
    per_stream_stats.lock().unwrap().insert(name.to_string(), checker.stats_handle());
    checker
}
```

Replace `ts_stats: Arc<Mutex<Option<TsStatsHandle>>>` with `Arc<Mutex<HashMap<String, TsStatsHandle>>>` keyed by stream name. Update health-endpoint consumers accordingly (search `ts_stats` usages).

Caller-mode branch (`main.rs:321-334`) stays unchanged.

**Verify:** `cargo build -p websrt-gateway`.

### 6. `crates/websrt/tests/srt_multi.rs` — new integration test

- Bind `SrtListenerService` on `127.0.0.1:0`.
- Spawn two `mock_obs`-style publishers with `?streamid=foo` and `?streamid=bar` (reuse the existing `mock_obs` binary pattern).
- Assert: `registry.is_alive("foo")` && `registry.is_alive("bar")`; `subscribe("foo")` and `subscribe("bar")` return `Some`.
- Spawn a third publisher with `?streamid=foo` → assert the existing "foo" stream is *not* replaced (still the first publisher's bytes; the third's socket closes).
- Spawn a publisher with no streamid → assert no new stream is registered.

**Verify:** `cargo test -p websrt --test srt_multi`.

### 7. `AGENTS.md` — update stale architecture paragraph

The "single dumb SRT repeater" wording in the Architecture section is stale. Replace with: gateway is a multi-stream SRT repeater; multiple OBS publishers route by `?streamid=`; viewers subscribe by `?stream=<name>`.

## Success criteria

- Two OBS instances publishing `?streamid=foo` / `?streamid=bar` concurrently → both live, viewers subscribe to either independently.
- Third OBS with `?streamid=foo` while first is live → rejected at SRT accept layer.
- OBS with no `?streamid=` → rejected.
- Caller mode (`--srt-mode caller`) unchanged.
- Per-stream CC stats visible in health output, keyed by stream name.
- `./build.sh check` + `./build.sh test` both green.

## Risk notes

- **`SrtIncoming` ownership**: current `SrtIngester` stores `SrtListener` purely to keep it alive (`Kind::Listener(_, incoming, _)` with `dead_code` allow at `srt.rs:15`). New `SrtListenerService` does the same — keep `listener` in the task scope. If `srt-tokio` requires `SrtListener` to outlive spawned sub-tasks, the accept loop must hold it by value.
- **Shutdown race**: `try_publish` happens inside the accept loop; if `cancel` fires mid-accept, a just-published stream is still drained by `shutdown_all` (existing mechanism — `Broadcaster::shutdown` is signal-driven).
- **`mock_obs` binary**: confirm it supports `?streamid=`. If not, the test needs a small extension to that binary (one CLI arg).

## Key file references

| Concern | File | Lines |
|---|---|---|
| `Ingester` trait | `crates/websrt/src/ingest/mod.rs` | 32-34 |
| Current single-conn SRT ingester | `crates/websrt/src/ingest/srt.rs` | 19-28 (struct), 63-90 (accept), 182-224 (read loop) |
| `TsContinuityChecker` wrap | `crates/websrt/src/ingest/continuity.rs` | 46-75 |
| `StreamRegistry` (multi-stream map) | `crates/websrt/src/stream_registry.rs` | 33-37 (struct), 74-88 (publish_ingester) |
| `Broadcaster` (per-stream fanout) | `crates/websrt/src/broadcaster.rs` | 29-43, 53-90 |
| Gateway builder + accept loop | `crates/websrt/src/gateway.rs` | 33-43, 288-378 |
| Demo binary SRT wiring | `crates/websrt-gateway/src/main.rs` | 302-356 |
| Viewer WT URL construction | `web/src/shared/viewer.ts` | 283-291 |
