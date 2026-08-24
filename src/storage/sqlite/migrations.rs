//! Schema migrations.
//!
//! Migrations are embedded in the binary, applied in order inside a
//! transaction, and recorded with a checksum. The checksum turns "someone
//! edited a shipped migration" from a silent corruption into a loud error, and
//! a database written by a newer ContextD is refused rather than downgraded.

use rusqlite::{Connection, Transaction};

use crate::error::{Error, Result};
use crate::util::hash::sha256_hex;
use crate::util::time;

/// One embedded migration step.
#[derive(Debug, Clone, Copy)]
pub struct Migration {
    pub version: i64,
    pub name: &'static str,
    pub sql: &'static str,
}

/// All migrations, ordered by version. Append only — never edit a released one.
pub const MIGRATIONS: &[Migration] = &[
    Migration { version: 1, name: "init", sql: include_str!("migrations/0001_init.sql") },
    Migration { version: 2, name: "fts", sql: include_str!("migrations/0002_fts.sql") },
    Migration { version: 3, name: "sessions", sql: include_str!("migrations/0003_sessions.sql") },
    Migration {
        version: 4,
        name: "tombstones",
        sql: include_str!("migrations/0004_tombstones.sql"),
    },
];

/// Highest version this binary knows about.
pub fn target_version() -> i64 {
    MIGRATIONS.last().map_or(0, |m| m.version)
}

/// Version currently stored in `conn`.
pub fn current_version(conn: &Connection) -> Result<i64> {
    ensure_bookkeeping_table(conn)?;
    let version: Option<i64> =
        conn.query_row("SELECT MAX(version) FROM schema_migrations", [], |row| row.get(0))?;
    Ok(version.unwrap_or(0))
}

/// Bring `conn` up to [`target_version`]. Idempotent.
pub fn migrate(conn: &mut Connection) -> Result<Vec<i64>> {
    ensure_bookkeeping_table(conn)?;
    verify_applied(conn)?;

    let current = current_version(conn)?;
    if current > target_version() {
        return Err(Error::Config(format!(
            "database schema version {current} is newer than this build supports \
             ({}); upgrade contextd",
            target_version()
        )));
    }

    let mut applied = Vec::new();
    for migration in MIGRATIONS.iter().filter(|m| m.version > current) {
        let tx = conn.transaction()?;
        apply(&tx, migration)?;
        tx.commit()?;
        tracing::info!(version = migration.version, name = migration.name, "applied migration");
        applied.push(migration.version);
    }
    Ok(applied)
}

fn apply(tx: &Transaction<'_>, migration: &Migration) -> Result<()> {
    tx.execute_batch(migration.sql).map_err(|source| Error::Migration {
        name: format!("{:04}_{}", migration.version, migration.name),
        source,
    })?;
    tx.execute(
        "INSERT INTO schema_migrations (version, name, checksum, applied_at)
         VALUES (?1, ?2, ?3, ?4)",
        rusqlite::params![
            migration.version,
            migration.name,
            sha256_hex(migration.sql.as_bytes()),
            time::to_storage(&time::now()),
        ],
    )?;
    Ok(())
}

/// Refuse to run when an already-applied migration's text has changed.
fn verify_applied(conn: &Connection) -> Result<()> {
    let mut stmt = conn.prepare("SELECT version, name, checksum FROM schema_migrations")?;
    let rows = stmt
        .query_map([], |row| {
            Ok((row.get::<_, i64>(0)?, row.get::<_, String>(1)?, row.get::<_, String>(2)?))
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;

    for (version, name, checksum) in rows {
        let Some(known) = MIGRATIONS.iter().find(|m| m.version == version) else {
            // A migration this build has never heard of: the database came
            // from a newer version. `migrate` reports that with a better message.
            continue;
        };
        let expected = sha256_hex(known.sql.as_bytes());
        if expected != checksum {
            return Err(Error::Config(format!(
                "migration {version:04}_{name} has changed since it was applied \
                 (recorded {}, embedded {}); this database was created by an \
                 incompatible build",
                &checksum[..8.min(checksum.len())],
                &expected[..8],
            )));
        }
    }
    Ok(())
}

fn ensure_bookkeeping_table(conn: &Connection) -> Result<()> {
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS schema_migrations (
            version    INTEGER PRIMARY KEY,
            name       TEXT NOT NULL,
            checksum   TEXT NOT NULL,
            applied_at TEXT NOT NULL
         );",
    )?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn mem() -> Connection {
        Connection::open_in_memory().unwrap()
    }

    #[test]
    fn migrate_applies_all_then_is_idempotent() {
        let mut conn = mem();
        let applied = migrate(&mut conn).unwrap();
        assert_eq!(applied, (1..=target_version()).collect::<Vec<_>>());
        assert_eq!(current_version(&conn).unwrap(), target_version());

        let again = migrate(&mut conn).unwrap();
        assert!(again.is_empty(), "second migrate should be a no-op");
    }

    #[test]
    fn versions_are_unique_and_ordered() {
        let mut last = 0;
        for m in MIGRATIONS {
            assert!(m.version > last, "migration versions must ascend: {}", m.version);
            last = m.version;
        }
    }

    #[test]
    fn tampered_migration_is_rejected() {
        let mut conn = mem();
        migrate(&mut conn).unwrap();
        conn.execute("UPDATE schema_migrations SET checksum = 'deadbeef' WHERE version = 1", [])
            .unwrap();
        let err = migrate(&mut conn).unwrap_err().to_string();
        assert!(err.contains("has changed"), "unexpected error: {err}");
    }

    #[test]
    fn future_schema_is_refused() {
        let mut conn = mem();
        migrate(&mut conn).unwrap();
        conn.execute(
            "INSERT INTO schema_migrations (version, name, checksum, applied_at)
             VALUES (9999, 'from-the-future', 'x', '2030-01-01T00:00:00Z')",
            [],
        )
        .unwrap();
        let err = migrate(&mut conn).unwrap_err().to_string();
        assert!(err.contains("newer than this build"), "unexpected error: {err}");
    }

    #[test]
    fn expected_tables_exist_after_migration() {
        let mut conn = mem();
        migrate(&mut conn).unwrap();
        let mut stmt =
            conn.prepare("SELECT name FROM sqlite_master WHERE type IN ('table','view')").unwrap();
        let names: Vec<String> =
            stmt.query_map([], |r| r.get(0)).unwrap().map(|r| r.unwrap()).collect();
        for expected in [
            "projects",
            "memories",
            "memory_tags",
            "checkpoints",
            "architecture_decisions",
            "sessions",
            "agent_bindings",
            "tombstones",
            "embeddings",
            "search_index",
        ] {
            assert!(names.iter().any(|n| n == expected), "missing table {expected} in {names:?}");
        }
    }
}
