// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 Mark Wells <contact@markwells.dev>

//! Session observability types and queries.
//!
//! Sessions are stored in SQLite via the [`crate::db`] module. The daemon
//! writes session rows directly; this module provides the query functions
//! for `catenary list`, `catenary monitor`, and `catenary query`.

use anyhow::{Context, Result};
use chrono::{DateTime, Utc};
use rusqlite::Connection;
use serde::{Deserialize, Serialize};
use std::path::PathBuf;

/// Session metadata.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionInfo {
    /// Unique session ID.
    pub id: String,
    /// Process ID of the Catenary instance.
    pub pid: u32,
    /// Display name (comma-joined workspace roots).
    pub workspace: String,
    /// When the session started.
    pub started_at: DateTime<Utc>,
    /// Name of the connected MCP client.
    #[serde(default)]
    pub client_name: Option<String>,
    /// Version of the connected MCP client.
    #[serde(default)]
    pub client_version: Option<String>,
    /// Session ID from the host CLI (Claude Code / Gemini CLI UUID).
    #[serde(default)]
    pub client_session_id: Option<String>,
}

/// A protocol message row from the `messages` table.
///
/// All envelope fields plus the raw protocol payload.
#[derive(Debug, Clone)]
pub struct SessionMessage {
    /// Unique message ID (autoincrement primary key).
    pub id: i64,
    /// Protocol boundary: `mcp`, `lsp`, or `hook`.
    pub r#type: String,
    /// Tracing severity: `debug`, `info`, `warn`, or `error`.
    pub level: String,
    /// Protocol method (e.g., `textDocument/hover`, `tools/call`).
    pub method: String,
    /// Server endpoint name.
    pub server: String,
    /// Client endpoint name.
    pub client: String,
    /// In-process correlation ID ([`crate::logging::CorrelationId`]).
    /// Request and response share the same value; pair merge matches
    /// adjacent messages with equal non-`None` `request_id`. Not a
    /// foreign key into this table's `id` column.
    pub request_id: Option<i64>,
    /// Causation link. References the `request_id` of the message that
    /// caused this one (e.g., an LSP request's `parent_id` is the MCP
    /// tool call's `request_id`). Not a foreign key into `id`.
    pub parent_id: Option<i64>,
    /// When the message was logged.
    pub timestamp: DateTime<Utc>,
    /// Raw protocol JSON, untouched.
    pub payload: serde_json::Value,
}

/// Returns the base directory for session runtime artifacts.
///
/// Used by garbage collection and TUI for cleaning up socket directories
/// left by previous (pre-daemon) Catenary versions.
#[must_use]
pub fn sessions_dir() -> PathBuf {
    crate::db::state_dir().join("catenary").join("sessions")
}

// ── Message tailing (SQLite-backed) ──────────────────────────────────

/// Polls the `messages` table for new rows since the last read.
pub struct SqliteMessageTail {
    conn: Connection,
    session_id: String,
    last_id: i64,
    include_debug: bool,
}

