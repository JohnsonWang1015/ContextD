//! SQLite-backed [`Storage`](crate::storage::repository::Storage).
//!
//! One connection guarded by a mutex. ContextD is a CLI and a single-user MCP
//! server, so contention is negligible, while a single connection makes
//! transaction boundaries obvious and avoids a pool dependency. WAL mode plus a
//! busy timeout keeps a concurrently running `contextd mcp serve` from
//! colliding with an interactive command.

mod bindings;
mod checkpoints;
mod decisions;
mod embeddings;
mod fts;
mod memories;
pub mod migrations;
mod projects;
mod sessions;
mod tombstones;

use std::path::Path;
use std::sync::{Mutex, MutexGuard};

use rusqlite::Connection;

use crate::error::{Error, Result};
use crate::storage::repository::Storage;

/// How long to wait for another process to release a write lock.
const BUSY_TIMEOUT_MS: u32 = 5_000;

/// The shipped storage backend.
pub struct SqliteStore {
    conn: Mutex<Connection>,
}

impl std::fmt::Debug for SqliteStore {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SqliteStore").finish_non_exhaustive()
    }
}

impl SqliteStore {
    /// Open (creating if needed) the database at `path` and migrate it.
    pub fn open(path: &Path) -> Result<Self> {
        if let Some(parent) = path.parent() {
            if !parent.as_os_str().is_empty() {
                std::fs::create_dir_all(parent).map_err(|e| Error::io(parent, e))?;
            }
        }
        let conn = Connection::open(path)?;
        Self::from_connection(conn)
    }

    /// In-memory database, for tests.
    pub fn open_in_memory() -> Result<Self> {
        Self::from_connection(Connection::open_in_memory()?)
    }

    fn from_connection(mut conn: Connection) -> Result<Self> {
        configure(&conn)?;
        migrations::migrate(&mut conn)?;
        Ok(Self { conn: Mutex::new(conn) })
    }

    /// Schema version in the open database.
    pub fn schema_version(&self) -> Result<i64> {
        migrations::current_version(&self.conn())
    }

    /// Lock the connection.
    ///
    /// A panic while holding the lock poisons it; the data is still consistent
    /// because every multi-statement write runs in a transaction that is rolled
    /// back on unwind, so recovering the guard is preferable to aborting every
    /// later command.
    pub(crate) fn conn(&self) -> MutexGuard<'_, Connection> {
        self.conn.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
    }
}

/// Connection pragmas.
fn configure(conn: &Connection) -> Result<()> {
    // WAL survives across connections; setting it on an in-memory database is
    // a no-op that returns "memory", which is not an error.
    let _: String = conn
        .query_row("PRAGMA journal_mode = WAL", [], |row| row.get(0))
        .unwrap_or_else(|_| "unknown".to_string());
    conn.busy_timeout(std::time::Duration::from_millis(BUSY_TIMEOUT_MS as u64))?;
    conn.execute_batch(
        "PRAGMA foreign_keys = ON;
         PRAGMA synchronous = NORMAL;
         PRAGMA temp_store = MEMORY;",
    )?;
    Ok(())
}

impl Storage for SqliteStore {
    fn maintenance(&self) -> Result<()> {
        let conn = self.conn();
        conn.execute_batch("ANALYZE; PRAGMA wal_checkpoint(TRUNCATE); VACUUM;")?;
        Ok(())
    }

    fn schema_version(&self) -> Result<i64> {
        migrations::current_version(&self.conn())
    }
}

/// Serialise a string list into a JSON column.
pub(crate) fn to_json_list(items: &[String]) -> String {
    serde_json::to_string(items).unwrap_or_else(|_| "[]".to_string())
}

/// Read a JSON list column, tolerating legacy or hand-edited values.
///
/// A malformed value degrades to an empty list rather than failing the query:
/// losing an auxiliary file list is far better than making a memory unreadable.
pub(crate) fn from_json_list(raw: &str) -> Vec<String> {
    match serde_json::from_str::<Vec<String>>(raw) {
        Ok(items) => items,
        Err(_) if raw.trim().is_empty() => Vec::new(),
        Err(err) => {
            tracing::warn!(error = %err, "ignoring malformed JSON list column");
            Vec::new()
        }
    }
}

/// Build `IN (?, ?, …)` with the given parameter count.
pub(crate) fn placeholders(n: usize) -> String {
    let mut s = String::with_capacity(n * 2);
    for i in 0..n {
        if i > 0 {
            s.push(',');
        }
        s.push('?');
    }
    s
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn opens_and_migrates() {
        let store = SqliteStore::open_in_memory().unwrap();
        assert_eq!(store.schema_version().unwrap(), migrations::target_version());
    }

    #[test]
    fn fts5_is_available() {
        let store = SqliteStore::open_in_memory().unwrap();
        let conn = store.conn();
        conn.execute_batch(
            "CREATE VIRTUAL TABLE probe USING fts5(x);
             INSERT INTO probe(x) VALUES ('hello world');",
        )
        .expect("FTS5 must be compiled into the bundled SQLite");
        let hits: i64 = conn
            .query_row("SELECT count(*) FROM probe WHERE probe MATCH 'hello'", [], |r| r.get(0))
            .unwrap();
        assert_eq!(hits, 1);
    }

    #[test]
    fn foreign_keys_are_enforced() {
        let store = SqliteStore::open_in_memory().unwrap();
        let conn = store.conn();
        let err = conn.execute(
            "INSERT INTO checkpoints (id, project_id, summary, created_at)
             VALUES ('c1', 'missing-project', 's', '2026-01-01T00:00:00Z')",
            [],
        );
        assert!(err.is_err(), "foreign keys should be enforced");
    }

    #[test]
    fn json_list_roundtrip_and_tolerance() {
        let items = vec!["a.rs".to_string(), "b.rs".to_string()];
        assert_eq!(from_json_list(&to_json_list(&items)), items);
        assert!(from_json_list("not json").is_empty());
        assert!(from_json_list("").is_empty());
    }

    #[test]
    fn placeholders_shape() {
        assert_eq!(placeholders(0), "");
        assert_eq!(placeholders(3), "?,?,?");
    }
}
