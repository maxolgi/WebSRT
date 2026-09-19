// Debug panel deep-linking. Hash format: #<tab> or #demux/<subtab>.

export function readHash(): { tab: string; demuxSub: string | null } {
  const parts = location.hash.replace(/^#/, '').split('/')
  const tab = parts[0] ?? ''
  return { tab, demuxSub: tab === 'demux' ? (parts[1] || null) : null }
}

export function writeHash(tab: string, demuxSub?: string | null): void {
  history.replaceState(null, '', `#${tab}${demuxSub ? `/${demuxSub}` : ''}`)
}
