#!/usr/bin/env bash
# Instala blueman (UI Bluetooth) — rode uma vez: bash ~/.config/waybar/scripts/install-blueman.sh
set -euo pipefail
if [[ "$(id -u)" -ne 0 ]]; then
  exec sudo -E bash "$0" "$@"
fi
pacman -S --needed --noconfirm blueman
systemctl enable --now bluetooth
echo "OK — blueman instalado. Clique no ícone BT na Waybar."
