//! Server-side AI task loop.
//!
//! The browser owns tool execution (it holds the live pty and the approval
//! UI); the server owns the model loop, the message history, persistence, and
//! progress events. A task is created with `ai_task_start`, streams
//! `task_*` events over the websocket, and is driven forward by the browser
//! posting `ai_task_results` back after it has run/approved the tool calls.
//!
//! Because the loop and the conversation live on the server, a task survives
//! a tab close: it parks while waiting for tool results and resumes when a
//! client re-attaches (`ai_task_attach`).

use std::collections::HashSet;
use std::sync::{Arc, Mutex};

use puppetterm_core::ai::{
    chat_completion, ensure_valid_token, load_config, AiConfig, ChatMessage, Role, ToolCall,
    ToolDef,
};
use puppetterm_core::chat_history;
use serde_json::{json, Value};
use tokio::sync::mpsc;

use crate::hub::EventHub;

const MAX_STEPS: usize = 120;
const MAX_HISTORY: usize = 60;
const MAX_CONTEXT_CHARS: usize = 200_000;

/// A tool result posted back by the browser.
#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
pub struct ToolResult {
    pub tool_call_id: String,
    pub content: String,
}

/// Client → loop messages.
pub enum ClientMsg {
    Results { task_id: String, results: Vec<ToolResult> },
    Abort { task_id: String },
}

pub struct Task {
    pub id: String,
    pub owner: Mutex<String>,
    /// Target host and autonomy mode are carried for audit/UI context; the
    /// browser currently owns approval policy, so the loop itself does not
    /// branch on them yet.
    #[allow(dead_code)]
    pub host: String,
    pub tab_id: i64,
    pub conversation_id: String,
    pub tools: Vec<ToolDef>,
    #[allow(dead_code)]
    pub autonomy: String,
    pub messages: Mutex<Vec<ChatMessage>>,
    pub status: Mutex<String>,
    pub abort: Mutex<bool>,
    /// Tool calls that were sent to the browser and are still awaiting results.
    pub pending: Mutex<Option<Vec<ToolCall>>>,
    tx: mpsc::UnboundedSender<ClientMsg>,
    pub hub: Arc<EventHub>,
}

impl Task {
    fn emit(&self, event: &str, payload: Value) {
        self.hub.emit(event, payload);
    }

    fn set_status(&self, status: &str) {
        *self.status.lock().unwrap() = status.to_string();
        self.emit(
            "task_status",
            json!({ "task_id": self.id, "owner": self.owner.lock().unwrap().clone(), "status": status }),
        );
    }

    fn persist(&self) {
        let msgs = self.messages.lock().unwrap().clone();
        let rows: Vec<chat_history::ChatMsg> = msgs.iter().map(to_history_msg).collect();
        let id = self.conversation_id.clone();
        // Best-effort: a persistence failure must not kill a running task.
        let _ = chat_history::replace_messages(&id, &rows);
    }
}

pub struct TaskManager {
    tasks: Mutex<Vec<Arc<Task>>>,
}

impl TaskManager {
    pub fn new() -> Self {
        Self { tasks: Mutex::new(Vec::new()) }
    }

    /// Create a task and start its loop. `messages` is the full history
    /// including the new user message.
    pub fn start(
        &self,
        owner: String,
        host: String,
        tab_id: i64,
        conversation_id: String,
        messages: Vec<ChatMessage>,
        tools: Vec<ToolDef>,
        autonomy: String,
        hub: Arc<EventHub>,
    ) -> Result<String, String> {
        let id = puppetterm_core::chat_history::new_id();
        let (tx, rx) = mpsc::unbounded_channel();
        let task = Arc::new(Task {
            id: id.clone(),
            owner: Mutex::new(owner.clone()),
            host: host.clone(),
            tab_id,
            conversation_id,
            tools,
            autonomy,
            messages: Mutex::new(messages),
            status: Mutex::new("starting".into()),
            abort: Mutex::new(false),
            pending: Mutex::new(None),
            tx,
            hub: hub.clone(),
        });
        {
            let mut tasks = self.tasks.lock().unwrap();
            tasks.push(task.clone());
            // Keep the registry bounded: drop finished tasks older than 50.
            if tasks.len() > 50 {
                tasks.retain(|t| {
                    !matches!(t.status.lock().unwrap().as_str(), "done" | "error" | "aborted")
                });
            }
        }
        task.emit(
            "task_status",
            json!({ "task_id": id, "owner": owner, "status": "starting" }),
        );
        let cfg = load_config()?;
        tokio::spawn(run_loop(task, rx, cfg));
        Ok(id)
    }

