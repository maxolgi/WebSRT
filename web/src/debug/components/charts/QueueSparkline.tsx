import { useEffect, useRef, useState } from 'preact/hooks'
import type { JSX } from 'preact'
import type { DebugStore } from '../../store'

interface Props {
  store: DebugStore
  height?: number
}

const RENDER_TICK_MS = 200

// Decode-queue depth over time, pulled from the sampler's history ring (100ms
// cadence, 300 samples = 30s window). A flat ~0 line is healthy; oscillation
// 0↔8 means the decoder is stalling and emitAu() is dropping non-keyframes.
// Also plots audio queue depth on the same axis for comparison.
export function QueueSparkline({ store, height = 60 }: Props): JSX.Element {
  const canvasRef = useRef<HTMLCanvasElement | null>(null)
  const [, forceRender] = useState(0)

  useEffect(() => {
    const id = setInterval(() => forceRender((n) => n + 1), RENDER_TICK_MS)
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

    const history = store.history.value
    if (history.length < 2) return

    const t0 = history[0].t
    const t1 = history[history.length - 1].t
    const span = Math.max(t1 - t0, 1)
    // Fixed scale to 16 so small values remain visible; clamp anything bigger.
    const maxY = 16

    // Gridlines at 8 and 16.
    ctx.strokeStyle = '#333'
    ctx.lineWidth = 1
    for (const yv of [8, 16]) {
      const y = h - (yv / maxY) * (h - 4) - 2
      ctx.beginPath()
      ctx.moveTo(0, y)
      ctx.lineTo(w, y)
      ctx.stroke()
    }

    const drawLine = (
      key: 'videoQueueDepth' | 'audioQueueDepth',
      color: string,
    ) => {
      ctx.strokeStyle = color
      ctx.lineWidth = 1.5
      ctx.beginPath()
      for (let i = 0; i < history.length; i++) {
        const v = Math.min(history[i][key], maxY)
        const x = ((history[i].t - t0) / span) * w
        const y = h - (v / maxY) * (h - 4) - 2
        if (i === 0) ctx.moveTo(x, y)
        else ctx.lineTo(x, y)
      }
      ctx.stroke()
    }

    drawLine('videoQueueDepth', '#fc6')
    drawLine('audioQueueDepth', '#6cf')
  })

  return (
    <div>
      <canvas
        ref={canvasRef}
        style={{ width: '100%', height: `${height}px`, display: 'block' }}
      />
      <div style={{ fontSize: '10px', color: '#888', marginTop: '2px' }}>
        <span style={{ color: '#fc6' }}>■</span> video queue
        &nbsp;&nbsp;
        <span style={{ color: '#6cf' }}>■</span> audio queue
        &nbsp;&nbsp;last 30s (max y = 16)
      </div>
    </div>
  )
}
