// Lazy-loaded debug entry point. Mounts the existing Preact debug panel onto
// a PlayerHandle, wiring the panel's data sources to the handle's event
// surface. Must only be dynamic-import()ed by pages/hosts — never imported by
// web/src/player/* (this module pulls in preact/chart.js/eruda).

import { render, createElement } from 'preact';
import { DebugStore } from './store';
import { DebugPanel } from './components/Panel';
import { startSampler, attachConsoleErrorCapture } from './sampler';
import type { PlayerHandle, PlayerStatsDetail, PlayerErrorDetail } from '../player';

export function mountDebug(
  handle: PlayerHandle,
  container: HTMLElement,
): { destroy(): void } {
  const store = new DebugStore();
  store.latencyMs.value = handle.latencyMs;
  const log = (msg: string, cls = ''): void => store.pushLog(msg, cls);

  render(createElement(DebugPanel, { store }), container);

  const stopSampler = startSampler(store, () => ({
    video: handle.getVideo(),
    audio: handle.getAudio(),
    renderer: handle.getRenderer(),
  }));
  const detachConsole = attachConsoleErrorCapture(store);

  store.testActions.value = {
    resetDecoder: () => {
      handle.getVideo()?.reset();
      log('VideoDecoder reset — will re-sync on next keyframe', 'info');
    },
    reconnect: () => {
      log('Manual reconnect triggered', 'info');
      handle.disconnect();
      setTimeout(() => handle.connect(), 100);
    },
    cycleLatency: () => {
      const current = store.latencyMs.value;
      const next = current >= 2000 ? 120 : current >= 500 ? 2000 : 500;
      store.latencyMs.value = next;
      handle.setLatencyMs(next);
      log(`Latency cycled to ${next}ms`, 'info');
    },
    setHwMode: (mode) => {
      const v = handle.getVideo();
      if (!v) {
        log('Cannot switch hw mode — no active VideoPipeline', 'err');
        return;
      }
      v.setHwMode(mode);
      log(`VideoDecoder hw preference → ${mode} (applies on next feed())`, 'info');
    },
    setDecodePacing: (enabled) => {
      localStorage.setItem('websrt-pacing-decode', enabled ? '1' : '0');
      handle.setDecodePacing(enabled);
      log(`decode pacing → ${enabled ? 'ON' : 'OFF'}`, 'info');
    },
    setRenderPacing: (enabled) => {
      localStorage.setItem('websrt-pacing-render', enabled ? '1' : '0');
      handle.setRenderPacing(enabled);
      log(`render pacing → ${enabled ? 'ON' : 'OFF'}`, 'info');
    },
  };

  const onStats = (e: Event): void => {
    const d = (e as CustomEvent<PlayerStatsDetail>).detail;
    store.srtStats.value = d.stats;
    if (d.demux) store.demuxStats.value = d.demux;
  };
  const onDrift = (e: Event): void => {
    store.driftMs.value = (e as CustomEvent<number>).detail;
  };
  const onError = (e: Event): void => {
    log((e as CustomEvent<PlayerErrorDetail>).detail.message, 'err');
  };

  handle.addEventListener('stats', onStats);
  handle.addEventListener('drift', onDrift);
  handle.addEventListener('error', onError);

  function onceWorkerReady(): void {
    const w = handle.getWorker();
    if (w) {
      w.postMessage({ cmd: 'debug-rate', ms: 250 });
      handle.removeEventListener('stats', onceWorkerReady);
    }
  }
  const w0 = handle.getWorker();
  if (w0) {
    w0.postMessage({ cmd: 'debug-rate', ms: 250 });
  } else {
    handle.addEventListener('stats', onceWorkerReady);
  }

  let destroyed = false;
  return {
    destroy(): void {
      if (destroyed) return;
      destroyed = true;
      stopSampler();
      detachConsole();
      handle.removeEventListener('stats', onStats);
      handle.removeEventListener('drift', onDrift);
      handle.removeEventListener('error', onError);
      handle.removeEventListener('stats', onceWorkerReady);
      handle.getWorker()?.postMessage({ cmd: 'debug-rate', ms: 1000 });
      render(null, container);
    },
  };
}
