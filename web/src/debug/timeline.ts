// Shared time-window + focus-playhead helpers for the debug overlay. Every
// time-based chart slices its data through `windowed()` and draws the focus
// marker through `drawFocusLine()` so a moment picked on any tab (or the
// scrub bar) is marked identically everywhere.

import type { TimeSeriesBucket } from './types'

export function windowed(
  history: TimeSeriesBucket[],
  windowSec: number,
): TimeSeriesBucket[] {
  if (windowSec <= 0 || history.length === 0) return history
  const refT = history[history.length - 1].t
  const cutoff = refT - windowSec * 1000
  return history.filter((b) => b.t >= cutoff)
}

export function xForTime(
  left: number,
  right: number,
  tMin: number,
  tMax: number,
  t: number,
): number {
  return tMax <= tMin
    ? left
    : left + ((t - tMin) / (tMax - tMin)) * (right - left)
}

export function drawFocusLine(
  ctx: CanvasRenderingContext2D,
  x: number,
  top: number,
  bottom: number,
  color = 'rgba(246,102,102,0.85)',
): void {
  ctx.save()
  ctx.strokeStyle = color
  ctx.lineWidth = 1
  ctx.setLineDash([3, 3])
  ctx.beginPath()
  ctx.moveTo(x, top)
  ctx.lineTo(x, bottom)
  ctx.stroke()
  ctx.restore()
}
