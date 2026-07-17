// Root debug panel: tab bar + lazy-loaded content per tab.
// Each tab component is dynamically imported on first activation to keep
// the initial bundle small.

import { useEffect, useState, useCallback } from 'preact/hooks';
import type { DebugStore } from '../store';

interface Props {
  store: DebugStore;
}

const TABS = [
  { id: 'codec', label: 'Codec' },
  { id: 'gpu', label: 'GPU' },
  { id: 'srt', label: 'SRT' },
  { id: 'devtools', label: 'DevTools' },
  { id: 'console', label: 'Console' },
] as const;

export function DebugPanel({ store }: Props) {
  const activeTab = store.activeTab.value;

  const close = useCallback(() => {
    store.panelVisible.value = false;
    document.getElementById('debug-root')?.classList.remove('visible');
  }, [store]);

  return (
    <>
      <div class="debug-header">
        <strong style={{ flex: 1 }}>Debug Panel</strong>
        <button onClick={() => copyDiagnostics(store)} title="Copy debug info to clipboard">
          Copy Info
        </button>
        <button onClick={close}>✕</button>
      </div>
      <div class="debug-tabs">
        {TABS.map((t) => (
          <button
            class={`debug-tab ${activeTab === t.id ? 'active' : ''}`}
            onClick={() => { store.activeTab.value = t.id; }}
          >
            {t.label}
          </button>
        ))}
      </div>
      <div class="debug-content">
        {activeTab === 'codec' && <div>Codec tab — loading…</div>}
        {activeTab === 'gpu' && <div>GPU tab — loading…</div>}
        {activeTab === 'srt' && <div>SRT tab — loading…</div>}
        {activeTab === 'devtools' && <div>DevTools tab — loading…</div>}
        {activeTab === 'console' && <div>Console tab — loading…</div>}
      </div>
    </>
  );
}

async function copyDiagnostics(store: DebugStore) {
  try {
    const { buildDiagnostics } = await import('../diagnostics');
    const diag = await buildDiagnostics(store);
    const json = JSON.stringify(diag, null, 2);
    await navigator.clipboard.writeText(json);
    console.info('Debug diagnostics copied to clipboard');
  } catch (e) {
    console.error('Failed to copy diagnostics:', e);
  }
}
