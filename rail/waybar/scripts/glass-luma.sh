#!/usr/bin/env bash
# Amostra a região e imprime "light" ou "dark" (luminância média).
set -euo pipefail

x="${1:-0}"
y="${2:-0}"
w="${3:-200}"
h="${4:-200}"

# clamp mínimo
(( w < 8 )) && w=8
(( h < 8 )) && h=8

tmp="$(mktemp /tmp/glass-luma-XXXXXX.png)"
cleanup() { rm -f "$tmp"; }
trap cleanup EXIT

if ! grim -g "${x},${y} ${w}x${h}" "$tmp" 2>/dev/null; then
  echo dark
  exit 0
fi

# mean em grayscale 0..1
mean="$(magick "$tmp" -colorspace Gray -format '%[fx:mean]' info: 2>/dev/null || echo 0.3)"
python3 - <<PY
mean = float("${mean}")
print("light" if mean >= 0.52 else "dark")
PY
