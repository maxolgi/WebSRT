// A/B snapshot diff: recursively flattens two DebugDiagnostics objects into
// path -> leaf maps and emits a row per path for the Tools tab diff table.

import type { DebugDiagnostics } from './types';

export interface DiffRow {
  path: string;
  a: string;
  b: string;
  changed: boolean;
}

const SKIP_TOP = new Set(['timestamp', 'history', 'consoleErrors']);
const STRING_TRUNC = 60;

function isPlainObject(v: unknown): v is Record<string, unknown> {
  return typeof v === 'object' && v !== null && !Array.isArray(v);
}

function fmt(v: unknown): string {
  if (v === undefined) return '-';
  if (v === null) return 'null';
  if (typeof v === 'string') {
    return v.length > STRING_TRUNC ? v.slice(0, STRING_TRUNC) + '…' : v;
  }
  return String(v);
}

// Display for a non-leaf value whose counterpart is a different kind.
function fmtValue(v: unknown): string {
  if (Array.isArray(v)) return `[len ${v.length}]`;
  if (isPlainObject(v)) return `[obj ${Object.keys(v).length} keys]`;
  return fmt(v);
}

export function diffSnapshots(a: DebugDiagnostics, b: DebugDiagnostics): DiffRow[] {
  const rows: DiffRow[] = [];
  const aObj = a as unknown as Record<string, unknown>;
  const bObj = b as unknown as Record<string, unknown>;
  for (const key of new Set([...Object.keys(aObj), ...Object.keys(bObj)])) {
    if (SKIP_TOP.has(key)) continue;
    walk(aObj[key], bObj[key], key, rows);
  }
  rows.sort((x, y) => (x.path < y.path ? -1 : x.path > y.path ? 1 : 0));
  return rows;
}

function walk(av: unknown, bv: unknown, path: string, rows: DiffRow[]): void {
  if (av === undefined || bv === undefined) {
    const fa = fmtValue(av);
    const fb = fmtValue(bv);
    rows.push({ path, a: fa, b: fb, changed: fa !== fb });
    return;
  }
  if (Array.isArray(av) || Array.isArray(bv)) {
    if (Array.isArray(av) && Array.isArray(bv)) {
      if (av.length !== bv.length) {
        rows.push({ path, a: `[len ${av.length}]`, b: `[len ${bv.length}]`, changed: true });
      } else {
        for (let i = 0; i < av.length; i++) {
          walk(av[i], bv[i], `${path}[${i}]`, rows);
        }
      }
      return;
    }
    const fa = fmtValue(av);
    const fb = fmtValue(bv);
    rows.push({ path, a: fa, b: fb, changed: fa !== fb });
    return;
  }
  if (isPlainObject(av) || isPlainObject(bv)) {
    if (isPlainObject(av) && isPlainObject(bv)) {
      for (const key of new Set([...Object.keys(av), ...Object.keys(bv)])) {
        walk(av[key], bv[key], `${path}.${key}`, rows);
      }
      return;
    }
    const fa = fmtValue(av);
    const fb = fmtValue(bv);
    rows.push({ path, a: fa, b: fb, changed: fa !== fb });
    return;
  }
  const fa = fmt(av);
  const fb = fmt(bv);
  rows.push({ path, a: fa, b: fb, changed: fa !== fb });
}
