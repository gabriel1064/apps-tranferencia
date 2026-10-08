#!/usr/bin/env bash
# Long-lived Niri event listener → layout state. Slot modules watch the file.
set -euo pipefail
SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
exec python3 "${SCRIPT_DIR}/niri-layout.py" events
