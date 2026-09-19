import { useEffect, useRef } from 'preact/hooks'
import type { JSX } from 'preact'
import type { Chart } from 'chart.js'
import type { DebugStore } from '../../store'
import { windowed, drawFocusLine } from '../../timeline'

interface Props {
  store: DebugStore
  height?: number
}

const MAX_POINTS = 120
const UPDATE_MS = 500

// Per-PID bitrate from the WASM snapshot, pushed into the shared history as
// video/audio Mbps by sampler.ts. Two-line time-series, ~30 s window.
export function BitrateChart({ store, height = 120 }: Props): JSX.Element {
  const canvasRef = useRef<HTMLCanvasElement | null>(null)

  useEffect(() => {
    let destroyed = false
    let chart: Chart | null = null
    let intervalId: ReturnType<typeof setInterval> | undefined

    void (async () => {
      const mod = await import('chart.js')
      if (destroyed) return
      mod.Chart.register(
        mod.LineController,
        mod.LineElement,
        mod.PointElement,
        mod.LinearScale,
        mod.Tooltip,
        mod.Legend,
      )

      const ctx = canvasRef.current
      if (!ctx) return

      const pts = (field: 'videoMbps' | 'audioMbps') =>
        windowed(store.history.value, store.timeWindowSec.value)
          .slice(-MAX_POINTS)
          .map((b) => ({ x: b.t, y: b[field] }))

      chart = new mod.Chart(ctx, {
        type: 'line',
        data: {
          datasets: [
            {
              label: 'video (Mbps)',
              data: pts('videoMbps'),
              borderColor: '#6cf',
              borderWidth: 1.5,
              pointRadius: 0,
              tension: 0.1,
              fill: false,
            },
            {
              label: 'audio (Mbps)',
              data: pts('audioMbps'),
              borderColor: '#fc6',
              borderWidth: 1.5,
              pointRadius: 0,
              tension: 0.1,
              fill: false,
            },
          ],
        },
        options: {
          animation: false,
          responsive: true,
          maintainAspectRatio: false,
          onClick: (evt, _els, chart) => {
            if (evt.x == null) return
            const v = chart.scales.x.getValueForPixel(evt.x)
            store.focusTime.value = typeof v === 'number' && isFinite(v) ? v : null
          },
          plugins: {
            legend: { display: true, labels: { color: '#999', font: { size: 10 } } },
            tooltip: { callbacks: { label: (i) => `${(i.parsed.y ?? 0).toFixed(3)} Mbps` } },
          },
          scales: {
            x: { type: 'linear', display: false },
            y: { beginAtZero: true, ticks: { color: '#999', font: { size: 10 } } },
          },
        },
        plugins: [
          {
            id: 'focusLine',
            afterDraw(chart) {
              const ft = store.focusTime.value
              if (ft === null) return
              const s = chart.scales.x
              if (!s) return
              const px = s.getPixelForValue(ft)
              if (px < s.left || px > s.right) return
              drawFocusLine(chart.ctx, px, s.top, s.bottom)
            },
          },
        ],
      })

      intervalId = setInterval(() => {
        if (!chart) return
        const ds = chart.data.datasets
        ds[0].data = pts('videoMbps')
        ds[1].data = pts('audioMbps')
        chart.update('none')
      }, UPDATE_MS)
    })()

    return () => {
      destroyed = true
      if (intervalId) clearInterval(intervalId)
      chart?.destroy()
    }
  }, [store])

  return (
    <div class="chart-container" style={{ height: `${height}px`, position: 'relative' }}>
      <canvas ref={canvasRef} />
    </div>
  )
}
