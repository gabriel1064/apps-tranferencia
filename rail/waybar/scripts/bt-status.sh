#!/usr/bin/env bash
# Status Bluetooth para Waybar (só ícone)
set -u

powered=0
connected=0
tooltip="Bluetooth"

if command -v bluetoothctl >/dev/null 2>&1; then
  show="$(bluetoothctl show 2>/dev/null || true)"
  if echo "$show" | grep -qi 'Powered: yes'; then
    powered=1
  fi
  while IFS= read -r line; do
    mac="${line#Device }"
    mac="${mac%% *}"
    [[ -z "$mac" ]] && continue
    info="$(bluetoothctl info "$mac" 2>/dev/null || true)"
    if echo "$info" | grep -qi 'Connected: yes'; then
      connected=$((connected + 1))
      name="$(echo "$info" | awk -F': ' '/Name:/{print $2; exit}')"
      tooltip="${tooltip}\n● ${name:-$mac}"
    fi
  done < <(bluetoothctl devices 2>/dev/null || true)
fi

# Ícones Nerd Font (Material Design)
icon_off="󰂲"
icon_on="󰂯"
icon_connected="󰂱"

if [[ "$powered" -eq 0 ]]; then
  printf '{"text":"%s","class":"off","tooltip":"Bluetooth desligado\\nClique: gerenciador\\nDir: ligar/desligar"}\n' "$icon_off"
elif [[ "$connected" -gt 0 ]]; then
  printf '{"text":"%s","class":"connected","tooltip":"%s\\nClique: gerenciador\\nDir: ligar/desligar"}\n' \
    "$icon_connected" "${tooltip//$'\n'/\\n}"
else
  printf '{"text":"%s","class":"on","tooltip":"Bluetooth ligado\\nClique: gerenciador\\nDir: ligar/desligar"}\n' "$icon_on"
fi