    fn find(&self, task_id: &str) -> Option<Arc<Task>> {
        self.tasks.lock().unwrap().iter().find(|t| t.id == task_id).cloned()
    }

    pub fn submit_results(&self, task_id: &str, results: Vec<ToolResult>) -> Result<(), String> {
        let task = self.find(task_id).ok_or("task not found")?;
        task.tx
            .send(ClientMsg::Results { task_id: task_id.to_string(), results })
            .map_err(|_| "task loop is gone".to_string())
    }

    pub fn abort(&self, task_id: &str) -> Result<(), String> {
        let task = self.find(task_id).ok_or("task not found")?;
        *task.abort.lock().unwrap() = true;
        let _ = task.tx.send(ClientMsg::Abort { task_id: task_id.to_string() });
        Ok(())
    }

    /// Re-attach a client as the task owner. If the loop is parked waiting for
    /// tool results, re-send them so the new client can finish the step.
    pub fn attach(&self, task_id: &str, owner: String) -> Result<(), String> {
        let task = self.find(task_id).ok_or("task not found")?;
        *task.owner.lock().unwrap() = owner.clone();
        let pending = task.pending.lock().unwrap().clone();
        if let Some(calls) = pending {
            let explain = String::new();
            task.emit(
                "task_tool_calls",
                json!({
                    "task_id": task.id,
                    "owner": owner,
                    "host": task.host,
                    "tab_id": task.tab_id,
                    "explain": explain,
                    "tool_calls": calls,
                }),
            );
        }
        Ok(())
    }

    pub fn status(&self, task_id: &str) -> Result<Value, String> {
        let task = self.find(task_id).ok_or("task not found")?;
        let msgs = task.messages.lock().unwrap().clone();
        Ok(json!({
            "task_id": task.id,
            "owner": task.owner.lock().unwrap().clone(),
            "status": task.status.lock().unwrap().clone(),
            "conversation_id": task.conversation_id,
            "messages": msgs,
        }))
    }
}

fn to_history_msg(m: &ChatMessage) -> chat_history::ChatMsg {
    chat_history::ChatMsg {
        role: match m.role {
            Role::System => "system",
            Role::User => "user",
            Role::Assistant => "assistant",
            Role::Tool => "tool",
        }
        .to_string(),
        content: m.content.clone(),
        tool_call_id: m.tool_call_id.clone(),
        tool_calls: m
            .tool_calls
            .as_ref()
            .map(|tcs| serde_json::to_string(tcs).unwrap_or_default()),
    }
}

// ---- history compaction (ported from the frontend) ------------------------

fn count_chars(h: &[ChatMessage]) -> usize {
    h.iter()
        .map(|m| m.content.as_deref().unwrap_or("").len())
        .sum()
}

