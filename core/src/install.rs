//! In-app agent installer — installs `puppetterm-agent` on a remote host over
//! the user's existing SSH keys, without requiring a password or sudo.
//!
//! Strategy:
//! 1. Always install a **user-space** agent (binary + command-locked key +
//!    config under `~/.snap/app/puppetterm/`). No sudo, works with the existing
//!    key. The agent binary is world-executable so the command-locked SSH entry
//!    can invoke it as the login user.
//! 2. If the `installer/install.sh` payload is available locally, ALSO upgrade
//!    to full privileges (scoped sudoers, /etc config, /var/log audit dir),
//!    giving the agent state-changing capabilities.
//!
//! Permissions on the host (user-owned via `~/.snap/app/puppetterm/`):
//!   - `~/.snap/app/puppetterm/bin`  0755 (traversable + executable by user)
//!   - `puppetterm-agent` binary     0755 (world-exec — agent runs as the SSH
//!     user via the command-locked key)
//!   - `config.json`                 0644 (readable by user)

use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use serde::Serialize;

#[derive(Debug, Serialize)]
pub struct InstallResult {
    pub host: String,
    pub arch: String,
    pub agent_path: String,
    pub mode: String, // "user" | "root"
    pub sudoers: bool,
    pub already: bool, // agent was already present (idempotent re-run)
}

/// True if the agent binary is already reachable on the host (either the
/// `~/.snap/app/puppetterm/bin` user-space path or a legacy system path).
///
/// Uses the SAME `sh -c` probe as `agent::resolve_agent_bin` so the badge can
/// never disagree with what `run_action` will actually find. The probe runs
/// through the user's LOGIN shell, so it must be POSIX-safe (no `~` expansion
/// dependence, no deprecated `test -o`) — the badge could otherwise claim
/// "agent mode" while `resolve_agent_bin` reported the binary missing and the
/// AI fell back to typing into the live terminal.
pub fn check_agent(host: &str) -> bool {
    let mut cmd = Command::new("ssh");
    cmd.args(["-o", "BatchMode=yes", "-o", "ConnectTimeout=8"]);
    crate::ssh::attach_control(&mut cmd, host);
    crate::ssh::ssh_host(&mut cmd, host);
    let out = cmd
        .arg(
            "sh -c 'for p in \"$HOME/.snap/app/puppetterm/bin/puppetterm-agent\" /var/local/puppetterm/bin/puppetterm-agent /usr/local/bin/puppetterm-agent; do [ -x \"$p\" ] && exit 0; done; exit 1'",
        )
        .output();
    matches!(out, Ok(o) if o.status.success())
}

