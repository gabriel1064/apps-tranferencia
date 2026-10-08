#!/usr/bin/env bash
# Abre/fecha o painel de mídia (Quickshell IPC)
export PATH="${HOME}/.local/bin:${PATH}"
exec qs ipc call bar toggleMedia
