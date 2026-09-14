//! Chat history — server-side persistence for AI conversations.
//!
//! Each SSH host (and the local session) can have multiple named conversations
//! backed by SQLite. The schema lives in `chat.db` next to `audit.db`.

use std::path::PathBuf;

use base64::Engine as _;
use rusqlite::{params, Connection};
use serde::{Deserialize, Serialize};

// ---- types -----------------------------------------------------------------

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct ConversationMeta {
    pub id: String,
    pub host: String,
    pub title: String,
    pub created_at: String,
    pub updated_at: String,
    pub message_count: i64,
}

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct ChatMsg {
    pub role: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub content: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tool_call_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tool_calls: Option<String>, // JSON string of tool call array
}

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct FullConversation {
    pub meta: ConversationMeta,
    pub messages: Vec<ChatMsg>,
}

// ---- db path ---------------------------------------------------------------

pub fn chat_db_path() -> PathBuf {
    if let Ok(p) = std::env::var("PUPPETTERM_CHAT_DB") {
        return PathBuf::from(p);
    }
    let home = std::env::var("HOME").unwrap_or_else(|_| ".".into());
    PathBuf::from(home)
        .join(".config")
        .join("puppetterm")
        .join("chat.db")
}

fn conn() -> Result<Connection, String> {
    let path = chat_db_path();
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir).map_err(|e| e.to_string())?;
    }
    let conn = Connection::open(&path).map_err(|e| e.to_string())?;
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS chat_conversations (
            id TEXT PRIMARY KEY,
            host TEXT NOT NULL,
            title TEXT NOT NULL DEFAULT '',
            created_at TEXT NOT NULL,
            updated_at TEXT NOT NULL
        );
        CREATE TABLE IF NOT EXISTS chat_messages (
            id INTEGER PRIMARY KEY AUTOINCREMENT,
            conversation_id TEXT NOT NULL REFERENCES chat_conversations(id) ON DELETE CASCADE,
            role TEXT NOT NULL,
            content TEXT,
            tool_call_id TEXT,
            tool_calls TEXT,
            seq INTEGER NOT NULL,
            created_at TEXT NOT NULL
        );
        CREATE INDEX IF NOT EXISTS idx_chat_msg_conv
            ON chat_messages(conversation_id, seq);
        CREATE INDEX IF NOT EXISTS idx_chat_conv_host
            ON chat_conversations(host);",
    )
    .map_err(|e| e.to_string())?;
    Ok(conn)
}

// ---- helpers ---------------------------------------------------------------

fn now_ts() -> String {
    time::OffsetDateTime::now_utc()
        .format(&time::format_description::well_known::Rfc3339)
        .unwrap_or_else(|_| "unknown".into())
}

fn gen_id() -> String {
    let bytes: Vec<u8> = (0..16).map(|_| rand_byte()).collect();
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(&bytes)
}

fn rand_byte() -> u8 {
    let mut b = [0u8; 1];
    getrandom::getrandom(&mut b).expect("rng");
    b[0]
}

// ---- public API ------------------------------------------------------------

/// Create a new conversation for a host. Returns the conversation id.
pub fn create_conversation(host: &str, title: &str) -> Result<String, String> {
    let conn = conn()?;
    let id = gen_id();
    let ts = now_ts();
    let title = if title.trim().is_empty() {
        "New chat".to_string()
    } else {
        title.trim().to_string()
    };
    conn.execute(
        "INSERT INTO chat_conversations (id, host, title, created_at, updated_at)
         VALUES (?1, ?2, ?3, ?4, ?5)",
        params![id, host, title, ts, ts],
    )
    .map_err(|e| e.to_string())?;
    Ok(id)
}

/// List all conversations for a host (newest first).
pub fn list_conversations(host: &str) -> Result<Vec<ConversationMeta>, String> {
    let conn = conn()?;
    let mut stmt = conn
        .prepare(
            "SELECT c.id, c.host, c.title, c.created_at, c.updated_at,
                    (SELECT COUNT(*) FROM chat_messages m WHERE m.conversation_id = c.id) AS msg_count
             FROM chat_conversations c
             WHERE c.host = ?1
             ORDER BY c.updated_at DESC",
        )
        .map_err(|e| e.to_string())?;
    let rows = stmt
        .query_map(params![host], |row| {
            Ok(ConversationMeta {
                id: row.get(0)?,
                host: row.get(1)?,
                title: row.get(2)?,
                created_at: row.get(3)?,
                updated_at: row.get(4)?,
                message_count: row.get(5)?,
            })
        })
        .map_err(|e| e.to_string())?;
    let mut out = Vec::new();
    for r in rows {
        out.push(r.map_err(|e| e.to_string())?);
    }
    Ok(out)
}

