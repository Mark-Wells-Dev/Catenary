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

/// A server noise row from the `language_servers` table: progress and
/// message state for a single server instance.
#[derive(Debug, Clone)]
pub struct ServerNoiseRow {
    /// Server binary name (matches `ServerStatusRow::server`).
    pub server: String,
    /// Scope root path (matches `ServerStatusRow::scope_root`).
    pub scope_root: String,
    /// Active progress title, if any (from `$/progress`).
    pub progress_title: Option<String>,
    /// Active progress percentage, if any (from `$/progress`).
    pub progress_pct: Option<u32>,
    /// Most recent server message (from `window/logMessage` or `window/showMessage`).
    pub last_message: Option<String>,
}

/// A single server message entry for the popup detail view.
#[derive(Debug, Clone)]
pub struct ServerMessageDetail {
    /// LSP method (`window/logMessage` or `window/showMessage`).
    pub method: String,
    /// The message text extracted from the payload.
    pub message: String,
    /// When the message was received.
    pub timestamp: DateTime<Utc>,
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

    /// List IDs of sessions marked alive in the database.
    ///
    /// This is a lightweight query (no PID checks, no joins) suitable for
    /// frequent calls on WAL change to detect new sessions.
    ///
    /// # Errors
    ///
    /// Returns an error if the query fails.
    fn list_alive_session_ids(&self) -> Result<Vec<String>>;

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
    /// # Errors
    ///
    /// Returns an error if the database cannot be queried.
    fn older_scopes(&self, before_id: i64, limit: usize) -> Result<Vec<SessionMessage>>;

    /// List active server instances from the `language_servers` table.
    ///
    /// Returns only non-terminal servers (excludes `"dead"` state).
    ///
    /// # Errors
    ///
    /// Returns an error if the database cannot be queried.
    fn list_server_statuses(&self) -> Result<Vec<ServerStatusRow>>;

    /// Load server noise (progress + messages) from the `language_servers` table.
    ///
    /// Returns one row per non-dead server instance that has an active
    /// progress title or a stored server message. Progress and message
    /// columns are written by the daemon as it processes `$/progress`
    /// and `window/logMessage`/`window/showMessage` notifications.
    ///
    /// # Errors
    ///
    /// Returns an error if the database cannot be queried.
    fn list_server_noise(&self) -> Result<Vec<ServerNoiseRow>>;

    /// Load all `window/logMessage` and `window/showMessage` entries for
    /// a specific server instance, newest first.
    ///
    /// Used by the server message popup to show full message history.
    ///
    /// # Errors
    ///
    /// Returns an error if the database cannot be queried.
    fn list_server_message_history(
        &self,
        server: &str,
        scope_root: &str,
    ) -> Result<Vec<ServerMessageDetail>>;
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
                 FROM sessions WHERE id LIKE 'mcp:%' \
                 ORDER BY alive DESC, started_at DESC",
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

        // Batch query: all (session_id, server) pairs in one pass.
        let lang_map = batch_active_languages(&self.conn);

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

            let alive = db_alive && session::is_process_alive(pid);

            let languages = lang_map.get(&id).cloned().unwrap_or_default();

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

    fn list_alive_session_ids(&self) -> Result<Vec<String>> {
        let mut stmt = self
            .conn
            .prepare("SELECT id FROM sessions WHERE alive = 1 AND id LIKE 'mcp:%'")?;
        let mut rows = stmt.query([])?;
        let mut ids = Vec::new();
        while let Some(row) = rows.next()? {
            ids.push(row.get(0)?);
        }
        Ok(ids)
    }

    fn create_all_message_tail(&self) -> Result<Box<dyn MessageTail>> {
        let tail = session::tail_all_messages_new(false)?;
        Ok(Box::new(tail))
    }

    fn recent_scopes(&self, limit: usize) -> Result<Vec<SessionMessage>> {
        session::recent_scopes_with_conn(&self.conn, limit, false)
    }

    fn older_scopes(&self, before_id: i64, limit: usize) -> Result<Vec<SessionMessage>> {
        session::older_scopes_with_conn(&self.conn, before_id, limit, false)
    }

