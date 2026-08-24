//! Migration tests.
//!
//! These guard the two things that break a long-lived local database: applying
//! migrations twice, and opening a database written by a different build.

use contextd::storage::sqlite::migrations::{self, MIGRATIONS};
use contextd::storage::SqliteStore;
use rusqlite::Connection;

#[test]
fn a_fresh_database_reaches_the_target_version() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("contextd.db");
    let store = SqliteStore::open(&path).unwrap();
    assert_eq!(store.schema_version().unwrap(), migrations::target_version());
    drop(store);

    // Re-opening is a no-op, not a re-run.
    let reopened = SqliteStore::open(&path).unwrap();
    assert_eq!(reopened.schema_version().unwrap(), migrations::target_version());
}

#[test]
fn a_partially_migrated_database_is_brought_up_to_date() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("contextd.db");

    // Apply only the first migration, as an older build would have.
    {
        let mut conn = Connection::open(&path).unwrap();
        let first = MIGRATIONS[0];
        let tx = conn.transaction().unwrap();
        tx.execute_batch(first.sql).unwrap();
        tx.execute_batch(
            "CREATE TABLE IF NOT EXISTS schema_migrations (
                version INTEGER PRIMARY KEY, name TEXT NOT NULL,
                checksum TEXT NOT NULL, applied_at TEXT NOT NULL);",
        )
        .unwrap();
        let checksum =
            format!("{:x}", <sha2::Sha256 as sha2::Digest>::digest(first.sql.as_bytes()));
        tx.execute(
            "INSERT INTO schema_migrations (version, name, checksum, applied_at)
             VALUES (?1, ?2, ?3, '2026-01-01T00:00:00Z')",
            rusqlite::params![first.version, first.name, checksum],
        )
        .unwrap();
        tx.commit().unwrap();
    }

    let store = SqliteStore::open(&path).unwrap();
    assert_eq!(store.schema_version().unwrap(), migrations::target_version());

    // The FTS table from the second migration must now exist and work.
    let conn = Connection::open(&path).unwrap();
    let count: i64 = conn
        .query_row("SELECT count(*) FROM search_index", [], |row| row.get(0))
        .expect("search_index should exist after migrating");
    assert_eq!(count, 0);
}

#[test]
fn a_database_from_a_newer_build_is_refused() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("contextd.db");
    SqliteStore::open(&path).unwrap();

    {
        let conn = Connection::open(&path).unwrap();
        conn.execute(
            "INSERT INTO schema_migrations (version, name, checksum, applied_at)
             VALUES (9999, 'future', 'x', '2030-01-01T00:00:00Z')",
            [],
        )
        .unwrap();
    }

    let err = SqliteStore::open(&path).unwrap_err().to_string();
    assert!(err.contains("newer than this build"), "unexpected error: {err}");
}

#[test]
fn data_survives_a_reopen() {
    use contextd::core::model::{Category, Memory};
    use contextd::storage::repository::{MemoryFilter, MemoryRepository};

    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("contextd.db");

    let id = {
        let store = SqliteStore::open(&path).unwrap();
        let memory = Memory::new(Category::Architecture, "Transport", "Scheduler uses NATS");
        store.create_memory(&memory).unwrap();
        memory.id
    };

    let store = SqliteStore::open(&path).unwrap();
    let loaded = store.get_memory(&id).unwrap().expect("memory survives reopen");
    assert_eq!(loaded.content, "Scheduler uses NATS");
    assert_eq!(store.count_memories(&MemoryFilter::default()).unwrap(), 1);
}
