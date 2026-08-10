//! Agent action runner — invokes `puppetterm-agent` on a remote host over SSH,
//! streaming NDJSON events back and returning them.
//!
//! Each invocation is its own SSH exec, so concurrent calls never interleave.
//! If a ControlMaster socket exists (see client/scripts/ssh-mux.sh) it is
//! reused for the connection, otherwise a fresh one is opened.

use std::io::{BufRead, BufReader, Read, Write};
use std::path::PathBuf;
use std::process::{Command, Stdio};

use serde::Serialize;

/// One NDJSON event streamed from the remote agent.
#[derive(Clone, Serialize)]
pub struct AgentEvent {
    pub host: String,
    pub event: serde_json::Value,
}

/// Final result of one action run.
#[derive(Serialize)]
pub struct AgentRunResult {
    pub host: String,
    pub exit: i32,
    pub events: Vec<serde_json::Value>,
}

/// Run one agent action on `host`. `emit` is called for each streamed event.
///
/// Blocking — call from a worker thread (e.g. `spawn_blocking`) in the app.
pub fn run_action(
    host: &str,
    request: &str,
    emit: impl Fn(AgentEvent) + Send + Sync + 'static,
) -> Result<AgentRunResult, String> {
    let agent = std::env::var("PUPPETTERM_AGENT_BIN")
        .unwrap_or_else(|_| "/usr/local/bin/puppetterm-agent".to_string());

    let mut cmd = Command::new("ssh");
    cmd.args(["-o", "BatchMode=yes", "-o", "ConnectTimeout=10"]);
    if let Some(sock) = mux_socket_for(host) {
        cmd.arg("-S").arg(sock);
    }
    cmd.arg(host).arg(agent);
    cmd.stdin(Stdio::piped()).stdout(Stdio::piped()).stderr(Stdio::piped());

    let mut child = cmd.spawn().map_err(|e| format!("spawn ssh: {e}"))?;

    // Send the single request, then close stdin so the agent runs and exits.
    if let Some(mut stdin) = child.stdin.take() {
        stdin
            .write_all(request.as_bytes())
            .map_err(|e| format!("write request: {e}"))?;
        drop(stdin);
    }

    // Read stdout as NDJSON lines, streaming each event via `emit`.
    let stdout = child.stdout.take().ok_or("no stdout pipe")?;
    let reader = BufReader::new(stdout);

    let mut events: Vec<serde_json::Value> = Vec::new();
    for line in reader.lines() {
        let line = line.map_err(|e| format!("read stdout: {e}"))?;
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        if let Ok(value) = serde_json::from_str::<serde_json::Value>(trimmed) {
            emit(AgentEvent { host: host.to_string(), event: value.clone() });
            events.push(value);
        }
    }

    // Capture stderr (auth failures, ssh errors, ...) for diagnostics.
    let mut stderr_text = String::new();
    if let Some(mut err) = child.stderr.take() {
        let _ = err.read_to_string(&mut stderr_text);
    }

    let status = child.wait().map_err(|e| format!("wait: {e}"))?;
    let exit = status.code().unwrap_or(-1);

    if exit != 0 && events.is_empty() {
        return Err(if stderr_text.trim().is_empty() {
            format!("agent action failed (exit {exit})")
        } else {
            stderr_text.trim().to_string()
        });
    }

    Ok(AgentRunResult { host: host.to_string(), exit, events })
}

/// Best-effort lookup of an existing ControlMaster socket for a host
/// (matches the layout used by client/scripts/ssh-mux.sh).
fn mux_socket_for(host: &str) -> Option<String> {
    let dir = if let Ok(d) = std::env::var("PUPPETTERM_MUX_DIR") {
        PathBuf::from(d)
    } else if let Ok(x) = std::env::var("XDG_RUNTIME_DIR") {
        PathBuf::from(x).join("puppetterm-mux")
    } else {
        PathBuf::from("/tmp/puppetterm-mux")
    };
    let sock = dir.join(format!("{}.sock", sanitize(host)));
    sock.exists().then(|| sock.to_string_lossy().into_owned())
}

fn sanitize(host: &str) -> String {
    host.chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '_' })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    // Integration test against real localhost SSH. Skipped unless
    // PUPPETTERM_TEST_SSH=1 (set PUPPETTERM_AGENT_BIN to the built agent).
    #[test]
    fn run_action_parallel_no_interleave() {
        if std::env::var("PUPPETTERM_TEST_SSH").unwrap_or_default() != "1" {
            eprintln!("skipping; set PUPPETTERM_TEST_SSH=1 to run against localhost");
            return;
        }

        let res = run_action("localhost", r#"{"action":"snapshot","request_id":"s-1"}"#, |_| {})
            .expect("single action");
        assert_eq!(res.exit, 0, "single action exit");
        assert!(!res.events.is_empty(), "expected events");

        // Parallel runs must each contain only their own marker.
        let mut handles = Vec::new();
        for i in 0..4 {
            let req = format!(
                r#"{{"action":"run","params":{{"cmd":"echo run-{i}"}},"request_id":"p-{i}"}}"#
            );
            handles.push(std::thread::spawn(move || {
                run_action("localhost", &req, |_| {}).expect("parallel action")
            }));
        }
        for (i, h) in handles.into_iter().enumerate() {
            let res = h.join().expect("thread join");
            let text = res
                .events
                .iter()
                .filter_map(|e| e.get("data").and_then(|d| d.as_str()))
                .collect::<String>();
            assert!(text.contains(&format!("run-{i}")), "run {i} output: {text:?}");
            for j in 0..4 {
                if i != j {
                    assert!(
                        !text.contains(&format!("run-{j}")),
                        "run {i} leaked run {j}: {text:?}"
                    );
                }
            }
        }
    }
}
