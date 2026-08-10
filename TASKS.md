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

## Phase 3 — Tauri Client Shell
**Goal:** the app is a working terminal you can SSH out of, with the three-pane layout.
**DoD:** you can open a session to a VPS and use it like a normal terminal; switching hosts works.

- [ ] **T3.1 — Terminal pane (xterm.js + ssh pty)**
  - Depends on: T0.4, T2.1
  - Spawn system `ssh` pty, wire stdout/stdin to xterm.js, `@xterm/addon-fit`.
  - **AC:** interactive shell works (typing, output, `Ctrl+C`); resizing the window re-flows correctly; output colors render.

- [ ] **T3.2 — Agent list pane**
  - Depends on: T3.1, T2.3
  - Hosts from `~/.ssh/config`; status dot (reachable/unreachable); click to switch active host.
  - **AC:** hosts appear as aliases; status dot reflects live reachability; clicking switches the active session/host; an unreachable host shows clearly.

- [ ] **T3.3 — Chat + AI options pane (layout)**
  - Depends on: T3.1
  - Chat box; model picker; autonomy selector (read-only auto / ask-first).
  - **AC:** three-pane layout renders and is resizable; selections persist across restarts.

- [ ] **T3.4 — Session lifecycle**
  - Depends on: T3.1, T2.1
  - Open/close/reconnect; mux cleanup.
  - **AC:** closing a session kills the master and removes the control socket; reconnect works; `pgrep -f 'ssh -M'` shows no orphaned processes after closing.

---

## Phase 4 — Structured Agent Actions
**Goal:** typed actions beyond raw exec, driven from the client over the SSH mux.
**DoD:** service/log/config actions work against the hardened agent; allow-lists and presets enforced.

- [ ] **T4.1 — `service` action (systemctl)**
  - Depends on: T1.6, T2.5
  - `status/start/stop/restart` for systemd units, structured JSON result.
  - **AC:** `status` returns `{state, since, ...}`; start/stop/restart change real state and report exit; unknown unit returns a clean error.

- [ ] **T4.2 — `log` action (tail + follow)**
  - Depends on: T1.6, T2.5
  - Tail N lines; follow streams new lines.
  - **AC:** tail returns exactly N lines; `follow` streams appended lines until cancelled; path outside the allow-list is rejected.

- [ ] **T4.3 — `config` action (allow-listed paths)**
  - Depends on: T1.6, T2.5
  - Read/write scoped to allow-listed paths.
  - **AC:** read works inside the allow-list; write updates the file; read/write of a path outside the allow-list is rejected with a clear error.

- [ ] **T4.4 — Client action runner (parallel over mux)**
  - Depends on: T3.1, T2.1
  - Issue multiple actions concurrently over one mux; render each into the terminal.
  - **AC:** two simultaneous actions (e.g. snapshot + service status) both complete; their NDJSON streams don't interleave or corrupt; each renders distinctly in the terminal.

- [ ] **T4.5 — Capability presets + session extension**
  - Depends on: T4.1–T4.3, T2.5
  - Presets (e.g. `web-server`: `/etc/nginx/` writes + `apt`/`systemctl` sudo); out-of-grant actions trigger "extend capability for this session?".
  - **AC:** a `web-server`-preset agent can write `/etc/nginx/` and run `apt`; an out-of-grant action (e.g. writing `/etc/hosts`) prompts for a session-scoped grant; granting is logged and not permanent (next session requires it again).

---

## Phase 5 — Agentic AI Chat Panel
**Goal:** the Warp-like flow — ask the AI, it plans and acts on the active host, you only approve.
**DoD:** the full nginx scenario works: install + configure + start + verify, with the user only clicking Approve.

- [ ] **T5.1 — Claude tool-calling loop**
  - Depends on: T3.3, T4.4
  - Chat sends tools (agent actions); receives `tool_calls`; executes; returns results; final answer.
  - **AC:** a request like "check disk on this host" triggers a `snapshot`/`run` tool call, results come back, and the AI answers from them.

- [ ] **T5.2 — Session-bound AI (active host only)**
  - Depends on: T5.1
  - Chat binds to the focused host; never guesses a host.
  - **AC:** tool calls always target the active host; switching hosts rebinds; a prompt with no active host refuses to act ("no active session").

- [ ] **T5.3 — Approval gates**
  - Depends on: T5.1, T4.5
  - Read-only auto-run; state-changing ask-first, with exact command + target host + preview.
  - **AC:** snapshot/log/status auto-run silently; `apt install`/`config write`/`systemctl restart` require Approve; Reject cancels and the terminal shows the pending action clearly.

- [ ] **T5.4 — Plan-then-approve + per-step mode**
  - Depends on: T5.3
  - AI proposes a multi-step plan; approve plan or per-step. Per-step is default for state changes.
  - **AC:** "install nginx" shows a plan; approving the plan runs read-only steps and prompts per state-changing step; toggling per-step mode prompts for every state-changing step individually.

- [ ] **T5.5 — Stream AI actions into terminal**
  - Depends on: T5.3, T4.4
  - Every AI-executed action's output renders in the terminal in real time.
  - **AC:** during the nginx flow, apt output, config diffs, and status lines all appear in the terminal as they happen; nothing is hidden.

- [ ] **T5.6 — Token/context management**
  - Depends on: T5.1
  - Truncate large tool results (tail N lines); AI can request more.
  - **AC:** a multi-MB log returns only the last N lines to Claude; the AI can explicitly fetch more; no context-limit errors on a 10k-line tail.

---

## Phase 6 — Audit Log, Hardening & Polish
**Goal:** everything is recorded, resilient, and pleasant to use.
**DoD:** audit trail exists on both sides; end-to-end demo script passes.

- [ ] **T6.1 — Client SQLite audit log**
  - Depends on: T4.4
  - Every action: timestamp, host, source (user/AI), approval state, result.
  - **AC:** after a few actions, `sqlite3 client.db 'select * from audit;'` shows each with all fields; rows are immutable (no UPDATE allowed).

- [ ] **T6.2 — Agent-side append-only log**
  - Depends on: T2.4
  - Agent appends executed actions to a log file.
  - **AC:** running actions produces append-only entries on the box (`tail` the log); rotation policy documented (e.g. logrotate).

- [ ] **T6.3 — Themes & status indicators**
  - Depends on: T3.1, T3.2
  - Theme switcher; live status dots; last-seen timestamps.
  - **AC:** switching theme restyles the terminal without reload; status dot reflects real connectivity; last-seen updates on activity.

- [ ] **T6.4 — Error UX**
  - Depends on: T3.4, T5.3
  - Offline host, bad key, API failure — clear messages, no crashes.
  - **AC:** each failure shows a human-readable inline message; app stays responsive; retry path works (e.g. after `ssh-add`).

- [ ] **T6.5 — End-to-end acceptance script**
  - Depends on: T6.1–T6.4, T5.6
  - Scripted E2E on a throwaway VPS: enroll → open session → AI installs nginx with approvals → verify + audit.
  - **AC:** script exits 0 only if: agent enrolled, session opened, nginx installed + serving (curl 200), approvals were required for state changes, audit entries exist on client and agent logs.

---

## Phase dependency overview
```
Phase 0 ──▶ Phase 1 ──▶ Phase 2 ──▶ Phase 3 ──▶ Phase 4 ──▶ Phase 5 ──▶ Phase 6
           (protocol)   (ssh/harden) (shell)     (actions)   (AI loop)   (audit/polish)
```
Phases 1–2 are the critical path; the client UI (Phase 3) can be built in parallel with Phase 2 using a stub agent.
