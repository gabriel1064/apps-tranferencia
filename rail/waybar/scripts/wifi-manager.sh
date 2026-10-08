#!/usr/bin/env bash
# Abre gerenciador gráfico de Wi‑Fi (iwd)
set -euo pipefail

GUI="${HOME}/.config/waybar/scripts/wifi-gui.py"

# Preferir GUI própria (GTK4 + iwd)
if [[ -x "$GUI" ]] || [[ -f "$GUI" ]]; then
  exec /usr/bin/python3 "$GUI"
fi

# Fallback: iwgtk (AUR), se instalado
if command -v iwgtk >/dev/null 2>&1; then
  exec iwgtk
fi

notify-send "Wi‑Fi" "Interface gráfica não encontrada ($GUI)." 2>/dev/null || true
exit 1
