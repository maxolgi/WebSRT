// Wraps the mpeg2ts-wasm pkg. Phase 4: feed SRT-delivered TS message bytes,
// collect demux events, hand them to callbacks.

import init, { TsDemuxer, type TsEvent } from '../wasm/mpeg2ts-wasm/mpeg2ts_wasm.js';

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

  private dispatch(e: TsEvent) {
    // Debug counter (exposed via window.__demuxStats).
    const s = (globalThis as any).__demuxStats ??= { pat: 0, pmt: 0, pes: 0, ra: 0, err: 0, raw: 0 };
    switch (e.kind) {
      case 0: // pat
        s.pat++;
        this.cb.onPat?.(e.program_num, e.pid);
        break;
      case 1: // pmt
        s.pmt++;
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
        s.pes++;
        this.cb.onPes?.(
          e.pid,
          e.pts < 0 ? null : e.pts,
          e.dts < 0 ? null : e.dts,
          e.data,
          e.randomAccess,
        );
        break;
      case 3: // random_access
        s.ra++;
        this.cb.onRandomAccess?.(e.pid);
        break;
      case 4: // error
        s.err++;
        this.cb.onError?.(e.text);
        break;
      default:
        s.raw++;
    }
  }
}
