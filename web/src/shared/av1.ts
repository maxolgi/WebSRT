// AV1 OBU (Open Bitstream Unit) helper. Shared between the worker
// (probe unknown 0x06 PIDs to disambiguate AV1 vs Opus) and the decode
// pipeline (fallback codec sniff when no PMT hint is available).

/**
 * Content-probe: does this PES payload begin with a valid low-overhead
 * AV1 OBU? Validates the first OBU header (forbidden=0, reserved=0,
 * type ∈ {SH=1, TD=2, Frame=6}, has_size=1) and that its LEB128 size
 * fits the payload. Opus TOC bytes won't pass.
 */
export function looksLikeAv1(payload: Uint8Array): boolean {
  if (payload.length < 2) return false;
  const b = payload[0];
  if ((b & 0x80) !== 0) return false; // forbidden bit
  if ((b & 0x01) !== 0) return false; // reserved bit
  const type = (b >> 3) & 0x0f;
  if (type !== 1 && type !== 2 && type !== 6) return false;
  const hasSize = (b >> 1) & 0x01;
  if (hasSize === 0) return false; // low-overhead OBUs always carry size
  const extFlag = (b >> 2) & 0x01;
  let p = 1 + extFlag;
  let size = 0;
  let shift = 0;
  while (p < payload.length) {
    const byte = payload[p++];
    size |= (byte & 0x7f) << shift;
    if ((byte & 0x80) === 0) break;
    shift += 7;
    if (shift > 28) return false;
  }
  return p + size <= payload.length;
}
