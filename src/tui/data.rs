// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 Mark Wells <contact@markwells.dev>

//! Data abstraction layer for the TUI.
//!
//! [`SqliteDataSource`] reads from the database (production).
//! [`MockDataSource`] returns pre-configured data (testing).

use std::collections::HashMap;
use std::collections::VecDeque;

use anyhow::{Context, Result};
use chrono::{DateTime, Utc};

use crate::session::{self, SessionInfo, SessionMessage, SqliteAllMessageTail, SqliteMessageTail};

/// Collected session row: info, liveness, and active language servers.
pub struct SessionRow {
    /// Session metadata.
    pub info: SessionInfo,
    /// Whether the session process is still alive.
    pub alive: bool,
    /// Active language server IDs for this session.
    pub languages: Vec<String>,
}

/// Abstraction over session data access.
///
/// [`SqliteDataSource`] reads from the database (production).
/// [`MockDataSource`] returns pre-configured data (testing).
pub trait DataSource {
    /// List all sessions with their liveness status and active languages.
    ///
    /// # Errors
    ///
    /// Returns an error if session data cannot be read.
    fn list_sessions(&self) -> Result<Vec<SessionRow>>;

    /// Load all historical messages for a session.
    ///
    /// When `include_debug` is false, messages with `level = "debug"` are
    /// excluded from the result set.
    ///
    /// # Errors
    ///
    /// Returns an error if the session does not exist or messages cannot be read.
    fn monitor_messages(
        &self,
        session_id: &str,
        include_debug: bool,
    ) -> Result<Vec<SessionMessage>>;

    /// Create a tail reader for new messages (from current position onward).
    ///
    /// When `include_debug` is false, the tail skips debug-level messages.
    ///
    /// # Errors
    ///
    /// Returns an error if the session does not exist or the tail cannot be created.
    fn create_message_tail(
        &self,
        session_id: &str,
        include_debug: bool,
    ) -> Result<Box<dyn MessageTail>>;

    /// Delete a dead session's data.
    ///
    /// # Errors
    ///
    /// Returns an error if the session data cannot be removed.
    fn delete_session(&self, session_id: &str) -> Result<()>;

    /// List IDs of sessions marked alive in the database.
    ///
    /// This is a lightweight query (no PID checks, no joins) suitable for
    /// frequent calls on WAL change to detect new sessions.
    ///
    /// # Errors
    ///
    /// Returns an error if the query fails.
    fn list_alive_session_ids(&self) -> Result<Vec<String>>;

    /// Load all historical messages across all sessions.
    ///
    /// When `include_debug` is false, messages with `level = "debug"` are
    /// excluded from the result set.
    ///
    /// # Errors
    ///
    /// Returns an error if the database cannot be queried.
    fn monitor_all_messages(&self, include_debug: bool) -> Result<Vec<SessionMessage>>;

    /// Create a tail reader for new messages across all sessions.
    ///
    /// # Errors
    ///
    /// Returns an error if the tail cannot be created.
    fn create_all_message_tail(&self, include_debug: bool) -> Result<Box<dyn MessageTail>>;
}

/// Tail reader abstraction for streaming new messages.
pub trait MessageTail: Send {
    /// Read the next message if available. Returns `None` if no new message yet.
    ///
    /// # Errors
    ///
    /// Returns an error if reading from the underlying source fails.
    fn try_next_message(&mut self) -> Result<Option<SessionMessage>>;
}

impl MessageTail for SqliteMessageTail {
    fn try_next_message(&mut self) -> Result<Option<SessionMessage>> {
        self.try_next_message()
    }
}

impl MessageTail for SqliteAllMessageTail {
    fn try_next_message(&mut self) -> Result<Option<SessionMessage>> {
        self.try_next_message()
    }
}

// ── SQLite (production) implementation ───────────────────────────────

/// Data source backed by `SQLite` via the [`crate::db`] module.
pub struct SqliteDataSource {
    conn: rusqlite::Connection,
}

