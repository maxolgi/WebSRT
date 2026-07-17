// Diagnostics export: collects all debug data into a JSON object for
// GitHub issue reports. Called from the "Copy Info" button.

import type { DebugStore } from './store';
import type { DebugDiagnostics } from './types';

export async function buildDiagnostics(store: DebugStore): Promise<DebugDiagnostics> {
  const nav = navigator as any;
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
