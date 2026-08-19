// Canvas-based VideoFrame renderer.
//
// PTS-paced presentation is ON by default. setRenderPacing(false) opts out
// to trust SRT's TSBPD layer entirely: the most recent decoded frame is
// drawn on each requestAnimationFrame.
//
// When pacing is enabled, decoded frames are queued (small bounded ring).
// On each requestAnimationFrame, the head frame is drawn only when its PTS
// is due — measured against a wall-clock ↔ PTS mapping established on the
// first frame and reset on large gaps (seek, stream restart, tab
// backgrounding).
//
// This is necessary because SRT's TSBPD smooths datagram delivery at the
// SRT layer, but downstream stages (WebCodecs decoder output, worker→main
// postMessage batching, encoder pipelining on the publisher side)
// re-introduce bursts. Draining the ring at RAF rate without checking PTS
// plays those bursts at ~display-refresh rate (too fast), then stalls when
// the ring empties — visible as unstable FPS and (on bursty remote paths)
// large droppedOverflow. PTS pacing holds bursts until each frame's slot,
// so the canvas updates at the source frame rate regardless of arrival
// pattern.
//
// Frames that arrive already far past their PTS (decoder emitted a backlog
// after a stall) are dropped as `droppedLate` rather than displayed in a
// burst; the very last frame in a backlog is always kept so the canvas
// doesn't freeze during clear-out. The ring cap remains as a memory
// safety valve for the backgrounded-tab case (RAF throttled to ~1Hz).

export class CanvasRenderer {
  private static readonly RING_CAP = 8;
  // Drop head frame if its PTS is more than this many µs behind the
  // presentation clock — it missed its slot. ~3 RAF cycles at 60 Hz;
  // absorbs jitter without accumulating latency.
  private static readonly LATE_DROP_US = 50_000;
  // Reset the PTS↔wall clock mapping when a frame's PTS diverges from the
  // expected presentation time by more than this — indicates seek,
  // stream restart, or recovery from a backgrounded tab.
  private static readonly CLOCK_RESET_US = 1_000_000;
  // Proportional slew of the PTS↔wall mapping toward the incoming PTS
  // stream (per arriving frame, clamped). Source clocks drift vs wall
  // time (measured 1.00094 on one encoder — +0.94 ms/s); a fixed mapping
  // accumulates the drift until the ring pins full with every head frame
  // still "in the future" — canvas frozen, tail overflow-dropped at the
  // incoming rate — because CLOCK_RESET_US is never reached while the
  // deficit stays under 1 s. The slew keeps the mapping phase-locked to
  // the stream: gain 0.01/frame absorbs ~1.2 ms/s of drift at <2 ms
  // steady-state error, and the 3 ms/frame clamp heals a pin (up to
  // RING_CAP of deficit) in ~1 s without visible cadence wobble.
  private static readonly CLOCK_SLEW_GAIN = 0.01;
  private static readonly CLOCK_SLEW_MAX_US = 3_000;

  private ctx: CanvasRenderingContext2D;
  private canvas: HTMLCanvasElement;

  // Decoded frames awaiting presentation, in decode (PTS) order. Bounded;
  // pushing past RING_CAP closes the oldest frame (latency protection).
  private ring: VideoFrame[] = [];

  private rafId: number | null = null;
  private frameCount = 0;

  // Arrivals-per-rAF-interval histogram (rolling). Each rAF tick records how
  // many frames arrived since the previous tick (capped at 4+). With the
  // cap-1 baseline ring, 1 arrival per interval is the only lossless case:
  // 0-arrival intervals are stalls, ≥2-arrival intervals force drops. The
  // share of 0/≥2 slots quantifies arrival clustering (TSBPD release
  // quantization, B-frame reorder bursts, postMessage batching) vs rAF.
  private static readonly ARRIVAL_RING = 512; // ~8.5 s of intervals at 60 Hz
  private static readonly ARRIVAL_MAX_BUCKET = 4;
  private arrivalRing = new Uint8Array(CanvasRenderer.ARRIVAL_RING);
  private arrivalIdx = 0;
  private arrivalsSinceRaf = 0;
  private droppedOld = 0;
  private droppedLate = 0;
  private lastFpsTime = performance.now();
  private lastFps = 0;
  private lastRafDeltaMs = 16.67;
  private lastPtsUs: number | null = null;

