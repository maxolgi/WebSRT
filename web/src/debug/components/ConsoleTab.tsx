// Console tab: Eruda in-page console (lazy-loaded). Stub for now —
// the full Eruda integration is implemented in a later wave.

import { useEffect, useState } from 'preact/hooks'
import type { DebugStore } from '../store'

interface Props {
  store: DebugStore
}

export function ConsoleTab({ store }: Props) {
  const [loaded, setLoaded] = useState(false)
  const [erudaContainer, setErudaContainer] = useState<HTMLElement | null>(null)

  useEffect(() => {
    if (!erudaContainer || loaded) return
    let cancelled = false
    ;(async () => {
      try {
        const eruda = (await import('eruda')).default
        if (cancelled) return
        eruda.init({ container: erudaContainer })
        setLoaded(true)
      } catch (e) {
        console.error('Failed to load Eruda:', e)
      }
    })()
    return () => {
      cancelled = true
      try {
        import('eruda').then((m) => m.default.destroy()).catch(() => {})
      } catch {}
    }
  }, [erudaContainer, loaded])

  const errors = store.consoleErrors.value

  return (
    <>
      <div class="debug-section">
        <h3>Recent Errors ({errors.length})</h3>
        {errors.length === 0 ? (
          <div style={{ color: '#999' }}>No errors captured</div>
        ) : (
          <div style={{ maxHeight: '200px', overflowY: 'auto' }}>
            {errors.slice(-10).map((e, i) => (
              <div class="err" style={{ padding: '2px 0', borderBottom: '1px solid #222', wordBreak: 'break-word' }}>
                {e}
              </div>
            ))}
          </div>
        )}
      </div>
      <div class="debug-section">
        <h3>In-Page Console (Eruda)</h3>
        {!loaded && <div style={{ color: '#999' }}>Loading Eruda…</div>}
        <div
          ref={setErudaContainer}
          style={{ position: 'relative', height: '400px', overflow: 'hidden' }}
        />
      </div>
    </>
  )
}