impl SqliteMessageTail {
    /// Read the next message if available.
    ///
    /// # Errors
    ///
    /// Returns an error if reading from the database fails.
    pub fn try_next_message(&mut self) -> Result<Option<SessionMessage>> {
        let query = if self.include_debug {
            "SELECT id, timestamp, type, level, method, server, client, \
             request_id, parent_id, payload FROM messages \
             WHERE session_id = ?1 AND id > ?2 ORDER BY id LIMIT 1"
        } else {
            "SELECT id, timestamp, type, level, method, server, client, \
             request_id, parent_id, payload FROM messages \
             WHERE session_id = ?1 AND id > ?2 AND level != 'debug' \
             ORDER BY id LIMIT 1"
        };

        let result = self.conn.query_row(
            query,
            rusqlite::params![&self.session_id, self.last_id],
            |row| {
                let id: i64 = row.get(0)?;
                let ts: String = row.get(1)?;
                let r#type: String = row.get(2)?;
                let level: String = row.get(3)?;
                let method: String = row.get(4)?;
                let server: String = row.get(5)?;
                let client: String = row.get(6)?;
                let request_id: Option<i64> = row.get(7)?;
                let parent_id: Option<i64> = row.get(8)?;
                let payload: String = row.get(9)?;
                Ok((
                    id, ts, r#type, level, method, server, client, request_id, parent_id, payload,
                ))
            },
        );

        match result {
            Ok((id, ts, r#type, level, method, server, client, request_id, parent_id, payload)) => {
                self.last_id = id;
                let timestamp = DateTime::parse_from_rfc3339(&ts)
                    .with_context(|| format!("invalid message timestamp: {ts}"))?
                    .with_timezone(&Utc);
                let payload: serde_json::Value =
                    serde_json::from_str(&payload).context("invalid message payload")?;
                Ok(Some(SessionMessage {
                    id,
                    r#type,
                    level,
                    method,
                    server,
                    client,
                    request_id,
                    parent_id,
                    timestamp,
                    payload,
                }))
            }
            Err(rusqlite::Error::QueryReturnedNoRows) => {
                // Check if GC deleted rows past our high-water mark.
                let max_id: Option<i64> = self
                    .conn
                    .query_row(
                        "SELECT MAX(id) FROM messages WHERE session_id = ?1",
                        [&self.session_id],
                        |row| row.get(0),
                    )
                    .ok()
                    .flatten();

                if let Some(max) = max_id
                    && max < self.last_id
                {
                    self.last_id = 0;
                }

                Ok(None)
            }
            Err(e) => Err(e.into()),
        }
    }
}

// ── Free functions ───────────────────────────────────────────────────

/// Raw row from the sessions table (avoids complex tuple types).
struct SessionRow {
    id: String,
    pid: u32,
    display_name: String,
    client_name: Option<String>,
    client_version: Option<String>,
    client_session_id: Option<String>,
    started_at_str: String,
    db_alive: bool,
}

/// List all sessions (active and inactive).
///
/// Opens a database connection internally. For explicit connection
/// management, use [`list_sessions_with_conn`].
///
/// # Errors
///
/// Returns an error if the database cannot be opened or queried.
pub fn list_sessions() -> Result<Vec<(SessionInfo, bool)>> {
    let conn = crate::db::open_and_migrate()?;
    list_sessions_with_conn(&conn)
}

/// List all sessions using an existing database connection.
///
/// Returns a list of sessions and their status (true = active, false = dead).
/// Crashed sessions (PID gone but `alive` flag set) are marked dead in the DB.
///
/// # Errors
///
/// Returns an error if the database cannot be queried.
pub fn list_sessions_with_conn(conn: &Connection) -> Result<Vec<(SessionInfo, bool)>> {
    // Collect raw rows first to release the statement borrow.
    let rows = {
        let mut stmt = conn.prepare(
            "SELECT id, pid, display_name, client_name, client_version, \
             client_session_id, started_at, alive \
             FROM sessions ORDER BY started_at DESC",
        )?;
        let mut r = stmt.query([])?;
        let mut rows = Vec::new();
        while let Some(row) = r.next()? {
            rows.push(SessionRow {
                id: row.get(0)?,
                pid: row.get(1)?,
                display_name: row.get(2)?,
                client_name: row.get(3)?,
                client_version: row.get(4)?,
                client_session_id: row.get(5)?,
                started_at_str: row.get(6)?,
                db_alive: row.get(7)?,
            });
        }
        rows
    };

    let mut sessions = Vec::with_capacity(rows.len());
    for r in rows {
        let SessionRow {
            id,
            pid,
            display_name,
            client_name,
            client_version,
            client_session_id,
            started_at_str,
            db_alive,
        } = r;
        let started_at = DateTime::parse_from_rfc3339(&started_at_str)
            .with_context(|| format!("invalid started_at: {started_at_str}"))?
            .with_timezone(&Utc);

        let alive = if db_alive {
            if is_process_alive(pid) {
                true
            } else {
                // Process crashed — mark dead in DB.
                let _ = conn.execute(
                    "UPDATE sessions SET alive = 0, ended_at = ?1 WHERE id = ?2",
                    rusqlite::params![Utc::now().to_rfc3339(), &id],
                );
                false
            }
        } else {
            false
        };

        sessions.push((
            SessionInfo {
                id,
                pid,
                workspace: display_name,
                started_at,
                client_name,
                client_version,
                client_session_id,
            },
            alive,
        ));
    }

    Ok(sessions)
}

/// Get a specific session by ID.
///
/// Opens a database connection internally. For explicit connection
/// management, use [`get_session_with_conn`].
///
/// # Errors
///
/// Returns an error if the database cannot be opened or queried.
pub fn get_session(id: &str) -> Result<Option<(SessionInfo, bool)>> {
    let conn = crate::db::open_and_migrate()?;
    get_session_with_conn(&conn, id)
}

/// Get a specific session by ID using an existing database connection.
///
/// Returns the session info and its status (true = active, false = dead).
///
/// # Errors
///
/// Returns an error if the database cannot be queried.
pub fn get_session_with_conn(conn: &Connection, id: &str) -> Result<Option<(SessionInfo, bool)>> {
    let result = conn.query_row(
        "SELECT id, pid, display_name, client_name, client_version, \
         client_session_id, started_at, alive \
         FROM sessions WHERE id = ?1",
        [id],
        |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, u32>(1)?,
                row.get::<_, String>(2)?,
                row.get::<_, Option<String>>(3)?,
                row.get::<_, Option<String>>(4)?,
                row.get::<_, Option<String>>(5)?,
                row.get::<_, String>(6)?,
                row.get::<_, bool>(7)?,
            ))
        },
    );

    match result {
        Ok((
            sid,
            pid,
            display_name,
            client_name,
            client_version,
            client_session_id,
            started_at_str,
            db_alive,
        )) => {
            let started_at = DateTime::parse_from_rfc3339(&started_at_str)
                .with_context(|| format!("invalid started_at: {started_at_str}"))?
                .with_timezone(&Utc);

            let alive = if db_alive {
                if is_process_alive(pid) {
                    true
                } else {
                    let _ = conn.execute(
                        "UPDATE sessions SET alive = 0, ended_at = ?1 WHERE id = ?2",
                        rusqlite::params![Utc::now().to_rfc3339(), &sid],
                    );
                    false
                }
            } else {
                false
            };

            Ok(Some((
                SessionInfo {
                    id: sid,
                    pid,
                    workspace: display_name,
                    started_at,
                    client_name,
                    client_version,
                    client_session_id,
                },
                alive,
            )))
        }
        Err(rusqlite::Error::QueryReturnedNoRows) => Ok(None),
        Err(e) => Err(e.into()),
    }
}

