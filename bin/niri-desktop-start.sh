#!/usr/bin/env bash
# Start Nemo as the desktop layer (icons on Background) for niri.
# Prefers the gtk-layer-shell build in ~/.local/opt/nemo-layer (see ~/src/nemo-layer).
set -euo pipefail

export XDG_CURRENT_DESKTOP="${XDG_CURRENT_DESKTOP:-niri}"

DESKTOP_BIN=""
if [[ -x "${HOME}/.local/bin/nemo-desktop-layered" ]]; then
  DESKTOP_BIN="${HOME}/.local/bin/nemo-desktop-layered"
elif command -v nemo-desktop >/dev/null; then
  DESKTOP_BIN="$(command -v nemo-desktop)"
else
  echo "nemo-desktop not found (install ~/src/nemo-layer package or layered wrapper)" >&2
  exit 1
fi

# Desktop folder + icons
gsettings set org.nemo.desktop show-desktop-icons true >/dev/null 2>&1 || true
gsettings set org.nemo.desktop home-icon-visible false >/dev/null 2>&1 || true
gsettings set org.nemo.desktop computer-icon-visible false >/dev/null 2>&1 || true
gsettings set org.nemo.desktop trash-icon-visible true >/dev/null 2>&1 || true
gsettings set org.nemo.desktop volumes-visible true >/dev/null 2>&1 || true

desktop="$(xdg-user-dir DESKTOP 2>/dev/null || true)"
if [[ -n "${desktop}" ]]; then
  mkdir -p "${desktop}"
fi

if pgrep -x nemo-desktop >/dev/null 2>&1; then
  echo "nemo-desktop already running"
  exit 0
fi

nohup "${DESKTOP_BIN}" >/tmp/nemo-desktop.log 2>&1 &
disown || true
sleep 0.5
if pgrep -x nemo-desktop >/dev/null; then
  echo "nemo-desktop started via ${DESKTOP_BIN}"
else
  echo "nemo-desktop failed; see /tmp/nemo-desktop.log" >&2
  tail -20 /tmp/nemo-desktop.log >&2 || true
  exit 1
fi
