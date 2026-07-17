import { useState, useEffect } from 'preact/hooks'
import type { JSX } from 'preact'
import type { DebugStore } from '../store'
import type { GpuInfo } from '../types'

interface Props {
  store: DebugStore
}

const RENDER_TICK_MS = 100

export function GpuTab({ store }: Props): JSX.Element {
  const [gpu, setGpu] = useState<GpuInfo | null>(null)
  const [gpuError, setGpuError] = useState<string | null>(null)

  useEffect(() => {
    let cancelled = false
    void (async () => {
      try {
        const { getGpuInfo } = await import('../gpu-info')
        const info = await getGpuInfo()
        if (!cancelled) {
          setGpu(info)
          store.gpuInfo.value = info
        }
      } catch (e) {
        if (!cancelled) setGpuError(e instanceof Error ? e.message : String(e))
      }
    })()
    return () => { cancelled = true }
  }, [store])

  const [, forceRender] = useState(0)
  useEffect(() => {
    const id = setInterval(() => forceRender((n) => n + 1), RENDER_TICK_MS)
    return () => clearInterval(id)
  }, [])

  const render = store.renderStats.value

  const ringPct =
    render && render.ringCap > 0
      ? Math.round((render.ringLength / render.ringCap) * 100)
      : null

  return (
    <>
      <div class="debug-section">
        <h3>GPU Info</h3>
        {gpuError ? (
          <div class="stat-bad">Failed to query GPU: {gpuError}</div>
        ) : !gpu ? (
          <div style={{ color: '#999' }}>Querying WebGL…</div>
        ) : gpu.available && (gpu.vendor || gpu.renderer) ? (
          <table class="debug-table">
            <tr><td>Vendor</td><td style={{ textAlign: 'right' }}>{gpu.vendor ?? '—'}</td></tr>
            <tr><td>Renderer</td><td style={{ textAlign: 'right' }}>{gpu.renderer ?? '—'}</td></tr>
          </table>
        ) : (
          <div class="stat-warn">WebGL debug info blocked by browser</div>
        )}
        <div style={{ color: '#777', marginTop: '4px', fontSize: '11px' }}>
          Check chrome://gpu/ for full details
        </div>
      </div>

      <div class="debug-section">
        <h3>Canvas / Rendering</h3>
        <table class="debug-table">
          <tr><td>Frames Presented</td><td>{render?.frameCount ?? 0}</td></tr>
          <tr><td>Dropped (Late)</td><td>{render?.droppedLate ?? 0}</td></tr>
          <tr><td>Dropped (Overflow)</td><td>{render?.droppedOverflow ?? 0}</td></tr>
          <tr>
            <td>Ring Buffer</td>
            <td>
              {render
                ? `${render.ringLength}/${render.ringCap}${ringPct !== null ? ` (${ringPct}%)` : ''}`
                : '—'}
            </td>
          </tr>
          <tr>
            <td>Presentation PTS</td>
            <td>{render?.currentPtsUs != null ? `${render.currentPtsUs} µs` : 'Not anchored yet'}</td>
          </tr>
          <tr><td>rAF Delta</td><td>{render ? `${render.rafDeltaMs.toFixed(1)} ms` : '—'}</td></tr>
          <tr><td>FPS</td><td>{render?.fps ?? '—'}</td></tr>
        </table>
      </div>
    </>
  )
}
