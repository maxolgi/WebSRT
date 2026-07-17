import { useEffect, useRef, useState } from 'preact/hooks'
import type { JSX } from 'preact'
import type { DebugStore } from '../../store'

interface Props {
  store: DebugStore
  height?: number
}

const MAX_POINTS = 120
const UPDATE_MS = 500
const MAX_FPS = 60

export function FrameTimeline({ store, height = 80 }: Props): JSX.Element {
  const canvasRef = useRef<HTMLCanvasElement | null>(null)
  const [, forceRender] = useState(0)

  useEffect(() => {
    const id = setInterval(() => forceRender((n) => n + 1), UPDATE_MS)
    return () => clearInterval(id)
  }, [])

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

    const points = store.history.value.slice(-MAX_POINTS)
    if (points.length === 0) return

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
  })

  return (
    <div>
      <div style={{ color: '#999', fontSize: '11px', marginBottom: '2px' }}>
        Render Health (FPS over time)
      </div>
      <canvas
        ref={canvasRef}
        style={{ width: '100%', height: `${height}px`, display: 'block' }}
      />
    </div>
  )
}
