//! In-app agent installer — installs `puppetterm-agent` on a remote host over
//! the user's existing SSH keys, without requiring a password or sudo.
//!
//! Strategy:
//! 1. Always install a **user-space** agent (binary + command-locked key +
//!    config under `~/.puppetterm/`). No sudo, works with the existing key.
//! 2. If passwordless sudo is available on the host (`sudo -n true`), ALSO
//!    upgrade to a **root** install by running the full `installer/install.sh`
//!    (systemctl/apt grants + /etc config + /var/log audit), giving the agent
//!    full state-changing privileges.

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
/// user-space or the system-wide path).
pub fn check_agent(host: &str) -> bool {
    let out = Command::new("ssh")
        .args(["-o", "BatchMode=yes", "-o", "ConnectTimeout=8"])
        .arg(host)
        .args([
            "test", "-x", "~/.puppetterm/bin/puppetterm-agent", "-o", "-x",
            "/usr/local/bin/puppetterm-agent",
        ])
        .output();
    matches!(out, Ok(o) if o.status.success())
}

/// Run a remote command over SSH, optionally feeding stdin, streaming stdout
/// lines through `emit`. Returns (exit_code, stdout_text).
fn ssh_io(
    host: &str,
    remote: &[&str],
    stdin_data: Option<&[u8]>,
    emit: &dyn Fn(&str),
) -> Result<(i32, String), String> {
    let mut cmd = Command::new("ssh");
    cmd.args(["-o", "BatchMode=yes", "-o", "ConnectTimeout=10"]);
    cmd.arg(host).args(remote);
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
    if code != 0 {
        let msg = err.trim();
        return Err(if msg.is_empty() {
            format!("remote command failed (exit {code})")
        } else {
            msg.to_string()
        });
    }
    Ok((code, out))
}

fn ssh_ok(host: &str, remote: &[&str]) -> bool {
    ssh_io(host, remote, None, &|_| {}).is_ok()
}

