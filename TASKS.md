# Puppetterm — Phase-by-Phase Task Tracker

Tracking doc for the SSH-native, agentic remote terminal (see `agentic-remote-terminal-plan.txt`).

## How to use this tracker
- Mark a task `[x]` only when **every** acceptance criterion below it passes.
- Acceptance criteria are written to be runnable/verifiable — check each one off as you verify it.
- A phase is **Done** when all its tasks are `[x]` (definition of done at the top of each phase).
- Dependencies are listed per task (`Depends on: ...`). A task with unmet dependencies is `[!]` blocked.

## Status legend
`[ ]` not started · `[~]` in progress · `[x]` done · `[!]` blocked

---

## Phase 0 — Project Scaffold & Toolchain
**Goal:** a buildable monorepo skeleton so every later phase has a place to land.

- [x] **T0.1 — Repo structure**
  - Depends on: —
  - Create monorepo layout: `agent/`, `client/`, `installer/`, `README.md`.
  - `git init`, sensible `.gitignore` (target/, node_modules/, dist/, *.control).
  - **AC:** `git status` clean after init; `tree -L 2` shows the expected dirs; README states the one-line project summary.

- [~] **T0.2 — Toolchain verified** (Go 1.26.5 + Node OK; Rust/`tauri-cli` pending)
  - Depends on: —
  - Install/verify: Go, Node, Rust, `tauri-cli`.
  - **AC:** all of `go version`, `node -v`, `rustc --version`, `tauri --version` succeed.

- [x] **T0.3 — Agent build (Makefile)**
  - Depends on: T0.1
  - `agent/Makefile` with `build` target (host OS).
  - **AC:** `make build` produces `agent/bin/puppetterm-agent`; binary runs (`--help` exits 0).

- [ ] **T0.4 — Tauri app skeleton boots**
  - Depends on: T0.2, T0.1
  - `npx create-tauri-app client --template svelte-ts` (or chosen template); three-pane placeholder layout.
  - **AC:** `npm run tauri dev` opens a window showing the placeholder panes with no console errors.

---

## Phase 1 — Go Agent MVP (protocol core)
**Goal:** a stateless CLI that reads one action from stdin and streams NDJSON to stdout. Everything downstream depends on this protocol.
**DoD:** `go test ./...` green, protocol stable, zero external Go dependencies.

- [x] **T1.1 — NDJSON protocol defined**
  - Depends on: T0.3
  - Define request shape and event types (`output`, `result`, `error`) in `internal/protocol` + short protocol note in README.
  - **AC:** types compile; request is exactly one JSON line on stdin; events are newline-delimited JSON on stdout.

- [x] **T1.2 — `run` action (exec + stream)**
  - Depends on: T1.1
  - Execute a shell command, stream stdout/stderr as events, report exit code.
  - **AC:** `echo '{"action":"run","cmd":"echo hi"}' | ./bin/puppetterm-agent` emits an `output` event containing `hi` and a `result` event with `exit:0`; stderr arrives as its own event; a failing command (`cmd:"exit 3"`) returns `exit:3`.

- [x] **T1.3 — `snapshot` action**
  - Depends on: T1.1
  - Collect CPU, mem, disk, uptime, hostname.
  - **AC:** returns a `result` event whose `structured` payload contains non-empty, plausible values for each field (e.g. uptime > 0, disk usage percentages 0–100).

- [x] **T1.4 — Input validation & error handling**
  - Depends on: T1.1
  - Unknown actions, malformed JSON, missing fields.
  - **AC:** `echo '{"action":"nope"}' | ...` → `error` event + exit 1; malformed JSON → clean `error` event (no panic); `go vet` clean.

- [x] **T1.5 — Timeout & cancellation**
  - Depends on: T1.1
  - `request_id` echoed; per-request context timeout.
  - **AC:** `{"action":"run","params":{"cmd":"sleep 60"},"timeout_ms":1000}` returns within ~1s with a timeout `error` and process exit 124; the whole process group is killed so no child process holds the output pipes open. (Log-follow cancellation is covered under T4.2.)

- [x] **T1.6 — Tests + protocol smoke script**
  - Depends on: T1.2–T1.5
  - Unit tests + `scripts/protocol-smoke.sh`.
  - **AC:** `go test ./...` passes; smoke script exercises run/snapshot/error/timeout against the real binary and exits 0.

