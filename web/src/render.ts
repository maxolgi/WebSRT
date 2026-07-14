// Canvas-based VideoFrame renderer.

export class CanvasRenderer {
  private ctx: CanvasRenderingContext2D | null = null;
  private canvas: HTMLCanvasElement;
  private frameCount = 0;
  private lastTime = performance.now();

  constructor(canvas: HTMLCanvasElement) {
    this.canvas = canvas;
    const ctx = canvas.getContext('2d');
    if (!ctx) throw new Error('canvas 2d context unavailable');
    this.ctx = ctx;
  }

  draw(frame: VideoFrame) {
    if (!this.ctx) return;
    // Resize canvas to match frame dimensions if they changed.
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
      const now = performance.now();
      const fps = (30 * 1000) / (now - this.lastTime);
      this.lastTime = now;
      console.debug(`render fps: ${fps.toFixed(1)} (frame ${this.frameCount})`);
    }
  }
}
