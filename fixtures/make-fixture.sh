#!/usr/bin/env bash
# Generate the test fixture. ~350 KB, committed.
#
# H.264 baseline 3.0 (max WebCodecs compatibility), 30fps keyframe-every-1s,
# Opus audio. Duration 10 seconds.

set -euo pipefail

DIR="$(cd "$(dirname "$0")/.." && pwd)"
OUT="$DIR/fixtures/test.ts"

if [[ -f "$OUT" ]] && [[ "${1:-}" != "--force" ]]; then
  echo "fixture already exists: $OUT ($(stat -c%s "$OUT") bytes)"
  echo "use '$0 --force' to regenerate"
  exit 0
fi

ffmpeg -y \
  -f lavfi -i testsrc=duration=10:size=640x360:rate=30 \
  -f lavfi -i sine=frequency=440:duration=10 \
  -c:v libx264 -profile:v baseline -level 3.0 -pix_fmt yuv420p -g 30 -bf 0 \
  -c:a libopus -b:a 64k \
  -f mpegts "$OUT"

echo "wrote $OUT ($(stat -c%s "$OUT") bytes)"