impl SqliteDataSource {
    /// Open a new read-only data source.
    ///
    /// The database must already exist (created by `catenary serve`).
    /// The TUI never writes to the database.
    ///
    /// # Errors
    ///
    /// Returns an error if the database file does not exist or cannot be opened.
    pub fn new() -> Result<Self> {
        let path = crate::db::db_path();
        let conn = rusqlite::Connection::open_with_flags(
            &path,
            rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY
                | rusqlite::OpenFlags::SQLITE_OPEN_URI
                | rusqlite::OpenFlags::SQLITE_OPEN_NO_MUTEX,
        )
        .with_context(|| {
            format!(
                "No database found at {}. Is a Catenary session running?",
                path.display()
            )
        })?;
        Ok(Self { conn })
    }

    /// Create a data source with an existing database connection.
    ///
    /// Useful for testing with isolated temporary databases.
    #[must_use]
    pub const fn with_conn(conn: rusqlite::Connection) -> Self {
        Self { conn }
    }
}

/// Raw row from the sessions table (avoids complex tuple types).
struct RawSessionRow {
    id: String,
    pid: u32,
    display_name: String,
    client_name: Option<String>,
    client_version: Option<String>,
    client_session_id: Option<String>,
    started_at_str: String,
    db_alive: bool,
}

