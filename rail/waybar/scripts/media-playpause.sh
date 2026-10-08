#!/usr/bin/env bash
# Play/pause no player atual
set -euo pipefail
export PATH="${HOME}/.local/bin:${PATH}"
ROOT="${HOME}/.local/opt/waybar-root"
export LD_LIBRARY_PATH="${ROOT}/usr/lib${LD_LIBRARY_PATH:+:$LD_LIBRARY_PATH}"
DIR="$(cd "$(dirname "$0")" && pwd)"
player="$("$DIR/media-current.sh")"
[[ -n "$player" ]] || exit 0
playerctl -p "$player" play-pause 2>/dev/null || true
pkill -RTMIN+5 waybar 2>/dev/null || true
