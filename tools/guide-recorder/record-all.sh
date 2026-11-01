#!/bin/bash
# Records every how-to guide against a local `wrangler dev` Worker started with fresh state and
# SPOT_ALLOW_MOCK_AUTH=1, then writes docs/images/<guide>/<guide>.gif. The step screenshots are
# written while the scenes run. Needs node with playwright, ffmpeg, gifsicle, and python3 with Pillow.
# Run it from a fresh local state: the scenes build on each other in this order.
set -euo pipefail
here="$(cd "$(dirname "$0")" && pwd)"
images="$here/../../docs/images"
work="${RECORD_DIR:-${TMPDIR:-/tmp}/guide-recorder}"
base="${LOCAL_WORKER:-http://127.0.0.1:8787}"
curl -sf "$base/api/health" >/dev/null || { echo "No local Worker at $base" >&2; exit 1; }
scenes=(
  g_signin:sign-in g_cheap:find-cheap-hosts g_launch:launch-a-pool g_workflow:use-a-pool-in-a-workflow
  g_template:make-a-runner-template g_change_tpl:change-a-pool-template g_check:check-a-pool
  g_resize:resize-or-remove-a-pool g_admins:add-an-administrator
  g_tour:tour-the-dashboard g_builder:launch-a-builder g_map:explore-the-fleet-map
  g_instances:review-instances-and-cost g_regions:choose-regions g_scaling:set-autoscaling
  g_observability:read-observability-and-logs g_patches:edit-a-runner-patch g_rust:track-rust-builds
  g_vmtpl:save-a-vm-template
)
for pair in "${scenes[@]}"; do
  scene="${pair%%:*}"; guide="${pair##*:}"
  echo "== $guide"
  node "$here/$scene.js"
  list="$work/out/$guide.txt"
  gif="$images/$guide/$guide.gif"
  # Palette from the frames that change, no dithering, so unchanged areas stay identical.
  ffmpeg -v error -y -f concat -safe 0 -i "$list" \
    -vf "fps=8,scale=960:-1:flags=lanczos,palettegen=max_colors=64:stats_mode=diff" "$work/palette.png"
  ffmpeg -v error -y -f concat -safe 0 -i "$list" -i "$work/palette.png" \
    -lavfi "fps=8,scale=960:-1:flags=lanczos [x]; [x][1:v] paletteuse=dither=none:diff_mode=rectangle" "$gif"
  gifsicle -O3 --lossy=40 "$gif" -o "$gif.tmp" && mv "$gif.tmp" "$gif"
done
# Step screenshots: 256-color palettes keep them small without visible loss on the dark theme.
python3 - "$images" "${scenes[@]}" <<'PY'
import glob, sys
from PIL import Image
images = sys.argv[1]
for pair in sys.argv[2:]:
    for f in glob.glob(f"{images}/{pair.split(':')[1]}/*.png"):
        Image.open(f).convert("RGB").quantize(256, method=Image.Quantize.MEDIANCUT, dither=Image.Dither.NONE).save(f, optimize=True)
PY
du -ch "$images"/*/*.gif | tail -1
