#!/usr/bin/env bash
# Live test publisher — streams to the running WebSRT gateway (SRT 9000).
#
# Usage: ./fixtures/stream.sh [h264|av1]
#   h264 (default): H.264 via NVENC (mirrors a typical OBS setup on this box).
#   av1:            AV1 via VAAPI hardware encode. ffmpeg muxes AV1 as
#                   stream_type 0x06 with no descriptor, so the viewer takes
#                   the content-probe fallback path (looksLikeAv1).
#
# Override target via WEBSTRT_SRT_URL; VAAPI render node via WEBSTRT_VAAPI_NODE.
# Ctrl+C to stop.

set -euo pipefail

CODEC="${1:-h264}"
SRT_URL="${WEBSTRT_SRT_URL:-srt://127.0.0.1:9000}"
RENDER_NODE="${WEBSTRT_VAAPI_NODE:-/dev/dri/renderD128}"

case "$CODEC" in
  h264)
    exec ffmpeg -re \
      -f lavfi -i testsrc=size=1280x720:rate=30 \
      -f lavfi -i sine=frequency=440:duration=86400 \
      -c:v h264_nvenc -b:v 4M -g 60 -bf 0 -pix_fmt yuv420p \
      -c:a libopus -b:a 64k \
      -f mpegts "$SRT_URL"
    ;;
  av1)
    exec ffmpeg -re \
      -f lavfi -i testsrc=size=1280x720:rate=30 \
      -f lavfi -i sine=frequency=440:duration=86400 \
      -init_hw_device vaapi=Intel:"$RENDER_NODE" -filter_hw_device Intel \
      -vf 'format=nv12,hwupload' \
      -c:v av1_vaapi -b:v 4M -g 60 \
      -c:a libopus -b:a 64k \
      -f mpegts "$SRT_URL"
    ;;
  *)
    echo "usage: $0 [h264|av1]" >&2
    exit 1
    ;;
esac
