# puppetterm installer

Bootstrap + hardening for Ubuntu boxes:
- `ssh-copy-id` for key enrollment
- install `puppetterm-agent` (.deb or static binary)
- scoped `sudoers.d/` entry
- `authorized_keys` entry locked with `restrict,command=...`

**Status:** not started — Phase 2 of `TASKS.md`.
