// Diagnostics export: collects all debug data into a JSON object for
// GitHub issue reports. Called from the "Copy Info" and "Download" buttons.

import type { DebugStore } from './store';
import type { DebugDiagnostics, DemuxStats } from './types';

export async function buildDiagnostics(store: DebugStore): Promise<DebugDiagnostics> {
  const nav = navigator as any;
  if (!store.gpuInfo.value) {
    try {
      const { getGpuInfo } = await import('./gpu-info');
      store.gpuInfo.value = getGpuInfo();
    } catch {}
  }
  return {
    timestamp: new Date().toISOString(),
    browser: {
      userAgent: navigator.userAgent,
      platform: navigator.platform,
      language: navigator.language,
      hardwareConcurrency: navigator.hardwareConcurrency,
      deviceMemory: nav.deviceMemory ?? null,
    },
    gpu: store.gpuInfo.value,
    capabilities: store.mediaCaps.value,
    video: store.videoStats.value,
    audio: store.audioStats.value,
    render: store.renderStats.value,
    srt: store.srtStats.value,
    demux: serializeDemux(store.demuxStats.value),
    latencyMs: store.latencyMs.value,
    certMode: store.certMode.value,
    history: store.history.value,
    consoleErrors: store.consoleErrors.value,
  };
}

export async function downloadDiagnostics(store: DebugStore): Promise<void> {
  try {
    const diag = await buildDiagnostics(store);
    const json = JSON.stringify(diag, null, 2);
    const blob = new Blob([json], { type: 'application/json' });
    const url = URL.createObjectURL(blob);
    const a = document.createElement('a');
    a.href = url;
    a.download = `websrt-debug-${formatStamp()}.json`;
    document.body.appendChild(a);
    a.click();
    a.remove();
    URL.revokeObjectURL(url);
  } catch (e) {
    console.error('Failed to download diagnostics:', e);
  }
}

function formatStamp(): string {
  const d = new Date();
  const pad = (n: number) => String(n).padStart(2, '0');
  return `${d.getFullYear()}-${pad(d.getMonth() + 1)}-${pad(d.getDate())}` +
    `-${pad(d.getHours())}${pad(d.getMinutes())}${pad(d.getSeconds())}`;
}

// Typed arrays JSON-serialize as index-keyed objects, so flatten them to plain
// arrays for the diagnostics export. Packet-ring fields are omitted (large and
// covered by the packet-timeline view in the panel itself).
function serializeDemux(d: DemuxStats | null): DebugDiagnostics['demux'] {
  if (!d) return null;
  return {
    programNum: d.programNum,
    pmtPid: d.pmtPid,
    pmtPids: Array.from(d.pmtPids),
    pmtStreamTypes: Array.from(d.pmtStreamTypes),
    pmtFormatIds: d.pmtFormatIds,
    pids: Array.from(d.pids),
    pesCounts: Array.from(d.pesCounts),
    byteTotals: Array.from(d.byteTotals),
    bitratesMbps: Array.from(d.bitratesMbps),
    raCounts: Array.from(d.raCounts),
    lastPts: Array.from(d.lastPts),
    lastDts: Array.from(d.lastDts),
    ptsJumps: Array.from(d.ptsJumps),
    ccErrors: Array.from(d.ccErrors),
    teiCounts: Array.from(d.teiCounts),
    pusiCounts: Array.from(d.pusiCounts),
    scramblingCounts: Array.from(d.scramblingCounts),
    afControlCounts: Array.from(d.afControlCounts),
    pcrPids: Array.from(d.pcrPids),
    pcrIntervalsMs: Array.from(d.pcrIntervalsMs),
    pcrJitterMs: Array.from(d.pcrJitterMs),
    nalPids: Array.from(d.nalPids),
    nalStats: Array.from(d.nalStats),
    errorT: Array.from(d.errorT),
    errorMsg: d.errorMsg,
  };
}
