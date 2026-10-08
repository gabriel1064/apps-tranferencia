#!/usr/bin/env bash
# Botões ‹ › — só mostram texto se houver 2+ players
set -euo pipefail
DIR="$(cd "$(dirname "$0")" && pwd)"
side="${1:-prev}"  # prev | next

mapfile -t PLAYERS < <("$DIR/media-players.sh" | grep -v '^$' || true)
n=${#PLAYERS[@]}

if (( n < 2 )); then
  printf '%s\n' '{"text":"","class":"hidden"}'
  exit 0
fi

if [[ "$side" == "prev" ]]; then
  printf '%s\n' '{"text":"","class":"visible","tooltip":"Mídia anterior"}'
else
  printf '%s\n' '{"text":"","class":"visible","tooltip":"Próxima mídia"}'
fi
