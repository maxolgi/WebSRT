# Gateway & Browser Pipeline Hardening Plan

## Problem Summary

Two symptoms reported:
1. **Latency buildup** — after several minutes, the browser's video/audio drifts later and later with no recovery.
2. **Poor multi-client performance** — even 2 concurrent viewers degrade quality.

Root causes identified by code analysis:

| # | Cause | Location | Severity |
|---|-------|----------|----------|
| A | Video frames drawn immediately on decode — no rAF/PTS presentation scheduling | `web/src/render.ts:16`, `web/src/main.ts:212` | Critical |
| B | AudioWorklet PCM queue unbounded — grows monotonically | `web/src/decode.ts:585-615` | Critical |
| C | `pendingNalus` unbounded — grows if decoder configure stalls | `web/src/decode.ts:279,313,318` | High |
| D | No backpressure anywhere — decoder queue depth never checked | `web/src/decode.ts:346` | High |
| E | SRT TSBPD release timestamp discarded — pacing lost | `crates/srt-wasm/src/lib.rs:328` | Medium |
| F | OBS reconnect not supported — single accept, never re-listens | `crates/gateway/src/ingest/srt.rs:37-41` | High |
| G | Half-dead broadcaster — new viewers connect into a black hole | `crates/gateway/src/broadcaster.rs:52-54`, `server.rs:127-137` | High |
| H | `active_conns` Vec never shrinks — memory leak on disconnect | `crates/gateway/src/server.rs:174` | Medium |
| I | Process-global `PUSHED` counter — racy under multi-viewer | `crates/gateway/src/srt_sender.rs:145` | Low |

---

## Phase 1 — Browser Latency Fixes (P0)

These changes directly fix the "getting late after minutes" symptom. No gateway or WASM changes required — pure TypeScript.

### 1A: rAF-Gated Video Presentation with PTS Clock

**File:** `web/src/render.ts` (full rewrite, ~80 lines)

**Problem:** `CanvasRenderer.draw(frame)` calls `drawImage` + `frame.close()` synchronously inside the `VideoDecoder.output` callback. Frames are drawn as fast as they decode. The decoder runs ahead of realtime, so the displayed frame creeps earlier in media time relative to audio. No wall-clock alignment.

**Solution:** Introduce a presentation scheduler that:
1. Buffers decoded `VideoFrame`s in a small ring (cap: 4 frames).
2. Uses `requestAnimationFrame` to present frames at the right wall-clock time.
3. Maps each frame's `timestamp` (PTS in microseconds) to wall-clock using a clock-anchor established at first frame.
4. Drops frames that are already past their presentation window (catch-up instead of accumulate).
5. Drops frames if the ring is full (decoder is ahead — skip to newest).

**Code sketch:**

