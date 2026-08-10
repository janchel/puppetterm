//! puppetterm client backend.
//!
//! Manages interactive SSH sessions backed by a pty (via `portable-pty`),
//! streams pty output to the webview as events, and provides host discovery
//! from ~/.ssh/config.

mod agent;
mod ai;
mod audit;
mod install;
mod ssh;

use std::collections::HashMap;
use std::io::{Read, Write};
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::Mutex;

use portable_pty::{native_pty_system, CommandBuilder, MasterPty, PtySize};
use serde::Serialize;
use tauri::{AppHandle, Emitter, Manager, State};

/// One live SSH session backed by a pty.
struct Session {
    master: Box<dyn MasterPty + Send>,
    writer: Box<dyn Write + Send>,
    child: Box<dyn portable_pty::Child + Send + Sync>,
}

#[derive(Default)]
struct AppState {
    sessions: Mutex<HashMap<u32, Session>>,
    next_id: AtomicU32,
}

#[derive(Clone, Serialize)]
struct PtyOutput {
    id: u32,
    data: String,
}

#[derive(Clone, Serialize)]
struct PtyExit {
    id: u32,
}

/// Progress line emitted while installing the agent on a remote host.
#[derive(Clone, Serialize)]
struct InstallOutput {
    host: String,
    data: String,
}

/// List concrete host aliases from ~/.ssh/config.
#[tauri::command]
fn list_ssh_hosts() -> Vec<String> {
    ssh::parse_ssh_config_hosts()
}

/// Quick reachability probe for a host (used for status dots).
#[tauri::command]
fn check_host(host: String) -> bool {
    ssh::check_host(&host)
}

/// Open a pty running `cmd` (optionally in `cwd`) and stream its output as
/// `pty-output`/`pty-exit` events. Returns the new session id.
fn spawn_pty_session(
    app: &AppHandle,
    state: &State<'_, AppState>,
    cmd_name: &str,
    args: &[&str],
    cwd: Option<&std::path::Path>,
) -> Result<u32, String> {
    let id = state.next_id.fetch_add(1, Ordering::SeqCst);

    let pty_system = native_pty_system();
    let pair = pty_system
        .openpty(PtySize { rows: 24, cols: 80, pixel_width: 0, pixel_height: 0 })
        .map_err(|e| e.to_string())?;

    let mut cmd = CommandBuilder::new(cmd_name);
    cmd.args(args);
    if let Some(cwd) = cwd {
        cmd.cwd(cwd);
    }
    let child = pair.slave.spawn_command(cmd).map_err(|e| e.to_string())?;
    drop(pair.slave);

    let mut reader = pair.master.try_clone_reader().map_err(|e| e.to_string())?;
    let writer = pair.master.take_writer().map_err(|e| e.to_string())?;

    // Stream pty output to the frontend, then signal exit.
    let app_out = app.clone();
    let app_exit = app.clone();
    std::thread::spawn(move || {
        let mut buf = [0u8; 8192];
        loop {
            match reader.read(&mut buf) {
                Ok(0) | Err(_) => break,
                Ok(n) => {
                    let data = String::from_utf8_lossy(&buf[..n]).to_string();
                    let _ = app_out.emit("pty-output", PtyOutput { id, data });
                }
            }
        }
        let _ = app_exit.emit("pty-exit", PtyExit { id });
    });

    {
        let mut sessions = state.sessions.lock().unwrap();
        sessions.insert(id, Session { master: pair.master, writer, child });
    }

    Ok(id)
}

/// Open an interactive SSH session to `host` and start streaming its pty.
#[tauri::command]
fn start_ssh_session(
    app: AppHandle,
    state: State<'_, AppState>,
    host: String,
) -> Result<u32, String> {
    spawn_pty_session(&app, &state, "ssh", &["-tt", &host], None)
}

/// Open a local shell in the user's home directory (no remote connection).
#[tauri::command]
fn start_local_session(app: AppHandle, state: State<'_, AppState>) -> Result<u32, String> {
    let shell = std::env::var("SHELL").unwrap_or_else(|_| "/bin/bash".into());
    let home = std::env::var("HOME").unwrap_or_else(|_| "/".into());
    spawn_pty_session(&app, &state, &shell, &[], Some(std::path::Path::new(&home)))
}

/// Send terminal input (keystrokes) to the session.
#[tauri::command]
fn write_ssh_input(state: State<'_, AppState>, id: u32, data: String) -> Result<(), String> {
    let mut sessions = state.sessions.lock().unwrap();
    let s = sessions.get_mut(&id).ok_or("no such session")?;
    s.writer.write_all(data.as_bytes()).map_err(|e| e.to_string())?;
    s.writer.flush().map_err(|e| e.to_string())
}

