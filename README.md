# puppetterm

A lightweight, Warp-like desktop terminal for managing Ubuntu servers over your
**existing SSH setup**, with an agentic AI chat that only acts after you approve.

- Design: [`agentic-remote-terminal-plan.txt`](./agentic-remote-terminal-plan.txt)
- Task tracker: [`TASKS.md`](./TASKS.md)

## Layout

| Path | What it is |
|---|---|
| `agent/` | Go — stateless remote worker (`puppetterm-agent`), invoked through SSH. No listener, no daemon. |
| `client/` | Tauri desktop app — terminal pane (xterm.js) + AI chat panel. |
| `installer/` | `ssh-copy-id` bootstrap + agent install + sudoers/`authorized_keys` hardening. |

## Status

- **Phase 0** (scaffold) — done.
- **Phase 1** (agent MVP) — done: `run` + `snapshot` over NDJSON, unit tests + smoke green.
- **Phase 2** (SSH plumbing & provisioning) — **done**, verified end-to-end on a real Ubuntu box (192.168.5.50): ControlMaster mux, hardened install (`install.sh`), scoped sudoers (T2.5 AC caught + fixed a `cat *` hole), command-locked `authorized_keys`, cross-compile amd64/arm64.
- Next: **Phase 3** (Tauri client shell) — pending Rust toolchain.
- Go toolchain: installed locally at `~/.local/go` — add to PATH with `export PATH="$HOME/.local/go/bin:$PATH"`.

## Quick start (agent)

```bash
cd agent
make build                 # build bin/puppetterm-agent
make test                  # unit tests
make smoke                 # end-to-end protocol smoke test
printf '%s' '{"action":"snapshot"}' | bin/puppetterm-agent
```
