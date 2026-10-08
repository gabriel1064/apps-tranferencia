#!/usr/bin/env bash
# Resolve capa MPRIS (Zen/Firefox costuma apontar p/ arquivo já apagado)
set -euo pipefail
export PATH="${HOME}/.local/bin:${PATH}"
ROOT="${HOME}/.local/opt/waybar-root"
export LD_LIBRARY_PATH="${ROOT}/usr/lib${LD_LIBRARY_PATH:+:$LD_LIBRARY_PATH}"

try_url() {
  local url="$1"
  [[ -n "$url" ]] || return 1
  if [[ "$url" == file://* ]]; then
    local path="${url#file://}"
    # URL-decode simples de %20
    path="$(printf '%b' "${path//%/\\x}")"
    if [[ -f "$path" ]]; then
      printf '%s\n' "file://${path}"
      return 0
    fi
    return 1
  fi
  # http(s) — deixa o Image carregar
  printf '%s\n' "$url"
  return 0
}

# 1) artUrl do playerctl (todos / específico)
if [[ -n "${1:-}" ]]; then
  u="$(playerctl -p "$1" metadata mpris:artUrl 2>/dev/null || true)"
  try_url "$u" && exit 0
fi

while IFS= read -r p; do
  [[ -z "$p" || "$p" == playerctld ]] && continue
  u="$(playerctl -p "$p" metadata mpris:artUrl 2>/dev/null || true)"
  try_url "$u" && exit 0
done < <(playerctl -l 2>/dev/null || true)

# 2) Fallback: PNG mais recente dos caches comuns do Firefox/Zen
for dir in \
  "${HOME}/.config/zen/firefox-mpris" \
  "${HOME}/.config/firefox/firefox-mpris" \
  "${HOME}/.cache/mozilla/firefox-mpris"
do
  [[ -d "$dir" ]] || continue
  newest="$(ls -t "$dir"/*.png "$dir"/*.jpg "$dir"/*.jpeg 2>/dev/null | head -1 || true)"
  if [[ -n "$newest" && -f "$newest" ]]; then
    printf 'file://%s\n' "$newest"
    exit 0
  fi
done

exit 1
