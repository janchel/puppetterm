//! puppetterm client backend.
//!
//! Manages interactive SSH sessions backed by a pty (via `portable-pty`),
//! streams pty output to the webview as events, and provides host discovery
//! from ~/.ssh/config.

mod agent;
mod ai;
mod audit;
mod ssh;

use std::collections::HashMap;
use std::io::{Read, Write};
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::Mutex;

use portable_pty::{native_pty_system, CommandBuilder, MasterPty, PtySize};
use serde::Serialize;
use tauri::{AppHandle, Emitter, State};

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
    let result = tauri::async_runtime::spawn_blocking(move || {
        agent::run_action(&host_for_action, &request_for_action, move |ev| {
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

/// Return the most recent audit log entries (newest first).
#[tauri::command]
fn audit_recent(limit: Option<i64>) -> Result<Vec<audit::AuditRow>, String> {
    audit::recent(limit.unwrap_or(50))
}

/// Return the AI provider config (endpoint + model + whether a key is set).
/// The API key itself is never returned to the frontend.
#[tauri::command]
fn get_ai_config() -> Result<ai::AiConfigView, String> {
    let cfg = ai::load_config()?;
    Ok(ai::AiConfigView {
        base_url: cfg.base_url,
        model: cfg.model,
        has_api_key: !cfg.api_key.is_empty(),
    })
}

/// Update the AI provider config on disk (key optional — kept if blank).
#[tauri::command]
fn set_ai_config(
    base_url: String,
    model: String,
    api_key: Option<String>,
) -> Result<(), String> {
    let mut cfg = match ai::load_config() {
        Ok(c) => c,
        Err(_) => ai::AiConfig { base_url: String::new(), api_key: String::new(), model: String::new() },
    };
    if !base_url.trim().is_empty() {
        cfg.base_url = base_url.trim().to_string();
    }
    if !model.trim().is_empty() {
        cfg.model = model.trim().to_string();
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

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    tauri::Builder::default()
        .plugin(tauri_plugin_opener::init())
        .plugin(tauri_plugin_clipboard_manager::init())
        .manage(AppState::default())
        .invoke_handler(tauri::generate_handler![
            list_ssh_hosts,
            check_host,
            start_ssh_session,
            start_local_session,
            write_ssh_input,
            resize_ssh_pty,
            stop_ssh_session,
            run_agent_action,
            audit_recent,
            get_ai_config,
            set_ai_config,
            ai_chat
        ])
        .run(tauri::generate_context!())
        .expect("error while running tauri application");
}
