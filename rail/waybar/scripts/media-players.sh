#!/usr/bin/env bash
# Lista players MPRIS (exclui playerctld se aparecer)
set -euo pipefail
export PATH="${HOME}/.local/bin:${PATH}"
ROOT="${HOME}/.local/opt/waybar-root"
export LD_LIBRARY_PATH="${ROOT}/usr/lib${LD_LIBRARY_PATH:+:$LD_LIBRARY_PATH}"

mapfile -t PLAYERS < <(playerctl -l 2>/dev/null | grep -v '^playerctld$' || true)
printf '%s\n' "${PLAYERS[@]:-}"
