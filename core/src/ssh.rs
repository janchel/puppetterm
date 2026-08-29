//! SSH helpers: discover host aliases from ~/.ssh/config and probe reachability.

use std::path::PathBuf;
use std::process::Command;

/// Path to the user's SSH config file.
pub fn ssh_config_path() -> PathBuf {
    std::env::var("HOME")
        .map(PathBuf::from)
        .unwrap_or_default()
        .join(".ssh")
        .join("config")
}

/// Parse concrete `Host` aliases from ~/.ssh/config.
///
/// Wildcard patterns (`*`, `?`) and negations are skipped — only concrete
/// aliases are returned (sorted, de-duplicated).
pub fn parse_ssh_config_hosts() -> Vec<String> {
    let mut hosts: Vec<String> = Vec::new();
    let Ok(content) = std::fs::read_to_string(ssh_config_path()) else {
        return hosts;
    };
    for line in content.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        // "Host foo bar" (note the space — excludes "HostName ...").
        if let Some(rest) = line.strip_prefix("Host ") {
            for pat in rest.split_whitespace() {
                if pat.is_empty() || pat.contains('*') || pat.contains('?') {
                    continue;
                }
                hosts.push(pat.to_string());
            }
        }
    }
    hosts.sort();
    hosts.dedup();
    hosts
}

/// Split a `user@host:port` (or `host:port`) target into the host and an
/// optional port. OpenSSH itself does not accept `host:port`, so callers must
/// translate the returned port into a `-p` argument (see [`ssh_host`]).
pub fn split_ssh_host(host: &str) -> (String, Option<u16>) {
    fn parse_port(s: &str) -> Option<u16> {
        s.parse::<u16>().ok()
    }
    if let Some(at) = host.rfind('@') {
        let after = &host[at + 1..];
        if let Some(colon) = after.rfind(':') {
            if let Some(p) = parse_port(&after[colon + 1..]) {
                return (format!("{}{}", &host[..at + 1], &after[..colon]), Some(p));
            }
        }
    } else if let Some(colon) = host.rfind(':') {
        if let Some(p) = parse_port(&host[colon + 1..]) {
            return (host[..colon].to_string(), Some(p));
        }
    }
    (host.to_string(), None)
}

/// Apply a target host to an `ssh` Command, inserting `-p <port>` when the host
/// carries a `:port` suffix. This lets non-standard SSH ports work uniformly
/// across terminal sessions, agent runs, installs and host probes — the caller
/// just passes the same `user@host:port` string everywhere.
pub fn ssh_host(cmd: &mut Command, host: &str) {
    let (h, port) = split_ssh_host(host);
    if let Some(p) = port {
        cmd.arg("-p").arg(p.to_string());
    }
    cmd.arg(h);
}

/// Probe whether a host is reachable over SSH (key-based, short timeout).
pub fn check_host(host: &str) -> bool {
    let mut cmd = Command::new("ssh");
    cmd.args([
        "-o", "BatchMode=yes",
        "-o", "ConnectTimeout=2",
        "-o", "StrictHostKeyChecking=accept-new",
    ]);
    ssh_host(&mut cmd, host);
    cmd.arg("true")
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}