  // Wall-clock ↔ PTS mapping. Established on first frame; reset on large
  // gap so the presentation clock tracks the source instead of drifting.
  private ptsOriginUs: number | null = null;
  private wallOriginMs = 0;

  // PTS-paced presentation is on by default (matches the pre-pacing
  // f2caba7 baseline that paces canvas draws by PTS). Disable via
  // setRenderPacing(false) to trust SRT's TSBPD layer instead.
  private renderPacing = true;

  constructor(canvas: HTMLCanvasElement) {
    this.canvas = canvas;
    const ctx = canvas.getContext('2d');
    if (!ctx) throw new Error('canvas 2d context unavailable');
    this.ctx = ctx;
    this.startRafLoop();
  }

  /** Toggle PTS-paced presentation. Off by default (trust SRT TSBPD). */
  setRenderPacing(enabled: boolean) {
    this.renderPacing = enabled;
    if (!enabled) {
      // Drain ring to a single latest frame (baseline behavior).
      while (this.ring.length > 1) {
        this.ring.shift()!.close();
      }
      // Reset pacing clock anchors so the mapping is re-established if
      // pacing is re-enabled later.
      this.ptsOriginUs = null;
    }
  }

  draw(frame: VideoFrame) {
    this.lastPtsUs = frame.timestamp;
    this.arrivalsSinceRaf++;
    if (!this.renderPacing) {
      // Baseline: keep only the latest frame; drop any undrawn
      // predecessor. The frame is drawn on the next RAF and consumed.
      if (this.ring.length > 0) {
        this.ring[0].close();
        this.droppedOld++;
        this.ring[0] = frame;
      } else {
        this.ring.push(frame);
      }
      return;
    }
    // When the ring is empty, the decoder paused before this frame (B-frame
    // reorder: WebCodecs holds B-frames until their forward reference
    // arrives, then emits a burst ~reorder-depth after their presentation
    // slots). Re-anchor the presentation clock to this frame so the burst's
    // leading B-frame is treated as on-time rather than dropped as late —
    // the following frames in the burst still pace by PTS via present().
    if (this.ring.length === 0) {
      this.ptsOriginUs = frame.timestamp;
      this.wallOriginMs = performance.now();
    } else {
      this.updateClock(frame.timestamp);
    }
    this.ring.push(frame);
    while (this.ring.length > CanvasRenderer.RING_CAP) {
      const old = this.ring.shift()!;
      old.close();
      this.droppedOld++;
    }
  }

  /** Current video PTS in microseconds, or null if no frame received yet. */
  currentPtsUs(): number | null {
    return this.lastPtsUs;
  }

  /**
   * Establish or reset the wall-clock ↔ PTS mapping. The first frame sets
   * the origin; subsequent frames reset it if their PTS is far from the
   * current presentation time (seek, stream restart, post-backgrounding).
   */
  private updateClock(ptsUs: number) {
    if (this.ptsOriginUs === null) {
      this.ptsOriginUs = ptsUs;
      this.wallOriginMs = performance.now();
      return;
    }
    const nowPtsUs = this.ptsOriginUs + (performance.now() - this.wallOriginMs) * 1000;
    const errUs = ptsUs - nowPtsUs;
    if (Math.abs(errUs) > CanvasRenderer.CLOCK_RESET_US) {
      this.ptsOriginUs = ptsUs;
      this.wallOriginMs = performance.now();
      return;
    }
    const slewUs = Math.max(
      -CanvasRenderer.CLOCK_SLEW_MAX_US,
      Math.min(CanvasRenderer.CLOCK_SLEW_MAX_US, errUs * CanvasRenderer.CLOCK_SLEW_GAIN),
    );
    this.ptsOriginUs += slewUs;
  }

