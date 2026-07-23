import { useEffect, useState } from 'preact/hooks'
import type { JSX } from 'preact'
import type { DebugStore } from '../store'
import type { StatsMsg } from '../../worker'

interface Props {
  store: DebugStore
}

export function StreamTab({ store }: Props): JSX.Element {
  const [, forceRender] = useState(0)
  useEffect(() => {
    const id = setInterval(() => forceRender((n) => n + 1), 250)
    return () => clearInterval(id)
  }, [])

  const status = store.status.value
  const srt = store.srtStats.value
  const drift = store.driftMs.value
  const entries = store.logEntries.value
  const latency = store.latencyMs.value
  const certMode = store.certMode.value

  return (
    <>
      <div class="debug-section">
        <h3>Connection</h3>
        <table class="debug-table">
          <tr><td>status</td><td>{status}</td></tr>
          <tr><td>latency</td><td>{latency}ms</td></tr>
          <tr><td>cert mode</td><td>{certMode}</td></tr>
        </table>
      </div>

      {srt && <SrtStatsTable srt={srt} drift={drift} />}

      <div class="debug-section">
        <h3>Event Log ({entries.length})</h3>
        <div style={{ maxHeight: '300px', overflowY: 'auto', fontSize: '11px', lineHeight: '1.4' }}>
          {entries.length === 0 ? (
            <div style={{ color: '#999' }}>No events yet</div>
          ) : (
            entries.map((e, i) => (
              <div class={e.cls} style={{ padding: '1px 0', wordBreak: 'break-word' }}>
                {e.msg}
              </div>
            ))
          )}
        </div>
      </div>
    </>
  )
}

function SrtStatsTable({ srt, drift }: { srt: StatsMsg; drift: number | null }) {
  const lossRate = (srt.rxData + srt.rxLoss) > 0
    ? ((srt.rxLoss / (srt.rxData + srt.rxLoss)) * 100).toFixed(2)
    : '0.00'
  const mbps = (srt.bandwidthBps / 1e6).toFixed(1)
  const elapsed = (srt.elapsedMs / 1000).toFixed(0)
  const lossCls = parseFloat(lossRate) > 5 ? 'stat-bad' : parseFloat(lossRate) > 1 ? 'stat-warn' : 'stat-good'

  return (
    <div class="debug-section">
      <h3>SRT Stats</h3>
      <table class="debug-table">
        <tr><td>uptime</td><td>{elapsed}s</td></tr>
        <tr><td>RTT</td><td>{srt.rttMs.toFixed(1)}ms</td></tr>
        <tr><td>bandwidth</td><td>{mbps} Mbps</td></tr>
        <tr><td>rx packets</td><td>{srt.rxData}</td></tr>
        <tr><td>rx bytes</td><td>{(srt.rxBytes / 1e6).toFixed(1)} MB</td></tr>
        <tr><td>loss</td><td class={lossCls}>{srt.rxLoss} ({lossRate}%)</td></tr>
        <tr><td>retransmit</td><td>{srt.rxRetransmit}</td></tr>
        <tr><td>dropped</td><td>{srt.rxDropped}</td></tr>
        <tr><td>belated</td><td>{srt.rxBelated}</td></tr>
        <tr><td>buffered</td><td>{srt.rxBuffered}</td></tr>
          <tr><td>ACK / NAK</td><td>{srt.rxAck} / {srt.rxNak}</td></tr>
          <tr><td>poll max</td><td>{srt.pollMaxMs.toFixed(1)}ms</td></tr>
        {drift !== null && (
          <tr><td>A/V drift</td><td>{drift >= 0 ? '+' : ''}{drift.toFixed(0)}ms</td></tr>
        )}
      </table>
    </div>
  )
}
