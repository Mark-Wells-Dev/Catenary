// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 Mark Wells <contact@markwells.dev>

//! SQLite database connection management, schema creation, and migrations.
//!
//! Provides the foundation for all persistent state in Catenary. The database
//! file lives at `~/.local/state/catenary/catenary.db` (or platform equivalent).

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use rusqlite::Connection;

/// Current schema version. Bump when adding migrations.
const SCHEMA_VERSION: u32 = 9;

/// Resolve the Catenary state directory.
///
/// Resolution order:
/// 1. `CATENARY_STATE_DIR` environment variable (cross-platform override).
/// 2. `dirs::state_dir()` (`XDG_STATE_HOME` on Linux).
/// 3. `dirs::data_local_dir()` (macOS / Windows fallback).
/// 4. `/tmp` as a last resort.
#[must_use]
pub fn state_dir() -> PathBuf {
    std::env::var_os("CATENARY_STATE_DIR")
        .map(PathBuf::from)
        .or_else(dirs::state_dir)
        .or_else(dirs::data_local_dir)
        .unwrap_or_else(|| PathBuf::from("/tmp"))
}

/// Returns the path to the Catenary database file.
///
/// Uses [`state_dir`] for the base directory.
#[must_use]
pub fn db_path() -> PathBuf {
    state_dir().join("catenary").join("catenary.db")
}

/// Opens a connection to the Catenary database with standard pragmas.
///
/// Sets WAL journal mode, 5-second busy timeout, and enables foreign keys.
/// Creates the parent directory if it does not exist.
///
/// # Errors
///
/// Returns an error if the parent directory cannot be created or the
/// database cannot be opened.
pub fn open() -> Result<Connection> {
    open_at(&db_path())
}

/// Opens a connection to a database at the given path with standard pragmas.
///
/// Like [`open`] but uses an explicit path instead of the default location.
/// Useful for testing with temporary directories.
///
/// # Errors
///
/// Returns an error if the parent directory cannot be created or the
/// database cannot be opened.
pub fn open_at(path: &Path) -> Result<Connection> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).with_context(|| {
            format!("failed to create database directory: {}", parent.display())
        })?;
    }

    let conn = Connection::open(path)
        .with_context(|| format!("failed to open database: {}", path.display()))?;

    conn.execute_batch(
        "PRAGMA journal_mode=WAL;
         PRAGMA busy_timeout=5000;
         PRAGMA foreign_keys=ON;",
    )
    .context("failed to set database pragmas")?;

    Ok(conn)
}

/// Opens a connection and runs schema migrations if needed.
///
/// On a fresh database, creates all tables. On an existing database,
/// checks the schema version and applies any pending migrations.
///
/// # Errors
///
/// Returns an error if the connection cannot be opened, schema creation
/// fails, or a migration step fails.
pub fn open_and_migrate() -> Result<Connection> {
    open_and_migrate_at(&db_path())
}

/// Opens a connection at the given path and runs schema migrations if needed.
///
/// Like [`open_and_migrate`] but uses an explicit path instead of the default
/// location. Useful for testing with temporary directories.
///
/// # Errors
///
/// Returns an error if the connection cannot be opened, schema creation
/// fails, or a migration step fails.
pub fn open_and_migrate_at(path: &Path) -> Result<Connection> {
    let conn = open_at(path)?;

    if table_exists(&conn, "meta") {
        let version = current_schema_version(&conn)?;
        #[allow(
            clippy::collapsible_if,
            reason = "migration chain reads clearer with separate guards"
        )]
        if version < SCHEMA_VERSION {
            if version < 2 {
                migrate_v1_to_v2(&conn)?;
            }
            if version < 3 {
                migrate_v2_to_v3(&conn)?;
            }
            if version < 4 {
                migrate_v3_to_v4(&conn)?;
            }
            if version < 5 {
                migrate_v4_to_v5(&conn)?;
            }
            if version < 6 {
                migrate_v5_to_v6(&conn)?;
            }
            if version < 7 {
                migrate_v6_to_v7(&conn)?;
            }
            if version < 8 {
                migrate_v7_to_v8(&conn)?;
            }
            if version < 9 {
                migrate_v8_to_v9(&conn)?;
            }
        }
    } else {
        create_schema(&conn)?;
    }

    Ok(conn)
}

/// Checks whether a table exists in the database.
fn table_exists(conn: &Connection, name: &str) -> bool {
    conn.query_row(
        "SELECT COUNT(*) FROM sqlite_master WHERE type='table' AND name=?1",
        [name],
        |row| row.get::<_, i64>(0),
    )
    .is_ok_and(|count| count > 0)
}