  private startRafLoop() {
    let lastRaf = performance.now();
    const loop = () => {
      const now = performance.now();
      this.lastRafDeltaMs = now - lastRaf;
      lastRaf = now;
      this.arrivalRing[this.arrivalIdx] = Math.min(
        this.arrivalsSinceRaf,
        CanvasRenderer.ARRIVAL_MAX_BUCKET,
      );
      this.arrivalIdx = (this.arrivalIdx + 1) % CanvasRenderer.ARRIVAL_RING;
      this.arrivalsSinceRaf = 0;
      this.present();
      this.rafId = requestAnimationFrame(loop);
    };
    this.rafId = requestAnimationFrame(loop);
  }

  private present() {
    if (!this.renderPacing) {
      if (this.ring.length === 0) return;
      const frame = this.ring.shift()!;
      if (this.canvas.width !== frame.displayWidth) {
        this.canvas.width = frame.displayWidth;
      }
      if (this.canvas.height !== frame.displayHeight) {
        this.canvas.height = frame.displayHeight;
      }
      this.ctx.drawImage(frame, 0, 0);
      frame.close();
      this.frameCount++;
      if (this.frameCount % 30 === 0) {
        this.lastFps = (30 * 1000) / (performance.now() - this.lastFpsTime);
        this.lastFpsTime = performance.now();
      }
      return;
    }
    if (this.ptsOriginUs === null || this.ring.length === 0) return;
    const nowPtsUs = this.ptsOriginUs + (performance.now() - this.wallOriginMs) * 1000;

    // Drop frames that missed their slot. We always keep at least the
    // newest frame so the canvas never freezes during a backlog clear-out.
    while (this.ring.length > 1 && this.ring[0].timestamp < nowPtsUs - CanvasRenderer.LATE_DROP_US) {
      const old = this.ring.shift()!;
      old.close();
      this.droppedLate++;
    }

    // Hold frames whose PTS is still in the future; they'll be drawn on a
    // subsequent RAF cycle when their time arrives.
    if (this.ring[0].timestamp > nowPtsUs) return;

    const frame = this.ring.shift()!;

    if (this.canvas.width !== frame.displayWidth) {
      this.canvas.width = frame.displayWidth;
    }
    if (this.canvas.height !== frame.displayHeight) {
      this.canvas.height = frame.displayHeight;
    }
    this.ctx.drawImage(frame, 0, 0);
    frame.close();

    this.frameCount++;
    if (this.frameCount % 30 === 0) {
      this.lastFps = (30 * 1000) / (performance.now() - this.lastFpsTime);
      this.lastFpsTime = performance.now();
    }
  }

  getStats(): import('./shared/types').RenderStats {
    const buckets = new Array<number>(CanvasRenderer.ARRIVAL_MAX_BUCKET + 1).fill(0);
    for (let i = 0; i < this.arrivalRing.length; i++) {
      buckets[this.arrivalRing[i]]++;
    }
    return {
      frameCount: this.frameCount,
      droppedLate: this.droppedLate,
      droppedOverflow: this.droppedOld,
      ringLength: this.ring.length,
      ringCap: this.renderPacing ? CanvasRenderer.RING_CAP : 1,
      currentPtsUs: this.lastPtsUs,
      fps: this.lastFps,
      rafDeltaMs: this.lastRafDeltaMs,
      arrivalHistogram: buckets,
    };
  }

  destroy() {
    if (this.rafId !== null) cancelAnimationFrame(this.rafId);
    this.rafId = null;
    for (const f of this.ring) { try { f.close(); } catch {} }
    this.ring = [];
    this.ptsOriginUs = null;
  }
}