impl DataSource for SqliteDataSource {
    fn list_sessions(&self) -> Result<Vec<SessionRow>> {
        let raw = {
            let mut stmt = self.conn.prepare(
                "SELECT id, pid, display_name, client_name, client_version, \
                 client_session_id, started_at, alive \
                 FROM sessions ORDER BY alive DESC, started_at DESC",
            )?;
            let mut r = stmt.query([])?;
            let mut rows = Vec::new();
            while let Some(row) = r.next()? {
                rows.push(RawSessionRow {
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

        let mut sessions = Vec::with_capacity(raw.len());
        for RawSessionRow {
            id,
            pid,
            display_name,
            client_name,
            client_version,
            client_session_id,
            started_at_str,
            db_alive,
        } in raw
        {
            let started_at = DateTime::parse_from_rfc3339(&started_at_str)
                .with_context(|| format!("invalid started_at: {started_at_str}"))?
                .with_timezone(&Utc);

            let alive = if db_alive {
                if session::is_process_alive(pid) {
                    true
                } else {
                    let _ = self.conn.execute(
                        "UPDATE sessions SET alive = 0, ended_at = ?1 WHERE id = ?2",
                        rusqlite::params![Utc::now().to_rfc3339(), &id],
                    );
                    false
                }
            } else {
                false
            };

            let languages = active_languages_for(&self.conn, &id);

            sessions.push(SessionRow {
                info: SessionInfo {
                    id,
                    pid,
                    workspace: display_name,
                    started_at,
                    client_name,
                    client_version,
                    client_session_id,
                },
                alive,
                languages,
            });
        }

        Ok(sessions)
    }

    fn monitor_messages(
        &self,
        session_id: &str,
        include_debug: bool,
    ) -> Result<Vec<SessionMessage>> {
        session::monitor_messages_with_conn(&self.conn, session_id, include_debug)
    }

    fn create_message_tail(
        &self,
        session_id: &str,
        include_debug: bool,
    ) -> Result<Box<dyn MessageTail>> {
        let tail = session::tail_messages_new(session_id, include_debug)?;
        Ok(Box::new(tail))
    }

    fn delete_session(&self, session_id: &str) -> Result<()> {
        self.conn
            .execute("DELETE FROM sessions WHERE id = ?1", [session_id])?;

        // Clean up socket directory if it exists.
        let socket_dir = session::sessions_dir().join(session_id);
        let _ = std::fs::remove_dir_all(&socket_dir);

        Ok(())
    }

    fn list_alive_session_ids(&self) -> Result<Vec<String>> {
        let mut stmt = self
            .conn
            .prepare("SELECT id FROM sessions WHERE alive = 1")?;
        let mut rows = stmt.query([])?;
        let mut ids = Vec::new();
        while let Some(row) = rows.next()? {
            ids.push(row.get(0)?);
        }
        Ok(ids)
    }

    fn monitor_all_messages(&self, include_debug: bool) -> Result<Vec<SessionMessage>> {
        session::monitor_all_messages_with_conn(&self.conn, include_debug)
    }

    fn create_all_message_tail(&self, include_debug: bool) -> Result<Box<dyn MessageTail>> {
        let tail = session::tail_all_messages_new(include_debug)?;
        Ok(Box::new(tail))
    }
}

/// Query active languages for a session from its messages.
fn active_languages_for(conn: &rusqlite::Connection, session_id: &str) -> Vec<String> {
    let Ok(mut stmt) = conn.prepare(
        "SELECT DISTINCT server FROM messages \
         WHERE session_id = ?1 AND type = 'lsp' \
         ORDER BY server",
    ) else {
        return vec![];
    };

    let Ok(mut rows) = stmt.query([session_id]) else {
        return vec![];
    };

    let mut result = Vec::new();
    while let Ok(Some(row)) = rows.next() {
        if let Ok(server) = row.get::<_, String>(0) {
            result.push(server);
        }
    }
    result
}

// ── Mock (testing) implementation ────────────────────────────────────

/// Data source backed by in-memory data for deterministic testing.
pub struct MockDataSource {
    /// Sessions to return from [`DataSource::list_sessions`].
    pub sessions: Vec<SessionRow>,
    /// Messages keyed by session ID for [`DataSource::monitor_messages`].
    pub messages: HashMap<String, Vec<SessionMessage>>,
    /// Tail messages keyed by session ID for [`DataSource::create_message_tail`].
    pub tail_messages: HashMap<String, VecDeque<SessionMessage>>,
}

impl DataSource for MockDataSource {
    fn list_sessions(&self) -> Result<Vec<SessionRow>> {
        // MockDataSource cannot clone SessionRow (SessionInfo requires Clone,
        // which it derives), so we rebuild rows from the stored data.
        let rows = self
            .sessions
            .iter()
            .map(|r| SessionRow {
                info: r.info.clone(),
                alive: r.alive,
                languages: r.languages.clone(),
            })
            .collect();
        Ok(rows)
    }

    fn monitor_messages(
        &self,
        session_id: &str,
        include_debug: bool,
    ) -> Result<Vec<SessionMessage>> {
        let messages = self
            .messages
            .get(session_id)
            .cloned()
            .ok_or_else(|| anyhow::anyhow!("Session not found: {session_id}"))?;
        if include_debug {
            Ok(messages)
        } else {
            Ok(messages
                .into_iter()
                .filter(|m| m.level != "debug")
                .collect())
        }
    }

    fn create_message_tail(
        &self,
        session_id: &str,
        include_debug: bool,
    ) -> Result<Box<dyn MessageTail>> {
        let messages = self
            .tail_messages
            .get(session_id)
            .cloned()
            .unwrap_or_default();
        if include_debug {
            Ok(Box::new(MockMessageTail { messages }))
        } else {
            let filtered = messages
                .into_iter()
                .filter(|m| m.level != "debug")
                .collect();
            Ok(Box::new(MockMessageTail { messages: filtered }))
        }
    }

    fn delete_session(&self, _session_id: &str) -> Result<()> {
        Ok(())
    }

    fn list_alive_session_ids(&self) -> Result<Vec<String>> {
        Ok(self
            .sessions
            .iter()
            .filter(|r| r.alive)
            .map(|r| r.info.id.clone())
            .collect())
    }

    fn monitor_all_messages(&self, include_debug: bool) -> Result<Vec<SessionMessage>> {
        let mut all: Vec<SessionMessage> = self.messages.values().flatten().cloned().collect();
        if !include_debug {
            all.retain(|m| m.level != "debug");
        }
        all.sort_by_key(|m| m.id);
        Ok(all)
    }

    fn create_all_message_tail(&self, include_debug: bool) -> Result<Box<dyn MessageTail>> {
        let mut all: VecDeque<SessionMessage> =
            self.tail_messages.values().flatten().cloned().collect();
        if !include_debug {
            all.retain(|m| m.level != "debug");
        }
        Ok(Box::new(MockMessageTail { messages: all }))
    }
}

/// Tail reader backed by a [`VecDeque`] for testing.
pub struct MockMessageTail {
    messages: VecDeque<SessionMessage>,
}

impl MockMessageTail {
    /// Create a new mock tail with the given messages.
    #[must_use]
    pub const fn new(messages: VecDeque<SessionMessage>) -> Self {
        Self { messages }
    }
}

impl MessageTail for MockMessageTail {
    fn try_next_message(&mut self) -> Result<Option<SessionMessage>> {
        Ok(self.messages.pop_front())
    }
}

#[cfg(test)]
#[allow(
    clippy::expect_used,
    reason = "tests use expect for readable assertions"
)]
mod tests {
    use super::*;
    use chrono::Utc;

    use crate::session::SessionInfo;

    /// Open an isolated test database in a tempdir.
    /// Returns `(TempDir, PathBuf, Connection)` — the tempdir guard must
    /// be held for the lifetime of the connection.
    fn test_db() -> (tempfile::TempDir, std::path::PathBuf, rusqlite::Connection) {
        let dir = tempfile::tempdir().expect("failed to create tempdir for test DB");
        let path = dir.path().join("catenary").join("catenary.db");
        let conn = crate::db::open_and_migrate_at(&path).expect("failed to open test DB");
        (dir, path, conn)
    }

    fn make_session_info(id: &str) -> SessionInfo {
        SessionInfo {
            id: id.to_string(),
            pid: 1234,
            workspace: "/tmp/test".to_string(),
            started_at: Utc::now(),
            client_name: None,
            client_version: None,
            client_session_id: None,
        }
    }

    fn make_message(method: &str) -> SessionMessage {
        crate::session::test_support::message("lsp", method, "rust-analyzer")
    }

    // ── Mock tests ──────────────────────────────────────────────────

    #[test]
    fn test_mock_data_source_list_sessions() -> Result<()> {
        let ds = MockDataSource {
            sessions: vec![
                SessionRow {
                    info: make_session_info("active-1"),
                    alive: true,
                    languages: vec!["rust".to_string()],
                },
                SessionRow {
                    info: make_session_info("dead-1"),
                    alive: false,
                    languages: vec![],
                },
            ],
            messages: HashMap::new(),
            tail_messages: HashMap::new(),
        };

        let rows = ds.list_sessions()?;
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0].info.id, "active-1");
        assert!(rows[0].alive);
        assert_eq!(rows[0].languages, vec!["rust".to_string()]);
        assert_eq!(rows[1].info.id, "dead-1");
        assert!(!rows[1].alive);
        Ok(())
    }