/// Load all messages for a session, ordered by id.
///
/// When `include_debug` is false, messages with `level = 'debug'` are
/// excluded from the result set.
///
/// # Errors
///
/// Returns an error if the database cannot be queried.
pub fn monitor_messages_with_conn(
    conn: &Connection,
    session_id: &str,
    include_debug: bool,
) -> Result<Vec<SessionMessage>> {
    let query = if include_debug {
        "SELECT id, timestamp, type, level, method, server, client, \
         request_id, parent_id, payload FROM messages \
         WHERE session_id = ?1 ORDER BY id"
    } else {
        "SELECT id, timestamp, type, level, method, server, client, \
         request_id, parent_id, payload FROM messages \
         WHERE session_id = ?1 AND level != 'debug' ORDER BY id"
    };
    let mut stmt = conn.prepare(query)?;
    let mut rows = stmt.query([session_id])?;
    let mut messages = Vec::new();

    while let Some(row) = rows.next()? {
        let id: i64 = row.get(0)?;
        let ts: String = row.get(1)?;
        let r#type: String = row.get(2)?;
        let level: String = row.get(3)?;
        let method: String = row.get(4)?;
        let server: String = row.get(5)?;
        let client: String = row.get(6)?;
        let request_id: Option<i64> = row.get(7)?;
        let parent_id: Option<i64> = row.get(8)?;
        let payload_str: String = row.get(9)?;

        if let Ok(timestamp) = DateTime::parse_from_rfc3339(&ts)
            && let Ok(payload) = serde_json::from_str::<serde_json::Value>(&payload_str)
        {
            messages.push(SessionMessage {
                id,
                r#type,
                level,
                method,
                server,
                client,
                request_id,
                parent_id,
                timestamp: timestamp.with_timezone(&Utc),
                payload,
            });
        }
    }

    Ok(messages)
}

/// Tail only *new* messages from a session (starts from current end).
///
/// Opens a database connection internally. For explicit connection
/// management, use [`tail_messages_new_with_conn`].
///
/// # Errors
///
/// Returns an error if the database cannot be opened or queried.
pub fn tail_messages_new(id: &str, include_debug: bool) -> Result<SqliteMessageTail> {
    let conn = crate::db::open()?;
    tail_messages_new_with_conn(conn, id, include_debug)
}

/// Tail only *new* messages from a session using an existing database connection.
///
/// The connection is moved into the returned [`SqliteMessageTail`] for polling.
///
/// # Errors
///
/// Returns an error if the database cannot be queried.
pub fn tail_messages_new_with_conn(
    conn: Connection,
    id: &str,
    include_debug: bool,
) -> Result<SqliteMessageTail> {
    let last_id: i64 = conn
        .query_row(
            "SELECT COALESCE(MAX(id), 0) FROM messages WHERE session_id = ?1",
            [id],
            |row| row.get(0),
        )
        .unwrap_or(0);

    Ok(SqliteMessageTail {
        conn,
        session_id: id.to_string(),
        last_id,
        include_debug,
    })
}

/// Get active languages for a session by reading its events.
///
/// Opens a database connection internally. For explicit connection
/// management, use [`active_languages_with_conn`].
///
/// # Errors
///
/// Returns an error if the database cannot be opened or queried.
pub fn active_languages(id: &str) -> Result<Vec<String>> {
    let conn = crate::db::open_and_migrate()?;
    active_languages_with_conn(&conn, id)
}

/// Get active languages for a session using an existing database connection.
///
/// Returns the set of LSP server names that have communicated during
/// the session, derived from the `messages` table.
///
/// # Errors
///
/// Returns an error if the database cannot be queried.
pub fn active_languages_with_conn(conn: &Connection, id: &str) -> Result<Vec<String>> {
    let mut stmt = conn.prepare(
        "SELECT DISTINCT server FROM messages \
         WHERE session_id = ?1 AND type = 'lsp' \
         ORDER BY server",
    )?;
    let mut rows = stmt.query([id])?;
    let mut languages = Vec::new();

    while let Some(row) = rows.next()? {
        languages.push(row.get(0)?);
    }

    Ok(languages)
}

