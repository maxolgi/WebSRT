// Wraps the mpeg2ts-wasm pkg. Phase 4: feed SRT-delivered TS message bytes,
// collect demux events, hand them to callbacks.

import init, { TsDemuxer, type TsEvent, type DebugSnapshot } from '../wasm/mpeg2ts-wasm/mpeg2ts_wasm.js';

let initPromise: Promise<unknown> | null = null;
export async function ensureMpeg2tsWasm(): Promise<void> {
  if (!initPromise) initPromise = init();
  await initPromise;
}

export interface DemuxCallbacks {
  onPat?: (programNum: number, pmtPid: number) => void;
  onPmt?: (entries: { pid: number; streamType: number; formatId: string | null }[]) => void;
  onPes?: (pid: number, pts: number | null, dts: number | null, bytes: Uint8Array, randomAccess: boolean) => void;
  onRandomAccess?: (pid: number) => void;
  onError?: (msg: string) => void;
}

export class Demuxer {
  private demux: TsDemuxer;
  private cb: DemuxCallbacks;

  private constructor(demux: TsDemuxer, cb: DemuxCallbacks) {
    this.demux = demux;
    this.cb = cb;
  }

  static async create(cb: DemuxCallbacks): Promise<Demuxer> {
    await ensureMpeg2tsWasm();
    return new Demuxer(new TsDemuxer(), cb);
  }

  /** Feed SRT-delivered TS message bytes. */
  feed(bytes: Uint8Array) {
    const events = this.demux.feed(bytes);
    for (const e of events) this.dispatch(e);
  }

  /**
   * Snapshot the demuxer's full analysis state for the debug panel.
   * Each typed array is a fresh JS-owned copy (WASM getter `.slice()`s), so
   * the snapshot struct can be freed immediately after reading. Owned by JS;
   * cheap to call every ~250ms.
   */
  debugSnapshot(): DebugSnapshot {
    return this.demux.debugSnapshot();
  }

  private dispatch(e: TsEvent) {
    switch (e.kind) {
      case 0: // pat
        this.cb.onPat?.(e.program_num, e.pid);
        break;
      case 1: // pmt
        {
          const flat = e.pmtEntries();
          const formatIds = e.pmtFormatIds();
          const entries: { pid: number; streamType: number; formatId: string | null }[] = [];
          for (let i = 0; i + 1 < flat.length; i += 2) {
            const fmt = formatIds[i / 2];
            entries.push({
              pid: flat[i],
              streamType: flat[i + 1],
              formatId: fmt && fmt.length > 0 ? fmt : null,
            });
          }
          this.cb.onPmt?.(entries);
        }
        break;
      case 2: // pes
        this.cb.onPes?.(
          e.pid,
          e.pts < 0 ? null : e.pts,
          e.dts < 0 ? null : e.dts,
          e.data,
          e.randomAccess,
        );
        break;
      case 3: // random_access
        this.cb.onRandomAccess?.(e.pid);
        break;
      case 4: // error
        this.cb.onError?.(e.text);
        break;
    }
  }
}
