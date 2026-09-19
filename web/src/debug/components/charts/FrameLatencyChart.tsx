import { useEffect, useRef } from 'preact/hooks'
import type { JSX } from 'preact'
import type { DebugStore } from '../../store'

interface Props {
  store: DebugStore
}

const SPARK_H = 60

// Per-frame video latency waterfall: the fixed TSBPD latency (config) plus the
// mean worker-side release jitter (|actual release − TSBPD deadline|, from the
// worker's rolling per-frame ring), with a sparkline of recent per-frame jitter.
export function FrameLatencyChart({ store }: Props): JSX.Element {
  const canvasRef = useRef<HTMLCanvasElement | null>(null)
  const vl = store.srtStats.value?.videoLatency
  const tlb = store.latencyMs.value

  useEffect(() => {
    const canvas = canvasRef.current
    if (!canvas || !vl || vl.count === 0) return
    const ctx = canvas.getContext('2d')
    if (!ctx) return

    const dpr = window.devicePixelRatio || 1
    canvas.width = canvas.clientWidth * dpr
    canvas.height = SPARK_H * dpr
    ctx.scale(dpr, dpr)

    const w = canvas.clientWidth
    const h = SPARK_H
    ctx.fillStyle = '#1a1a1a'
    ctx.fillRect(0, 0, w, h)

    const samples = vl.samples
    if (samples.length === 0) return

    let maxY = 1
    for (const s of samples) {
      const v = s.jitterUs / 1000
      if (v > maxY) maxY = v
    }

    // Faint 0 baseline.
    ctx.strokeStyle = '#333'
    ctx.lineWidth = 1
    ctx.beginPath()
    ctx.moveTo(0, h - 2)
    ctx.lineTo(w, h - 2)
    ctx.stroke()

    ctx.strokeStyle = '#6cf'
    ctx.lineWidth = 1.5
    ctx.beginPath()
    for (let i = 0; i < samples.length; i++) {
      const x = samples.length > 1 ? (i / (samples.length - 1)) * w : 0
      const v = Math.min(samples[i].jitterUs / 1000, maxY)
      const y = h - 2 - (v / maxY) * (h - 4)
      if (i === 0) ctx.moveTo(x, y)
      else ctx.lineTo(x, y)
    }
    ctx.stroke()
  })

  if (!vl || vl.count === 0) {
    return <div style={{ color: '#666', fontSize: '12px' }}>No video frames yet</div>
  }

  const meanJitterMs = vl.meanJitterUs / 1000
  const totalMs = tlb + meanJitterMs

  const bar = totalMs < 0.05 ? (
    <div style={{ height: '10px', background: '#3a6', borderRadius: '2px' }} />
  ) : (
    <div style={{ display: 'flex', height: '10px', borderRadius: '2px', overflow: 'hidden' }}>
      <div style={{ width: `${(tlb / totalMs) * 100}%`, background: '#3a6' }} />
      <div style={{ width: `${(meanJitterMs / totalMs) * 100}%`, background: '#fc6' }} />
    </div>
  )

  return (
    <div>
      {bar}
      <div style={{ fontSize: '10px', color: '#888', marginTop: '2px' }}>
        <span style={{ color: '#3a6' }}>■</span> TSBPD {tlb}ms
        &nbsp;&nbsp;
        <span style={{ color: '#fc6' }}>■</span> jitter {meanJitterMs.toFixed(1)}ms
      </div>
      <div style={{ fontSize: '11px', marginTop: '4px' }}>
        mean worker-side wait ≈ {totalMs.toFixed(1)}ms · max jitter
        {(vl.maxJitterUs / 1000).toFixed(1)}ms · {vl.count} frames
      </div>
      <canvas
        ref={canvasRef}
        style={{ width: '100%', height: `${SPARK_H}px`, display: 'block', marginTop: '6px' }}
      />
    </div>
  )
}
