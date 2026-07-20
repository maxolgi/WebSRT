import { useState, useEffect, useCallback } from 'preact/hooks'
import type { JSX } from 'preact'
import type { DebugStore } from '../store'
import type { MediaCapResult } from '../types'
import { QueueSparkline } from './charts/QueueSparkline'

interface Props {
  store: DebugStore
}

const RENDER_TICK_MS = 100

export function CodecTab({ store }: Props): JSX.Element {
  const [, forceRender] = useState(0)
  useEffect(() => {
    const id = setInterval(() => forceRender((n) => n + 1), RENDER_TICK_MS)
    return () => clearInterval(id)
  }, [])

  const runProbe = useCallback(async () => {
    if (store.mediaCapsLoading.value) return
    store.mediaCapsLoading.value = true
    try {
      const { probeMatrix } = await import('../media-capabilities')
      const results = await probeMatrix()
      store.mediaCaps.value = results
    } catch (e) {
      console.error('MediaCapabilities probe failed:', e)
    } finally {
      store.mediaCapsLoading.value = false
    }
  }, [store])

  const video = store.videoStats.value
  const audio = store.audioStats.value
  const render = store.renderStats.value
  const loading = store.mediaCapsLoading.value
  const caps = store.mediaCaps.value
  const testActions = store.testActions.value

  const hwPref = video?.hwModePreference ?? 'prefer-hardware'
  const setHw = (mode: 'prefer-hardware' | 'prefer-software') => {
    testActions?.setHwMode(mode)
  }

  const btnStyle = (active: boolean): JSX.CSSProperties => ({
    background: active ? '#3a3' : '#333',
    color: active ? '#0f0' : '#ddd',
    padding: '4px 10px',
    cursor: testActions ? 'pointer' : 'not-allowed',
    border: '1px solid #555',
    borderRadius: '3px',
    font: 'inherit',
    fontSize: '11px',
    marginLeft: '6px',
    opacity: testActions ? 1 : 0.5,
  })

  return (
    <>
      <div class="debug-section">
        <h3>Video Decoder</h3>
        <table class="debug-table">
          <tr><td>Codec</td><td>{video?.codecString ?? 'Not configured'}</td></tr>
          <tr><td>Profile / Level</td><td>{video ? `${video.profile} / ${video.level}` : '—'}</td></tr>
          <tr><td>Coded Resolution</td><td>{video ? `${video.codedWidth} × ${video.codedHeight}` : '—'}</td></tr>
          <tr><td>Decoder State</td><td>{video?.decoderState ?? '—'}</td></tr>
          <tr><td>Hardware Accel</td><td>{video?.hwAcceleration ?? '—'}</td></tr>
          <tr><td>Reconfigure Count</td>
              <td class={(video?.reconfigureCount ?? 0) > 3 ? 'stat-warn' : ''}>
                {video?.reconfigureCount ?? 0}
                {(video?.reconfigureCount ?? 0) > 3
                  ? ' — SPS/SH is bouncing, check encoder'
                  : ''}
              </td>
          </tr>
          <tr><td>Decode Queue</td><td>{video?.decodeQueueSize ?? 0}</td></tr>
          <tr><td>Decoded Frames</td><td>{video?.decodedCount ?? 0}</td></tr>
          <tr><td>Dropped Frames</td><td>{video?.droppedFrames ?? 0}</td></tr>
          <tr><td>Decode FPS</td><td>{render?.fps ?? '—'}</td></tr>
        </table>
      </div>

      <div class="debug-section">
        <h3>Hardware Acceleration</h3>
        <div style={{ marginBottom: '6px' }}>
          <button onClick={() => setHw('prefer-hardware')} disabled={!testActions} style={btnStyle(hwPref === 'prefer-hardware')}>
            Prefer Hardware {hwPref === 'prefer-hardware' ? '✓' : ''}
          </button>
          <button onClick={() => setHw('prefer-software')} disabled={!testActions} style={btnStyle(hwPref === 'prefer-software')}>
            Prefer Software {hwPref === 'prefer-software' ? '✓' : ''}
          </button>
        </div>
        <div style={{ color: '#888', fontSize: '11px' }}>
          Forces re-configure on next feed() using the cached SPS/PPS. A/B test
          if VAAPI is producing choppy output — software decode rules out driver
          issues. (Live toggle requires an active stream.)
        </div>
      </div>

      <div class="debug-section">
        <h3>Decode Queue Depth (last 30s)</h3>
        <QueueSparkline store={store} />
      </div>

      <div class="debug-section">
        <h3>Audio Decoder</h3>
        <table class="debug-table">
          <tr><td>Codec</td><td>{audio?.codec ?? 'Not configured'}</td></tr>
          <tr><td>Sample Rate</td><td>{audio?.sampleRate ? `${audio.sampleRate} Hz` : '—'}</td></tr>
          <tr><td>Channels</td><td>{audio?.channels ?? '—'}</td></tr>
          <tr><td>Decoder State</td><td>{audio?.decoderState ?? '—'}</td></tr>
          <tr><td>Decode Queue</td><td>{audio?.decodeQueueSize ?? 0}</td></tr>
          <tr><td>Packets Decoded</td><td>{audio?.packetsDecoded ?? 0}</td></tr>
          <tr><td>Packets Dropped</td><td>{audio?.droppedPackets ?? 0}</td></tr>
          <tr><td>Output Mode</td><td>{audio?.outputMode ?? '—'}</td></tr>
        </table>
      </div>

      <div class="debug-section">
        <h3>
          MediaCapabilities Probe
          <button
            onClick={runProbe}
            disabled={loading}
            style={{ marginLeft: '8px', font: 'inherit', fontSize: '11px' }}
          >
            {loading ? 'Probing…' : 'Run Probe'}
          </button>
        </h3>
        {caps.length === 0 ? (
          <div style={{ color: '#999' }}>
            {loading ? 'Running probe…' : 'No probe results yet. Click "Run Probe".'}
          </div>
        ) : (
          <table class="debug-table">
            <thead>
              <tr>
                <th style={{ textAlign: 'left', padding: '2px 6px' }}>Codec</th>
                <th style={{ textAlign: 'right', padding: '2px 6px' }}>Resolution</th>
                <th style={{ textAlign: 'right', padding: '2px 6px' }}>Supported</th>
                <th style={{ textAlign: 'right', padding: '2px 6px' }}>PowerEff</th>
                <th style={{ textAlign: 'right', padding: '2px 6px' }}>Smooth</th>
              </tr>
            </thead>
            <tbody>
              {caps.map((r) => (
                <ProbeRow key={`${r.codec}-${r.width}x${r.height}-${r.framerate}`} r={r} />
              ))}
            </tbody>
          </table>
        )}
      </div>
    </>
  )
}

function ProbeRow({ r }: { r: MediaCapResult }): JSX.Element {
  const supportClass = !r.supported
    ? 'stat-bad'
    : r.smooth
      ? 'stat-good'
      : 'stat-warn'
  return (
    <tr>
      <td style={{ textAlign: 'left', padding: '2px 6px' }}>{r.codec}</td>
      <td style={{ textAlign: 'right', padding: '2px 6px' }}>{r.width}×{r.height}@{r.framerate}</td>
      <td class={supportClass} style={{ textAlign: 'right', padding: '2px 6px' }}>
        {r.supported ? 'yes' : 'no'}
      </td>
      <td style={{ textAlign: 'right', padding: '2px 6px' }}>{r.powerEfficient ? 'yes' : 'no'}</td>
      <td class={supportClass} style={{ textAlign: 'right', padding: '2px 6px' }}>
        {r.smooth ? 'yes' : 'no'}
      </td>
    </tr>
  )
}
