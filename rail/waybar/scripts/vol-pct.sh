#!/usr/bin/env bash
# Porcentagem de volume
set -euo pipefail

out="$(wpctl get-volume @DEFAULT_AUDIO_SINK@ 2>/dev/null || true)"
if [[ -z "$out" ]]; then
  printf '%s\n' '{"text":"—%","class":"idle","tooltip":"Volume"}'
  exit 0
fi

vol="$(awk '{print $2}' <<<"$out")"
pct="$(python3 -c "print(int(round(float('$vol')*100)))" 2>/dev/null || echo 0)"
cls="ok"
grep -q MUTED <<<"$out" && cls="muted"

printf '%s\n' "{\"text\":\"${pct}%\",\"class\":\"${cls}\",\"tooltip\":\"Clique: painel · Scroll: ±5%\"}"
