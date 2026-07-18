import { useEffect, useRef } from 'preact/hooks'
import type { JSX } from 'preact'
import type { Chart } from 'chart.js'
import type { DebugStore } from '../../store'

interface Props {
  store: DebugStore
  height?: number
}

const UPDATE_MS = 500

// NAL frame-type counts for the first video PID. nalStats is flat 9×M with
// order [I, P, B, IDR, SPS, PPS, SEI, AUD, NonIDR]. Shown as a current-state
// bar chart that refreshes each tick.
export function NalStackedBar({ store, height = 140 }: Props): JSX.Element {
  const canvasRef = useRef<HTMLCanvasElement | null>(null)

  useEffect(() => {
    let destroyed = false
    let chart: Chart | null = null
    let intervalId: ReturnType<typeof setInterval> | undefined

    void (async () => {
      const mod = await import('chart.js')
      if (destroyed) return
      mod.Chart.register(
        mod.BarController,
        mod.BarElement,
        mod.CategoryScale,
        mod.LinearScale,
        mod.Tooltip,
        mod.Legend,
      )

      const ctx = canvasRef.current
      if (!ctx) return

      const read = () => {
        const d = store.demuxStats.value
        if (!d || d.nalPids.length === 0) return [0, 0, 0, 0, 0, 0, 0, 0, 0]
        const j = 0
        const off = 9 * j
        return [
          d.nalStats[off + 0], // I
          d.nalStats[off + 1], // P
          d.nalStats[off + 2], // B
          d.nalStats[off + 3], // IDR
          d.nalStats[off + 4], // SPS
          d.nalStats[off + 5], // PPS
          d.nalStats[off + 6], // SEI
          d.nalStats[off + 7], // AUD
          d.nalStats[off + 8], // NonIDR
        ]
      }

      chart = new mod.Chart(ctx, {
        type: 'bar',
        data: {
          labels: ['I', 'P', 'B', 'IDR', 'SPS', 'PPS', 'SEI', 'AUD', 'NonIDR'],
          datasets: [
            {
              label: 'NAL count',
              data: read(),
              backgroundColor: [
                '#6f6', '#6cf', '#c6f', '#3f6',
                '#fc6', '#f96', '#9cf', '#999', '#69c',
              ],
            },
          ],
        },
        options: {
          animation: false,
          responsive: true,
          maintainAspectRatio: false,
          plugins: {
            legend: { display: false },
            tooltip: { callbacks: { label: (i) => `${i.parsed.y}` } },
          },
          scales: {
            x: { ticks: { color: '#999', font: { size: 10 } } },
            y: { beginAtZero: true, ticks: { color: '#999', font: { size: 10 } } },
          },
        },
      })

      intervalId = setInterval(() => {
        if (!chart) return
        chart.data.datasets[0].data = read()
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
