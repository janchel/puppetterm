#!/usr/bin/env bash
# puppetterm — launch the app in dev mode.
#
# After a reboot, cargo/rustc and go are NOT on PATH (they were installed with
# `--no-modify-path`). This script adds them and starts `npm run tauri dev`.
#
# Usage:
#   ./dev.sh          # full desktop app (Tauri + Svelte frontend)
#   ./dev.sh --browser  # browser-only preview of the UI (mock backend, no Tauri)
#
# Requires: npm deps already installed (`cd client && npm install` once).
set -euo pipefail

export PATH="$HOME/.cargo/bin:$HOME/.local/go/bin:$PATH"

case "${1:-}" in
  --browser)
    npm --prefix client run dev -- --port 1420 --strictPort
    ;;
  *)
    cd client
    npm run tauri dev
    ;;
esac
