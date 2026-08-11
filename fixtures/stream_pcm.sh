#!/usr/bin/env bash
# Phase 0 PCM spike source — pushes SMPTE 302M (AES3) audio over SRT to the
# running WebSRT gateway. Audio-only MPEG-2 TS, no video.
#
# Usage: ./fixtures/stream_pcm.sh [mono|stereo|surround]
#   mono (default): 440 Hz sine, single audio PID.
#   stereo:         two-tone (440/660 Hz) sine pair, single audio PID.
#   surround:       6-channel sine bed (L/R/C/LFE/Ls/Rs tones), single audio PID.
#
# NOTE on "mono": the SMPTE 302M / AES3 frame is inherently a stereo pair
# (2 subframes per frame). ffmpeg's s302m encoder rejects 1-channel input
# outright ("Only 2, 4, 6 and 8 channels are supported"). The conventional way
# to carry a mono source over AES3 is a stereo pair with identical subframes,
# so `mono` emits 2 channels of the same 440 Hz tone. The Phase 1 demuxer can
# report channelCount=1 by detecting identical subframes, or honestly report
# 2 — either way the browser hears a single tone.
#
# Produces SMPTE 302M (AES3 in MPEG-2 TS) per audioplan.md Phase 0 design.
# The gateway is already a byte-for-byte pass-through, so this exercises the
# full ffmpeg → gateway → browser PCM path with zero codec work.
#
# Sample rate is fixed at 48000 Hz with -sample_fmt s32 (s302m packs 24-bit
# audio into 32-bit words — "24-in-32"). The mpegts muxer stamps PTS on PES
# packets by default (audioplan.md line 82 confirms ffmpeg populates ptsMs
# correctly for s302m), so no explicit timestamp flag is needed — note that
# `-mpegts_flags +system_time` is NOT a valid flag in this muxer.
# -strict -2 is required because s302m is flagged experimental.
# -metadata:s:a:0 language=eng writes the MPEG-2 audio-component language_code
# descriptor that Phase 1 will surface in the pidmap event.
#
# Override target via WEBSTRT_SRT_URL.
# Ctrl+C to stop.
#
# See audioplan.md (Phase 0 — Spike) for the end-to-end contract.

set -euo pipefail

MODE="${1:-mono}"
SRT_URL="${WEBSTRT_SRT_URL:-srt://127.0.0.1:9000}"

# s302m is a GPL encoder — not every ffmpeg build ships it.
if ! ffmpeg -hide_banner -encoders 2>/dev/null | grep -qw s302m; then
  echo "error: this ffmpeg build does not include the s302m encoder (SMPTE 302M)." >&2
  echo "       s302m is GPL-only; install an ffmpeg built with --enable-gpl." >&2
  exit 1
fi

case "$MODE" in
  mono)
    # Single sine source, single audio PID. s302m cannot emit 1 channel, so
    # we duplicate the mono sine into a 2-channel AES3 pair (L=R=440 Hz) —
    # the standard way to carry mono over AES3. See header note.
    exec ffmpeg -re \
      -f lavfi -i sine=frequency=440:duration=86400 \
      -ac 2 -ar 48000 \
      -c:a s302m -sample_fmt s32 -strict -2 \
      -metadata:s:a:0 language=eng \
      -f mpegts "$SRT_URL"
    ;;
  stereo)
    # Two-tone sine pair (440/660 Hz), amerged into a single stereo PID.
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
    # 6-channel sine bed (L/R/C/LFE/Ls/Rs tones), single 5.1 PID. Mirrors the
    # stereo amerge pattern with 6 sine inputs (LFE uses 82 Hz to stay musical).
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
    echo "usage: $0 [mono|stereo|surround]" >&2
    exit 1
    ;;
esac
