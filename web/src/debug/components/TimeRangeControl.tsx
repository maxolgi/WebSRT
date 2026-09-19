import type { JSX } from 'preact'
import type { DebugStore } from '../store'
import { useSignals } from '../useSignals'

const PRESETS = [10, 20, 30]

// Global time-window + scrub control. Sits between the tab bar and the content
// in Panel.tsx. Every time-based chart reads `timeWindowSec` and `focusTime`
// from the store, so the window and the playhead persist across tab switches.
export function TimeRangeControl({ store }: { store: DebugStore }): JSX.Element {
  useSignals(store.history, store.timeWindowSec, store.focusTime)

  const windowSec = store.timeWindowSec.value
  const focusTime = store.focusTime.value
  const history = store.history.value
  const latestT = history[history.length - 1]?.t ?? 0
  const scrubVal =
    focusTime !== null ? Math.min(Math.max((latestT - focusTime) / 1000, 0), windowSec) : 0

  return (
    <div
      style={{
        display: 'flex',
        alignItems: 'center',
        gap: '8px',
        padding: '4px 8px',
        fontSize: '11px',
        color: '#888',
        borderBottom: '1px solid #2a2a2a',
        flexShrink: 0,
        userSelect: 'none',
      }}
    >
      {PRESETS.map((p) => {
        const active = windowSec === p
        return (
          <button
            key={p}
            onClick={() => {
              store.timeWindowSec.value = p
            }}
            style={{
              padding: '2px 8px',
              fontSize: '11px',
              cursor: 'pointer',
              border: `1px solid ${active ? '#6cf' : '#333'}`,
              borderRadius: '3px',
              background: active ? 'rgba(102,204,255,0.12)' : 'none',
              color: active ? '#6cf' : '#888',
              font: 'inherit',
              lineHeight: '1.2',
            }}
          >
            {p}s
          </button>
        )
      })}
      <input
        type="range"
        min={0}
        max={windowSec}
        step={0.5}
        value={scrubVal}
        onInput={(e) => {
          const v = parseFloat(e.currentTarget.value)
          store.focusTime.value = v <= 0 ? null : latestT - v * 1000
        }}
        style={{ flex: '1 1 80px', minWidth: '60px', accentColor: '#6cf' }}
      />
      {focusTime !== null ? (
        <span
          style={{
            color: '#f66',
            display: 'inline-flex',
            alignItems: 'center',
            gap: '4px',
            whiteSpace: 'nowrap',
          }}
        >
          T−{((latestT - focusTime) / 1000).toFixed(1)}s
          <button
            onClick={() => {
              store.focusTime.value = null
            }}
            style={{
              cursor: 'pointer',
              border: 'none',
              background: 'none',
              color: '#888',
              fontSize: '12px',
              padding: '0 2px',
            }}
          >
            ×
          </button>
        </span>
      ) : (
        <span style={{ color: '#555', whiteSpace: 'nowrap' }}>no focus</span>
      )}
    </div>
  )
}
