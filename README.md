# puppetterm

A lightweight, Warp-like **desktop terminal + agentic AI** for managing Linux/Ubuntu
servers over your **existing SSH setup**. No extra daemons, no listeners, no cloud —
the app talks to servers the same way you do, and **every state-changing action
requires your approval**.

> 🧪 Experimental solo project — early stage.

---

## Why

Most remote-management tools either run a persistent agent (a new attack surface) or
replay commands blindly. puppetterm:

- Uses **only the SSH access you already have** (keys or password).
- Gives you an **agentic AI** that can inspect and change remote servers — but
  **you see and approve** every state-changing command.
- Installs a **stateless Go agent** on key-based hosts for structured, scriptable
  results (snapshot, service control, logs, config) — invoked *through* SSH, never
  listening.
- Falls back to **driving your live terminal** on any host (including password-only
  ones) with nothing to install.

## Two operating modes

| | **Agent mode** (SSH key available) | **Terminal mode** (any host, incl. password-only) |
|---|---|---|
| Setup | Install `puppetterm-agent` on the remote (in-app or `installer/install.sh`) | Nothing to install |
| How the AI acts | Structured tools over SSH: `run_command`, `service`, `log`, `config`, `snapshot` | Types the command into your **live terminal** and waits for the output |
| Result | Structured (exit code, snapshot data, service state, audit log) | What you see on screen |
| Good for | Agentic coding / management, repeatable ops | Quick checks, password-auth servers |

Both modes share the same **approval gate**:

- ✅ Read-only actions (`ls`, `cat`, `ps`, `df`, `snapshot`, `status`, …) run automatically.
- ⚠️ State-changing actions (install, restart, write config, `run_command`) prompt you first.
- 🚨 Dangerous patterns (`rm -rf /`, `mkfs`, `reboot`, `dd of=/dev/…`) are flagged red.
- You can also set the chat to **read-only-auto** (never changes state) or **ask-first**.

## Features

- **Tabbed terminals** (xterm.js) — local-first: launch opens a local shell; type
  `ssh user@host` to connect (the tab follows the host, even from shell history).
- **Resizable AI chat panel** with a **provider/model switcher** (custom
  OpenAI-compatible, DeepSeek, or Claude), **new chat**, live "acting on \<host\>" banner,
  and an **Activity** log.
- **Multi-provider AI** — your API key is **encrypted at rest** (ChaCha20-Poly1305,
  machine-bound); plaintext never touches disk and is never committed.
- **In-app agent installer** — user-space by default (no sudo); auto-upgrades to root
  when passwordless sudo exists; **idempotent**.
- **Audit trail** — every action is recorded in a client-side SQLite DB (append-only)
  and in the remote agent's log.
- **Safety** — AI targets are pinned per task (switching tabs mid-task can't redirect
  it), Abort kills the remote command, and dangerous commands are screened.

## Requirements

- **Node.js** 20+ and **npm**
- **Rust** toolchain (`cargo`) for the Tauri backend
- **Go** 1.2x for building the remote agent
- **Tauri Linux system deps** (webkit2gtk, etc.) — see
  [Tauri prerequisites](https://tauri.app/start/prerequisites/)
- SSH access to the machines you manage (key or password)

> On this machine `cargo` and `go` are installed with `--no-modify-path`, so they are
> **not on PATH by default** — `./dev.sh` adds them automatically.

## Run in development

```bash
# one command — adds ~/.cargo/bin + ~/.local/go/bin to PATH, then launches
./dev.sh

# browser-only UI preview (mock backend, no Tauri, no SSH):
./dev.sh --browser        # → http://localhost:1420/
```

Manual equivalent:

```bash
export PATH="$HOME/.cargo/bin:$HOME/.local/go/bin:$PATH"
cd client
npm install                 # once
npm run tauri dev           # full desktop app
```

### Build the remote agent (for agent mode)

```bash
cd agent
make cross        # → bin/puppetterm-agent-linux-{amd64,arm64} (static)
make test && make smoke
```

## Install the agent on a server

**In-app (recommended):** connect to a key-based host with `ssh user@host`, click
**Install agent** in the AI panel, type `y`. It installs user-space
(`~/.puppetterm/bin/puppetterm-agent`) with a command-locked key, then upgrades to
root if passwordless sudo is available.

**Manual:**

```bash
installer/install.sh --binary agent/bin/puppetterm-agent-linux-amd64 \
                     --agent-pubkey ~/.ssh/puppetterm-agent.pub \
                     --ssh-user ubuntu --yes
```

The dedicated agent key is **command-locked** (`restrict,command="…puppetterm-agent",
no-pty,…`) — it can only invoke the agent, never open a shell.

## AI configuration

Open **Settings (⚙)** in the app:

- **Provider** — Custom (OpenAI-compatible), DeepSeek, or Claude (Anthropic).
- **Endpoint / Model / API key** — presets prefill for DeepSeek/Claude.
- The config is stored at `~/.config/puppetterm/ai.json` (outside the repo, `chmod 600`);
  the key is encrypted at rest. Env overrides: `PUPPETTERM_AI_BASE_URL`, `PUPPETTERM_AI_MODEL`,
  `PUPPETTERM_AI_API_KEY`, `PUPPETTERM_AI_PROVIDER`.

> 🔒 **Never commit API keys.** `ai.json` and everything under `~/.config/puppetterm/`
> are outside the repository.

## Security notes

- **No persistent agent/listener** on remote hosts — the agent is invoked per-action
  through SSH and exits. Nothing new listens.
- **Least privilege** — user-space install by default; root upgrade only when the user
  already has passwordless sudo; scoped sudoers aliases (no blanket root).
- **No secrets in the repo** — keys, AI config, and DBs are gitignored and/or live
  outside the repo.
- **Approval-gated** — the AI can't change state without you saying yes; dangerous
  commands are flagged; the audit log records what ran and when.

## Project layout

| Path | What it is |
|---|---|
| `agent/` | Go — stateless remote worker (`puppetterm-agent`), invoked through SSH (NDJSON on stdin/stdout). No listener, no daemon. |
| `client/` | Tauri desktop app (Svelte 5 + xterm.js) — tabbed terminals + AI chat panel. |
| `installer/` | `install.sh` + sudoers/`authorized_keys` templates for hardening. |
| `scripts/` | `e2e.py` end-to-end test, `ssh-mux.sh` ControlMaster helper. |
| `dev.sh` | One-command dev launcher (adds toolchains to PATH, runs the app). |

## Development & testing

```bash
cd agent && make test && make smoke   # Go agent: unit + protocol smoke
cd client/src-tauri && cargo test --lib # Rust backend (AI, install, audit, agent)
cd client && npm run check && npm run build   # frontend (svelte-check + vite)
scripts/e2e.py --host user@server --approve-all   # full end-to-end
```