fn drop_unbalanced_tool_batches(h: Vec<ChatMessage>) -> Vec<ChatMessage> {
    let mut out: Vec<ChatMessage> = Vec::new();
    let mut pending: HashSet<String> = HashSet::new();
    let mut batch_start: Option<usize> = None;
    for m in h {
        if matches!(m.role, Role::Assistant) {
            if let Some(tcs) = &m.tool_calls {
                let ids: Vec<String> =
                    tcs.iter().map(|tc| tc.id.clone()).filter(|id| !id.is_empty()).collect();
                if !ids.is_empty() {
                    pending = ids.into_iter().collect();
                    batch_start = Some(out.len());
                    out.push(m);
                    continue;
                }
            }
        }
        if matches!(m.role, Role::Tool) {
            if !pending.is_empty() {
                if let Some(id) = &m.tool_call_id {
                    pending.remove(id);
                }
                out.push(m);
                continue;
            }
            continue;
        }
        if !pending.is_empty() {
            if let Some(pos) = batch_start {
                out.truncate(pos);
            }
            pending.clear();
            batch_start = None;
        }
        out.push(m);
    }
    out
}

fn compact_history(h: Vec<ChatMessage>) -> Vec<ChatMessage> {
    if h.len() < 2 {
        return drop_unbalanced_tool_batches(h);
    }
    if h.len() - 2 <= MAX_HISTORY - 2 && count_chars(&h) <= MAX_CONTEXT_CHARS {
        return drop_unbalanced_tool_batches(h);
    }
    let mut kept: Vec<(usize, ChatMessage)> =
        vec![(0, h[0].clone()), (1, h[1].clone())];
    let mut middle: Vec<(usize, ChatMessage)> =
        h.iter().enumerate().skip(2).map(|(i, m)| (i, m.clone())).collect();
    middle.sort_by_key(|(i, m)| {
        let keep = matches!(m.role, Role::User | Role::Assistant);
        (!keep, *i)
    });
    let mut budget = MAX_CONTEXT_CHARS.saturating_sub(count_chars(&kept.iter().map(|(_, m)| m.clone()).collect::<Vec<_>>()));
    for (i, m) in middle {
        let is_tool = matches!(m.role, Role::Tool);
        let size = if is_tool {
            std::cmp::min(m.content.as_deref().unwrap_or("").len() * 2, budget)
        } else {
            m.content.as_deref().unwrap_or("").len()
        };
        if kept.len() < MAX_HISTORY && (size <= budget || !is_tool) {
            kept.push((i, m));
            budget = budget.saturating_sub(size);
        }
    }
    kept.sort_by_key(|(i, _)| *i);
    let dropped = h.len() - kept.len();
    let note = ChatMessage {
        role: Role::System,
        content: Some(format!(
            "(Note: {dropped} earlier tool outputs were trimmed to keep this conversation bounded — \
             the conversation text is preserved, but the latest terminal/tool state may have \
             changed. Re-query the live server state if needed.)"
        )),
        tool_call_id: None,
        tool_calls: None,
    };
    let mut final_msgs: Vec<ChatMessage> = kept.into_iter().map(|(_, m)| m).collect();
    final_msgs.push(note);
    drop_unbalanced_tool_batches(final_msgs)
}

// ---- the loop ---------------------------------------------------------------

