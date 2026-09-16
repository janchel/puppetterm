//! SSH helpers: discover host aliases from ~/.ssh/config, probe reachability,
//! and manage a shared OpenSSH ControlMaster so backend commands ride the same
//! authenticated connection as the live terminal (no re-resolving hostnames).

use std::collections::HashMap;
use std::net::{IpAddr, ToSocketAddrs};
use std::path::PathBuf;
use std::process::Command;
use std::sync::{LazyLock, Mutex};

/// Cache of aliases → resolved IP, so a successful resolution makes later
/// backend commands DNS-free even if resolution turns flaky in between.
static DNS_CACHE: LazyLock<Mutex<HashMap<String, IpAddr>>> = LazyLock::new(Default::default);

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
    attach_control(&mut cmd, host);
    ssh_host(&mut cmd, host);
    cmd.arg("true")
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

/// Sanitize a host string into a safe socket filename component (must match
/// `client/scripts/ssh-mux.sh`'s `sock_for()`).
pub fn ssh_sanitize(host: &str) -> String {
    host.chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '_' })
        .collect()
}

/// Directory for puppetterm-managed ControlMaster sockets (ssh-mux.sh layout).
pub fn ssh_mux_dir() -> PathBuf {
    if let Ok(d) = std::env::var("PUPPETTERM_MUX_DIR") {
        if !d.is_empty() {
            return PathBuf::from(d);
        }
    }
    if let Ok(x) = std::env::var("XDG_RUNTIME_DIR") {
        if !x.is_empty() {
            return PathBuf::from(x).join("puppetterm-mux");
        }
    }
    PathBuf::from("/tmp/puppetterm-mux")
}

/// Ensure the puppetterm ControlMaster socket directory exists. ControlPath /
/// `-S` sockets fail to bind if their parent directory is missing, so every
/// path that attaches or spawns a master must call this first.
pub fn ensure_mux_dir() {
    let _ = std::fs::create_dir_all(ssh_mux_dir());
}

/// ControlPath for a NEW control master (e.g. an interactive SSH tab opened by
/// the app). The app owns its own socket scheme so a session is reusable by
/// installs/probes/agent runs without depending on the user's ssh config.
pub fn ssh_control_path(host: &str) -> PathBuf {
    ssh_mux_dir().join(format!("{}.sock", ssh_sanitize(host)))
}

/// First `User` directive that applies to `host` from ~/.ssh/config (sshd
/// first-match-wins). Only literal `Host <name>` blocks are considered, so
/// wildcard sections are ignored.
pub fn ssh_user_for(host: &str) -> Option<String> {
    let Ok(content) = std::fs::read_to_string(ssh_config_path()) else {
        return None;
    };
    let mut block: Option<Vec<String>> = None;
    for line in content.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let mut kv = line.splitn(2, char::is_whitespace);
        let (Some(kw), Some(val)) = (kv.next(), kv.next()) else {
            continue;
        };
        if kw == "Host" {
            block = Some(val.split_whitespace().map(str::to_string).collect());
            continue;
        }
        if kw == "User" && block.as_ref().is_some_and(|names| names.iter().any(|n| n == host)) {
            return Some(val.trim().to_string());
        }
    }
    None
}

/// If the USER's own ssh config (`ControlMaster auto` + the
/// `~/.ssh/puppetterm-mux/%r@%h:%p` scheme that `cleanup_stale_masters` manages)
/// has already established a master for `host`, point at that socket instead —
/// only ever reused, never created from here.
fn user_mux_socket(host: &str) -> Option<PathBuf> {
    let home = std::env::var("HOME").ok()?;
    let dir = PathBuf::from(&home).join(".ssh").join("puppetterm-mux");
    if !dir.is_dir() {
        return None;
    }
    let (h, port) = split_ssh_host(host);
    let p = port.unwrap_or(22);
    let mut cands: Vec<String> = Vec::new();
    // Socket name is what the master was spawned with: %r@%h:%p (never created
    // here — only reused if the user's interactive ssh already made one).
    if let Some(at) = host.rfind('@') {
        cands.push(format!("{}@{h}:{p}", &host[..at])); // explicit user@ is the strongest hint
    }
    if let Some(u) = ssh_user_for(&h).filter(|u| !u.is_empty()) {
        cands.push(format!("{u}@{h}:{p}"));
    }
    cands.push(format!("{h}:{p}"));
    cands.dedup();
    cands.into_iter().map(|s| dir.join(s)).find(|p| p.exists())
}

/// Point `cmd` at the shared ControlMaster for `host` (creating the master on
/// the first successful connection via `ControlMaster=auto` + `ControlPersist`).
///
/// Once any connection to the host exists, later commands ride the SAME
/// authenticated channel instead of opening a fresh one — so they never have to
/// re-resolve hostnames or re-authenticate. This is what keeps installs working
/// when an alias like `mail` is only resolvable from the live session's context
/// (DNS, ProxyJump, VPN, …). `auto` also heals a stale socket by re-mastering.
/// Returns true when a control path was attached (always, unless disabled).
pub fn attach_control(cmd: &mut Command, host: &str) -> bool {
    let sock = user_mux_socket(host).unwrap_or_else(|| ssh_control_path(host));
    ensure_mux_dir();
    cmd.arg("-S").arg(&sock);
    cmd.arg("-o").arg("ControlMaster=auto");
    cmd.arg("-o").arg("ControlPersist=600");
    true
}

