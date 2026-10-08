#!/usr/bin/env bash
# Toggle Niri Waybar between expanded (apps+strip) and lingueta (tiny tab).
# Uses dual bars in config-niri.jsonc:
#   SIGUSR1 → collapse (hide main, show tab)
#   SIGUSR2 → expand   (show main, hide tab)
set -euo pipefail

STATE_FILE="${HOME}/.cache/waybar-niri/collapsed"
NIRI_CONFIG="${HOME}/.config/niri/config.kdl"
STRUT_EXPANDED=64
STRUT_COLLAPSED=12
mkdir -p "$(dirname "$STATE_FILE")"

set_niri_top_strut() {
  local px="${1:?}"
  python3 - "$NIRI_CONFIG" "$px" <<'PY'
import pathlib, re, sys
path = pathlib.Path(sys.argv[1])
px = sys.argv[2]
text = path.read_text(encoding="utf-8")
new, n = re.subn(
    r"(struts\s*\{[^}]*?\btop\s+)\d+",
    rf"\g<1>{px}",
    text,
    count=1,
    flags=re.S,
)
if n != 1:
    sys.exit(0)
path.write_text(new, encoding="utf-8")
PY
}

pids_for_niri_waybar() {
  # Exact binary via /proc/exe (never match shells that merely mention config-niri).
  local pid exe cmd
  for pid in /proc/[0-9]*; do
    pid="${pid##*/}"
    exe="$(readlink "/proc/$pid/exe" 2>/dev/null || true)"
    case "$exe" in
      */waybar) ;;
      *) continue ;;
    esac
    cmd="$(tr '\0' ' ' <"/proc/$pid/cmdline" 2>/dev/null || true)"
    case "$cmd" in
      *config-niri*)
        printf '%s\n' "$pid"
        ;;
    esac
  done
}

collapse() {
  local p
  mapfile -t pids < <(pids_for_niri_waybar)
  for p in "${pids[@]:-}"; do
    [[ -n "$p" ]] || continue
    kill -SIGUSR1 "$p" 2>/dev/null || true
  done
  echo 1 >"$STATE_FILE"
  set_niri_top_strut "$STRUT_COLLAPSED"
}

expand() {
  local p
  mapfile -t pids < <(pids_for_niri_waybar)
  for p in "${pids[@]:-}"; do
    [[ -n "$p" ]] || continue
    kill -SIGUSR2 "$p" 2>/dev/null || true
  done
  echo 0 >"$STATE_FILE"
  set_niri_top_strut "$STRUT_EXPANDED"
}

toggle() {
  local cur=0
  [[ -f "$STATE_FILE" ]] && cur="$(cat "$STATE_FILE" 2>/dev/null || echo 0)"
  if [[ "$cur" == "1" ]]; then
    expand
  else
    collapse
  fi
}

case "${1:-toggle}" in
  collapse|hide) collapse ;;
  expand|show) expand ;;
  toggle) toggle ;;
  *) echo "usage: $0 [toggle|collapse|expand]" >&2; exit 2 ;;
esac