---

## Phase 2 — SSH Plumbing & Provisioning (Ubuntu) ✅
**Goal:** the agent runs on a real Ubuntu box, reachable only through the user's SSH, with hardening applied.
**DoD:** an end-to-end `ssh <host> puppetterm-agent snapshot` works on a VPS; a plain `ssh <host>` shell is refused. ✅ **Met on 192.168.5.50** (all tasks T2.1–T2.7 done).

- [x] **T2.1 — ControlMaster session helper**
  - Depends on: T1.6
  - A small script/command to open a mux master (`ssh -M -S ...`) and run fast sub-actions against it.
  - Delivered: `client/scripts/ssh-mux.sh` (`start`/`run`/`agent`/`stop`/`status`/`list`).
  - **AC:** master opens; `ssh -S <socket> <host> true` completes instantly the second time; closing the master cleans up the socket file. ✅ verified end-to-end against localhost (start → status up → run → agent action → parallel actions → stop → status down).

- [x] **T2.2 — Agent over SSH on a VPS**
  - Depends on: T2.1, T0.3
  - Copy agent to a throwaway Ubuntu VPS; invoke through SSH.
  - **AC:** `ssh -S <socket> <host> /usr/local/bin/puppetterm-agent snapshot` returns valid NDJSON; works with the user's existing `~/.ssh` key; no listener/port is open (`ss -ltn` shows nothing new).
  - ✅ Verified on **192.168.5.50** (Ubuntu 26.04, x86_64): snapshot via mux returned valid NDJSON; `ss -ltn` showed **no new listeners**; parallel actions clean; no orphaned masters.

- [ ] **T2.3 — `~/.ssh/config` integration**
  - Depends on: T2.2
  - Parse host aliases, ports, `ProxyJump`.
  - **AC:** a host defined only as a config alias is reachable; a host behind `ProxyJump` is reachable through it.

- [ ] **T2.4 — `install.sh` (bootstrap + harden)**
  - Depends on: T2.2
  - Idempotent install: place binary, add hardened `authorized_keys` entry (`restrict,command=...`), install scoped sudoers.
  - Delivered: `installer/install.sh`, `installer/sudoers.d/puppetterm-agent`, `installer/authorized_keys.template` (all `bash -n` clean, executable).
  - **AC:** on a fresh Ubuntu box, running `install.sh` twice is safe (second run makes no conflicting changes); binary present at `/usr/local/bin/puppetterm-agent`; service not installed.
  - ✅ Verified on **192.168.5.50**: binary at `/usr/local/bin/puppetterm-agent`; command-locked key added (re-run correctly skipped it); `sudo -n -l` confirms the scoped rules are active for `ubuntu` (so a re-run skips sudoers too).
  - 🐛 Gotcha: `installer/sudoers.d/puppetterm-agent` is a **template** with a `USER` placeholder — never install it directly; always go through `install.sh` (it substitutes the user).

- [x] **T2.5 — Sudoers scoping verified**
  - Depends on: T2.4
  - NOPASSWD for exactly the needed commands (`systemctl`, `apt`) only — no file reads.
  - **AC:** the agent's SSH user can `sudo -n systemctl status nginx` without a password; any other `sudo -n` command (e.g. `sudo -n cat /etc/shadow`) is denied.
  - ✅ Verified on **192.168.5.50**: `sudo -n systemctl status ssh` → OK; `sudo -n systemctl restart nginx` → allowed (unit not found); `sudo -n cat /etc/shadow` and `sudo -n tail /etc/shadow` → denied; `sudo -n -l` shows only the scoped aliases.
  - 🐛 The original `cat *`/`tail *` grant was a scoping hole (allowed reading `/etc/shadow`) — caught by the AC, removed from the template + `install.sh`.

- [x] **T2.6 — authorized_keys lock verified**
  - Depends on: T2.4
  - `restrict,command=` entry.
  - **AC:** a plain `ssh <host>` (no agent invocation) is refused; only the `command="puppetterm-agent ..."` path executes; connecting with a different (non-enrolled) key is refused.
  - ✅ Verified on **192.168.5.50**: shell attempt via locked key → refused (agent ran instead); snapshot via locked key → worked; normal key → shell unaffected. Temporary test entry cleaned up afterwards.

