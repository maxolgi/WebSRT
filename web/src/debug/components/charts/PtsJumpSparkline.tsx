import { useEffect, useRef, useState } from 'preact/hooks'
import type { JSX } from 'preact'
import type { DebugStore } from '../../store'

interface Props {
  store: DebugStore
  height?: number
}

const MAX_POINTS = 120
const UPDATE_MS = 500

// Total PTS-jump count (across all PIDs) over time. Cumulative, so this is a
// monotone step; jumps in the line mark PTS-discontinuity events.
export function PtsJumpSparkline({ store, height = 50 }: Props): JSX.Element {
  const canvasRef = useRef<HTMLCanvasElement | null>(null)
  const historyRef = useRef<{ t: number; n: number }[]>([])
  const [, forceRender] = useState(0)

  useEffect(() => {
    const id = setInterval(() => {
      const d = store.demuxStats.value
      if (!d) return
      let n = 0
      for (let i = 0; i < d.ptsJumps.length; i++) n += d.ptsJumps[i]
      const arr = historyRef.current
      const next = [...arr, { t: performance.now(), n }]
      historyRef.current = next.length > MAX_POINTS ? next.slice(-MAX_POINTS) : next
      forceRender((x) => x + 1)
    }, UPDATE_MS)
    return () => clearInterval(id)
  }, [store])

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

    const pts = historyRef.current
    if (pts.length < 2) return
    const t0 = pts[0].t
    const t1 = pts[pts.length - 1].t
    const span = Math.max(t1 - t0, 1)
    const max = Math.max(pts[pts.length - 1].n, 1)

    ctx.strokeStyle = '#fc6'
    ctx.lineWidth = 1.5
    ctx.beginPath()
    for (let i = 0; i < pts.length; i++) {
      const x = ((pts[i].t - t0) / span) * w
      const y = h - (pts[i].n / max) * (h - 4) - 2
      if (i === 0) ctx.moveTo(x, y)
      else ctx.lineTo(x, y)
    }
    ctx.stroke()
  })

  return (
    <canvas
      ref={canvasRef}
      style={{ width: '100%', height: `${height}px`, display: 'block' }}
    />
  )
}