```typescript
// web/src/render.ts

export class CanvasRenderer {
  private ctx: CanvasRenderingContext2D;
  private canvas: HTMLCanvasElement;

  // Presentation buffer: at most N decoded frames awaiting their PTS time.
  private ring: VideoFrame[] = [];
  private static readonly RING_CAP = 4;

  // Clock anchor: maps media-time (PTS µs) → wall-clock (performance.now() ms).
  // Established on first frame. Video PTS = frame.timestamp (µs).
  private ptsAnchorUs: number | null = null;   // first frame's PTS
  private wallAnchorMs: number = 0;             // wall-clock when first frame was due

  // Playout delay: how far behind realtime we intentionally present.
  // This absorbs jitter. Should be <= TSBPD latency.
  private playoutDelayMs: number;

  private rafId: number | null = null;
  private frameCount = 0;
  private droppedLate = 0;
  private droppedOverflow = 0;
  private lastFpsTime = performance.now();

  constructor(canvas: HTMLCanvasElement, playoutDelayMs = 100) {
    this.canvas = canvas;
    const ctx = canvas.getContext('2d');
    if (!ctx) throw new Error('canvas 2d context unavailable');
    this.ctx = ctx;
    this.playoutDelayMs = playoutDelayMs;
    this.startRafLoop();
  }

  /**
   * Called from VideoDecoder.output. Enqueues the frame for scheduled
   * presentation. If the ring is full, we're decoding ahead — drop the
   * oldest frame to make room (overflow drop).
   */
  draw(frame: VideoFrame) {
    // Establish clock anchor on first frame.
    if (this.ptsAnchorUs === null) {
      this.ptsAnchorUs = frame.timestamp;
      this.wallAnchorMs = performance.now() + this.playoutDelayMs;
    }

    if (this.ring.length >= CanvasRenderer.RING_CAP) {
      // Decoder is ahead of presentation. Drop oldest, keep newest.
      const old = this.ring.shift()!;
      old.close();
      this.droppedOverflow++;
    }

    this.ring.push(frame);
  }

  private startRafLoop() {
    const loop = () => {
      this.presentDueFrames();
      this.rafId = requestAnimationFrame(loop);
    };
    this.rafId = requestAnimationFrame(loop);
  }

  private presentDueFrames() {
    if (this.ring.length === 0 || this.ptsAnchorUs === null) return;

    const nowMs = performance.now();
    // Find the frame that should be showing right now.
    // PTS-to-wall mapping: wallMs = wallAnchorMs + (frame.pts - ptsAnchor) / 1000
    // Present the latest frame whose wallMs <= nowMs.

    let bestIdx = -1;
    for (let i = 0; i < this.ring.length; i++) {
      const ptsDeltaUs = this.ring[i].timestamp - this.ptsAnchorUs;
      const presentAtMs = this.wallAnchorMs + ptsDeltaUs / 1000;
      if (presentAtMs <= nowMs) {
        bestIdx = i;
      } else {
        break; // ring is sorted by PTS (decoder guarantees decode order)
      }
    }

    if (bestIdx < 0) return; // no frame is due yet

    // Present the best frame, close all before it (they're late).
    const showFrame = this.ring[bestIdx];
    for (let i = 0; i < bestIdx; i++) {
      this.ring[i].close();
      this.droppedLate++;
    }

    // Resize canvas if needed.
    if (this.canvas.width !== showFrame.displayWidth) {
      this.canvas.width = showFrame.displayWidth;
    }
    if (this.canvas.height !== showFrame.displayHeight) {
      this.canvas.height = showFrame.displayHeight;
    }
    this.ctx.drawImage(showFrame, 0, 0);
    showFrame.close();

    // Keep only frames after bestIdx.
    this.ring = this.ring.slice(bestIdx + 1);

    this.frameCount++;
    if (this.frameCount % 30 === 0) {
      const fps = (30 * 1000) / (nowMs - this.lastFpsTime);
      this.lastFpsTime = nowMs;
      console.debug(
        `render fps: ${fps.toFixed(1)} (frame ${this.frameCount}, ` +
        `late ${this.droppedLate}, overflow ${this.droppedOverflow}, ` +
        `ring ${this.ring.length})`
      );
    }
  }

  destroy() {
    if (this.rafId !== null) cancelAnimationFrame(this.rafId);
    for (const f of this.ring) f.close();
    this.ring = [];
  }
}
```

**Wiring changes in `web/src/main.ts`:**

```typescript
// Line ~210: pass playoutDelayMs (use a fraction of TSBPD latency)
renderer = new CanvasRenderer(canvas, Math.min(150, +latencySlider.value / 2));

// Line ~143 teardown(): destroy renderer to cancel rAF
renderer?.destroy();
renderer = null;
```

**Verification:**
- Play stream for 10+ minutes. Video should stay at constant latency.
- `render fps` log should show low `late` and `overflow` counts in steady state.
- Observe that `ring` stays at 0-2 in steady state (not accumulating).

---

### 1B: Bounded AudioWorklet Queue with Drop Policy

**File:** `web/src/decode.ts` (lines 584-615, the `PCM_PLAYER_WORKLET` string)

**Problem:** The AudioWorklet processor pushes every received sample into an unbounded JS array. If decode rate exceeds playout rate (burst after loss recovery, tab throttle, clock correction burst), the queue grows without limit and audio latency grows monotonically. Also `q.slice(avail)` is O(n) per quantum.

**Solution:** Replace the unbounded array with a fixed-size ring buffer. When full, drop oldest samples. Use `Float32Array` indexed ring instead of JS array for O(1) operations.

**Replacement code for `PCM_PLAYER_WORKLET`:**