/// Remove dead sessions older than the configured retention period.
///
/// Opens a database connection internally. For explicit connection
/// management, use [`prune_sessions_with_conn`].
///
/// # Errors
///
/// Returns an error if the database cannot be opened or queried.
pub fn prune_sessions(retention_days: i64) -> Result<usize> {
    if retention_days < 0 {
        return Ok(0);
    }
    let conn = crate::db::open_and_migrate()?;
    prune_sessions_with_conn(&conn, retention_days)
}

/// Remove dead sessions older than the configured retention period
/// using an existing database connection.
///
/// - `retention_days == -1`: retain forever (no-op).
/// - `retention_days == 0`: remove all dead sessions regardless of age.
/// - `retention_days > 0`: remove dead sessions whose `started_at` is older
///   than `retention_days` days ago.
///
/// Active sessions are never pruned. Crashed sessions (PID gone) are
/// detected and marked dead before pruning.
///
/// # Errors
///
/// Returns an error if the database cannot be queried.
pub fn prune_sessions_with_conn(conn: &Connection, retention_days: i64) -> Result<usize> {
    if retention_days < 0 {
        return Ok(0);
    }

    // Detect crashed sessions (alive in DB but PID gone).
    let crashed: Vec<String> = {
        let mut stmt = conn.prepare("SELECT id, pid FROM sessions WHERE alive = 1")?;
        let mut rows = stmt.query([])?;
        let mut ids = Vec::new();
        while let Some(row) = rows.next()? {
            let id: String = row.get(0)?;
            let pid: u32 = row.get(1)?;
            if !is_process_alive(pid) {
                ids.push(id);
            }
        }
        ids
    };

    let ended_at = Utc::now().to_rfc3339();
    for id in &crashed {
        let _ = conn.execute(
            "UPDATE sessions SET alive = 0, ended_at = ?1 WHERE id = ?2",
            rusqlite::params![&ended_at, id],
        );
    }

    let cutoff = if retention_days == 0 {
        // Remove all dead sessions — use a far-future cutoff.
        Utc::now() + chrono::Duration::days(1)
    } else {
        Utc::now() - chrono::Duration::days(retention_days)
    };

    let removed = conn.execute(
        "DELETE FROM sessions WHERE alive = 0 AND started_at < ?1",
        rusqlite::params![cutoff.to_rfc3339()],
    )?;

    Ok(removed)
}

/// Delete a session and all its associated data.
///
/// Opens a database connection internally. For explicit connection
/// management, use [`delete_session_data_with_conn`].
///
/// # Errors
///
/// Returns an error if the database cannot be opened or the delete fails.
pub fn delete_session_data(id: &str) -> Result<()> {
    let conn = crate::db::open_and_migrate()?;
    delete_session_data_with_conn(&conn, id)
}

/// Delete a session and all its associated data using an existing database
/// connection.
///
/// # Errors
///
/// Returns an error if the delete fails.
pub fn delete_session_data_with_conn(conn: &Connection, id: &str) -> Result<()> {
    conn.execute("DELETE FROM sessions WHERE id = ?1", [id])?;

    // Clean up socket directory if it exists.
    let socket_dir = sessions_dir().join(id);
    let _ = std::fs::remove_dir_all(&socket_dir);

    Ok(())
}

// ── Private helpers ──────────────────────────────────────────────────

/// Check if a process is still running.
#[must_use]
pub fn is_process_alive(pid: u32) -> bool {
    #[cfg(target_os = "linux")]
    {
        // On Linux, checking /proc/<pid> is safe and doesn't require unsafe blocks.
        std::path::Path::new("/proc").join(pid.to_string()).exists()
    }

    #[cfg(all(unix, not(target_os = "linux")))]
    {
        // On other Unix systems, we use the kill command with signal 0.
        // This is safe but slightly slower than a syscall.
        std::process::Command::new("kill")
            .arg("-0")
            .arg(pid.to_string())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .map(|s| s.success())
            .unwrap_or(false)
    }

    #[cfg(not(unix))]
    {
        // On non-Unix, assume alive (could use platform-specific APIs).
        let _ = pid;
        true
    }
}

// ── Test helpers (shared across crate) ──────────────────────────────

/// Shared [`SessionMessage`] constructors for tests.
///
/// Centralizes struct construction so adding new fields is a one-line
/// change instead of touching every test file.
#[cfg(test)]
pub(crate) mod test_support {
    use super::SessionMessage;
    use chrono::Utc;

