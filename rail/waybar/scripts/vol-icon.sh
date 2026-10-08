#!/usr/bin/env bash
# Ícone de volume para mute dedicado (Waybar)
set -euo pipefail

out="$(wpctl get-volume @DEFAULT_AUDIO_SINK@ 2>/dev/null || true)"
if [[ -z "$out" ]]; then
  printf '%s\n' '{"text":"󰕾","class":"idle","tooltip":"Volume"}'
  exit 0
fi

vol="$(awk '{print $2}' <<<"$out")"
pct="$(python3 -c "print(int(round(float('$vol')*100)))" 2>/dev/null || echo 0)"

if grep -q MUTED <<<"$out"; then
  printf '%s\n' '{"text":"󰝟","class":"muted","tooltip":"Clique: desmutar"}'
elif (( pct < 35 )); then
  printf '%s\n' "{\"text\":\"󰕿\",\"class\":\"low\",\"tooltip\":\"Clique: mutar ($pct%)\"}"
elif (( pct < 80 )); then
  printf '%s\n' "{\"text\":\"󰖀\",\"class\":\"mid\",\"tooltip\":\"Clique: mutar ($pct%)\"}"
else
  printf '%s\n' "{\"text\":\"󰕾\",\"class\":\"high\",\"tooltip\":\"Clique: mutar ($pct%)\"}"
fi
