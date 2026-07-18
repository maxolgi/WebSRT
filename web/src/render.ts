// Canvas-based VideoFrame renderer.
//
// SRT's TSBPD already provides jitter buffering and delivery timing. The
// renderer simply draws the most recent decoded frame on each
// requestAnimationFrame — no PTS-based scheduling, no playout delay, no
// ring-buffer capacity math. This avoids a class of bugs where high-fps
// streams overflow a fixed-size ring before any frame becomes "due".

export class CanvasRenderer {
  private ctx: CanvasRenderingContext2D;
  private canvas: HTMLCanvasElement;

  // The most recent decoded frame awaiting presentation. The decoder's output
  // callback stores a frame here; the RAF loop draws and closes it. If a new
  // frame arrives before the previous one was drawn, the previous one is
  // dropped (the viewer skips to the latest, maintaining low latency).
  private pending: VideoFrame | null = null;

  private rafId: number | null = null;
  private frameCount = 0;
  private droppedOld = 0;
  private lastFpsTime = performance.now();
  private lastFps = 0;
  private lastRafDeltaMs = 16.67;
  private lastPtsUs: number | null = null;

  constructor(canvas: HTMLCanvasElement) {
    this.canvas = canvas;
    const ctx = canvas.getContext('2d');
    if (!ctx) throw new Error('canvas 2d context unavailable');
    this.ctx = ctx;
    this.startRafLoop();
  }

  draw(frame: VideoFrame) {
    if (this.pending) {
      this.pending.close();
      this.droppedOld++;
    }
    this.pending = frame;
    this.lastPtsUs = frame.timestamp;
  }

  /** Current video PTS in microseconds, or null if no frame received yet. */
  currentPtsUs(): number | null {
    return this.lastPtsUs;
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
    const frame = this.pending;
    if (!frame) return;
    this.pending = null;

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
      droppedLate: 0,
      droppedOverflow: this.droppedOld,
      ringLength: this.pending ? 1 : 0,
      ringCap: 1,
      currentPtsUs: this.lastPtsUs,
      fps: this.lastFps,
      rafDeltaMs: this.lastRafDeltaMs,
    };
  }

  destroy() {
    if (this.rafId !== null) cancelAnimationFrame(this.rafId);
    this.rafId = null;
    if (this.pending) {
      this.pending.close();
      this.pending = null;
    }
  }
}