    fn list_server_statuses(&self) -> Result<Vec<ServerStatusRow>> {
        let mut stmt = self.conn.prepare(
            "SELECT ls.language_id, ls.server, ls.scope_kind, ls.scope_root, ls.state \
             FROM language_servers ls \
             JOIN sessions s ON s.id = ls.session_id AND s.alive = 1 \
             WHERE ls.state != 'dead' \
             ORDER BY ls.server, ls.language_id, ls.scope_root",
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
        let mut stmt = self.conn.prepare(
            "SELECT ls.server, ls.scope_root, ls.progress_title, ls.progress_pct, ls.last_message \
             FROM language_servers ls \
             JOIN sessions s ON s.id = ls.session_id AND s.alive = 1 \
             WHERE ls.state != 'dead' \
               AND (ls.progress_title IS NOT NULL OR ls.last_message IS NOT NULL) \
             ORDER BY ls.server, ls.scope_root",
        )?;
        let mut rows = stmt.query([])?;
        let mut result = Vec::new();
        while let Some(row) = rows.next()? {
            result.push(ServerNoiseRow {
                server: row.get(0)?,
                scope_root: row.get(1)?,
                progress_title: row.get(2)?,
                progress_pct: row.get(3)?,
                last_message: row.get(4)?,
            });
        }
        Ok(result)
    }

    fn list_server_message_history(
        &self,
        server: &str,
        scope_root: &str,
    ) -> Result<Vec<ServerMessageDetail>> {
        let mut stmt = self.conn.prepare(
            "SELECT method, payload, timestamp FROM messages \
             WHERE type = 'lsp' \
               AND method IN ('window/logMessage', 'window/showMessage') \
               AND server = ?1 \
               AND scope_root = ?2 \
               AND parent_id IS NULL \
             ORDER BY id DESC",
        )?;
        let mut rows = stmt.query(rusqlite::params![server, scope_root])?;
        let mut result = Vec::new();
        while let Some(row) = rows.next()? {
            let method: String = row.get(0)?;
            let payload_str: String = row.get(1)?;
            let ts_str: String = row.get(2)?;
            let timestamp = DateTime::parse_from_rfc3339(&ts_str)
                .map(|dt| dt.with_timezone(&Utc))
                .unwrap_or_default();
            let payload: serde_json::Value =
                serde_json::from_str(&payload_str).unwrap_or(serde_json::Value::Null);
            let message = payload
                .get("params")
                .and_then(|p| p.get("message"))
                .and_then(|m| m.as_str())
                .unwrap_or("")
                .to_string();
            if !message.is_empty() {
                result.push(ServerMessageDetail {
                    method,
                    message,
                    timestamp,
                });
            }
        }
        Ok(result)
    }
}

/// Batch query: all active languages grouped by session ID.
///
/// Replaces N+1 per-session `SELECT DISTINCT` queries with a single
/// `GROUP BY session_id, server` scan.
fn batch_active_languages(conn: &rusqlite::Connection) -> HashMap<String, Vec<String>> {
    let Ok(mut stmt) = conn.prepare(
        "SELECT session_id, server FROM messages \
         WHERE type = 'lsp' \
         GROUP BY session_id, server \
         ORDER BY session_id, server",
    ) else {
        return HashMap::new();
    };

    let Ok(mut rows) = stmt.query([]) else {
        return HashMap::new();
    };

    let mut map: HashMap<String, Vec<String>> = HashMap::new();
    while let Ok(Some(row)) = rows.next() {
        if let (Ok(sid), Ok(server)) = (row.get::<_, String>(0), row.get::<_, String>(1)) {
            map.entry(sid).or_default().push(server);
        }
    }
    map
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

    fn list_alive_session_ids(&self) -> Result<Vec<String>> {
        Ok(self
            .sessions
            .iter()
            .filter(|r| r.alive)
            .map(|r| r.info.id.clone())
            .collect())
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

    fn older_scopes(&self, before_id: i64, limit: usize) -> Result<Vec<SessionMessage>> {
        let all = self.sorted_messages();
        let roots = scope_roots_from_messages(&all);
        let page: Vec<_> = roots
            .iter()
            .rev()
            .filter(|r| r.root_id < before_id)
            .take(limit)
            .cloned()
            .collect();
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

    fn list_server_message_history(
        &self,
        server: &str,
        scope_root: &str,
    ) -> Result<Vec<ServerMessageDetail>> {
        let result: Vec<ServerMessageDetail> = self
            .server_noise
            .iter()
            .filter(|n| n.server == server && n.scope_root == scope_root)
            .filter_map(|n| {
                let message = n.last_message.as_ref()?.clone();
                Some(ServerMessageDetail {
                    method: "window/logMessage".to_string(),
                    message,
                    timestamp: Utc::now(),
                })
            })
            .collect();
        Ok(result)
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
        insert_session(&write_conn, "mcp:5", "/tmp/test-ds-list");
        let ds = SqliteDataSource::with_conn(conn);

        let rows = ds.list_sessions()?;
        assert!(rows.iter().any(|r| r.info.id == "mcp:5"));

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
        insert_session(&write_conn, "mcp:6", "/tmp/test-ds-lang");
        insert_test_message(&write_conn, "mcp:6");

        let ds = SqliteDataSource::with_conn(conn);
        let rows = ds.list_sessions()?;
        let row = rows
            .iter()
            .find(|r| r.info.id == "mcp:6")
            .expect("session should exist");
        assert_eq!(
            row.languages,
            vec!["rust-analyzer".to_string()],
            "active_languages_for should return the server from LSP messages"
        );

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
    fn test_sqlite_noise_scoped_by_instance() -> Result<()> {
        let (_dir, path, conn) = test_db();
        let write_conn = crate::db::open_and_migrate_at(&path)?;
        insert_session(&write_conn, "daemon", "/tmp/daemon");

        // Two rust-analyzer instances with different scope_root,
        // each with progress data in the language_servers table.
        crate::db::upsert_server_state(
            &write_conn,
            "daemon",
            "rust",
            "rust-analyzer",
            "root",
            "/home/user/A",
            "busy",
        )?;
        crate::db::update_server_progress(
            &write_conn,
            "daemon",
            "rust",
            "rust-analyzer",
            "root",
            "/home/user/A",
            Some("Indexing"),
            Some(20),
        )?;

        crate::db::upsert_server_state(
            &write_conn,
            "daemon",
            "rust",
            "rust-analyzer",
            "root",
            "/home/user/B",
            "busy",
        )?;
        crate::db::update_server_progress(
            &write_conn,
            "daemon",
            "rust",
            "rust-analyzer",
            "root",
            "/home/user/B",
            Some("Loading"),
            Some(80),
        )?;

        let ds = SqliteDataSource::with_conn(conn);
        let noise = ds.list_server_noise()?;

        // Should return two separate rows — one per (server, scope_root).
        assert_eq!(noise.len(), 2, "expected 2 noise rows, got {}", noise.len());
        assert_eq!(noise[0].scope_root, "/home/user/A");
        assert_eq!(noise[1].scope_root, "/home/user/B");

        // Progress titles should be distinct.
        assert_eq!(noise[0].progress_title.as_deref(), Some("Indexing"));
        assert_eq!(noise[1].progress_title.as_deref(), Some("Loading"));
        assert_eq!(noise[0].progress_pct, Some(20));
        assert_eq!(noise[1].progress_pct, Some(80));

        Ok(())
    }

    #[test]
    fn test_sqlite_list_alive_session_ids() -> Result<()> {
        let (_dir, path, conn) = test_db();
        let write_conn = crate::db::open_and_migrate_at(&path)?;
        insert_session(&write_conn, "mcp:7", "/tmp/test-ds-alive-ids");
        let ds = SqliteDataSource::with_conn(conn);

        // Session is alive (process is running, PID matches current).
        let ids = ds.list_alive_session_ids()?;
        assert!(
            ids.contains(&"mcp:7".to_string()),
            "alive MCP session should appear"
        );

        Ok(())
    }

    #[test]
    fn test_sqlite_shows_only_mcp_sessions() -> Result<()> {
        let (_dir, path, conn) = test_db();
        let write_conn = crate::db::open_and_migrate_at(&path)?;
        insert_session(&write_conn, "daemon", "/tmp/daemon-workspace");
        insert_session(&write_conn, "hook-session-1", "/tmp/hook-workspace");
        insert_session(&write_conn, "mcp:8", "/tmp/mcp-workspace");
        let ds = SqliteDataSource::with_conn(conn);

        // list_alive_session_ids should only include MCP connections.
        let ids = ds.list_alive_session_ids()?;
        assert!(
            !ids.contains(&"daemon".to_string()),
            "daemon should be excluded"
        );
        assert!(
            !ids.contains(&"hook-session-1".to_string()),
            "hook sessions should be excluded"
        );
        assert!(
            ids.contains(&"mcp:8".to_string()),
            "MCP connection should appear"
        );

        // list_sessions should also only include MCP connections.
        let rows = ds.list_sessions()?;
        assert!(
            !rows.iter().any(|r| r.info.id == "daemon"),
            "daemon should not appear in list_sessions"
        );
        assert!(
            !rows.iter().any(|r| r.info.id == "hook-session-1"),
            "hook sessions should not appear in list_sessions"
        );
        assert!(
            rows.iter().any(|r| r.info.id == "mcp:8"),
            "MCP connection should appear in list_sessions"
        );

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
        let msgs = ds.older_scopes(3, 10)?;
        let ids: Vec<i64> = msgs.iter().map(|m| m.id).collect();
        // Should include scope uuid-a (root=1) messages.
        assert!(ids.contains(&1), "should include scope-a root: {ids:?}");
        assert!(ids.contains(&2), "should include scope-a child: {ids:?}");
        // Should NOT include scope-b or standalone.
        assert!(!ids.contains(&3), "should not include scope-b: {ids:?}");
        assert!(!ids.contains(&4), "should not include standalone: {ids:?}");
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
        let msgs = ds.older_scopes(3, 10)?;
        let ids: Vec<i64> = msgs.iter().map(|m| m.id).collect();
        assert!(ids.contains(&1), "should include scope-a root: {ids:?}");
        assert!(ids.contains(&2), "should include scope-a child: {ids:?}");
        assert!(!ids.contains(&3), "should not include scope-b: {ids:?}");

        Ok(())
    }
}
