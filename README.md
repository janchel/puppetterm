# puppetterm

<p align="center">
  <img src="docs/puppetterm-logo-1024.png" alt="puppetterm logo" width="180" />
</p>

**puppetterm** is a desktop terminal + AI assistant for managing Linux/Ubuntu servers
over your **existing SSH setup**. The AI is your *puppet*: you pull the strings — it can
work agentically (inspect, install, configure, fix), but it only ever moves with your
approval. No extra daemons, no listeners, no cloud — it talks to servers the same way
you do.

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
| How the AI acts | Structured tools over SSH: `run_command`, `read`, `service`, `log`, `config`, `snapshot` (run_command output capped ~24k words; page big files with `read`/`grep`) | Types the command into your **live terminal** and waits for the output |
| Result | Structured (exit code, snapshot data, service state, audit log) | What you see on screen |
| Good for | Agentic coding / management, repeatable ops | Quick checks, password-auth servers |

Both modes share the same **approval gate**:

- ✅ Read-only actions (`ls`, `cat`, `ps`, `df`, `snapshot`, `status`, …) run automatically.
- ⚠️ State-changing actions (install, restart, write config, `run_command`) prompt you first.
- 🚨 Dangerous patterns (`rm -rf /`, `mkfs`, `reboot`, `dd of=/dev/…`) are flagged red.
- The AI always answers your question in text *before* running anything, so you see *why* it wants to act.
- Autonomy modes (⚙ Settings): **ask-first** (default), **propose-first** (approve *every* command, even read-only), or **read-only-auto** (never changes state).

> **Agent mode vs terminal mode — why it matters.** The app detects whether
> `puppetterm-agent` is installed on the host and tells the AI which mode it's
> in (a small **agent** / **terminal** badge shows next to the host). Agent mode
> returns **structured, audited results** (clean exit codes, snapshot data,
> allow-listed config/log access, scoped sudoers) — more reliable and safer than
> screen-scraping a terminal. Terminal mode needs nothing installed and works on
> any host, but the AI only sees what's on screen. The two are complementary:
> agent mode for real management, terminal mode as the universal fallback.

## Features

- **Tabbed terminals** (xterm.js) — local-first: launch opens a local shell; type
  `ssh user@host` to connect (the tab follows the host, even from shell history).
- **Resizable AI chat panel** with a **provider/model switcher** (custom
  OpenAI-compatible, DeepSeek, or Claude), **new chat**, live "acting on \<host\>" banner
  with an **agent / terminal mode badge**, and an **Activity** log. The AI is
  **agent-aware**: it detects whether `puppetterm-agent` is installed on the host and
  adapts its tools + behavior accordingly (structured agent tools vs live-terminal only).
 - **Multi-provider AI** — your API key is **encrypted at rest** (ChaCha20-Poly1305,
   machine-bound); plaintext never touches disk and is never committed. You can also
   authenticate via **Web login (OAuth)** (PKCE) for providers like GitHub Models and
   OpenRouter — the bearer token is stored in the same encrypted slot, no key to paste.
- **In-app agent installer** — user-space by default (no sudo); auto-upgrades to root
  when passwordless sudo exists; **idempotent**.
- **Audit trail** — every action is recorded in a client-side SQLite DB (append-only)
  and in the remote agent's log.
- **Safety** — AI targets are pinned per task (switching tabs mid-task can't redirect
  it), Abort kills the remote command, and dangerous commands are screened.
- **Bounded file access (agent mode)** — `run_command` output is capped at ~24k words
  per command (truncated with a "narrow your command" hint), and a paginated `read`
  tool (`offset`/`limit`) pages through large logs without dumping them into context.
  The AI is told to `grep` first, then `read` the exact range.
- **Local chat history** — the conversation auto-persists in the browser (localStorage)
  and can be **dumped** to Markdown or JSON; the AI is instructed not to trust stale
  chat/activity history and to re-query the live server state instead.
- **Audit detail on demand** — the Activity panel is click-to-expand: each row shows the
  command plus the full output (stored in a file, kept out of the SQLite index and out
  of AI context).
- **AI provider management** — delete a provider and **test the connection** before
  saving (endpoint/model/key validated with a real completion call).

