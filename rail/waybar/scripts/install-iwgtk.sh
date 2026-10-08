#!/usr/bin/env bash
# Instala iwgtk (GUI para iwd) — recomendado neste setup
set -euo pipefail
if [[ "$(id -u)" -ne 0 ]]; then
  exec sudo -E bash "$0" "$@"
fi
if command -v yay >/dev/null 2>&1; then
  sudo -u "${SUDO_USER:-gabriel}" yay -S --needed --noconfirm iwgtk
else
  echo "Instale yay e rode: yay -S iwgtk"
  exit 1
fi
echo "OK — clique no ícone Wi‑Fi na Waybar abre o iwgtk."
