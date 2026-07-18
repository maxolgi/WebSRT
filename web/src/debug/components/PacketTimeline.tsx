import { useEffect, useRef, useState } from 'preact/hooks';
import type { JSX } from 'preact';
import type { DebugStore } from '../store';
import type { DemuxStats } from '../types';
import {
  formatBytes,
  kindName,
  nalTypeName,
  parsePidFilter,
  pidHex,
  type VideoCodec,
} from './packetUtils';

interface Props {
  store: DebugStore;
}

// Virtualization geometry.
const ROW_HEIGHT = 24;
const OVERSCAN = 50;
const VIEWPORT_PX = 300;

// Column widths (px). NAL column is flex (fills remainder).
const W_NUM = 36;
const W_TIME = 72;
const W_PID = 54;
const W_KIND = 30;
const W_PTS = 56;
const W_DTS = 56;
const W_SIZE = 52;
const W_FLAGS = 50;

const ST_H264 = 0x1b;
const ST_HEVC = 0x24;

const KIND_OPTIONS: { value: string; label: string }[] = [
  { value: 'all', label: 'All' },
  { value: 'video', label: 'Video PES' },
  { value: 'audio', label: 'Audio PES' },
  { value: 'psi', label: 'PSI (PAT/PMT)' },
  { value: 'ra', label: 'RA' },
  { value: 'errors', label: 'Errors' },
  { value: 'other', label: 'Other' },
];