```javascript
const PCM_PLAYER_WORKLET = `
class PcmPlayerProcessor extends AudioWorkletProcessor {
  constructor() {
    super();
    // Per-channel ring buffer.
    this.queues = [];
    this.heads = [];   // read index
    this.tails = [];   // write index
    this.counts = [];  // current sample count
    // ~0.5s at 48kHz — enough for jitter, small enough to not drift far.
    this.CAP = 24000;
    this.port.onmessage = (e) => {
      const planes = e.data.planes;
      for (let ch = 0; ch < planes.length; ch++) {
        if (!this.queues[ch]) {
          this.queues[ch] = new Float32Array(this.CAP);
          this.heads[ch] = 0;
          this.tails[ch] = 0;
          this.counts[ch] = 0;
        }
        const incoming = planes[ch];
        const q = this.queues[ch];
        let tail = this.tails[ch];
        let count = this.counts[ch];
        for (let i = 0; i < incoming.length; i++) {
          if (count >= this.CAP) {
            // Buffer full — drop oldest by advancing head.
            this.heads[ch] = (this.heads[ch] + 1) % this.CAP;
            count--;
          }
          q[tail] = incoming[i];
          tail = (tail + 1) % this.CAP;
          count++;
        }
        this.tails[ch] = tail;
        this.counts[ch] = count;
      }
    };
  }
  process(inputs, outputs) {
    const output = outputs[0];
    const framesNeeded = output[0].length;
    for (let ch = 0; ch < output.length; ch++) {
      if (!this.queues[ch]) {
        for (let i = 0; i < framesNeeded; i++) output[ch][i] = 0;
        continue;
      }
      const q = this.queues[ch];
      let head = this.heads[ch];
      let count = this.counts[ch];
      const avail = Math.min(framesNeeded, count);
      // If we have more than ~50ms buffered beyond what's needed, skip ahead
      // to reduce accumulated latency (2400 samples at 48kHz).
      const TARGET_MS = 50;
      const targetSamples = Math.round(this.CAP * 0); // keep it simple
      if (count > framesNeeded + 2400) {
        const skip = count - framesNeeded - 2400;
        head = (head + skip) % this.CAP;
        count -= skip;
      }
      const toRead = Math.min(framesNeeded, count);
      for (let i = 0; i < framesNeeded; i++) {
        if (i < toRead) {
          output[ch][i] = q[head];
          head = (head + 1) % this.CAP;
        } else {
          output[ch][i] = 0;
        }
      }
      this.heads[ch] = head;
      this.counts[ch] = count - toRead;
    }
    return true;
  }
}
registerProcessor('pcm-player', PcmPlayerProcessor);
`;
```

**Key changes:**
- Fixed `Float32Array(this.CAP)` ring instead of unbounded `[]`.
- O(1) read/write via head/tail indices.
- When buffer exceeds `framesNeeded + 2400` (~50ms headroom), skips ahead to drain excess latency.
- Drop-oldest when buffer is completely full.

**Verification:**
- On Firefox with AudioWorklet path: play for 10+ minutes. Audio latency should stay constant.
- No `q.slice()` allocations.
- After a simulated loss burst (enable `--sim-loss 5` on gateway), audio should recover to baseline latency within ~1 second, not accumulate.

---

### 1C: Decode Queue Backpressure + pendingNalus Cap

**File:** `web/src/decode.ts`

**Problem 1:** `VideoPipeline.emitAu()` calls `this.decoder.decode(chunk)` without checking `decodeQueueSize`. If the decoder falls behind, the internal queue grows unbounded (browser-dependent limit, then errors or drops).

**Problem 2:** `pendingNalus` array has no cap. If `VideoDecoder.configure` fails or stalls, NALUs accumulate forever.

**Solution 1: Check `decodeQueueSize` before decode, skip non-keyframes when behind:**

In `VideoPipeline.emitAu()` (line ~331), before `this.decoder.decode(chunk)`:

```typescript
private emitAu(nalus: NalUnit[], pts: number | null, _hint: boolean) {
    if (!this.decoder || this.decoder.state !== 'configured') return;
    const hasIdr = nalus.some((n) => n.type === NAL_IDR);
    const decodeNalus = nalus.filter((n) => n.type >= 1 && n.type <= 5);
    if (decodeNalus.length === 0) return;
    if (!this.seenKeyframe && !hasIdr) return;
    if (hasIdr) this.seenKeyframe = true;

    // Backpressure: if decoder queue is deep, skip delta frames.
    // Always decode keyframes so we can resync.
    const qDepth = this.decoder.decodeQueueSize;
    if (!hasIdr && qDepth > 8) {
      // Decoder is behind — skip this delta frame to let it catch up.
      return;
    }

    const data = nalusToLengthPrefixed(decodeNalus);
    const tsUs = pts != null ? Math.floor(pts / 90) : undefined;
    const chunk = new EncodedVideoChunk({
      type: hasIdr ? 'key' : 'delta',
      timestamp: tsUs ?? 0,
      data,
    });
    try {
      this.decoder.decode(chunk);
      this.decodedCount++;
    } catch (e) {
      this.cb.onError(e);
      this.reset();
    }
  }
```

**Solution 2: Cap `pendingNalus` to prevent unbounded growth:**

