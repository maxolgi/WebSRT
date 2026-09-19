import { useEffect, useReducer, useRef } from 'preact/hooks'
import type { Signal } from '@preact/signals-core'

// Re-render the calling component whenever any of the given signals changes.
// Store signals are stable references (created once in the DebugStore
// constructor), so we subscribe exactly once on mount.
export function useSignals(...sigs: Array<Signal<unknown>>): void {
  const [, force] = useReducer<number, void>((x: number) => x + 1, 0)
  const ref = useRef(sigs)
  ref.current = sigs
  useEffect(() => {
    const unsubs = ref.current.map((s) => s.subscribe(() => force()))
    return () => { for (const u of unsubs) u() }
  }, [])
}
