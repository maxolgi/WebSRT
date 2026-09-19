import { useEffect, useRef } from 'preact/hooks'
import type { JSX } from 'preact'
import type { Chart } from 'chart.js'
import type { DebugStore } from '../../store'
import { drawFocusLine } from '../../timeline'

interface Props {
  store: DebugStore
  height?: number
}

const MAX_POINTS = 120
const UPDATE_MS = 500
// ISO 101 290: PCR intervals over 100 ms are a red alarm.
const PCR_TARGET_MS = 100

// PCR interval over time for the first PCR PID, with a 100 ms reference line.
// Jitter is shown in the DemuxTab table; here we focus on interval stability.
export function PcrChart({ store, height = 120 }: Props): JSX.Element {
  const canvasRef = useRef<HTMLCanvasElement | null>(null)
  const histRef = useRef<{ x: number; y: number }[]>([])

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

      const target = histRef.current.length > 0
        ? histRef.current.map((p) => ({ x: p.x, y: PCR_TARGET_MS }))
        : []

      chart = new mod.Chart(ctx, {
        type: 'line',
        data: {
          datasets: [
            {
              label: 'PCR interval (ms)',
              data: histRef.current.slice(),
              borderColor: '#6cf',
              borderWidth: 1.5,
              pointRadius: 0,
              tension: 0.1,
              fill: false,
            },
            {
              label: `target ${PCR_TARGET_MS} ms`,
              data: target,
              borderColor: 'rgba(255,100,100,0.5)',
              borderWidth: 1,
              borderDash: [4, 3],
              pointRadius: 0,
              fill: false,
            },
          ],
        },
        options: {
          animation: false,
          responsive: true,
          maintainAspectRatio: false,
          plugins: {
            legend: { display: true, labels: { color: '#999', font: { size: 10 } } },
            tooltip: { callbacks: { label: (i) => `${(i.parsed.y ?? 0).toFixed(1)} ms` } },
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
        const d = store.demuxStats.value
        if (d && d.pcrPids.length > 0) {
          const interval = d.pcrIntervalsMs[0]
          const next = [...histRef.current, { x: performance.now(), y: interval }]
          histRef.current = next.length > MAX_POINTS ? next.slice(-MAX_POINTS) : next
        }
        const all = histRef.current
        const refT = all[all.length - 1]?.x
        const wSec = store.timeWindowSec.value
        const view =
          wSec > 0 && refT !== undefined
            ? all.filter((p) => p.x >= refT - wSec * 1000)
            : all
        const ds = chart.data.datasets
        ds[0].data = view.slice()
        ds[1].data = view.map((p) => ({ x: p.x, y: PCR_TARGET_MS }))
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