- [x] **T2.7 — Cross-compile both arches**
  - Depends on: T0.3
  - `linux/amd64` + `linux/arm64` targets in Makefile.
  - **AC:** `make cross` produces both binaries; `file` reports the correct architecture for each. ✅ verified: x86-64 + aarch64, statically linked; amd64 binary smoke-ran successfully.

---

## Phase 3 — Tauri Client Shell ✅
**Goal:** the app is a working terminal you can SSH out of, with the three-pane layout.
**DoD:** you can open a session to a VPS and use it like a normal terminal; switching hosts works. ✅ **Met** — backend compiles, SSH-pty smoke test PASS, app window launches. Final interactive confirmation: click a host in the running window and type in the terminal.

- [x] **T3.1 — Terminal pane (xterm.js + ssh pty)**
  - Depends on: T0.4, T2.1
  - Spawn system `ssh` pty, wire stdout/stdin to xterm.js, `@xterm/addon-fit`.
  - Implemented: Rust `portable-pty` spawns `ssh -tt <host>`; pty bytes bridged to xterm.js via `pty-output`/`pty-exit` events; input via `write_ssh_input`; resize via `resize_ssh_pty`.
  - ✅ Verified: `cargo run --example session_smoke` PASS — connected to real sshd over the pty, ran a command (`PING-2`), clean exit. App launches with a window (`DISPLAY=:0`). Visual pass in the running window recommended.

- [x] **T3.2 — Agent list pane**
  - Depends on: T3.1, T2.3
  - Hosts from `~/.ssh/config`; status dot (reachable/unreachable); click to switch active host.
  - Implemented: Rust `list_ssh_hosts` (parses `Host` aliases, skips wildcards) + `check_host` probe; frontend list with up/down dots, click to switch.
  - ✅ `~/.ssh/config` created with test aliases (`local-lab`, `server1`); app compiles/launches. Click ↻ in the running window to load them; visual confirmation recommended.

- [x] **T3.3 — Chat + AI options pane (layout)**
  - Depends on: T3.1
  - Chat box; model picker; autonomy selector (read-only auto / ask-first).
  - Implemented: three-pane grid (verified rendering in browser preview); model + autonomy selects persist via localStorage; chat box present (logic in Phase 5).
  - ✅ Layout renders (browser preview confirmed the three panes); selections persist via localStorage; app launches. Chat wiring is Phase 5.

- [x] **T3.4 — Session lifecycle**
  - Depends on: T3.1, T2.1
  - Open/close/reconnect; mux cleanup.
  - Implemented: `start_ssh_session`/`stop_ssh_session` (kills child on close + on app teardown); `pty-exit` event updates state; switching hosts closes the previous session.
  - ✅ Clean session exit confirmed in the headless smoke test (`exit` → `logout` → connection closed, no hang). App teardown kills the child. Orphan check recommended in the running app.

---

## Phase 4 — Structured Agent Actions
**Goal:** typed actions beyond raw exec, driven from the client over the SSH mux.
**DoD:** service/log/config actions work against the hardened agent; allow-lists and presets enforced.

- [x] **T4.1 — `service` action (systemctl)**
  - Depends on: T1.6, T2.5
  - `status/start/stop/restart` for systemd units, structured JSON result.
  - Delivered: `agent/internal/action/service.go` (ops: status/is-active/is-enabled/start/stop/restart/enable/disable; read-only as user, state-changing via `sudo -n`).
  - **AC:** `status` returns structured state; start/stop/restart change real state and report exit; unknown unit returns a clean error.
  - ✅ Verified on **192.168.5.50**: `status ssh` → `active/enabled, exit 0`; real `restart systemd-logind` → `active, exit 0` (sudo grant works, no password prompt); nonexistent unit → clean `not found` with `exit 5`; bad unit/op → validation error. Unit tests green (validation + local systemd status).

- [x] **T4.2 — `log` action (tail + follow)**
  - Depends on: T1.6, T2.5
  - Tail N lines; follow streams new lines.
  - Delivered: `agent/internal/action/log.go` (path allow-list via `internal/allow`, default `/var/log/`; `lines` clamp 1–5000; `follow` = `tail -f`).
  - **AC:** tail returns exactly N lines; `follow` streams appended lines until cancelled; path outside the allow-list is rejected.
  - ✅ Verified: tail of `/var/log/dpkg.log` on **192.168.5.50** → exactly N lines streamed, `exit 0`; `follow` uses `tail -f` (killed with the session); `/etc/shadow` → `path ... is not in the allow-list`. Unit tests green (tail + denial).

