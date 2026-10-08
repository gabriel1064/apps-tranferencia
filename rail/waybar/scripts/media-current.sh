#!/usr/bin/env bash
# Índice do player atual (persistente)
set -euo pipefail
IDX_FILE="${HOME}/.cache/waybar-media-idx"
DIR="$(cd "$(dirname "$0")" && pwd)"

mapfile -t PLAYERS < <("$DIR/media-players.sh" | grep -v '^$' || true)
n=${#PLAYERS[@]}
if (( n == 0 )); then
  echo ""
  exit 0
fi

idx=0
if [[ -f "$IDX_FILE" ]]; then
  idx=$(cat "$IDX_FILE" 2>/dev/null || echo 0)
fi
# sanitiza
[[ "$idx" =~ ^[0-9]+$ ]] || idx=0
idx=$((idx % n))
echo "$idx" > "$IDX_FILE"
echo "${PLAYERS[$idx]}"