In `VideoPipeline.feed()`, after each `this.pendingNalus.push(...)`:

```typescript
// Cap pending NALUs to prevent unbounded growth during configure stalls.
private static readonly PENDING_CAP = 30;

// After each push to pendingNalus:
if (this.pendingNalus.length > VideoPipeline.PENDING_CAP) {
  // Drop oldest non-keyframe batches — we need a keyframe to decode anyway.
  // Keep only the last few batches that might contain an IDR.
  const dropped = this.pendingNalus.splice(
    0,
    this.pendingNalus.length - VideoPipeline.PENDING_CAP
  );
  console.warn(`pendingNalus overflow: dropped ${dropped.length} batches`);
}
```

Add this check in both places where `pendingNalus.push(...)` occurs (lines ~313 and ~318).

**Verification:**
- With `--sim-loss 10` on the gateway, video should not stall permanently.
- `decodeQueueSize` should stay low (< 5) in steady state.
- No unbounded memory growth visible in DevTools Memory tab over 10+ minutes.

---

### 1D: Audio decode queue backpressure

**File:** `web/src/decode.ts`, `AudioPipelineBase.feedFrame()` (line ~545)

**Problem:** Same as video — no `decodeQueueSize` check on audio decoder.

**Solution:**

```typescript
protected feedFrame(chunk: EncodedAudioChunk) {
    if (this.decoder && this.decoder.decodeQueueSize > 20) {
      // Audio decoder is behind — skip this frame to prevent buildup.
      return;
    }
    try {
      this.decoder?.decode(chunk);
      this.packetsDecoded++;
    } catch (e) {
      this.cb.onError(e);
    }
  }
```

Threshold of 20 is generous for audio (Opus 20ms frames = 400ms buffered). Adjust if needed.

**Verification:**
- Audio latency should not grow during loss recovery bursts.

---

## Phase 2 — Gateway Robustness (P1)

These changes make the gateway survive OBS disconnects/reconnects and clean up after viewer disconnects. Rust changes require `cargo build --release -p gateway` + supervisord restart.

### 2A: OBS Reconnect Loop

**File:** `crates/gateway/src/ingest/srt.rs`

**Problem:** `bind_with_addr()` (line 31-53) accepts exactly one OBS connection via `incoming.incoming().next().await` and never returns to the incoming stream. If OBS disconnects, the gateway is dead.

**Solution:** Convert `SrtIngester` from single-accept to a reconnect loop. The `Ingester` trait stays the same — the loop happens internally.

**Approach:** When `next_message()` returns `None` (socket closed), re-enter the accept loop instead of returning `None`.

