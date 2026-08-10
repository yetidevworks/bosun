//! Local SQLite store for per-user metadata that survives across
//! bosun runs. Right now this holds just one thing — the recents
//! list for the new-session modal — but the schema is versioned so
//! later phases can add tables (user prefs, session-specific
//! metadata, detector rules, etc) without surgery.
//!
//! Storage location (via the `directories` crate):
//!   macOS  → ~/Library/Application Support/bosun/bosun.db
//!   Linux  → ~/.local/share/bosun/bosun.db
//!   Windows → %APPDATA%/bosun/bosun.db
//!
//! Tests use `Store::in_memory()` to avoid touching the real filesystem.
//!
//! Concurrency: a single `Mutex<Connection>` guards the connection.
//! rusqlite is a blocking API, so callers must call from a
//! `spawn_blocking` context or from a synchronous actor path. The
//! current tmux_actor calls are synchronous-enough that inline use
//! is fine — each SQL roundtrip is ~microseconds on a local file.

use std::path::PathBuf;
use std::sync::Mutex;

use rusqlite::Connection;

use crate::error::{BosunError, Result};

pub mod recents;

pub use recents::Recent;

pub struct Store {
    conn: Mutex<Connection>,
}

impl Store {
    /// Open (or create) the default on-disk store under the user's
    /// platform-standard data directory. Creates parent dirs and runs
    /// migrations. Returns an error if the directory can't be created
    /// or the DB file can't be opened.
    pub fn open_default() -> Result<Self> {
        let path = default_db_path()?;
        Self::open_at(&path)
    }

    /// Open a store at an explicit path. Useful for `--db` overrides
    /// and for tests that want to exercise real file I/O.
    pub fn open_at(path: &std::path::Path) -> Result<Self> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).map_err(BosunError::Io)?;
        }
        let conn = Connection::open(path).map_err(map_sql_err)?;
        let mut s = Self {
            conn: Mutex::new(conn),
        };
        s.migrate()?;
        Ok(s)
    }

    /// In-memory store for tests. No filesystem involvement.
    #[allow(dead_code)]
    pub fn in_memory() -> Result<Self> {
        let conn = Connection::open_in_memory().map_err(map_sql_err)?;
        let mut s = Self {
            conn: Mutex::new(conn),
        };
        s.migrate()?;
        Ok(s)
    }

    fn migrate(&mut self) -> Result<()> {
        let conn = self.conn.get_mut().expect("store mutex poisoned");
        conn.pragma_update(None, "journal_mode", "WAL")
            .map_err(map_sql_err)?;
        conn.pragma_update(None, "synchronous", "NORMAL")
            .map_err(map_sql_err)?;
        // Idempotent schema. Future-compatible approach: versioned
        // migrations in a schema_version table. For Phase 3c one
        // table is enough.
        conn.execute_batch(
            r#"
            CREATE TABLE IF NOT EXISTS recents (
                id                       INTEGER PRIMARY KEY AUTOINCREMENT,
                name                     TEXT NOT NULL,
                path                     TEXT NOT NULL,
                agent                    TEXT NOT NULL,
                args                     TEXT NOT NULL DEFAULT '',
                claude_session_mode      TEXT NOT NULL DEFAULT 'New',
                claude_skip_permissions  INTEGER NOT NULL DEFAULT 0,
                codex_yolo               INTEGER NOT NULL DEFAULT 0,
                kimi_session_mode        TEXT NOT NULL DEFAULT 'New',
                kimi_yolo                INTEGER NOT NULL DEFAULT 0,
                codex_session_mode       TEXT NOT NULL DEFAULT 'New',
                opencode_session_mode    TEXT NOT NULL DEFAULT 'New',
                opencode_auto            INTEGER NOT NULL DEFAULT 0,
                qwen_session_mode        TEXT NOT NULL DEFAULT 'New',
                qwen_yolo                INTEGER NOT NULL DEFAULT 0,
                last_used_at             INTEGER NOT NULL,
                use_count                INTEGER NOT NULL DEFAULT 1,
                UNIQUE(name, path, agent) ON CONFLICT IGNORE
            );
            CREATE INDEX IF NOT EXISTS idx_recents_last_used
                ON recents(last_used_at DESC);
            CREATE INDEX IF NOT EXISTS idx_recents_path
                ON recents(path);
            "#,
        )
        .map_err(map_sql_err)?;
        // Additive migrations for DBs created before newer agent
        // columns existed. `CREATE TABLE IF NOT EXISTS` above is a
        // no-op on an existing table, so add the columns explicitly
        // and treat the "duplicate column name" error as success
        // (idempotent re-run).
        for stmt in [
            "ALTER TABLE recents ADD COLUMN kimi_session_mode TEXT NOT NULL DEFAULT 'New'",
            "ALTER TABLE recents ADD COLUMN kimi_yolo INTEGER NOT NULL DEFAULT 0",
            "ALTER TABLE recents ADD COLUMN codex_session_mode TEXT NOT NULL DEFAULT 'New'",
            "ALTER TABLE recents ADD COLUMN opencode_session_mode TEXT NOT NULL DEFAULT 'New'",
            "ALTER TABLE recents ADD COLUMN opencode_auto INTEGER NOT NULL DEFAULT 0",
            "ALTER TABLE recents ADD COLUMN qwen_session_mode TEXT NOT NULL DEFAULT 'New'",
            "ALTER TABLE recents ADD COLUMN qwen_yolo INTEGER NOT NULL DEFAULT 0",
        ] {
            if let Err(e) = conn.execute(stmt, []) {
                let msg = e.to_string();
                if !msg.contains("duplicate column name") {
                    return Err(map_sql_err(e));
                }
            }
        }
        Ok(())
    }
}

fn default_db_path() -> Result<PathBuf> {
    let dirs = directories::ProjectDirs::from("dev", "yetidevworks", "bosun")
        .ok_or_else(|| BosunError::Store("could not determine user data directory".to_string()))?;
    Ok(dirs.data_dir().join("bosun.db"))
}

pub(crate) fn map_sql_err(e: rusqlite::Error) -> BosunError {
    BosunError::Store(e.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn in_memory_open_runs_migrations() {
        let s = Store::in_memory().expect("in-memory open");
        // The recents table should exist and be queryable.
        let count = s
            .conn
            .lock()
            .unwrap()
            .query_row("SELECT COUNT(*) FROM recents", [], |row| {
                row.get::<_, i64>(0)
            })
            .expect("select from recents");
        assert_eq!(count, 0);
    }
}