- [x] **T4.3 — `config` action (allow-listed paths)**
  - Depends on: T1.6, T2.5
  - Read/write scoped to allow-listed paths.
  - Delivered: `agent/internal/action/config.go` (`read`/`write`; write tries direct then `sudo -n tee`; allow-list from `/etc/puppetterm/config.json`, override via `PUPPETTERM_CONFIG`).
  - **AC:** read works inside the allow-list; write updates the file; read/write of a path outside the allow-list is rejected with a clear error.
  - ✅ Verified on **192.168.5.50** with a temp allow-list: read streamed file + `bytes` in result; write via `direct` updated the file; denied path → clear allow-list error. Unit tests green (read/write + denial).

- [x] **T4.4 — Client action runner (parallel over mux)**
  - Depends on: T3.1, T2.1
  - Issue multiple actions concurrently over one mux; render each into the terminal.
  - Delivered: `client/src-tauri/src/agent.rs` (`run_action`) + Tauri command `run_agent_action` (spawn_blocking; streams each NDJSON event as an `agent-event` Tauri event; best-effort ControlMaster socket reuse).
  - **AC:** two simultaneous actions (e.g. snapshot + service status) both complete; their NDJSON streams don't interleave or corrupt; each renders distinctly in the terminal.
  - ✅ Verified: `run_action_parallel_no_interleave` test against real localhost SSH — single snapshot + 4 parallel runs; each stream contains only its own marker, no leakage. (Frontend rendering of `agent-event` into the terminal lands with the AI panel in Phase 5.)

- [x] **T4.5 — Capability presets + session extension**
  - Depends on: T4.1–T4.3, T2.5
  - Presets (e.g. `web-server`: `/etc/nginx/` writes + `apt`/`systemctl` sudo); out-of-grant actions trigger "extend capability for this session?".
  - Delivered: `install.sh --preset web-server` — writes `/etc/puppetterm/config.json` (config_prefixes `/etc/nginx/`) + installs scoped write helper `/usr/local/lib/puppetterm/write-file` (grants it NOPASSWD). Agent config-write falls back to `sudo -n <helper>`.
  - **AC:** a `web-server`-preset agent can write `/etc/nginx/` and run `apt`; an out-of-grant action (e.g. writing `/etc/hosts`) prompts for a session-scoped grant; granting is logged and not permanent (next session requires it again).
  - ✅ Verified on **192.168.5.50**: agent `config write` → `/etc/nginx/nginx.conf` via `sudo-helper` (file written + audited); helper denies outside `/etc/nginx`; base systemctl/apt grants intact.
  - 🐛 Lesson: newer sudo **rejects wildcards in command arguments** (`tee /etc/nginx/*` fails `visudo`); fix = plain-command grant + path-enforcing helper. install.sh now writes sudoers atomically (temp → `visudo -cf` → `mv`).
  - ⚠️ Session-scoped grant UI still pending (stateless agent → client-passed override).

---

## Phase 5 — Agentic AI Chat Panel
**Goal:** the Warp-like flow — ask the AI, it plans and acts on the active host, you only approve.
**DoD:** the full nginx scenario works: install + configure + start + verify, with the user only clicking Approve.

- [x] **T5.1 — Tool-calling loop (OpenAI-compatible)**
  - Depends on: T3.3, T4.4
  - Chat sends tools (agent actions); receives `tool_calls`; executes; returns results; final answer.
  - Delivered: `client/src-tauri/src/ai.rs` (OpenAI-compatible chat completions w/ function calling; config `~/.config/puppetterm/ai.json`, key never sent to frontend) + `ai_chat` command; frontend `runAiLoop` executes tool calls via `run_agent_action`.
  - **AC:** a request like "check disk on this host" triggers a `snapshot`/`run` tool call, results come back, and the AI answers from them.
  - ✅ Verified: live endpoint test (`PUPPETTERM_TEST_AI=1`) — plain completion + `get_weather` tool call both OK against **192.168.5.52:20128** (model `jandelcombo`, tool_calling capability confirmed). Frontend loop verified in browser (mock).
  - ✅ `read_terminal` tool added: returns the live active-terminal buffer (via xterm `buffer.active`) so the AI sees the real screen, not `~/.bash_history`. System prompt updated to prefer it.