```rust
// crates/gateway/src/ingest/srt.rs

use super::{Ingester, TsMessage};
use anyhow::{anyhow, Result};
use async_trait::async_trait;
use futures::StreamExt;
use srt_tokio::{SrtListener, SrtSocket};
use std::time::Duration;

enum Kind {
    Listener(SrtListener),
    Caller(String),  // store addr for reconnect
}

pub struct SrtIngester {
    kind: Kind,
    socket: Option<SrtSocket>,  // None when disconnected
    bind_addr: String,
}

impl SrtIngester {
    pub async fn bind(port: u16) -> Result<Self> {
        Self::bind_with_addr(format!("0.0.0.0:{port}")).await
    }

    pub async fn bind_with_addr(addr: impl AsRef<str>) -> Result<Self> {
        let addr_str = addr.as_ref().to_string();
        let (listener, _) = SrtListener::builder()
            .bind(addr_str.as_str())
            .await
            .map_err(|e| anyhow!("srt listener bind: {e}"))?;
        tracing::info!("SRT listener bound, awaiting OBS connection…");

        // Accept first connection immediately so startup is not delayed.
        let socket = Self::accept_one(&listener).await?;

        Ok(Self {
            kind: Kind::Listener(listener),
            socket: Some(socket),
            bind_addr: addr_str,
        })
    }

    async fn accept_one(listener: &SrtListener) -> Result<SrtSocket> {
        loop {
            // Re-acquire the incoming stream each time — srt-tokio's API
            // gives a fresh incoming() per call.
            let mut incoming = listener.incoming();
            let request = incoming
                .incoming()
                .next()
                .await
                .ok_or_else(|| anyhow!("srt listener closed"))?;
            let remote = request.remote();
            let stream_id = request.stream_id().map(|s| s.to_string());
            tracing::info!(%remote, ?stream_id, "SRT connection accepted from OBS");
            match request.accept(None).await {
                Ok(socket) => return Ok(socket),
                Err(e) => {
                    tracing::warn!(?e, "SRT accept failed; retrying");
                    tokio::time::sleep(Duration::from_secs(1)).await;
                }
            }
        }
    }

    pub async fn call(addr: impl AsRef<str>) -> Result<Self> {
        let addr_str = addr.as_ref().to_string();
        tracing::info!(addr = %addr_str, "SRT caller: dialing OBS…");
        let socket_addr: srt_protocol::options::SocketAddress = addr_str
            .as_str()
            .try_into()
            .map_err(|e| anyhow!("invalid SRT address {addr_str}: {e:?}"))?;
        let socket = SrtSocket::builder()
            .call(socket_addr, None)
            .await
            .map_err(|e| anyhow!("srt call to {addr_str}: {e}"))?;
        tracing::info!(addr = %addr_str, "SRT caller: connected to OBS");
        Ok(Self {
            kind: Kind::Caller(addr_str),
            socket: Some(socket),
            bind_addr: String::new(),
        })
    }

    async fn reconnect(&mut self) -> Result<SrtSocket> {
        match &self.kind {
            Kind::Listener(listener) => {
                tracing::info!("SRT: OBS disconnected; waiting for reconnect…");
                Self::accept_one(listener).await
            }
            Kind::Caller(addr) => {
                tracing::info!(addr, "SRT caller: re-dialing OBS…");
                loop {
                    let socket_addr: srt_protocol::options::SocketAddress = addr
                        .as_str()
                        .try_into()
                        .map_err(|e| anyhow!("invalid address: {e:?}"))?;
                    match SrtSocket::builder().call(socket_addr, None).await {
                        Ok(s) => {
                            tracing::info!(addr, "SRT caller: reconnected to OBS");
                            return Ok(s);
                        }
                        Err(e) => {
                            tracing::warn!(?e, addr, "SRT reconnect failed; retrying in 2s");
                            tokio::time::sleep(Duration::from_secs(2)).await;
                        }
                    }
                }
            }
        }
    }
}

#[async_trait]
impl Ingester for SrtIngester {
    async fn next_message(&mut self) -> Result<Option<TsMessage>> {
        loop {
            if let Some(ref mut socket) = self.socket {
                match socket.next().await {
                    Some(Ok(msg)) => return Ok(Some(msg)),
                    Some(Err(e)) => {
                        tracing::warn!(?e, "srt recv error; will attempt reconnect");
                        self.socket = None;
                    }
                    None => {
                        tracing::info!("srt socket closed; attempting reconnect");
                        self.socket = None;
                    }
                }
            }
            // No active socket — reconnect.
            match self.reconnect().await {
                Ok(s) => self.socket = Some(s),
                Err(e) => {
                    tracing::error!(?e, "reconnect failed; retrying in 2s");
                    tokio::time::sleep(Duration::from_secs(2)).await;
                }
            }
        }
    }
}
```

**Note:** The `incoming.incoming()` API may differ depending on srt-tokio version. Check `vendor/srt-protocol` / `srt-tokio` for the exact `SrtListener` API. The key point: after a socket closes, call `self.reconnect()` which re-enters the accept loop (listener mode) or re-dials (caller mode), and `next_message()` never returns `None` unless we want to stop.

**IMPORTANT:** Check whether `SrtListener::incoming()` returns a fresh stream each call or must be stored. If it must be stored, change `Kind::Listener` to hold `(SrtListener, Pin<Box<dyn Stream...>>)`.

**Verification:**
1. Start gateway, connect OBS, verify streaming works.
2. Kill OBS (Ctrl-C). Gateway logs "waiting for reconnect" — no crash.
3. Restart OBS. Gateway reconnects within seconds.
4. Browser viewers should auto-reconnect (existing WT reconnect logic) and resume.

---

### 2B: Dead Broadcaster Detection + Subscriber Rejection

**Files:** `crates/gateway/src/broadcaster.rs`, `crates/gateway/src/server.rs`

**Problem:** When the ingester source ends (`Ok(None)` from `next_message`), the broadcaster task exits and the `broadcast::Sender` is dropped. But the `Arc<Broadcaster>` stays in the server's `Mutex<Option<...>>`. New `subscribe()` calls check `receiver_count() < max_viewers` — which is now 0 — so they succeed. But `subscribe()` on a closed channel gives a receiver that immediately returns `Closed`, so the viewer session instantly ends.

Worse: with the OBS reconnect fix (2A), `next_message()` never returns `None`, so the broadcaster task never exits from `Ok(None)`. But it can still exit from repeated `Err` (the 100ms sleep loop runs forever). And the OBS disconnect/reconnect window is a dead zone for new viewers.