/// Resize the remote pty to match the frontend terminal.
#[tauri::command]
fn resize_ssh_pty(
    state: State<'_, AppState>,
    id: u32,
    cols: u16,
    rows: u16,
) -> Result<(), String> {
    let sessions = state.sessions.lock().unwrap();
    let s = sessions.get(&id).ok_or("no such session")?;
    s.master
        .resize(PtySize { rows, cols, pixel_width: 0, pixel_height: 0 })
        .map_err(|e| e.to_string())
}

/// Terminate a session and release its resources.
#[tauri::command]
fn stop_ssh_session(state: State<'_, AppState>, id: u32) -> Result<(), String> {
    let mut sessions = state.sessions.lock().unwrap();
    if let Some(mut s) = sessions.remove(&id) {
        let _ = s.child.kill();
    }
    Ok(())
}

/// Run one agent action on a host over SSH. Streams NDJSON events as
/// `agent-event` Tauri events and returns the collected result.
/// Every action is recorded in the append-only audit log.
#[tauri::command]
async fn run_agent_action(
    app: AppHandle,
    host: String,
    request: String,
    source: Option<String>,
    approved: Option<bool>,
) -> Result<agent::AgentRunResult, String> {
    let app2 = app.clone();
    let source = source.unwrap_or_else(|| "user".to_string());
    let approval = match approved {
        Some(true) => "approved",
        Some(false) => "rejected",
        None => "auto",
    };

    let host_for_action = host.clone();
    let request_for_action = request.clone();
    let request_value: serde_json::Value = serde_json::from_str(&request).unwrap_or_default();
    let request_id = request_value
        .get("request_id")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();
    let result = tauri::async_runtime::spawn_blocking(move || {
        agent::run_action(&host_for_action, &request_for_action, &request_id, move |ev| {
            let _ = app2.emit("agent-event", ev);
        })
    })
    .await
    .map_err(|e| e.to_string())?;

    // Audit log (best-effort — never blocks or fails the action).
    let req_value: serde_json::Value = serde_json::from_str(&request).unwrap_or_default();
    let action = req_value
        .get("action")
        .and_then(|a| a.as_str())
        .unwrap_or("unknown")
        .to_string();
    let params = req_value.get("params").map(|p| p.to_string());
    let exit = result.as_ref().map(|r| r.exit as i64).ok();
    let summary = match &result {
        Ok(r) => serde_json::json!({ "exit": r.exit, "events": r.events.len() }).to_string(),
        Err(e) => serde_json::json!({ "error": e }).to_string(),
    };
    let _ = audit::record(&host, &source, &action, params.as_deref(), approval, exit, Some(&summary));

    result
}

/// Abort a running agent action: kills the local ssh process group AND tells
/// the remote host to kill the user's agent processes. Killing the local ssh
/// alone does NOT stop the remote command — sshd keeps the session's process
/// running after the connection drops (verified), so we issue an explicit
/// remote `pkill` over a fresh connection. This is the "take back control"
/// escape hatch.
#[tauri::command]
fn stop_agent_action(request_id: String, host: Option<String>) -> bool {
    let killed = agent::kill_action(&request_id);
    if let Some(h) = host {
        let user = h.split('@').next().unwrap_or_default();
        let mut cmd = std::process::Command::new("ssh");
        cmd.args([
            "-o", "BatchMode=yes",
            "-o", "ControlMaster=auto",
            "-o", "ConnectTimeout=5",
        ])
        .arg(&h)
        .arg("pkill");
        if !user.is_empty() {
            cmd.args(["-TERM", "-u", user, "-f", "puppetterm-agent"]);
        } else {
            cmd.args(["-TERM", "-f", "puppetterm-agent"]);
        }
        let _ = cmd.output(); // best-effort; SIGTERM lets the agent clean up its command group
    }
    killed
}

/// Whether the puppetterm agent is already present on the host.
#[tauri::command]
fn check_agent(host: String) -> bool {
    install::check_agent(&host)
}

