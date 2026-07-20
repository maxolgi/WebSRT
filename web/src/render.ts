// Canvas-based VideoFrame renderer with PTS-paced presentation.
//
// Decoded frames are queued (small bounded ring). On each
// requestAnimationFrame, the head frame is drawn only when its PTS is due
// — measured against a wall-clock ↔ PTS mapping established on the first
// frame and reset on large gaps (seek, stream restart, tab backgrounding).
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

  private ctx: CanvasRenderingContext2D;
  private canvas: HTMLCanvasElement;

  // Decoded frames awaiting presentation, in decode (PTS) order. Bounded;
  // pushing past RING_CAP closes the oldest frame (latency protection).
  private ring: VideoFrame[] = [];

  private rafId: number | null = null;
  private frameCount = 0;
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

  constructor(canvas: HTMLCanvasElement) {
    this.canvas = canvas;
    const ctx = canvas.getContext('2d');
    if (!ctx) throw new Error('canvas 2d context unavailable');
    this.ctx = ctx;
    this.startRafLoop();
  }

  draw(frame: VideoFrame) {
    this.lastPtsUs = frame.timestamp;
    this.updateClock(frame.timestamp);
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
    if (Math.abs(ptsUs - nowPtsUs) > CanvasRenderer.CLOCK_RESET_US) {
      this.ptsOriginUs = ptsUs;
      this.wallOriginMs = performance.now();
    }
  }

  private startRafLoop() {
    let lastRaf = performance.now();
    const loop = () => {
      const now = performance.now();
      this.lastRafDeltaMs = now - lastRaf;
      lastRaf = now;
      this.present();
      this.rafId = requestAnimationFrame(loop);
    };
    this.rafId = requestAnimationFrame(loop);
  }

  private present() {
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

  getStats(): import('./debug/types').RenderStats {
    return {
      frameCount: this.frameCount,
      droppedLate: this.droppedLate,
      droppedOverflow: this.droppedOld,
      ringLength: this.ring.length,
      ringCap: CanvasRenderer.RING_CAP,
      currentPtsUs: this.lastPtsUs,
      fps: this.lastFps,
      rafDeltaMs: this.lastRafDeltaMs,
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