**Solution:** Add an `is_alive` flag to `Broadcaster`. The ingest task sets it to `false` before exiting. `subscribe()` checks it.

```rust
// crates/gateway/src/broadcaster.rs

use std::sync::atomic::{AtomicBool, Ordering};

pub struct Broadcaster {
    tx: broadcast::Sender<TsMessage>,
    pub max_viewers: usize,
    alive: Arc<AtomicBool>,
}

impl Broadcaster {
    pub fn spawn<I>(mut ingester: I, max_viewers: usize, capacity: usize) -> Arc<Self>
    where
        I: Ingester + Send + 'static,
    {
        let (tx, _rx0) = broadcast::channel(capacity);
        let alive = Arc::new(AtomicBool::new(true));
        let alive_clone = alive.clone();
        let broadcaster = Arc::new(Self {
            tx: tx.clone(),
            max_viewers,
            alive,
        });
        let tx2 = tx.clone();
        tokio::spawn(async move {
            let mut sent = 0u64;
            loop {
                match ingester.next_message().await {
                    Ok(Some(msg)) => {
                        sent += 1;
                        if sent <= 3 || sent % 100 == 0 {
                            tracing::debug!(sent, bytes = msg.1.len(), "broadcaster: forwarded message");
                        }
                        let _ = tx2.send(msg);
                    }
                    Ok(None) => {
                        tracing::info!("ingester source ended; broadcaster shutting down");
                        break;
                    }
                    Err(e) => {
                        tracing::warn!(?e, "ingester error");
                        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
                    }
                }
            }
            alive_clone.store(false, Ordering::SeqCst);
            tracing::info!("broadcaster task exited");
        });
        broadcaster
    }

    /// Subscribe a new viewer. Returns `None` if the session cap is reached
    /// or the broadcaster is dead (source ended).
    pub fn subscribe(&self) -> Option<ViewerRx> {
        if !self.alive.load(Ordering::SeqCst) {
            return None;
        }
        if self.tx.receiver_count() >= self.max_viewers {
            return None;
        }
        Some(ViewerRx {
            rx: self.tx.subscribe(),
            lag_count: 0,
        })
    }

    pub fn is_alive(&self) -> bool {
        self.alive.load(Ordering::SeqCst)
    }

    // ... viewer_count() unchanged
}
```

**Server-side change** (`server.rs`): when `subscribe()` returns `None` and the broadcaster is dead, log it differently than "cap reached":

```rust
// server.rs, in the accept loop (line ~127):
let viewer = {
    let guard = broadcaster.lock().await;
    match guard.as_ref() {
        Some(b) if !b.is_alive() => {
            drop(guard);
            tracing::warn!("session rejected: source is dead");
            session_request.too_many_requests().await;
            continue;
        }
        Some(b) => match b.subscribe() {
            Some(v) => v,
            None => {
                drop(guard);
                tracing::warn!("session rejected: viewer cap reached");
                session_request.too_many_requests().await;
                continue;
            }
        },
        None => {
            drop(guard);
            tracing::warn!("session rejected: source not ready yet");
            session_request.too_many_requests().await;
            continue;
        }
    }
};
```

**Verification:**
1. With OBS disconnected, browser should get a clear rejection, not connect-then-instantly-disconnect.
2. Server logs should say "source is dead" vs "viewer cap reached" vs "source not ready yet".

---

### 2C: Connection Cleanup on Session Exit

**File:** `crates/gateway/src/server.rs`

**Problem:** `active_conns` (line 97-98) is a `Vec<Connection>` that only grows. When a browser disconnects naturally, the session task ends but the `Connection` clone stays in the Vec until ctrl-c.

**Solution:** Remove the `active_conns` Vec entirely. It's only used for graceful shutdown (close all connections on ctrl-C). Instead, close connections via the session's own task lifecycle. The `BrowserSession::run()` already returns when either pump finishes — the `Connection` is dropped then, which closes the WT session.

If we need the graceful-drain-on-shutdown behavior (ctrl-C should close all sessions), the simplest fix is: don't store `Connection` clones at all. Instead, rely on the process exit to close QUIC connections. Or use a `tokio::sync::watch` to signal all sessions to shut down.

**Minimal fix — track session count instead of connections:**

