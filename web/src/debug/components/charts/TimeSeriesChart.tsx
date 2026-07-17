import { useEffect, useRef } from 'preact/hooks'
import type { JSX } from 'preact'
import type { Chart } from 'chart.js'
import type { DebugStore } from '../../store'
import type { TimeSeriesBucket } from '../../types'

interface Props {
  store: DebugStore
  field: keyof TimeSeriesBucket
  label: string
  color: string
  yFormat?: (v: number) => string
  transform?: (v: number) => number
  height?: number
}

const MAX_POINTS = 120
const UPDATE_MS = 500

export function TimeSeriesChart({
  store,
  field,
  label,
  color,
  yFormat,
  transform,
  height = 120,
}: Props): JSX.Element {
  const canvasRef = useRef<HTMLCanvasElement | null>(null)

  // Keep latest function props without re-subscribing the effect.
  const yFormatRef = useRef(yFormat)
  yFormatRef.current = yFormat
  const transformRef = useRef(transform)
  transformRef.current = transform

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

      chart = new mod.Chart(ctx, {
        type: 'line',
        data: {
          datasets: [
            {
              label,
              data: extractPoints(store.history.value, field, transformRef.current),
              borderColor: color,
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
          plugins: {
            legend: { display: false },
            tooltip: {
              callbacks: {
                label: (item) => {
                  const v = item.parsed.y ?? 0
                  const fmt = yFormatRef.current
                  return fmt ? fmt(v) : String(v)
                },
              },
            },
          },
          scales: {
            x: { type: 'linear', display: false },
            y: {
              beginAtZero: true,
              title: { display: true, text: label, color: '#999' },
              ticks: { color: '#999', font: { size: 10 } },
            },
          },
        },
      })

      intervalId = setInterval(() => {
        if (!chart) return
        const ds = chart.data.datasets
        if (ds.length > 0) {
          ds[0].data = extractPoints(store.history.value, field, transformRef.current)
        }
        chart.update('none')
      }, UPDATE_MS)
    })()

    return () => {
      destroyed = true
      if (intervalId) clearInterval(intervalId)
      chart?.destroy()
      chart = null
    }
    // label/color/field are literal props; store is a stable instance.
  }, [store, field, label, color])

  return (
    <div
      class="chart-container"
      style={{ height: `${height}px`, position: 'relative' }}
    >
      <canvas ref={canvasRef} />
    </div>
  )
}

function extractPoints(
  history: TimeSeriesBucket[],
  field: keyof TimeSeriesBucket,
  transform?: (v: number) => number,
): { x: number; y: number }[] {
  const slice = history.slice(-MAX_POINTS)
  return slice.map((b) => {
    const raw = b[field]
    return { x: b.t, y: transform ? transform(raw) : raw }
  })
}