/// Creates the full database schema in a single transaction.
///
/// # Errors
///
/// Returns an error if any CREATE TABLE or INSERT statement fails.
#[allow(
    clippy::too_many_lines,
    reason = "single execute_batch with all DDL statements"
)]
fn create_schema(conn: &Connection) -> Result<()> {
    conn.execute_batch(
        "BEGIN IMMEDIATE;

         CREATE TABLE IF NOT EXISTS meta (
             key   TEXT PRIMARY KEY,
             value TEXT NOT NULL
         );
         INSERT OR IGNORE INTO meta (key, value) VALUES ('schema_version', '9');

         CREATE TABLE IF NOT EXISTS sessions (
             id             TEXT PRIMARY KEY,
             pid            INTEGER NOT NULL,
             display_name   TEXT NOT NULL,
             client_name    TEXT,
             client_version TEXT,
             client_session_id TEXT,
             started_at     TEXT NOT NULL,
             ended_at       TEXT,
             alive          INTEGER NOT NULL DEFAULT 1
         );

         CREATE TABLE IF NOT EXISTS workspace_roots (
             session_id  TEXT NOT NULL REFERENCES sessions(id) ON DELETE CASCADE,
             root_path   TEXT NOT NULL,
             PRIMARY KEY (session_id, root_path)
         );

         CREATE TABLE IF NOT EXISTS events (
             id          INTEGER PRIMARY KEY AUTOINCREMENT,
             session_id  TEXT NOT NULL REFERENCES sessions(id) ON DELETE CASCADE,
             timestamp   TEXT NOT NULL,
             kind        TEXT NOT NULL,
             payload     TEXT NOT NULL,
             created_at  TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ', 'now'))
         );

         CREATE INDEX IF NOT EXISTS idx_events_session_id ON events(session_id, id);
         CREATE INDEX IF NOT EXISTS idx_events_timestamp ON events(timestamp);

         CREATE TABLE IF NOT EXISTS messages (
             id          INTEGER PRIMARY KEY AUTOINCREMENT,
             session_id  TEXT NOT NULL REFERENCES sessions(id) ON DELETE CASCADE,
             timestamp   TEXT NOT NULL,
             type        TEXT NOT NULL,
             level       TEXT NOT NULL DEFAULT 'info',
             method      TEXT NOT NULL,
             server      TEXT NOT NULL,
             client      TEXT NOT NULL,
             request_id  INTEGER,
             parent_id   INTEGER,
             payload     TEXT NOT NULL,
             created_at  TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ', 'now'))
         );

         CREATE INDEX IF NOT EXISTS idx_messages_session ON messages(session_id);
         CREATE INDEX IF NOT EXISTS idx_messages_type ON messages(type);
         CREATE INDEX IF NOT EXISTS idx_messages_level ON messages(level);
         CREATE INDEX IF NOT EXISTS idx_messages_request_id ON messages(request_id);
         CREATE INDEX IF NOT EXISTS idx_messages_parent_id ON messages(parent_id);

         CREATE TABLE IF NOT EXISTS language_servers (
             session_id   TEXT NOT NULL REFERENCES sessions(id) ON DELETE CASCADE,
             language_id  TEXT NOT NULL,
             server       TEXT NOT NULL,
             scope_kind   TEXT NOT NULL,
             scope_root   TEXT NOT NULL DEFAULT '',
             state        TEXT NOT NULL,
             PRIMARY KEY (session_id, language_id, server, scope_kind, scope_root)
         );

         CREATE TABLE IF NOT EXISTS filter_history (
             id          INTEGER PRIMARY KEY AUTOINCREMENT,
             workspace   TEXT NOT NULL,
             pattern     TEXT NOT NULL,
             created_at  TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ', 'now'))
         );

         CREATE INDEX IF NOT EXISTS idx_filter_workspace ON filter_history(workspace, created_at DESC);

         COMMIT;",
    )
    .context("failed to create database schema")?;

    Ok(())
}

/// Migrates the database from schema version 1 to 2.
///
/// Adds grammar registry, symbol index, and file parse state tables
/// for the `SEARCHv2` feature.
///
/// # Errors
///
/// Returns an error if any table creation or version update fails.
fn migrate_v1_to_v2(conn: &Connection) -> Result<()> {
    conn.execute_batch(
        "BEGIN IMMEDIATE;

         CREATE TABLE grammars (
             scope       TEXT PRIMARY KEY,
             file_types  TEXT NOT NULL,
             lib_path    TEXT NOT NULL,
             tags_path   TEXT NOT NULL,
             repo_url    TEXT NOT NULL,
             installed_at TEXT NOT NULL
         );

         CREATE TABLE symbols (
             file_path   TEXT NOT NULL,
             name        TEXT NOT NULL,
             kind        TEXT NOT NULL,
             line        INTEGER NOT NULL,
             end_line    INTEGER NOT NULL,
             scope       TEXT,
             scope_kind  TEXT,
             PRIMARY KEY (file_path, line)
         );

         CREATE INDEX idx_symbols_name ON symbols(name);

         CREATE TABLE file_parse_state (
             file_path   TEXT PRIMARY KEY,
             mtime_ns    INTEGER NOT NULL,
             grammar     TEXT NOT NULL REFERENCES grammars(scope)
         );

         UPDATE meta SET value = '2' WHERE key = 'schema_version';

         COMMIT;",
    )
    .context("failed to migrate schema from v1 to v2")?;

    Ok(())
}

/// Migrates the database from schema version 2 to 3.
///
/// Adds `client_session_id` column to the `sessions` table for storing
/// the host CLI's session ID (e.g., Claude Code or Gemini CLI UUID).
///
/// # Errors
///
/// Returns an error if the column addition or version update fails.
fn migrate_v2_to_v3(conn: &Connection) -> Result<()> {
    conn.execute_batch(
        "BEGIN IMMEDIATE;

         ALTER TABLE sessions ADD COLUMN client_session_id TEXT;

         UPDATE meta SET value = '3' WHERE key = 'schema_version';

         COMMIT;",
    )
    .context("failed to migrate schema from v2 to v3")?;

    Ok(())
}

/// Migrates the database from schema version 3 to 4.
///
/// Adds the `messages` table for protocol message logging (collapse workstream).
///
/// # Errors
///
/// Returns an error if the table creation or version update fails.
fn migrate_v3_to_v4(conn: &Connection) -> Result<()> {
    conn.execute_batch(
        "BEGIN IMMEDIATE;

         CREATE TABLE messages (
             id          INTEGER PRIMARY KEY AUTOINCREMENT,
             session_id  TEXT NOT NULL REFERENCES sessions(id) ON DELETE CASCADE,
             timestamp   TEXT NOT NULL,
             type        TEXT NOT NULL,
             method      TEXT NOT NULL,
             server      TEXT NOT NULL,
             client      TEXT NOT NULL,
             request_id  INTEGER REFERENCES messages(id),
             parent_id   INTEGER REFERENCES messages(id),
             payload     TEXT NOT NULL,
             created_at  TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ', 'now'))
         );

         CREATE INDEX idx_messages_session ON messages(session_id);
         CREATE INDEX idx_messages_type ON messages(type);
         CREATE INDEX idx_messages_request_id ON messages(request_id);
         CREATE INDEX idx_messages_parent_id ON messages(parent_id);

         UPDATE meta SET value = '4' WHERE key = 'schema_version';

         COMMIT;",
    )
    .context("failed to migrate schema from v3 to v4")?;

    Ok(())
}