```rust
// server.rs

// Replace:
//   let active_conns: Arc<Mutex<Vec<wtransport::Connection>>> = ...

// With a session counter:
let active_sessions = Arc::new(Mutex::new(0u32));

// On accept (line ~174):
{
    let mut count = active_sessions.lock().await;
    *count += 1;
}

// Pass active_sessions to BrowserSession::spawn so it can decrement on exit.
// BrowserSession::spawn gets an extra param: Arc<Mutex<u32>>.

// On ctrl-C shutdown (line ~180):
let count = *active_sessions.lock().await;
tracing::info!(count, "shutting down with active sessions");
// Sessions will be closed when their Connection drops on process exit,
// or we can close them via a broadcast shutdown signal.
```

**Simpler alternative — just leak the Vec approach but clean up on session exit:**

Give `BrowserSession::spawn` an `Arc<Mutex<Slab<Option<Connection>>>>` (or use a DashMap). On session exit, remove the entry. The session's `run()` function is already an async block — add a `Drop` guard or explicit cleanup at the end.

**Simplest approach — skip the Vec, close on ctrl-C via task abort:**

Store `JoinHandle<()>` for each session in the Vec instead of `Connection`. On ctrl-C, abort all handles + sleep 2s. The `Connection` drops when the task is aborted.

```rust
// server.rs, replace active_conns type:
let session_handles: Arc<Mutex<Vec<tokio::task::JoinHandle<()>>>> =
    Arc::new(Mutex::new(Vec::new()));

// BrowserSession::spawn returns JoinHandle:
let handle = BrowserSession::spawn(connection, viewer, cli.sim_loss, cli.sim_seed, cli.latency);
session_handles.lock().await.push(handle);

// On ctrl-C:
let handles = std::mem::take(&mut *session_handles.lock().await);
for h in &handles { h.abort(); }
// Connections drop when tasks abort.
tokio::time::sleep(Duration::from_secs(1)).await;
```

This requires `BrowserSession::spawn` to return `JoinHandle<()>` instead of `()`.

**Change in `session.rs`:**

```rust
pub fn spawn(
    conn: Connection,
    viewer: ViewerRx,
    sim_loss: u8,
    sim_seed: u64,
    latency_ms: u64,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        if let Err(e) = Self::run(conn, viewer, sim_loss, sim_seed, latency_ms).await {
            tracing::info!(?e, "browser session ended");
        }
    })
}
```

**Verification:**
1. Connect 2 browsers, disconnect one. Memory should not grow.
2. Connect/disconnect 20 times. Check `htop` or `/proc/<pid>/status` — VmRSS should plateau, not grow linearly.
3. ctrl-C should still close all sessions gracefully.

---

### 2D: Per-Session PUSHED Counter

**File:** `crates/gateway/src/srt_sender.rs` (line 145)

**Problem:** `static PUSHED: AtomicU64` is process-global. Under multi-viewer, the debug log throttling (`n < 3 || n % 100 == 0`) interleaves across sessions.

**Solution:** Move the counter to a per-session field or remove it. It's debug-only — simplest fix is to make it an instance field.

```rust
// srt_sender.rs, in SrtInitiator struct:
pub struct SrtInitiator {
    state: InitiatorState,
    remote: SocketAddr,
    #[allow(dead_code)]
    local_addr: IpAddr,
    last_stats: Option<SocketStatistics>,
    pushed: u64,  // per-session counter
}

// In push_message():
pub fn push_message(&mut self, msg: (Instant, Bytes)) -> Vec<SenderAction> {
    let mut out = Vec::new();
    if let InitiatorState::Connected(duplex) = &mut self.state {
        let now = Instant::now();
        self.pushed += 1;
        if self.pushed <= 3 || self.pushed % 100 == 0 {
            tracing::debug!(pushed = self.pushed, bytes = msg.1.len(), "push_message: to sender");
        }
        duplex.handle_data_input(now, Some(msg));
        drain(duplex, now, &mut out, &mut self.last_stats);
    }
    out
}
```

Initialize `pushed: 0` in both `new()` and `new_with_settings()`.

**Verification:** With 2 viewers, debug logs should show per-session counting (each session logs its own `pushed = 1, 2, 3, 100...`).

---

## Phase 3 — Multi-Client Performance (P2)

Lower priority — the Phase 1 changes address the latency symptom directly. These changes improve throughput under load.

### 3A: Increase Broadcast Capacity for Multi-Viewer

**File:** `crates/gateway/src/server.rs` (line 21)

**Problem:** `BROADCAST_CAPACITY = 2048` (~1.2s at 1700 msg/sec). With 2+ viewers, if one is briefly slow (GC pause, tab switch), it gets `Lagged`, misses messages, and its SRT receiver NAKs for retransmits — adding CPU load to the gateway.

