//! Chat history — server-side persistence for AI conversations.
//!
//! Each SSH host (and the local session) can have multiple named conversations
//! backed by SQLite. The schema lives in `chat.db` next to `audit.db`.

use std::path::PathBuf;
use std::time::Duration;

use base64::Engine as _;
use rusqlite::{params, Connection, OptionalExtension};
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
    // WAL keeps concurrent readers (WS clients, list queries) from blocking the
    // writer; foreign_keys turns ON DELETE CASCADE into a real guarantee; a busy
    // timeout avoids spurious "database is locked" errors under parallel tabs.
    let _ = conn.pragma_update(None, "journal_mode", "WAL");
    conn.pragma_update(None, "foreign_keys", "ON")
        .map_err(|e| e.to_string())?;
    conn.busy_timeout(Duration::from_secs(5))
        .map_err(|e| e.to_string())?;
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

/// Fixed-width RFC3339 with exactly 9 fractional digits. `time`'s well-known
/// `Rfc3339` format trims trailing zeros (`.83283648Z` vs `.832836480Z`), which
/// makes lexicographic ordering of `updated_at` wrong — a newer row can sort
/// before an older one. A constant-width string sorts chronologically.
const TS_FORMAT: &[time::format_description::FormatItem<'_>] =
    time::macros::format_description!(
        "[year]-[month]-[day]T[hour]:[minute]:[second].[subsecond digits:9]Z"
    );

fn now_ts() -> String {
    time::OffsetDateTime::now_utc()
        .format(TS_FORMAT)
        .unwrap_or_else(|_| "unknown".into())
}

/// Parse a stored timestamp for chronological sorting. Legacy rows written with
/// the variable-width format still parse; unparseable values sort last.
fn parse_ts(s: &str) -> Option<time::OffsetDateTime> {
    time::OffsetDateTime::parse(s, &time::format_description::well_known::Rfc3339).ok()
}

fn gen_id() -> String {
    let bytes: Vec<u8> = (0..16).map(|_| rand_byte()).collect();
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(&bytes)
}

/// Public id generator for other subsystems (e.g. server-side AI tasks).
pub fn new_id() -> String {
    gen_id()
}

fn rand_byte() -> u8 {
    let mut b = [0u8; 1];
    getrandom::getrandom(&mut b).expect("rng");
    b[0]
}

fn conversation_exists(conn: &Connection, id: &str) -> Result<bool, String> {
    conn.query_row(
        "SELECT 1 FROM chat_conversations WHERE id = ?1",
        params![id],
        |_| Ok(true),
    )
    .optional()
    .map(|o| o.unwrap_or(false))
    .map_err(|e| e.to_string())
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
             WHERE c.host = ?1",
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
    // Sort by parsed timestamp (descending), not by the raw string: legacy rows
    // may have variable-width fractional seconds, which sorts incorrectly.
    out.sort_by(|a, b| {
        parse_ts(&b.updated_at)
            .cmp(&parse_ts(&a.updated_at))
            .then_with(|| b.updated_at.cmp(&a.updated_at))
    });
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
        .collect::<Result<Vec<_>, _>>()
        .map_err(|e| e.to_string())?;

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
    let mut conn = conn()?;
    let tx = conn.transaction().map_err(|e| e.to_string())?;
    tx.execute("DELETE FROM chat_messages WHERE conversation_id = ?1", params![id])
        .map_err(|e| e.to_string())?;
    let n = tx
        .execute("DELETE FROM chat_conversations WHERE id = ?1", params![id])
        .map_err(|e| e.to_string())?;
    tx.commit().map_err(|e| e.to_string())?;
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
    let mut conn = conn()?;
    let tx = conn.transaction().map_err(|e| e.to_string())?;
    if !conversation_exists(&tx, conversation_id)? {
        return Err(format!("conversation not found: {conversation_id}"));
    }
    let ts = now_ts();
    // Get next seq inside the same transaction so concurrent appends can't
    // pick the same value.
    let seq: i64 = tx
        .query_row(
            "SELECT COALESCE(MAX(seq), -1) + 1 FROM chat_messages WHERE conversation_id = ?1",
            params![conversation_id],
            |row| row.get(0),
        )
        .map_err(|e| e.to_string())?;
    tx.execute(
        "INSERT INTO chat_messages (conversation_id, role, content, tool_call_id, tool_calls, seq, created_at)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
        params![conversation_id, role, content, tool_call_id, tool_calls, seq, ts],
    )
    .map_err(|e| e.to_string())?;
    // Touch updated_at
    tx.execute(
        "UPDATE chat_conversations SET updated_at = ?2 WHERE id = ?1",
        params![conversation_id, ts],
    )
    .map_err(|e| e.to_string())?;
    tx.commit().map_err(|e| e.to_string())?;
    Ok(())
}

/// Replace all messages in a conversation (the client's only write path).
///
/// The delete+insert runs in one transaction so a failure can never leave the
/// conversation half-written, and a missing conversation is reported instead of
/// silently creating orphaned rows. Also derives the title from the first user
/// message while it is still the default "New chat" — the frontend persists via
/// this function, so `chat_append`'s auto-title alone never fired.
pub fn replace_messages(conversation_id: &str, messages: &[ChatMsg]) -> Result<(), String> {
    let mut conn = conn()?;
    if !conversation_exists(&conn, conversation_id)? {
        return Err(format!("conversation not found: {conversation_id}"));
    }
    let tx = conn.transaction().map_err(|e| e.to_string())?;
    tx.execute(
        "DELETE FROM chat_messages WHERE conversation_id = ?1",
        params![conversation_id],
    )
    .map_err(|e| e.to_string())?;
    let ts = now_ts();
    {
        let mut stmt = tx
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
    }
    tx.execute(
        "UPDATE chat_conversations SET updated_at = ?2 WHERE id = ?1",
        params![conversation_id, ts],
    )
    .map_err(|e| e.to_string())?;
    tx.commit().map_err(|e| e.to_string())?;
    let _ = auto_title_from_first_message(conversation_id);
    Ok(())
}

fn title_from_message(raw: &str) -> String {
    // Collapse whitespace so a multi-line first message makes a one-line title.
    let collapsed = raw.split_whitespace().collect::<Vec<_>>().join(" ");
    if collapsed.chars().count() > 60 {
        format!("{}…", collapsed.chars().take(60).collect::<String>())
    } else {
        collapsed
    }
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
    let truncated = title_from_message(&title);
    if truncated.is_empty() {
        return Ok(());
    }
    conn.execute(
        "UPDATE chat_conversations SET title = ?2, updated_at = ?3 WHERE id = ?1 AND title = 'New chat'",
        params![id, truncated, now_ts()],
    )
    .map_err(|e| e.to_string())?;
    Ok(())
}

/// One-time migration: derive titles for conversations created before
/// `replace_messages` started auto-titleing. Best-effort — called at startup.
pub fn backfill_titles() -> Result<usize, String> {
    let conn = conn()?;
    let ids: Vec<String> = conn
        .prepare("SELECT id FROM chat_conversations WHERE title = 'New chat'")
        .map_err(|e| e.to_string())?
        .query_map([], |row| row.get(0))
        .map_err(|e| e.to_string())?
        .collect::<Result<Vec<_>, _>>()
        .map_err(|e| e.to_string())?;
    let mut n = 0;
    for id in ids {
        let title: Option<String> = conn
            .query_row(
                "SELECT content FROM chat_messages
                 WHERE conversation_id = ?1 AND role = 'user' AND content IS NOT NULL
                 ORDER BY seq LIMIT 1",
                params![id],
                |row| row.get(0),
            )
            .optional()
            .map_err(|e| e.to_string())?;
        let Some(title) = title.map(|t| title_from_message(&t)).filter(|t| !t.is_empty()) else {
            continue;
        };
        n += conn
            .execute(
                "UPDATE chat_conversations SET title = ?2, updated_at = ?3
                 WHERE id = ?1 AND title = 'New chat'",
                params![id, title, now_ts()],
            )
            .map_err(|e| e.to_string())?;
    }
    Ok(n)
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

    #[test]
    fn replace_auto_titles_and_rejects_missing() {
        let _guard = TEST_MUTEX.lock().unwrap();
        let dir = std::env::temp_dir().join(format!("pp-chat-c-{}", std::process::id()));
        let db = dir.join("chat.db");
        std::env::set_var("PUPPETTERM_CHAT_DB", &db);

        // The client persists via replace_messages, so the title must be derived
        // there — not only in the (unused) append path.
        let id = create_conversation("host-replace", "").unwrap();
        let msgs = vec![
            ChatMsg { role: "system".into(), content: Some("sys".into()), tool_call_id: None, tool_calls: None },
            ChatMsg { role: "user".into(), content: Some("  rotate the TLS certs  ".into()), tool_call_id: None, tool_calls: None },
            ChatMsg { role: "assistant".into(), content: Some("done".into()), tool_call_id: None, tool_calls: None },
        ];
        replace_messages(&id, &msgs).unwrap();
        let full = get_conversation(&id).unwrap();
        assert_eq!(full.meta.title, "rotate the TLS certs");

        // A second replace must not clobber an existing title.
        let msgs2 = vec![
            ChatMsg { role: "system".into(), content: Some("sys".into()), tool_call_id: None, tool_calls: None },
            ChatMsg { role: "user".into(), content: Some("a much later question".into()), tool_call_id: None, tool_calls: None },
        ];
        replace_messages(&id, &msgs2).unwrap();
        assert_eq!(get_conversation(&id).unwrap().meta.title, "rotate the TLS certs");

        // Missing conversation → clear error, no orphaned rows.
        let missing = replace_messages(
            "does-not-exist",
            &[ChatMsg { role: "user".into(), content: Some("x".into()), tool_call_id: None, tool_calls: None }],
        );
        assert!(missing.is_err());
        assert!(append_message("does-not-exist", "user", Some("x"), None, None).is_err());

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn list_orders_newest_first() {
        let _guard = TEST_MUTEX.lock().unwrap();
        let dir = std::env::temp_dir().join(format!("pp-chat-d-{}", std::process::id()));
        let db = dir.join("chat.db");
        std::env::set_var("PUPPETTERM_CHAT_DB", &db);

        let a = create_conversation("host-order", "").unwrap();
        let b = create_conversation("host-order", "").unwrap();
        let c = create_conversation("host-order", "").unwrap();
        // Make `a` the most recently updated despite being created first.
        for (id, text) in [(&c, "newest created"), (&b, "middle"), (&a, "oldest created, newest updated")] {
            replace_messages(
                id,
                &[
                    ChatMsg { role: "system".into(), content: Some("sys".into()), tool_call_id: None, tool_calls: None },
                    ChatMsg { role: "user".into(), content: Some(text.into()), tool_call_id: None, tool_calls: None },
                ],
            )
            .unwrap();
        }
        let list = list_conversations("host-order").unwrap();
        assert_eq!(list.len(), 3);
        assert_eq!(list[0].id, a);
        assert_eq!(list[1].id, b);
        assert_eq!(list[2].id, c);

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn timestamps_are_fixed_width_and_sortable() {
        let t1 = now_ts();
        let t2 = now_ts();
        // 9 fractional digits + trailing Z → lexicographic order == chronological.
        assert!(t1.len() == 30, "unexpected timestamp width: {t1}");
        assert!(t1.ends_with('Z'));
        assert!(t2 >= t1, "timestamps must sort lexicographically: {t1} vs {t2}");
        assert!(parse_ts(&t1).is_some());
    }

    #[test]
    fn backfill_titles_only_untitled_with_user_messages() {
        let _guard = TEST_MUTEX.lock().unwrap();
        let dir = std::env::temp_dir().join(format!("pp-chat-e-{}", std::process::id()));
        let db = dir.join("chat.db");
        std::env::set_var("PUPPETTERM_CHAT_DB", &db);

        let titled = create_conversation("host-backfill", "").unwrap();
        append_message(&titled, "user", Some("first question here"), None, None).unwrap();
        let empty = create_conversation("host-backfill", "").unwrap();
        append_message(&empty, "system", Some("sys"), None, None).unwrap();
        let named = create_conversation("host-backfill", "Already named").unwrap();
        append_message(&named, "user", Some("other"), None, None).unwrap();

        assert_eq!(backfill_titles().unwrap(), 1);
        assert_eq!(get_conversation(&titled).unwrap().meta.title, "first question here");
        assert_eq!(get_conversation(&empty).unwrap().meta.title, "New chat");
        assert_eq!(get_conversation(&named).unwrap().meta.title, "Already named");
        // Idempotent: nothing left to title.
        assert_eq!(backfill_titles().unwrap(), 0);

        std::fs::remove_dir_all(&dir).ok();
    }
}