    /// Build a `SessionMessage` with sensible defaults.
    ///
    /// `level` defaults to `"info"`, `request_id`/`parent_id` to `None`,
    /// `client` to `"catenary"`, `payload` to `{}`.
    #[must_use]
    pub fn message(r#type: &str, method: &str, server: &str) -> SessionMessage {
        SessionMessage {
            id: 0,
            r#type: r#type.to_string(),
            level: "info".to_string(),
            method: method.to_string(),
            server: server.to_string(),
            client: "catenary".to_string(),
            request_id: None,
            parent_id: None,
            timestamp: Utc::now(),
            payload: serde_json::json!({}),
        }
    }

    /// Build a `SessionMessage` with a specific payload.
    #[must_use]
    pub fn message_with_payload(
        r#type: &str,
        method: &str,
        server: &str,
        payload: serde_json::Value,
    ) -> SessionMessage {
        SessionMessage {
            payload,
            ..message(r#type, method, server)
        }
    }

    /// Build a `SessionMessage` with explicit `id`, `request_id`, and `parent_id`.
    #[must_use]
    pub fn message_with_ids(
        id: i64,
        r#type: &str,
        method: &str,
        server: &str,
        request_id: Option<i64>,
        parent_id: Option<i64>,
    ) -> SessionMessage {
        SessionMessage {
            id,
            request_id,
            parent_id,
            ..message(r#type, method, server)
        }
    }

    /// Build a `SessionMessage` with explicit `id`, `request_id`, `parent_id`, and payload.
    #[must_use]
    pub fn message_with_ids_payload(
        id: i64,
        r#type: &str,
        method: &str,
        server: &str,
        request_id: Option<i64>,
        parent_id: Option<i64>,
        payload: serde_json::Value,
    ) -> SessionMessage {
        SessionMessage {
            id,
            request_id,
            parent_id,
            payload,
            ..message(r#type, method, server)
        }
    }
}

#[cfg(test)]
#[allow(
    clippy::expect_used,
    reason = "tests use expect for readable assertions"
)]
mod tests {
    use super::*;
    use anyhow::Result;

    /// Open an isolated test database in a tempdir.
    /// Returns `(TempDir, PathBuf, Connection)` — the tempdir guard must
    /// be held for the lifetime of the connection.
    fn test_db() -> (tempfile::TempDir, PathBuf, Connection) {
        let dir = tempfile::tempdir().expect("failed to create tempdir for test DB");
        let path = dir.path().join("catenary").join("catenary.db");
        let conn = crate::db::open_and_migrate_at(&path).expect("failed to open test DB");
        (dir, path, conn)
    }

    /// Insert a test session directly into the database.
    ///
    /// Uses the current PID so `is_process_alive` returns `true`.
    fn insert_alive_session(conn: &Connection, id: &str, workspace: &str) {
        conn.execute(
            "INSERT INTO sessions (id, pid, display_name, started_at, alive) \
             VALUES (?1, ?2, ?3, ?4, 1)",
            rusqlite::params![id, std::process::id(), workspace, "2026-01-01T00:00:00Z"],
        )
        .expect("insert test session");
    }

    /// Insert a test message row directly into the `messages` table.
    ///
    /// Returns the inserted ROWID.
    #[allow(clippy::too_many_arguments, reason = "test helper mirrors schema")]
    fn insert_test_message(
        conn: &Connection,
        session_id: &str,
        r#type: &str,
        method: &str,
        server: &str,
        client: &str,
        request_id: Option<i64>,
        parent_id: Option<i64>,
        payload: &str,
    ) -> i64 {
        conn.execute(
            "INSERT INTO messages \
             (session_id, timestamp, type, method, server, client, \
              request_id, parent_id, payload) \
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
            rusqlite::params![
                session_id,
                "2026-01-01T00:00:00.000Z",
                r#type,
                method,
                server,
                client,
                request_id,
                parent_id,
                payload,
            ],
        )
        .expect("insert test message");
        conn.last_insert_rowid()
    }

    /// Insert a test message with an explicit `level` column.
    fn insert_test_message_with_level(
        conn: &Connection,
        session_id: &str,
        r#type: &str,
        level: &str,
        method: &str,
        server: &str,
        payload: &str,
    ) -> i64 {
        conn.execute(
            "INSERT INTO messages \
             (session_id, timestamp, type, level, method, server, client, \
              request_id, parent_id, payload) \
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, 'catenary', NULL, NULL, ?7)",
            rusqlite::params![
                session_id,
                "2026-01-01T00:00:00.000Z",
                r#type,
                level,
                method,
                server,
                payload,
            ],
        )
        .expect("insert test message with level");
        conn.last_insert_rowid()
    }

