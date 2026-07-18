import { useEffect, useRef, useState } from 'preact/hooks'
import type { JSX } from 'preact'
import type { DebugStore } from '../../store'

interface Props {
  store: DebugStore
  height?: number
}

const MAX_POINTS = 120
const UPDATE_MS = 500

// CC-error intensity over time. Mirrors LossHeatmap: green when clean, red
// scaling with the bucket's total CC-error count.
export function CcHeatmap({ store, height = 40 }: Props): JSX.Element {
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
    ctx.fillStyle = '#1a1a1a'
    ctx.fillRect(0, 0, w, height)

    const points = store.history.value.slice(-MAX_POINTS)
    if (points.length === 0) return

    const cellW = w / Math.max(points.length, 1)
    for (let i = 0; i < points.length; i++) {
      const n = points[i].ccErrors
      const intensity = Math.min(n / 10, 1)
      if (intensity === 0) {
        ctx.fillStyle = 'rgba(100, 255, 100, 0.15)'
      } else {
        const r = Math.round(255 * Math.min(intensity * 2, 1))
        const g = Math.round(255 * Math.max(1 - intensity * 2, 0))
        ctx.fillStyle = `rgba(${r}, ${g}, 50, ${0.3 + intensity * 0.7})`
      }
      ctx.fillRect(i * cellW, 0, cellW + 1, height)
    }
  })

  return (
    <canvas
      ref={canvasRef}
      style={{ width: '100%', height: `${height}px`, display: 'block' }}
    />
  )
}