## Requirements

- Desktop dev: **Node.js** 20+, **npm**, **Rust** toolchain (`cargo`), **Tauri
  Linux system deps** (webkit2gtk, etc.) — see
  [Tauri prerequisites](https://tauri.app/start/prerequisites/)
- Headless server / Docker: just Docker (the `server/` crate has no GUI deps)
- **Go** 1.2x only if you want to rebuild the remote agent yourself
- SSH access to the machines you manage (key or password)

> On this machine `cargo` and `go` are installed with `--no-modify-path`, so they are
> **not on PATH by default** — `./dev.sh` adds them automatically.

## Run in development

```bash
# one command — adds ~/.cargo/bin + ~/.local/go/bin to PATH, then launches
./dev.sh

# browser UI against a locally running headless server:
cargo run -p puppetterm-server &   # API + WS on http://127.0.0.1:8080
./dev.sh --browser                 # → http://localhost:1420/ (proxied to :8080)

# browser-only UI preview with a mock backend (no server, no SSH):
./dev.sh --browser --mock
```

Manual equivalent:

```bash
export PATH="$HOME/.cargo/bin:$HOME/.local/go/bin:$PATH"
cd client
npm install                 # once
npm run tauri dev           # full desktop app
```

## Run in Docker (self-hosted web app)

Instead of installing the desktop app, you can run puppetterm as a service and
use it from any browser. The container serves the same UI and talks to your
servers over the SSH keys you mount into it:

```bash
# open (LAN only — the server warns loudly):
docker compose up -d --build

# with basic auth (recommended for anything reachable beyond localhost):
PUPPETTERM_BASIC_AUTH=admin:s3cret docker compose up -d --build
# → http://localhost:8080
```

What compose mounts:

| Mount | Purpose |
|---|---|
| `$HOME/.ssh` → `/ssh-in` (**read-only**) | Your existing keys/config, copied by the entrypoint into the container's own writable `~/.ssh`. The host's real files are never modified. |
| `puppetterm-ssh` volume | Writable `~/.ssh` inside the container (`known_hosts`, ControlMaster sockets). |
| `puppetterm-config` volume | AI config + audit DB + pinned machine-id (the encrypted AI key survives restarts). |

> **Updating SSH keys/hosts:** host `~/.ssh` changes are only synced at container startup (`docker/entrypoint.sh:24-31` copies `/ssh-in` → `~/.ssh`). After adding a key/host or editing `~/.ssh/config`, run `docker compose restart puppetterm` (or `docker compose up -d`) — no rebuild needed.

Useful knobs: `PUPPETTERM_PORT` (host port), `PUPPETTERM_SSH_DIR` (alternate
SSH dir), `PUID`/`PGID` (container user), `PUPPETTERM_AI_*` (AI provider env
overrides). The AI provider/model/key can also be set in-app via ⚙ Settings.

> The **local terminal** tab inside the web app is a shell *in the container*.
> Remote tabs behave exactly like the desktop app.

### Build the image without compose

```bash
docker build -t puppetterm .
docker run --rm -p 8080:8080 -e PUPPETTERM_BASIC_AUTH=me:pass \
  -v "$HOME/.ssh:/ssh-in:ro" puppetterm
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
root if passwordless sudo is available. The **↻ Update agent** button (or any re-run)
reinstalls and refreshes the existing binary, so deploying always updates an installed
agent.

**Manual:**

```bash
installer/install.sh --binary agent/bin/puppetterm-agent-linux-amd64 \
                     --agent-pubkey ~/.ssh/puppetterm-agent.pub \
                     --ssh-user ubuntu --yes
```

The dedicated agent key is **command-locked** (`restrict,command="…puppetterm-agent",
no-pty,…`) — it can only invoke the agent, never open a shell.

## AI configuration

Open **Settings (⚙)** in the app. The AI talks to any **OpenAI-compatible** chat
endpoint. There are two ways to authenticate:

### 1. API key (static)

- **Provider** — Custom (OpenAI-compatible), DeepSeek, or Claude (Anthropic).
- **Endpoint / Model / API key** — presets prefill for DeepSeek/Claude; for the
  Custom provider paste your own base URL (e.g. Google AI Studio, OpenRouter, a
  self-hosted gateway).

### 2. Web login (OAuth — no key)

Pick **Authentication → Web login (OAuth)** to log in through the provider's
browser login instead of pasting a key. The app opens the provider's authorize
page, the provider redirects back to the app, and the returned **bearer token is
stored encrypted at rest** (same slot as an API key) — the chat path is unchanged.

- **Provider preset** — pick a preset to auto-fill the endpoint and OAuth
  metadata (no need to copy URLs by hand):
  - **GitHub Models** — standard OAuth: logs in via `github.com`, then calls
    `https://models.inference.ai.azure.com/openai/v1` (scope `read:models`).
  - **OpenRouter** — OpenRouter's PKCE flow: logs in at `https://openrouter.ai/auth`,
    then exchanges the code for a long-lived API key at `https://openrouter.ai/api/v1/auth/keys`.
  - **Google (Chrome account)** — standard OAuth via the Google account already
    signed into Chrome: `accounts.google.com` → `generativelanguage.googleapis.com/v1beta/openai/`
    (model `gemini-2.0-flash`, scope `generative-language.retriever` + `cloud-platform`).
    Chrome reuses your existing Google session, so no extra password if you're already logged in.
- **Manual OAuth** — any OpenAI-compatible provider that exposes a standard
  authorization-code + PKCE endpoint: fill **Auth URL**, **Token URL**, **Client ID**,
  **Scope** (optional), and **Redirect URI** yourself.
- **Log in** — opens the provider login in a popup and polls until the token lands.

> ⚙️ **Redirect URI** — register `<your-server-origin>/oauth/callback` as the OAuth
> app's redirect/callback URL (OpenRouter identifies the app by the `callback_url`).
> This callback route is exempt from HTTP basic auth so the provider redirect can reach it.

### Common notes

- **Test connection** — validate the endpoint/model/key (or token) with a live
  completion call before saving.
- **Delete provider** — remove a configured provider from the UI.
- The config is stored at `~/.config/puppetterm/ai.json` (outside the repo, `chmod 600`);
  the key/token is encrypted at rest and never sent to the browser. Env overrides:
  `PUPPETTERM_AI_BASE_URL`, `PUPPETTERM_AI_MODEL`, `PUPPETTERM_AI_API_KEY`,
  `PUPPETTERM_AI_PROVIDER`.

> 🔒 **Never commit API keys/tokens.** `ai.json` and everything under `~/.config/puppetterm/`
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
| `core/` | Rust — shared backend logic (SSH sessions, agent bridge, AI client, audit DB, installer). No UI dependencies. |
| `client/` | Tauri desktop app (Svelte 5 + xterm.js) — tabbed terminals + AI chat panel. |
| `server/` | Rust — headless axum server: same commands over HTTP + WebSocket, serves the web UI. |
| `installer/` | `install.sh` + sudoers/`authorized_keys` templates for hardening. |
| `docker/` | Container entrypoint (SSH sync + machine-id pinning + privilege drop). |
| `Dockerfile`, `docker-compose.yml` | Self-hosted web deployment. |
| `docs/` | Branding assets — `puppetterm-logo-1024.png` (regenerate icons via `npm --prefix client run tauri icon`). |
| `scripts/` | `e2e.py` end-to-end test, `ssh-mux.sh` ControlMaster helper. |
| `dev.sh` | One-command dev launcher (adds toolchains to PATH, runs the app). |

The Rust code is a Cargo workspace (`Cargo.toml` at the repo root): the
desktop shell (`client/src-tauri`) and the web server (`server/`) both depend
on `core/`, so behavior is identical everywhere.

## Development & testing

```bash
cargo test --workspace                 # core + server (Rust backend tests)
cd agent && make test && make smoke    # Go agent: unit + protocol smoke
cd client && npm run check && npm run build   # frontend (svelte-check + vite)
scripts/e2e.py --host user@server --approve-all   # full end-to-end
```

## License

[MIT](./LICENSE) © 2026 [Jan Bas](https://github.com/janchel)

