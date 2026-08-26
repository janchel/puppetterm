#!/usr/bin/env bash
# puppetterm — launch the app in dev mode.
#
# After a reboot, cargo/rustc and go are NOT on PATH (they were installed with
# `--no-modify-path`). This script adds them and starts `npm run tauri dev`.
#
# Usage:
#   ./dev.sh              # full desktop app (Tauri + Svelte frontend)
#   ./dev.sh --browser    # browser UI against a running puppetterm-server
#                         #   (start one first:  cargo run -p puppetterm-server)
#   ./dev.sh --browser --mock   # browser UI with the mock backend (no server)
#
# Requires: npm deps already installed (`cd client && npm install` once).
set -euo pipefail

export PATH="$HOME/.cargo/bin:$HOME/.local/go/bin:$PATH"

case "${1:-}" in
  --browser)
    if [ "${2:-}" = "--mock" ]; then
      VITE_PUPPETTERM_MOCK=1 npm --prefix client run dev -- --port 1420 --strictPort
    else
      npm --prefix client run dev -- --port 1420 --strictPort
      # /api + /ws are proxied to http://127.0.0.1:8080
      # (override with PUPPETTERM_SERVER_URL=...)
    fi
    ;;
  *)
    cd client
    npm run tauri dev
    ;;
esac