/// Run a remote command over SSH, optionally feeding stdin, streaming stdout
/// lines through `emit`. Returns (exit_code, stdout_text).
///
/// Flaky name resolution is handled transparently: on a transient
/// resolution/connect failure the command is retried — first against the
/// concrete `HostName`/`User`/`Port`/`ProxyJump` the alias maps to in
/// ~/.ssh/config (`ssh -G`, no DNS), then with a short backoff against the
/// alias itself.
fn ssh_io(
    host: &str,
    remote: &[&str],
    stdin_data: Option<&[u8]>,
    emit: &dyn Fn(&str),
) -> Result<(i32, String), String> {
    // ControlMaster socket stays keyed on the ORIGINAL alias so a live session
    // over that alias is still reused even when a retry targets a concrete IP.
    let control_host = host.to_string();
    // Config-derived concrete target, computed once (when it exists).
    let resolved: Option<(String, Option<String>)> = crate::ssh::config_resolve(host)
        .map(|(user, h, port, jump)| {
            let target = if user.is_empty() {
                format!("{h}:{port}")
            } else {
                format!("{user}@{h}:{port}")
            };
            (target, jump)
        });

    let mut target = host.to_string();
    let mut attachment_args: Vec<String> = Vec::new();
    // If DNS was already resolved for this host, start straight on the IP —
    // later install commands stay DNS-free even when resolution is flaky.
    if let Some(ip) = crate::ssh::dns_cached(host) {
        target = crate::ssh::ip_target(host, ip);
        let (full, _) = crate::ssh::split_ssh_host(host);
        let alias = full.rsplit('@').next().unwrap_or(&full).to_string();
        attachment_args.push("-o".to_string());
        attachment_args.push(format!("HostKeyAlias={alias}"));
    }
    let mut trying_resolved = false;
    let mut tried_dns = false;
    let mut attempt = 0;
    loop {
        let mut cmd = Command::new("ssh");
        cmd.args(["-o", "BatchMode=yes", "-o", "ConnectTimeout=10"]);
        crate::ssh::attach_control(&mut cmd, &control_host);
        for a in &attachment_args {
            cmd.arg(a);
        }
        crate::ssh::ssh_host(&mut cmd, &target);
        cmd.args(remote);
        cmd.stdin(Stdio::piped()).stdout(Stdio::piped()).stderr(Stdio::piped());
        let mut child = cmd.spawn().map_err(|e| format!("spawn ssh: {e}"))?;
        if let Some(mut si) = child.stdin.take() {
            if let Some(d) = stdin_data {
                let _ = si.write_all(d);
            }
        }

        let mut out = String::new();
        if let Some(mut so) = child.stdout.take() {
            let mut buf = [0u8; 8192];
            loop {
                match so.read(&mut buf) {
                    Ok(0) | Err(_) => break,
                    Ok(n) => {
                        let s = String::from_utf8_lossy(&buf[..n]).to_string();
                        for line in s.lines() {
                            if !line.trim().is_empty() {
                                emit(line);
                            }
                        }
                        out.push_str(&s);
                    }
                }
            }
        }
        let mut err = String::new();
        if let Some(mut se) = child.stderr.take() {
            let _ = se.read_to_string(&mut err);
        }
        let status = child.wait().map_err(|e| format!("wait: {e}"))?;
        let code = status.code().unwrap_or(-1);
        if code == 0 {
            return Ok((code, out));
        }

        let msg = err.trim();
        let transient = crate::ssh::is_transient_ssh_error(msg);
        attempt += 1;
        // 1st fallback: full config re-resolution (bypasses DNS entirely).
        if transient && !trying_resolved {
            if let Some((t, jump)) = &resolved {
                trying_resolved = true;
                target = t.clone();
                if let Some(j) = jump {
                    attachment_args.push("-J".to_string());
                    attachment_args.push(j.clone());
                }
                std::thread::sleep(std::time::Duration::from_millis(150));
                continue;
            }
        }
        // 2nd fallback: resolve the alias once via DNS and connect to the IP
        // directly (cached process-wide). HostKeyAlias keeps known_hosts keyed
        // on the alias, matching the entry the interactive session wrote.
        if transient && !tried_dns {
            if let Some(ip) = crate::ssh::dns_resolve(host) {
                tried_dns = true;
                target = crate::ssh::ip_target(host, ip);
                let (full, _) = crate::ssh::split_ssh_host(host);
                let alias = full.rsplit('@').next().unwrap_or(&full).to_string();
                attachment_args.push("-o".to_string());
                attachment_args.push(format!("HostKeyAlias={alias}"));
                std::thread::sleep(std::time::Duration::from_millis(150));
                continue;
            }
        }
        // 3rd fallback: plain retry for transient DNS/network hiccups.
        if transient && attempt < 4 {
            std::thread::sleep(std::time::Duration::from_millis(300 * attempt as u64));
            continue;
        }

        return Err(if msg.is_empty() {
            format!("remote command failed (exit {code})")
        } else {
            crate::ssh::resolution_hint(msg)
        });
    }
}

fn ssh_ok(host: &str, remote: &[&str]) -> bool {
    ssh_io(host, remote, None, &|_| {}).is_ok()
}

/// Best-effort location of `installer/install.sh` for the optional root upgrade.
fn resolve_installer(agent_dir: &Path) -> Option<PathBuf> {
    if let Ok(p) = std::env::var("PUPPETTERM_INSTALLER") {
        let p = PathBuf::from(p);
        if p.exists() {
            return Some(p);
        }
    }
    // dev layout: agent/bin + installer/ side by side under the repo root
    let candidate = agent_dir.parent()?.parent()?.join("installer").join("install.sh");
    if candidate.exists() {
        return Some(candidate);
    }
    None
}

