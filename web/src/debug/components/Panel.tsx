// Root debug panel: tab bar + content per tab.
// Tab components are imported eagerly (they're small). Chart.js and Eruda
// are lazy-loaded from within their respective tabs.

import { useCallback } from 'preact/hooks'
import type { DebugStore } from '../store'
import { downloadDiagnostics } from '../diagnostics'
import { CodecTab } from './CodecTab'
import { GpuTab } from './GpuTab'
import { SrtTab } from './SrtTab'
import { DevToolsTab } from './DevToolsTab'
import { ConsoleTab } from './ConsoleTab'

interface Props {
  store: DebugStore
}

const TABS = [
  { id: 'codec', label: 'Codec' },
  { id: 'gpu', label: 'GPU' },
  { id: 'srt', label: 'SRT' },
  { id: 'devtools', label: 'DevTools' },
  { id: 'console', label: 'Console' },
] as const

export function DebugPanel({ store }: Props) {
  const activeTab = store.activeTab.value

  const close = useCallback(() => {
    store.panelVisible.value = false
    document.getElementById('debug-root')?.classList.remove('visible')
  }, [store])

  return (
    <>
      <div class="debug-header">
        <strong style={{ flex: 1 }}>Debug Panel</strong>
        <button onClick={() => copyDiagnostics(store)} title="Copy debug info to clipboard">
          Copy Info
        </button>
        <button onClick={() => downloadDiagnostics(store)} title="Download debug info as JSON file">
          Download
        </button>
        <button onClick={close}>✕</button>
      </div>
      <div class="debug-tabs">
        {TABS.map((t) => (
          <button
            class={`debug-tab ${activeTab === t.id ? 'active' : ''}`}
            onClick={() => { store.activeTab.value = t.id }}
          >
            {t.label}
          </button>
        ))}
      </div>
      <div class="debug-content">
        {activeTab === 'codec' && <CodecTab store={store} />}
        {activeTab === 'gpu' && <GpuTab store={store} />}
        {activeTab === 'srt' && <SrtTab store={store} />}
        {activeTab === 'devtools' && <DevToolsTab />}
        {activeTab === 'console' && <ConsoleTab store={store} />}
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
