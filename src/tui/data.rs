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

/// A server instance row from the `language_servers` table.
#[derive(Debug, Clone)]
pub struct ServerStatusRow {
    /// Language ID this instance handles.
    pub language_id: String,
    /// Server binary name (config key).
    pub server: String,
    /// Scope kind (`"root"`, `"single_file"`).
    pub scope_kind: String,
    /// Scope root path (empty for single-file).
    pub scope_root: String,
    /// Lifecycle display state (`"initializing"`, `"ready"`, `"busy"`, `"dead"`).
    pub state: String,
}

/// A server noise row: the most recent `$/progress`, `window/logMessage`,
/// or `window/showMessage` per server.
#[derive(Debug, Clone)]
pub struct ServerNoiseRow {
    /// Server binary name (matches `ServerStatusRow::server`).
    pub server: String,
    /// LSP method (`$/progress`, `window/logMessage`, `window/showMessage`).
    pub method: String,
    /// Raw protocol JSON payload.
    pub payload: serde_json::Value,
}

/// Methods that constitute server noise — redirected from stream to sidebar.
pub const SERVER_NOISE_METHODS: &[&str] =
    &["$/progress", "window/logMessage", "window/showMessage"];

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

    /// Load all historical messages for a session (info level and above).
    ///
    /// # Errors
    ///
    /// Returns an error if the session does not exist or messages cannot be read.
    fn monitor_messages(&self, session_id: &str) -> Result<Vec<SessionMessage>>;

    /// Create a tail reader for new messages (from current position onward).
    ///
    /// # Errors
    ///
    /// Returns an error if the session does not exist or the tail cannot be created.
    fn create_message_tail(&self, session_id: &str) -> Result<Box<dyn MessageTail>>;

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

    /// Load all historical messages across all sessions (info level and above).
    ///
    /// # Errors
    ///
    /// Returns an error if the database cannot be queried.
    fn monitor_all_messages(&self) -> Result<Vec<SessionMessage>>;

    /// Create a tail reader for new messages across all sessions.
    ///
    /// # Errors
    ///
    /// Returns an error if the tail cannot be created.
    fn create_all_message_tail(&self) -> Result<Box<dyn MessageTail>>;

    /// Load the most recent `limit` scopes (roots + children).
    ///
    /// A scope root is either a standalone message or the first message
    /// for a given `parent_id` UUID.
    ///
    /// # Errors
    ///
    /// Returns an error if the database cannot be queried.
    fn recent_scopes(&self, limit: usize) -> Result<Vec<SessionMessage>>;

    /// Load scopes with root ID older than `before_id`, newest first.
    ///
    /// When `after_id` is provided, results are bounded below (for gap
    /// filling from the bottom side).
    ///
    /// # Errors
    ///
    /// Returns an error if the database cannot be queried.
    fn older_scopes(
        &self,
        before_id: i64,
        after_id: Option<i64>,
        limit: usize,
    ) -> Result<Vec<SessionMessage>>;

    /// Load scopes with root ID newer than `after_id`, oldest first.
    ///
    /// When `before_id` is provided, results are bounded above (for gap
    /// filling between two loaded regions).
    ///
    /// # Errors
    ///
    /// Returns an error if the database cannot be queried.
    fn newer_scopes(
        &self,
        after_id: i64,
        before_id: Option<i64>,
        limit: usize,
    ) -> Result<Vec<SessionMessage>>;

    /// Load the oldest `limit` scopes (roots + children).
    ///
    /// # Errors
    ///
    /// Returns an error if the database cannot be queried.
    fn oldest_scopes(&self, limit: usize) -> Result<Vec<SessionMessage>>;

    /// List active server instances from the `language_servers` table.
    ///
    /// Returns only non-terminal servers (excludes `"dead"` state).
    ///
    /// # Errors
    ///
    /// Returns an error if the database cannot be queried.
    fn list_server_statuses(&self) -> Result<Vec<ServerStatusRow>>;

    /// Load the most recent server noise row per server.
    ///
    /// Queries `$/progress`, `window/logMessage`, and `window/showMessage`
    /// from the messages table. Returns at most one row per (server, method)
    /// combination — the most recent by message ID.
    ///
    /// # Errors
    ///
    /// Returns an error if the database cannot be queried.
    fn list_server_noise(&self) -> Result<Vec<ServerNoiseRow>>;
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

    fn monitor_messages(&self, session_id: &str) -> Result<Vec<SessionMessage>> {
        session::monitor_messages_with_conn(&self.conn, session_id, false)
    }

    fn create_message_tail(&self, session_id: &str) -> Result<Box<dyn MessageTail>> {
        let tail = session::tail_messages_new(session_id, false)?;
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

    fn monitor_all_messages(&self) -> Result<Vec<SessionMessage>> {
        session::monitor_all_messages_with_conn(&self.conn, false)
    }

    fn create_all_message_tail(&self) -> Result<Box<dyn MessageTail>> {
        let tail = session::tail_all_messages_new(false)?;
        Ok(Box::new(tail))
    }

    fn recent_scopes(&self, limit: usize) -> Result<Vec<SessionMessage>> {
        session::recent_scopes_with_conn(&self.conn, limit, false)
    }

    fn older_scopes(
        &self,
        before_id: i64,
        after_id: Option<i64>,
        limit: usize,
    ) -> Result<Vec<SessionMessage>> {
        session::older_scopes_with_conn(&self.conn, before_id, after_id, limit, false)
    }

    fn newer_scopes(
        &self,
        after_id: i64,
        before_id: Option<i64>,
        limit: usize,
    ) -> Result<Vec<SessionMessage>> {
        session::newer_scopes_with_conn(&self.conn, after_id, before_id, limit, false)
    }

    fn oldest_scopes(&self, limit: usize) -> Result<Vec<SessionMessage>> {
        session::oldest_scopes_with_conn(&self.conn, limit, false)
    }

    fn list_server_statuses(&self) -> Result<Vec<ServerStatusRow>> {
        let mut stmt = self.conn.prepare(
            "SELECT language_id, server, scope_kind, scope_root, state \
             FROM language_servers \
             WHERE state != 'dead' \
             ORDER BY server, language_id, scope_root",
        )?;
        let mut rows = stmt.query([])?;
        let mut result = Vec::new();
        while let Some(row) = rows.next()? {
            result.push(ServerStatusRow {
                language_id: row.get(0)?,
                server: row.get(1)?,
                scope_kind: row.get(2)?,
                scope_root: row.get(3)?,
                state: row.get(4)?,
            });
        }
        Ok(result)
    }

    fn list_server_noise(&self) -> Result<Vec<ServerNoiseRow>> {
        // Most recent row per (server, method) for server noise methods.
        let mut stmt = self.conn.prepare(
            "SELECT server, method, payload FROM messages \
             WHERE type = 'lsp' \
               AND method IN ('$/progress', 'window/logMessage', 'window/showMessage') \
               AND parent_id IS NULL \
               AND id IN ( \
                   SELECT MAX(id) FROM messages \
                   WHERE type = 'lsp' \
                     AND method IN ('$/progress', 'window/logMessage', 'window/showMessage') \
                     AND parent_id IS NULL \
                   GROUP BY server, method \
               ) \
             ORDER BY server, method",
        )?;
        let mut rows = stmt.query([])?;
        let mut result = Vec::new();
        while let Some(row) = rows.next()? {
            let payload_str: String = row.get(2)?;
            let payload: serde_json::Value =
                serde_json::from_str(&payload_str).unwrap_or(serde_json::Value::Null);
            result.push(ServerNoiseRow {
                server: row.get(0)?,
                method: row.get(1)?,
                payload,
            });
        }
        Ok(result)
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
    /// Server statuses for [`DataSource::list_server_statuses`].
    pub server_statuses: Vec<ServerStatusRow>,
    /// Server noise for [`DataSource::list_server_noise`].
    pub server_noise: Vec<ServerNoiseRow>,
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

    fn monitor_messages(&self, session_id: &str) -> Result<Vec<SessionMessage>> {
        let messages = self
            .messages
            .get(session_id)
            .cloned()
            .ok_or_else(|| anyhow::anyhow!("Session not found: {session_id}"))?;
        Ok(messages
            .into_iter()
            .filter(|m| m.level != "debug")
            .collect())
    }

    fn create_message_tail(&self, session_id: &str) -> Result<Box<dyn MessageTail>> {
        let messages = self
            .tail_messages
            .get(session_id)
            .cloned()
            .unwrap_or_default();
        let filtered = messages
            .into_iter()
            .filter(|m| m.level != "debug")
            .collect();
        Ok(Box::new(MockMessageTail { messages: filtered }))
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

    fn monitor_all_messages(&self) -> Result<Vec<SessionMessage>> {
        let mut all: Vec<SessionMessage> = self.messages.values().flatten().cloned().collect();
        all.retain(|m| m.level != "debug");
        all.sort_by_key(|m| m.id);
        Ok(all)
    }

    fn create_all_message_tail(&self) -> Result<Box<dyn MessageTail>> {
        let mut all: VecDeque<SessionMessage> =
            self.tail_messages.values().flatten().cloned().collect();
        all.retain(|m| m.level != "debug");
        Ok(Box::new(MockMessageTail { messages: all }))
    }

    fn recent_scopes(&self, limit: usize) -> Result<Vec<SessionMessage>> {
        let all = self.sorted_messages();
        let roots = scope_roots_from_messages(&all);
        let page: Vec<_> = roots.iter().rev().take(limit).cloned().collect();
        Ok(collect_scope_messages(&all, &page))
    }

    fn older_scopes(
        &self,
        before_id: i64,
        after_id: Option<i64>,
        limit: usize,
    ) -> Result<Vec<SessionMessage>> {
        let all = self.sorted_messages();
        let roots = scope_roots_from_messages(&all);
        let page: Vec<_> = roots
            .iter()
            .rev()
            .filter(|r| r.root_id < before_id && after_id.is_none_or(|a| r.root_id > a))
            .take(limit)
            .cloned()
            .collect();
        Ok(collect_scope_messages(&all, &page))
    }

    fn newer_scopes(
        &self,
        after_id: i64,
        before_id: Option<i64>,
        limit: usize,
    ) -> Result<Vec<SessionMessage>> {
        let all = self.sorted_messages();
        let roots = scope_roots_from_messages(&all);
        let page: Vec<_> = roots
            .iter()
            .filter(|r| r.root_id > after_id && before_id.is_none_or(|b| r.root_id < b))
            .take(limit)
            .cloned()
            .collect();
        Ok(collect_scope_messages(&all, &page))
    }

    fn oldest_scopes(&self, limit: usize) -> Result<Vec<SessionMessage>> {
        let all = self.sorted_messages();
        let roots = scope_roots_from_messages(&all);
        let page: Vec<_> = roots.iter().take(limit).cloned().collect();
        Ok(collect_scope_messages(&all, &page))
    }

    fn list_server_statuses(&self) -> Result<Vec<ServerStatusRow>> {
        Ok(self
            .server_statuses
            .iter()
            .filter(|s| s.state != "dead")
            .cloned()
            .collect())
    }

    fn list_server_noise(&self) -> Result<Vec<ServerNoiseRow>> {
        Ok(self.server_noise.clone())
    }
}

impl MockDataSource {
    /// Collect and sort all messages across sessions (info level and above).
    fn sorted_messages(&self) -> Vec<SessionMessage> {
        let mut all: Vec<SessionMessage> = self.messages.values().flatten().cloned().collect();
        all.retain(|m| m.level != "debug");
        all.sort_by_key(|m| m.id);
        all
    }
}

/// A scope root entry identified from the message stream.
#[derive(Clone)]
struct ScopeRoot {
    /// Message ID of the first message in this scope.
    root_id: i64,
    /// `parent_id` UUID, or `None` for standalone messages.
    parent_id: Option<String>,
}

/// Identify scope roots from a sorted message list.
///
/// A scope root is either a standalone message (`parent_id` is `None`)
/// or the first message (lowest ID) for a given `parent_id` UUID.
/// Returns roots sorted by `root_id` ascending.
fn scope_roots_from_messages(messages: &[SessionMessage]) -> Vec<ScopeRoot> {
    let mut seen_parents: std::collections::HashSet<&str> = std::collections::HashSet::new();
    let mut roots = Vec::new();

    for msg in messages {
        match &msg.parent_id {
            None => roots.push(ScopeRoot {
                root_id: msg.id,
                parent_id: None,
            }),
            Some(pid) => {
                if seen_parents.insert(pid.as_str()) {
                    roots.push(ScopeRoot {
                        root_id: msg.id,
                        parent_id: Some(pid.clone()),
                    });
                }
            }
        }
    }

    roots.sort_by_key(|r| r.root_id);
    roots
}

/// Collect all messages belonging to the given scope roots.
fn collect_scope_messages(
    all_messages: &[SessionMessage],
    page_roots: &[ScopeRoot],
) -> Vec<SessionMessage> {
    let standalone_ids: std::collections::HashSet<i64> = page_roots
        .iter()
        .filter(|r| r.parent_id.is_none())
        .map(|r| r.root_id)
        .collect();

    let parent_ids: std::collections::HashSet<&str> = page_roots
        .iter()
        .filter_map(|r| r.parent_id.as_deref())
        .collect();

    let mut result: Vec<SessionMessage> = all_messages
        .iter()
        .filter(|m| {
            m.parent_id.as_ref().map_or_else(
                || standalone_ids.contains(&m.id),
                |pid| parent_ids.contains(pid.as_str()),
            )
        })
        .cloned()
        .collect();

    result.sort_by_key(|m| m.id);
    result
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
            server_statuses: Vec::new(),
            server_noise: Vec::new(),
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
            server_statuses: Vec::new(),
            server_noise: Vec::new(),
        };

        let result = ds.monitor_messages("abc")?;
        assert_eq!(result.len(), 3);

        let err = ds.monitor_messages("nonexistent");
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
              parent_id, payload) \
             VALUES (?1, ?2, 'lsp', 'textDocument/hover', 'rust-analyzer', \
              'catenary', NULL, '{}')",
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

        let messages = ds.monitor_messages("ds-msg-1")?;
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
            server_statuses: Vec::new(),
            server_noise: Vec::new(),
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
    fn test_sqlite_list_server_statuses() -> Result<()> {
        let (_dir, path, conn) = test_db();
        let write_conn = crate::db::open_and_migrate_at(&path)?;

        // FK requires a session row.
        insert_session(&write_conn, "daemon", "/tmp/daemon");

        // Insert server rows.
        write_conn.execute(
            "INSERT INTO language_servers \
             (session_id, language_id, server, scope_kind, scope_root, state) \
             VALUES ('daemon', 'rust', 'rust-analyzer', 'root', '/home/user/A', 'ready')",
            [],
        )?;
        write_conn.execute(
            "INSERT INTO language_servers \
             (session_id, language_id, server, scope_kind, scope_root, state) \
             VALUES ('daemon', 'rust', 'rust-analyzer', 'root', '/home/user/B', 'busy')",
            [],
        )?;
        write_conn.execute(
            "INSERT INTO language_servers \
             (session_id, language_id, server, scope_kind, scope_root, state) \
             VALUES ('daemon', 'lua', 'lua-ls', 'root', '/home/user/C', 'dead')",
            [],
        )?;

        let ds = SqliteDataSource::with_conn(conn);
        let rows = ds.list_server_statuses()?;

        // Should exclude dead servers.
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0].server, "rust-analyzer");
        assert_eq!(rows[0].state, "ready");
        assert_eq!(rows[1].server, "rust-analyzer");
        assert_eq!(rows[1].state, "busy");
        Ok(())
    }

    #[test]
    fn test_mock_list_server_statuses_excludes_dead() -> Result<()> {
        let ds = MockDataSource {
            sessions: vec![],
            messages: HashMap::new(),
            tail_messages: HashMap::new(),
            server_statuses: vec![
                ServerStatusRow {
                    language_id: "rust".to_string(),
                    server: "rust-analyzer".to_string(),
                    scope_kind: "root".to_string(),
                    scope_root: "/A".to_string(),
                    state: "ready".to_string(),
                },
                ServerStatusRow {
                    language_id: "lua".to_string(),
                    server: "lua-ls".to_string(),
                    scope_kind: "root".to_string(),
                    scope_root: "/B".to_string(),
                    state: "dead".to_string(),
                },
            ],
            server_noise: Vec::new(),
        };

        let rows = ds.list_server_statuses()?;
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].server, "rust-analyzer");
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
            server_statuses: Vec::new(),
            server_noise: Vec::new(),
        };

        let mut tail = ds.create_message_tail("sess-1")?;
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

    // ── Scope paging tests (mock) ───────────────────────────────────

    use crate::session::test_support;

    fn scoped_msg(id: i64, parent_id: &str, r#type: &str, method: &str) -> SessionMessage {
        SessionMessage {
            id,
            parent_id: Some(parent_id.to_string()),
            ..test_support::message(r#type, method, "rust-analyzer")
        }
    }

    fn standalone_msg(id: i64, method: &str) -> SessionMessage {
        SessionMessage {
            id,
            ..test_support::message("lsp", method, "rust-analyzer")
        }
    }

    /// Build a `MockDataSource` with the given messages under session "test".
    fn mock_ds(messages: Vec<SessionMessage>) -> MockDataSource {
        let mut map = HashMap::new();
        map.insert("test".to_string(), messages);
        MockDataSource {
            sessions: vec![],
            messages: map,
            tail_messages: HashMap::new(),
            server_statuses: Vec::new(),
            server_noise: Vec::new(),
        }
    }

    #[test]
    fn test_mock_recent_scopes_returns_newest() -> Result<()> {
        let ds = mock_ds(vec![
            scoped_msg(1, "uuid-a", "mcp", "tools/call"),
            scoped_msg(2, "uuid-a", "lsp", "workspace/symbol"),
            scoped_msg(3, "uuid-b", "mcp", "tools/call"),
            standalone_msg(4, "textDocument/hover"),
        ]);

        // Request 2 most recent scopes.
        let msgs = ds.recent_scopes(2)?;
        let ids: Vec<i64> = msgs.iter().map(|m| m.id).collect();
        // Should include scope uuid-b (root=3) and standalone (id=4).
        assert!(ids.contains(&3), "should include scope-b root: {ids:?}");
        assert!(ids.contains(&4), "should include standalone: {ids:?}");
        // Should NOT include scope uuid-a messages.
        assert!(!ids.contains(&1), "should not include scope-a: {ids:?}");
        Ok(())
    }

    #[test]
    fn test_mock_older_scopes() -> Result<()> {
        let ds = mock_ds(vec![
            scoped_msg(1, "uuid-a", "mcp", "tools/call"),
            scoped_msg(2, "uuid-a", "lsp", "workspace/symbol"),
            scoped_msg(3, "uuid-b", "mcp", "tools/call"),
            standalone_msg(4, "textDocument/hover"),
        ]);

        // Scopes older than id 3 (scope-b root).
        let msgs = ds.older_scopes(3, None, 10)?;
        let ids: Vec<i64> = msgs.iter().map(|m| m.id).collect();
        // Should include scope uuid-a (root=1) messages.
        assert!(ids.contains(&1), "should include scope-a root: {ids:?}");
        assert!(ids.contains(&2), "should include scope-a child: {ids:?}");
        // Should NOT include scope-b or standalone.
        assert!(!ids.contains(&3), "should not include scope-b: {ids:?}");
        assert!(!ids.contains(&4), "should not include standalone: {ids:?}");
        Ok(())
    }

    #[test]
    fn test_mock_newest_scopes() -> Result<()> {
        let ds = mock_ds(vec![
            scoped_msg(1, "uuid-a", "mcp", "tools/call"),
            scoped_msg(3, "uuid-b", "mcp", "tools/call"),
            standalone_msg(5, "textDocument/hover"),
        ]);

        // Scopes newer than id 1 (scope-a root).
        let msgs = ds.newer_scopes(1, None, 10)?;
        let ids: Vec<i64> = msgs.iter().map(|m| m.id).collect();
        assert!(ids.contains(&3), "should include scope-b: {ids:?}");
        assert!(ids.contains(&5), "should include standalone: {ids:?}");
        assert!(!ids.contains(&1), "should not include scope-a: {ids:?}");
        Ok(())
    }

    #[test]
    fn test_mock_newest_scopes_bounded() -> Result<()> {
        let ds = mock_ds(vec![
            scoped_msg(1, "uuid-a", "mcp", "tools/call"),
            scoped_msg(3, "uuid-b", "mcp", "tools/call"),
            standalone_msg(5, "textDocument/hover"),
        ]);

        // Scopes newer than 1 but older than 5.
        let msgs = ds.newer_scopes(1, Some(5), 10)?;
        let ids: Vec<i64> = msgs.iter().map(|m| m.id).collect();
        assert!(ids.contains(&3), "should include scope-b: {ids:?}");
        assert!(!ids.contains(&5), "should not include standalone: {ids:?}");
        Ok(())
    }

    #[test]
    fn test_mock_oldest_scopes() -> Result<()> {
        let ds = mock_ds(vec![
            standalone_msg(1, "init"),
            scoped_msg(2, "uuid-a", "mcp", "tools/call"),
            scoped_msg(3, "uuid-b", "mcp", "tools/call"),
        ]);

        let msgs = ds.oldest_scopes(1)?;
        let ids: Vec<i64> = msgs.iter().map(|m| m.id).collect();
        assert_eq!(ids, vec![1], "should include only the oldest scope");
        Ok(())
    }

    // ── Scope paging tests (SQLite) ─────────────────────────────────

    fn insert_scoped_message(
        conn: &rusqlite::Connection,
        session_id: &str,
        id: i64,
        parent_id: Option<&str>,
        method: &str,
    ) {
        conn.execute(
            "INSERT INTO messages \
             (id, session_id, timestamp, type, method, server, client, \
              parent_id, payload) \
             VALUES (?1, ?2, ?3, 'mcp', ?4, 'ra', 'catenary', ?5, '{}')",
            rusqlite::params![
                id,
                session_id,
                "2026-01-01T00:00:00.000Z",
                method,
                parent_id,
            ],
        )
        .expect("insert scoped message");
    }

    #[test]
    fn test_sqlite_recent_scopes() -> Result<()> {
        let (_dir, path, conn) = test_db();
        let write = crate::db::open_and_migrate_at(&path)?;
        insert_session(&write, "sp-1", "/tmp/test-sp");

        // Insert 3 scopes + 1 standalone.
        insert_scoped_message(&write, "sp-1", 1, Some("uuid-a"), "tools/call");
        insert_scoped_message(&write, "sp-1", 2, Some("uuid-a"), "workspace/symbol");
        insert_scoped_message(&write, "sp-1", 3, Some("uuid-b"), "tools/call");
        insert_scoped_message(&write, "sp-1", 4, Some("uuid-b"), "workspace/symbol");
        insert_scoped_message(&write, "sp-1", 5, Some("uuid-c"), "tools/call");
        insert_scoped_message(&write, "sp-1", 6, None, "textDocument/hover");

        let ds = SqliteDataSource::with_conn(conn);

        // Request 2 most recent scopes.
        let msgs = ds.recent_scopes(2)?;
        let ids: Vec<i64> = msgs.iter().map(|m| m.id).collect();

        // Scope uuid-c (root=5) and standalone (id=6) are the 2 most recent.
        assert!(ids.contains(&5), "should include scope-c root: {ids:?}");
        assert!(ids.contains(&6), "should include standalone: {ids:?}");
        // Scope uuid-a should not be included.
        assert!(!ids.contains(&1), "should not include scope-a: {ids:?}");

        ds.delete_session("sp-1")?;
        Ok(())
    }

    #[test]
    fn test_sqlite_older_scopes() -> Result<()> {
        let (_dir, path, conn) = test_db();
        let write = crate::db::open_and_migrate_at(&path)?;
        insert_session(&write, "sp-2", "/tmp/test-sp-older");

        insert_scoped_message(&write, "sp-2", 1, Some("uuid-a"), "tools/call");
        insert_scoped_message(&write, "sp-2", 2, Some("uuid-a"), "workspace/symbol");
        insert_scoped_message(&write, "sp-2", 3, Some("uuid-b"), "tools/call");

        let ds = SqliteDataSource::with_conn(conn);

        // Scopes older than id 3 (scope-b root).
        let msgs = ds.older_scopes(3, None, 10)?;
        let ids: Vec<i64> = msgs.iter().map(|m| m.id).collect();
        assert!(ids.contains(&1), "should include scope-a root: {ids:?}");
        assert!(ids.contains(&2), "should include scope-a child: {ids:?}");
        assert!(!ids.contains(&3), "should not include scope-b: {ids:?}");

        ds.delete_session("sp-2")?;
        Ok(())
    }

    #[test]
    fn test_sqlite_oldest_scopes() -> Result<()> {
        let (_dir, path, conn) = test_db();
        let write = crate::db::open_and_migrate_at(&path)?;
        insert_session(&write, "sp-3", "/tmp/test-sp-oldest");

        insert_scoped_message(&write, "sp-3", 1, Some("uuid-a"), "tools/call");
        insert_scoped_message(&write, "sp-3", 2, Some("uuid-b"), "tools/call");
        insert_scoped_message(&write, "sp-3", 3, None, "hover");

        let ds = SqliteDataSource::with_conn(conn);

        let msgs = ds.oldest_scopes(1)?;
        let ids: Vec<i64> = msgs.iter().map(|m| m.id).collect();
        assert_eq!(ids, vec![1], "should return only scope-a: {ids:?}");

        ds.delete_session("sp-3")?;
        Ok(())
    }

    #[test]
    fn test_sqlite_newer_scopes_bounded() -> Result<()> {
        let (_dir, path, conn) = test_db();
        let write = crate::db::open_and_migrate_at(&path)?;
        insert_session(&write, "sp-4", "/tmp/test-sp-newer");

        insert_scoped_message(&write, "sp-4", 1, Some("uuid-a"), "tools/call");
        insert_scoped_message(&write, "sp-4", 3, Some("uuid-b"), "tools/call");
        insert_scoped_message(&write, "sp-4", 5, Some("uuid-c"), "tools/call");

        let ds = SqliteDataSource::with_conn(conn);

        // Scopes newer than 1 but older than 5.
        let msgs = ds.newer_scopes(1, Some(5), 10)?;
        let ids: Vec<i64> = msgs.iter().map(|m| m.id).collect();
        assert!(ids.contains(&3), "should include scope-b: {ids:?}");
        assert!(!ids.contains(&1), "should not include scope-a: {ids:?}");
        assert!(!ids.contains(&5), "should not include scope-c: {ids:?}");

        ds.delete_session("sp-4")?;
        Ok(())
    }
}