fn home_of(host: &str) -> Result<String, String> {
    let (_, out) = ssh_io(host, &["echo", "$HOME"], None, &|_| {})?;
    Ok(out.trim().to_string())
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

    // 1b) idempotency: if a full (root) agent is already installed, nothing to
    // do — UNLESS `force` (update/reinstall) is set, in which case we re-run
    // the whole install so the binary/config/sudoers are refreshed.
    let root_agent = "/usr/local/bin/puppetterm-agent";
    if !force && ssh_ok(host, &["test", "-x", root_agent]) {
        emit(&format!(
            "==> agent already installed at {root_agent} (root install) — nothing to do"
        ));
        return Ok(InstallResult {
            host: host.to_string(),
            arch: arch.to_string(),
            agent_path: root_agent.into(),
            mode: "root".into(),
            sudoers: true,
            already: true,
        });
    }
    // If only the user-space agent exists, refresh it in place (idempotent).
    let mut already = false;
    if ssh_ok(host, &["test", "-x", "~/.puppetterm/bin/puppetterm-agent"]) {
        already = true;
        emit(if force {
            "==> agent already installed (user-space) — forced update: refreshing binary + config"
        } else {
            "==> agent already installed (user-space) — refreshing binary + config (idempotent)"
        });
    }

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

    // 3) agent pubkey (command-locked authorized_keys entry)
    let pubkey_file = pubkey_path
        .or_else(|| std::env::var("PUPPETTERM_AGENT_PUBKEY").ok())
        .unwrap_or_else(|| format!("{home}/.ssh/puppetterm-agent.pub"));
    let pubkey = std::fs::read_to_string(&pubkey_file)
        .map_err(|e| format!("agent pubkey not found at {pubkey_file} ({e})"))?;
    let key_body: Vec<&str> = pubkey.split_whitespace().take(2).collect();
    if key_body.len() < 2 {
        return Err("malformed agent pubkey".into());
    }
    let key_body = format!("{} {}", key_body[0], key_body[1]);
    let pubkey_bytes = std::fs::read(&pubkey_file).unwrap_or_default();

    let r_home = home_of(host)?;
    let user_agent = format!("{r_home}/.puppetterm/bin/puppetterm-agent");

    // 4) install/refresh binary (user-space)
    emit(if already {
        "==> refreshing binary (user-space)"
    } else {
        "==> installing binary (user-space)"
    });
    ssh_io(host, &["mkdir", "-p", "~/.puppetterm/bin"], None, emit)?;
    ssh_io(host, &["cat", ">", "~/.puppetterm/bin/puppetterm-agent"], Some(&bin), &|_| {})?;
    ssh_io(host, &["chmod", "0755", "~/.puppetterm/bin/puppetterm-agent"], None, emit)?;

    // 5) command-locked authorized_keys entry (idempotent)
    emit("==> authorizing agent key");
    let (_, existing) = ssh_io(host, &["cat", "~/.ssh/authorized_keys"], None, &|_| {})?;
    if existing.contains("puppetterm-agent") {
        emit("    agent key already present (skipping)");
    } else {
        let lock = format!(
            "\n# puppetterm-agent (command-locked)\nrestrict,command=\"{user_agent}\",no-pty,no-agent-forwarding,no-port-forwarding,no-X11-forwarding {key_body} puppetterm-agent\n"
        );
        ssh_io(host, &["cat", ">>", "~/.ssh/authorized_keys"], Some(lock.as_bytes()), &|_| {})?;
        ssh_io(host, &["chmod", "0600", "~/.ssh/authorized_keys"], None, emit)?;
        emit("    authorized_keys updated (command-locked entry)");
    }

    // 6) agent config
    emit("==> writing agent config");
    let cfg = "{\"log_prefixes\":[\"/var/log/\"],\"config_prefixes\":[]}\n";
    ssh_io(host, &["cat", ">", "~/.puppetterm/config.json"], Some(cfg.as_bytes()), &|_| {})?;

    // 7) verify
    emit("==> verifying agent");
    let req = b"{\"action\":\"snapshot\",\"request_id\":\"install-check\"}\n";
    let (code, out) = ssh_io(host, &["~/.puppetterm/bin/puppetterm-agent"], Some(req), &|_| {})?;
    if code != 0 || !out.contains("\"exit\":0") {
        return Err(format!("agent verification failed (exit {code}): {}", out.trim()));
    }
    emit("    agent responded OK");

    // 8) optional root upgrade when passwordless sudo is available
    let mut mode = "user".to_string();
    let mut sudoers = false;
    if ssh_ok(host, &["sudo", "-n", "true"]) {
        emit("==> passwordless sudo detected — upgrading to root install");
        if let Some(installer) = resolve_installer(&dir) {
            let script = std::fs::read_to_string(&installer)
                .map_err(|e| format!("cannot read installer {}: {e}", installer.display()))?;
            ssh_io(host, &["cat", ">", "/tmp/puppetterm-install.sh"], Some(script.as_bytes()), &|_| {})?;
            ssh_io(host, &["cat", ">", "/tmp/puppetterm-agent"], Some(&bin), &|_| {})?;
            ssh_io(host, &["cat", ">", "/tmp/puppetterm-agent.pub"], Some(&pubkey_bytes), &|_| {})?;
            let user = if host.contains('@') {
                host.split('@').next().unwrap_or_default().to_string()
            } else {
                std::env::var("USER").unwrap_or_default()
            };
            ssh_io(
                host,
                &[
                    "sudo", "-n", "bash", "/tmp/puppetterm-install.sh", "--binary",
                    "/tmp/puppetterm-agent", "--agent-pubkey", "/tmp/puppetterm-agent.pub",
                    "--ssh-user", &user, "--yes",
                ],
                None,
                emit,
            )?;
            mode = "root".into();
            sudoers = true;
            emit("==> root install complete (full agentic privileges)");
        } else {
            emit("    (installer script not found — skipping root upgrade; user-space agent is active)");
        }
    } else {
        emit("    (no passwordless sudo — user-space agent only; run installer/install.sh manually for full privileges)");
    }

    emit(&format!("==> done: agent installed on {host}"));
    Ok(InstallResult {
        host: host.to_string(),
        arch: arch.to_string(),
        agent_path: if mode == "root" {
            "/usr/local/bin/puppetterm-agent".into()
        } else {
            user_agent
        },
        mode,
        sudoers,
        already,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    // Live test against a real host. Skipped unless PUPPETTERM_TEST_INSTALL=1
    // (set PUPPETTERM_AGENT_DIR to the dir with the built binaries).
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
            res.agent_path.contains(".puppetterm") || res.agent_path.contains("/usr/local/bin"),
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
