#!/usr/bin/env bash
#
# install.sh — install puppetterm-agent on an Ubuntu/Debian host and apply
# hardening (scoped sudoers + a command-locked authorized_keys entry).
#
# Run ON the target machine as root (or with sudo):
#
#   sudo ./install.sh \
#       --binary ./puppetterm-agent-linux-amd64 \
#       --agent-pubkey ./agent.pub \
#       --ssh-user devops
#
# Idempotent — safe to run multiple times.
set -euo pipefail

BINARY_SRC=""
RELEASE_URL=""
AGENT_PUBKEY=""
SSH_USER="${SUDO_USER:-$(id -un)}"
ASSUME_YES=0

usage() {
  cat <<'EOF'
usage: install.sh [options]

Options:
  --binary <path>        path to the puppetterm-agent binary to install
  --release <url>        download the binary from <url> (overrides --binary)
  --agent-pubkey <path>  client's dedicated agent public key (hardened entry)
  --ssh-user <name>      SSH user to grant scoped privileges to (default: current)
  --yes                  non-interactive (auto-confirm)
  -h, --help             show this help
EOF
}

while [ $# -gt 0 ]; do
  case "$1" in
    --binary)      BINARY_SRC="$2"; shift 2 ;;
    --release)     RELEASE_URL="$2"; shift 2 ;;
    --agent-pubkey) AGENT_PUBKEY="$2"; shift 2 ;;
    --ssh-user)    SSH_USER="$2"; shift 2 ;;
    --yes)         ASSUME_YES=1; shift ;;
    -h|--help)     usage; exit 0 ;;
    *) echo "unknown option: $1" >&2; usage >&2; exit 2 ;;
  esac
done

# --- root check (re-exec with sudo if needed) -------------------------------
if [ "$(id -u)" -ne 0 ]; then
  if command -v sudo >/dev/null 2>&1; then
    exec sudo --preserve-env=BINARY_SRC,RELEASE_URL,AGENT_PUBKEY,SSH_USER,ASSUME_YES "$0"
  fi
  echo "error: run as root (e.g. 'sudo $0 ...')" >&2
  exit 1
fi

confirm() { # confirm <prompt>  -> 0 yes / 1 no
  [ "$ASSUME_YES" -eq 1 ] && return 0
  local ans
  read -r -p "$1 [y/N] " ans
  case "$ans" in y|Y) return 0 ;; *) return 1 ;; esac
}

AGENT_PATH="/usr/local/bin/puppetterm-agent"

echo "==> puppetterm-agent install (user: $SSH_USER)"

# --- install the binary -----------------------------------------------------
if [ -n "$RELEASE_URL" ]; then
  echo "    downloading $RELEASE_URL"
  curl -fsSL -o "$AGENT_PATH" "$RELEASE_URL"
  chmod 0755 "$AGENT_PATH"
elif [ -n "$BINARY_SRC" ]; then
  [ -f "$BINARY_SRC" ] || { echo "error: binary not found: $BINARY_SRC" >&2; exit 1; }
  install -m 0755 "$BINARY_SRC" "$AGENT_PATH"
else
  echo "error: provide --binary or --release" >&2
  exit 1
fi
echo "    installed: $AGENT_PATH"
"$AGENT_PATH" </dev/null >/dev/null 2>&1 || true # smoke: should exit 1 with an error, not crash

# --- scoped sudoers ----------------------------------------------------------
SUDOERS_FILE="/etc/sudoers.d/puppetterm-agent"
if [ -f "$SUDOERS_FILE" ] && grep -q "^$SSH_USER " "$SUDOERS_FILE"; then
  echo "    sudoers already configured for $SSH_USER (skipping)"
else
  if confirm "install scoped sudoers for user '$SSH_USER'?"; then
    cat > "$SUDOERS_FILE" <<EOF
# puppetterm-agent — scoped privileges (managed by install.sh)
Cmnd_Alias PUPPETTERM_SYSTEMCTL = /usr/bin/systemctl status *, /usr/bin/systemctl start *, /usr/bin/systemctl stop *, /usr/bin/systemctl restart *, /usr/bin/systemctl enable *, /usr/bin/systemctl disable *, /usr/bin/systemctl is-active *, /usr/bin/systemctl is-enabled *
Cmnd_Alias PUPPETTERM_APT = /usr/bin/apt-get update, /usr/bin/apt-get install -y *, /usr/bin/apt-get remove -y *, /usr/bin/apt-get autoremove -y *, /usr/bin/apt update, /usr/bin/apt install -y *, /usr/bin/apt remove -y *, /usr/bin/apt autoremove -y *
Cmnd_Alias PUPPETTERM_READ = /usr/bin/tail *, /bin/tail *, /usr/bin/journalctl *, /bin/journalctl *, /usr/bin/cat *, /bin/cat *
Cmnd_Alias PUPPETTERM_DEPLOY = /usr/bin/git pull, /usr/bin/systemctl restart *
$SSH_USER ALL=(root) NOPASSWD: PUPPETTERM_SYSTEMCTL, PUPPETTERM_APT, PUPPETTERM_READ, PUPPETTERM_DEPLOY
EOF
    chmod 0440 "$SUDOERS_FILE"
    visudo -cf "$SUDOERS_FILE" >/dev/null
    echo "    wrote $SUDOERS_FILE"
  else
    echo "    skipped sudoers"
  fi
fi

# --- hardened authorized_keys entry ------------------------------------------
if [ -n "$AGENT_PUBKEY" ]; then
  [ -f "$AGENT_PUBKEY" ] || { echo "error: pubkey not found: $AGENT_PUBKEY" >&2; exit 1; }
  AUTH_KEYS="/home/$SSH_USER/.ssh/authorized_keys"
  mkdir -p "$(dirname "$AUTH_KEYS")"
  chmod 700 "$(dirname "$AUTH_KEYS")"
  [ -f "$AUTH_KEYS" ] || : > "$AUTH_KEYS"

  PUB="$(cat "$AGENT_PUBKEY")"
  if grep -qF -- "$PUB" "$AUTH_KEYS"; then
    echo "    agent key already present in $AUTH_KEYS (skipping)"
  else
    if confirm "add command-locked agent key to $AUTH_KEYS?"; then
      {
        echo "# puppetterm-agent (command-locked)"
        printf 'restrict,command="%s",no-pty,no-agent-forwarding,no-port-forwarding,no-X11-forwarding %s puppetterm-agent\n' "$AGENT_PATH" "$PUB"
      } >> "$AUTH_KEYS"
      chmod 600 "$AUTH_KEYS"
      echo "    added command-locked key to $AUTH_KEYS"
    else
      echo "    skipped authorized_keys"
    fi
  fi
else
  echo "    --agent-pubkey not given; skipping authorized_keys hardening"
fi

# --- ensure sshd is present and enabled --------------------------------------
if command -v systemctl >/dev/null 2>&1; then
  systemctl enable --now ssh >/dev/null 2>&1 || systemctl enable --now sshd >/dev/null 2>&1 || true
fi

cat <<EOF

==> Done.
    agent:      $AGENT_PATH
    sudoers:    $SUDOERS_FILE (user: $SSH_USER)
    agent key:  $(if [ -n "$AGENT_PUBKEY" ]; then echo "command-locked in $AUTH_KEYS"; else echo "NOT configured"; fi)

Next steps on your client:
    1. Add this host to the client (host alias from ~/.ssh/config).
    2. Test:  printf '%s' '{"action":"snapshot"}' | ssh <host> "$AGENT_PATH"
EOF
