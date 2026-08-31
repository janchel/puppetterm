//! HTTP API — one endpoint per frontend "command", mirroring the Tauri IPC
//! surface exactly so the same Svelte frontend runs against either backend.

use std::sync::Arc;

use axum::body::Bytes;
use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::response::{Html, IntoResponse, Response as AxumResponse};
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
                // Full output (verbose) goes to a file keyed by the new row id;
                // the DB row keeps only the light summary above. This keeps the
                // audit index small and means the AI never ingests history
                // output as context — details are pulled on demand by the UI.
                let detail_json = match &result {
                    Ok(r) => serde_json::to_string(r).unwrap_or_default(),
                    Err(e) => json!({ "error": e }).to_string(),
                };
                if let Ok(id) = puppetterm_core::audit::record(
                    &host,
                    &source,
                    &action,
                    params.as_deref(),
                    approval,
                    exit,
                    Some(&summary),
                ) {
                    let _ = puppetterm_core::audit::write_detail(id, &detail_json);
                }
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
        "audit_detail" => {
            let id = arg_str(&args, "id")
                .parse::<i64>()
                .unwrap_or(-1);
            run_blocking(move || {
                puppetterm_core::audit::read_detail(id).map(|opt| {
                    json!({ "id": id, "detail": opt })
                })
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
                    "auth_method": cfg.auth_method,
                    "oauth": {
                        "auth_url": cfg.oauth.auth_url,
                        "token_url": cfg.oauth.token_url,
                        "client_id": cfg.oauth.client_id,
                        "scope": cfg.oauth.scope,
                        "redirect_uri": cfg.oauth.redirect_uri,
                        "flow": cfg.oauth.flow,
                        "has_client_secret": !cfg.oauth.client_secret.is_empty(),
                    },
                })
            })
        })
        .await,
        "set_ai_config" => run_blocking(move || {
            let auth_method = args
                .get("auth_method")
                .and_then(|v| v.as_str())
                .map(String::from);
            let oauth: Option<puppetterm_core::ai::AiOAuthMeta> = args
                .get("oauth")
                .and_then(|v| serde_json::from_value(v.clone()).ok());
            puppetterm_core::ai::apply_ai_config(
                arg_str(&args, "base_url"),
                arg_str(&args, "model"),
                args.get("provider").and_then(|v| v.as_str()).map(String::from),
                args.get("api_key").and_then(|v| v.as_str()).map(String::from),
                auth_method,
                oauth,
            )?;
            Ok(json!(null))
        })
        .await,
        "ai_oauth_begin" => run_blocking(|| {
            puppetterm_core::ai::begin_oauth().map(|b| {
                json!({ "authorize_url": b.authorize_url, "state": b.state })
            })
        })
        .await,
        "list_ai_models" => {
            // If a specific provider is requested (for per-provider Models tab), use it.
            // Otherwise fall back to the active config (ai.json).
            if let Some(id) = args.get("provider_id").and_then(|v| v.as_str()).filter(|s| !s.is_empty()) {
                match puppetterm_core::ai::list_provider_models(id).await {
                    Ok(models) => ok(json!({ "models": models })),
                    Err(e) => err(e),
                }
            } else {
                match puppetterm_core::ai::load_config() {
                    Ok(mut cfg) => {
                        if let Err(e) = puppetterm_core::ai::ensure_valid_token(&mut cfg).await {
                            return err(e);
                        }
                        match puppetterm_core::ai::list_models(&cfg).await {
                            Ok(models) => ok(json!({ "models": models })),
                            Err(e) => err(e),
                        }
                    }
                    Err(e) => err(e),
                }
            }
        }
        "list_providers" => run_blocking(|| {
            let providers = puppetterm_core::ai::load_providers()?;
            let views: Vec<Value> = providers
                .into_iter()
                .map(|p| {
                    json!({
                        "id": p.id,
                        "label": p.label,
                        "base_url": p.base_url,
                        "model": p.model,
                        "provider": p.provider,
                        "auth_method": p.auth_method,
                        "enabled": p.enabled,
                        "has_api_key": !p.api_key.is_empty(),
                    })
                })
                .collect();
            Ok(json!({ "providers": views }))
        })
        .await,
        "add_provider" => run_blocking(move || {
            let label = arg_str(&args, "label");
            let base_url = arg_str(&args, "base_url");
            let model = arg_str(&args, "model");
            let provider = args.get("provider").and_then(|v| v.as_str()).map(String::from);
            let api_key = args.get("api_key").and_then(|v| v.as_str()).map(String::from);
            let auth_method = args.get("auth_method").and_then(|v| v.as_str()).map(String::from);
            let oauth: Option<puppetterm_core::ai::AiOAuthMeta> = args
                .get("oauth")
                .and_then(|v| serde_json::from_value(v.clone()).ok());
            let p = puppetterm_core::ai::add_saved_provider(
                label, base_url, model, provider, api_key, auth_method, oauth,
            )?;
            Ok(json!({ "id": p.id }))
        })
        .await,
        "delete_provider" => run_blocking(move || {
            let id = arg_str(&args, "id");
            if id.is_empty() {
                return Err("id is required".into());
            }
            puppetterm_core::ai::delete_saved_provider(&id)?;
            Ok(json!(null))
        })
        .await,
        "toggle_provider" => run_blocking(move || {
            let id = arg_str(&args, "id");
            let enabled = args.get("enabled").and_then(|v| v.as_bool()).unwrap_or(true);
            if id.is_empty() {
                return Err("id is required".into());
            }
            puppetterm_core::ai::toggle_saved_provider(&id, enabled)?;
            Ok(json!(null))
        })
        .await,
        "delete_ai_config" => run_blocking(|| {
            puppetterm_core::ai::delete_config()?;
            Ok(json!(null))
        })
        .await,
        "test_ai_config" => {
            let base_url = arg_str(&args, "base_url");
            let model = arg_str(&args, "model");
            let provider = args.get("provider").and_then(|v| v.as_str()).map(String::from);
            let api_key = args.get("api_key").and_then(|v| v.as_str()).map(String::from);
            match puppetterm_core::ai::test_config(base_url, model, provider, api_key).await {
                Ok(summary) => ok(json!({ "ok": true, "summary": summary })),
                Err(e) => ok(json!({ "ok": false, "error": e })),
            }
        }
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
                Ok(mut cfg) => {
                    if let Err(e) = puppetterm_core::ai::ensure_valid_token(&mut cfg).await {
                        return err(e);
                    }
                    match puppetterm_core::ai::chat_completion(&cfg, messages, tools, Some(4096)).await {
                        Ok(resp) => ok(serde_json::to_value(resp).unwrap_or(Value::Null)),
                        Err(e) => err(e),
                    }
                }
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

