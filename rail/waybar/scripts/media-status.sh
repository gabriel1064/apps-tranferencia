#!/usr/bin/env bash
# Status JSON para o módulo de mídia (qualquer player MPRIS)
set -euo pipefail
export PATH="${HOME}/.local/bin:${PATH}"
ROOT="${HOME}/.local/opt/waybar-root"
export LD_LIBRARY_PATH="${ROOT}/usr/lib${LD_LIBRARY_PATH:+:$LD_LIBRARY_PATH}"
DIR="$(cd "$(dirname "$0")" && pwd)"

mapfile -t PLAYERS < <("$DIR/media-players.sh" | grep -v '^$' || true)
n=${#PLAYERS[@]}

if (( n == 0 )); then
  printf '%s\n' '{"text":"","alt":"none","class":"idle","tooltip":"Nenhuma mídia"}'
  exit 0
fi

player="$("$DIR/media-current.sh")"
[[ -n "$player" ]] || player="${PLAYERS[0]}"

status=$(playerctl -p "$player" status 2>/dev/null || echo "Stopped")
artist=$(playerctl -p "$player" metadata artist 2>/dev/null || true)
title=$(playerctl -p "$player" metadata title 2>/dev/null || true)
if [[ -z "${title}" ]]; then
  title=$(playerctl -p "$player" metadata xesam:url 2>/dev/null | sed 's|.*/||;s|%20| |g' || true)
fi

pname="${player%%.*}"
case "$pname" in
  spotify*|Spotify*) icon="" ;;
  chromium*|google-chrome*|brave*|vivaldi*|firefox*|zen*) icon="󰗃" ;;
  mpv*|vlc*|celluloid*) icon="󰐹" ;;
  *) icon="" ;;
esac

if [[ "$status" == "Playing" ]]; then
  class="playing"
elif [[ "$status" == "Paused" ]]; then
  class="paused"
else
  class="stopped"
fi

if [[ -n "$artist" && -n "$title" ]]; then
  text="${artist} - ${title}"
elif [[ -n "$title" ]]; then
  text="$title"
else
  text="$pname"
fi

if (( ${#text} > 42 )); then
  text="${text:0:39}…"
fi

idx=$(cat "${HOME}/.cache/waybar-media-idx" 2>/dev/null || echo 0)
tooltip="${pname} (${status})"
if (( n > 1 )); then
  tooltip="${tooltip} · fonte $((idx + 1))/${n}"
fi
if [[ -n "$artist" ]]; then
  tooltip="${tooltip}"$'\n'"${artist}"
fi
if [[ -n "$title" ]]; then
  tooltip="${tooltip}"$'\n'"${title}"
fi

export WB_ICON="$icon" WB_TEXT="$text" WB_CLASS="$class" WB_TIP="$tooltip"
python3 - <<'PY'
import json, os
print(json.dumps({
    "text": f"{os.environ['WB_ICON']}  {os.environ['WB_TEXT']}",
    "alt": os.environ["WB_CLASS"],
    "class": os.environ["WB_CLASS"],
    "tooltip": os.environ["WB_TIP"],
}, ensure_ascii=False))
PY