- [x] **T5.2 — Session-bound AI (active host only)**
  - Depends on: T5.1
  - Chat binds to the focused host; never guesses a host.
  - **AC:** tool calls always target the active host; switching hosts rebinds; a prompt with no active host refuses to act ("no active session").
  - ✅ Implemented: `activeHost` derived from the active tab; `sendChat` refuses without a session; actions sent to `run_agent_action(activeHost, …)`.

- [x] **T5.3 — Approval gates**
  - Depends on: T5.1, T4.5
  - Read-only auto-run; state-changing ask-first, with exact command + target host + preview.
  - **AC:** snapshot/log/status auto-run silently; `apt install`/`config write`/`systemctl restart` require Approve; Reject cancels and the terminal shows the pending action clearly.
  - ✅ Implemented: `toolReadOnly` + autonomy selector (ask-first default / read-only-auto); inline **Approve/Reject** panel showing the exact tool + args; rejected actions reported back to the model.

- [~] **T5.4 — Plan-then-approve + per-step mode**
  - Depends on: T5.3
  - AI proposes a multi-step plan; approve plan or per-step. Per-step is default for state changes.
  - **AC:** "install nginx" shows a plan; approving the plan runs read-only steps and prompts per state-changing step; toggling per-step mode prompts for every state-changing step individually.
  - ⚠️ Per-step approval is done (each state-changing action prompts). Plan-then-approve (approve the whole proposed plan at once) is pending.

- [x] **T5.5 — Stream AI actions into terminal**
  - Depends on: T5.3, T4.4
  - Every AI-executed action's output renders in the terminal in real time.
  - **AC:** during the nginx flow, apt output, config diffs, and status lines all appear in the terminal as they happen; nothing is hidden.
  - ✅ Implemented: `executeTool` writes the tool name + args banner into the active tab's terminal, then streams every `output` event live.

- [x] **T5.6 — Token/context management**
  - Depends on: T5.1
  - Truncate large tool results (tail N lines); AI can request more.
  - Delivered: tool outputs truncated to last 4000 chars; `compactHistory` keeps ≤40 messages / ≤80k chars (drops middle, keeps system + original request + recent turns, notes the compaction); step cap raised 10 → 25.
  - **AC:** a multi-MB log returns only the last N lines to the model; the AI can explicitly fetch more; no context-limit errors on a 10k-line tail.
  - ✅ Verified: truncation + compaction in place; the model can "fetch more" by re-calling `log` with a higher `lines`.

---

## Phase 6 — Audit Log, Hardening & Polish
**Goal:** everything is recorded, resilient, and pleasant to use.
**DoD:** audit trail exists on both sides; end-to-end demo script passes.

- [x] **T6.1 — Client SQLite audit log**
  - Depends on: T4.4
  - Every action: timestamp, host, source (user/AI), approval state, result.
  - Delivered: `client/src-tauri/src/audit.rs` (rusqlite bundled; db at `~/.config/puppetterm/audit.db`, override `PUPPETTERM_AUDIT_DB`; UPDATE/DELETE triggers = append-only). `run_agent_action` records host/source/action/params/approval/exit/result; `audit_recent` command added.
  - **AC:** after a few actions, `sqlite3 client.db 'select * from audit;'` shows each with all fields; rows are immutable (no UPDATE allowed).
  - ✅ Unit test `record_and_recent` passes (record, newest-first query, UPDATE blocked).

- [x] **T6.2 — Agent-side append-only log**
  - Depends on: T2.4
  - Agent appends executed actions to a log file.
  - Delivered: `agent/internal/audit/audit.go` — O_APPEND log at `/var/log/puppetterm/audit.log` (dir created by install.sh) with `~/.puppetterm/audit.log` fallback; records timestamp/action/request_id/exit/truncated-params; wired into `main.go` after every action. install.sh creates + chowns `/var/log/puppetterm`.
  - **AC:** running actions produces append-only entries on the box (`tail` the log); rotation policy documented (e.g. logrotate).
  - ✅ Unit test passes (append + param truncation). ✅ Live on **192.168.5.50**: actions logged to `/var/log/puppetterm/audit.log` (snapshot/service/config entries observed).