export function PacketTimeline({ store }: Props): JSX.Element {
  const [, forceRender] = useState(0);
  const [scrollTop, setScrollTop] = useState(0);
  const [selectedIdx, setSelectedIdx] = useState<number | null>(null);
  const [pidFilterText, setPidFilterText] = useState('');
  const [kindFilter, setKindFilter] = useState('all');
  const [minSize, setMinSize] = useState(0);

  const scrollRef = useRef<HTMLDivElement | null>(null);
  const wasAtTopRef = useRef(true);
  const prevRingLenRef = useRef(0);

  useEffect(() => {
    const id = setInterval(() => forceRender((n) => n + 1), 250);
    return () => clearInterval(id);
  }, []);

  const d = store.demuxStats.value;
  const ringLen = d ? d.ringT.length : 0;

  // Auto-scroll to top when the ring grows, only if the user was at the top.
  useEffect(() => {
    if (ringLen === prevRingLenRef.current) return;
    const grew = ringLen > prevRingLenRef.current;
    prevRingLenRef.current = ringLen;
    if (grew && wasAtTopRef.current && scrollRef.current) {
      scrollRef.current.scrollTop = 0;
      setScrollTop(0);
    }
  }, [ringLen]);

  if (!d) {
    return <div class="debug-section">No demux stats yet — awaiting stream.</div>;
  }

  // PID -> codec from PMT. Built fresh each render (PMT is tiny, changes rarely).
  const pidCodec = new Map<number, VideoCodec>();
  for (let i = 0; i < d.pmtPids.length; i++) {
    const pid = d.pmtPids[i];
    const st = d.pmtStreamTypes[i];
    if (st === ST_H264) pidCodec.set(pid, 'h264');
    else if (st === ST_HEVC) pidCodec.set(pid, 'hevc');
  }
  const isVideoPid = (pid: number): boolean => pidCodec.has(pid);

  // Filter -> ascending list of ring indices that pass.
  const pidFilter = parsePidFilter(pidFilterText);
  const hasPidFilter = pidFilter !== null;
  const filteredIndices: number[] = [];
  for (let i = 0; i < ringLen; i++) {
    const pid = d.ringPid[i];
    const kind = d.ringKind[i];
    if (hasPidFilter && pid !== pidFilter) continue;
    if (minSize > 0 && d.ringSize[i] < minSize) continue;
    if (kindFilter !== 'all') {
      if (kindFilter === 'video' && !(kind === 2 && isVideoPid(pid))) continue;
      if (kindFilter === 'audio' && !(kind === 2 && !isVideoPid(pid))) continue;
      if (kindFilter === 'psi' && !(kind === 0 || kind === 1)) continue;
      if (kindFilter === 'ra' && kind !== 3) continue;
      if (kindFilter === 'errors' && kind !== 4) continue;
      if (kindFilter === 'other' && kind !== 255) continue;
    }
    filteredIndices.push(i);
  }
  const displayLen = filteredIndices.length;

  // Virtualization window over display rows (row 0 = newest).
  const startIdx = Math.max(0, Math.floor(scrollTop / ROW_HEIGHT) - OVERSCAN);
  const endIdx = Math.min(
    displayLen,
    startIdx + Math.ceil(VIEWPORT_PX / ROW_HEIGHT) + OVERSCAN * 2,
  );
  const topSpacer = startIdx * ROW_HEIGHT;
  const botSpacer = (displayLen - endIdx) * ROW_HEIGHT;

  const visible: { displayNum: number; ringIdx: number }[] = [];
  for (let r = startIdx; r < endIdx; r++) {
    const ringIdx = filteredIndices[displayLen - 1 - r];
    visible.push({ displayNum: ringIdx + 1, ringIdx });
  }

  // If the selected packet aged out of the ring, drop the selection.
  const sel = selectedIdx !== null && selectedIdx < ringLen ? selectedIdx : null;

  return (
    <div class="packet-split">
      <LiveHeader d={d} displayLen={displayLen} filtered={displayLen !== ringLen} />

      {/* Filter row */}
      <div class="pkt-filterbar">
        <input
          type="text"
          placeholder="PID 0x1010 or 4112"
          value={pidFilterText}
          onInput={(e) => setPidFilterText(e.currentTarget.value)}
        />
        <select
          value={kindFilter}
          onChange={(e) => setKindFilter(e.currentTarget.value)}
        >
          {KIND_OPTIONS.map((o) => (
            <option value={o.value}>{o.label}</option>
          ))}
        </select>
        <input
          type="number"
          placeholder="min B"
          value={minSize}
          min={0}
          onInput={(e) => setMinSize(e.currentTarget.valueAsNumber || 0)}
        />
        <button
          onClick={() => {
            setPidFilterText('');
            setKindFilter('all');
            setMinSize(0);
          }}
        >
          Clear
        </button>
      </div>

      {/* Virtualized packet list */}
      <div
        class="packet-scroll"
        ref={scrollRef}
        onScroll={(e) => {
          const el = e.currentTarget;
          wasAtTopRef.current = el.scrollTop < 4;
          setScrollTop(el.scrollTop);
        }}
      >
        {ringLen === 0 ? (
          <div class="pkt-muted" style={{ padding: '12px' }}>Waiting for packets…</div>
        ) : displayLen === 0 ? (
          <div class="pkt-muted" style={{ padding: '12px' }}>No packets match filter.</div>
        ) : (
          <table class="packet-list">
            <thead>
              <tr>
                <th style={{ width: W_NUM }}>#</th>
                <th style={{ width: W_TIME }}>Time</th>
                <th style={{ width: W_PID }}>PID</th>
                <th class="pkt-c-kind" style={{ width: W_KIND }}>K</th>
                <th style={{ width: W_PTS }}>PTS</th>
                <th style={{ width: W_DTS }}>DTS</th>
                <th style={{ width: W_SIZE }}>Size</th>
                <th class="pkt-c-nal">NAL</th>
                <th style={{ width: W_FLAGS }}>Flags</th>
              </tr>
            </thead>
            <tbody style={{ paddingTop: topSpacer, paddingBottom: botSpacer }}>
              {visible.map(({ displayNum, ringIdx }) => (
                <Row
                  key={ringIdx}
                  d={d}
                  ringIdx={ringIdx}
                  displayNum={displayNum}
                  codec={pidCodec.get(d.ringPid[ringIdx]) ?? null}
                  isVideo={isVideoPid(d.ringPid[ringIdx])}
                  selected={sel === ringIdx}
                  onSelect={() => setSelectedIdx(ringIdx)}
                  onCopy={() => copyPacket(d, ringIdx, pidCodec.get(d.ringPid[ringIdx]) ?? null)}
                  onFilter={() => setPidFilterText(pidHex(d.ringPid[ringIdx]))}
                />
              ))}
            </tbody>
          </table>
        )}
      </div>

      {/* Inspector */}
      {sel !== null ? (
        <PacketInspector d={d} idx={sel} pidCodec={pidCodec} isVideoPid={isVideoPid} />
      ) : (
        <div class="pkt-inspector pkt-muted">Select a packet to inspect.</div>
      )}
    </div>
  );
}