    #[test]
    fn test_session_list_and_get() -> Result<()> {
        let (_dir, _path, conn) = test_db();
        insert_alive_session(&conn, "s-test", "/tmp/test-workspace");

        // Should appear in list
        let sessions = list_sessions_with_conn(&conn)?;
        assert!(sessions.iter().any(|(s, _)| s.id == "s-test"));

        // Should be retrievable
        let found = get_session_with_conn(&conn, "s-test")?;
        let (found_session, alive) = found.expect("session should be retrievable");
        assert_eq!(found_session.workspace, "/tmp/test-workspace");
        assert!(alive);

        // Mark dead and verify
        conn.execute(
            "UPDATE sessions SET alive = 0, ended_at = ?1 WHERE id = ?2",
            rusqlite::params![Utc::now().to_rfc3339(), "s-test"],
        )?;
        let found = get_session_with_conn(&conn, "s-test")?;
        let (_, alive) = found.expect("session should exist after marking dead");
        assert!(!alive);

        // Clean up
        delete_session_data_with_conn(&conn, "s-test")?;

        Ok(())
    }

    #[test]
    fn test_active_languages_empty() -> Result<()> {
        let (_dir, _path, conn) = test_db();

        conn.execute(
            "INSERT INTO sessions (id, pid, display_name, started_at) \
                 VALUES ('s1', 1, 'test', '2026-01-01T00:00:00Z')",
            [],
        )?;

        let langs = active_languages_with_conn(&conn, "s1")?;
        assert!(langs.is_empty());

        Ok(())
    }

    #[test]
    fn test_active_languages_single_server() -> Result<()> {
        let (_dir, _path, conn) = test_db();

        conn.execute(
            "INSERT INTO sessions (id, pid, display_name, started_at) \
                 VALUES ('s1', 1, 'test', '2026-01-01T00:00:00Z')",
            [],
        )?;
        insert_test_message(
            &conn,
            "s1",
            "lsp",
            "textDocument/hover",
            "rust-analyzer",
            "catenary",
            None,
            None,
            "{}",
        );

        let langs = active_languages_with_conn(&conn, "s1")?;
        assert_eq!(langs, vec!["rust-analyzer"]);

        Ok(())
    }

    #[test]
    fn test_active_languages_excludes_non_lsp() -> Result<()> {
        let (_dir, _path, conn) = test_db();

        conn.execute(
            "INSERT INTO sessions (id, pid, display_name, started_at) \
                 VALUES ('s1', 1, 'test', '2026-01-01T00:00:00Z')",
            [],
        )?;

        // MCP and hook messages should not appear.
        insert_test_message(
            &conn,
            "s1",
            "mcp",
            "tools/call",
            "catenary",
            "claude-code",
            None,
            None,
            "{}",
        );
        insert_test_message(
            &conn,
            "s1",
            "hook",
            "post-tool",
            "catenary",
            "claude-code",
            None,
            None,
            "{}",
        );

        let langs = active_languages_with_conn(&conn, "s1")?;
        assert!(langs.is_empty());

        Ok(())
    }

    #[test]
    fn test_active_languages_multiple_servers() -> Result<()> {
        let (_dir, _path, conn) = test_db();

        conn.execute(
            "INSERT INTO sessions (id, pid, display_name, started_at) \
                 VALUES ('s1', 1, 'test', '2026-01-01T00:00:00Z')",
            [],
        )?;

        insert_test_message(
            &conn,
            "s1",
            "lsp",
            "initialize",
            "rust-analyzer",
            "catenary",
            None,
            None,
            "{}",
        );
        insert_test_message(
            &conn,
            "s1",
            "lsp",
            "initialize",
            "pyright",
            "catenary",
            None,
            None,
            "{}",
        );
        insert_test_message(
            &conn,
            "s1",
            "lsp",
            "initialize",
            "typescript-language-server",
            "catenary",
            None,
            None,
            "{}",
        );
        // Duplicate — should not produce a second entry.
        insert_test_message(
            &conn,
            "s1",
            "lsp",
            "textDocument/hover",
            "rust-analyzer",
            "catenary",
            None,
            None,
            "{}",
        );

        let langs = active_languages_with_conn(&conn, "s1")?;
        assert_eq!(
            langs,
            vec!["pyright", "rust-analyzer", "typescript-language-server"]
        );

        Ok(())
    }

    /// Insert a dead session, optionally backdated.
    fn insert_dead_session(
        conn: &Connection,
        id: &str,
        workspace: &str,
        backdate_days: Option<i64>,
    ) {
        let started_at =
            backdate_days.map_or_else(Utc::now, |days| Utc::now() - chrono::Duration::days(days));
        conn.execute(
            "INSERT INTO sessions (id, pid, display_name, started_at, alive, ended_at) \
             VALUES (?1, ?2, ?3, ?4, 0, ?5)",
            rusqlite::params![
                id,
                0_u32, // PID 0 — not alive
                workspace,
                started_at.to_rfc3339(),
                Utc::now().to_rfc3339(),
            ],
        )
        .expect("insert dead session");
    }