/// Get a full conversation (metadata + all messages in order).
pub fn get_conversation(id: &str) -> Result<FullConversation, String> {
    let conn = conn()?;
    let meta: ConversationMeta = conn
        .query_row(
            "SELECT c.id, c.host, c.title, c.created_at, c.updated_at,
                    (SELECT COUNT(*) FROM chat_messages m WHERE m.conversation_id = c.id)
             FROM chat_conversations c WHERE c.id = ?1",
            params![id],
            |row| {
                Ok(ConversationMeta {
                    id: row.get(0)?,
                    host: row.get(1)?,
                    title: row.get(2)?,
                    created_at: row.get(3)?,
                    updated_at: row.get(4)?,
                    message_count: row.get(5)?,
                })
            },
        )
        .map_err(|_| format!("conversation not found: {id}"))?;

    let mut stmt = conn
        .prepare(
            "SELECT role, content, tool_call_id, tool_calls
             FROM chat_messages WHERE conversation_id = ?1 ORDER BY seq",
        )
        .map_err(|e| e.to_string())?;
    let messages = stmt
        .query_map(params![id], |row| {
            Ok(ChatMsg {
                role: row.get(0)?,
                content: row.get(1)?,
                tool_call_id: row.get(2)?,
                tool_calls: row.get(3)?,
            })
        })
        .map_err(|e| e.to_string())?
        .filter_map(|r| r.ok())
        .collect();

    Ok(FullConversation { meta, messages })
}

/// Rename a conversation.
pub fn rename_conversation(id: &str, title: &str) -> Result<(), String> {
    let conn = conn()?;
    let n = conn
        .execute(
            "UPDATE chat_conversations SET title = ?2, updated_at = ?3 WHERE id = ?1",
            params![id, title.trim(), now_ts()],
        )
        .map_err(|e| e.to_string())?;
    if n == 0 {
        return Err("conversation not found".into());
    }
    Ok(())
}

/// Delete a conversation and all its messages.
pub fn delete_conversation(id: &str) -> Result<(), String> {
    let conn = conn()?;
    conn.execute("DELETE FROM chat_messages WHERE conversation_id = ?1", params![id])
        .map_err(|e| e.to_string())?;
    let n = conn
        .execute("DELETE FROM chat_conversations WHERE id = ?1", params![id])
        .map_err(|e| e.to_string())?;
    if n == 0 {
        return Err("conversation not found".into());
    }
    Ok(())
}

/// Append a single message to a conversation.
pub fn append_message(
    conversation_id: &str,
    role: &str,
    content: Option<&str>,
    tool_call_id: Option<&str>,
    tool_calls: Option<&str>,
) -> Result<(), String> {
    let conn = conn()?;
    let ts = now_ts();
    // Get next seq
    let seq: i64 = conn
        .query_row(
            "SELECT COALESCE(MAX(seq), -1) + 1 FROM chat_messages WHERE conversation_id = ?1",
            params![conversation_id],
            |row| row.get(0),
        )
        .map_err(|e| e.to_string())?;
    conn.execute(
        "INSERT INTO chat_messages (conversation_id, role, content, tool_call_id, tool_calls, seq, created_at)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
        params![conversation_id, role, content, tool_call_id, tool_calls, seq, ts],
    )
    .map_err(|e| e.to_string())?;
    // Touch updated_at
    conn.execute(
        "UPDATE chat_conversations SET updated_at = ?2 WHERE id = ?1",
        params![conversation_id, ts],
    )
    .map_err(|e| e.to_string())?;
    Ok(())
}

