#!/usr/bin/env bash
# Abre/fecha o painel de volume (Quickshell IPC)
export PATH="${HOME}/.local/bin:${PATH}"
exec qs ipc call bar toggleVolume
