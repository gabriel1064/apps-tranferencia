#!/usr/bin/env bash
# Reapply known-good Noctalia settings (bottom bar; center hosts niri-rail).
# Always sync the bar layout from settings.wanted.toml so laptop and HDMI
# stay consistent (GUI edits / dock hotplug must not leave a broken bar).
set -euo pipefail

WANTED="${HOME}/.config/noctalia/settings.wanted.toml"
LIVE="${HOME}/.local/state/noctalia/settings.toml"
WALL="${HOME}/Pictures/Wallpapers/astro.jpg"

if [[ ! -f "$WANTED" ]]; then
  echo "missing $WANTED" >&2
  exit 1
fi

mkdir -p "$(dirname "$LIVE")"

need_restore=0
if [[ ! -f "$LIVE" ]]; then
  need_restore=1
elif ! grep -q 'position = "bottom"' "$LIVE" 2>/dev/null; then
  need_restore=1
elif ! grep -q 'capsule = false' "$LIVE" 2>/dev/null; then
  need_restore=1
elif ! grep -q 'margin_ends = 0' "$LIVE" 2>/dev/null; then
  # Non-zero margin_ends leaves wallpaper holes on both sides.
  need_restore=1
elif ! grep -q 'bar.default.monitor.edp' "$LIVE" 2>/dev/null; then
  need_restore=1
elif grep -q '"media"' "$LIVE" 2>/dev/null; then
  # Long media label was clipping the power button on 1920 logical width.
  need_restore=1
elif grep -q '"spacer"' "$LIVE" 2>/dev/null; then
  # Launcher/power must sit at the screen edges (no spacers).
  need_restore=1
elif ! grep -q 'thickness = 42' "$LIVE" 2>/dev/null; then
  need_restore=1
fi

if [[ "$need_restore" -eq 1 ]]; then
  cp -f "$WANTED" "$LIVE"
fi

if pgrep -x noctalia >/dev/null 2>&1; then
  noctalia msg config-reload >/dev/null 2>&1 || true
else
  nohup "${HOME}/.local/bin/noctalia-daemon.sh" >/dev/null 2>&1 &
  sleep 1
fi

if [[ -f "$WALL" ]]; then
  noctalia msg wallpaper-set "$WALL" >/dev/null 2>&1 || true
fi

echo "noctalia ok (bar synced for eDP + HDMI)"
