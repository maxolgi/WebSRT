#!/bin/bash 
# Phase 0 PCM spike source — pushes SMPTE 302M (AES3) audio over SRT to the
# running WebSRT gateway. Audio-only MPEG-2 TS, no video.
#
# Usage: ./fixtures/stream_pcm.sh [mono|stereo|surround|<channel_count>]
#   mono (default): 440 Hz sine, single stereo PID (L=R).
#   stereo:         two-tone (440/660 Hz) sine pair, single stereo PID.
#   surround:       6-channel sine bed (L/R/C/LFE/Ls/Rs), single 5.1 PID.
#   <number>:       N-channel mode — generates ceil(N/2) stereo s302m PIDs,
#                   each at a different frequency. Example: 128 → 64 PIDs.
#
# Override target via WEBSTRT_SRT_URL (default srt://127.0.0.1:9000).
# Use streamid: WEBSTRT_SRT_URL="srt://127.0.0.1:9000?streamid=audio"
# Ctrl+C to stop.

set -euo pipefail

MODE="${1:-mono}"
SRT_URL="${WEBSTRT_SRT_URL:-srt://127.0.0.1:9000}"

if ! ffmpeg -hide_banner -encoders 2>/dev/null | grep -qw s302m; then
  echo "error: this ffmpeg build does not include the s302m encoder (SMPTE 302M)." >&2
  echo "       s302m is GPL-only; install an ffmpeg built with --enable-gpl." >&2
  exit 1
fi

case "$MODE" in
  ''|[0-9]*)
    CH="${MODE:-2}"
    if [ "$CH" -lt 2 ]; then CH=2; fi
    PIDS=$(( (CH + 1) / 2 ))
    FFMPEG_INPUTS=()
    FFMPEG_MAPS=()
    FFMPEG_META=()
    for i in $(seq 0 $((PIDS - 1))); do
      FREQ=$(( 100 + i * 50 ))
      FFMPEG_INPUTS+=( -f lavfi -i "sine=frequency=${FREQ}:duration=86400" )
      FFMPEG_MAPS+=( -map "${i}:a" )
      FFMPEG_META+=( -metadata:"s:a:${i}" "language=ch${i}" )
    done
    echo "PCM: ${CH} channels = ${PIDS} stereo s302m PIDs" >&2
    exec ffmpeg -re \
      "${FFMPEG_INPUTS[@]}" \
      "${FFMPEG_MAPS[@]}" \
      -c:a s302m -ac 2 -ar 48000 -sample_fmt s32 -strict -2 \
      "${FFMPEG_META[@]}" \
      -f mpegts "$SRT_URL"
    ;;
  mono)
    exec ffmpeg -re \
      -f lavfi -i sine=frequency=440:duration=86400 \
      -ac 2 -ar 48000 \
      -c:a s302m -sample_fmt s32 -strict -2 \
      -metadata:s:a:0 language=eng \
      -f mpegts "$SRT_URL"
    ;;
  stereo)
    exec ffmpeg -re \
      -f lavfi -i sine=frequency=440:duration=86400 \
      -f lavfi -i sine=frequency=660:duration=86400 \
      -filter_complex '[0][1]amerge=inputs=2' \
      -ac 2 -ar 48000 \
      -c:a s302m -sample_fmt s32 -strict -2 \
      -metadata:s:a:0 language=eng \
      -f mpegts "$SRT_URL"
    ;;
  surround)
    exec ffmpeg -re \
      -f lavfi -i sine=frequency=440:duration=86400 \
      -f lavfi -i sine=frequency=550:duration=86400 \
      -f lavfi -i sine=frequency=660:duration=86400 \
      -f lavfi -i sine=frequency=82:duration=86400 \
      -f lavfi -i sine=frequency=880:duration=86400 \
      -f lavfi -i sine=frequency=990:duration=86400 \
      -filter_complex '[0][1][2][3][4][5]amerge=inputs=6' \
      -ac 6 -channel_layout 5.1 -ar 48000 \
      -c:a s302m -sample_fmt s32 -strict -2 \
      -metadata:s:a:0 language=eng \
      -f mpegts "$SRT_URL"
    ;;
  *)
    echo "usage: $0 [mono|stereo|surround|<channel_count>]" >&2
    echo "  channel_count: any even number (2, 4, 6, 8, ..., 128)" >&2
    exit 1
    ;;
esac