    /// Single sequential test covering all `prune_sessions` behaviours.
    ///
    /// These must run in sequence because `prune_sessions` operates on the
    /// shared database and parallel execution causes interference.
    #[test]
    fn test_prune_sessions() -> Result<()> {
        let (_dir, _path, conn) = test_db();
        // -- retention=-1 retains forever --
        insert_dead_session(&conn, "prune-forever", "/tmp/prune-forever", Some(365));
        let removed = prune_sessions_with_conn(&conn, -1)?;
        assert_eq!(removed, 0, "retention=-1 should never prune");
        assert!(
            get_session_with_conn(&conn, "prune-forever")?.is_some(),
            "session should still exist"
        );
        delete_session_data_with_conn(&conn, "prune-forever")?;

        // -- retention=7 keeps recent, removes old --
        insert_dead_session(&conn, "prune-recent", "/tmp/prune-recent", None);
        insert_dead_session(&conn, "prune-old", "/tmp/prune-old", Some(10));

        let _ = prune_sessions_with_conn(&conn, 7)?;
        assert!(
            get_session_with_conn(&conn, "prune-recent")?.is_some(),
            "recent dead session should survive prune"
        );
        assert!(
            get_session_with_conn(&conn, "prune-old")?.is_none(),
            "old dead session should be pruned"
        );
        delete_session_data_with_conn(&conn, "prune-recent")?;

        // -- retention=0 removes all dead --
        insert_dead_session(&conn, "prune-zero", "/tmp/prune-zero", None);
        let _ = prune_sessions_with_conn(&conn, 0)?;
        assert!(
            get_session_with_conn(&conn, "prune-zero")?.is_none(),
            "dead session should be removed with retention=0"
        );

        Ok(())
    }

    // ── Message query tests ─────────────────────────────────────────