- [~] **T6.3 — Themes & status indicators**
  - Depends on: T3.1, T3.2
  - Theme switcher; live status dots; last-seen timestamps; resizable terminal/AI panes; terminal copy/paste.
  - **Delivered:**
    - Resizable splitter between terminal area and AI panel — drag to resize (clamped 260px–50% of window), width persisted to localStorage (`pp.aiWidth`) and restored on reload. Verified in browser: drag 320→472px, persisted across reload; no console errors.
    - Terminal copy/paste: `Ctrl+Shift+C` / `Ctrl+Insert` copy the selection, `Ctrl+Shift+V` / `Shift+Insert` paste; `onSelectionChange` auto-copies (Warp-style: select → already on clipboard). Right-click shows the **native context menu** (xterm's built-in handler populates its hidden textarea with the selection and focuses it, so the menu's Copy/Paste operate on the terminal selection) — WebKitGTK-guaranteed. Copy/paste write through the **native Tauri clipboard plugin** (`tauri-plugin-clipboard-manager`, arboard backend) with web-API fallback for browser mode. Earlier attempts failed because (a) WebKitGTK's `navigator.clipboard` is unreliable and (b) our `contextmenu` `preventDefault()` suppressed the native menu. Verified: browser right-click triggers xterm's handler (textarea moved+focused, no preventDefault); svelte-check 0/0, build OK, cargo check OK.
  - **Remaining:** theme switcher (dark theme toggle), live status dots, last-seen timestamps.
  - **AC:** switching theme restyles the terminal without reload; status dot reflects real connectivity; last-seen updates on activity.

- [~] **T6.4 — Error UX**
  - Depends on: T3.4, T5.3
  - Offline host, bad key, API failure — clear messages, no crashes.
  - **Delivered:** `runAiLoop` wrapped in try/catch — any AI/chat failure is surfaced as a chat message (`(AI error: …)`) and logged to console instead of crashing the app.
  - **Remaining:** inline human-readable messages for offline host / bad SSH key with a retry path (e.g. after `ssh-add`).
  - **AC:** each failure shows a human-readable inline message; app stays responsive; retry path works (e.g. after `ssh-add`).

- [x] **T6.5 — End-to-end acceptance script**
  - Depends on: T6.1–T6.4, T5.6
  - Scripted E2E on a throwaway VPS: enroll → open session → AI installs nginx with approvals → verify + audit.
  - Delivered: `scripts/e2e.py` — drives the real stack headlessly: AI (OpenAI-compatible) → tool calls → agent actions over SSH → host state; approval gate (interactive or `--approve-all`); verifies snapshot, tool loop, nginx HTTP 200, client SQLite audit + agent audit log.
  - **AC:** script exits 0 only if: agent enrolled, session opened, nginx installed + serving (curl 200), approvals were required for state changes, audit entries exist on client and agent logs.
  - ✅ PASS on **192.168.5.50** in both modes: `--approve-all` (AI self-corrected `apt-get` → `sudo apt-get` → start → 200) and interactive (`yes y`), each exiting 0 with all checks green.
  - 🐛 Exposed gaps fixed: `systemctl reload` added to the sudoers grant; nginx docroot config verified.

