#!/usr/bin/env bash
# Um único texto: ícone + % (pilha horizontal na mesma pill)
set -euo pipefail

out="$(wpctl get-volume @DEFAULT_AUDIO_SINK@ 2>/dev/null || true)"
if [[ -z "$out" ]]; then
  printf '%s\n' '{"text":"󰕾  —%","class":"idle","tooltip":"Volume"}'
  exit 0
fi

vol="$(awk '{print $2}' <<<"$out")"
pct="$(python3 -c "print(int(round(float('$vol')*100)))" 2>/dev/null || echo 0)"

if grep -q MUTED <<<"$out"; then
  icon="󰝟"
  cls="muted"
  tip="Mudo · Esq: painel · Dir: desmutar · Scroll: ±5%"
elif (( pct < 35 )); then
  icon="󰕿"
  cls="low"
  tip="${pct}% · Esq: painel · Dir: mutar · Scroll: ±5%"
elif (( pct < 80 )); then
  icon="󰖀"
  cls="mid"
  tip="${pct}% · Esq: painel · Dir: mutar · Scroll: ±5%"
else
  icon="󰕾"
  cls="high"
  tip="${pct}% · Esq: painel · Dir: mutar · Scroll: ±5%"
fi

printf '%s\n' "{\"text\":\"${icon}  ${pct}%\",\"class\":\"${cls}\",\"tooltip\":\"${tip}\"}"
