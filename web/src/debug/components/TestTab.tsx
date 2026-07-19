import { useState, useEffect } from 'preact/hooks'
import type { JSX } from 'preact'
import type { DebugStore } from '../store'
import { downloadDiagnostics } from '../diagnostics'

interface Props {
  store: DebugStore
}

const RENDER_TICK_MS = 1000

export function TestTab({ store }: Props): JSX.Element {
  const [, forceRender] = useState(0)
  useEffect(() => {
    const id = setInterval(() => forceRender((n) => n + 1), RENDER_TICK_MS)
    return () => clearInterval(id)
  }, [])

  const testActions = store.testActions.value
  const latencyMs = store.latencyMs.value

  if (!testActions) {
    return (
      <div class="debug-section" style={{ color: '#999' }}>
        Connect to a stream first.
      </div>
    )
  }

  return (
    <>
      <div class="debug-section">
        <h3>Decoder Tests</h3>
        <div style={{ marginBottom: '12px' }}>
          <button
            onClick={() => testActions.resetDecoder()}
            style={{ background: '#443', color: '#fc6', padding: '6px 12px', cursor: 'pointer', border: '1px solid #555', borderRadius: '3px' }}
          >
            Reset VideoDecoder
          </button>
          <div style={{ color: '#888', fontSize: '11px', marginTop: '2px' }}>
            Drops all decoder state. Will re-sync on next keyframe.
          </div>
        </div>
      </div>

      <div class="debug-section">
        <h3>Connection Tests</h3>
        <div style={{ marginBottom: '12px' }}>
          <button
            onClick={() => testActions.reconnect()}
            style={{ background: '#433', color: '#f66', padding: '6px 12px', cursor: 'pointer', border: '1px solid #555', borderRadius: '3px' }}
          >
            Force Reconnect
          </button>
          <div style={{ color: '#888', fontSize: '11px', marginTop: '2px' }}>
            Tears down and reconnects the WebTransport session.
          </div>
        </div>
      </div>

      <div class="debug-section">
        <h3>Latency Tests</h3>
        <div style={{ marginBottom: '12px' }}>
          <button
            onClick={() => testActions.cycleLatency()}
            style={{ background: '#345', color: '#9cf', padding: '6px 12px', cursor: 'pointer', border: '1px solid #555', borderRadius: '3px' }}
          >
            Cycle Latency (120→500→2000ms)
          </button>
          <div style={{ color: '#888', fontSize: '11px', marginTop: '2px' }}>
            Cycles through common latency presets. Reconnect to apply. Current: {latencyMs}ms
          </div>
        </div>
      </div>

      <div class="debug-section">
        <h3>Info</h3>
        <div style={{ color: '#aaa', fontSize: '12px', marginBottom: '8px' }}>
          These buttons help test error recovery and resilience. Use alongside the Codec and SRT tabs to observe effects.
        </div>
        <div style={{ color: '#888', fontSize: '11px' }}>
          The gateway's --sim-loss flag is the recommended way to test NAK/retransmit.
        </div>
      </div>

      <div class="debug-section">
        <h3>Diagnostics</h3>
        <div style={{ display: 'flex', gap: '8px' }}>
          <button
            onClick={() => copyDiagnostics(store)}
            title="Copy debug info to clipboard"
            style={{ background: '#333', color: '#ddd', padding: '6px 12px', cursor: 'pointer', border: '1px solid #555', borderRadius: '3px' }}
          >
            Copy Info
          </button>
          <button
            onClick={() => downloadDiagnostics(store)}
            title="Download debug info as JSON file"
            style={{ background: '#333', color: '#ddd', padding: '6px 12px', cursor: 'pointer', border: '1px solid #555', borderRadius: '3px' }}
          >
            Download
          </button>
        </div>
      </div>
    </>
  )
}

async function copyDiagnostics(store: DebugStore) {
  try {
    const { buildDiagnostics } = await import('../diagnostics')
    const diag = await buildDiagnostics(store)
    const json = JSON.stringify(diag, null, 2)
    await navigator.clipboard.writeText(json)
    console.info('Debug diagnostics copied to clipboard')
  } catch (e) {
    console.error('Failed to copy diagnostics:', e)
  }
}
