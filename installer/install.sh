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
PRESET=""
ASSUME_YES=0

usage() {
  cat <<'EOF'
usage: install.sh [options]

Options:
  --binary <path>        path to the puppetterm-agent binary to install
  --release <url>        download the binary from <url> (overrides --binary)
  --agent-pubkey <path>  client's dedicated agent public key (hardened entry)
  --ssh-user <name>      SSH user to grant scoped privileges to (default: current)
  --preset <name>        capability preset: web-server (grants /etc/nginx/ config writes)
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
    --preset)      PRESET="$2"; shift 2 ;;
    --yes)         ASSUME_YES=1; shift ;;
    -h|--help)     usage; exit 0 ;;
    *) echo "unknown option: $1" >&2; usage >&2; exit 2 ;;
  esac
done

case "$PRESET" in
  ""|web-server) ;;
  *) echo "error: unknown preset '$PRESET' (supported: web-server)" >&2; exit 2 ;;
esac

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
# Write to a temp file then atomically rename over the target. Overwriting the
# binary in place fails with "Text file busy" (ETXTBSY) when the agent is
# currently executing (e.g. a live metrics poll or in-flight action); rename
# only relinks the directory entry so the running process keeps its old inode.
if [ -n "$RELEASE_URL" ]; then
  echo "    downloading $RELEASE_URL"
  curl -fsSL -o "${AGENT_PATH}.tmp" "$RELEASE_URL"
  chmod 0755 "${AGENT_PATH}.tmp"
elif [ -n "$BINARY_SRC" ]; then
  [ -f "$BINARY_SRC" ] || { echo "error: binary not found: $BINARY_SRC" >&2; exit 1; }
  install -m 0755 "$BINARY_SRC" "${AGENT_PATH}.tmp"
else
  echo "error: provide --binary or --release" >&2
  exit 1
fi
mv -f "${AGENT_PATH}.tmp" "$AGENT_PATH"
echo "    installed: $AGENT_PATH"
"$AGENT_PATH" </dev/null >/dev/null 2>&1 || true # smoke: should exit 1 with an error, not crash

# --- capability preset --------------------------------------------------------
case "$PRESET" in
  web-server)
    mkdir -p /etc/puppetterm /usr/local/lib/puppetterm
    cat > /etc/puppetterm/config.json <<'EOF'
{"log_prefixes":["/var/log/"],"config_prefixes":["/etc/nginx/"]}
EOF
    cat > /usr/local/lib/puppetterm/write-file <<'EOF'
#!/bin/sh
# puppetterm write-file helper — writes stdin to an allow-listed path.
# Granted NOPASSWD via sudoers; enforces its own path allow-list.
set -eu
ALLOWED_PREFIXES="/etc/nginx/"
[ "$#" -eq 1 ] || { echo "usage: write-file <path>" >&2; exit 2; }
path="$1"
case "$path" in
  ${ALLOWED_PREFIXES}*) ;;
  *) echo "write-file: path not allowed: $path" >&2; exit 1 ;;
esac
mkdir -p "$(dirname "$path")" 2>/dev/null || true
cat > "$path"
EOF
    chmod 0755 /usr/local/lib/puppetterm/write-file
    echo "    preset web-server: allow-list (/etc/nginx/) + write helper installed"
    ;;
esac

# --- agent audit log dir ------------------------------------------------------
mkdir -p /var/log/puppetterm
chown "$SSH_USER" /var/log/puppetterm 2>/dev/null || true
echo "    audit log dir: /var/log/puppetterm (owner: $SSH_USER)"

# --- scoped sudoers ----------------------------------------------------------
SUDOERS_FILE="/etc/sudoers.d/puppetterm-agent"
SUDOERS_CURRENT=""
if [ -f "$SUDOERS_FILE" ] && grep -q "^$SSH_USER " "$SUDOERS_FILE"; then
  SUDOERS_CURRENT=1
  # A preset may require extra grants the existing file lacks — rewrite then.
  case "$PRESET" in
    web-server)
      grep -q "/usr/local/lib/puppetterm/write-file" "$SUDOERS_FILE" || SUDOERS_CURRENT=""
      ;;
  esac
fi
if [ -n "$SUDOERS_CURRENT" ]; then
  echo "    sudoers already configured for $SSH_USER (skipping)"
else
  if confirm "install scoped sudoers for user '$SSH_USER'?"; then
    tmpfile="$(mktemp "${SUDOERS_FILE}.XXXXXX")"
    {
      echo "# puppetterm-agent — scoped privileges (managed by install.sh)"
      echo "Cmnd_Alias PUPPETTERM_SYSTEMCTL = /usr/bin/systemctl status *, /usr/bin/systemctl start *, /usr/bin/systemctl stop *, /usr/bin/systemctl restart *, /usr/bin/systemctl reload *, /usr/bin/systemctl reload-or-restart *, /usr/bin/systemctl enable *, /usr/bin/systemctl disable *, /usr/bin/systemctl is-active *, /usr/bin/systemctl is-enabled *"
      echo "Cmnd_Alias PUPPETTERM_APT = /usr/bin/apt-get update, /usr/bin/apt-get install -y *, /usr/bin/apt-get remove -y *, /usr/bin/apt-get autoremove -y *, /usr/bin/apt update, /usr/bin/apt install -y *, /usr/bin/apt remove -y *, /usr/bin/apt autoremove -y *"
      echo "Cmnd_Alias PUPPETTERM_DEPLOY = /usr/bin/git pull, /usr/bin/systemctl restart *"
      echo "# No cat/tail/journalctl aliases: arbitrary file reads (e.g. /etc/shadow) must"
      echo "# stay denied. Log reads rely on group access (user in 'adm') instead."
      echo "$SSH_USER ALL=(root) NOPASSWD: PUPPETTERM_SYSTEMCTL, PUPPETTERM_APT, PUPPETTERM_DEPLOY"
      case "$PRESET" in
        web-server)
          echo "# web-server preset: config writes via the scoped helper (no wildcards)"
          echo "$SSH_USER ALL=(root) NOPASSWD: /usr/local/lib/puppetterm/write-file"
          ;;
      esac
    } > "$tmpfile"
    chmod 0440 "$tmpfile"
    if visudo -cf "$tmpfile" >/dev/null 2>&1; then
      mv "$tmpfile" "$SUDOERS_FILE"
      echo "    wrote $SUDOERS_FILE"
    else
      rm -f "$tmpfile"
      echo "error: sudoers validation failed; existing file left untouched" >&2
      exit 1
    fi
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
    preset:     ${PRESET:-none}

Next steps on your client:
    1. Add this host to the client (host alias from ~/.ssh/config).
    2. Test:  printf '%s' '{"action":"snapshot"}' | ssh <host> "$AGENT_PATH"
EOF
