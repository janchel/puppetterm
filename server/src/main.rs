//! puppetterm headless server.
//!
//! Serves the web frontend (static files) plus an HTTP API that mirrors the
//! desktop app's Tauri commands, and a WebSocket (`/ws`) that streams the
//! same events (`pty-output`, `pty-exit`, `install-output`, `agent-event`).
//!
//! Configuration is environment-only:
//!   PUPPETTERM_BIND        bind address            (default 0.0.0.0)
//!   PUPPETTERM_PORT        port                    (default 8080)
//!   PUPPETTERM_WEB_DIST    static frontend dir     (optional — API-only if unset)
//!   PUPPETTERM_BASIC_AUTH  `user:pass` basic auth  (optional — open if unset)

mod api;
mod auth;
mod hub;

use std::sync::Arc;

use axum::http::StatusCode;
use axum::response::IntoResponse;use axum::routing::{get, post};
use axum::Router;
use puppetterm_core::sessions::SessionManager;

use hub::EventHub;

#[derive(Clone)]
pub struct App {
    pub sessions: Arc<SessionManager>,
    pub hub: Arc<EventHub>,
}

async fn health() -> impl IntoResponse {
    axum::Json(serde_json::json!({ "ok": true }))
}

async fn not_found() -> impl IntoResponse {
    (StatusCode::NOT_FOUND, "not found")
}

#[tokio::main]
async fn main() {
    let bind = std::env::var("PUPPETTERM_BIND").unwrap_or_else(|_| "0.0.0.0".into());
    let port: u16 = std::env::var("PUPPETTERM_PORT")
        .ok()
        .and_then(|p| p.parse().ok())
        .unwrap_or(8080);
    let dist = std::env::var("PUPPETTERM_WEB_DIST").ok().filter(|d| !d.is_empty());

    // Clean up any leftover ControlMaster sockets from previous runs.
    std::thread::spawn(puppetterm_core::sessions::cleanup_stale_masters);

    let app_state = App { sessions: Arc::new(SessionManager::new()), hub: Arc::new(EventHub::new()) };

    let mut router = Router::new()
        .route("/api/health", get(health))
        .route("/api/{cmd}", post(api::command))
        .route("/ws", get(api::ws_upgrade))
        .route("/oauth/callback", get(api::oauth_callback))
        .with_state(app_state.clone());

    if let Some(dist) = dist {
        match std::fs::metadata(&dist) {
            Ok(m) if m.is_dir() => {
                let index = std::path::Path::new(&dist).join("index.html");
                let serve = tower_http::services::ServeDir::new(&dist)
                    .not_found_service(tower_http::services::ServeFile::new(index));
                router = router.fallback_service(serve);
                println!("[puppetterm] serving web UI from {dist}");
            }
            _ => {
                eprintln!(
                    "[puppetterm] warning: PUPPETTERM_WEB_DIST={dist} is not a directory — starting API-only"
                );
                router = router.fallback(not_found);
            }
        }
    } else {
        println!("[puppetterm] no PUPPETTERM_WEB_DIST set — API only");
        router = router.fallback(not_found);
    }

    // Optional HTTP basic auth over everything (API, WS, and static files).
    if let Some(creds) = auth::configured() {
        println!("[puppetterm] basic auth ENABLED for user {:?}", creds.user);
        router = router.layer(axum::middleware::from_fn_with_state(
            creds,
            auth::require_basic_auth,
        ));
    } else {
        eprintln!(
            "[puppetterm] WARNING: running WITHOUT authentication — anyone who can reach this \
             port can run commands on your servers. Set PUPPETTERM_BASIC_AUTH=user:pass."
        );
    }

    let addr = format!("{bind}:{port}");
    let listener = tokio::net::TcpListener::bind(&addr)
        .await
        .unwrap_or_else(|e| panic!("cannot bind {addr}: {e}"));
    println!("[puppetterm] listening on http://{addr}");

    axum::serve(listener, router)
        .with_graceful_shutdown(shutdown_signal())
        .await
        .expect("server error");
}

async fn shutdown_signal() {
    let ctrl_c = async {
        tokio::signal::ctrl_c().await.ok();
    };
    #[cfg(unix)]
    let terminate = async {
        tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
            .expect("install SIGTERM handler")
            .recv()
            .await;
    };
    #[cfg(not(unix))]
    let terminate = std::future::pending::<()>();

    tokio::select! {
        _ = ctrl_c => {},
        _ = terminate => {},
    }
    println!("[puppetterm] shutting down");
}
