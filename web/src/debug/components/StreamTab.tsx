import { useState } from 'preact/hooks'
import type { JSX } from 'preact'
import type { DebugStore } from '../store'
import { useSignals } from '../useSignals'
import { IssuesStrip } from './IssuesStrip'
import type { StatsMsg } from '../../worker'

interface Props {
  store: DebugStore
}

export function StreamTab({ store }: Props): JSX.Element {
  useSignals(store.status, store.srtStats, store.driftMs, store.logEntries, store.latencyMs, store.certMode)

  const [filterText, setFilterText] = useState('')
  const [levelFilter, setLevelFilter] = useState('all')

  const status = store.status.value
  const srt = store.srtStats.value
  const drift = store.driftMs.value
  const entries = store.logEntries.value
  const latency = store.latencyMs.value
  const certMode = store.certMode.value

  const query = filterText.trim().toLowerCase()
  const visible = entries.filter((e) => {
    const level = e.cls === 'err' ? 'error' : e.cls === 'info' ? 'info' : 'other'
    if (levelFilter !== 'all' && level !== levelFilter) return false
    return !query || e.msg.toLowerCase().includes(query)
  })

  return (
    <>
      <IssuesStrip store={store} />
      <div class="debug-section">
        <h3>Connection</h3>
        <table class="debug-table">
          <tbody>
            <tr><td>status</td><td>{status}</td></tr>
            <tr><td>latency</td><td>{latency}ms</td></tr>
            <tr><td>cert mode</td><td>{certMode}</td></tr>
          </tbody>
        </table>
      </div>

      {srt && <SrtStatsTable srt={srt} drift={drift} />}

      <CcErrorCounter store={store} />

      <div class="debug-section">
        <h3>Event Log ({visible.length} / {entries.length})</h3>
        <div style={{ display: 'flex', gap: '4px', marginBottom: '4px' }}>
          <input
            type="text"
            placeholder="filter…"
            value={filterText}
            onInput={(e) => setFilterText(e.currentTarget.value)}
            style={{ flex: 1, fontSize: '11px', padding: '2px 4px' }}
          />
          <select
            value={levelFilter}
            onChange={(e) => setLevelFilter(e.currentTarget.value)}
            style={{ fontSize: '11px', padding: '2px 4px' }}
          >
            <option value="all">All</option>
            <option value="error">Error</option>
            <option value="info">Info</option>
            <option value="other">Other</option>
          </select>
        </div>
        <div style={{ maxHeight: '300px', overflowY: 'auto', fontSize: '11px', lineHeight: '1.4' }}>
          {visible.length === 0 ? (
            <div style={{ color: '#999' }}>{entries.length === 0 ? 'No events yet' : 'No matching events'}</div>
          ) : (
            visible.map((e, i) => (
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

function CcErrorCounter({ store }: { store: DebugStore }): JSX.Element {
  useSignals(store.demuxStats)

  const [baseline, setBaseline] = useState(0)

  const demux = store.demuxStats.value
  let total = 0
  if (demux) {
    for (let i = 0; i < demux.ccErrors.length; i++) total += demux.ccErrors[i]
  }
  const display = Math.max(0, total - baseline)
  const cls = display > 0 ? 'stat-bad' : 'stat-good'

  return (
    <div class="debug-section">
      <h3>Continuity Counter Errors</h3>
      <table class="debug-table">
        <tbody>
          <tr>
            <td>total</td>
            <td class={cls} style={{ cursor: 'pointer' }} onClick={() => setBaseline(total)} title="click to reset">
              {display}
            </td>
          </tr>
        </tbody>
      </table>
    </div>
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
        <tbody>
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
          <tr><td>WASM handle</td><td>{srt.wasmHandleAvgUs.toFixed(1)}µs/call</td></tr>
          <tr><td>WASM poll</td><td>{srt.wasmPollAvgUs.toFixed(1)}µs/call</td></tr>
          <tr><td>loop avg</td><td>{srt.loopIterAvgMs.toFixed(2)}ms</td></tr>
          {drift !== null && (
            <tr><td>A/V drift</td><td>{drift >= 0 ? '+' : ''}{drift.toFixed(0)}ms</td></tr>
          )}
        </tbody>
      </table>
    </div>
  )
}
