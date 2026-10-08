#!/usr/bin/env bash
# Cria/foca a próxima área de trabalho: max(id com janelas) + 1.
set -euo pipefail

next=$(
  python3 <<'PY'
import json
import subprocess

workspaces = json.loads(subprocess.check_output(["hyprctl", "-j", "workspaces"]))
active = json.loads(subprocess.check_output(["hyprctl", "-j", "activeworkspace"]))

open_ids = [w["id"] for w in workspaces if w.get("windows", 0) > 0]
if open_ids:
    n = max(open_ids) + 1
else:
    n = int(active.get("id", 1)) + 1

print(max(1, min(n, 10)))
PY
)

exec "$(dirname "$0")/ws-focus.sh" "$next"