interface RowProps {
  d: DemuxStats;
  ringIdx: number;
  displayNum: number;
  codec: VideoCodec;
  isVideo: boolean;
  selected: boolean;
  onSelect: () => void;
  onCopy: () => void;
  onFilter: () => void;
}

function Row({
  d, ringIdx, displayNum, codec, isVideo, selected, onSelect, onCopy, onFilter,
}: RowProps): JSX.Element {
  const pid = d.ringPid[ringIdx];
  const kind = d.ringKind[ringIdx];
  const t = d.ringT[ringIdx];
  const pts = d.ringPts[ringIdx];
  const dtsV = d.ringDts[ringIdx];
  const size = d.ringSize[ringIdx];
  const ccErr = !!d.ringCcErr[ringIdx];
  const tei = !!d.ringTei[ringIdx];
  const ra = !!d.ringRa[ringIdx];
  const nal = nalSummary(d, ringIdx, codec);
  const cls = rowClass(kind, isVideo);
  const title = `PID ${pidHex(pid)} (${pid}) • t=${(t / 1000).toFixed(3)}s • kind=${kindName(kind)} • size=${size}B • CC err=${ccErr} • TEI=${tei}`;

  return (
    <tr
      class={`${cls}${selected ? ' pkt-row-selected' : ''}`}
      style={{ height: ROW_HEIGHT }}
      title={title}
      onClick={onSelect}
    >
      <td class="pkt-c-num" style={{ width: W_NUM }}>{displayNum}</td>
      <td style={{ width: W_TIME }}>{(t / 1000).toFixed(3)}s</td>
      <td style={{ width: W_PID }}>{pidHex(pid)}</td>
      <td class="pkt-c-kind" style={{ width: W_KIND }}>{kindLetter(kind, isVideo)}</td>
      <td style={{ width: W_PTS }}>{pts >= 0 ? `${(pts / 90).toFixed(0)}ms` : '—'}</td>
      <td style={{ width: W_DTS }}>{dtsV >= 0 ? `${(dtsV / 90).toFixed(0)}ms` : '—'}</td>
      <td style={{ width: W_SIZE }}>{formatBytes(size)}</td>
      <td class="pkt-c-nal">{nal}</td>
      <td style={{ width: W_FLAGS }}>
        {ccErr && <span class="pkt-badge pkt-badge-bad">CC</span>}
        {tei && <span class="pkt-badge pkt-badge-bad">TEI</span>}
        {ra && <span class="pkt-badge pkt-badge-good">RA</span>}
      </td>
      <span class="pkt-actions">
        <button
          class="pkt-mini"
          title="Copy packet JSON"
          onClick={(e) => { e.stopPropagation(); onCopy(); }}
        >📋</button>
        <button
          class="pkt-mini"
          title="Filter by this PID"
          onClick={(e) => { e.stopPropagation(); onFilter(); }}
        >Filter</button>
      </span>
    </tr>
  );
}

interface InspectorProps {
  d: DemuxStats;
  idx: number;
  pidCodec: Map<number, VideoCodec>;
  isVideoPid: (pid: number) => boolean;
}

