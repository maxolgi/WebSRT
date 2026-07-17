// GPU info extraction via WEBGL_debug_renderer_info. Some browsers block
// this extension for fingerprinting protection; in that case available is
// false and vendor/renderer are null.

import type { GpuInfo } from './types'

function asString(v: unknown): string | null {
  return typeof v === 'string' ? v : null
}

export function getGpuInfo(): GpuInfo {
  const canvas = document.createElement('canvas')
  const gl = canvas.getContext('webgl')
  if (!gl) {
    return { vendor: null, renderer: null, available: false }
  }

  const dbg = gl.getExtension('WEBGL_debug_renderer_info')
  let info: GpuInfo
  if (dbg) {
    info = {
      vendor: asString(gl.getParameter(dbg.UNMASKED_VENDOR_WEBGL)),
      renderer: asString(gl.getParameter(dbg.UNMASKED_RENDERER_WEBGL)),
      available: true,
    }
  } else {
    info = { vendor: null, renderer: null, available: false }
  }

  const lose = gl.getExtension('WEBGL_lose_context')
  if (lose) lose.loseContext()
  return info
}
