//! HTTP API — one endpoint per frontend "command", mirroring the Tauri IPC
//! surface exactly so the same Svelte frontend runs against either backend.

use std::sync::Arc;

use axum::body::Bytes;
use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response as AxumResponse};
use axum::Json;
use futures_util::{SinkExt, StreamExt};
use puppetterm_core::sessions::Emitter;
use serde_json::{json, Value};

use crate::hub::EventHub;
use crate::App;

// ---- response helpers ------------------------------------------------------

fn ok(v: Value) -> AxumResponse {
    (StatusCode::OK, Json(v)).into_response()
}

fn err(e: String) -> AxumResponse {
    (StatusCode::INTERNAL_SERVER_ERROR, Json(json!({ "error": e }))).into_response()
}

/// Parse the request body as JSON, tolerating empty bodies (`{}` commands).
fn parse_body(bytes: &Bytes) -> Value {
    if bytes.is_empty() {
        Value::Null
    } else {
        serde_json::from_slice(bytes).unwrap_or(Value::Null)
    }
}

fn arg_str(args: &Value, key: &str) -> String {
    args.get(key).and_then(|v| v.as_str()).unwrap_or_default().to_string()
}

fn arg_u32(args: &Value, key: &str) -> Option<u32> {
    args.get(key).and_then(|v| v.as_u64()).map(|v| v as u32)
}

/// Map a core emitter callback onto the event hub.
fn hub_emitter(hub: &Arc<EventHub>) -> Emitter {
    let hub = hub.clone();
    Arc::new(move |event, payload| hub.emit(event, payload))
}

// ---- command dispatch ------------------------------------------------------

