import { useState, useMemo } from 'preact/hooks'
import type { JSX } from 'preact'
import type { DebugStore } from '../store'
import { useSignals } from '../useSignals'
import { downloadDiagnostics, buildDiagnostics } from '../diagnostics'
import { diffSnapshots } from '../diff'

interface Props {
  store: DebugStore
}

export function TestTab({ store }: Props): JSX.Element {
  useSignals(store.testActions, store.latencyMs, store.snapA, store.snapB)

  const [decodePacing, setDecodePacingState] = useState(
    () => localStorage.getItem('websrt-pacing-decode') === '1',
  )
  const [renderPacing, setRenderPacingState] = useState(
    () => localStorage.getItem('websrt-pacing-render') !== '0',
  )
  const [showAll, setShowAll] = useState(false)

  const snapA = store.snapA.value
  const snapB = store.snapB.value
  const rows = useMemo(
    () => (snapA && snapB ? diffSnapshots(snapA.data, snapB.data) : null),
    [snapA, snapB],
  )
  const changedCount = rows ? rows.filter((r) => r.changed).length : 0

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
        <h3>Pacing Tests</h3>
        <div style={{ color: '#888', fontSize: '11px', marginBottom: '8px' }}>
          Decode pacing off, render pacing on by default. Toggle to A/B test.
        </div>
        <div style={{ marginBottom: '12px' }}>
          <label style={{ display: 'flex', alignItems: 'center', gap: '6px', cursor: 'pointer', color: '#ddd' }}>
            <input
              type="checkbox"
              checked={decodePacing}
              onChange={(e) => {
                const v = e.currentTarget.checked
                setDecodePacingState(v)
                testActions.setDecodePacing(v)
              }}
            />
            Decode pacing (DTS gate)
          </label>
          <div style={{ color: '#888', fontSize: '11px', marginLeft: '20px', marginTop: '2px' }}>
            Defers VideoDecoder.decode() by DTS to prevent run-ahead.
          </div>
        </div>
        <div style={{ marginBottom: '12px' }}>
          <label style={{ display: 'flex', alignItems: 'center', gap: '6px', cursor: 'pointer', color: '#ddd' }}>
            <input
              type="checkbox"
              checked={renderPacing}
              onChange={(e) => {
                const v = e.currentTarget.checked
                setRenderPacingState(v)
                testActions.setRenderPacing(v)
              }}
            />
            Render pacing (PTS ring)
          </label>
          <div style={{ color: '#888', fontSize: '11px', marginLeft: '20px', marginTop: '2px' }}>
            Gates canvas presentation by PTS (8-frame ring).
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
        <h3>A/B Snapshot</h3>
        <div style={{ color: '#888', fontSize: '11px', marginBottom: '8px' }}>
          Capture diagnostics, change something (latency, pacing, hw mode, loss), capture the other side, and diff field-by-field.
        </div>
        <div style={{ display: 'flex', gap: '24px', marginBottom: '8px' }}>
          <div>
            <button
              onClick={() => captureSnap(store, 'A')}
              style={{ background: '#343', color: '#cdc', padding: '6px 12px', cursor: 'pointer', border: '1px solid #555', borderRadius: '3px' }}
            >
              Capture A
            </button>
            {snapA && (
              <div style={{ color: '#888', fontSize: '11px', marginTop: '2px' }}>
                A captured {snapA.ts}{' '}
                <span
                  onClick={() => (store.snapA.value = null)}
                  style={{ color: '#f66', cursor: 'pointer' }}
                >
                  clear
                </span>
              </div>
            )}
          </div>
          <div>
            <button
              onClick={() => captureSnap(store, 'B')}
              style={{ background: '#343', color: '#cdc', padding: '6px 12px', cursor: 'pointer', border: '1px solid #555', borderRadius: '3px' }}
            >
              Capture B
            </button>
            {snapB && (
              <div style={{ color: '#888', fontSize: '11px', marginTop: '2px' }}>
                B captured {snapB.ts}{' '}
                <span
                  onClick={() => (store.snapB.value = null)}
                  style={{ color: '#f66', cursor: 'pointer' }}
                >
                  clear
                </span>
              </div>
            )}
          </div>
        </div>
        {rows && (
          <>
            <div style={{ color: '#aaa', fontSize: '12px', marginBottom: '4px' }}>
              A/B diff — {changedCount} of {rows.length} fields changed
            </div>
            <label style={{ display: 'flex', alignItems: 'center', gap: '6px', cursor: 'pointer', color: '#ddd', fontSize: '12px', marginBottom: '4px' }}>
              <input
                type="checkbox"
                checked={showAll}
                onChange={(e) => setShowAll(e.currentTarget.checked)}
              />
              show all fields
            </label>
            <div style={{ maxHeight: '400px', overflow: 'auto' }}>
              <table class="debug-table">
                <thead>
                  <tr>
                    <th style={{ textAlign: 'left', padding: '2px 6px' }}>Field</th>
                    <th style={{ textAlign: 'left', padding: '2px 6px' }}>A</th>
                    <th style={{ textAlign: 'right', padding: '2px 6px' }}>B</th>
                  </tr>
                </thead>
                <tbody>
                  {(showAll ? rows : rows.filter((r) => r.changed)).map((r) => (
                    <tr key={r.path} style={r.changed ? { background: 'rgba(255,102,102,0.08)' } : undefined}>
                      <td style={{ fontSize: '11px', whiteSpace: 'nowrap' }}>{r.path}</td>
                      <td style={{ fontSize: '11px', whiteSpace: 'nowrap' }}>{r.a}</td>
                      <td style={{ fontSize: '11px', whiteSpace: 'nowrap', color: r.changed ? '#f66' : undefined }}>{r.b}</td>
                    </tr>
                  ))}
                </tbody>
              </table>
            </div>
          </>
        )}
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

async function captureSnap(store: DebugStore, label: 'A' | 'B') {
  try {
    const data = await buildDiagnostics(store)
    const snap = { ts: new Date().toLocaleTimeString(), label, data }
    if (label === 'A') store.snapA.value = snap
    else store.snapB.value = snap
  } catch (e) {
    console.error('Failed to capture snapshot:', e)
  }
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
