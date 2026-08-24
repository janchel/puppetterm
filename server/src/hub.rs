//! Event hub — broadcasts backend events to every connected WebSocket client.
//!
//! Mirrors Tauri's `app.emit` semantics (process-wide broadcast): the desktop
//! app has a single webview, the web server may have several browser tabs —
//! in both cases events go to all of them and each tab filters by session id.

use tokio::sync::broadcast;

/// Capacity of the per-client event queue before slow clients start dropping
/// frames (terminal output is lossy under backpressure anyway).
const CAPACITY: usize = 1024;

#[derive(Clone)]
pub struct EventHub {
    tx: broadcast::Sender<String>,
}

impl EventHub {
    pub fn new() -> Self {
        let (tx, _) = broadcast::channel(CAPACITY);
        Self { tx }
    }

    /// Send one event to all connected clients. `payload` must be JSON.
    /// Safe to call from sync contexts (pty reader threads).
    pub fn emit(&self, event: &str, payload: serde_json::Value) {
        let frame = match serde_json::to_string(&serde_json::json!({
            "event": event,
            "payload": payload,
        })) {
            Ok(s) => s,
            Err(_) => return,
        };
        // No receivers → nobody is listening; that's fine.
        let _ = self.tx.send(frame);
    }

    /// Subscribe to the broadcast stream (one receiver per WS client).
    pub fn subscribe(&self) -> broadcast::Receiver<String> {
        self.tx.subscribe()
    }
}