    #[test]
    fn test_mock_data_source_monitor_messages() -> Result<()> {
        let messages = vec![
            make_message("initialize"),
            make_message("textDocument/hover"),
            make_message("textDocument/definition"),
        ];
        let mut messages_map = HashMap::new();
        messages_map.insert("abc".to_string(), messages);

        let ds = MockDataSource {
            sessions: vec![],
            messages: messages_map,
            tail_messages: HashMap::new(),
        };

        let result = ds.monitor_messages("abc", true)?;
        assert_eq!(result.len(), 3);

        let err = ds.monitor_messages("nonexistent", true);
        assert!(err.is_err());
        Ok(())
    }

    #[test]
    fn test_mock_message_tail_drains() -> Result<()> {
        let mut messages = VecDeque::new();
        messages.push_back(make_message("initialize"));
        messages.push_back(make_message("shutdown"));

        let mut tail = MockMessageTail::new(messages);

        assert!(tail.try_next_message()?.is_some());
        assert!(tail.try_next_message()?.is_some());
        assert!(tail.try_next_message()?.is_none());
        Ok(())
    }

    // ── SQLite tests ─────────────────────────────────────────────────

    /// Insert a test session directly into the database.
    fn insert_session(conn: &rusqlite::Connection, id: &str, workspace: &str) {
        conn.execute(
            "INSERT INTO sessions (id, pid, display_name, started_at, alive) \
             VALUES (?1, ?2, ?3, '2026-01-01T00:00:00Z', 1)",
            rusqlite::params![id, std::process::id(), workspace],
        )
        .expect("insert test session");
    }

