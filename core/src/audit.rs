//! Client-side audit log (SQLite, append-only).
//!
//! Every action run through the client is recorded: timestamp, host, source
//! (user or AI), action, params, approval state, exit, and a result summary.
//! The table has triggers that prevent UPDATE/DELETE (append-only).

use std::path::PathBuf;

use rusqlite::{params, Connection};
use serde::Serialize;

#[derive(Serialize)]
pub struct AuditRow {
    pub id: i64,
    pub ts: String,
    pub host: String,
    pub source: String,
    pub action: String,
    pub params: Option<String>,
    pub approval: String,
    pub exit: Option<i64>,
    pub result: Option<String>,
}

pub fn audit_db_path() -> PathBuf {
    if let Ok(p) = std::env::var("PUPPETTERM_AUDIT_DB") {
        return PathBuf::from(p);
    }
    let home = std::env::var("HOME").unwrap_or_else(|_| ".".into());
    PathBuf::from(home).join(".config").join("puppetterm").join("audit.db")
}

fn conn() -> Result<Connection, String> {
    let path = audit_db_path();
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir).map_err(|e| e.to_string())?;
    }
    let conn = Connection::open(&path).map_err(|e| e.to_string())?;
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS audit (
            id INTEGER PRIMARY KEY AUTOINCREMENT,
            ts TEXT NOT NULL,
            host TEXT NOT NULL,
            source TEXT NOT NULL DEFAULT 'user',
            action TEXT NOT NULL,
            params TEXT,
            approval TEXT NOT NULL DEFAULT 'auto',
            exit INTEGER,
            result TEXT
        );
        CREATE TRIGGER IF NOT EXISTS audit_no_update
            BEFORE UPDATE ON audit
            BEGIN SELECT RAISE(ABORT, 'audit is append-only'); END;
        CREATE TRIGGER IF NOT EXISTS audit_no_delete
            BEFORE DELETE ON audit
            BEGIN SELECT RAISE(ABORT, 'audit is append-only'); END;",
    )
    .map_err(|e| e.to_string())?;
    Ok(conn)
}

fn now_ts() -> String {
    time::OffsetDateTime::now_utc()
        .format(&time::format_description::well_known::Rfc3339)
        .unwrap_or_else(|_| "unknown".into())
}

/// Append one audit entry. Returns the new row's id so callers can attach a
/// full-output detail file (see `write_detail`).
pub fn record(
    host: &str,
    source: &str,
    action: &str,
    params: Option<&str>,
    approval: &str,
    exit: Option<i64>,
    result: Option<&str>,
) -> Result<i64, String> {
    let conn = conn()?;
    conn.execute(
        "INSERT INTO audit (ts, host, source, action, params, approval, exit, result)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
        params![
            now_ts(),
            host,
            source,
            action,
            params,
            approval,
            exit,
            result
        ],
    )
    .map_err(|e| e.to_string())?;
    Ok(conn.last_insert_rowid())
}

/// Directory holding per-entry full-output detail files (`<id>.json`). Kept
/// separate from the SQLite row so verbose command output never bloats the
/// index; the DB stays a lightweight header, details are pulled on demand.
pub fn detail_dir() -> PathBuf {
    audit_db_path()
        .parent()
        .map(|p| p.join("audit-details"))
        .unwrap_or_else(|| PathBuf::from("audit-details"))
}

/// Write the full output for an audit entry to a file keyed by its id.
pub fn write_detail(id: i64, content: &str) -> Result<(), String> {
    let dir = detail_dir();
    std::fs::create_dir_all(&dir).map_err(|e| e.to_string())?;
    let path = dir.join(format!("{id}.json"));
    std::fs::write(&path, content).map_err(|e| e.to_string())
}

/// Read the full output for an audit entry, if one was stored.
pub fn read_detail(id: i64) -> Result<Option<String>, String> {
    let path = detail_dir().join(format!("{id}.json"));
    match std::fs::read_to_string(&path) {
        Ok(s) => Ok(Some(s)),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(e.to_string()),
    }
}

/// Return the most recent audit entries (newest first).
pub fn recent(limit: i64) -> Result<Vec<AuditRow>, String> {
    let conn = conn()?;
    let mut stmt = conn
        .prepare(
            "SELECT id, ts, host, source, action, params, approval, exit, result
             FROM audit ORDER BY id DESC LIMIT ?1",
        )
        .map_err(|e| e.to_string())?;
    let rows = stmt
        .query_map(params![limit], |row| {
            Ok(AuditRow {
                id: row.get(0)?,
                ts: row.get(1)?,
                host: row.get(2)?,
                source: row.get(3)?,
                action: row.get(4)?,
                params: row.get(5)?,
                approval: row.get(6)?,
                exit: row.get(7)?,
                result: row.get(8)?,
            })
        })
        .map_err(|e| e.to_string())?;
    let mut out = Vec::new();
    for r in rows {
        out.push(r.map_err(|e| e.to_string())?);
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn record_and_recent() {
        let dir = std::env::temp_dir().join(format!("pp-audit-test-{}", std::process::id()));
        let db = dir.join("audit.db");
        std::env::set_var("PUPPETTERM_AUDIT_DB", &db);

        record("host-a", "ai", "service", Some("{\"unit\":\"nginx\",\"op\":\"restart\"}"), "approved", Some(0), Some("{\"exit\":0}")).unwrap();
        record("host-a", "user", "run", Some("{\"cmd\":\"ls\"}"), "auto", Some(0), Some("{\"exit\":0}")).unwrap();

        let rows = recent(10).unwrap();
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0].action, "run");
        assert_eq!(rows[0].source, "user");
        assert_eq!(rows[1].action, "service");
        assert_eq!(rows[1].approval, "approved");

        // Append-only: UPDATE must fail.
        let conn = conn().unwrap();
        assert!(conn.execute("UPDATE audit SET host='x' WHERE id=1", []).is_err());

        std::fs::remove_dir_all(&dir).ok();
    }
}