    #[test]
    fn test_monitor_messages_with_conn() -> Result<()> {
        let (_dir, _path, conn) = test_db();

        conn.execute(
            "INSERT INTO sessions (id, pid, display_name, started_at) \
                 VALUES ('s1', 1, 'test', '2026-01-01T00:00:00Z')",
            [],
        )?;

        insert_test_message(
            &conn,
            "s1",
            "lsp",
            "textDocument/hover",
            "rust-analyzer",
            "catenary",
            None,
            None,
            r#"{"method":"textDocument/hover"}"#,
        );
        insert_test_message(
            &conn,
            "s1",
            "mcp",
            "tools/call",
            "catenary",
            "claude-code",
            None,
            None,
            r#"{"name":"grep"}"#,
        );
        insert_test_message(
            &conn,
            "s1",
            "lsp",
            "textDocument/definition",
            "typescript-language-server",
            "catenary",
            None,
            None,
            r#"{"method":"textDocument/hover"}"#,
        );

        let messages = monitor_messages_with_conn(&conn, "s1", true)?;

        assert_eq!(messages.len(), 3);
        assert_eq!(messages[0].r#type, "lsp");
        assert_eq!(messages[0].method, "textDocument/hover");
        assert_eq!(messages[0].server, "rust-analyzer");
        assert_eq!(messages[1].r#type, "mcp");
        assert_eq!(messages[1].method, "tools/call");
        assert_eq!(messages[2].server, "typescript-language-server");

        Ok(())
    }

    #[test]
    fn test_message_tail_streams() -> Result<()> {
        let (_dir, path, conn) = test_db();

        conn.execute(
            "INSERT INTO sessions (id, pid, display_name, started_at) \
                 VALUES ('s1', 1, 'test', '2026-01-01T00:00:00Z')",
            [],
        )?;

        // Insert one message before opening the tail.
        insert_test_message(
            &conn,
            "s1",
            "lsp",
            "initialize",
            "rust-analyzer",
            "catenary",
            None,
            None,
            "{}",
        );

        // Open tail — should start from current end.
        let tail_conn = crate::db::open_at(&path)?;
        let mut tail = tail_messages_new_with_conn(tail_conn, "s1", true)?;

        // Nothing new yet.
        assert!(
            tail.try_next_message()?.is_none(),
            "should have no messages initially"
        );

        // Insert a new message.
        insert_test_message(
            &conn,
            "s1",
            "lsp",
            "textDocument/hover",
            "rust-analyzer",
            "catenary",
            None,
            None,
            r#"{"result":null}"#,
        );

        let msg = tail.try_next_message()?;
        assert!(msg.is_some(), "should see newly inserted message");
        let msg = msg.expect("verified Some above");
        assert_eq!(msg.method, "textDocument/hover");

        // No more messages.
        assert!(tail.try_next_message()?.is_none());

        Ok(())
    }

    #[test]
    fn test_active_languages_from_messages() -> Result<()> {
        let (_dir, _path, conn) = test_db();

        conn.execute(
            "INSERT INTO sessions (id, pid, display_name, started_at) \
                 VALUES ('s1', 1, 'test', '2026-01-01T00:00:00Z')",
            [],
        )?;

        insert_test_message(
            &conn,
            "s1",
            "lsp",
            "textDocument/hover",
            "rust-analyzer",
            "catenary",
            None,
            None,
            "{}",
        );
        insert_test_message(
            &conn,
            "s1",
            "lsp",
            "textDocument/definition",
            "typescript-language-server",
            "catenary",
            None,
            None,
            "{}",
        );
        // MCP message should not appear in active languages.
        insert_test_message(
            &conn,
            "s1",
            "mcp",
            "tools/call",
            "catenary",
            "claude-code",
            None,
            None,
            "{}",
        );

        let langs = active_languages_with_conn(&conn, "s1")?;

        assert_eq!(langs, vec!["rust-analyzer", "typescript-language-server"]);

        Ok(())
    }

    // ── Level filtering tests ──────────────────────────────────────────

    #[test]
    fn default_threshold_excludes_debug() -> Result<()> {
        let (_dir, _path, conn) = test_db();

        conn.execute(
            "INSERT INTO sessions (id, pid, display_name, started_at) \
                 VALUES ('s1', 1, 'test', '2026-01-01T00:00:00Z')",
            [],
        )?;

        insert_test_message_with_level(
            &conn,
            "s1",
            "lsp",
            "info",
            "textDocument/hover",
            "ra",
            "{}",
        );
        insert_test_message_with_level(
            &conn,
            "s1",
            "lsp",
            "debug",
            "textDocument/didOpen",
            "ra",
            "{}",
        );
        insert_test_message_with_level(&conn, "s1", "lsp", "warn", "window/logMessage", "ra", "{}");

        let messages = monitor_messages_with_conn(&conn, "s1", false)?;
        assert_eq!(messages.len(), 2, "debug messages should be excluded");
        assert_eq!(messages[0].method, "textDocument/hover");
        assert_eq!(messages[1].method, "window/logMessage");

        Ok(())
    }

    #[test]
    fn debug_threshold_includes_all() -> Result<()> {
        let (_dir, _path, conn) = test_db();

        conn.execute(
            "INSERT INTO sessions (id, pid, display_name, started_at) \
                 VALUES ('s1', 1, 'test', '2026-01-01T00:00:00Z')",
            [],
        )?;

        insert_test_message_with_level(
            &conn,
            "s1",
            "lsp",
            "info",
            "textDocument/hover",
            "ra",
            "{}",
        );
        insert_test_message_with_level(
            &conn,
            "s1",
            "lsp",
            "debug",
            "textDocument/didOpen",
            "ra",
            "{}",
        );
        insert_test_message_with_level(&conn, "s1", "lsp", "warn", "window/logMessage", "ra", "{}");

        let messages = monitor_messages_with_conn(&conn, "s1", true)?;
        assert_eq!(messages.len(), 3, "all levels should be included");

        Ok(())
    }

    #[test]
    fn tail_respects_threshold() -> Result<()> {
        let (_dir, path, conn) = test_db();

        conn.execute(
            "INSERT INTO sessions (id, pid, display_name, started_at) \
                 VALUES ('s1', 1, 'test', '2026-01-01T00:00:00Z')",
            [],
        )?;

        // Open tail with Info threshold (exclude debug).
        let tail_conn = crate::db::open_at(&path)?;
        let mut tail = tail_messages_new_with_conn(tail_conn, "s1", false)?;

        // Insert a debug message — should be skipped.
        insert_test_message_with_level(
            &conn,
            "s1",
            "lsp",
            "debug",
            "textDocument/didOpen",
            "ra",
            "{}",
        );
        assert!(
            tail.try_next_message()?.is_none(),
            "debug messages should be skipped with Info threshold"
        );

        // Insert an info message — should appear.
        insert_test_message_with_level(
            &conn,
            "s1",
            "lsp",
            "info",
            "textDocument/hover",
            "ra",
            "{}",
        );
        let msg = tail.try_next_message()?;
        assert!(msg.is_some(), "info messages should pass Info threshold");
        assert_eq!(msg.expect("verified Some").method, "textDocument/hover");

        Ok(())
    }

    #[test]
    fn level_field_round_trips() -> Result<()> {
        let (_dir, _path, conn) = test_db();

        conn.execute(
            "INSERT INTO sessions (id, pid, display_name, started_at) \
                 VALUES ('s1', 1, 'test', '2026-01-01T00:00:00Z')",
            [],
        )?;

        insert_test_message_with_level(
            &conn,
            "s1",
            "lsp",
            "debug",
            "textDocument/hover",
            "ra",
            "{}",
        );

        let messages = monitor_messages_with_conn(&conn, "s1", true)?;
        assert_eq!(messages.len(), 1);
        assert_eq!(messages[0].level, "debug");

        Ok(())
    }
}
