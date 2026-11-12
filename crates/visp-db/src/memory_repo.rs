//! SQLite-backed durable task memory.
//!
//! Uses the same database file as sessions/messages (separate connection,
//! WAL mode) so memory shares transactionality, backup and cascade deletion
//! with the rest of the session state.

use std::sync::Mutex;

use rusqlite::{Connection, Result, params};
use visp_core::error::SessionError;
use visp_core::memory::{MemoryEntry, MemoryKind, MemoryStore, NewMemoryEntry};

/// DAO for the `context_memory` table.
pub struct MemoryRepo;

impl MemoryRepo {
    /// Insert a memory entry. Identical `(session_id, kind, content)` rows are
    /// idempotent and return the existing row.
    pub fn insert(conn: &Connection, entry: &NewMemoryEntry) -> Result<MemoryEntry> {
        let now = chrono::Utc::now().timestamp_millis();
        conn.execute(
            "INSERT INTO context_memory (session_id, kind, content, source, created_at)
             VALUES (?1, ?2, ?3, ?4, ?5)
             ON CONFLICT(session_id, kind, content) DO NOTHING",
            params![
                entry.session_id,
                entry.kind.as_str(),
                entry.content,
                entry.source,
                now,
            ],
        )?;

        let mut stmt = conn.prepare(
            "SELECT id, session_id, kind, content, source, created_at
             FROM context_memory
             WHERE session_id = ?1 AND kind = ?2 AND content = ?3
             ORDER BY id ASC LIMIT 1",
        )?;
        stmt.query_row(
            params![entry.session_id, entry.kind.as_str(), entry.content],
            row_to_entry,
        )
    }

    /// Search memories for a session. Substring match, newest first.
    pub fn search(
        conn: &Connection,
        session_id: &str,
        query: Option<&str>,
        limit: usize,
    ) -> Result<Vec<MemoryEntry>> {
        let limit = limit.max(1) as i64;
        let Some(query) = query.filter(|q| !q.trim().is_empty()) else {
            return Self::list(conn, session_id, limit as usize);
        };

        let escaped = query
            .replace('\\', "\\\\")
            .replace('%', "\\%")
            .replace('_', "\\_");

        let mut stmt = conn.prepare(
            "SELECT id, session_id, kind, content, source, created_at
             FROM context_memory
             WHERE session_id = ?1 AND content LIKE '%' || ?2 || '%' ESCAPE '\\'
             ORDER BY id DESC LIMIT ?3",
        )?;
        stmt.query_map(params![session_id, escaped, limit], row_to_entry)?
            .collect()
    }

    /// List the most recent `limit` entries in chronological order.
    pub fn list(conn: &Connection, session_id: &str, limit: usize) -> Result<Vec<MemoryEntry>> {
        let limit = limit.max(1) as i64;
        let mut stmt = conn.prepare(
            "SELECT id, session_id, kind, content, source, created_at FROM (
                 SELECT id, session_id, kind, content, source, created_at
                 FROM context_memory
                 WHERE session_id = ?1
                 ORDER BY id DESC
                 LIMIT ?2
             ) ORDER BY id ASC",
        )?;
        stmt.query_map(params![session_id, limit], row_to_entry)?
            .collect()
    }

    /// Count entries for a session.
    pub fn count(conn: &Connection, session_id: &str) -> Result<usize> {
        let count: i64 = conn.query_row(
            "SELECT COUNT(*) FROM context_memory WHERE session_id = ?1",
            params![session_id],
            |row| row.get(0),
        )?;
        Ok(count as usize)
    }
}

fn row_to_entry(row: &rusqlite::Row<'_>) -> Result<MemoryEntry> {
    let kind_str: String = row.get(2)?;
    Ok(MemoryEntry {
        id: row.get(0)?,
        session_id: row.get(1)?,
        kind: MemoryKind::parse(&kind_str).unwrap_or(MemoryKind::Fact),
        content: row.get(3)?,
        source: row.get(4)?,
        created_at: row.get(5)?,
    })
}

/// `MemoryStore` implementation backed by a dedicated SQLite connection.
pub struct SqliteMemoryStore {
    conn: Mutex<Connection>,
}

impl SqliteMemoryStore {
    /// Open (or create) the memory store at `path`, running migrations.
    pub fn open(path: &str) -> Result<Self, SessionError> {
        let expanded = visp_config::path::expand_home(path);
        if let Some(parent) = expanded.parent() {
            std::fs::create_dir_all(parent)
                .map_err(|e| SessionError::Other(format!("Failed to create db dir: {e}")))?;
        }
        let conn = Connection::open(&expanded)
            .map_err(|e| SessionError::Other(format!("Failed to open memory db: {e}")))?;
        crate::schema::Migrator::run(&conn)
            .map_err(|e| SessionError::Other(format!("Memory db migration failed: {e}")))?;
        Ok(Self {
            conn: Mutex::new(conn),
        })
    }

    /// Open an in-memory store (tests, ephemeral sessions).
    pub fn open_in_memory() -> Result<Self, SessionError> {
        let conn = Connection::open_in_memory()
            .map_err(|e| SessionError::Other(format!("Failed to open memory db: {e}")))?;
        crate::schema::Migrator::run(&conn)
            .map_err(|e| SessionError::Other(format!("Memory db migration failed: {e}")))?;
        Ok(Self {
            conn: Mutex::new(conn),
        })
    }

    /// Test-only access to the underlying connection.
    #[doc(hidden)]
    pub fn conn_for_tests(&self) -> std::sync::MutexGuard<'_, Connection> {
        self.conn.lock().expect("memory db lock")
    }

    fn with_conn<T>(&self, f: impl FnOnce(&Connection) -> Result<T>) -> Result<T, String> {
        let conn = self
            .conn
            .lock()
            .map_err(|e| format!("memory db lock poisoned: {e}"))?;
        f(&conn).map_err(|e| format!("memory db error: {e}"))
    }
}

impl MemoryStore for SqliteMemoryStore {
    fn append(&self, entry: NewMemoryEntry) -> Result<MemoryEntry, String> {
        self.with_conn(|conn| MemoryRepo::insert(conn, &entry))
    }

    fn search(
        &self,
        session_id: &str,
        query: Option<&str>,
        limit: usize,
    ) -> Result<Vec<MemoryEntry>, String> {
        self.with_conn(|conn| MemoryRepo::search(conn, session_id, query, limit))
    }

    fn list(&self, session_id: &str, limit: usize) -> Result<Vec<MemoryEntry>, String> {
        self.with_conn(|conn| MemoryRepo::list(conn, session_id, limit))
    }

    fn count(&self, session_id: &str) -> Result<usize, String> {
        self.with_conn(|conn| MemoryRepo::count(conn, session_id))
    }
}

#[cfg(test)]
#[path = "memory_repo_tests.rs"]
mod tests;
