#!/usr/bin/env bash
# Avançar/voltar: Spotify/mpv via playerctl; YouTube no browser via Shift+N/P (Hyprland).
#
# YouTube no Firefox/Zen só aceita o atalho com a página “armada” (foco interno).
# Sem focar, o 1º skip às vezes vai e o 2º morre até clicar na janela.
# Aqui: foca o browser, clica pra rearmar o player, manda o atalho e devolve o foco.
# Animações de workspace desligam nesse instante pra quase não piscar a tela.
set -euo pipefail
export PATH="${HOME}/.local/bin:${PATH}"

dir="${1:-next}" # next | prev
player="${2:-}"
key="n"
[[ "$dir" == "prev" || "$dir" == "previous" ]] && key="p"

if [[ -z "$player" ]]; then
  player="$(playerctl -l 2>/dev/null | grep -v '^playerctld$' | head -1 || true)"
fi

pl="${player,,}"
is_browser=0
if [[ "$pl" == *firefox* || "$pl" == *zen* || "$pl" == *chromium* || "$pl" == *chrome* \
   || "$pl" == *brave* || "$pl" == *vivaldi* || "$pl" == *librewolf* ]]; then
  is_browser=1
fi

hy_eval() {
  hyprctl eval "$1" >/dev/null 2>&1 || true
}

ws_anim() {
  local enabled="$1" # true|false
  if [[ "$enabled" == "false" ]]; then
    hy_eval 'return hl.animation({ leaf = "workspaces", enabled = false })'
    hy_eval 'return hl.animation({ leaf = "workspacesIn", enabled = false })'
    hy_eval 'return hl.animation({ leaf = "workspacesOut", enabled = false })'
  else
    hy_eval 'return hl.animation({ leaf = "workspaces", enabled = true, speed = 5, bezier = "workspaceEase", style = "slide" })'
    hy_eval 'return hl.animation({ leaf = "workspacesIn", enabled = true, speed = 5, bezier = "workspaceEase", style = "slide" })'
    hy_eval 'return hl.animation({ leaf = "workspacesOut", enabled = true, speed = 5, bezier = "workspaceEase", style = "slide" })'
  fi
}

send_yt() {
  local cls="$1"
  local prev_addr prev_cls url_before url_after
  prev_addr="$(hyprctl activewindow -j 2>/dev/null | python3 -c "import json,sys; print(json.load(sys.stdin).get('address') or '')" 2>/dev/null || true)"
  prev_cls="$(hyprctl activewindow -j 2>/dev/null | python3 -c "import json,sys; print(json.load(sys.stdin).get('class') or '')" 2>/dev/null || true)"

  url_before=""
  if [[ -n "${player:-}" ]]; then
    url_before="$(playerctl -p "$player" metadata xesam:url 2>/dev/null || true)"
  fi

  _yt_once() {
    hy_eval "return hl.dispatch(hl.dsp.focus({ window = \"class:${cls}\" }))"
    sleep 0.08
    # Clique esquerdo: rearma o foco do player no YouTube (senão só o 1º Shift+N funciona)
    hy_eval "return hl.dispatch(hl.dsp.send_shortcut({ mods = \"\", key = \"mouse:272\", window = \"class:${cls}\" }))"
    sleep 0.06
    hy_eval "return hl.dispatch(hl.dsp.send_shortcut({ mods = \"SHIFT\", key = \"${key}\", window = \"class:${cls}\" }))"
    sleep 0.06
  }

  ws_anim false
  _yt_once

  # Se o vídeo ainda estava carregando, tenta de novo uma vez
  if [[ -n "$url_before" ]]; then
    sleep 0.35
    url_after="$(playerctl -p "$player" metadata xesam:url 2>/dev/null || true)"
    if [[ -n "$url_after" && "$url_after" == "$url_before" ]]; then
      _yt_once
    fi
  fi

  if [[ -n "$prev_addr" && "$prev_cls" != "$cls" ]]; then
    hy_eval "return hl.dispatch(hl.dsp.focus({ window = \"address:${prev_addr}\" }))"
  fi

  ws_anim true
}

if (( is_browser == 1 )); then
  cls="$(hyprctl clients -j 2>/dev/null | python3 -c "
import json, sys
want = ('zen', 'firefox', 'chromium', 'brave', 'chrome', 'vivaldi', 'librewolf')
clients = json.load(sys.stdin)
for c in clients:
    cl = (c.get('class') or '').lower()
    title = (c.get('title') or '').lower()
    if any(w in cl for w in want) and ('youtube' in title or 'youtu.be' in title):
        print(c.get('class') or '')
        raise SystemExit
for c in clients:
    cl = (c.get('class') or '').lower()
    if any(w in cl for w in want):
        print(c.get('class') or '')
        break
" 2>/dev/null || true)"

  if [[ -n "${cls:-}" ]]; then
    send_yt "$cls"
    exit 0
  fi
  for c in zen firefox firefox-esr librewolf chromium brave-browser brave google-chrome; do
    send_yt "$c"
  done
  exit 0
fi

action="next"
[[ "$key" == "p" ]] && action="previous"
if [[ -n "$player" ]]; then
  playerctl -p "$player" "$action" 2>/dev/null || true
else
  playerctl "$action" 2>/dev/null || true
fi