/// Install the puppetterm agent on a host over the existing SSH key
/// (user-space by default; upgraded to root when passwordless sudo exists).
/// Streams progress lines as `install-output` events.
#[tauri::command]
async fn install_agent_on_host(
    app: AppHandle,
    host: String,
    agent_dir: Option<String>,
    pubkey_path: Option<String>,
) -> Result<install::InstallResult, String> {
    // Make sure ControlMaster sharing is enabled so install works over the
    // user's interactive (possibly password-only) connection.
    let _ = ensure_ssh_control_master();
    // Resolve the agent binary dir: explicit param → env → source-tree
    // agent/bin (dev: `make cross` output) → bundled resource dir (packaged
    // builds) → home default.
    let agent_dir = agent_dir
        .or_else(|| std::env::var("PUPPETTERM_AGENT_DIR").ok().filter(|d| !d.is_empty()))
        .or_else(|| {
            // Dev: prefer the source-tree agent/bin (repo/agent/bin, built with
            // `make cross`). In `tauri dev` the resource dir is target/debug,
            // which NEVER contains the agent binary — picking it first made
            // installs fail with "agent binary not found".
            let dev = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../agent/bin");
            if dev.join("puppetterm-agent-linux-amd64").exists()
                || dev.join("puppetterm-agent-linux-arm64").exists()
            {
                Some(dev.to_string_lossy().into_owned())
            } else {
                // Packaged build: the binaries are bundled into the app resources.
                app.path()
                    .resource_dir()
                    .ok()
                    .map(|p| p.to_string_lossy().into_owned())
            }
        });
    let app2 = app.clone();
    let host2 = host.clone();
    tauri::async_runtime::spawn_blocking(move || {
        install::install_agent(&host2, agent_dir.as_deref(), pubkey_path, &|line| {
            let _ = app2.emit(
                "install-output",
                InstallOutput { host: host2.clone(), data: line.to_string() },
            );
        })
    })
    .await
    .map_err(|e| e.to_string())?
}

/// Return the most recent audit log entries (newest first).
#[tauri::command]
fn audit_recent(limit: Option<i64>) -> Result<Vec<audit::AuditRow>, String> {
    audit::recent(limit.unwrap_or(50))
}

/// Return the AI provider config (provider + endpoint + model + whether a key
/// is set). The API key itself is never returned to the frontend.
#[tauri::command]
fn get_ai_config() -> Result<ai::AiConfigView, String> {
    let cfg = ai::load_config()?;
    Ok(ai::AiConfigView {
        base_url: cfg.base_url,
        model: cfg.model,
        provider: cfg.provider,
        has_api_key: !cfg.api_key.is_empty(),
    })
}

/// Update the AI provider config on disk (key optional — kept if blank).
/// The key is encrypted at rest (ChaCha20-Poly1305, machine-bound).
#[tauri::command]
fn set_ai_config(
    base_url: String,
    model: String,
    provider: Option<String>,
    api_key: Option<String>,
) -> Result<(), String> {
    let mut cfg = match ai::load_config() {
        Ok(c) => c,
        Err(_) => ai::AiConfig {
            base_url: String::new(),
            api_key: String::new(),
            model: String::new(),
            provider: ai::PROVIDER_OPENAI.into(),
            api_key_enc: None,
        },
    };
    if !base_url.trim().is_empty() {
        cfg.base_url = base_url.trim().to_string();
    }
    if !model.trim().is_empty() {
        cfg.model = model.trim().to_string();
    }
    if let Some(p) = provider {
        let p = p.trim().to_string();
        if !p.is_empty() {
            cfg.provider = if p == ai::PROVIDER_ANTHROPIC
                || p == ai::PROVIDER_DEEPSEEK
                || p == ai::PROVIDER_OPENAI
            {
                p
            } else {
                ai::PROVIDER_OPENAI.into()
            };
        }
    }
    if let Some(k) = api_key {
        let k = k.trim();
        if !k.is_empty() {
            cfg.api_key = k.to_string();
        }
    }
    ai::save_config(&cfg)
}

/// Send a chat completion to the configured OpenAI-compatible endpoint,
/// including tool calls. The API key is read from disk/env, never the frontend.
#[tauri::command]
async fn ai_chat(
    messages: Vec<ai::ChatMessage>,
    tools: Option<Vec<ai::ToolDef>>,
) -> Result<ai::ChatResponse, String> {
    let cfg = ai::load_config()?;
    ai::chat_completion(&cfg, messages, tools, Some(4096)).await
}

