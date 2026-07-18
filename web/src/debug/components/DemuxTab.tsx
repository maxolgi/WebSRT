import { useState, useEffect } from 'preact/hooks';
import type { JSX } from 'preact';
import type { DebugStore } from '../store';
import type { DemuxStats } from '../types';
import { streamTypeName } from './streamTypes';
import { BitrateChart } from './charts/BitrateChart';
import { PidDonutChart } from './charts/PidDonutChart';
import { CcHeatmap } from './charts/CcHeatmap';
import { RaTimeline } from './charts/RaTimeline';
import { PtsJumpSparkline } from './charts/PtsJumpSparkline';
import { PcrChart } from './charts/PcrChart';
import { NalStackedBar } from './charts/NalStackedBar';
import { PacketTimeline } from './PacketTimeline';

const ST_H264 = 0x1b;
const ST_HEVC = 0x24;
const ST_AAC = 0x0f;
const ST_PRIVATE = 0x06;

interface Props {
  store: DebugStore;
}

export function DemuxTab({ store }: Props): JSX.Element {
  const [, forceRender] = useState(0);
  useEffect(() => {
    const id = setInterval(() => forceRender((n) => n + 1), 250);
    return () => clearInterval(id);
  }, []);

  const d = store.demuxStats.value;
  if (!d) return <div>No demux stats yet — awaiting stream.</div>;

  // Per-PID index lookup (small N — linear scan is fine).
  const idxOf = (pid: number): number => {
    for (let i = 0; i < d.pids.length; i++) if (d.pids[i] === pid) return i;
    return -1;
  };

  // Resolve codec name for a PMT entry, including 0x06 disambiguation.
  const codecName = (streamType: number, formatId: string): string => {
    if (streamType === ST_PRIVATE) {
      if (formatId === 'AV01') return 'AV1';
      if (formatId === 'Opus') return 'Opus';
      return formatId ? `Private (${formatId})` : 'Private';
    }
    return streamTypeName(streamType);
  };

  // Two highest-by-byteTotal PIDs (video first, audio second) for PTS panel.
  const top2 = [...d.pids.keys()]
    .sort((a, b) => d.byteTotals[b] - d.byteTotals[a])
    .slice(0, 2)
    .map((i) => d.pids[i]);

  const totalCcErrors = d.ccErrors.reduce((a, b) => a + b, 0);

  return (
    <>
      {/* 1. Program table */}
      <div class="debug-section">
        <h3>Program</h3>
        <table class="debug-table">
          <tbody>
            <tr><td>Program Number</td><td>{d.programNum >= 0 ? d.programNum : '—'}</td></tr>
            <tr><td>PMT PID</td><td>{d.pmtPid >= 0 ? pidCell(d.pmtPid) : '—'}</td></tr>
          </tbody>
        </table>
      </div>

      {/* 2. Elementary streams table */}
      <div class="debug-section">
        <h3>Elementary Streams</h3>
        <table class="debug-table">
          <thead>
            <tr>
              <th>PID</th><th>Type</th><th>Format ID</th><th>Mbps</th>
              <th>PES</th><th>RA</th><th>CC err</th>
            </tr>
          </thead>
          <tbody>
            {d.pmtPids.length === 0 ? (
              <tr><td colspan={7}>No PMT entries yet</td></tr>
            ) : Array.from(d.pmtPids).map((pid, i) => {
              const st = d.pmtStreamTypes[i];
              const fmt = d.pmtFormatIds[i] ?? '';
              const idx = idxOf(pid);
              const mbps = idx >= 0 ? d.bitratesMbps[idx] : null;
              const pes = idx >= 0 ? d.pesCounts[idx] : 0;
              const ra = idx >= 0 ? d.raCounts[idx] : 0;
              const cc = idx >= 0 ? d.ccErrors[idx] : 0;
              return (
                <tr key={pid}>
                  <td>{pidCell(pid)}</td>
                  <td>{codecName(st, fmt)}</td>
                  <td>{fmt || '—'}</td>
                  <td>{mbps !== null ? mbps.toFixed(3) : '—'}</td>
                  <td>{pes}</td>
                  <td>{ra}</td>
                  <td class={cc > 0 ? 'stat-bad' : 'stat-good'}>{cc}</td>
                </tr>
              );
            })}
          </tbody>
        </table>
      </div>

      {/* 3. PTS / DTS panel */}
      <div class="debug-section">
        <h3>PTS / DTS (top-2 PIDs by bytes)</h3>
        <table class="debug-table">
          <thead>
            <tr><th>PID</th><th>last PTS (ms)</th><th>last DTS (ms)</th><th>PTS jumps</th></tr>
          </thead>
          <tbody>
            {top2.length === 0 ? (
              <tr><td colspan={4}>No PIDs yet</td></tr>
            ) : top2.map((pid) => {
              const idx = idxOf(pid);
              const pts = idx >= 0 ? d.lastPts[idx] : -1;
              const dtsV = idx >= 0 ? d.lastDts[idx] : -1;
              const jumps = idx >= 0 ? d.ptsJumps[idx] : 0;
              return (
                <tr key={pid}>
                  <td>{pidCell(pid)}</td>
                  <td>{pts >= 0 ? (pts / 90).toFixed(1) : '—'}</td>
                  <td>{dtsV >= 0 ? (dtsV / 90).toFixed(1) : '—'}</td>
                  <td class={jumps > 0 ? 'stat-bad' : ''}>{jumps}</td>
                </tr>
              );
            })}
          </tbody>
        </table>
      </div>

      {/* 4. CC errors table */}
      <div class="debug-section">
        <h3>Continuity Counter Errors <span class={totalCcErrors > 0 ? 'stat-bad' : 'stat-good'}>(total {totalCcErrors})</span></h3>
        <table class="debug-table">
          <thead><tr><th>PID</th><th>CC errors</th></tr></thead>
          <tbody>
            {d.pids.length === 0 ? (
              <tr><td colspan={2}>No PIDs yet</td></tr>
            ) : Array.from(d.pids).map((pid, i) => (
              <tr key={pid}>
                <td>{pidCell(pid)}</td>
                <td class={d.ccErrors[i] > 0 ? 'stat-bad' : 'stat-good'}>{d.ccErrors[i]}</td>
              </tr>
            ))}
          </tbody>
        </table>
      </div>

      {/* 5. TS header flags panel */}
      <div class="debug-section">
        <h3>TS Header Flags</h3>
        <table class="debug-table">
          <thead>
            <tr>
              <th>PID</th><th>TEI</th><th>PUSI</th>
              <th>Scramble (0/Even/Odd)</th><th>Adapt (Pay/Adp/Both)</th>
            </tr>
          </thead>
          <tbody>
            {d.pids.length === 0 ? (
              <tr><td colspan={5}>No PIDs yet</td></tr>
            ) : Array.from(d.pids).map((pid, i) => {
              const o4 = 4 * i;
              const sc = d.scramblingCounts; // [NotScrambled, _, Even, Odd]
              const af = d.afControlCounts;   // [_, Pay, Adp, Both]
              return (
                <tr key={pid}>
                  <td>{pidCell(pid)}</td>
                  <td class={d.teiCounts[i] > 0 ? 'stat-bad' : ''}>{d.teiCounts[i]}</td>
                  <td>{d.pusiCounts[i]}</td>
                  <td>{sc[o4]}/{sc[o4 + 2]}/{sc[o4 + 3]}</td>
                  <td>{af[o4 + 1]}/{af[o4 + 2]}/{af[o4 + 3]}</td>
                </tr>
              );
            })}
          </tbody>
        </table>
      </div>

      {/* 6. PCR panel */}
      <div class="debug-section">
        <h3>PCR</h3>
        <table class="debug-table">
          <thead><tr><th>PID</th><th>interval (ms)</th><th>jitter (ms)</th></tr></thead>
          <tbody>
            {d.pcrPids.length === 0 ? (
              <tr><td colspan={3}>No PCR-bearing PID seen yet</td></tr>
            ) : Array.from(d.pcrPids).map((pid, i) => {
              const interval = d.pcrIntervalsMs[i];
              const jitter = d.pcrJitterMs[i];
              const bad = interval > 100;
              return (
                <tr key={pid}>
                  <td>{pidCell(pid)}</td>
                  <td class={bad ? 'stat-bad' : ''}>{interval.toFixed(1)}</td>
                  <td>{jitter.toFixed(2)}</td>
                </tr>
              );
            })}
          </tbody>
        </table>
        <div style={{ marginTop: '6px' }}>
          <PcrChart store={store} height={100} />
        </div>
      </div>

      {/* 7. NAL frame-type breakdown */}
      <div class="debug-section">
        <h3>NAL Frame Types</h3>
        <table class="debug-table">
          <thead>
            <tr>
              <th>PID</th><th>Codec</th><th>I</th><th>P</th><th>B</th>
              <th>IDR</th><th>SPS</th><th>PPS</th><th>SEI</th><th>AUD</th><th>NonIDR</th>
            </tr>
          </thead>
          <tbody>
            {nalRows(d)}
          </tbody>
        </table>
        <div style={{ marginTop: '6px' }}>
          <NalStackedBar store={store} height={140} />
        </div>
      </div>

      {/* 8. Error log */}
      <div class="debug-section">
        <h3>Demux Errors {d.errorMsg.length > 0 && <span class="stat-bad">({d.errorMsg.length})</span>}</h3>
        {d.errorMsg.length === 0 ? (
          <div class="stat-good">No demux errors.</div>
        ) : (
          <table class="debug-table">
            <thead><tr><th>t (s)</th><th>message</th></tr></thead>
            <tbody>
              {d.errorMsg.map((msg, i) => (
                <tr key={i}>
                  <td>{(d.errorT[i] / 1000).toFixed(2)}</td>
                  <td class="stat-bad">{msg}</td>
                </tr>
              ))}
            </tbody>
          </table>
        )}
      </div>

      {/* 9. Packet timeline + inspector */}
      <div class="debug-section">
        <h3>Packet Timeline</h3>
        <PacketTimeline store={store} />
      </div>

      {/* Charts */}
      <div class="debug-section">
        <h3>Bitrate</h3>
        <BitrateChart store={store} height={120} />
      </div>
      <div class="debug-section">
        <h3>Byte Distribution</h3>
        <PidDonutChart store={store} height={180} />
      </div>
      <div class="debug-section">
        <h3>CC Error Heatmap</h3>
        <CcHeatmap store={store} height={40} />
      </div>
      <div class="debug-section">
        <h3>Random-Access Cadence (video PID)</h3>
        <RaTimeline store={store} height={60} />
      </div>
      <div class="debug-section">
        <h3>PTS Jump Sparkline (cumulative)</h3>
        <PtsJumpSparkline store={store} height={50} />
      </div>
    </>
  );
}

