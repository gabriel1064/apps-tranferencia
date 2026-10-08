#!/usr/bin/env bash
# Utilização aproximada da iGPU Intel via residência RC6 (sem gpu_busy_percent / intel_gpu_top).
set -euo pipefail

# Ícone distinto da dGPU/CPU (nf-md-integrated-circuit-variant)
ICON="󰘚"
TOOLTIP="Intel integrada"

# Nome amigável via lspci (ex.: UHD Graphics 630)
if command -v lspci >/dev/null 2>&1; then
  name=$(lspci -nn 2>/dev/null \
    | grep -iE 'VGA|3D|Display' \
    | grep -i 'Intel' \
    | head -n1 \
    | sed -E 's/^[^:]+:[[:space:]]*//; s/[[:space:]]*\[[0-9a-fA-F:]+\]//g; s/[[:space:]]+$//' \
    || true)
  if [[ -n "${name}" ]]; then
    # "Intel Corporation CoffeeLake-H GT2 [UHD Graphics 630]" → preferir o nome curto entre []
    short=$(grep -oE '\[[^]]+Graphics[^]]*\]' <<<"${name}" | head -n1 | tr -d '[]' || true)
    if [[ -n "${short}" ]]; then
      TOOLTIP="Intel ${short}"
    else
      TOOLTIP="Intel integrada"
    fi
  fi
fi

find_rc6() {
  local card vendor f
  for card in /sys/class/drm/card[0-9]; do
    vendor=$(cat "${card}/device/vendor" 2>/dev/null || true)
    [[ "${vendor}" == "0x8086" ]] || continue
    for f in \
      "${card}/gt/gt0/rc6_residency_ms" \
      "${card}/gt/gt1/rc6_residency_ms" \
      "${card}/device/rc6_residency_ms" \
      "${card}/device/power/rc6_residency_ms"
    do
      if [[ -r "${f}" ]]; then
        printf '%s\n' "${f}"
        return 0
      fi
    done
  done
  return 1
}

RC6=$(find_rc6 || true)
if [[ -z "${RC6}" ]]; then
  printf '{"text":"%s iGPU --","tooltip":"%s","class":"gpu-intel"}\n' "${ICON}" "${TOOLTIP}"
  exit 0
fi

r1=$(cat "${RC6}")
t1=$(date +%s%N)
sleep 0.45
r2=$(cat "${RC6}")
t2=$(date +%s%N)

busy=$(awk -v r1="${r1}" -v r2="${r2}" -v t1="${t1}" -v t2="${t2}" 'BEGIN {
  elapsed = (t2 - t1) / 1e6
  if (elapsed <= 0) { print 0; exit }
  delta = r2 - r1
  if (delta < 0) delta = 0
  idle = delta / elapsed * 100
  if (idle > 100) idle = 100
  if (idle < 0) idle = 0
  busy = 100 - idle
  if (busy < 0) busy = 0
  if (busy > 100) busy = 100
  printf "%d", busy + 0.5
}')

printf '{"text":"%s iGPU %s%%","tooltip":"%s","class":"gpu-intel"}\n' "${ICON}" "${busy}" "${TOOLTIP}"