**Solution:** Increase to `4096` (~2.4s). This costs ~3.5MB memory (4096 × ~900 bytes) but gives more headroom for multi-viewer jitter. Alternatively, make it configurable via `--broadcast-capacity`.

```rust
const BROADCAST_CAPACITY: usize = 4096;  // ~2.4s at 1700 msg/sec
```

**Verification:** With 2 viewers under `--sim-loss 5`, `lag` count in stats should be lower.

---

### 3B: Tune sender_pump Drain Batch Size

**File:** `crates/gateway/src/session.rs` (line 251)

**Problem:** The `try_recv` drain loop is capped at 16 messages per tick. At 1700 msg/sec with a 2ms ticker, that's 16 msg/tick × 500 tick/sec = 8000 msg/sec capacity. At higher bitrates or with 2+ viewers sharing the broadcast, this can be a bottleneck.

**Solution:** Increase to 32. The loop is inside the mutex lock, so don't go too high — each `push_message` does protocol work.

```rust
if drained >= 32 { break; }
```

**Verification:** With 2 viewers, check that `tx_buffered_data` in stats stays bounded (not accumulating).

---

### 3C: Reduce Mutex Contention (Future — Not Immediate)

**File:** `crates/gateway/src/session.rs`

**Problem:** `recv_pump` and `sender_pump` share `Arc<Mutex<SrtInitiator>>`. Under heavy ACK traffic + retransmit load, the recv_pump can hold the lock when the sender_pump's 2ms tick fires, causing the sender to skip a tick.

**Current status:** The dual-task split (documented in session.rs:9-13) was specifically designed to fix this. The `MissedTickBehavior::Skip` prevents burst-catchup. This is not the primary latency issue.

**If needed later:** Use `parking_lot::Mutex` (fair, faster) instead of `tokio::sync::Mutex`. Or restructure to use a single task with careful `select!` ordering (the original approach that was abandoned due to ACK starvation). The current split is the right call for now.

**No change needed in this phase.**

---

## Implementation Order

```
Phase 1A (rAF video presentation)        ← highest impact on latency drift
Phase 1B (bounded audio worklet)         ← highest impact on Firefox
Phase 1C (decode backpressure + cap)     ← prevents buildup enablers
Phase 1D (audio decode backpressure)     ← quick win
    ↓ verify: play 10+ min, latency stable
Phase 2A (OBS reconnect loop)            ← gateway robustness
Phase 2B (dead broadcaster detection)    ← gateway robustness
Phase 2C (connection cleanup)            ← memory leak
Phase 2D (per-session PUSHED counter)    ← cosmetic
    ↓ verify: OBS disconnect/reconnect, memory plateau
Phase 3A (broadcast capacity)            ← multi-viewer headroom
Phase 3B (drain batch size)              ← multi-viewer throughput
    ↓ verify: 2+ viewers for 10+ min, no degradation
```

---

## Build & Test Commands

After Phase 1 (web only):
```bash
cd web && npx tsc --noEmit
cd web && npm run dev
# Open browser, play stream for 10+ minutes, check render fps log
```

After Phase 2-3 (Rust):
```bash
cargo build --release -p gateway
sudo supervisorctl restart srtsocket
# Test OBS disconnect/reconnect
# Test 2+ viewers
# Check memory with: watch -n5 'cat /proc/$(pgrep gateway)/status | grep VmRSS'
```

After WASM changes (if any):
```bash
(cd crates/srt-wasm && wasm-pack build --target web --release)
cp crates/srt-wasm/pkg/* web/wasm/srt-wasm/
```

---

## Risk Assessment

| Change | Risk | Mitigation |
|--------|------|------------|
| 1A: rAF video presentation | Frames could drop if rAF is throttled (background tab) | Acceptable — background tab means user isn't watching. Ring cap prevents memory growth. |
| 1B: Audio ring buffer | Drop-oldest could cause audible pops during buffer overflow | 24000-sample cap (~0.5s) is generous. Skip-ahead only triggers at >50ms excess. |
| 1C: decodeQueueSize skip | Could drop frames during normal jitter if threshold too low | Threshold of 8 is ~133ms at 60fps — generous. Keyframes always pass. |
| 2A: OBS reconnect loop | `SrtListener` API may not support re-accept from the same listener | Test with `mock_obs` binary first. If API differs, restructure to re-bind. |
| 2C: JoinHandle Vec | Aborting tasks is slightly less graceful than close() | 1s sleep after abort gives QUIC time to send CONNECTION_CLOSE. |
