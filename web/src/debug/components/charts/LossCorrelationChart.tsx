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
const BANDS = 3

// Stacked-band correlation chart. Three horizontal bands share one timeline so
// CC errors, SRT loss, and SRT drops can be read side-by-side: a vertical red
// stripe through only the top band means CC errors with no SRT loss (TS bytes
// dropped between receiver and demuxer).
export function LossCorrelationChart({ store, height = 80 }: Props): JSX.Element {
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
    const bandH = height / BANDS

    ctx.fillStyle = '#1a1a1a'
    ctx.fillRect(0, 0, w, height)

    const points = windowed(store.history.value, store.timeWindowSec.value).slice(-MAX_POINTS)
    if (points.length === 0) return
    const tMin = points[0].t
    const tMax = points[points.length - 1].t

    const cellW = w / Math.max(points.length, 1)
    const series = [
      points.map((p) => p.ccErrors),
      points.map((p) => p.srtLoss),
      points.map((p) => p.srtDropped),
    ]

    for (let band = 0; band < BANDS; band++) {
      const yOff = band * bandH
      for (let i = 0; i < points.length; i++) {
        const n = series[band][i]
        const intensity = Math.min(n / 10, 1)
        if (intensity === 0) {
          ctx.fillStyle = 'rgba(100, 255, 100, 0.12)'
        } else {
          const r = Math.round(255 * Math.min(intensity * 2, 1))
          const g = Math.round(255 * Math.max(1 - intensity * 2, 0))
          ctx.fillStyle = `rgba(${r}, ${g}, 50, ${0.3 + intensity * 0.7})`
        }
        ctx.fillRect(i * cellW, yOff, cellW + 1, bandH)
      }
      ctx.fillStyle = '#000'
      ctx.fillRect(0, yOff, w, 1)
    }

    const ft = store.focusTime.value
    if (ft !== null && ft >= tMin && ft <= tMax) {
      drawFocusLine(ctx, xForTime(0, w, tMin, tMax, ft), 0, height)
    }
  })

  return (
    <div>
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
      <div style={{ display: 'flex', gap: '12px', fontSize: '11px', marginTop: '4px' }}>
        <span style={{ color: '#f66' }}>■ CC errors</span>
        <span style={{ color: '#fa3' }}>■ SRT loss</span>
        <span style={{ color: '#ec3' }}>■ SRT dropped</span>
      </div>
    </div>
  )
}