/// Migrates the database from schema version 4 to 5.
///
/// Adds the `editing_state` table for per-file diagnostic suppression
/// during multi-edit sessions.
///
/// # Errors
///
/// Returns an error if the table creation or version update fails.
fn migrate_v4_to_v5(conn: &Connection) -> Result<()> {
    conn.execute_batch(
        "BEGIN IMMEDIATE;

         CREATE TABLE editing_state (
             file_path   TEXT NOT NULL,
             session_id  TEXT NOT NULL,
             agent_id    TEXT NOT NULL DEFAULT '',
             started_at  TEXT NOT NULL,
             PRIMARY KEY (file_path, session_id, agent_id)
         );

         UPDATE meta SET value = '5' WHERE key = 'schema_version';

         COMMIT;",
    )
    .context("failed to migrate schema from v4 to v5")?;

    Ok(())
}

/// Migrates the database from schema version 5 to 6.
///
/// Replaces the per-file `editing_state` table with a stateless editing
/// flag (`session_id, agent_id` only) and a separate `editing_files` table
/// for accumulating modified file paths during editing mode.
///
/// # Errors
///
/// Returns an error if the table recreation or version update fails.
fn migrate_v5_to_v6(conn: &Connection) -> Result<()> {
    conn.execute_batch(
        "BEGIN IMMEDIATE;

         DROP TABLE IF EXISTS editing_state;

         CREATE TABLE editing_state (
             session_id  TEXT NOT NULL,
             agent_id    TEXT NOT NULL DEFAULT '',
             started_at  TEXT NOT NULL,
             PRIMARY KEY (session_id, agent_id)
         );

         CREATE TABLE IF NOT EXISTS editing_files (
             session_id  TEXT NOT NULL,
             agent_id    TEXT NOT NULL DEFAULT '',
             file_path   TEXT NOT NULL,
             PRIMARY KEY (session_id, agent_id, file_path)
         );

         UPDATE meta SET value = '6' WHERE key = 'schema_version';

         COMMIT;",
    )
    .context("failed to migrate schema from v5 to v6")?;

    Ok(())
}

/// Migrates the database from schema version 6 to 7.
///
/// Drops the `editing_state` and `editing_files` tables. Editing state
/// is now managed in-memory by `EditingManager` on `Session`.
///
/// # Errors
///
/// Returns an error if the table drop or version update fails.
fn migrate_v6_to_v7(conn: &Connection) -> Result<()> {
    conn.execute_batch(
        "BEGIN IMMEDIATE;

         DROP TABLE IF EXISTS editing_files;
         DROP TABLE IF EXISTS editing_state;

         UPDATE meta SET value = '7' WHERE key = 'schema_version';

         COMMIT;",
    )
    .context("failed to migrate schema from v6 to v7")?;

    Ok(())
}

/// Migrates the database from schema version 7 to 8.
///
/// Recreates the `language_servers` table with a wider primary key
/// (`language_id`, `server`, `scope_kind`, `scope_root`) to support
/// multiple instances per language (different servers, different scopes).
///
/// # Errors
///
/// Returns an error if the table recreation or version update fails.
fn migrate_v7_to_v8(conn: &Connection) -> Result<()> {
    conn.execute_batch(
        "BEGIN IMMEDIATE;

         DROP TABLE IF EXISTS language_servers;

         CREATE TABLE language_servers (
             session_id   TEXT NOT NULL REFERENCES sessions(id) ON DELETE CASCADE,
             language_id  TEXT NOT NULL,
             server       TEXT NOT NULL,
             scope_kind   TEXT NOT NULL,
             scope_root   TEXT NOT NULL DEFAULT '',
             state        TEXT NOT NULL,
             PRIMARY KEY (session_id, language_id, server, scope_kind, scope_root)
         );

         UPDATE meta SET value = '8' WHERE key = 'schema_version';

         COMMIT;",
    )
    .context("failed to migrate schema from v7 to v8")?;

    Ok(())
}

/// Migrates the database from schema version 8 to 9.
///
/// Adds a `level` column (tracing severity: `debug`/`info`/`warn`/`error`)
/// to the `messages` table and drops the foreign-key constraints on
/// `request_id` / `parent_id`. Those columns hold in-process monotonic
/// correlation IDs from `LoggingServer::next_id()`, not ROWIDs, so the FK
/// constraints caused every protocol message INSERT to fail.
///
/// `SQLite` does not support `ALTER TABLE … DROP CONSTRAINT`, so the table
/// is recreated. Existing trace-event rows (whose `type` was a severity
/// string) get `type = 'internal'` and `level` set to the old `type` value.
/// Protocol rows keep their `type` and receive `level = 'info'`.
///
/// # Errors
///
/// Returns an error if the table recreation or version update fails.
fn migrate_v8_to_v9(conn: &Connection) -> Result<()> {
    conn.execute_batch(
        "BEGIN IMMEDIATE;

         CREATE TABLE messages_new (
             id          INTEGER PRIMARY KEY AUTOINCREMENT,
             session_id  TEXT NOT NULL REFERENCES sessions(id) ON DELETE CASCADE,
             timestamp   TEXT NOT NULL,
             type        TEXT NOT NULL,
             level       TEXT NOT NULL DEFAULT 'info',
             method      TEXT NOT NULL,
             server      TEXT NOT NULL,
             client      TEXT NOT NULL,
             request_id  INTEGER,
             parent_id   INTEGER,
             payload     TEXT NOT NULL,
             created_at  TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ', 'now'))
         );

         INSERT INTO messages_new
             (id, session_id, timestamp, type, level, method, server, client,
              request_id, parent_id, payload, created_at)
         SELECT
             id, session_id, timestamp,
             CASE WHEN type IN ('lsp','mcp','hook') THEN type ELSE 'internal' END,
             CASE WHEN type IN ('lsp','mcp','hook') THEN 'info' ELSE type END,
             method, server, client,
             request_id, parent_id,
             payload, created_at
         FROM messages;

         DROP TABLE messages;
         ALTER TABLE messages_new RENAME TO messages;

         CREATE INDEX idx_messages_session ON messages(session_id);
         CREATE INDEX idx_messages_type ON messages(type);
         CREATE INDEX idx_messages_level ON messages(level);
         CREATE INDEX idx_messages_request_id ON messages(request_id);
         CREATE INDEX idx_messages_parent_id ON messages(parent_id);

         UPDATE meta SET value = '9' WHERE key = 'schema_version';

         COMMIT;",
    )
    .context("failed to migrate schema from v8 to v9")?;

    Ok(())
}

