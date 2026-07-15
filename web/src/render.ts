// Canvas-based VideoFrame renderer with rAF-gated presentation scheduling.
//
// Decoded frames are buffered in a small ring and presented via
// requestAnimationFrame at the wall-clock time corresponding to their PTS.
// This prevents the decoder from running ahead of realtime and causing
// latency drift over time.

export class CanvasRenderer {
  private ctx: CanvasRenderingContext2D;
  private canvas: HTMLCanvasElement;

  private ring: VideoFrame[] = [];
  private static readonly RING_CAP = 8;

  private ptsAnchorUs: number | null = null;
  private wallAnchorMs = 0;
  private playoutDelayMs: number;
  private lastFrameTsUs: number | null = null;

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

  draw(frame: VideoFrame) {
    if (
      this.lastFrameTsUs !== null &&
      (frame.timestamp < this.lastFrameTsUs ||
        frame.timestamp - this.lastFrameTsUs > 5_000_000)
    ) {
      this.ptsAnchorUs = null;
    }

    if (this.ptsAnchorUs === null) {
      this.ptsAnchorUs = frame.timestamp;
      this.wallAnchorMs = performance.now() + this.playoutDelayMs;
    }

    this.lastFrameTsUs = frame.timestamp;

    if (this.ring.length >= CanvasRenderer.RING_CAP) {
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

    let bestIdx = -1;
    for (let i = 0; i < this.ring.length; i++) {
      const ptsDeltaUs = this.ring[i].timestamp - this.ptsAnchorUs;
      const presentAtMs = this.wallAnchorMs + ptsDeltaUs / 1000;
      if (presentAtMs <= nowMs) {
        bestIdx = i;
      } else {
        break;
      }
    }

    if (bestIdx < 0) return;

    const showFrame = this.ring[bestIdx];
    for (let i = 0; i < bestIdx; i++) {
      this.ring[i].close();
      this.droppedLate++;
    }

    if (this.canvas.width !== showFrame.displayWidth) {
      this.canvas.width = showFrame.displayWidth;
    }
    if (this.canvas.height !== showFrame.displayHeight) {
      this.canvas.height = showFrame.displayHeight;
    }
    this.ctx.drawImage(showFrame, 0, 0);
    showFrame.close();

    this.ring.splice(0, bestIdx + 1);

    this.frameCount++;
    if (this.frameCount % 30 === 0) {
      const fps = (30 * 1000) / (nowMs - this.lastFpsTime);
      this.lastFpsTime = nowMs;
      console.debug(
        `render fps: ${fps.toFixed(1)} (frame ${this.frameCount}, ` +
        `late ${this.droppedLate}, overflow ${this.droppedOverflow}, ` +
        `ring ${this.ring.length})`,
      );
    }
  }

  destroy() {
    if (this.rafId !== null) cancelAnimationFrame(this.rafId);
    this.rafId = null;
    for (const f of this.ring) f.close();
    this.ring = [];
  }
}
