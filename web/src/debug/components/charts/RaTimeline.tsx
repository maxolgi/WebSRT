import { useEffect, useRef, useState } from 'preact/hooks'
import type { JSX } from 'preact'
import type { DebugStore } from '../../store'

interface Props {
  store: DebugStore
  height?: number
}

const MAX_POINTS = 120
const UPDATE_MS = 500

// Random-access cadence for the video PID. The snapshot's raCounts is
// cumulative, so a marker is drawn wherever consecutive samples rise.
// FrameTimeline-style 2D canvas.
export function RaTimeline({ store, height = 60 }: Props): JSX.Element {
  const canvasRef = useRef<HTMLCanvasElement | null>(null)
  const historyRef = useRef<{ t: number; ra: number }[]>([])
  const [, forceRender] = useState(0)

  useEffect(() => {
    const id = setInterval(() => {
      const d = store.demuxStats.value
      if (!d) return
      // Find the video PID's cumulative RA count.
      let videoPid = -1
      for (let i = 0; i < d.pmtPids.length; i++) {
        const st = d.pmtStreamTypes[i]
        if (st === 0x1b || st === 0x24) { videoPid = d.pmtPids[i]; break }
        if (st === 0x06 && d.pmtFormatIds[i] === 'AV01') { videoPid = d.pmtPids[i]; break }
      }
      let ra = 0
      if (videoPid >= 0) {
        for (let i = 0; i < d.pids.length; i++) {
          if (d.pids[i] === videoPid) { ra = d.raCounts[i]; break }
        }
      }
      const arr = historyRef.current
      const next = [...arr, { t: performance.now(), ra }]
      historyRef.current = next.length > MAX_POINTS ? next.slice(-MAX_POINTS) : next
      forceRender((n) => n + 1)
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
    // Draw a marker wherever the cumulative count increased (a new keyframe).
    ctx.fillStyle = '#6f6'
    for (let i = 1; i < pts.length; i++) {
      if (pts[i].ra > pts[i - 1].ra) {
        const x = ((pts[i].t - t0) / span) * w
        ctx.fillRect(x - 1, 4, 2, h - 8)
      }
    }
    // Baseline
    ctx.strokeStyle = '#444'
    ctx.lineWidth = 1
    ctx.beginPath()
    ctx.moveTo(0, h - 2)
    ctx.lineTo(w, h - 2)
    ctx.stroke()
  })

  return (
    <canvas
      ref={canvasRef}
      style={{ width: '100%', height: `${height}px`, display: 'block' }}
    />
  )
}
