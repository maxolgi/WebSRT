import { useEffect, useRef } from 'preact/hooks'
import type { JSX } from 'preact'
import type { DebugStore } from '../../store'
import { useSignals } from '../../useSignals'
import { windowed, xForTime, drawFocusLine } from '../../timeline'

interface Props {
  store: DebugStore
  height?: number
}

const MAX_POINTS = 120
const MAX_FPS = 60

export function FrameTimeline({ store, height = 80 }: Props): JSX.Element {
  useSignals(store.history, store.timeWindowSec, store.focusTime)
  const canvasRef = useRef<HTMLCanvasElement | null>(null)

  useEffect(() => {
    const canvas = canvasRef.current
    if (!canvas) return
    const ctx = canvas.getContext('2d')
    if (!ctx) return

    const dpr = window.devicePixelRatio || 1
    canvas.width = canvas.clientWidth * dpr
    canvas.height = height * dpr
    ctx.scale(dpr, dpr)

    const w = canvas.clientWidth
    const h = height

    ctx.fillStyle = '#1a1a1a'
    ctx.fillRect(0, 0, w, h)

    // Grid lines every 30 FPS (0, 30, 60)
    ctx.strokeStyle = '#333'
    ctx.lineWidth = 1
    ctx.fillStyle = '#666'
    ctx.font = '9px monospace'
    ctx.textBaseline = 'bottom'
    for (let f = 0; f <= MAX_FPS; f += 30) {
      const y = h - (f / MAX_FPS) * h
      ctx.beginPath()
      ctx.moveTo(0, y)
      ctx.lineTo(w, y)
      ctx.stroke()
      if (f > 0) ctx.fillText(`${f}`, 2, y - 1)
    }

    const points = windowed(store.history.value, store.timeWindowSec.value).slice(-MAX_POINTS)
    if (points.length === 0) return
    const tMin = points[0].t
    const tMax = points[points.length - 1].t

    const stepX = w / Math.max(points.length - 1, 1)

    ctx.beginPath()
    ctx.moveTo(0, h)
    for (let i = 0; i < points.length; i++) {
      const x = i * stepX
      const y = h - (Math.min(points[i].fps, MAX_FPS) / MAX_FPS) * h
      ctx.lineTo(x, y)
    }
    ctx.lineTo(w, h)
    ctx.closePath()

    const avgFps = points.reduce((s, p) => s + p.fps, 0) / Math.max(points.length, 1)
    ctx.fillStyle =
      avgFps >= 25
        ? 'rgba(100, 255, 100, 0.2)'
        : avgFps >= 15
          ? 'rgba(255, 200, 100, 0.2)'
          : 'rgba(255, 100, 100, 0.3)'
    ctx.fill()
    ctx.strokeStyle = avgFps >= 25 ? '#6f6' : avgFps >= 15 ? '#fc6' : '#f66'
    ctx.lineWidth = 1
    ctx.stroke()

    const ft = store.focusTime.value
    if (ft !== null && ft >= tMin && ft <= tMax) {
      drawFocusLine(ctx, xForTime(0, w, tMin, tMax, ft), 0, h)
    }
  })

  return (
    <canvas
      ref={canvasRef}
      onClick={(e) => {
        const pts = windowed(store.history.value, store.timeWindowSec.value).slice(-MAX_POINTS)
        if (pts.length === 0) return
        const tMin = pts[0].t
        const tMax = pts[pts.length - 1].t
        const right = e.currentTarget.clientWidth
        store.focusTime.value = tMin + ((e.offsetX / right) * (tMax - tMin))
      }}
      style={{ width: '100%', height: `${height}px`, display: 'block' }}
    />
  )
}
