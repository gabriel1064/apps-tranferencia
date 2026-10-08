#!/usr/bin/env bash
# Focus / reorder helpers for Niri Waybar app slots.
# Usage:
#   niri-app-action.sh focus <slot>
#   niri-app-action.sh move-left <slot>
#   niri-app-action.sh move-right <slot>
set -euo pipefail
ACTION="${1:?action}"
SLOT="${2:?slot}"
STATE="${HOME}/.cache/waybar-niri/state.json"

if [[ ! -f "$STATE" ]]; then
  exit 0
fi

WID="$(python3 - "$SLOT" "$STATE" <<'PY'
import json, sys
idx = int(sys.argv[1])
apps = json.load(open(sys.argv[2], encoding="utf-8")).get("apps") or []
if 0 <= idx < len(apps):
    wid = apps[idx].get("window_id")
    if wid is not None:
        print(wid)
PY
)"

[[ -n "${WID}" ]] || exit 0

case "$ACTION" in
  focus)
    niri msg action focus-window --id "$WID"
    ;;
  move-left)
    niri msg action focus-window --id "$WID"
    niri msg action move-column-left
    ;;
  move-right)
    niri msg action focus-window --id "$WID"
    niri msg action move-column-right
    ;;
  *)
    echo "unknown action: $ACTION" >&2
    exit 2
    ;;
esac