/// Install (or upgrade) the agent on `host`, streaming progress via `emit`.
pub fn install_agent(
    host: &str,
    agent_dir: Option<&str>,
    pubkey_path: Option<String>,
    force: bool,
    emit: &dyn Fn(&str),
) -> Result<InstallResult, String> {
    // 1) remote arch
    let (_, arch_out) = ssh_io(host, &["uname", "-m"], None, &|_| {})?;
    let machine = arch_out.trim().to_string();
    let arch = match machine.as_str() {
        "x86_64" | "amd64" => "amd64",
        "aarch64" | "arm64" => "arm64",
        other => return Err(format!("unsupported remote architecture: {other}")),
    };
    emit(&format!("==> puppetterm-agent install on {host} ({machine})"));

    // 1b) idempotency + home dir. The agent installs into the SSH user's home
    // under ~/.snap/app/puppetterm/ (no sudo needed). Legacy shared paths are
    // still recognized so existing installs aren't duplicated.
    const AGENT_PATH: &str = "$HOME/.snap/app/puppetterm/bin/puppetterm-agent";
    // Single-quoted as ONE argument so the remote shell re-parses it as a
    // quoted `echo "$HOME"` (splitting it into vector elements would make the
    // shell run bare `echo`, losing $HOME and yielding an empty result).
    let (_, home_out) = ssh_io(host, &["sh", "-c", "'echo \"$HOME\"'"], None, &|_| {})?;
    let r_home = home_out.trim().to_string();
    if r_home.is_empty() {
        return Err("could not determine remote home directory".into());
    }
    let abs_agent = format!("{r_home}/.snap/app/puppetterm/bin/puppetterm-agent");

    let existing: Option<String> = if ssh_ok(host, &["test", "-x", AGENT_PATH]) {
        Some(abs_agent.clone())
    } else if ssh_ok(host, &["test", "-x", "/var/local/puppetterm/bin/puppetterm-agent"]) {
        Some("/var/local/puppetterm/bin/puppetterm-agent".to_string())
    } else if ssh_ok(host, &["test", "-x", "/usr/local/bin/puppetterm-agent"]) {
        Some("/usr/local/bin/puppetterm-agent".to_string())
    } else {
        None
    };
    if !force {
        if let Some(p) = existing {
            emit(&format!(
                "==> agent already installed at {p} — nothing to do (use --force to refresh)"
            ));
            return Ok(InstallResult {
                host: host.to_string(),
                arch: arch.to_string(),
                agent_path: p,
                mode: "user".into(),
                sudoers: false,
                already: true,
            });
        }
    }
    let already = existing.is_some();
    if already {
        emit(if force {
            "==> agent already installed — forced update: refreshing binary + config"
        } else {
            "==> agent already installed — refreshing binary + config (idempotent)"
        });
    }

    // The SSH user (needed only for the optional full-privileges upgrade).
    let (_, whoami_out) = ssh_io(host, &["whoami"], None, &|_| {})?;
    let ssh_user = whoami_out.trim().to_string();

    // 2) local agent binary
    let home = std::env::var("HOME").unwrap_or_default();
    let dir = agent_dir
        .map(|d| d.to_string())
        .or_else(|| std::env::var("PUPPETTERM_AGENT_DIR").ok())
        .filter(|d| !d.is_empty())
        .unwrap_or_else(|| format!("{home}/.puppetterm/agents"));
    let dir = PathBuf::from(dir);
    let bin_path = dir.join(format!("puppetterm-agent-linux-{arch}"));
    let bin = std::fs::read(&bin_path).map_err(|e| {
        format!(
            "agent binary not found at {} ({e}) — build with 'make cross' (agent/) and set PUPPETTERM_AGENT_DIR",
            bin_path.display()
        )
    })?;

    // 3) agent pubkey (OPTIONAL — command-locked authorized_keys entry).
    //    Only installed when explicitly provided (--agent-pubkey param or
    //    PUPPETTERM_AGENT_PUBKEY). The client invokes the agent via the user's
    //    normal SSH key, so a dedicated pubkey is not required for agent mode.
    let have_pubkey;
    let (key_body, pubkey_bytes) = match pubkey_path
        .or_else(|| std::env::var("PUPPETTERM_AGENT_PUBKEY").ok())
        .filter(|p| !p.is_empty())
    {
        Some(pubkey_file) => {
            let pubkey = std::fs::read_to_string(&pubkey_file)
                .map_err(|e| format!("agent pubkey not found at {pubkey_file} ({e})"))?;
            let key_body: Vec<&str> = pubkey.split_whitespace().take(2).collect();
            if key_body.len() < 2 {
                return Err(format!("malformed agent pubkey at {pubkey_file}"));
            }
            have_pubkey = true;
            (
                format!("{} {}", key_body[0], key_body[1]),
                std::fs::read(&pubkey_file).unwrap_or_default(),
            )
        }
        None => {
            have_pubkey = false;
            (String::new(), Vec::new())
        }
    };

    // 4) create the user-space install dir tree
    emit("==> creating ~/.snap/app/puppetterm/bin");
    ssh_io(host, &["mkdir", "-p", "~/.snap/app/puppetterm/bin"], None, emit)?;
    ssh_io(host, &["chmod", "0755", "~/.snap/app/puppetterm"], None, emit)?;

    // 5) stage + install the binary. A temp file then rename avoids ETXTBSY
    // ("Text file busy") when the agent is currently executing — `mv` relinks
    // the directory entry, so the running process keeps its old inode.
    emit(if already {
        "==> refreshing binary (user-space)"
    } else {
        "==> installing binary (user-space)"
    });
    ssh_io(host, &["cat", ">", "~/.snap/app/puppetterm/bin/puppetterm-agent.tmp"], Some(&bin), &|_| {})?;
    ssh_io(host, &["chmod", "0755", "~/.snap/app/puppetterm/bin/puppetterm-agent.tmp"], None, emit)?;
    ssh_io(host, &["mv", "-f", "~/.snap/app/puppetterm/bin/puppetterm-agent.tmp", "~/.snap/app/puppetterm/bin/puppetterm-agent"], None, emit)?;

    // 6) command-locked authorized_keys entry (idempotent, optional)
    if have_pubkey {
        emit("==> authorizing agent key (command-locked)");
        let (_, existing_keys) = ssh_io(host, &["cat", "~/.ssh/authorized_keys"], None, &|_| {})?;
        if existing_keys.contains("puppetterm-agent") {
            emit("    agent key already present (skipping)");
        } else {
            let lock = format!(
                "\n# puppetterm-agent (command-locked)\nrestrict,command=\"{abs_agent}\",no-pty,no-agent-forwarding,no-port-forwarding,no-X11-forwarding {key_body} puppetterm-agent\n"
            );
            ssh_io(host, &["mkdir", "-p", "~/.ssh"], None, emit)?;
            ssh_io(host, &["cat", ">>", "~/.ssh/authorized_keys"], Some(lock.as_bytes()), &|_| {})?;
            ssh_io(host, &["chmod", "0600", "~/.ssh/authorized_keys"], None, emit)?;
            emit("    authorized_keys updated (command-locked entry)");
        }
    } else {
        emit("==> skipping agent key (no pubkey configured — the client uses your SSH key)");
    }

    // 7) agent allow-list config (user-owned)
    emit("==> writing agent config");
    let cfg = "{\"log_prefixes\":[\"/var/log/\"],\"config_prefixes\":[]}\n";
    ssh_io(host, &["cat", ">", "~/.snap/app/puppetterm/config.json"], Some(cfg.as_bytes()), &|_| {})?;
    ssh_io(host, &["chmod", "0644", "~/.snap/app/puppetterm/config.json"], None, emit)?;

    // 8) verify
    emit("==> verifying agent");
    let req = b"{\"action\":\"snapshot\",\"request_id\":\"install-check\"}\n";
    let (code, out) = ssh_io(host, &[&abs_agent], Some(req), &|_| {})?;
    if code != 0 || !out.contains("\"exit\":0") {
        return Err(format!("agent verification failed (exit {code}): {}", out.trim()));
    }
    emit("    agent responded OK");

    // 9) optional full-privileges upgrade (scoped sudoers + /etc config) when
    // the local installer script is available AND passwordless sudo works.
    let mut mode = "user".to_string();
    let mut sudoers = false;
    if let Some(installer) = resolve_installer(&dir) {
        emit("==> applying full-privileges installer (sudoers + config)");
        let script = std::fs::read_to_string(&installer)
            .map_err(|e| format!("cannot read installer {}: {e}", installer.display()))?;
        ssh_io(host, &["cat", ">", "/tmp/puppetterm-install.sh"], Some(script.as_bytes()), &|_| {})?;
        ssh_io(host, &["cat", ">", "/tmp/puppetterm-agent"], Some(&bin), &|_| {})?;
        if have_pubkey {
            ssh_io(host, &["cat", ">", "/tmp/puppetterm-agent.pub"], Some(&pubkey_bytes), &|_| {})?;
        }
        if !ssh_user.is_empty() && ssh_ok(host, &["sudo", "-n", "true"]) {
            let mut args = vec![
                "sudo", "-n", "bash", "/tmp/puppetterm-install.sh", "--binary",
                "/tmp/puppetterm-agent", "--ssh-user", &ssh_user, "--yes",
            ];
            if have_pubkey {
                args.splice(7..7, ["--agent-pubkey", "/tmp/puppetterm-agent.pub"]);
            }
            ssh_io(host, &args, None, emit)?;
            sudoers = true;
            mode = "root".into();
            emit("==> full install complete (agentic privileges granted)");
        } else {
            emit("    (no passwordless sudo — user-space agent only; run installer/install.sh manually for full privileges)");
        }
    } else if already && !force {
        // Already installed and no payload to refresh — nothing more to do.
    } else {
        emit("    (installer script not found — agent is active under ~/.snap/app/puppetterm; run installer/install.sh manually for full privileges)");
    }

    emit(&format!("==> done: agent installed on {host}"));
    Ok(InstallResult {
        host: host.to_string(),
        arch: arch.to_string(),
        agent_path: abs_agent,
        mode,
        sudoers,
        already,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    // Live test against a real host. Skipped unless PUPPETTERM_TEST_INSTALL=1
    // (set PUPPETTERM_AGENT_DIR to the dir with the built binaries). Installs
    // user-space into ~/.snap/app/puppetterm — no sudo required.
    #[test]
    fn install_agent_user_space_live() {
        if std::env::var("PUPPETTERM_TEST_INSTALL").unwrap_or_default() != "1" {
            eprintln!("skipping; set PUPPETTERM_TEST_INSTALL=1 to run against a host");
            return;
        }
        let host = std::env::var("PUPPETTERM_TEST_HOST")
            .unwrap_or_else(|_| "user@host".to_string());
        let lines = std::cell::RefCell::new(Vec::<String>::new());
        let res = install_agent(&host, None, None, false, &|l| lines.borrow_mut().push(l.to_string()))
            .expect("install_agent");
        assert!(
            res.agent_path.contains(".snap/app/puppetterm")
                || res.agent_path.contains("/var/local/puppetterm")
                || res.agent_path.contains("/usr/local/bin"),
            "agent path: {}",
            res.agent_path
        );
        eprintln!("install OK: {res:?}\n{}", lines.borrow().join("\n"));
    }

    #[test]
    fn check_agent_live() {
        if std::env::var("PUPPETTERM_TEST_INSTALL").unwrap_or_default() != "1" {
            return;
        }
        let host = std::env::var("PUPPETTERM_TEST_HOST")
            .unwrap_or_else(|_| "user@host".to_string());
        assert!(check_agent(&host), "agent should be present after install");
    }
}