pub async fn command(State(app): State<App>, Path(cmd): Path<String>, body: Bytes) -> AxumResponse {
    let args = parse_body(&body);
    match cmd.as_str() {
        // ---- terminal sessions --------------------------------------------
        "list_ssh_hosts" => {
            run_blocking(|| Ok(json!(puppetterm_core::ssh::parse_ssh_config_hosts()))).await
        }
        "check_host" => {
            let host = arg_str(&args, "host");
            run_blocking(move || Ok(json!(puppetterm_core::ssh::check_host(&host)))).await
        }
        "start_ssh_session" => {
            let host = arg_str(&args, "host");
            let emit = hub_emitter(&app.hub);
            let sessions = app.sessions.clone();
            run_blocking(move || sessions.spawn_ssh(emit, &host).map(|id| json!(id))).await
        }
        "start_local_session" => {
            let emit = hub_emitter(&app.hub);
            let sessions = app.sessions.clone();
            run_blocking(move || sessions.spawn_local(emit).map(|id| json!(id))).await
        }
        "write_ssh_input" => {
            let id = arg_u32(&args, "id");
            let data = arg_str(&args, "data");
            let sessions = app.sessions.clone();
            run_blocking(move || {
                let id = id.ok_or("missing id")?;
                sessions.write_input(id, &data)?;
                Ok(json!(null))
            })
            .await
        }
        "resize_ssh_pty" => {
            let id = arg_u32(&args, "id");
            let cols = args.get("cols").and_then(|v| v.as_u64()).unwrap_or(80) as u16;
            let rows = args.get("rows").and_then(|v| v.as_u64()).unwrap_or(24) as u16;
            let sessions = app.sessions.clone();
            run_blocking(move || {
                let id = id.ok_or("missing id")?;
                sessions.resize(id, cols, rows)?;
                Ok(json!(null))
            })
            .await
        }
        "stop_ssh_session" => {
            let id = arg_u32(&args, "id");
            let sessions = app.sessions.clone();
            run_blocking(move || {
                let id = id.ok_or("missing id")?;
                sessions.stop(id)?;
                Ok(json!(null))
            })
            .await
        }

        // ---- remote agent ---------------------------------------------------
        "run_agent_action" => {
            let host = arg_str(&args, "host");
            let request = arg_str(&args, "request");
            let source = arg_str(&args, "source");
            let source = if source.is_empty() { "user".to_string() } else { source };
            let approved = args.get("approved").and_then(|v| v.as_bool());
            let approval = match approved {
                Some(true) => "approved",
                Some(false) => "rejected",
                None => "auto",
            };
            let emit = hub_emitter(&app.hub);
            run_blocking(move || {
                let request_value: Value = serde_json::from_str(&request).unwrap_or_default();
                let request_id =
                    request_value.get("request_id").and_then(|v| v.as_str()).unwrap_or("").to_string();
                let result =
                    puppetterm_core::agent::run_action(&host, &request, &request_id, move |ev| {
                        emit("agent-event", serde_json::to_value(ev).unwrap_or_default());
                    });
                // Audit log (best-effort — never blocks or fails the action).
                let action = request_value
                    .get("action")
                    .and_then(|a| a.as_str())
                    .unwrap_or("unknown")
                    .to_string();
                let params = request_value.get("params").map(|p| p.to_string());
                let exit = result.as_ref().map(|r| r.exit as i64).ok();
                let summary = match &result {
                    Ok(r) => json!({ "exit": r.exit, "events": r.events.len() }).to_string(),
                    Err(e) => json!({ "error": e }).to_string(),
                };
                let _ = puppetterm_core::audit::record(
                    &host,
                    &source,
                    &action,
                    params.as_deref(),
                    approval,
                    exit,
                    Some(&summary),
                );
                result.map(|r| serde_json::to_value(r).unwrap_or(Value::Null))
            })
            .await
        }
        "stop_agent_action" => {
            let request_id = arg_str(&args, "request_id");
            let host = args.get("host").and_then(|v| v.as_str()).map(String::from);
            run_blocking(move || {
                let killed = puppetterm_core::agent::kill_action(&request_id);
                // Remote pkill best-effort over a fresh connection (the local
                // kill alone does not stop the remote process group).
                if let Some(h) = host {
                    let user = h.split('@').next().unwrap_or_default().to_string();
                    std::thread::spawn(move || {
                        let mut cmd = std::process::Command::new("ssh");
                        cmd.args(["-o", "BatchMode=yes", "-o", "ConnectTimeout=5"])
                            .arg(&h)
                            .arg("pkill");
                        if !user.is_empty() {
                            cmd.args(["-TERM", "-u", &user, "-f", "puppetterm-agent"]);
                        } else {
                            cmd.args(["-TERM", "-f", "puppetterm-agent"]);
                        }
                        let _ = cmd.output();
                    });
                }
                Ok(json!(killed))
            })
            .await
        }
        "check_agent" => {
            let host = arg_str(&args, "host");
            run_blocking(move || Ok(json!(puppetterm_core::install::check_agent(&host)))).await
        }
        "ssh_capture" => {
            let host = arg_str(&args, "host");
            let cmd2 = arg_str(&args, "cmd");
            match puppetterm_core::run_ssh_capture(host, cmd2).await {
                Ok(v) => ok(v),
                Err(e) => err(e),
            }
        }
        "install_agent_on_host" => {
            let host = arg_str(&args, "host");
            let agent_dir = args.get("agent_dir").and_then(|v| v.as_str()).map(String::from);
            let pubkey_path = args.get("pubkey_path").and_then(|v| v.as_str()).map(String::from);
            let force = args.get("force").and_then(|v| v.as_bool()).unwrap_or(false);
            let emit = hub_emitter(&app.hub);
            run_blocking(move || {
                puppetterm_core::install::install_agent(
                    &host,
                    agent_dir.as_deref(),
                    pubkey_path,
                    force,
                    &|line| {
                        emit(
                            "install-output",
                            json!({ "host": host.clone(), "data": line.to_string() }),
                        );
                    },
                )
                .map(|r| serde_json::to_value(r).unwrap_or(Value::Null))
            })
            .await
        }

        // ---- audit ----------------------------------------------------------
        "audit_recent" => {
            let limit = args.get("limit").and_then(|v| v.as_i64()).unwrap_or(50);
            run_blocking(move || {
                puppetterm_core::audit::recent(limit)
                    .map(|rows| serde_json::to_value(rows).unwrap_or(Value::Null))
            })
            .await
        }

        // ---- AI ---------------------------------------------------------------
        "get_ai_config" => run_blocking(move || {
            puppetterm_core::ai::load_config().map(|cfg| {
                json!({
                    "base_url": cfg.base_url,
                    "model": cfg.model,
                    "provider": cfg.provider,
                    "has_api_key": !cfg.api_key.is_empty(),
                })
            })
        })
        .await,
        "set_ai_config" => run_blocking(move || {
            puppetterm_core::ai::apply_ai_config(
                arg_str(&args, "base_url"),
                arg_str(&args, "model"),
                args.get("provider").and_then(|v| v.as_str()).map(String::from),
                args.get("api_key").and_then(|v| v.as_str()).map(String::from),
            )?;
            Ok(json!(null))
        })
        .await,
        "ai_chat" => {
            let messages: Vec<puppetterm_core::ai::ChatMessage> = match args.get("messages") {
                Some(m) => match serde_json::from_value(m.clone()) {
                    Ok(m) => m,
                    Err(e) => return err(format!("invalid messages: {e}")),
                },
                None => return err("missing messages".into()),
            };
            let tools: Option<Vec<puppetterm_core::ai::ToolDef>> = match args.get("tools") {
                Some(t) => match serde_json::from_value(t.clone()) {
                    Ok(t) => t,
                    Err(e) => return err(format!("invalid tools: {e}")),
                },
                None => None,
            };
            match puppetterm_core::ai::load_config() {
                Ok(cfg) => match puppetterm_core::ai::chat_completion(&cfg, messages, tools, Some(4096)).await {
                    Ok(resp) => ok(serde_json::to_value(resp).unwrap_or(Value::Null)),
                    Err(e) => err(e),
                },
                Err(e) => err(e),
            }
        }

        other => err(format!("unknown command: {other}")),
    }
}