function PacketInspector({ d, idx, pidCodec, isVideoPid }: InspectorProps): JSX.Element {
  const pid = d.ringPid[idx];
  const kind = d.ringKind[idx];
  const codec = pidCodec.get(pid) ?? null;
  const isVid = isVideoPid(pid);
  const t = d.ringT[idx];
  const pts = d.ringPts[idx];
  const dtsV = d.ringDts[idx];
  const size = d.ringSize[idx];
  const ccErr = !!d.ringCcErr[idx];
  const tei = !!d.ringTei[idx];
  const pusi = !!d.ringPusi[idx];
  const ra = !!d.ringRa[idx];

  const prev = prevSamePid(d, idx);
  const dtPrev = prev >= 0 ? t - d.ringT[prev] : null;

  // PTS jump vs previous same-PID packet (> 1 s triggers a warning).
  let ptsJumpMs: number | null = null;
  if (prev >= 0 && pts >= 0 && d.ringPts[prev] >= 0) {
    const delta = pts - d.ringPts[prev];
    if (Math.abs(delta) > 90000) ptsJumpMs = delta / 90;
  }

  const nalDesc = nalDescription(d, idx, codec);

  return (
    <div class="pkt-inspector">
      <h4>Packet Header</h4>
      <table>
        <tbody>
          <tr><td>PID</td><td>{pidHex(pid)} ({pid})</td></tr>
          <tr><td>Kind</td><td>{kindName(kind)} — {kindDescription(kind, isVid, ra)}</td></tr>
          <tr><td>TEI</td><td class={tei ? 'stat-bad' : ''}>{tei ? 'SET ⚠️' : 'clear'}</td></tr>
          <tr><td>PUSI</td><td>{pusi ? 'SET' : 'clear'}</td></tr>
          <tr><td>CC</td><td class={ccErr ? 'stat-bad' : 'stat-good'}>{ccErr ? 'DISCONTINUITY ⚠️' : 'ok'}</td></tr>
          <tr><td>Random access</td><td class={ra ? 'stat-good' : ''}>{ra ? 'SET (keyframe)' : 'clear'}</td></tr>
        </tbody>
      </table>

      <h4>Timing</h4>
      <table>
        <tbody>
          <tr><td>Timestamp</td><td>{(t / 1000).toFixed(3)}s</td></tr>
          <tr><td>PTS</td><td>{pts >= 0 ? `${(pts / 90).toFixed(2)} ms` : '—'}</td></tr>
          <tr><td>DTS</td><td>{dtsV >= 0 ? `${(dtsV / 90).toFixed(2)} ms` : '—'}</td></tr>
          <tr>
            <td>PTS−DTS</td>
            <td>{pts >= 0 && dtsV >= 0 ? `${((pts - dtsV) / 90).toFixed(2)} ms` : '—'}</td>
          </tr>
          <tr>
            <td>Δt prev same PID</td>
            <td>{dtPrev !== null ? `${dtPrev.toFixed(1)} ms` : '— (first)'}</td>
          </tr>
        </tbody>
      </table>

      <h4>Payload</h4>
      <table>
        <tbody>
          <tr><td>Size</td><td>{size} B ({formatBytes(size)})</td></tr>
          {nalDesc && <tr><td>NAL types</td><td>{nalDesc}</td></tr>}
        </tbody>
      </table>

      {(ccErr || tei || ptsJumpMs !== null) && (
        <>
          <h4>Warnings</h4>
          <table>
            <tbody>
              {ccErr && <tr><td colspan={2} class="pkt-warn">⚠️ Continuity counter discontinuity</td></tr>}
              {tei && <tr><td colspan={2} class="pkt-warn">⚠️ Transport error indicator set</td></tr>}
              {ptsJumpMs !== null && (
                <tr>
                  <td colspan={2} class="pkt-warn">
                    ⚠️ PTS jump: {ptsJumpMs > 0 ? '+' : ''}{ptsJumpMs.toFixed(0)} ms
                  </td>
                </tr>
              )}
            </tbody>
          </table>
        </>
      )}

      <h4>Hex Dump</h4>
      <div class="pkt-muted">Hex dump requires WASM update (pending)</div>
    </div>
  );
}