    #[test]
    fn test_sqlite_data_source_list_sessions() -> Result<()> {
        let (_dir, path, conn) = test_db();
        let write_conn = crate::db::open_and_migrate_at(&path)?;
        insert_session(&write_conn, "ds-list-1", "/tmp/test-ds-list");
        let ds = SqliteDataSource::with_conn(conn);

        let rows = ds.list_sessions()?;
        assert!(rows.iter().any(|r| r.info.id == "ds-list-1"));

        ds.delete_session("ds-list-1")?;
        Ok(())
    }

    /// Insert a test message row directly into the `messages` table.
    fn insert_test_message(conn: &rusqlite::Connection, session_id: &str) {
        conn.execute(
            "INSERT INTO messages \
             (session_id, timestamp, type, method, server, client, \
              request_id, parent_id, payload) \
             VALUES (?1, ?2, 'lsp', 'textDocument/hover', 'rust-analyzer', \
              'catenary', NULL, NULL, '{}')",
            rusqlite::params![session_id, "2026-01-01T00:00:00.000Z"],
        )
        .expect("insert test message");
    }

    #[test]
    fn test_sqlite_data_source_monitor_messages() -> Result<()> {
        let (_dir, path, conn) = test_db();
        let ds = SqliteDataSource::with_conn(conn);

        let write_conn = crate::db::open_and_migrate_at(&path)?;
        insert_session(&write_conn, "ds-msg-1", "/tmp/test-ds-messages");
        insert_test_message(&write_conn, "ds-msg-1");

        let messages = ds.monitor_messages("ds-msg-1", true)?;
        assert!(!messages.is_empty(), "should have at least one message");

        ds.delete_session("ds-msg-1")?;
        Ok(())
    }

    #[test]
    fn test_sqlite_message_tail_streams() -> Result<()> {
        let (_dir, path, conn) = test_db();

        let write_conn = crate::db::open_and_migrate_at(&path)?;
        insert_session(&write_conn, "ds-tail-1", "/tmp/test-ds-tail");

        // Open a fresh connection for the tail (it takes ownership).
        let tail_conn = crate::db::open_at(&path)?;
        let mut tail: Box<dyn MessageTail> = Box::new(crate::session::tail_messages_new_with_conn(
            tail_conn,
            "ds-tail-1",
            true,
        )?);

        // No new messages since tail was created after any existing messages
        // (tail_messages_new starts from the current end).
        assert!(
            tail.try_next_message()?.is_none(),
            "should have no messages initially"
        );

        // Insert a new message directly.
        insert_test_message(&write_conn, "ds-tail-1");

        let msg = tail.try_next_message()?;
        assert!(msg.is_some(), "should see newly inserted message");

        // No more messages.
        assert!(tail.try_next_message()?.is_none());

        conn.execute("DELETE FROM sessions WHERE id = ?1", ["ds-tail-1"])?;
        Ok(())
    }

