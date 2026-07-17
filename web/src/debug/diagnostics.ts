// Diagnostics export: collects all debug data into a JSON object for
// GitHub issue reports. Called from the "Copy Info" and "Download" buttons.

import type { DebugStore } from './store';
import type { DebugDiagnostics } from './types';

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
    demux: store.demuxStats.value,
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