async fn run_loop(task: Arc<Task>, mut rx: mpsc::UnboundedReceiver<ClientMsg>, mut cfg: AiConfig) {
    task.set_status("running");
    let mut guard = 0usize;
    let mut last_sig: Option<String> = None;
    let mut repeat_count = 0usize;

    while guard < MAX_STEPS {
        guard += 1;
        if *task.abort.lock().unwrap() {
            task.set_status("aborted");
            task.emit(
                "task_status",
                json!({ "task_id": task.id, "owner": task.owner.lock().unwrap().clone(), "status": "aborted" }),
            );
            return;
        }
        let messages = {
            let raw = task.messages.lock().unwrap().clone();
            compact_history(raw)
        };
        *task.messages.lock().unwrap() = messages.clone();

        if let Err(e) = ensure_valid_token(&mut cfg).await {
            fail(&task, &e);
            return;
        }
        let resp = match chat_completion(&cfg, messages, Some(task.tools.clone()), Some(4096)).await {
            Ok(r) => r,
            Err(e) => {
                fail(&task, &e);
                return;
            }
        };
        let msg = match resp.choices.into_iter().next().and_then(|c| c.message) {
            Some(m) => m,
            None => {
                fail(&task, "no response from the model");
                return;
            }
        };

        if let Some(tool_calls) = msg.tool_calls.clone() {
            if tool_calls.is_empty() {
                // fall through to text handling
            } else {
                let sig = serde_json::to_string(&tool_calls).unwrap_or_default();
                if last_sig.as_deref() == Some(sig.as_str()) {
                    repeat_count += 1;
                } else {
                    last_sig = Some(sig);
                    repeat_count = 1;
                }
                if repeat_count >= 3 {
                    let text = "(stopped — the AI repeated the same action 3× in a row and appears stuck. \
                               Try rephrasing, or ask it to be more specific.)";
                    push_assistant(&task, text);
                    task.set_status("done");
                    return;
                }

                // Append the assistant tool-call message and persist.
                {
                    let mut msgs = task.messages.lock().unwrap();
                    msgs.push(ChatMessage {
                        role: Role::Assistant,
                        content: msg.content.clone(),
                        tool_call_id: None,
                        tool_calls: Some(tool_calls.clone()),
                    });
                }
                task.persist();

                let explain = msg.content.clone().unwrap_or_default();
                *task.pending.lock().unwrap() = Some(tool_calls.clone());
                task.set_status("waiting_tools");
                task.emit(
                    "task_tool_calls",
                    json!({
                        "task_id": task.id,
                        "owner": task.owner.lock().unwrap().clone(),
                        "host": task.host,
                        "tab_id": task.tab_id,
                        "explain": explain,
                        "tool_calls": tool_calls,
                    }),
                );

                // Wait for the browser to execute/approve and post results.
                let results = loop {
                    match rx.recv().await {
                        Some(ClientMsg::Results { task_id, results }) if task_id == task.id => {
                            break results;
                        }
                        Some(ClientMsg::Abort { task_id }) if task_id == task.id => {
                            task.set_status("aborted");
                            return;
                        }
                        Some(_) => continue,
                        None => {
                            task.set_status("error");
                            return;
                        }
                    }
                };
                *task.pending.lock().unwrap() = None;

                {
                    let mut msgs = task.messages.lock().unwrap();
                    for r in &results {
                        msgs.push(ChatMessage {
                            role: Role::Tool,
                            content: Some(r.content.clone()),
                            tool_call_id: Some(r.tool_call_id.clone()),
                            tool_calls: None,
                        });
                    }
                }
                task.persist();
                task.set_status("running");
                continue;
            }
        }

        let text = msg.content.clone().unwrap_or_else(|| "(done)".into());
        push_assistant(&task, &text);
        task.set_status("done");
        task.emit(
            "task_done",
            json!({
                "task_id": task.id,
                "owner": task.owner.lock().unwrap().clone(),
                "host": task.host,
                "tab_id": task.tab_id,
                "conversation_id": task.conversation_id,
            }),
        );
        return;
    }

    let text = format!(
        "(stopped after {MAX_STEPS} tool steps in a single run — the AI was still issuing tool \
         calls without reaching a final answer. Send a follow-up message to CONTINUE from here.)"
    );
    push_assistant(&task, &text);
    task.set_status("done");
}

fn push_assistant(task: &Task, text: &str) {
    {
        let mut msgs = task.messages.lock().unwrap();
        msgs.push(ChatMessage {
            role: Role::Assistant,
            content: Some(text.to_string()),
            tool_call_id: None,
            tool_calls: None,
        });
    }
    task.persist();
    task.emit(
        "task_text",
        json!({
            "task_id": task.id,
            "owner": task.owner.lock().unwrap().clone(),
            "text": text,
        }),
    );
}

fn fail(task: &Task, error: &str) {
    task.set_status("error");
    task.emit(
        "task_error",
        json!({
            "task_id": task.id,
            "owner": task.owner.lock().unwrap().clone(),
            "error": error,
        }),
    );
}
