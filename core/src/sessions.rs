//! Interactive terminal session management (pty-backed SSH / local shells).
//!
//! Shared by the Tauri desktop shell and the headless web server. The caller
//! supplies an `Emitter` — a callback that receives `(event_name, payload)` —
//! which the Tauri shell maps to IPC events and the web server maps to
//! WebSocket frames.

use std::collections::HashMap;
use std::io::{Read, Write};
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Mutex};

use portable_pty::{native_pty_system, CommandBuilder, MasterPty, PtySize};
use serde_json::json;

/// Callback used to stream events (`pty-output`, `pty-exit`, …) to the UI.
pub type Emitter = Arc<dyn Fn(&str, serde_json::Value) + Send + Sync>;

/// One live terminal session backed by a pty.
struct Session {
    master: Box<dyn MasterPty + Send>,
    writer: Box<dyn Write + Send>,
    child: Box<dyn portable_pty::Child + Send + Sync>,
}

/// All live sessions. Ids are process-global so both frontends (desktop and
/// web) can share one backend instance.
#[derive(Default)]
pub struct SessionManager {
    sessions: Mutex<HashMap<u32, Session>>,
    next_id: AtomicU32,
}

impl SessionManager {
    pub fn new() -> Self {
        Self::default()
    }

    /// Open a pty running `cmd` (optionally in `cwd`) and stream its output as
    /// `pty-output`/`pty-exit` events. Returns the new session id.
    pub fn spawn(
        &self,
        emit: Emitter,
        cmd_name: &str,
        args: &[&str],
        cwd: Option<&std::path::Path>,
    ) -> Result<u32, String> {
        let id = self.next_id.fetch_add(1, Ordering::SeqCst);

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
        let emit_out = emit.clone();
        std::thread::spawn(move || {
            let mut buf = [0u8; 8192];
            loop {
                match reader.read(&mut buf) {
                    Ok(0) | Err(_) => break,
                    Ok(n) => {
                        let data = String::from_utf8_lossy(&buf[..n]).to_string();
                        emit_out("pty-output", json!({ "id": id, "data": data }));
                    }
                }
            }
            emit("pty-exit", json!({ "id": id }));
        });

        self.sessions.lock().unwrap().insert(id, Session { master: pair.master, writer, child });

        Ok(id)
    }

    /// Open an interactive SSH session to `host`.
    pub fn spawn_ssh(&self, emit: Emitter, host: &str) -> Result<u32, String> {
        self.spawn(emit, "ssh", &["-tt", host], None)
    }

    /// Open a local shell in $HOME (no remote connection).
    pub fn spawn_local(&self, emit: Emitter) -> Result<u32, String> {
        let shell = std::env::var("SHELL").unwrap_or_else(|_| "/bin/bash".into());
        let home = std::env::var("HOME").unwrap_or_else(|_| "/".into());
        self.spawn(emit, &shell, &[], Some(std::path::Path::new(&home)))
    }

    /// Send terminal input (keystrokes) to the session.
    pub fn write_input(&self, id: u32, data: &str) -> Result<(), String> {
        let mut sessions = self.sessions.lock().unwrap();
        let s = sessions.get_mut(&id).ok_or("no such session")?;
        s.writer.write_all(data.as_bytes()).map_err(|e| e.to_string())?;
        s.writer.flush().map_err(|e| e.to_string())
    }

    /// Resize the pty to match the frontend terminal.
    pub fn resize(&self, id: u32, cols: u16, rows: u16) -> Result<(), String> {
        let sessions = self.sessions.lock().unwrap();
        let s = sessions.get(&id).ok_or("no such session")?;
        s.master
            .resize(PtySize { rows, cols, pixel_width: 0, pixel_height: 0 })
            .map_err(|e| e.to_string())
    }

    /// Terminate a session and release its resources.
    pub fn stop(&self, id: u32) -> Result<(), String> {
        if let Some(mut s) = self.sessions.lock().unwrap().remove(&id) {
            let _ = s.child.kill();
        }
        Ok(())
    }
}

/// Run a command; return true if it exits 0 within `ms` (killed on timeout).
pub fn run_with_timeout(args: &[&str], ms: u64) -> bool {
    use std::time::{Duration, Instant};
    let mut child = match std::process::Command::new(&args[0])
        .args(&args[1..])
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
    {
        Ok(c) => c,
        Err(_) => return false,
    };
    let start = Instant::now();
    loop {
        if let Some(st) = child.try_wait().unwrap_or(None) {
            return st.success();
        }
        if start.elapsed() >= Duration::from_millis(ms) {
            let _ = child.kill();
            let _ = child.wait();
            return false;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
}

/// Remove ControlMaster sockets whose master is dead OR broken. A broken master
/// — alive enough to answer `-O check` but unable to serve a new session — makes
/// the user's next interactive `ssh` hang or fail with "PTY allocation request
/// failed", so each socket is probed by attaching a throwaway session. Best-effort.
pub fn cleanup_stale_masters() {
    let home = match std::env::var("HOME") {
        Ok(h) => h,
        Err(_) => return,
    };
    let mux_dir = std::path::Path::new(&home).join(".ssh/puppetterm-mux");
    let entries = match std::fs::read_dir(&mux_dir) {
        Ok(e) => e,
        Err(_) => return,
    };
    for entry in entries.flatten() {
        let path = entry.path();
        // Socket filenames are `user@host:port` (no `.sock` extension), so
        // process every non-directory entry in the mux dir.
        if entry.file_type().map(|t| t.is_dir()).unwrap_or(false) {
            continue;
        }
        let Some(name) = entry.file_name().to_str().map(|s| s.to_string()) else {
            continue;
        };
        // Socket filename is `user@host:port` (from %r@%h:%p).
        let (user, host_port) = match name.rfind('@') {
            Some(i) => (name[..i].to_string(), name[i + 1..].to_string()),
            None => (String::new(), name.clone()),
        };
        let (host, port) = match host_port.rfind(':') {
            Some(i) => (host_port[..i].to_string(), host_port[i + 1..].to_string()),
            None => (host_port.clone(), "22".to_string()),
        };
        let target = if user.is_empty() {
            host
        } else {
            format!("{user}@{host}")
        };
        let sock = path.to_string_lossy().into_owned();
        // Attach a throwaway session: healthy master → `true` returns 0 fast;
        // broken/dead master → fails or hangs (killed by the timeout).
        let ok = run_with_timeout(
            &[
                "ssh",
                "-S", &sock,
                "-o", "ControlMaster=no",
                "-o", "BatchMode=yes",
                "-o", "ConnectTimeout=2",
                "-o", "StrictHostKeyChecking=accept-new",
                "-p", &port,
                &target,
                "true",
            ],
            4000,
        );
        if !ok {
            let _ = std::fs::remove_file(&path);
            eprintln!("[puppetterm] removed broken ControlMaster socket {name}");
        }
    }
}
