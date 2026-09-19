import { useEffect, useState } from 'preact/hooks'
import type { DebugStore } from '../store'
import { readHash, writeHash } from '../hash'
import { StreamTab } from './StreamTab'
import { CodecTab } from './CodecTab'
import { GpuTab } from './GpuTab'
import { SrtTab } from './SrtTab'
import { DemuxTab } from './DemuxTab'
import { ConsoleTab } from './ConsoleTab'
import { TestTab } from './TestTab'
import { AudioTab } from './AudioTab'
import { TimeRangeControl } from './TimeRangeControl'

interface Props {
  store: DebugStore
}

const TABS = [
  { id: 'stream', label: 'Stream' },
  { id: 'codec', label: 'Codec' },
  { id: 'audio', label: 'Audio' },
  { id: 'gpu', label: 'GPU' },
  { id: 'srt', label: 'SRT' },
  { id: 'demux', label: 'Demux' },
  { id: 'console', label: 'Console' },
  { id: 'test', label: 'Tools' },
] as const

export function DebugPanel({ store }: Props) {
  const [, forceRender] = useState(0)
  useEffect(() => {
    const id = setInterval(() => forceRender((n) => n + 1), 200)
    return () => clearInterval(id)
  }, [])

  useEffect(() => {
    const applyHash = () => {
      const { tab } = readHash()
      if (TABS.some((t) => t.id === tab)) store.activeTab.value = tab
    }
    applyHash()
    window.addEventListener('hashchange', applyHash)
    return () => window.removeEventListener('hashchange', applyHash)
  }, [])

  const activeTab = store.activeTab.value

  return (
    <>
      <div class="debug-tabs">
        {TABS.map((t) => (
          <button
            key={t.id}
            class={`debug-tab ${activeTab === t.id ? 'active' : ''}`}
            onClick={() => { store.activeTab.value = t.id; writeHash(t.id) }}
          >
            {t.label}
          </button>
        ))}
      </div>
      <TimeRangeControl store={store} />
      <div class="debug-content">
        {activeTab === 'stream' && <StreamTab store={store} />}
        {activeTab === 'codec' && <CodecTab store={store} />}
        {activeTab === 'audio' && <AudioTab store={store} />}
        {activeTab === 'gpu' && <GpuTab store={store} />}
        {activeTab === 'srt' && <SrtTab store={store} />}
        {activeTab === 'demux' && <DemuxTab store={store} />}
        {activeTab === 'console' && <ConsoleTab store={store} />}
        {activeTab === 'test' && <TestTab store={store} />}
      </div>
    </>
  )
}
