#!/usr/bin/env bash
# Instala UI gráfica do NetworkManager (opcional, melhor que nmtui)
set -euo pipefail
if [[ "$(id -u)" -ne 0 ]]; then
  exec sudo -E bash "$0" "$@"
fi
pacman -S --needed --noconfirm network-manager-applet
echo "OK — nm-connection-editor disponível. Clique no ícone Wi‑Fi na Waybar."
