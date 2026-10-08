#!/usr/bin/env bash
# Troca player: next | prev
set -euo pipefail
IDX_FILE="${HOME}/.cache/waybar-media-idx"
DIR="$(cd "$(dirname "$0")" && pwd)"
dir="${1:-next}"

mapfile -t PLAYERS < <("$DIR/media-players.sh" | grep -v '^$' || true)
n=${#PLAYERS[@]}
(( n > 0 )) || exit 0

idx=0
if [[ -f "$IDX_FILE" ]]; then
  idx=$(cat "$IDX_FILE" 2>/dev/null || echo 0)
fi
[[ "$idx" =~ ^[0-9]+$ ]] || idx=0

if [[ "$dir" == "prev" ]]; then
  idx=$(( (idx - 1 + n) % n ))
else
  idx=$(( (idx + 1) % n ))
fi
echo "$idx" > "$IDX_FILE"

# Sinaliza a waybar para atualizar o módulo custom
pkill -RTMIN+5 waybar 2>/dev/null || true