/// Reads the current schema version from the `meta` table.
///
/// # Errors
///
/// Returns an error if the `meta` table cannot be queried or the version
/// value cannot be parsed as a `u32`.
fn current_schema_version(conn: &Connection) -> Result<u32> {
    let version_str: String = conn
        .query_row(
            "SELECT value FROM meta WHERE key = 'schema_version'",
            [],
            |row| row.get(0),
        )
        .context("failed to read schema_version from meta table")?;

    version_str
        .parse::<u32>()
        .with_context(|| format!("invalid schema_version: {version_str}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[allow(clippy::expect_used, reason = "test assertions")]
    #[test]
    fn test_open_creates_db_file() {
        let dir = tempfile::tempdir().expect("failed to create tempdir");
        let path = dir.path().join("test.db");

        let _conn = open_at(&path).expect("open_at failed");
        assert!(path.exists(), "database file should exist after open");
    }

    #[allow(clippy::expect_used, reason = "test assertions")]
    #[test]
    fn test_open_and_migrate_creates_schema() {
        let dir = tempfile::tempdir().expect("failed to create tempdir");
        let path = dir.path().join("test.db");

        let conn = open_and_migrate_at(&path).expect("open_and_migrate_at failed");

        let expected_tables = [
            "meta",
            "sessions",
            "workspace_roots",
            "events",
            "messages",
            "language_servers",
            "filter_history",
        ];

        for table in &expected_tables {
            assert!(
                table_exists(&conn, table),
                "table '{table}' should exist after migration"
            );
        }
    }

    #[allow(clippy::expect_used, reason = "test assertions")]
    #[test]
    fn test_open_and_migrate_idempotent() {
        let dir = tempfile::tempdir().expect("failed to create tempdir");
        let path = dir.path().join("test.db");

        let conn1 = open_and_migrate_at(&path).expect("first open_and_migrate_at failed");
        drop(conn1);

        let conn2 = open_and_migrate_at(&path).expect("second open_and_migrate_at should succeed");

        assert!(
            table_exists(&conn2, "meta"),
            "meta table should still exist after second migration"
        );
    }

    #[allow(clippy::expect_used, reason = "test assertions")]
    #[test]
    fn test_schema_version() {
        let dir = tempfile::tempdir().expect("failed to create tempdir");
        let path = dir.path().join("test.db");

        let conn = open_and_migrate_at(&path).expect("open_and_migrate_at failed");

        let version = current_schema_version(&conn).expect("failed to read schema version");
        assert_eq!(version, 9, "schema version should be 9");
    }

    #[allow(clippy::expect_used, reason = "test assertions")]
    #[test]
    fn test_wal_mode() {
        let dir = tempfile::tempdir().expect("failed to create tempdir");
        let path = dir.path().join("test.db");

        let conn = open_at(&path).expect("open_at failed");

        let mode: String = conn
            .query_row("PRAGMA journal_mode", [], |row| row.get(0))
            .expect("failed to query journal_mode");

        assert_eq!(mode, "wal", "journal mode should be WAL");
    }

    #[allow(clippy::expect_used, reason = "test assertions")]
    #[test]
    fn test_foreign_keys_enabled() {
        let dir = tempfile::tempdir().expect("failed to create tempdir");
        let path = dir.path().join("test.db");

        let conn = open_at(&path).expect("open_at failed");

        let fk: i64 = conn
            .query_row("PRAGMA foreign_keys", [], |row| row.get(0))
            .expect("failed to query foreign_keys");

        assert_eq!(fk, 1, "foreign keys should be enabled");
    }

    // Grammar/symbol/parse_state tables removed from schema in SEARCHv2
    // ticket 06a — tree-sitter index uses in-memory SQLite, grammar registry
    // uses filesystem metadata.json sidecars.

    #[allow(clippy::expect_used, reason = "test assertions")]
    #[test]
    fn test_migration_v1_to_v2() {
        let dir = tempfile::tempdir().expect("failed to create tempdir");
        let path = dir.path().join("test.db");

        // Create a v1 database manually (meta + sessions tables).
        let conn = open_at(&path).expect("open_at failed");
        conn.execute_batch(
            "BEGIN IMMEDIATE;
             CREATE TABLE meta (key TEXT PRIMARY KEY, value TEXT NOT NULL);
             INSERT INTO meta (key, value) VALUES ('schema_version', '1');
             CREATE TABLE sessions (
                 id             TEXT PRIMARY KEY,
                 pid            INTEGER NOT NULL,
                 display_name   TEXT NOT NULL,
                 client_name    TEXT,
                 client_version TEXT,
                 started_at     TEXT NOT NULL,
                 ended_at       TEXT,
                 alive          INTEGER NOT NULL DEFAULT 1
             );
             COMMIT;",
        )
        .expect("failed to create v1 schema");
        drop(conn);

        // Open with migration — should upgrade through v2 to v3.
        let conn = open_and_migrate_at(&path).expect("open_and_migrate_at failed");

        let version = current_schema_version(&conn).expect("failed to read schema version");
        assert_eq!(version, 9, "schema version should be 9 after migration");

        for table in &["grammars", "symbols", "file_parse_state"] {
            assert!(
                table_exists(&conn, table),
                "table '{table}' should exist after v1→v2 migration"
            );
        }
    }

    #[allow(clippy::expect_used, reason = "test assertions")]
    #[test]
    fn test_migration_v2_to_v3() {
        let dir = tempfile::tempdir().expect("failed to create tempdir");
        let path = dir.path().join("test.db");

        // Create a v2 database manually.
        let conn = open_at(&path).expect("open_at failed");
        conn.execute_batch(
            "BEGIN IMMEDIATE;
             CREATE TABLE meta (key TEXT PRIMARY KEY, value TEXT NOT NULL);
             INSERT INTO meta (key, value) VALUES ('schema_version', '2');
             CREATE TABLE sessions (
                 id             TEXT PRIMARY KEY,
                 pid            INTEGER NOT NULL,
                 display_name   TEXT NOT NULL,
                 client_name    TEXT,
                 client_version TEXT,
                 started_at     TEXT NOT NULL,
                 ended_at       TEXT,
                 alive          INTEGER NOT NULL DEFAULT 1
             );
             COMMIT;",
        )
        .expect("failed to create v2 schema");
        drop(conn);

        // Open with migration — should upgrade to v3.
        let conn = open_and_migrate_at(&path).expect("open_and_migrate_at failed");

        let version = current_schema_version(&conn).expect("failed to read schema version");
        assert_eq!(version, 9, "schema version should be 9 after migration");

        // Verify client_session_id column exists by inserting a row that uses it.
        conn.execute(
            "INSERT INTO sessions (id, pid, display_name, started_at, client_session_id) \
             VALUES ('test', 1, 'test', '2026-01-01T00:00:00Z', 'client-uuid-123')",
            [],
        )
        .expect("insert with client_session_id should succeed");

        let csid: Option<String> = conn
            .query_row(
                "SELECT client_session_id FROM sessions WHERE id = 'test'",
                [],
                |row| row.get(0),
            )
            .expect("query client_session_id");
        assert_eq!(csid.as_deref(), Some("client-uuid-123"));
    }

    #[allow(clippy::expect_used, reason = "test assertions")]
    #[test]
    fn test_schema_migration_v3_to_v4() {
        let dir = tempfile::tempdir().expect("failed to create tempdir");
        let path = dir.path().join("test.db");

        // Create a v3 database manually.
        let conn = open_at(&path).expect("open_at failed");
        conn.execute_batch(
            "BEGIN IMMEDIATE;
             CREATE TABLE meta (key TEXT PRIMARY KEY, value TEXT NOT NULL);
             INSERT INTO meta (key, value) VALUES ('schema_version', '3');
             CREATE TABLE sessions (
                 id             TEXT PRIMARY KEY,
                 pid            INTEGER NOT NULL,
                 display_name   TEXT NOT NULL,
                 client_name    TEXT,
                 client_version TEXT,
                 client_session_id TEXT,
                 started_at     TEXT NOT NULL,
                 ended_at       TEXT,
                 alive          INTEGER NOT NULL DEFAULT 1
             );
             COMMIT;",
        )
        .expect("failed to create v3 schema");
        drop(conn);

        // Open with migration — should upgrade to v4.
        let conn = open_and_migrate_at(&path).expect("open_and_migrate_at failed");

        let version = current_schema_version(&conn).expect("failed to read schema version");
        assert_eq!(version, 9, "schema version should be 9 after migration");

        assert!(
            table_exists(&conn, "messages"),
            "messages table should exist after v3→v4 migration"
        );

        // Verify the table is usable by inserting a row.
        conn.execute(
            "INSERT INTO sessions (id, pid, display_name, started_at) \
             VALUES ('test-session', 1, 'test', '2026-01-01T00:00:00Z')",
            [],
        )
        .expect("insert session");

        conn.execute(
            "INSERT INTO messages \
             (session_id, timestamp, type, method, server, client, payload) \
             VALUES ('test-session', '2026-01-01T00:00:00Z', 'lsp', \
                     'textDocument/hover', 'rust-analyzer', 'catenary', '{}')",
            [],
        )
        .expect("insert into messages should succeed after migration");
    }

    #[allow(clippy::expect_used, reason = "test assertions")]
    #[test]
    fn test_schema_migration_v4_to_v7() {
        let dir = tempfile::tempdir().expect("failed to create tempdir");
        let path = dir.path().join("test.db");

        let conn = open_at(&path).expect("open_at failed");
        conn.execute_batch(
            "PRAGMA foreign_keys=OFF;
             BEGIN IMMEDIATE;
             CREATE TABLE meta (key TEXT PRIMARY KEY, value TEXT NOT NULL);
             INSERT INTO meta (key, value) VALUES ('schema_version', '4');
             CREATE TABLE sessions (
                 id TEXT PRIMARY KEY, pid INTEGER NOT NULL,
                 display_name TEXT NOT NULL, started_at TEXT NOT NULL,
                 ended_at TEXT, alive INTEGER NOT NULL DEFAULT 1
             );
             CREATE TABLE messages (
                 id          INTEGER PRIMARY KEY AUTOINCREMENT,
                 session_id  TEXT NOT NULL REFERENCES sessions(id) ON DELETE CASCADE,
                 timestamp   TEXT NOT NULL,
                 type        TEXT NOT NULL,
                 method      TEXT NOT NULL,
                 server      TEXT NOT NULL,
                 client      TEXT NOT NULL,
                 request_id  INTEGER REFERENCES messages(id),
                 parent_id   INTEGER REFERENCES messages(id),
                 payload     TEXT NOT NULL,
                 created_at  TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ', 'now'))
             );
             COMMIT;
             PRAGMA foreign_keys=ON;",
        )
        .expect("failed to create v4 schema");
        drop(conn);

        let conn = open_and_migrate_at(&path).expect("open_and_migrate_at failed");

        let version = current_schema_version(&conn).expect("failed to read schema version");
        assert_eq!(version, 9, "schema version should be 9 after migration");

        // Editing tables should be dropped by v6→v7
        assert!(
            !table_exists(&conn, "editing_state"),
            "editing_state should not exist after v7 migration"
        );
        assert!(
            !table_exists(&conn, "editing_files"),
            "editing_files should not exist after v7 migration"
        );
    }

    #[allow(clippy::expect_used, reason = "test assertions")]
    #[test]
    fn test_schema_migration_v6_to_v7() {
        let dir = tempfile::tempdir().expect("failed to create tempdir");
        let path = dir.path().join("test.db");

        // Create a v6 database with editing tables
        let conn = open_at(&path).expect("open_at failed");
        conn.execute_batch(
            "PRAGMA foreign_keys=OFF;
             BEGIN IMMEDIATE;
             CREATE TABLE meta (key TEXT PRIMARY KEY, value TEXT NOT NULL);
             INSERT INTO meta (key, value) VALUES ('schema_version', '6');
             CREATE TABLE sessions (
                 id TEXT PRIMARY KEY, pid INTEGER NOT NULL,
                 display_name TEXT NOT NULL, started_at TEXT NOT NULL,
                 ended_at TEXT, alive INTEGER NOT NULL DEFAULT 1
             );
             CREATE TABLE messages (
                 id          INTEGER PRIMARY KEY AUTOINCREMENT,
                 session_id  TEXT NOT NULL REFERENCES sessions(id) ON DELETE CASCADE,
                 timestamp   TEXT NOT NULL,
                 type        TEXT NOT NULL,
                 method      TEXT NOT NULL,
                 server      TEXT NOT NULL,
                 client      TEXT NOT NULL,
                 request_id  INTEGER REFERENCES messages(id),
                 parent_id   INTEGER REFERENCES messages(id),
                 payload     TEXT NOT NULL,
                 created_at  TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ', 'now'))
             );
             CREATE TABLE editing_state (
                 session_id  TEXT NOT NULL,
                 agent_id    TEXT NOT NULL DEFAULT '',
                 started_at  TEXT NOT NULL,
                 PRIMARY KEY (session_id, agent_id)
             );
             CREATE TABLE editing_files (
                 session_id  TEXT NOT NULL,
                 agent_id    TEXT NOT NULL DEFAULT '',
                 file_path   TEXT NOT NULL,
                 PRIMARY KEY (session_id, agent_id, file_path)
             );
             INSERT INTO editing_state (session_id, agent_id, started_at)
             VALUES ('s1', '', '2026-01-01T00:00:00Z');
             INSERT INTO editing_files (session_id, agent_id, file_path)
             VALUES ('s1', '', '/src/main.rs');
             COMMIT;
             PRAGMA foreign_keys=ON;",
        )
        .expect("failed to create v6 schema");
        drop(conn);

        let conn = open_and_migrate_at(&path).expect("open_and_migrate_at failed");

        let version = current_schema_version(&conn).expect("failed to read schema version");
        assert_eq!(version, 9, "schema version should be 9 after migration");

        assert!(
            !table_exists(&conn, "editing_state"),
            "editing_state should be dropped after v6→v7 migration"
        );
        assert!(
            !table_exists(&conn, "editing_files"),
            "editing_files should be dropped after v6→v7 migration"
        );
    }

    #[allow(clippy::expect_used, reason = "test assertions")]
    #[test]
    fn test_migrate_v7_to_v8() {
        let dir = tempfile::tempdir().expect("failed to create tempdir");
        let path = dir.path().join("test.db");

        // Create a v7 database with old language_servers schema.
        let conn = open_at(&path).expect("open_at failed");
        conn.execute_batch(
            "PRAGMA foreign_keys=OFF;
             BEGIN IMMEDIATE;
             CREATE TABLE meta (key TEXT PRIMARY KEY, value TEXT NOT NULL);
             INSERT INTO meta (key, value) VALUES ('schema_version', '7');
             CREATE TABLE sessions (
                 id             TEXT PRIMARY KEY,
                 pid            INTEGER NOT NULL,
                 display_name   TEXT NOT NULL,
                 client_name    TEXT,
                 client_version TEXT,
                 client_session_id TEXT,
                 started_at     TEXT NOT NULL,
                 ended_at       TEXT,
                 alive          INTEGER NOT NULL DEFAULT 1
             );
             CREATE TABLE messages (
                 id          INTEGER PRIMARY KEY AUTOINCREMENT,
                 session_id  TEXT NOT NULL REFERENCES sessions(id) ON DELETE CASCADE,
                 timestamp   TEXT NOT NULL,
                 type        TEXT NOT NULL,
                 method      TEXT NOT NULL,
                 server      TEXT NOT NULL,
                 client      TEXT NOT NULL,
                 request_id  INTEGER REFERENCES messages(id),
                 parent_id   INTEGER REFERENCES messages(id),
                 payload     TEXT NOT NULL,
                 created_at  TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ', 'now'))
             );
             CREATE TABLE language_servers (
                 session_id  TEXT NOT NULL REFERENCES sessions(id) ON DELETE CASCADE,
                 name        TEXT NOT NULL,
                 state       TEXT NOT NULL,
                 PRIMARY KEY (session_id, name)
             );
             INSERT INTO sessions (id, pid, display_name, started_at)
             VALUES ('s1', 1, 'test', '2026-01-01T00:00:00Z');
             INSERT INTO language_servers (session_id, name, state)
             VALUES ('s1', 'rust', 'ready');
             COMMIT;
             PRAGMA foreign_keys=ON;",
        )
        .expect("failed to create v7 schema");
        drop(conn);

        let conn = open_and_migrate_at(&path).expect("open_and_migrate_at failed");

        let version = current_schema_version(&conn).expect("failed to read schema version");
        assert_eq!(version, 9, "schema version should be 9 after migration");

        // Old data should be gone (table was recreated).
        let count: i64 = conn
            .query_row("SELECT COUNT(*) FROM language_servers", [], |row| {
                row.get(0)
            })
            .expect("query language_servers count");
        assert_eq!(count, 0, "old rows should be gone after table recreation");

        // New schema should accept wider PK.
        conn.execute(
            "INSERT INTO language_servers \
             (session_id, language_id, server, scope_kind, scope_root, state) \
             VALUES ('s1', 'rust', 'rust-analyzer', 'workspace', '', 'ready')",
            [],
        )
        .expect("insert with new schema should succeed");

        conn.execute(
            "INSERT INTO language_servers \
             (session_id, language_id, server, scope_kind, scope_root, state) \
             VALUES ('s1', 'rust', 'rust-analyzer', 'root', '/project', 'ready')",
            [],
        )
        .expect("insert second instance with different scope should succeed");
    }

    #[allow(clippy::expect_used, reason = "test assertions")]
    #[test]
    fn test_fresh_schema_has_new_language_servers() {
        let dir = tempfile::tempdir().expect("failed to create tempdir");
        let path = dir.path().join("test.db");

        let conn = open_and_migrate_at(&path).expect("open_and_migrate_at failed");

        // Insert a session for FK.
        conn.execute(
            "INSERT INTO sessions (id, pid, display_name, started_at) \
             VALUES ('s1', 1, 'test', '2026-01-01T00:00:00Z')",
            [],
        )
        .expect("insert session");

        // Two instances of the same language with different scopes.
        conn.execute(
            "INSERT INTO language_servers \
             (session_id, language_id, server, scope_kind, scope_root, state) \
             VALUES ('s1', 'rust', 'rust-analyzer', 'workspace', '', 'ready')",
            [],
        )
        .expect("insert workspace instance");

        conn.execute(
            "INSERT INTO language_servers \
             (session_id, language_id, server, scope_kind, scope_root, state) \
             VALUES ('s1', 'rust', 'rust-analyzer', 'root', '/tmp', 'busy')",
            [],
        )
        .expect("insert root instance");

        let count: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM language_servers WHERE session_id = 's1'",
                [],
                |row| row.get(0),
            )
            .expect("query count");
        assert_eq!(count, 2, "should have two entries for same language");
    }

    /// Helper: create a v8 database with sessions and messages tables
    /// matching the pre-migration schema (FK constraints, no level column).
    #[allow(clippy::expect_used, reason = "test helper")]
    fn create_v8_db(conn: &Connection) {
        conn.execute_batch(
            "PRAGMA foreign_keys=OFF;
             BEGIN IMMEDIATE;
             CREATE TABLE meta (key TEXT PRIMARY KEY, value TEXT NOT NULL);
             INSERT INTO meta (key, value) VALUES ('schema_version', '8');
             CREATE TABLE sessions (
                 id             TEXT PRIMARY KEY,
                 pid            INTEGER NOT NULL,
                 display_name   TEXT NOT NULL,
                 client_name    TEXT,
                 client_version TEXT,
                 client_session_id TEXT,
                 started_at     TEXT NOT NULL,
                 ended_at       TEXT,
                 alive          INTEGER NOT NULL DEFAULT 1
             );
             CREATE TABLE messages (
                 id          INTEGER PRIMARY KEY AUTOINCREMENT,
                 session_id  TEXT NOT NULL REFERENCES sessions(id) ON DELETE CASCADE,
                 timestamp   TEXT NOT NULL,
                 type        TEXT NOT NULL,
                 method      TEXT NOT NULL,
                 server      TEXT NOT NULL,
                 client      TEXT NOT NULL,
                 request_id  INTEGER REFERENCES messages(id),
                 parent_id   INTEGER REFERENCES messages(id),
                 payload     TEXT NOT NULL,
                 created_at  TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ', 'now'))
             );
             CREATE INDEX idx_messages_session ON messages(session_id);
             CREATE INDEX idx_messages_type ON messages(type);
             CREATE INDEX idx_messages_request_id ON messages(request_id);
             CREATE INDEX idx_messages_parent_id ON messages(parent_id);
             INSERT INTO sessions (id, pid, display_name, started_at)
             VALUES ('s1', 1, 'test', '2026-01-01T00:00:00Z');
             COMMIT;
             PRAGMA foreign_keys=ON;",
        )
        .expect("failed to create v8 schema");
    }

    #[allow(clippy::expect_used, reason = "test assertions")]
    #[test]
    fn test_migrate_v8_to_v9_adds_level_column() {
        let dir = tempfile::tempdir().expect("failed to create tempdir");
        let path = dir.path().join("test.db");
        let conn = open_at(&path).expect("open_at failed");
        create_v8_db(&conn);

        // Insert rows with old-style types: trace events use severity as type.
        conn.execute_batch(
            "PRAGMA foreign_keys=OFF;
             INSERT INTO messages (session_id, timestamp, type, method, server, client, payload)
             VALUES ('s1', '2026-01-01T00:00:01Z', 'debug', 'internal', '', '', '{}');
             INSERT INTO messages (session_id, timestamp, type, method, server, client, payload)
             VALUES ('s1', '2026-01-01T00:00:02Z', 'warn', 'internal', '', '', '{}');
             INSERT INTO messages (session_id, timestamp, type, method, server, client, payload)
             VALUES ('s1', '2026-01-01T00:00:03Z', 'lsp', 'textDocument/hover', 'ra', 'catenary', '{}');
             PRAGMA foreign_keys=ON;",
        )
        .expect("insert test rows");
        drop(conn);

        let conn = open_and_migrate_at(&path).expect("migration failed");
        let version = current_schema_version(&conn).expect("read version");
        assert_eq!(version, 9, "schema version should be 9");

        // Trace event: type was 'debug' → type='internal', level='debug'
        let (typ, level): (String, String) = conn
            .query_row(
                "SELECT type, level FROM messages WHERE timestamp = '2026-01-01T00:00:01Z'",
                [],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .expect("query debug row");
        assert_eq!(typ, "internal");
        assert_eq!(level, "debug");

        // Trace event: type was 'warn' → type='internal', level='warn'
        let (typ, level): (String, String) = conn
            .query_row(
                "SELECT type, level FROM messages WHERE timestamp = '2026-01-01T00:00:02Z'",
                [],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .expect("query warn row");
        assert_eq!(typ, "internal");
        assert_eq!(level, "warn");

        // Protocol event: type was 'lsp' → type='lsp', level='info'
        let (typ, level): (String, String) = conn
            .query_row(
                "SELECT type, level FROM messages WHERE timestamp = '2026-01-01T00:00:03Z'",
                [],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .expect("query lsp row");
        assert_eq!(typ, "lsp");
        assert_eq!(level, "info");
    }

    #[allow(clippy::expect_used, reason = "test assertions")]
    #[test]
    fn test_migrate_v8_to_v9_drops_fk_constraints() {
        let dir = tempfile::tempdir().expect("failed to create tempdir");
        let path = dir.path().join("test.db");
        let conn = open_at(&path).expect("open_at failed");
        create_v8_db(&conn);
        drop(conn);

        let conn = open_and_migrate_at(&path).expect("migration failed");

        // Insert a row with request_id = 99999 (no matching ROWID).
        // Before the migration this would fail with FK constraint violation.
        conn.execute(
            "INSERT INTO messages \
             (session_id, timestamp, type, level, method, server, client, \
              request_id, parent_id, payload) \
             VALUES ('s1', '2026-01-01T00:00:00Z', 'lsp', 'info', \
                     'textDocument/hover', 'ra', 'catenary', 99999, 88888, '{}')",
            [],
        )
        .expect("insert with non-matching request_id should succeed after FK drop");
    }

    #[allow(clippy::expect_used, reason = "test assertions")]
    #[test]
    fn test_migrate_v8_to_v9_preserves_data() {
        let dir = tempfile::tempdir().expect("failed to create tempdir");
        let path = dir.path().join("test.db");
        let conn = open_at(&path).expect("open_at failed");
        create_v8_db(&conn);

        conn.execute_batch(
            "PRAGMA foreign_keys=OFF;
             INSERT INTO messages (session_id, timestamp, type, method, server, client, request_id, parent_id, payload)
             VALUES ('s1', '2026-01-01T00:00:01Z', 'lsp', 'textDocument/definition', 'ra', 'catenary', 42, 10, '{\"result\":null}');
             INSERT INTO messages (session_id, timestamp, type, method, server, client, payload)
             VALUES ('s1', '2026-01-01T00:00:02Z', 'error', 'spawn_failed', 'ra', '', '{\"msg\":\"boom\"}');
             PRAGMA foreign_keys=ON;",
        )
        .expect("insert test rows");
        drop(conn);

        let conn = open_and_migrate_at(&path).expect("migration failed");

        // Protocol row: data preserved, correlation IDs intact.
        let (method, server, req_id, par_id, payload): (
            String,
            String,
            Option<i64>,
            Option<i64>,
            String,
        ) = conn
            .query_row(
                "SELECT method, server, request_id, parent_id, payload \
                 FROM messages WHERE timestamp = '2026-01-01T00:00:01Z'",
                [],
                |row| {
                    Ok((
                        row.get(0)?,
                        row.get(1)?,
                        row.get(2)?,
                        row.get(3)?,
                        row.get(4)?,
                    ))
                },
            )
            .expect("query protocol row");
        assert_eq!(method, "textDocument/definition");
        assert_eq!(server, "ra");
        assert_eq!(req_id, Some(42));
        assert_eq!(par_id, Some(10));
        assert_eq!(payload, "{\"result\":null}");

        // Trace row: data preserved, type/level split correct.
        let (typ, level, method, payload): (String, String, String, String) = conn
            .query_row(
                "SELECT type, level, method, payload \
                 FROM messages WHERE timestamp = '2026-01-01T00:00:02Z'",
                [],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
            )
            .expect("query trace row");
        assert_eq!(typ, "internal");
        assert_eq!(level, "error");
        assert_eq!(method, "spawn_failed");
        assert_eq!(payload, "{\"msg\":\"boom\"}");
    }

    #[allow(clippy::expect_used, reason = "test assertions")]
    #[test]
    fn test_fresh_install_has_level_column() {
        let dir = tempfile::tempdir().expect("failed to create tempdir");
        let path = dir.path().join("test.db");
        let conn = open_and_migrate_at(&path).expect("open_and_migrate_at failed");

        // Insert a session for FK.
        conn.execute(
            "INSERT INTO sessions (id, pid, display_name, started_at) \
             VALUES ('s1', 1, 'test', '2026-01-01T00:00:00Z')",
            [],
        )
        .expect("insert session");

        // Insert a message with explicit level.
        conn.execute(
            "INSERT INTO messages \
             (session_id, timestamp, type, level, method, server, client, payload) \
             VALUES ('s1', '2026-01-01T00:00:00Z', 'lsp', 'debug', \
                     'textDocument/hover', 'ra', 'catenary', '{}')",
            [],
        )
        .expect("insert with level column should succeed");

        let level: String = conn
            .query_row(
                "SELECT level FROM messages WHERE session_id = 's1'",
                [],
                |row| row.get(0),
            )
            .expect("query level");
        assert_eq!(level, "debug");

        // Default level should be 'info'.
        conn.execute(
            "INSERT INTO messages \
             (session_id, timestamp, type, method, server, client, payload) \
             VALUES ('s1', '2026-01-01T00:00:01Z', 'mcp', \
                     'tools/call', '', 'catenary', '{}')",
            [],
        )
        .expect("insert without explicit level");

        let default_level: String = conn
            .query_row(
                "SELECT level FROM messages WHERE timestamp = '2026-01-01T00:00:01Z'",
                [],
                |row| row.get(0),
            )
            .expect("query default level");
        assert_eq!(default_level, "info");

        // request_id with non-matching ROWID should succeed (no FK).
        conn.execute(
            "INSERT INTO messages \
             (session_id, timestamp, type, method, server, client, \
              request_id, parent_id, payload) \
             VALUES ('s1', '2026-01-01T00:00:02Z', 'lsp', 'info', \
                     'textDocument/hover', 'ra', 77777, 66666, '{}')",
            [],
        )
        .expect("non-matching correlation IDs should succeed on fresh schema");
    }
}