/** NAL rows: one per NAL PID; AV1 PIDs (in PMT, not in nalPids) show N/A. */
function nalRows(d: DemuxStats): JSX.Element {
  // Build the set of video PIDs that should appear (H.264/HEVC/AV1 from PMT).
  const videoPids: { pid: number; codec: string }[] = [];
  for (let i = 0; i < d.pmtPids.length; i++) {
    const st = d.pmtStreamTypes[i];
    const fmt = d.pmtFormatIds[i] ?? '';
    if (st === ST_H264 || st === ST_HEVC) {
      videoPids.push({ pid: d.pmtPids[i], codec: st === ST_H264 ? 'H.264' : 'HEVC' });
    } else if (st === ST_PRIVATE && fmt === 'AV01') {
      videoPids.push({ pid: d.pmtPids[i], codec: 'AV1' });
    }
  }
  if (videoPids.length === 0 && d.nalPids.length === 0) {
    return <tr><td colspan={11}>No video PID seen yet</td></tr>;
  }
  return (
    <>
      {videoPids.map((v) => {
        const j = d.nalPids.indexOf(v.pid);
        if (j < 0) {
          return (
            <tr key={v.pid}>
              <td>{pidCell(v.pid)}</td>
              <td>{v.codec}</td>
              <td colspan={9} class="stat-warn">No NAL stats (AV1 uses OBU syntax — N/A)</td>
            </tr>
          );
        }
        const off = 9 * j;
        const s = d.nalStats;
        return (
          <tr key={v.pid}>
            <td>{pidCell(v.pid)}</td>
            <td>{v.codec}</td>
            <td>{s[off + 0]}</td>
            <td>{s[off + 1]}</td>
            <td>{s[off + 2]}</td>
            <td>{s[off + 3]}</td>
            <td>{s[off + 4]}</td>
            <td>{s[off + 5]}</td>
            <td>{s[off + 6]}</td>
            <td>{s[off + 7]}</td>
            <td>{s[off + 8]}</td>
          </tr>
        );
      })}
      {/* Any NAL PID not covered by the PMT loop (edge case). */}
      {Array.from(d.nalPids)
        .filter((pid) => !videoPids.some((v) => v.pid === pid))
        .map((pid, k) => {
          const off = 9 * (d.nalPids.indexOf(pid));
          const s = d.nalStats;
          return (
            <tr key={`x-${pid}-${k}`}>
              <td>{pidCell(pid)}</td>
              <td>?</td>
              <td>{s[off + 0]}</td><td>{s[off + 1]}</td><td>{s[off + 2]}</td>
              <td>{s[off + 3]}</td><td>{s[off + 4]}</td><td>{s[off + 5]}</td>
              <td>{s[off + 6]}</td><td>{s[off + 7]}</td><td>{s[off + 8]}</td>
            </tr>
          );
        })}
    </>
  );
}

function pidCell(pid: number): string {
  return `0x${pid.toString(16)} (${pid})`;
}