    #[test]
    fn test_sqlite_data_source_delete_session() -> Result<()> {
        let (_dir, path, conn) = test_db();
        let ds = SqliteDataSource::with_conn(conn);

        let write_conn = crate::db::open_and_migrate_at(&path)?;
        insert_session(&write_conn, "ds-del-1", "/tmp/test-ds-delete");

        // Should exist
        assert!(ds.list_sessions()?.iter().any(|r| r.info.id == "ds-del-1"));

        // Delete
        ds.delete_session("ds-del-1")?;

        // Should be gone
        assert!(!ds.list_sessions()?.iter().any(|r| r.info.id == "ds-del-1"));

        Ok(())
    }

    #[test]
    fn test_mock_list_alive_session_ids() -> Result<()> {
        let ds = MockDataSource {
            sessions: vec![
                SessionRow {
                    info: make_session_info("alive-1"),
                    alive: true,
                    languages: vec![],
                },
                SessionRow {
                    info: make_session_info("dead-1"),
                    alive: false,
                    languages: vec![],
                },
                SessionRow {
                    info: make_session_info("alive-2"),
                    alive: true,
                    languages: vec![],
                },
            ],
            messages: HashMap::new(),
            tail_messages: HashMap::new(),
        };

        let ids = ds.list_alive_session_ids()?;
        assert_eq!(ids.len(), 2);
        assert!(ids.contains(&"alive-1".to_string()));
        assert!(ids.contains(&"alive-2".to_string()));
        assert!(!ids.contains(&"dead-1".to_string()));
        Ok(())
    }

    #[test]
    fn test_sqlite_data_source_active_languages() -> Result<()> {
        let (_dir, path, conn) = test_db();
        let write_conn = crate::db::open_and_migrate_at(&path)?;
        insert_session(&write_conn, "ds-lang-1", "/tmp/test-ds-lang");
        insert_test_message(&write_conn, "ds-lang-1");

        let ds = SqliteDataSource::with_conn(conn);
        let rows = ds.list_sessions()?;
        let row = rows
            .iter()
            .find(|r| r.info.id == "ds-lang-1")
            .expect("session should exist");
        assert_eq!(
            row.languages,
            vec!["rust-analyzer".to_string()],
            "active_languages_for should return the server from LSP messages"
        );

        ds.delete_session("ds-lang-1")?;
        Ok(())
    }

    #[test]
    fn test_mock_create_message_tail_filters_debug() -> Result<()> {
        let mut tail_messages = VecDeque::new();
        let mut debug_msg = make_message("textDocument/hover");
        debug_msg.level = "debug".to_string();
        let info_msg = make_message("textDocument/definition");
        tail_messages.push_back(debug_msg);
        tail_messages.push_back(info_msg);

        let mut map = HashMap::new();
        map.insert("sess-1".to_string(), tail_messages);

        let ds = MockDataSource {
            sessions: vec![],
            messages: HashMap::new(),
            tail_messages: map,
        };

        let mut tail = ds.create_message_tail("sess-1", false)?;
        let first = tail.try_next_message()?;
        assert!(first.is_some(), "should have one non-debug message");
        let msg = first.expect("checked above");
        assert_eq!(
            msg.method, "textDocument/definition",
            "non-debug message should be the one returned"
        );

        let second = tail.try_next_message()?;
        assert!(second.is_none(), "debug message should have been filtered");

        Ok(())
    }

    #[test]
    fn test_sqlite_list_alive_session_ids() -> Result<()> {
        let (_dir, path, conn) = test_db();
        let write_conn = crate::db::open_and_migrate_at(&path)?;
        insert_session(&write_conn, "ds-alive-1", "/tmp/test-ds-alive-ids");
        let ds = SqliteDataSource::with_conn(conn);

        // Session is alive (process is running, PID matches current).
        let ids = ds.list_alive_session_ids()?;
        assert!(
            ids.contains(&"ds-alive-1".to_string()),
            "alive session should appear"
        );

        ds.delete_session("ds-alive-1")?;
        Ok(())
    }
}
