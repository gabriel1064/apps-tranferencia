#!/usr/bin/env bash
# Utilização da GPU NVIDIA via nvidia-smi (NVML).
set -euo pipefail

# Ícone NVIDIA (nf-md-nvidia) — distinto do chip da iGPU
ICON="󰾲"
TOOLTIP="NVIDIA RTX 2070"

resolve_name() {
  local raw short
  if command -v nvidia-smi >/dev/null 2>&1; then
    raw=$(nvidia-smi --query-gpu=name --format=csv,noheader 2>/dev/null \
      | head -n1 | sed 's/^[[:space:]]*//;s/[[:space:]]*$//' || true)
    # Ignorar mensagens de erro do driver (vão para stdout em alguns setups)
    if [[ -n "${raw}" && "${raw}" != *[Ff]ailed* && "${raw}" != *[Ee]rror* && "${raw}" =~ [Gg]e[Ff]orce|[Rr][Tt][Xx]|[Qq]uadro|[Tt]esla ]]; then
      short=$(sed -E 's/[[:space:]]+Mobile$//; s/^NVIDIA[[:space:]]+//' <<<"${raw}")
      TOOLTIP="NVIDIA ${short}"
      return 0
    fi
  fi
  if command -v lspci >/dev/null 2>&1; then
    raw=$(lspci -nn 2>/dev/null \
      | grep -iE 'VGA|3D|Display' \
      | grep -i 'NVIDIA' \
      | head -n1 || true)
    if [[ -n "${raw}" ]]; then
      short=$(grep -oE 'GeForce [^]]+|RTX [0-9]+[^]]*' <<<"${raw}" | head -n1 | sed 's/[[:space:]]*$//' || true)
      if [[ -n "${short}" ]]; then
        short=$(sed -E 's/[[:space:]]+Mobile$//' <<<"${short}")
        TOOLTIP="NVIDIA ${short}"
      fi
    fi
  fi
}

resolve_name

util=""
if command -v nvidia-smi >/dev/null 2>&1; then
  util=$(nvidia-smi --query-gpu=utilization.gpu --format=csv,noheader,nounits 2>/dev/null \
    | head -n1 | tr -d '[:space:]' || true)
fi

if [[ -z "${util}" || ! "${util}" =~ ^[0-9]+$ ]]; then
  printf '{"text":"%s dGPU --","tooltip":"%s","class":"gpu-nvidia"}\n' "${ICON}" "${TOOLTIP}"
  exit 0
fi

printf '{"text":"%s dGPU %s%%","tooltip":"%s","class":"gpu-nvidia"}\n' "${ICON}" "${util}" "${TOOLTIP}"
