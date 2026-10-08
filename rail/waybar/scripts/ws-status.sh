#!/usr/bin/env bash
# Usage: ws-status.sh <id>
# Emits Waybar JSON for a single Hyprland workspace button.
# Hidden when the workspace does not exist / has no windows (unless active).
set -euo pipefail

id="${1:?workspace id required}"

# Arabic → Roman for the pill label (1..10).
roman_of() {
  case "$1" in
    1) echo I ;;
    2) echo II ;;
    3) echo III ;;
    4) echo IV ;;
    5) echo V ;;
    6) echo VI ;;
    7) echo VII ;;
    8) echo VIII ;;
    9) echo IX ;;
    10) echo X ;;
    *) echo "$1" ;;
  esac
}

label="$(roman_of "$id")"

read -r active windows < <(python3 - "$id" <<'PY'
import json, subprocess, sys
wid = int(sys.argv[1])
active = json.loads(subprocess.check_output(["hyprctl", "-j", "activeworkspace"]))
workspaces = json.loads(subprocess.check_output(["hyprctl", "-j", "workspaces"]))
ws = next((w for w in workspaces if w.get("id") == wid), None)
print(active.get("id", 0), (ws or {}).get("windows", 0) if ws else -1)
PY
)

if [[ "$active" == "$id" ]]; then
  printf '{"text":"%s","class":"active","tooltip":"Workspace %s"}\n' "$label" "$label"
elif [[ "$windows" -gt 0 ]]; then
  printf '{"text":"%s","class":"occupied","tooltip":"Workspace %s"}\n' "$label" "$label"
else
  # empty / nonexistent → hide in the bar
  printf '{"text":"","class":"hidden","tooltip":""}\n'
fi
