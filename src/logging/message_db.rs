// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 Mark Wells <contact@markwells.dev>

//! Unified message DB sink.
//!
//! Writes all tracing events — protocol (`kind` in `{"lsp", "mcp", "hook"}`)
//! and internal (everything else) — to the `messages` table. Populates both
//! `type` (boundary) and `level` (severity) columns. Broadcasts the inserted
//! ROWID so `SqliteMessageTail` can stay live.
//!
//! Replaces the former `ProtocolDbSink` + `TraceDbSink` split.

use std::sync::Arc;
use std::sync::Mutex;

use chrono::SecondsFormat;
use chrono::Utc;
use rusqlite::Connection;
use serde_json::Value;
use tokio::sync::broadcast;

use super::LogEvent;
use super::Severity;
use super::Sink;

/// Writes all events to the `messages` table and broadcasts the inserted ROWID.
///
/// Protocol events (`kind` in `{lsp, mcp, hook}`) set `type` from the kind
/// field; internal events set `type = "internal"`. The `level` column always
/// reflects the event's tracing severity.
///
/// DB failures are logged at `trace!` level (not `warn!`, to avoid re-entrant
/// event storms) and the write is dropped. Lock poisoning is recovered via
/// `into_inner` so the sink keeps working after a panic elsewhere.
pub struct MessageDbSink {
    conn: Arc<Mutex<Connection>>,
    instance_id: Arc<str>,
    broadcast: broadcast::Sender<i64>,
}

impl MessageDbSink {
    /// Create a new unified message DB sink.
    #[must_use]
    pub fn new(conn: Arc<Mutex<Connection>>, instance_id: Arc<str>) -> Arc<Self> {
        let (tx, _) = broadcast::channel(256);
        Arc::new(Self {
            conn,
            instance_id,
            broadcast: tx,
        })
    }

    /// Subscribe to ROWID broadcasts (for testing).
    #[cfg(test)]
    #[must_use]
    pub fn subscribe(&self) -> broadcast::Receiver<i64> {
        self.broadcast.subscribe()
    }
}

impl Sink for MessageDbSink {
    fn handle(&self, event: &LogEvent<'_>) {
        let type_val = match event.kind.as_deref() {
            Some("lsp") => "lsp",
            Some("mcp") => "mcp",
            Some("hook") => "hook",
            _ => "internal",
        };

        let level_val = match event.severity {
            Severity::Error => "error",
            Severity::Warn => "warn",
            Severity::Info => "info",
            Severity::Debug => "debug",
        };

        let is_protocol = type_val != "internal";

        // Protocol events use the raw JSON payload; internal events get a
        // structured JSON object preserving the ErrorLayer row shape.
        let payload = if is_protocol {
            event.payload.as_deref().unwrap_or("").to_string()
        } else {
            build_trace_payload(event)
        };

        // Protocol events use the method field; internal events use the
        // tracing target (module path).
        let method = if is_protocol {
            event.method.as_deref().unwrap_or("")
        } else {
            event.target
        };

        // Protocol events use the client field; internal events fix to "catenary".
        let client = if is_protocol {
            event.client.as_deref().unwrap_or("")
        } else {
            "catenary"
        };

        let timestamp = Utc::now().to_rfc3339_opts(SecondsFormat::Millis, true);

        let conn = match self.conn.lock() {
            Ok(guard) => guard,
            Err(poisoned) => {
                tracing::trace!("message_db: mutex poisoned, recovering");
                poisoned.into_inner()
            }
        };

        let insert_result = conn.execute(
            "INSERT INTO messages \
             (session_id, timestamp, type, level, method, server, client, \
              parent_id, scope_root, payload) \
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)",
            rusqlite::params![
                &*self.instance_id,
                timestamp,
                type_val,
                level_val,
                method,
                event.server.as_deref().unwrap_or(""),
                client,
                event.parent_id,
                event.scope_root.as_deref().unwrap_or(""),
                payload,
            ],
        );

        match insert_result {
            Ok(_) => {
                let rowid = conn.last_insert_rowid();
                drop(conn); // release DB lock before broadcasting
                let _ = self.broadcast.send(rowid);
            }
            Err(e) => {
                drop(conn);
                // trace!, not warn!, to avoid re-entrant event storm.
                tracing::trace!(error = %e, "message_db: insert failed");
            }
        }
    }
}

