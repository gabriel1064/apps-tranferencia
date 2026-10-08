#!/usr/bin/env bash
# Start Noctalia without the Quickshell reload popup ("…ckshell" strip).
set -euo pipefail
export QS_NO_RELOAD_POPUP=1
export QT_QUICK_CONTROLS_STYLE="${QT_QUICK_CONTROLS_STYLE:-}"
exec noctalia --daemon "$@"