function LiveHeader({
  d, displayLen, filtered,
}: {
  d: DemuxStats;
  displayLen: number;
  filtered: boolean;
}): JSX.Element {
  const ringLen = d.ringT.length;
  if (ringLen === 0) {
    return <div class="pkt-live"><span class="pkt-dot">●</span> LIVE • waiting for packets…</div>;
  }
  const span = d.ringT[ringLen - 1] - d.ringT[0];
  return (
    <div class="pkt-live">
      <span class="pkt-dot">●</span> LIVE • ring: {ringLen} events (~{Math.round(span / 1000)}s)
      {' '}• showing newest first
      {filtered ? ` • filtered: ${displayLen}` : ''}
    </div>
  );
}

// ---- pure helpers (module scope) ----

function rowClass(kind: number, isVideo: boolean): string {
  switch (kind) {
    case 0: case 1: return 'pkt-row-psi';
    case 2: return isVideo ? 'pkt-row-video' : 'pkt-row-audio';
    case 3: return 'pkt-row-ra';
    case 4: return 'pkt-row-error';
    default: return 'pkt-row-other';
  }
}

function kindLetter(kind: number, isVideo: boolean): string {
  switch (kind) {
    case 0: case 1: return 'P';
    case 2: return isVideo ? 'V' : 'A';
    case 3: return 'R';
    case 4: return 'E';
    default: return '?';
  }
}

function kindDescription(kind: number, isVideo: boolean, ra: boolean): string {
  switch (kind) {
    case 0: return 'PAT section';
    case 1: return 'PMT section';
    case 2: return isVideo ? (ra ? 'Video PES (keyframe)' : 'Video PES continuation') : 'Audio PES';
    case 3: return 'Random access point';
    case 4: return 'Demux error';
    default: return 'Other / raw';
  }
}

function nalSummary(d: DemuxStats, idx: number, codec: VideoCodec): string {
  const s = d.ringNalOffsets[idx];
  const e = d.ringNalOffsets[idx + 1];
  if (e <= s) return '';
  const counts = new Map<number, number>();
  for (let i = s; i < e; i++) {
    const b = d.ringNal[i];
    counts.set(b, (counts.get(b) ?? 0) + 1);
  }
  const parts: string[] = [];
  counts.forEach((c, b) => {
    const name = nalTypeName(b, codec);
    parts.push(c > 1 ? `${name}×${c}` : name);
  });
  return parts.join(' ');
}

function nalDescription(d: DemuxStats, idx: number, codec: VideoCodec): string {
  const s = d.ringNalOffsets[idx];
  const e = d.ringNalOffsets[idx + 1];
  if (e <= s) return '';
  const counts = new Map<number, number>();
  for (let i = s; i < e; i++) {
    const b = d.ringNal[i];
    counts.set(b, (counts.get(b) ?? 0) + 1);
  }
  const parts: string[] = [];
  counts.forEach((c, b) => {
    parts.push(`${nalTypeName(b, codec)} (${b})${c > 1 ? ` × ${c}` : ''}`);
  });
  return parts.join(', ');
}

function prevSamePid(d: DemuxStats, idx: number): number {
  const pid = d.ringPid[idx];
  for (let j = idx - 1; j >= 0; j--) {
    if (d.ringPid[j] === pid) return j;
  }
  return -1;
}

function copyPacket(d: DemuxStats, idx: number, codec: VideoCodec): void {
  const nal: number[] = [];
  const s = d.ringNalOffsets[idx];
  const e = d.ringNalOffsets[idx + 1];
  for (let i = s; i < e; i++) nal.push(d.ringNal[i]);
  const obj = {
    t: d.ringT[idx],
    pid: d.ringPid[idx],
    kind: d.ringKind[idx],
    kindName: kindName(d.ringKind[idx]),
    pts: d.ringPts[idx],
    dts: d.ringDts[idx],
    size: d.ringSize[idx],
    ra: !!d.ringRa[idx],
    ccError: !!d.ringCcErr[idx],
    tei: !!d.ringTei[idx],
    pusi: !!d.ringPusi[idx],
    codec,
    nalTypes: nal,
  };
  void navigator.clipboard.writeText(JSON.stringify(obj, null, 2));
}
