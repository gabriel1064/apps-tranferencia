#!/usr/bin/env bash
# Persistent Waybar exec: reprints JSON when layout state changes (no respawn).
set -euo pipefail
SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
IDX="${1:?slot index}"
exec python3 "${SCRIPT_DIR}/niri-layout.py" watch slot "$IDX"
