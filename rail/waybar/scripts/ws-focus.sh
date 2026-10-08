#!/usr/bin/env bash
# Focus a Hyprland workspace via Lua dispatcher (Hyprland 0.55+).
set -euo pipefail
id="${1:?workspace id required}"
hyprctl dispatch "hl.dsp.focus({ workspace = ${id} })" >/dev/null
