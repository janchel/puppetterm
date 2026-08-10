#!/usr/bin/env bash
# ssh-mux.sh — ControlMaster session helper for puppetterm.
#
# Manages one multiplexed SSH master connection per host so that every agent
# action (or plain command) rides the same authenticated connection instead of
# paying the SSH handshake cost each time.
#
# Usage:
#   ssh-mux.sh start  <host> [ssh args...]    open a master connection (backgrounded)
#   ssh-mux.sh run    <host> <cmd...>         run a command through the master
#   ssh-mux.sh agent  <host>                  run puppetterm-agent through the master
#                                             (reads a JSON request from stdin)
#   ssh-mux.sh stop   <host>                  close the master and remove its socket
#   ssh-mux.sh status <host>                  exit 0 if master is up, 1 otherwise
#   ssh-mux.sh list                           list hosts with live masters
#
# Environment:
#   PUPPETTERM_MUX_DIR     socket dir   (default: ${XDG_RUNTIME_DIR:-/tmp}/puppetterm-mux)
#   PUPPETTERM_AGENT_BIN   remote agent (default: /usr/local/bin/puppetterm-agent)
set -euo pipefail

MUX_DIR="${PUPPETTERM_MUX_DIR:-${XDG_RUNTIME_DIR:-/tmp}/puppetterm-mux}"
AGENT_BIN="${PUPPETTERM_AGENT_BIN:-/usr/local/bin/puppetterm-agent}"

sock_for() { # sanitize a host into a safe filename component
  printf '%s' "$1" | tr -c 'A-Za-z0-9' '_'
}

cmd_start() {
  local host="$1"; shift
  local sock="$MUX_DIR/$(sock_for "$host").sock"
  mkdir -p "$MUX_DIR"
  # -M master mode, -f background after auth, -N no remote command.
  # ControlPersist keeps the connection alive briefly after the last use.
  ssh -M -S "$sock" -fN \
      -o ControlMaster=yes \
      -o ControlPersist=600 \
      -o ServerAliveInterval=30 \
      "$host" "$@"
  echo "master up: $host ($sock)"
}

cmd_run() {
  local host="$1"; shift
  local sock="$MUX_DIR/$(sock_for "$host").sock"
  if [ ! -S "$sock" ]; then
    echo "no master for $host — run 'ssh-mux.sh start $host' first" >&2
    return 1
  fi
  ssh -S "$sock" -o ControlMaster=no "$host" "$@"
}

cmd_agent() {
  local host="$1"
  cmd_run "$host" "$AGENT_BIN"
}

cmd_stop() {
  local host="$1"
  local sock="$MUX_DIR/$(sock_for "$host").sock"
  if [ -S "$sock" ]; then
    ssh -S "$sock" -O exit "$host" || true
    rm -f "$sock"
    echo "master stopped: $host"
  else
    echo "no master for $host" >&2
    return 1
  fi
}

cmd_status() {
  local host="$1"
  local sock="$MUX_DIR/$(sock_for "$host").sock"
  if [ -S "$sock" ] && ssh -S "$sock" -O check "$host" >/dev/null 2>&1; then
    echo "up: $host"
    return 0
  fi
  echo "down: $host"
  return 1
}

cmd_list() {
  shopt -s nullglob
  for sock in "$MUX_DIR"/*.sock; do
    printf '%s\n' "$(basename "$sock" .sock)"
  done
}

case "${1:-}" in
  start)  shift; cmd_start "$@" ;;
  run)    shift; cmd_run "$@" ;;
  agent)  shift; cmd_agent "$@" ;;
  stop)   shift; cmd_stop "$@" ;;
  status) shift; cmd_status "$@" ;;
  list)   cmd_list ;;
  *)
    echo "usage: ssh-mux.sh {start|run|agent|stop|status|list} <host> [...]" >&2
    exit 2
    ;;
esac