/// Replace all messages in a conversation (used for history compaction).
pub fn replace_messages(conversation_id: &str, messages: &[ChatMsg]) -> Result<(), String> {
    let conn = conn()?;
    conn.execute(
        "DELETE FROM chat_messages WHERE conversation_id = ?1",
        params![conversation_id],
    )
    .map_err(|e| e.to_string())?;
    let ts = now_ts();
    let mut stmt = conn
        .prepare(
            "INSERT INTO chat_messages (conversation_id, role, content, tool_call_id, tool_calls, seq, created_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
        )
        .map_err(|e| e.to_string())?;
    for (i, m) in messages.iter().enumerate() {
        stmt.execute(params![
            conversation_id,
            m.role,
            m.content,
            m.tool_call_id,
            m.tool_calls,
            i as i64,
            ts,
        ])
        .map_err(|e| e.to_string())?;
    }
    conn.execute(
        "UPDATE chat_conversations SET updated_at = ?2 WHERE id = ?1",
        params![conversation_id, ts],
    )
    .map_err(|e| e.to_string())?;
    Ok(())
}

/// Auto-title a conversation from its first user message (if untitled).
pub fn auto_title_from_first_message(id: &str) -> Result<(), String> {
    let conn = conn()?;
    let title: String = conn
        .query_row(
            "SELECT content FROM chat_messages
             WHERE conversation_id = ?1 AND role = 'user' AND content IS NOT NULL
             ORDER BY seq LIMIT 1",
            params![id],
            |row| row.get(0),
        )
        .map_err(|_| "no messages".to_string())?;
    let truncated = if title.chars().count() > 60 {
        format!("{}…", title.chars().take(60).collect::<String>())
    } else {
        title
    };
    conn.execute(
        "UPDATE chat_conversations SET title = ?2, updated_at = ?3 WHERE id = ?1 AND title = 'New chat'",
        params![id, truncated, now_ts()],
    )
    .map_err(|e| e.to_string())?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    // NOTE: the DB path comes from a process-global env var (PUPPETTERM_CHAT_DB),
    // so tests must run serially — a static mutex serializes them.

    static TEST_MUTEX: std::sync::Mutex<()> = std::sync::Mutex::new(());

    #[test]
    fn conversation_lifecycle() {
        let _guard = TEST_MUTEX.lock().unwrap();
        let dir = std::env::temp_dir().join(format!("pp-chat-a-{}", std::process::id()));
        let db = dir.join("chat.db");
        std::env::set_var("PUPPETTERM_CHAT_DB", &db);

        let id = create_conversation("web-1", "Test chat").unwrap();
        assert!(!id.is_empty());

        let list = list_conversations("web-1").unwrap();
        assert_eq!(list.len(), 1);
        assert_eq!(list[0].title, "Test chat");

        append_message(&id, "user", Some("hello"), None, None).unwrap();
        append_message(&id, "assistant", Some("hi there"), None, None).unwrap();

        let full = get_conversation(&id).unwrap();
        assert_eq!(full.messages.len(), 2);

        rename_conversation(&id, "Renamed").unwrap();
        let full2 = get_conversation(&id).unwrap();
        assert_eq!(full2.meta.title, "Renamed");

        // replace_messages (history compaction path)
        let new_msgs = vec![
            ChatMsg { role: "system".into(), content: Some("sys".into()), tool_call_id: None, tool_calls: None },
            ChatMsg { role: "user".into(), content: Some("c".into()), tool_call_id: None, tool_calls: None },
        ];
        replace_messages(&id, &new_msgs).unwrap();
        let full3 = get_conversation(&id).unwrap();
        assert_eq!(full3.messages.len(), 2);
        assert_eq!(full3.messages[0].role, "system");

        delete_conversation(&id).unwrap();
        assert!(list_conversations("web-1").unwrap().is_empty());

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn auto_title() {
        let _guard = TEST_MUTEX.lock().unwrap();
        let dir = std::env::temp_dir().join(format!("pp-chat-b-{}", std::process::id()));
        let db = dir.join("chat.db");
        std::env::set_var("PUPPETTERM_CHAT_DB", &db);

        let id = create_conversation("host-a", "").unwrap();
        append_message(&id, "user", Some("please restart nginx and show logs"), None, None).unwrap();
        auto_title_from_first_message(&id).unwrap();

        let full = get_conversation(&id).unwrap();
        assert!(full.meta.title.starts_with("please restart"));
        assert!(full.meta.title.len() <= 61);

        std::fs::remove_dir_all(&dir).ok();
    }
}
