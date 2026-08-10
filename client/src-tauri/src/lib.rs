//! puppetterm client backend.
//!
//! Manages interactive SSH sessions backed by a pty (via `portable-pty`),
//! streams pty output to the webview as events, and provides host discovery
//! from ~/.ssh/config.

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

/// Open an interactive SSH session to `host` and start streaming its pty.
#[tauri::command]
fn start_ssh_session(
    app: AppHandle,
    state: State<'_, AppState>,
    host: String,
) -> Result<u32, String> {
    let id = state.next_id.fetch_add(1, Ordering::SeqCst);

    let pty_system = native_pty_system();
    let pair = pty_system
        .openpty(PtySize { rows: 24, cols: 80, pixel_width: 0, pixel_height: 0 })
        .map_err(|e| e.to_string())?;

    let mut cmd = CommandBuilder::new("ssh");
    cmd.args(["-tt", &host]);
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

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    tauri::Builder::default()
        .plugin(tauri_plugin_opener::init())
        .manage(AppState::default())
        .invoke_handler(tauri::generate_handler![
            list_ssh_hosts,
            check_host,
            start_ssh_session,
            write_ssh_input,
            resize_ssh_pty,
            stop_ssh_session
        ])
        .run(tauri::generate_context!())
        .expect("error while running tauri application");
}
