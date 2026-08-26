//! puppetterm desktop shell (Tauri).
//!
//! Thin command wrappers around `puppetterm-core`. All real logic — SSH
//! sessions, remote agent, AI client, audit log, installer — lives in core so
//! the headless web server (`server/`) shares exactly the same behavior.

use std::sync::Arc;

use puppetterm_core as core;
use puppetterm_core::sessions::{Emitter, SessionManager};

use tauri::{AppHandle, Emitter as _, Manager, State};

/// All live terminal sessions.
#[derive(Default)]
struct AppState {
    sessions: SessionManager,
}

/// Map a core emitter callback onto Tauri IPC events.
fn tauri_emitter(app: &AppHandle) -> Emitter {
    let app = app.clone();
    Arc::new(move |event, payload| {
        let _ = app.emit(event, payload);
    })
}

/// List concrete host aliases from ~/.ssh/config.
#[tauri::command]
fn list_ssh_hosts() -> Vec<String> {
    core::ssh::parse_ssh_config_hosts()
}

/// Quick reachability probe for a host (used for status dots).
#[tauri::command]
fn check_host(host: String) -> bool {
    core::ssh::check_host(&host)
}

/// Open an interactive SSH session to `host` and start streaming its pty.
#[tauri::command]
fn start_ssh_session(
    app: AppHandle,
    state: State<'_, AppState>,
    host: String,
) -> Result<u32, String> {
    state.sessions.spawn_ssh(tauri_emitter(&app), &host)
}

/// Open a local shell in the user's home directory (no remote connection).
#[tauri::command]
fn start_local_session(app: AppHandle, state: State<'_, AppState>) -> Result<u32, String> {
    state.sessions.spawn_local(tauri_emitter(&app))
}

/// Send terminal input (keystrokes) to the session.
#[tauri::command]
fn write_ssh_input(state: State<'_, AppState>, id: u32, data: String) -> Result<(), String> {
    state.sessions.write_input(id, &data)
}

/// Resize the remote pty to match the frontend terminal.
#[tauri::command]
fn resize_ssh_pty(state: State<'_, AppState>, id: u32, cols: u16, rows: u16) -> Result<(), String> {
    state.sessions.resize(id, cols, rows)
}

/// Terminate a session and release its resources.
#[tauri::command]
fn stop_ssh_session(state: State<'_, AppState>, id: u32) -> Result<(), String> {
    state.sessions.stop(id)
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
) -> Result<core::agent::AgentRunResult, String> {
    let emit = tauri_emitter(&app);
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
        core::agent::run_action(&host_for_action, &request_for_action, &request_id, move |ev| {
            emit("agent-event", serde_json::to_value(ev).unwrap_or_default());
        })
    })
    .await
    .map_err(|e| e.to_string())?;

    // Audit log (best-effort — never blocks or fails the action).
    record_agent_audit(&host, &source, approval, &request, &result);

    result
}

/// Shared audit bookkeeping for one completed agent action (best-effort).
fn record_agent_audit(
    host: &str,
    source: &str,
    approval: &str,
    request: &str,
    result: &Result<core::agent::AgentRunResult, String>,
) {
    let req_value: serde_json::Value = serde_json::from_str(request).unwrap_or_default();
    let action = req_value
        .get("action")
        .and_then(|a| a.as_str())
        .unwrap_or("unknown")
        .to_string();
    let params = req_value.get("params").map(|p| p.to_string());
    let exit = result.as_ref().map(|r| r.exit as i64).ok();
    let summary = match result {
        Ok(r) => serde_json::json!({ "exit": r.exit, "events": r.events.len() }).to_string(),
        Err(e) => serde_json::json!({ "error": e }).to_string(),
    };
    let _ = core::audit::record(host, source, &action, params.as_deref(), approval, exit, Some(&summary));
}