/// Idempotently enable ssh ControlMaster sharing so password-only remotes work:
/// the user's interactive `ssh user@host` (password typed in the terminal)
/// becomes the multiplexed master connection; puppetterm's automated ssh calls
/// attach to it and skip re-authentication (no key needed).
///
/// Writes `~/.ssh/puppetterm-control` and `Include`s it from `~/.ssh/config`
/// (creating both if missing). Best-effort; safe to edit/delete either file.
fn ensure_ssh_control_master() -> Result<(), String> {
    let home = std::env::var("HOME").map_err(|_| "HOME unset".to_string())?;
    let ssh_dir = std::path::Path::new(&home).join(".ssh");
    let extra = ssh_dir.join("puppetterm-control");
    let main = ssh_dir.join("config");
    std::fs::create_dir_all(&ssh_dir).map_err(|e| e.to_string())?;
    // ssh does NOT create the ControlPath parent dir; the master would fail to
    // listen without it.
    std::fs::create_dir_all(ssh_dir.join("puppetterm-mux")).map_err(|e| e.to_string())?;
    std::fs::write(
        &extra,
        r#"# puppetterm: share one authenticated connection per host (ControlMaster).
# Lets password-only remotes (no local key) work for agent install/actions:
# the interactive `ssh user@host` you type here becomes the master socket;
# puppetterm's automated ssh calls attach to it. Safe to edit or delete.
Host *
    # `yes` (not `auto`): the FIRST connection creates the master — that's the
    # user's interactive ssh with the password. `auto` would only reuse, never
    # create, so password remotes would never get a socket.
    ControlMaster yes
    ControlPath ~/.ssh/puppetterm-mux/%r@%h:%p
    ControlPersist 600
    ServerAliveInterval 30
"#,
    )
    .map_err(|e| e.to_string())?;
    let include = format!("Include {}", extra.display());
    let body = std::fs::read_to_string(&main).unwrap_or_default();
    // The Include MUST be at the TOP of the main config: OpenSSH has a quirk
    // where an Include placed after a `Host` block makes the included `Host *`
    // stop matching non-alias targets (e.g. `isr@192.168.150.22` got
    // controlmaster=false while the `server1` alias got true). Drop any
    // existing (possibly mis-positioned) occurrence, then prepend once.
    let filtered: Vec<&str> = body.lines().filter(|l| l.trim() != include.trim()).collect();
    let merged = if filtered.is_empty() {
        format!("{include}\n")
    } else {
        format!("{include}\n{}\n", filtered.join("\n").trim_end())
    };
    std::fs::write(&main, merged).map_err(|e| e.to_string())?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// ensure_ssh_control_master is idempotent and writes the expected layout.
    #[test]
    fn ssh_control_master_config_is_idempotent() {
        let dir = std::env::temp_dir().join(format!("puppetterm-ctl-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        std::env::set_var("HOME", &dir);

        // Existing user config should be preserved, Include appended once.
        let main = dir.join(".ssh/config");
        std::fs::create_dir_all(dir.join(".ssh")).unwrap();
        std::fs::write(&main, "Host server1\n    User ubuntu\n").unwrap();

        ensure_ssh_control_master().unwrap();
        ensure_ssh_control_master().unwrap(); // second run must not duplicate

        let body = std::fs::read_to_string(&main).unwrap();
        let inc = format!("Include {}/.ssh/puppetterm-control", dir.display());
        assert!(body.contains("Host server1"), "existing config preserved");
        assert_eq!(body.matches(&inc).count(), 1, "Include added exactly once");
        assert!(
            body.trim_start().starts_with(&inc),
            "Include must be at the TOP (a trailing Include breaks `Host *` matching)"
        );

        let ctl = std::fs::read_to_string(dir.join(".ssh/puppetterm-control")).unwrap();
        assert!(ctl.contains("ControlMaster yes"), "master must be `yes` (auto only reuses)");
        assert!(ctl.contains("ControlPath ~/.ssh/puppetterm-mux/%r@%h:%p"));
        assert!(
            dir.join(".ssh/puppetterm-mux").is_dir(),
            "ControlPath parent dir must exist (ssh won't create it)"
        );

        std::env::remove_var("HOME");
        let _ = std::fs::remove_dir_all(&dir);
    }
}

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    tauri::Builder::default()
        .plugin(tauri_plugin_opener::init())
        .plugin(tauri_plugin_clipboard_manager::init())
        .manage(AppState::default())
        .setup(|_app| {
            // Best-effort: enable ControlMaster sharing up-front so a
            // password-authenticated interactive ssh can back agent actions.
            let _ = ensure_ssh_control_master();
            Ok(())
        })
        .invoke_handler(tauri::generate_handler![
            list_ssh_hosts,
            check_host,
            start_ssh_session,
            start_local_session,
            write_ssh_input,
            resize_ssh_pty,
            stop_ssh_session,
            run_agent_action,
            stop_agent_action,
            check_agent,
            install_agent_on_host,
            audit_recent,
            get_ai_config,
            set_ai_config,
            ai_chat
        ])
        .run(tauri::generate_context!())
        .expect("error while running tauri application");
}
