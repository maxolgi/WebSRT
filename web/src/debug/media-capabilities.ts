// MediaCapabilities probing: queries navigator.mediaCapabilities for
// decoding support of given codec configurations. Feeds the Codec tab.

import type { MediaCapResult } from './types'

const DEFAULT_BITRATE = 5_000_000

type HwAccel = 'no-preference' | 'prefer-hardware' | 'prefer-software'

interface ProbeConfig {
  codec: string
  width: number
  height: number
  framerate: number
  bitrate?: number
  hwAcceleration?: HwAccel
}

const MATRIX_CODECS = [
  'avc1.42E01E',
  'avc1.4D401F',
  'avc1.640028',
  'hev1.1.6.L120.B0',
  'hev1.2.4.L120.B0',
  'vp09.00.10.08',
  'av01.0.04M.08',
]

const MATRIX_RESOLUTIONS = [
  { width: 1920, height: 1080, framerate: 30, bitrate: 5_000_000 },
  { width: 1280, height: 720, framerate: 30, bitrate: 2_500_000 },
]

export async function probeCodec(config: ProbeConfig): Promise<MediaCapResult> {
  const bitrate = config.bitrate ?? DEFAULT_BITRATE
  const videoConfig: VideoConfiguration = {
    contentType: config.codec,
    width: config.width,
    height: config.height,
    bitrate,
    framerate: config.framerate,
  }
  const result = await navigator.mediaCapabilities.decodingInfo({
    type: 'file',
    video: videoConfig,
  })
  return {
    codec: config.codec,
    width: config.width,
    height: config.height,
    framerate: config.framerate,
    bitrate,
    supported: result.supported,
    powerEfficient: result.powerEfficient,
    smooth: result.smooth,
    hwAcceleration: config.hwAcceleration,
  }
}

export async function probeMatrix(
  currentCodec?: string,
  width?: number,
  height?: number,
): Promise<MediaCapResult[]> {
  const configs: ProbeConfig[] = []

  for (const codec of MATRIX_CODECS) {
    for (const res of MATRIX_RESOLUTIONS) {
      configs.push({
        codec,
        width: res.width,
        height: res.height,
        framerate: res.framerate,
        bitrate: res.bitrate,
        hwAcceleration: 'no-preference',
      })
    }
  }

  if (currentCodec && width && height) {
    configs.push({
      codec: currentCodec,
      width,
      height,
      framerate: 30,
      bitrate: DEFAULT_BITRATE,
      hwAcceleration: 'no-preference',
    })
  }

  const results = await Promise.all(configs.map((c) => probeCodec(c)))
  results.sort((a, b) =>
    a.codec === b.codec ? b.height - a.height : a.codec < b.codec ? -1 : 1,
  )
  return results
}
