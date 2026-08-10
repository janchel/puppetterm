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

/// Probe whether a host is reachable over SSH (key-based, short timeout).
pub fn check_host(host: &str) -> bool {
    Command::new("ssh")
        .args([
            "-o", "BatchMode=yes",
            "-o", "ConnectTimeout=2",
            "-o", "StrictHostKeyChecking=accept-new",
            host,
            "true",
        ])
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}
