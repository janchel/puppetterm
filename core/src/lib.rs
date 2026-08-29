//! puppetterm-core — shared backend logic for puppetterm.
//!
//! Used by both the Tauri desktop shell (`client`) and the headless web
//! server (`server`). Deliberately free of any Tauri/UI dependencies:
//! everything here is plain Rust over SSH processes, files, and HTTP.

pub mod agent;
pub mod ai;
pub mod audit;
pub mod install;
pub mod sessions;
pub mod ssh;

use std::process::Command;

/// Run a command on a host over a plain key-based SSH connection and return
/// the FULL captured stdout (no pty, no agent). Used for file reads when the
/// agent binary isn't installed — a direct SSH exec returns every byte.
pub async fn run_ssh_capture(host: String, cmd: String) -> Result<serde_json::Value, String> {
    if host.trim().is_empty() || cmd.trim().is_empty() {
        return Err("ssh_capture: empty host or command".into());
    }
    let host2 = host.clone();
    let cmd2 = cmd.clone();
    tokio::task::spawn_blocking(move || {
    let mut cmd = Command::new("ssh");
    cmd.args([
        "-o", "BatchMode=yes",
        "-o", "ConnectTimeout=8",
        "-o", "StrictHostKeyChecking=accept-new",
    ]);
    crate::ssh::ssh_host(&mut cmd, &host2);
    let out = cmd
        .arg(&cmd2)
        .output()
        .map_err(|e| format!("ssh_capture: {e}"))?;
        Ok(serde_json::json!({
            "host": host2,
            "exit": out.status.code().unwrap_or(-1),
            "stdout": String::from_utf8_lossy(&out.stdout),
            "stderr": String::from_utf8_lossy(&out.stderr),
        }))
    })
    .await
    .map_err(|e| e.to_string())?
}