/// Run sync work off the async runtime.
async fn run_blocking<F>(f: F) -> AxumResponse
where
    F: FnOnce() -> Result<Value, String> + Send + 'static,
{
    match tokio::task::spawn_blocking(f).await {
        Ok(Ok(v)) => ok(v),
        Ok(Err(e)) => err(e),
        Err(e) => err(format!("internal error: {e}")),
    }
}

// ---- websocket -------------------------------------------------------------

pub async fn ws_upgrade(ws: WebSocketUpgrade, State(app): State<App>) -> AxumResponse {
    ws.on_upgrade(move |socket| handle_socket(socket, app))
}

async fn handle_socket(socket: WebSocket, app: App) {
    let (mut sender, mut receiver) = socket.split();
    let mut events = app.hub.subscribe();
    let mut ping = tokio::time::interval(std::time::Duration::from_secs(20));
    ping.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);

    loop {
        tokio::select! {
            frame = events.recv() => {
                match frame {
                    Ok(text) => {
                        if sender.send(Message::Text(text.into())).await.is_err() {
                            break;
                        }
                    }
                    Err(tokio::sync::broadcast::error::RecvError::Lagged(n)) => {
                        eprintln!("[puppetterm] ws client lagged, dropped {n} frames");
                        continue;
                    }
                    Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
                }
            }
            _ = ping.tick() => {
                if sender.send(Message::Ping(Vec::new().into())).await.is_err() {
                    break;
                }
            }
            msg = receiver.next() => {
                match msg {
                    Some(Ok(_)) => {} // ignore client-to-server messages
                    _ => break,
                }
            }
        }
    }
}