/// Abort a running agent action: kills the local ssh process group AND tells
/// the remote host to kill the user's agent processes. Killing the local ssh
/// alone does NOT stop the remote command — sshd keeps the session's process
/// running after the connection drops (verified), so we issue an explicit
/// remote `pkill` over a fresh connection. This is the "take back control"
/// escape hatch.
#[tauri::command]
fn stop_agent_action(request_id: String, host: Option<String>) -> bool {
    let killed = core::agent::kill_action(&request_id);
    if let Some(h) = host {
        let user = h.split('@').next().unwrap_or_default();
        let mut cmd = std::process::Command::new("ssh");
        cmd.args(["-o", "BatchMode=yes", "-o", "ConnectTimeout=5"])
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

/// Run a command on a host over a plain key-based SSH connection and return
/// the FULL captured stdout (no pty, no agent). Used for file reads when the
/// agent binary isn't installed — reading through the live terminal (typing
/// `cat` and scraping the xterm buffer) can cut large files at scrollback or
/// settle-time limits, but a direct SSH exec returns every byte.
#[tauri::command]
async fn ssh_capture(host: String, cmd: String) -> Result<serde_json::Value, String> {
    core::run_ssh_capture(host, cmd).await
}

/// Whether the puppetterm agent is already present on the host.
#[tauri::command]
fn check_agent(host: String) -> bool {
    core::install::check_agent(&host)
}

/// Resolve the directory holding the prebuilt agent binaries (dev tree or env).
fn agent_bin_dir() -> Option<String> {
    std::env::var("PUPPETTERM_AGENT_DIR").ok().filter(|d| !d.is_empty()).or_else(|| {
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
            None
        }
    })
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
    force: Option<bool>,
) -> Result<core::install::InstallResult, String> {
    // Resolve the agent binary dir: explicit param → env → dev source tree →
    // bundled resource dir (packaged builds).
    let agent_dir = agent_dir.or_else(agent_bin_dir).or_else(|| {
        app.path()
            .resource_dir()
            .ok()
            .map(|p| p.to_string_lossy().into_owned())
    });
    let emit = tauri_emitter(&app);
    let host2 = host.clone();
    tauri::async_runtime::spawn_blocking(move || {
        core::install::install_agent(
            &host2,
            agent_dir.as_deref(),
            pubkey_path,
            force.unwrap_or(false),
            &|line| {
                emit(
                    "install-output",
                    serde_json::json!({ "host": host2.clone(), "data": line }),
                );
            },
        )
    })
    .await
    .map_err(|e| e.to_string())?
}

/// Return the most recent audit log entries (newest first).
#[tauri::command]
fn audit_recent(limit: Option<i64>) -> Result<Vec<core::audit::AuditRow>, String> {
    core::audit::recent(limit.unwrap_or(50))
}

/// Return the AI provider config (provider + endpoint + model + whether a key
/// is set). The API key itself is never returned to the frontend.
#[tauri::command]
fn get_ai_config() -> Result<core::ai::AiConfigView, String> {
    let cfg = core::ai::load_config()?;
    Ok(core::ai::AiConfigView {
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
    core::ai::apply_ai_config(base_url, model, provider, api_key)
}

/// Send a chat completion to the configured OpenAI-compatible endpoint,
/// including tool calls. The API key is read from disk/env, never the frontend.
#[tauri::command]
async fn ai_chat(
    messages: Vec<core::ai::ChatMessage>,
    tools: Option<Vec<core::ai::ToolDef>>,
) -> Result<core::ai::ChatResponse, String> {
    let cfg = core::ai::load_config()?;
    core::ai::chat_completion(&cfg, messages, tools, Some(4096)).await
}

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    tauri::Builder::default()
        .plugin(tauri_plugin_opener::init())
        .plugin(tauri_plugin_clipboard_manager::init())
        .manage(AppState::default())
        .setup(|_app| {
            // Clean up any leftover ControlMaster sockets from the old approach.
            std::thread::spawn(core::sessions::cleanup_stale_masters);
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
            ssh_capture,
            install_agent_on_host,
            audit_recent,
            get_ai_config,
            set_ai_config,
            ai_chat
        ])
        .run(tauri::generate_context!())
        .expect("error while running tauri application");
}
