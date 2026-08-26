#!/bin/sh
# puppetterm container entrypoint.
#
# Runs as root ONLY to prepare writable state, then drops to uid/gid
# PUID/PGID (default 1000) before exec'ing the server:
#
#   1. Sync SSH material from the READ-ONLY host mount (/ssh-in) into the
#      container's writable ~/.ssh — the container needs to write known_hosts,
#      ControlMaster sockets, and agent keys there, but must never mutate the
#      host's real key files.
#   2. Pin /etc/machine-id from the persistent config volume so the encrypted
#      AI API key survives container restarts (it is machine-bound).
set -eu

PUID="${PUID:-1000}"
PGID="${PGID:-1000}"
HOME_DIR="/home/pp"
SSH_DIR="$HOME_DIR/.ssh"
CONFIG_DIR="$HOME_DIR/.config/puppetterm"

mkdir -p "$SSH_DIR" "$CONFIG_DIR"

# ---- ssh keys / config -----------------------------------------------------
if [ -d /ssh-in ] && [ -n "$(ls -A /ssh-in 2>/dev/null)" ]; then
    cp -a /ssh-in/. "$SSH_DIR/"
    chown -R "$PUID:$PGID" "$HOME_DIR/.config" "$SSH_DIR"
    chmod 700 "$SSH_DIR"
    # Private keys must be strictly permissioned or ssh refuses them.
    find "$SSH_DIR" -type f ! -name "*.pub" ! -name "known_hosts*" \
        ! -name "authorized_keys" ! -name "config" -exec chmod 600 {} +
    echo "[entrypoint] synced ssh material from /ssh-in into $SSH_DIR"
else
    chown -R "$PUID:$PGID" "$HOME_DIR/.config" "$SSH_DIR"
    echo "[entrypoint] no ssh material mounted at /ssh-in (password-entry over the terminal still works)"
fi

# ---- machine identity for the encrypted AI key ------------------------------
MACHINE_ID_FILE="/etc/machine-id"
SAVED_ID="$CONFIG_DIR/machine-id"
if [ -s "$SAVED_ID" ]; then
    cat "$SAVED_ID" > "$MACHINE_ID_FILE"
elif [ -s "$MACHINE_ID_FILE" ]; then
    cp "$MACHINE_ID_FILE" "$SAVED_ID"
else
    head -c 16 /dev/urandom | od -An -tx1 | tr -d ' \n' > "$MACHINE_ID_FILE"
    cp "$MACHINE_ID_FILE" "$SAVED_ID"
fi
chown "$PUID:$PGID" "$SAVED_ID"

# ---- drop privileges and run -------------------------------------------------
# NOTE: no --reset-env here — the server reads its config from PUPPETTERM_*
# env vars and they must survive the uid switch.
if [ "$(id -u)" = "0" ] && command -v setpriv >/dev/null 2>&1; then
    export HOME="$HOME_DIR"
    exec setpriv --reuid="$PUID" --regid="$PGID" --init-groups /app/puppetterm-server
fi
exec /app/puppetterm-server
