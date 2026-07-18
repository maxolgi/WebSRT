import { useEffect, useRef } from 'preact/hooks'
import type { JSX } from 'preact'
import type { Chart } from 'chart.js'
import type { DebugStore } from '../../store'
import { streamTypeName } from '../streamTypes'

interface Props {
  store: DebugStore
  height?: number
}

const UPDATE_MS = 500

// Instantaneous byte-share donut per PID. Reads the live snapshot; each slice
// is labelled with PID + resolved codec name.
export function PidDonutChart({ store, height = 160 }: Props): JSX.Element {
  const canvasRef = useRef<HTMLCanvasElement | null>(null)

  useEffect(() => {
    let destroyed = false
    let chart: Chart | null = null
    let intervalId: ReturnType<typeof setInterval> | undefined

    void (async () => {
      const mod = await import('chart.js')
      if (destroyed) return
      mod.Chart.register(
        mod.DoughnutController,
        mod.ArcElement,
        mod.Tooltip,
        mod.Legend,
      )

      const ctx = canvasRef.current
      if (!ctx) return

      const palette = ['#6cf', '#fc6', '#6f6', '#f66', '#c6f', '#ff6', '#6ff', '#fff']
      const read = () => {
        const d = store.demuxStats.value
        if (!d || d.pids.length === 0) return { labels: [], data: [] as number[] }
        const labels: string[] = []
        const data: number[] = []
        for (let i = 0; i < d.pids.length; i++) {
          const pid = d.pids[i]
          const name = pidName(d, pid)
          labels.push(`0x${pid.toString(16)} (${name})`)
          data.push(d.byteTotals[i])
        }
        return { labels, data }
      }

      const init = read()
      chart = new mod.Chart(ctx, {
        type: 'doughnut',
        data: {
          labels: init.labels,
          datasets: [
            {
              data: init.data,
              backgroundColor: palette,
              borderColor: '#222',
              borderWidth: 1,
            },
          ],
        },
        options: {
          animation: false,
          responsive: true,
          maintainAspectRatio: false,
          plugins: {
            legend: {
              display: true,
              position: 'right',
              labels: { color: '#999', font: { size: 10 } },
            },
            tooltip: {
              callbacks: {
                label: (i) => {
                  const bytes = i.parsed
                  const kb = bytes / 1024
                  return `${i.label}: ${kb >= 1024 ? `${(kb / 1024).toFixed(2)} MB` : `${kb.toFixed(1)} KB`}`
                },
              },
            },
          },
        },
      })

      intervalId = setInterval(() => {
        if (!chart) return
        const next = read()
        chart.data.labels = next.labels
        chart.data.datasets[0].data = next.data
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

// Resolve a PID's codec name from the PMT tables, falling back to "PID <pid>".
function pidName(
  d: { pmtPids: Uint16Array; pmtStreamTypes: Uint8Array; pmtFormatIds: string[] },
  pid: number,
): string {
  for (let i = 0; i < d.pmtPids.length; i++) {
    if (d.pmtPids[i] === pid) {
      const st = d.pmtStreamTypes[i]
      const fmt = d.pmtFormatIds[i]
      if (st === 0x06 && fmt) return fmt === 'AV01' ? 'AV1' : fmt === 'Opus' ? 'Opus' : 'Private'
      return streamTypeName(st)
    }
  }
  return 'PID'
}
