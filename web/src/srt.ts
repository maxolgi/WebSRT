// Wraps the srt-wasm pkg and drives SrtReceiver over WebTransport datagrams.
// Phase 3: handshake-only. No data plane yet.

import init, { SrtReceiver, type SrtAction, type SrtStats } from '../wasm/srt-wasm/srt_wasm.js';

let initPromise: Promise<unknown> | null = null;
export async function ensureSrtWasm(): Promise<void> {
  if (!initPromise) initPromise = init();
  await initPromise;
}

export interface SrtControllerCallbacks {
  onLog: (msg: string, cls?: string) => void;
  onHandshakeComplete: () => void;
  onDeliverMessage: (bytes: Uint8Array) => void;
  onClose: () => void;
}

export class SrtController {
  private rx: SrtReceiver;
  private wt: WebTransport;
  private cb: SrtControllerCallbacks;
  private pollTimer: number | null = null;
  private epoch = performance.now();
  private datagramWriter: WritableStreamDefaultWriter<Uint8Array> | null = null;

  private constructor(rx: SrtReceiver, wt: WebTransport, cb: SrtControllerCallbacks) {
    this.rx = rx;
    this.wt = wt;
    this.cb = cb;
    this.datagramWriter = wt.datagrams.writable.getWriter();
  }

  static async start(wt: WebTransport, latencyMs: number, cb: SrtControllerCallbacks): Promise<SrtController> {
    await ensureSrtWasm();
    const rx = SrtReceiver.newWithLatency(latencyMs);
    const ctrl = new SrtController(rx, wt, cb);
    ctrl.startLoops();
    return ctrl;
  }

  private startLoops() {
    // Drain WT datagrams → feed into the receiver.
    (async () => {
      const reader = this.wt.datagrams.readable.getReader();
      for (;;) {
        const { value, done } = await reader.read();
        if (done) {
          this.cb.onLog('datagram reader done', 'info');
          return;
        }
        const nowUs = (performance.now() - this.epoch) * 1000;
        const actions = this.rx.handle_datagram(value, nowUs);
        for (const a of actions) this.apply(a);
      }
    })();

    // Periodic poll. ~10ms cadence per the plan.
    const poll = () => {
      const nowUs = (performance.now() - this.epoch) * 1000;
      const actions = this.rx.poll(nowUs);
      for (const a of actions) this.apply(a);
      this.pollTimer = window.setTimeout(poll, 10);
    };
    this.pollTimer = window.setTimeout(poll, 10);
  }

  private apply(a: SrtAction) {
    switch (a.kind) {
      case 0: { // SendDatagram
        this.datagramWriter?.write(a.data);
        break;
      }
      case 1: { // DeliverMessage
        this.cb.onDeliverMessage(a.data);
        break;
      }
      case 2: { // HandshakeComplete
        this.cb.onLog('SRT handshake complete ✓', 'ok');
        this.cb.onHandshakeComplete();
        break;
      }
      case 3: { // WaitForData
        // no-op — poll loop will run anyway
        break;
      }
      case 4: { // Close
        this.cb.onLog('SRT closed', 'err');
        this.cb.onClose();
        break;
      }
      case 5: { // Log
        this.cb.onLog(`srt: ${a.text}`, 'info');
        break;
      }
    }
  }

  stop() {
    if (this.pollTimer != null) window.clearTimeout(this.pollTimer);
    this.pollTimer = null;
  }

  getStats(): SrtStats | null {
    return this.rx?.getStats() ?? null;
  }
}