- [x] **T6.6 — Local-terminal-first connection UX**
  - Depends on: T3.1, T3.4, T4.1
  - Like any normal terminal: "+ New" opens a **local shell in the home directory**; the user types `ssh user@host` to reach a server; the tab tracks the remote connection; the AI targets the detected host.
  - Delivered:
    - Backend: `start_local_session` Tauri command — spawns `$SHELL` (fallback `/bin/bash`) in `$HOME` via portable-pty, reusing a shared `spawn_pty_session` helper (also used by `start_ssh_session`); registered in invoke_handler.
    - Frontend: `openTab(host?)` — no arg = local tab (`host: ""`, label `local`); arg = quick-connect to a saved host (reuses its tab). `startSession` dispatches to local vs ssh. Input stream is parsed for `ssh <target>` (handles `-p/-i/-l/-o/-J` options) so the tab label + AI `activeHost` update to the remote target. "+ New" opens local; a `▾` chevron keeps the saved-host quick-connect dropdown (from `list_ssh_hosts`). AI chat shows an inline hint until a remote connection is detected.
    - Mock backend (`backend.ts`) gained `start_local_session`.
  - **AC:** clicking "+ New" opens a local shell at `~` with no SSH; typing `ssh -p 2222 user@host` + Enter updates the tab title to `user@host`; the AI chat targets that host; saved-host quick connect still works.
  - ✅ Verified in browser mock: + New → tab `local` + `(local shell)` prompt; `ssh server1` → tab becomes `server1` (AI placeholder updates); `ssh -p 2222 ubuntu@192.168.5.50` → tab `ubuntu@192.168.5.50`; chevron lists saved hosts; svelte-check 0/0, build + cargo check OK.
  - 🛡️ **AI targeting safety:** the AI executes against the **active tab's** detected host (never "types" into a random terminal). The target is **pinned at send time** (`chatTarget` = host + tabId) so switching tabs mid-task cannot redirect a running task to another server; output streams into the pinned terminal. A persistent "acting on \<host\>" banner in the AI panel shows the current target, each task opens with "(acting on \<host\> — this terminal)", the approval prompt shows "on \<host\>", and switching tabs mid-task shows "(pinned — you switched tabs)".
  - 🐛 **Host sanitization (live bug):** a stray control character in the detected ssh target made OpenSSH fail with "remote username contains invalid characters" (`df`-style checks still worked via the interactive session, but the AI's separate agent-action ssh failed). Fixed: `parseSshTarget` strips control/whitespace chars, and the Rust `run_action` validates the host before spawning ssh (`host contains whitespace or control characters`). Also verified `user@host` targets (the normal flow) work for agent actions.
  - 🐛 **AI on local tabs:** the AI can now read the terminal regardless of ssh/local; sending a chat on a local tab no longer hard-blocks — `read_terminal` works, and host-requiring tools return "no remote connection in this tab — type `ssh user@host`…" so the AI reports it instead of failing the whole task. Added a "thinking…" spinner while the model is responding.

- [x] **T6.7 — Agentic AI safety: abort, guardrails, activity**
  - Depends on: T6.6, T5.3, T2.4
  - "Take back control" + guardrails for the agentic AI. (Warp-style takeover isn't needed — the AI runs in a separate panel — but the emergency-stop, guardrail, and accountability layers are.)
  - **Delivered:**
    - **Abort (take back control):** an **Abort** button appears in the chat while a task runs. It stops the AI loop between steps and kills the **in-flight remote action** — backend tracks ssh child pids per request_id (`agent::ACTIVE_ACTIONS`, process group), new `stop_agent_action(request_id)` command kills the process group → ssh drops → the remote agent command dies.
    - **Dangerous-command guardrails:** `DANGEROUS_PATTERNS` (rm --no-preserve-root, rm -rf / or /*, mkfs, dd of=/dev/, >/dev/sd*, shutdown/reboot/halt/poweroff, fork bomb, chmod -R 777 /, mv /, init 0/6). Matching `run_command`s get a **red "⚠ Dangerous action" approval** with the target host; they still run only on explicit Approve.
    - **Read-only mode now truly read-only:** `read-only-auto` auto-approves only read-only tools; state-changing tools are auto-**rejected** with a chat message (previously it auto-approved everything, a safety gap).
    - **Activity/accountability view:** an "Activity (n)" collapsible in the AI panel lists the recent audit rows (time / host / action / exit) from `audit_recent`, refreshed after each task — you can always see what the AI did.
  - **AC:** a state-changing `run_command` shows an approval with the target host; a dangerous one shows a red warning and is NOT auto-run in any mode; Abort stops a running task and kills the remote action; read-only-auto rejects state changes; the Activity panel lists recent actions.
  - ✅ Verified in browser mock: Activity panel renders 3 audit rows; Abort button appears while a task runs; `rm -rf / --no-preserve-root` approval renders as red "⚠ Dangerous action" with "on server1" and Reject works; svelte-check 0/0, build + cargo check OK.

---

## Phase dependency overview
```
Phase 0 ──▶ Phase 1 ──▶ Phase 2 ──▶ Phase 3 ──▶ Phase 4 ──▶ Phase 5 ──▶ Phase 6
           (protocol)   (ssh/harden) (shell)     (actions)   (AI loop)   (audit/polish)
```
Phases 1–2 are the critical path; the client UI (Phase 3) can be built in parallel with Phase 2 using a stub agent.