/// Append actionable guidance when ssh fails to resolve a hostname
/// (the backend can't reach the DNS/network the live session could — a fresh
/// `ssh` will keep failing, only a live master or an explicit HostName helps).
pub fn resolution_hint(err: &str) -> String {
    if err.to_lowercase().contains("could not resolve hostname")
        || err.to_lowercase().contains("temporary failure in name resolution")
    {
        format!("{err} — the server can't resolve that hostname. Fixes: add a `Host <name>` entry with `HostName <ip>` to ~/.ssh/config and restart the container (or reopen an SSH tab to the host first so the installer reuses its live connection).")
    } else {
        err.to_string()
    }
}

/// Derive the concrete `(user, hostname, port, proxyjump)` an alias maps to
/// from `~/.ssh/config` via `ssh -G` — resolves `HostName`/`User`/`Port`/
/// `ProxyJump` WITHOUT touching DNS. Returns None when the alias has no
/// explicit `HostName` (only DNS can resolve it) or `ssh -G` fails.
///
/// This is the full-config re-resolution used to bypass flaky name resolution:
/// if the interactive session resolved `mail` via a `Host` block, installs can
/// retry against the concrete address straight from config.
pub fn config_resolve(host: &str) -> Option<(String, String, u16, Option<String>)> {
    let out = Command::new("ssh")
        .arg("-G")
        .arg(host)
        .stdin(std::process::Stdio::null())
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let text = String::from_utf8_lossy(&out.stdout);
    let mut user: Option<String> = None;
    let mut hostname: Option<String> = None;
    let mut port: u16 = 22;
    let mut jump: Option<String> = None;
    for line in text.lines() {
        let mut kv = line.splitn(2, ' ');
        let (Some(k), Some(v)) = (kv.next(), kv.next()) else {
            continue;
        };
        let v = v.trim();
        match k {
            "user" => {
                if user.is_none() && !v.is_empty() {
                    user = Some(v.to_string());
                }
            }
            "hostname" if !v.is_empty() => {
                hostname = Some(v.to_string());
            }
            "port" => {
                if let Ok(p) = v.parse::<u16>() {
                    port = p;
                }
            }
            "proxyjump" if !v.is_empty() && v != "none" => {
                jump = Some(v.to_string());
            }
            _ => {}
        }
    }
    // No explicit HostName → nothing config can do; DNS is the only source.
    let hostname = hostname.filter(|h| !h.is_empty() && h != host)?;
    // Prefer an explicit `user@` already on the target over config `User`.
    let user = if let Some(at) = host.rfind('@') {
        host[..at].to_string()
    } else {
        user.unwrap_or_default()
    };
    Some((user, hostname, port, jump))
}

/// True when an ssh error message looks like a transient name-resolution or
/// connect failure worth retrying (flaky DNS guarded by `Temporary failure`).
pub fn is_transient_ssh_error(err: &str) -> bool {
    let e = err.to_lowercase();
    e.contains("could not resolve hostname")
        || e.contains("temporary failure in name resolution")
        || e.contains("connection timed out")
        || e.contains("network is unreachable")
        || e.contains("connection refused")
}

/// Resolve a host alias to an IP via the OS resolver (the same DNS ssh itself
/// uses, so it benefits from retrying the flaky window). The result is cached
/// process-wide, so later install/agent commands reuse the address without
/// touching DNS again. `host` may carry a `user@`/`:port` prefix — only the
/// hostname is resolved.
pub fn dns_resolve(host: &str) -> Option<IpAddr> {
    let (full, _) = split_ssh_host(host);
    let hostname = full.rsplit('@').next().unwrap_or(&full).to_string();
    if let Some(ip) = DNS_CACHE.lock().ok()?.get(&hostname) {
        return Some(*ip);
    }
    let addrs: Vec<_> = (hostname.as_str(), 22u16)
        .to_socket_addrs()
        .ok()?
        .collect();
    let ip = addrs.into_iter().next().map(|s| s.ip())?;
    if let Ok(mut g) = DNS_CACHE.lock() {
        g.insert(hostname, ip);
    }
    Some(ip)
}

/// Non-blocking probe of the DNS cache (does not re-resolve). Used to start
/// later backend commands straight on the cached IP when one exists.
pub fn dns_cached(host: &str) -> Option<IpAddr> {
    let (full, _) = split_ssh_host(host);
    let hostname = full.rsplit('@').next().unwrap_or(&full).to_string();
    DNS_CACHE.lock().ok()?.get(&hostname).copied()
}

/// Build a concrete `[user@]ip[:port]` target from a resolved address, keeping
/// the `user@` prefix (and `:port` suffix) the caller used. The returned target
/// connects to the IP directly — DNS-free — while [`ssh_host`] keeps the port.
pub fn ip_target(host: &str, ip: IpAddr) -> String {
    let (full, port) = split_ssh_host(host);
    let port = port.unwrap_or(22);
    match full.rfind('@') {
        Some(i) => format!("{}@{}:{}", &full[..i], ip, port),
        None => format!("{ip}:{port}"),
    }
}