/// Build a JSON payload from an internal (non-protocol) trace event.
///
/// Preserves the existing `ErrorLayer` row shape: `level`, `message`, optional
/// `source`, `language`, and `fields`.
fn build_trace_payload(event: &LogEvent<'_>) -> String {
    let mut obj = serde_json::Map::new();
    obj.insert("level".into(), event.severity.tag().into());
    obj.insert("message".into(), event.message.clone().into());
    if let Some(source) = &event.source {
        obj.insert("source".into(), source.clone().into());
    }
    if let Some(language) = &event.language {
        obj.insert("language".into(), language.clone().into());
    }
    if !event.fields.is_empty() {
        obj.insert("fields".into(), Value::Object(event.fields.clone()));
    }
    Value::Object(obj).to_string()
}

#[cfg(test)]
#[allow(clippy::expect_used, reason = "tests use expect for assertions")]
mod tests {
    use std::sync::Arc;
    use std::sync::Mutex;

    use rusqlite::Connection;

    use super::MessageDbSink;
    use crate::logging::LogEvent;
    use crate::logging::Severity;
    use crate::logging::Sink;

    /// Create an in-memory DB with the messages table schema.
    fn test_db() -> Arc<Mutex<Connection>> {
        let conn = Connection::open_in_memory().expect("open in-memory db");
        conn.execute_batch(
            "CREATE TABLE messages (
                 id          INTEGER PRIMARY KEY AUTOINCREMENT,
                 session_id  TEXT NOT NULL,
                 timestamp   TEXT NOT NULL,
                 type        TEXT NOT NULL,
                 level       TEXT NOT NULL DEFAULT 'info',
                 method      TEXT NOT NULL,
                 server      TEXT NOT NULL,
                 client      TEXT NOT NULL,
                 parent_id   TEXT,
                 scope_root  TEXT NOT NULL DEFAULT '',
                 payload     TEXT NOT NULL
             );",
        )
        .expect("create tables");
        Arc::new(Mutex::new(conn))
    }

    fn make_protocol_event(
        kind: Option<&str>,
        method: Option<&str>,
        server: Option<&str>,
        parent_id: Option<&str>,
        payload: Option<&str>,
    ) -> LogEvent<'static> {
        LogEvent {
            severity: Severity::Info,
            target: "test",
            message: String::new(),
            kind: kind.map(str::to_string),
            method: method.map(str::to_string),
            server: server.map(str::to_string),
            client: None,
            parent_id: parent_id.map(str::to_string),
            source: None,
            language: None,
            payload: payload.map(str::to_string),
            scope_root: None,
            session_id: None,
            fields: serde_json::Map::new(),
        }
    }

    fn make_trace_event<'a>(severity: Severity, target: &'a str, message: &str) -> LogEvent<'a> {
        LogEvent {
            severity,
            target,
            message: message.to_string(),
            kind: None,
            method: None,
            server: None,
            client: None,
            parent_id: None,
            source: None,
            language: None,
            payload: None,
            scope_root: None,
            session_id: None,
            fields: serde_json::Map::new(),
        }
    }

    struct Row {
        session_id: String,
        r#type: String,
        level: String,
        method: String,
        server: String,
        client: String,
        parent_id: Option<String>,
        payload: String,
    }

    #[allow(
        clippy::significant_drop_tightening,
        reason = "MutexGuard must outlive the prepared statement"
    )]
    fn read_rows(conn: &Arc<Mutex<Connection>>) -> Vec<Row> {
        let conn = conn.lock().expect("lock db");
        let mut stmt = conn
            .prepare(
                "SELECT session_id, type, level, method, server, client, \
                 parent_id, payload FROM messages ORDER BY id",
            )
            .expect("prepare select");
        stmt.query_map([], |row| {
            Ok(Row {
                session_id: row.get(0)?,
                r#type: row.get(1)?,
                level: row.get(2)?,
                method: row.get(3)?,
                server: row.get(4)?,
                client: row.get(5)?,
                parent_id: row.get(6)?,
                payload: row.get(7)?,
            })
        })
        .expect("query")
        .collect::<Result<Vec<_>, _>>()
        .expect("collect rows")
    }

    fn row_count(conn: &Arc<Mutex<Connection>>) -> usize {
        read_rows(conn).len()
    }

    // ── Protocol event tests ─────────────────────────────────────────

    #[test]
    fn writes_lsp_event() {
        let db = test_db();
        let sink = MessageDbSink::new(db.clone(), "sess-1".into());
        sink.handle(&make_protocol_event(
            Some("lsp"),
            Some("textDocument/hover"),
            Some("rs"),
            None,
            Some("{}"),
        ));

        let rows = read_rows(&db);
        assert_eq!(rows.len(), 1);
        let r = &rows[0];
        assert_eq!(r.session_id, "sess-1");
        assert_eq!(r.r#type, "lsp");
        assert_eq!(r.level, "info");
        assert_eq!(r.method, "textDocument/hover");
        assert_eq!(r.server, "rs");
        assert_eq!(r.payload, "{}");
    }

    #[test]
    fn writes_mcp_event() {
        let db = test_db();
        let sink = MessageDbSink::new(db.clone(), "sess-1".into());
        sink.handle(&make_protocol_event(
            Some("mcp"),
            Some("tools/call"),
            None,
            None,
            Some("{\"tool\":\"grep\"}"),
        ));

        let rows = read_rows(&db);
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].r#type, "mcp");
        assert_eq!(rows[0].level, "info");
        assert_eq!(rows[0].method, "tools/call");
    }

    #[test]
    fn writes_hook_event() {
        let db = test_db();
        let sink = MessageDbSink::new(db.clone(), "sess-1".into());
        sink.handle(&make_protocol_event(
            Some("hook"),
            Some("post-tool"),
            None,
            None,
            None,
        ));

        let rows = read_rows(&db);
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].r#type, "hook");
        assert_eq!(rows[0].level, "info");
    }

    // ── Internal event tests ─────────────────────────────────────────

    #[test]
    fn writes_internal_event() {
        let db = test_db();
        let sink = MessageDbSink::new(db.clone(), "sess-1".into());
        let mut event = make_trace_event(
            Severity::Warn,
            "crate::logging::tests",
            "something happened",
        );
        event.server = Some("rust-analyzer".into());
        event.source = Some("lsp.lifecycle".into());
        sink.handle(&event);

        let rows = read_rows(&db);
        assert_eq!(rows.len(), 1);
        let r = &rows[0];
        assert_eq!(r.session_id, "sess-1");
        assert_eq!(r.r#type, "internal");
        assert_eq!(r.level, "warn");
        assert_eq!(r.method, "crate::logging::tests");
        assert_eq!(r.server, "rust-analyzer");
        assert_eq!(r.client, "catenary");

        let parsed: serde_json::Value =
            serde_json::from_str(&r.payload).expect("parse payload JSON");
        assert_eq!(parsed["level"], "warn");
        assert_eq!(parsed["message"], "something happened");
        assert_eq!(parsed["source"], "lsp.lifecycle");
    }

    #[test]
    fn unknown_kind_treated_as_internal() {
        let db = test_db();
        let sink = MessageDbSink::new(db.clone(), "sess-1".into());
        let mut event = make_trace_event(Severity::Info, "test", "unknown kind");
        event.kind = Some("catenary".to_string());
        sink.handle(&event);

        let rows = read_rows(&db);
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].r#type, "internal");
    }

    // ── Level column tests ───────────────────────────────────────────

    #[test]
    fn level_column_matches_severity() {
        let db = test_db();
        let sink = MessageDbSink::new(db.clone(), "sess-1".into());

        for (severity, _expected) in [
            (Severity::Debug, "debug"),
            (Severity::Info, "info"),
            (Severity::Warn, "warn"),
            (Severity::Error, "error"),
        ] {
            sink.handle(&make_trace_event(severity, "test", "level check"));
        }

        let rows = read_rows(&db);
        assert_eq!(rows.len(), 4);
        assert_eq!(rows[0].level, "debug");
        assert_eq!(rows[1].level, "info");
        assert_eq!(rows[2].level, "warn");
        assert_eq!(rows[3].level, "error");
    }

    // ── Correlation ID tests ─────────────────────────────────────────

    #[test]
    fn parent_id_stored() {
        let db = test_db();
        let sink = MessageDbSink::new(db.clone(), "sess-1".into());
        sink.handle(&make_protocol_event(
            Some("lsp"),
            Some("hover"),
            None,
            Some("scope-7"),
            None,
        ));

        let rows = read_rows(&db);
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].parent_id.as_deref(), Some("scope-7"));
    }

    #[test]
    fn nullable_parent_id() {
        let db = test_db();
        let sink = MessageDbSink::new(db.clone(), "sess-1".into());
        sink.handle(&make_protocol_event(
            Some("lsp"),
            Some("hover"),
            None,
            None,
            None,
        ));

        let rows = read_rows(&db);
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].parent_id, None);
    }

    #[test]
    fn internal_event_parent_id() {
        let db = test_db();
        let sink = MessageDbSink::new(db.clone(), "sess-1".into());
        let mut event = make_trace_event(Severity::Error, "test", "correlated");
        event.parent_id = Some("scope-7".to_string());
        sink.handle(&event);

        let rows = read_rows(&db);
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].parent_id.as_deref(), Some("scope-7"));
    }

    // ── Payload tests ────────────────────────────────────────────────

    #[test]
    fn internal_payload_includes_fields() {
        let db = test_db();
        let sink = MessageDbSink::new(db.clone(), "sess-1".into());

        let mut event = make_trace_event(Severity::Warn, "test", "with fields");
        event
            .fields
            .insert("foo".into(), serde_json::Value::String("bar".into()));
        event
            .fields
            .insert("count".into(), serde_json::Value::Number(42.into()));
        sink.handle(&event);

        let rows = read_rows(&db);
        assert_eq!(rows.len(), 1);
        let parsed: serde_json::Value =
            serde_json::from_str(&rows[0].payload).expect("parse payload JSON");
        assert_eq!(parsed["fields"]["foo"], "bar");
        assert_eq!(parsed["fields"]["count"], 42);
    }

    #[test]
    fn internal_payload_includes_source_and_language() {
        let db = test_db();
        let sink = MessageDbSink::new(db.clone(), "sess-1".into());
        let mut event = make_trace_event(Severity::Info, "test", "structured");
        event.source = Some("lsp.lifecycle".into());
        event.language = Some("rust".into());
        sink.handle(&event);

        let rows = read_rows(&db);
        assert_eq!(rows.len(), 1);
        let parsed: serde_json::Value =
            serde_json::from_str(&rows[0].payload).expect("parse payload JSON");
        assert_eq!(parsed["source"], "lsp.lifecycle");
        assert_eq!(parsed["language"], "rust");
    }

    // ── Broadcast tests ──────────────────────────────────────────────

    #[test]
    fn broadcasts_rowid_on_insert() {
        let db = test_db();
        let sink = MessageDbSink::new(db, "sess-1".into());
        let mut rx = sink.subscribe();

        sink.handle(&make_protocol_event(
            Some("lsp"),
            Some("hover"),
            None,
            None,
            None,
        ));

        let rowid = rx.try_recv().expect("should receive rowid");
        assert!(rowid > 0);
    }

    #[test]
    fn broadcasts_rowid_for_internal_events() {
        let db = test_db();
        let sink = MessageDbSink::new(db, "sess-1".into());
        let mut rx = sink.subscribe();

        sink.handle(&make_trace_event(Severity::Warn, "test", "internal"));

        let rowid = rx
            .try_recv()
            .expect("should receive rowid for internal event");
        assert!(rowid > 0);
    }

    #[test]
    fn broadcast_skipped_on_insert_failure() {
        let db = test_db();
        let sink = MessageDbSink::new(db.clone(), "sess-1".into());
        let mut rx = sink.subscribe();

        db.lock()
            .expect("lock db")
            .execute_batch("DROP TABLE messages")
            .expect("drop table");

        sink.handle(&make_protocol_event(
            Some("lsp"),
            Some("hover"),
            None,
            None,
            None,
        ));

        assert!(rx.try_recv().is_err(), "no broadcast on insert failure");
    }

    // ── Failure tests ────────────────────────────────────────────────

    #[test]
    fn db_failure_does_not_panic() {
        let db = test_db();
        let sink = MessageDbSink::new(db.clone(), "sess-1".into());

        db.lock()
            .expect("lock db")
            .execute_batch("DROP TABLE messages")
            .expect("drop table");

        // Should not panic — silently drops the write.
        sink.handle(&make_protocol_event(
            Some("lsp"),
            Some("hover"),
            None,
            None,
            None,
        ));
        sink.handle(&make_trace_event(
            Severity::Error,
            "test",
            "should not panic",
        ));
    }

    // ── Mixed event tests ────────────────────────────────────────────

    #[test]
    fn protocol_and_internal_events_coexist() {
        let db = test_db();
        let sink = MessageDbSink::new(db.clone(), "sess-1".into());

        // Protocol events.
        sink.handle(&make_protocol_event(
            Some("lsp"),
            Some("hover"),
            Some("rs"),
            None,
            Some("{}"),
        ));
        sink.handle(&make_protocol_event(
            Some("mcp"),
            Some("tools/call"),
            None,
            None,
            None,
        ));

        // Internal events.
        sink.handle(&make_trace_event(Severity::Warn, "test", "warning"));
        sink.handle(&make_trace_event(Severity::Debug, "test", "debug"));

        let rows = read_rows(&db);
        assert_eq!(rows.len(), 4);

        assert_eq!(rows[0].r#type, "lsp");
        assert_eq!(rows[0].level, "info");

        assert_eq!(rows[1].r#type, "mcp");
        assert_eq!(rows[1].level, "info");

        assert_eq!(rows[2].r#type, "internal");
        assert_eq!(rows[2].level, "warn");
        assert_eq!(rows[2].client, "catenary");

        assert_eq!(rows[3].r#type, "internal");
        assert_eq!(rows[3].level, "debug");
    }

    #[test]
    fn no_events_produces_no_rows() {
        let db = test_db();
        let _sink = MessageDbSink::new(db.clone(), "sess-1".into());
        assert_eq!(row_count(&db), 0);
    }
}