/// GET /oauth/callback — the provider redirects the browser here after the user
/// logs in. We exchange the `code` for an access token (PKCE) and persist it,
/// then show a simple "close this tab" page. This route is exempt from basic
/// auth (see `auth::require_basic_auth`) so the provider redirect can reach it.
pub async fn oauth_callback(Query(params): Query<std::collections::HashMap<String, String>>) -> AxumResponse {
    let code = params.get("code").cloned().unwrap_or_default();
    let state = params.get("state").cloned().unwrap_or_default();
    let result = match params.get("error").cloned() {
        Some(e) => Err(format!("provider returned error: {e}")),
        // Only `code` is strictly required; `state` may be absent (OpenRouter's
        // flow doesn't echo it) — `complete_oauth` handles that via its fallback.
        None if code.is_empty() => Err("missing code in OAuth callback".into()),
        None => puppetterm_core::ai::complete_oauth(&state, &code).await,
    };
    let html = match result {
        Ok(_) => "<!doctype html><html><head><meta charset=\"utf-8\"><title>puppetterm</title></head>\
<body style=\"font-family:system-ui,sans-serif;background:#0d1117;color:#e6edf3;text-align:center;padding-top:22vh\">\
<h2>✅ Login complete</h2><p>You can close this tab and return to puppetterm.</p></body></html>",
        Err(e) => {
            let safe = e.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;");
            Box::leak(format!(
                "<!doctype html><html><head><meta charset=\"utf-8\"><title>puppetterm</title></head>\
<body style=\"font-family:system-ui,sans-serif;background:#0d1117;color:#e6edf3;text-align:center;padding-top:22vh\">\
<h2>❌ Login failed</h2><p>{}</p></body></html>",
                safe
            ).into_boxed_str())
        }
    };
    Html(html.to_string()).into_response()
}
