#!/usr/bin/env bash
# Abre Mission Center (monitor de desempenho)
set -euo pipefail

for cmd in missioncenter mission-center; do
  if command -v "$cmd" >/dev/null 2>&1; then
    exec /home/gabriel/.local/bin/uwsm-launch.sh "$cmd"
  fi
done

notify-send "Desempenho" \
  "Mission Center não instalado.\nRode: ~/.local/bin/install-mission-center.sh" \
  2>/dev/null || true
exit 1
