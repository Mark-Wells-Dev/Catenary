// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 Mark Wells <contact@markwells.dev>

//! Multi-sink tracing dispatcher for Catenary telemetry.
//!
//! [`LoggingServer`] is a [`tracing_subscriber::Layer`] that subscribes to every
//! tracing event in the process and dispatches structured events to registered
//! sinks (notification queue, protocol-message DB, trace DB). It supports
//! two-phase construction: the Layer is installed at binary entry in a buffering
//! state, and [`LoggingServer::activate`] transitions to active once sinks are
//! ready, draining any buffered events through the new sinks.
//!
//! Post-activation, the hot path is lock-free: a single atomic load
//! ([`OnceLock::get`]) reads the sinks slice and dispatches directly. No Mutex,
//! no `Vec` clone, no refcount bumps per event.
//!
//! Each sink call is wrapped in [`std::panic::catch_unwind`]. A panicking sink
//! does not prevent other sinks from receiving the event or crash the caller.
//!
//! This module provides the scaffolding — types, field extraction, and the
//! Layer impl. Concrete sinks are added in subsequent tickets.

pub mod message_db;
pub mod notification_queue;
pub mod notification_router;

use std::collections::VecDeque;
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::OnceLock;
use std::sync::atomic::AtomicI64;
use std::sync::atomic::Ordering;

use chrono::DateTime;
use chrono::Utc;
use tracing::Subscriber;
use tracing_subscriber::layer::Context;
use tracing_subscriber::registry::LookupSpan;

/// In-process monotonic correlation ID.
///
/// Thin newtype over `i64` to prevent accidental conflation with DB ROWIDs
/// during the migration period (tickets 07–10). The inner value is `pub` for
/// ergonomic field access in `tracing` macros.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct CorrelationId(pub i64);

/// Severity of a logging event.
///
/// Mapped from [`tracing::Level`]. `TRACE` collapses into [`Severity::Debug`]
/// because the trace DB and notification queue don't distinguish the two.
/// Ordered so comparisons like `event.severity >= threshold` work directly.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Severity {
    /// Verbose diagnostic — below default notification threshold.
    Debug,
    /// Informational — below default notification threshold.
    Info,
    /// Warning — reaches default notification threshold.
    Warn,
    /// Error — reaches default notification threshold.
    Error,
}

impl Severity {
    /// Short lowercase tag used in notification rendering (`[warn]`, `[err]`).
    #[must_use]
    pub const fn tag(self) -> &'static str {
        match self {
            Self::Debug => "debug",
            Self::Info => "info",
            Self::Warn => "warn",
            Self::Error => "err",
        }
    }
}

impl From<&tracing::Level> for Severity {
    fn from(level: &tracing::Level) -> Self {
        if *level == tracing::Level::ERROR {
            Self::Error
        } else if *level == tracing::Level::WARN {
            Self::Warn
        } else if *level == tracing::Level::INFO {
            Self::Info
        } else {
            // DEBUG and TRACE collapse into Debug.
            Self::Debug
        }
    }
}

/// Emit a protocol event at a compile-time tracing level.
///
/// Tracing macros require the level to be a compile-time constant (`info!`,
/// `debug!`, etc.). This macro accepts the level as an identifier and expands
/// to the corresponding `tracing::$level!` call. The `parent_id` field is
/// optional — tracing macros require static field sets, so the macro branches
/// into two invocations.
///
/// Boundary components (`emit_lsp_event`, `emit_mcp_event`, `emit_hook_event`)
/// fix `kind` and delegate here via a runtime match on [`tracing::Level`].
#[doc(hidden)]
#[macro_export]
macro_rules! emit_protocol_event {
    ($level:ident, kind = $kind:expr, method = $method:expr,
     server = $server:expr, client = $client:expr,
     request_id = $rid:expr, parent_id = $pid:expr,
     payload = $payload:expr, $msg:expr) => {
        if let Some(pid) = $pid {
            tracing::$level!(
                kind = $kind,
                method = $method,
                server = $server,
                client = $client,
                request_id = $rid,
                parent_id = pid,
                payload = $payload,
                $msg
            );
        } else {
            tracing::$level!(
                kind = $kind,
                method = $method,
                server = $server,
                client = $client,
                request_id = $rid,
                payload = $payload,
                $msg
            );
        }
    };
}

/// Structured representation of a single tracing event.
///
/// Sinks receive a `&LogEvent<'_>` after the Layer extracts fields from the
/// raw [`tracing::Event`]. Reserved fields are pulled into typed members; all
/// other structured fields land in [`LogEvent::fields`].
#[derive(Debug)]
pub struct LogEvent<'a> {
    /// Event severity (derived from tracing level).
    pub severity: Severity,
    /// `metadata().target()` — module path of the call site.
    pub target: &'a str,
    /// Rendered `message` field of the event.
    pub message: String,
    /// Protocol routing field: `"lsp"` / `"mcp"` / `"hook"` / absent.
    pub kind: Option<String>,
    /// Protocol method (e.g., `textDocument/hover`).
    pub method: Option<String>,
    /// LSP server name.
    pub server: Option<String>,
    /// Client identifier (host CLI name).
    pub client: Option<String>,
    /// In-process monotonic correlation ID.
    pub request_id: Option<i64>,
    /// Parent correlation ID (causation).
    pub parent_id: Option<i64>,
    /// Subsystem emitting the event (e.g., `"lsp.lifecycle"`).
    pub source: Option<String>,
    /// Language ID when relevant.
    pub language: Option<String>,
    /// Raw protocol JSON payload (for `kind in {lsp, mcp, hook}`).
    pub payload: Option<String>,
    /// Session ID from the tracing span hierarchy.
    ///
    /// Extracted from the nearest ancestor span with a `session_id` field.
    /// Used by [`notification_router::NotificationRouter`] to route
    /// notifications to per-session queues.
    pub session_id: Option<String>,
    /// All non-reserved structured fields.
    pub fields: serde_json::Map<String, serde_json::Value>,
}

/// Owned counterpart of [`LogEvent`] for buffered replay.
///
/// Identical layout except `target` is an owned [`String`]. Used inside the
/// bootstrap buffer so events outlive the originating [`tracing::Event`].
#[derive(Debug)]
struct OwnedEvent {
    severity: Severity,
    target: String,
    message: String,
    kind: Option<String>,
    method: Option<String>,
    server: Option<String>,
    client: Option<String>,
    request_id: Option<i64>,
    parent_id: Option<i64>,
    source: Option<String>,
    language: Option<String>,
    payload: Option<String>,
    session_id: Option<String>,
    fields: serde_json::Map<String, serde_json::Value>,
}

impl OwnedEvent {
    fn as_log_event(&self) -> LogEvent<'_> {
        LogEvent {
            severity: self.severity,
            target: &self.target,
            message: self.message.clone(),
            kind: self.kind.clone(),
            method: self.method.clone(),
            server: self.server.clone(),
            client: self.client.clone(),
            request_id: self.request_id,
            parent_id: self.parent_id,
            source: self.source.clone(),
            language: self.language.clone(),
            payload: self.payload.clone(),
            session_id: self.session_id.clone(),
            fields: self.fields.clone(),
        }
    }
}

/// Destination for dispatched logging events.
///
/// Sinks must be `Send + Sync` because tracing events fire on arbitrary
/// threads. `handle` must not block — it runs on the thread that called
/// the originating tracing macro.
pub trait Sink: Send + Sync {
    /// Handle one logging event.
    fn handle(&self, event: &LogEvent<'_>);
}

/// Queued user-facing notification.
///
/// Produced by the notification queue sink (ticket 01) and drained at
/// stationary hook points (ticket 06) into the host CLI's `systemMessage`.
#[derive(Debug, Clone)]
pub struct Notification {
    /// Severity level.
    pub severity: Severity,
    /// Human-readable message body (without the `[severity]` prefix).
    pub message: String,
    /// When the notification was recorded.
    pub timestamp: DateTime<Utc>,
}

impl Notification {
    /// Format the notification as `[severity] message`.
    #[must_use]
    pub fn format(&self) -> String {
        format!("[{}] {}", self.severity.tag(), self.message)
    }
}

/// Dedup key for the notification queue.
///
/// Two notifications with equal keys collapse into a single queue entry.
/// Combines identity-relevant structured fields with a normalized form of
/// the message body that strips volatile numeric suffixes.
#[derive(Debug, Clone, Hash, PartialEq, Eq)]
pub struct NotificationKey {
    /// `source` field.
    pub source: Option<String>,
    /// `server` field.
    pub server: Option<String>,
    /// `language` field.
    pub language: Option<String>,
    /// Lowercase, digits-stripped, whitespace-collapsed message body.
    pub stem: String,
}

impl NotificationKey {
    /// Build a dedup key from an event.
    #[must_use]
    pub fn from_event(event: &LogEvent<'_>) -> Self {
        Self {
            source: event.source.clone(),
            server: event.server.clone(),
            language: event.language.clone(),
            stem: normalize_stem(&event.message),
        }
    }
}

/// Normalize a message for dedup: lowercase, strip ASCII digits, collapse
/// consecutive whitespace into single spaces, and trim.
fn normalize_stem(message: &str) -> String {
    let mut out = String::with_capacity(message.len());
    let mut prev_space = true;
    for c in message.chars() {
        if c.is_ascii_digit() {
            continue;
        }
        if c.is_whitespace() {
            if !prev_space {
                out.push(' ');
                prev_space = true;
            }
        } else {
            for lc in c.to_lowercase() {
                out.push(lc);
            }
            prev_space = false;
        }
    }
    if out.ends_with(' ') {
        out.pop();
    }
    out
}

/// Default buffer capacity for bootstrap events before activation.
///
/// Sized for ~16× headroom over realistic bootstrap traffic (5–20 events
/// of config load + DB migration). At ~200–500 bytes per [`OwnedEvent`],
/// the cap bounds worst-case memory under a megabyte — small enough to
/// ignore, large enough that a runaway logging bug during bootstrap is
/// the only way to hit it.
const DEFAULT_BUFFER_CAP: usize = 4096;

/// Multi-sink tracing dispatcher. Catenary's telemetry port/adapter.
///
/// Cheaply cloneable: the public type wraps an `Arc<Inner>`, so multiple
/// handles share the same sinks and buffer. Install one clone as the
/// tracing Layer and keep another on [`crate::bridge::Session`] (or
/// equivalent) for post-startup [`LoggingServer::activate`] calls.
///
/// Post-activation, the hot path is a single [`OnceLock::get`] (atomic
/// load) — no Mutex, no cloning.
#[derive(Clone)]
pub struct LoggingServer {
    inner: Arc<Inner>,
}

struct Inner {
    /// Sinks, set once at activation. Lock-free reads via `OnceLock::get`.
    sinks: OnceLock<Arc<[Arc<dyn Sink>]>>,
    /// Bootstrap buffer, `Some` until activation takes it.
    buffer: Mutex<Option<BufferingState>>,
    /// In-process monotonic correlation ID counter. Session-scoped, starts
    /// at 0. `Relaxed` ordering suffices — monotonicity per thread, no
    /// inter-field synchronization.
    next_id: AtomicI64,
}

struct BufferingState {
    buffer: VecDeque<OwnedEvent>,
    cap: usize,
    dropped: u32,
}

impl LoggingServer {
    /// Construct in buffering state with the default buffer cap (4096).
    #[must_use]
    pub fn new() -> Self {
        Self::with_buffer_cap(DEFAULT_BUFFER_CAP)
    }

    /// Construct in buffering state with a custom buffer cap.
    #[must_use]
    pub fn with_buffer_cap(cap: usize) -> Self {
        Self {
            inner: Arc::new(Inner {
                sinks: OnceLock::new(),
                buffer: Mutex::new(Some(BufferingState {
                    buffer: VecDeque::new(),
                    cap,
                    dropped: 0,
                })),
                next_id: AtomicI64::new(0),
            }),
        }
    }

    /// Transition to active, draining buffered events through `sinks` in
    /// FIFO order. Subsequent events dispatch directly to `sinks`.
    /// Idempotent: calling `activate` on an already-active server is a
    /// no-op.
    ///
    /// If bootstrap events were dropped due to buffer overflow, a
    /// `warn!()` is emitted after the drain describing the loss. That
    /// event flows through the now-active sinks like any other.
    pub fn activate(&self, sinks: Vec<Arc<dyn Sink>>) {
        let sinks_arc: Arc<[Arc<dyn Sink>]> = sinks.into();

        // Set sinks first — enables the fast path in on_event. OnceLock
        // returns Err if already set (idempotent activate).
        if self.inner.sinks.set(sinks_arc).is_err() {
            return;
        }

        // Re-read from OnceLock for the drain. (We just set it — this
        // cannot be None.)
        let Some(sinks) = self.inner.sinks.get() else {
            return;
        };

        // Take the buffer. Events arriving after sinks.set() use the
        // fast path. Events already holding the buffer lock finish their
        // push; we take the accumulated contents here.
        let buffered = lock_recover(&self.inner.buffer).take();

        if let Some(bs) = buffered {
            for owned in &bs.buffer {
                let log_event = owned.as_log_event();
                dispatch_to_sinks(sinks, &log_event);
            }

            if bs.dropped > 0 {
                tracing::warn!(
                    source = crate::source::Source::LoggingBootstrap.as_str(),
                    dropped = i64::from(bs.dropped),
                    "{} bootstrap events dropped (buffer overflow)",
                    bs.dropped,
                );
            }
        }
    }

    /// Number of registered sinks. Returns 0 before activation.
    #[must_use]
    pub fn sink_count(&self) -> usize {
        self.inner.sinks.get().map_or(0, |s| s.len())
    }

    /// Buffered event count (test/diagnostic accessor). Returns 0 after
    /// activation.
    #[must_use]
    pub fn buffered_len(&self) -> usize {
        lock_recover(&self.inner.buffer)
            .as_ref()
            .map_or(0, |bs| bs.buffer.len())
    }

    /// Mint a new correlation ID. Values are monotonically increasing,
    /// session-scoped, and unique across threads.
    ///
    /// Used by protocol-boundary components (tickets 07–09) to pair
    /// requests with responses (`request_id`) and build causation chains
    /// (`parent_id`).
    #[must_use]
    pub fn next_id(&self) -> CorrelationId {
        CorrelationId(self.inner.next_id.fetch_add(1, Ordering::Relaxed))
    }
}

impl Default for LoggingServer {
    fn default() -> Self {
        Self::new()
    }
}

/// Recover from Mutex poisoning by taking the inner guard.
///
/// A poisoned Mutex must still be usable so logging continues for
/// subsequent events. The guarded state is small (bootstrap buffer,
/// panic slot); partially-applied changes are tolerable.
fn lock_recover<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
}

/// Dispatch an event to sinks with per-sink panic isolation.
///
/// Each sink call is wrapped in [`std::panic::catch_unwind`]. A panicking
/// sink does not prevent other sinks from receiving the event or crash the
/// caller.
fn dispatch_to_sinks(sinks: &[Arc<dyn Sink>], event: &LogEvent<'_>) {
    for sink in sinks {
        let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| sink.handle(event)));
    }
}

/// Span extension for `session_id` propagation.
///
/// Stored in each span's extensions when it (or an ancestor) carries a
/// `session_id` field. The [`LoggingServer`] Layer populates this on
/// `on_new_span` and `on_record`, then reads it in `on_event` to attach
/// the session context to [`LogEvent`].
#[derive(Default)]
struct SpanFields {
    session_id: Option<String>,
}

/// Visitor for extracting `session_id` from span attributes.
#[derive(Default)]
struct SpanFieldVisitor {
    session_id: Option<String>,
}

impl tracing::field::Visit for SpanFieldVisitor {
    fn record_str(&mut self, field: &tracing::field::Field, value: &str) {
        if field.name() == "session_id" {
            self.session_id = Some(value.to_string());
        }
    }

    fn record_debug(&mut self, field: &tracing::field::Field, value: &dyn std::fmt::Debug) {
        if field.name() == "session_id" {
            self.session_id = Some(format!("{value:?}"));
        }
    }
}

impl<S> tracing_subscriber::Layer<S> for LoggingServer
where
    S: Subscriber + for<'a> LookupSpan<'a>,
{
    fn on_new_span(
        &self,
        attrs: &tracing::span::Attributes<'_>,
        id: &tracing::span::Id,
        ctx: Context<'_, S>,
    ) {
        let mut visitor = SpanFieldVisitor::default();
        attrs.record(&mut visitor);
        if let Some(span) = ctx.span(id) {
            span.extensions_mut().insert(SpanFields {
                session_id: visitor.session_id,
            });
        }
    }

    fn on_record(
        &self,
        id: &tracing::span::Id,
        values: &tracing::span::Record<'_>,
        ctx: Context<'_, S>,
    ) {
        let mut visitor = SpanFieldVisitor::default();
        values.record(&mut visitor);
        if let Some(session_id) = visitor.session_id
            && let Some(span) = ctx.span(id)
        {
            let mut ext = span.extensions_mut();
            if let Some(fields) = ext.get_mut::<SpanFields>() {
                fields.session_id = Some(session_id);
            }
        }
    }

    fn on_event(&self, event: &tracing::Event<'_>, ctx: Context<'_, S>) {
        let meta = event.metadata();
        let severity = Severity::from(meta.level());
        let target = meta.target();

        let mut visitor = FieldVisitor::default();
        event.record(&mut visitor);

        // Walk the span chain to find session_id if not on the event.
        let span_session_id = if visitor.session_id.is_some() {
            None // event field takes priority in finish()
        } else {
            ctx.event_span(event).and_then(|span| {
                // Check current span first, then walk ancestors.
                span.scope().find_map(|s| {
                    s.extensions()
                        .get::<SpanFields>()
                        .and_then(|f| f.session_id.clone())
                })
            })
        };

        // Fast path: sinks set (post-activate). Single atomic load — no
        // Mutex, no Vec clone, no refcount bumps.
        if let Some(sinks) = self.inner.sinks.get() {
            let log_event = visitor.finish(severity, target, span_session_id);
            dispatch_to_sinks(sinks, &log_event);
            return;
        }

        // Slow path: buffer (pre-activate bootstrap).
        let mut guard = lock_recover(&self.inner.buffer);

        // Double-check: activate may have run between OnceLock check
        // and lock acquisition.
        if let Some(sinks) = self.inner.sinks.get() {
            drop(guard);
            let log_event = visitor.finish(severity, target, span_session_id);
            dispatch_to_sinks(sinks, &log_event);
            return;
        }

        if let Some(bs) = guard.as_mut() {
            let mut owned = visitor.finish_owned(severity, target.to_string());
            // Bootstrap events lack span context; merge if available.
            if owned.session_id.is_none() {
                owned.session_id = span_session_id;
            }
            if bs.buffer.len() >= bs.cap {
                let _ = bs.buffer.pop_front();
                bs.dropped = bs.dropped.saturating_add(1);
            }
            bs.buffer.push_back(owned);
        }
    }
}

/// Field extractor for tracing events.
///
/// Reserved field names (`message`, `kind`, `method`, `server`, `client`,
/// `source`, `language`, `payload`, `request_id`, `parent_id`) populate
/// typed members. All other fields collect into `fields`.
#[derive(Default)]
struct FieldVisitor {
    message: String,
    kind: Option<String>,
    method: Option<String>,
    server: Option<String>,
    client: Option<String>,
    request_id: Option<i64>,
    parent_id: Option<i64>,
    source: Option<String>,
    language: Option<String>,
    payload: Option<String>,
    session_id: Option<String>,
    fields: serde_json::Map<String, serde_json::Value>,
}

impl FieldVisitor {
    fn set_str(&mut self, name: &str, value: String) {
        match name {
            "message" => self.message = value,
            "kind" => self.kind = Some(value),
            "method" => self.method = Some(value),
            "server" => self.server = Some(value),
            "client" => self.client = Some(value),
            "source" => self.source = Some(value),
            "language" => self.language = Some(value),
            "payload" => self.payload = Some(value),
            "session_id" => self.session_id = Some(value),
            _ => {
                self.fields
                    .insert(name.to_string(), serde_json::Value::String(value));
            }
        }
    }

    fn finish(
        self,
        severity: Severity,
        target: &str,
        span_session_id: Option<String>,
    ) -> LogEvent<'_> {
        LogEvent {
            severity,
            target,
            message: self.message,
            kind: self.kind,
            method: self.method,
            server: self.server,
            client: self.client,
            request_id: self.request_id,
            parent_id: self.parent_id,
            source: self.source,
            language: self.language,
            payload: self.payload,
            // Event field takes priority; fall back to span context.
            session_id: self.session_id.or(span_session_id),
            fields: self.fields,
        }
    }

    fn finish_owned(self, severity: Severity, target: String) -> OwnedEvent {
        OwnedEvent {
            severity,
            target,
            message: self.message,
            kind: self.kind,
            method: self.method,
            server: self.server,
            client: self.client,
            request_id: self.request_id,
            parent_id: self.parent_id,
            source: self.source,
            language: self.language,
            payload: self.payload,
            session_id: self.session_id,
            fields: self.fields,
        }
    }
}

impl tracing::field::Visit for FieldVisitor {
    fn record_str(&mut self, field: &tracing::field::Field, value: &str) {
        self.set_str(field.name(), value.to_string());
    }

    fn record_debug(&mut self, field: &tracing::field::Field, value: &dyn std::fmt::Debug) {
        self.set_str(field.name(), format!("{value:?}"));
    }

    fn record_i64(&mut self, field: &tracing::field::Field, value: i64) {
        match field.name() {
            "request_id" => self.request_id = Some(value),
            "parent_id" => self.parent_id = Some(value),
            name => {
                self.fields
                    .insert(name.to_string(), serde_json::Value::Number(value.into()));
            }
        }
    }

    fn record_u64(&mut self, field: &tracing::field::Field, value: u64) {
        // request_id / parent_id accept u64 but store as i64. Values
        // exceeding i64::MAX fall through to the generic map.
        if let Ok(as_i64) = i64::try_from(value) {
            match field.name() {
                "request_id" => {
                    self.request_id = Some(as_i64);
                    return;
                }
                "parent_id" => {
                    self.parent_id = Some(as_i64);
                    return;
                }
                _ => {}
            }
        }
        self.fields.insert(
            field.name().to_string(),
            serde_json::Value::Number(value.into()),
        );
    }

    fn record_bool(&mut self, field: &tracing::field::Field, value: bool) {
        self.fields
            .insert(field.name().to_string(), serde_json::Value::Bool(value));
    }
}

/// Shared logging test helpers.
///
/// Gated on `feature = "mockls"` so both in-crate unit tests and
/// integration tests (which compile with `--features mockls`) share a
/// single schema definition and query helpers.
#[cfg(feature = "mockls")]
#[doc(hidden)]
#[allow(
    clippy::expect_used,
    clippy::missing_panics_doc,
    clippy::missing_errors_doc,
    clippy::must_use_candidate,
    missing_docs,
    reason = "test-only module, doc lints are noise"
)]
pub mod test_support {
    use std::sync::Arc;
    use std::sync::Mutex;

    use tracing_subscriber::layer::SubscriberExt;

    use super::LoggingServer;
    use super::message_db::MessageDbSink;

    /// Row projection from the `messages` table.
    pub struct MsgRow {
        pub r#type: String,
        pub level: String,
        pub method: String,
        pub client: String,
        pub request_id: Option<i64>,
        pub parent_id: Option<i64>,
    }

    /// Create an in-memory DB with `sessions` and `messages` table schema.
    pub fn logging_test_db() -> Arc<Mutex<rusqlite::Connection>> {
        let conn = rusqlite::Connection::open_in_memory().expect("open in-memory db");
        conn.execute_batch(
            "CREATE TABLE sessions (
                 id           TEXT PRIMARY KEY,
                 pid          INTEGER NOT NULL,
                 display_name TEXT NOT NULL,
                 started_at   TEXT NOT NULL
             );
             INSERT INTO sessions (id, pid, display_name, started_at)
                 VALUES ('s1', 1, 'test', '2026-01-01T00:00:00Z');
             CREATE TABLE messages (
                 id          INTEGER PRIMARY KEY AUTOINCREMENT,
                 session_id  TEXT NOT NULL,
                 timestamp   TEXT NOT NULL,
                 type        TEXT NOT NULL,
                 level       TEXT NOT NULL DEFAULT 'info',
                 method      TEXT NOT NULL,
                 server      TEXT NOT NULL,
                 client      TEXT NOT NULL,
                 request_id  INTEGER,
                 parent_id   INTEGER,
                 payload     TEXT NOT NULL
             );",
        )
        .expect("create schema");
        Arc::new(Mutex::new(conn))
    }

    /// Create a `LoggingServer` with `MessageDbSink` backed by an in-memory
    /// DB, installed as the thread-local tracing subscriber.
    pub fn setup_logging() -> (
        LoggingServer,
        Arc<Mutex<rusqlite::Connection>>,
        tracing::subscriber::DefaultGuard,
    ) {
        let conn = logging_test_db();

        let logging = LoggingServer::new();
        let message_db = MessageDbSink::new(conn.clone(), "s1".into());
        logging.activate(vec![message_db]);

        let subscriber = tracing_subscriber::registry().with(logging.clone());
        let guard = tracing::subscriber::set_default(subscriber);

        (logging, conn, guard)
    }

    /// Query all messages ordered by id.
    #[allow(
        clippy::significant_drop_tightening,
        reason = "MutexGuard must outlive the prepared statement"
    )]
    pub fn query_all_messages(conn: &Arc<Mutex<rusqlite::Connection>>) -> Vec<MsgRow> {
        let c = conn.lock().expect("lock");
        c.prepare(
            "SELECT type, level, method, client, request_id, parent_id \
             FROM messages ORDER BY id",
        )
        .expect("prepare")
        .query_map([], |row| {
            Ok(MsgRow {
                r#type: row.get(0)?,
                level: row.get(1)?,
                method: row.get(2)?,
                client: row.get(3)?,
                request_id: row.get(4)?,
                parent_id: row.get(5)?,
            })
        })
        .expect("query")
        .filter_map(std::result::Result::ok)
        .collect()
    }

    /// Query protocol messages only (`type IN ('lsp', 'mcp', 'hook')`).
    pub fn query_protocol_messages(conn: &Arc<Mutex<rusqlite::Connection>>) -> Vec<MsgRow> {
        query_all_messages(conn)
            .into_iter()
            .filter(|m| matches!(m.r#type.as_str(), "lsp" | "mcp" | "hook"))
            .collect()
    }

    /// Count total rows in the `messages` table.
    pub fn message_count(conn: &Arc<Mutex<rusqlite::Connection>>) -> i64 {
        let c = conn.lock().expect("lock");
        c.query_row("SELECT COUNT(*) FROM messages", [], |row| row.get(0))
            .expect("count messages")
    }

    /// Spawn mockls with a `LoggingServer` and initialize it.
    ///
    /// Callers pass the mockls binary path via `env!("CARGO_BIN_EXE_mockls")`
    /// (only available in integration test binaries, not library code).
    pub async fn spawn_initialized_client(
        bin: &str,
        logging: LoggingServer,
        lang: &str,
    ) -> anyhow::Result<(crate::lsp::LspClient, tempfile::TempDir)> {
        let dir = tempfile::tempdir()?;

        let mut client = crate::lsp::LspClient::spawn(
            bin,
            &[lang],
            lang,
            lang,
            logging,
            None,
            std::collections::HashMap::new(),
            None,
        )?;

        client.initialize(&[dir.path().to_path_buf()], None).await?;
        Ok((client, dir))
    }
}

#[cfg(test)]
#[allow(
    clippy::expect_used,
    clippy::panic,
    reason = "tests use expect for assertions and panic for deliberate test sinks"
)]
mod tests {
    use super::LogEvent;
    use super::LoggingServer;
    use super::Notification;
    use super::NotificationKey;
    use super::Severity;
    use super::Sink;
    use super::normalize_stem;
    use std::sync::Arc;
    use std::sync::Mutex;
    use tracing_subscriber::layer::SubscriberExt;

    /// Sink that records a configurable projection of every event it sees.
    #[derive(Default)]
    struct RecorderSink {
        events: Mutex<Vec<RecordedEvent>>,
    }

    #[derive(Debug, Clone)]
    struct RecordedEvent {
        severity: Severity,
        message: String,
        kind: Option<String>,
        method: Option<String>,
        server: Option<String>,
        client: Option<String>,
        request_id: Option<i64>,
        parent_id: Option<i64>,
        source: Option<String>,
        language: Option<String>,
        payload: Option<String>,
        session_id: Option<String>,
        fields: serde_json::Map<String, serde_json::Value>,
    }

    impl RecorderSink {
        fn snapshot(&self) -> Vec<RecordedEvent> {
            self.events.lock().expect("lock recorder").clone()
        }
    }

    impl Sink for RecorderSink {
        fn handle(&self, event: &LogEvent<'_>) {
            self.events
                .lock()
                .expect("lock recorder")
                .push(RecordedEvent {
                    severity: event.severity,
                    message: event.message.clone(),
                    kind: event.kind.clone(),
                    method: event.method.clone(),
                    server: event.server.clone(),
                    client: event.client.clone(),
                    request_id: event.request_id,
                    parent_id: event.parent_id,
                    source: event.source.clone(),
                    language: event.language.clone(),
                    payload: event.payload.clone(),
                    session_id: event.session_id.clone(),
                    fields: event.fields.clone(),
                });
        }
    }

    fn make_event(message: &str, source: Option<&str>, server: Option<&str>) -> LogEvent<'static> {
        LogEvent {
            severity: Severity::Warn,
            target: "test",
            message: message.to_string(),
            kind: None,
            method: None,
            server: server.map(str::to_string),
            client: None,
            request_id: None,
            parent_id: None,
            source: source.map(str::to_string),
            language: None,
            payload: None,
            session_id: None,
            fields: serde_json::Map::new(),
        }
    }

    fn with_subscriber<F: FnOnce()>(server: LoggingServer, f: F) {
        let subscriber = tracing_subscriber::registry().with(server);
        tracing::subscriber::with_default(subscriber, f);
    }

    #[test]
    fn severity_from_level() {
        assert_eq!(Severity::from(&tracing::Level::ERROR), Severity::Error);
        assert_eq!(Severity::from(&tracing::Level::WARN), Severity::Warn);
        assert_eq!(Severity::from(&tracing::Level::INFO), Severity::Info);
        assert_eq!(Severity::from(&tracing::Level::DEBUG), Severity::Debug);
        assert_eq!(Severity::from(&tracing::Level::TRACE), Severity::Debug);
    }

    #[test]
    fn severity_ordering() {
        assert!(Severity::Error > Severity::Warn);
        assert!(Severity::Warn > Severity::Info);
        assert!(Severity::Info > Severity::Debug);
    }

    #[test]
    fn severity_tag_values() {
        assert_eq!(Severity::Debug.tag(), "debug");
        assert_eq!(Severity::Info.tag(), "info");
        assert_eq!(Severity::Warn.tag(), "warn");
        assert_eq!(Severity::Error.tag(), "err");
    }

    #[test]
    fn notification_format() {
        let n = Notification {
            severity: Severity::Warn,
            message: "rust-analyzer offline".into(),
            timestamp: chrono::Utc::now(),
        };
        assert_eq!(n.format(), "[warn] rust-analyzer offline");
    }

    #[test]
    fn normalize_stem_strips_digits_and_collapses_whitespace() {
        assert_eq!(
            normalize_stem("Fetch Failed 42 times"),
            "fetch failed times"
        );
        assert_eq!(normalize_stem("  HI    there  "), "hi there");
        assert_eq!(normalize_stem("123"), "");
        assert_eq!(normalize_stem(""), "");
    }

    #[test]
    fn notification_key_dedups_numeric_variance() {
        let e1 = make_event(
            "rust-analyzer crashed 3 times",
            Some("lsp.lifecycle"),
            Some("rust-analyzer"),
        );
        let e2 = make_event(
            "rust-analyzer crashed 5 times",
            Some("lsp.lifecycle"),
            Some("rust-analyzer"),
        );
        assert_eq!(
            NotificationKey::from_event(&e1),
            NotificationKey::from_event(&e2)
        );
    }

    #[test]
    fn notification_key_different_server_not_deduped() {
        let e1 = make_event("server crashed", None, Some("rust-analyzer"));
        let e2 = make_event("server crashed", None, Some("pylsp"));
        assert_ne!(
            NotificationKey::from_event(&e1),
            NotificationKey::from_event(&e2)
        );
    }

    #[test]
    fn layer_in_buffering_mode_stores_events() {
        let server = LoggingServer::new();
        with_subscriber(server.clone(), || {
            tracing::warn!("first");
            tracing::info!("second");
        });
        assert_eq!(server.buffered_len(), 2);
        assert_eq!(server.sink_count(), 0);
    }

    #[test]
    fn activate_drains_buffered_events_in_order() {
        let server = LoggingServer::new();
        let sink = Arc::new(RecorderSink::default());
        with_subscriber(server.clone(), || {
            tracing::warn!("first");
            tracing::warn!("second");
            tracing::warn!("third");
            server.activate(vec![sink.clone()]);
        });
        let events = sink.snapshot();
        assert_eq!(events.len(), 3);
        assert_eq!(events[0].message, "first");
        assert_eq!(events[1].message, "second");
        assert_eq!(events[2].message, "third");
    }

    #[test]
    fn activate_then_live_events_dispatch_directly() {
        let server = LoggingServer::new();
        let sink = Arc::new(RecorderSink::default());
        with_subscriber(server.clone(), || {
            server.activate(vec![sink.clone()]);
            tracing::warn!("after activate");
        });
        let events = sink.snapshot();
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].message, "after activate");
        assert_eq!(server.sink_count(), 1);
        assert_eq!(server.buffered_len(), 0);
    }

    #[test]
    fn buffer_overflow_drops_oldest_and_emits_warning() {
        let cap = 4;
        let server = LoggingServer::with_buffer_cap(cap);
        let sink = Arc::new(RecorderSink::default());
        with_subscriber(server.clone(), || {
            for i in 0..10 {
                tracing::warn!("event {}", i);
            }
            server.activate(vec![sink.clone()]);
        });
        let events = sink.snapshot();
        // 4 events from the buffer (the most recent 4: 6..=9) + 1 synthetic
        // warn! emitted by activate to describe the drop.
        assert_eq!(events.len(), cap + 1);
        assert_eq!(events[0].message, "event 6");
        assert_eq!(events[1].message, "event 7");
        assert_eq!(events[2].message, "event 8");
        assert_eq!(events[3].message, "event 9");
        assert_eq!(events[cap].severity, Severity::Warn);
        assert_eq!(events[cap].source.as_deref(), Some("logging.bootstrap"));
    }

    #[test]
    fn activate_is_idempotent() {
        let server = LoggingServer::new();
        let sink_a = Arc::new(RecorderSink::default());
        let sink_b = Arc::new(RecorderSink::default());
        with_subscriber(server.clone(), || {
            server.activate(vec![sink_a.clone()]);
            server.activate(vec![sink_b.clone()]);
            tracing::warn!("after");
        });
        // sink_b was ignored because activate is idempotent
        assert_eq!(sink_a.snapshot().len(), 1);
        assert_eq!(sink_b.snapshot().len(), 0);
    }

    #[test]
    fn layer_dispatches_to_all_sinks_in_order() {
        let server = LoggingServer::new();
        let sink_a = Arc::new(RecorderSink::default());
        let sink_b = Arc::new(RecorderSink::default());
        with_subscriber(server.clone(), || {
            server.activate(vec![sink_a.clone(), sink_b.clone()]);
            tracing::warn!("one");
            tracing::info!("two");
        });
        assert_eq!(sink_a.snapshot().len(), 2);
        assert_eq!(sink_b.snapshot().len(), 2);
        assert_eq!(sink_a.snapshot()[0].message, "one");
        assert_eq!(sink_b.snapshot()[1].message, "two");
    }

    #[test]
    fn layer_extracts_all_reserved_fields() {
        let server = LoggingServer::new();
        let sink = Arc::new(RecorderSink::default());
        with_subscriber(server.clone(), || {
            server.activate(vec![sink.clone()]);
            tracing::info!(
                kind = "lsp",
                method = "textDocument/hover",
                server = "rust-analyzer",
                client = "claude-code",
                request_id = 7_i64,
                parent_id = 3_i64,
                source = crate::source::Source::LspDispatch.as_str(),
                language = "rust",
                payload = "{}",
                "outgoing"
            );
        });
        let events = sink.snapshot();
        assert_eq!(events.len(), 1);
        let e = &events[0];
        assert_eq!(e.kind.as_deref(), Some("lsp"));
        assert_eq!(e.method.as_deref(), Some("textDocument/hover"));
        assert_eq!(e.server.as_deref(), Some("rust-analyzer"));
        assert_eq!(e.client.as_deref(), Some("claude-code"));
        assert_eq!(e.request_id, Some(7));
        assert_eq!(e.parent_id, Some(3));
        assert_eq!(
            e.source.as_deref(),
            Some(crate::source::Source::LspDispatch.as_str())
        );
        assert_eq!(e.language.as_deref(), Some("rust"));
        assert_eq!(e.payload.as_deref(), Some("{}"));
        assert_eq!(e.message, "outgoing");
    }

    #[test]
    fn unreserved_fields_land_in_fields_map() {
        let server = LoggingServer::new();
        let sink = Arc::new(RecorderSink::default());
        with_subscriber(server.clone(), || {
            server.activate(vec![sink.clone()]);
            tracing::warn!(extra = "value", count = 5_i64, ok = true, "event");
        });
        let events = sink.snapshot();
        assert_eq!(events.len(), 1);
        let f = &events[0].fields;
        assert_eq!(f["extra"], "value");
        assert_eq!(f["count"], 5);
        assert_eq!(f["ok"], true);
    }

    #[test]
    fn debug_formatted_value_preserved() {
        let server = LoggingServer::new();
        let sink = Arc::new(RecorderSink::default());
        with_subscriber(server.clone(), || {
            server.activate(vec![sink.clone()]);
            let value = vec![1_u32, 2, 3];
            tracing::warn!(items = ?value, "debug-fmt");
        });
        let events = sink.snapshot();
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].fields["items"], "[1, 2, 3]");
    }

    #[test]
    fn concurrent_emit_during_drain() {
        // Sink with a per-handle delay to widen the drain window so the
        // concurrent thread can interleave.
        struct SlowSink {
            inner: Arc<RecorderSink>,
            delay_us: u64,
        }
        impl Sink for SlowSink {
            fn handle(&self, event: &LogEvent<'_>) {
                std::thread::sleep(std::time::Duration::from_micros(self.delay_us));
                self.inner.handle(event);
            }
        }

        let server = LoggingServer::new();
        let recorder = Arc::new(RecorderSink::default());
        let slow = Arc::new(SlowSink {
            inner: recorder.clone(),
            delay_us: 200,
        });

        // Dispatch propagates into spawned threads; `with_default` only
        // sets a thread-local subscriber for the current thread.
        let subscriber = tracing_subscriber::registry().with(server.clone());
        let dispatch = tracing::Dispatch::new(subscriber);
        tracing::dispatcher::with_default(&dispatch, || {
            // Fill buffer with 20 events before activation.
            for i in 0..20 {
                tracing::warn!(idx = i64::from(i), "buf");
            }
            let server_for_thread = server.clone();
            let dispatch_for_thread = dispatch.clone();
            let thread = std::thread::spawn(move || {
                tracing::dispatcher::with_default(&dispatch_for_thread, || {
                    // Emit while drain is happening. These see Active
                    // state and dispatch directly to sinks.
                    for i in 0..10 {
                        tracing::warn!(idx = i64::from(i), "live");
                    }
                });
                drop(server_for_thread);
            });
            server.activate(vec![slow.clone()]);
            thread.join().expect("join concurrent emitter");
            // Flush any trailing live events.
            tracing::warn!("final");
        });

        let events = recorder.snapshot();
        // Count is deterministic: 20 "buf" + 10 "live" + 1 "final" = 31.
        // No drops (well under the 4096 cap), one-way sinks transition
        // means every emit path reaches a sink exactly once, and
        // thread.join() guarantees all live events fire before we check.
        let buf = events.iter().filter(|e| e.message == "buf").count();
        let live = events.iter().filter(|e| e.message == "live").count();
        let final_count = events.iter().filter(|e| e.message == "final").count();
        assert_eq!(buf, 20, "buffered events: {events:?}");
        assert_eq!(live, 10, "live events: {events:?}");
        assert_eq!(final_count, 1, "final events: {events:?}");
        assert_eq!(events.len(), 31);
    }

    #[test]
    fn sink_count_and_buffered_len_reflect_mode() {
        let server = LoggingServer::new();
        assert_eq!(server.sink_count(), 0);
        assert_eq!(server.buffered_len(), 0);
        with_subscriber(server.clone(), || {
            tracing::warn!("buffered");
            assert_eq!(server.buffered_len(), 1);
            let sink = Arc::new(RecorderSink::default());
            server.activate(vec![sink]);
            assert_eq!(server.sink_count(), 1);
            assert_eq!(server.buffered_len(), 0);
        });
    }

    #[test]
    fn panicking_sink_does_not_block_other_sinks() {
        struct PanicSink;
        impl Sink for PanicSink {
            fn handle(&self, _event: &LogEvent<'_>) {
                panic!("sink exploded");
            }
        }

        let server = LoggingServer::new();
        let panic_sink: Arc<dyn Sink> = Arc::new(PanicSink);
        let recorder = Arc::new(RecorderSink::default());
        with_subscriber(server.clone(), || {
            server.activate(vec![panic_sink, recorder.clone()]);
            tracing::warn!("survives");
        });
        let events = recorder.snapshot();
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].message, "survives");
    }

    #[test]
    fn correlation_next_id_is_monotonic() {
        let server = LoggingServer::new();
        assert_eq!(server.next_id(), super::CorrelationId(0));
        assert_eq!(server.next_id(), super::CorrelationId(1));
        assert_eq!(server.next_id(), super::CorrelationId(2));
    }

    #[test]
    fn correlation_next_id_is_thread_safe() {
        let server = LoggingServer::new();
        std::thread::scope(|s| {
            for _ in 0..4 {
                s.spawn(|| {
                    for _ in 0..1000 {
                        let _ = server.next_id();
                    }
                });
            }
        });
        assert_eq!(server.next_id(), super::CorrelationId(4000));
    }

    #[test]
    fn correlation_counter_survives_activation() {
        let server = LoggingServer::new();
        let _ = server.next_id(); // 0
        let _ = server.next_id(); // 1
        let sink = Arc::new(RecorderSink::default());
        server.activate(vec![sink]);
        assert_eq!(server.next_id(), super::CorrelationId(2));
    }

    #[test]
    fn multi_sink_independence_through_layer() {
        // Verify through the tracing Layer path that a panicking sink
        // does not prevent other sinks from receiving the event.
        struct PanicOnceSink {
            inner: Arc<RecorderSink>,
            panicked: std::sync::atomic::AtomicBool,
        }
        impl Sink for PanicOnceSink {
            fn handle(&self, event: &LogEvent<'_>) {
                if !self
                    .panicked
                    .swap(true, std::sync::atomic::Ordering::Relaxed)
                {
                    panic!("first-event panic");
                }
                self.inner.handle(event);
            }
        }

        let server = LoggingServer::new();
        let panic_recorder = Arc::new(RecorderSink::default());
        let panic_sink: Arc<dyn Sink> = Arc::new(PanicOnceSink {
            inner: panic_recorder.clone(),
            panicked: std::sync::atomic::AtomicBool::new(false),
        });
        let healthy = Arc::new(RecorderSink::default());

        with_subscriber(server.clone(), || {
            server.activate(vec![panic_sink, healthy.clone()]);
            tracing::warn!("event-1"); // panicking sink panics
            tracing::warn!("event-2"); // panicking sink records
        });

        // Healthy sink received both events regardless of the panic.
        assert_eq!(healthy.snapshot().len(), 2);
        // Panicking sink missed event-1 but got event-2.
        assert_eq!(panic_recorder.snapshot().len(), 1);
        assert_eq!(panic_recorder.snapshot()[0].message, "event-2");
    }

    #[test]
    fn session_id_from_span_propagates_to_event() {
        let server = LoggingServer::new();
        let sink = Arc::new(RecorderSink::default());
        with_subscriber(server.clone(), || {
            server.activate(vec![sink.clone()]);

            let span = tracing::info_span!("test_session", session_id = "sess-42");
            let _guard = span.enter();
            tracing::warn!("inside span");
        });
        let events = sink.snapshot();
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].session_id.as_deref(), Some("sess-42"));
    }

    #[test]
    fn session_id_from_ancestor_span() {
        let server = LoggingServer::new();
        let sink = Arc::new(RecorderSink::default());
        with_subscriber(server.clone(), || {
            server.activate(vec![sink.clone()]);

            let outer = tracing::info_span!("connection", session_id = "sess-99");
            let _outer_guard = outer.enter();

            let inner = tracing::info_span!("tool_call");
            let _inner_guard = inner.enter();
            tracing::warn!("deep inside");
        });
        let events = sink.snapshot();
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].session_id.as_deref(), Some("sess-99"));
    }

    #[test]
    fn session_id_absent_without_span() {
        let server = LoggingServer::new();
        let sink = Arc::new(RecorderSink::default());
        with_subscriber(server.clone(), || {
            server.activate(vec![sink.clone()]);
            tracing::warn!("no span");
        });
        let events = sink.snapshot();
        assert_eq!(events.len(), 1);
        assert!(events[0].session_id.is_none());
    }

    #[test]
    fn session_id_recorded_after_span_creation() {
        // Simulates the daemon pattern: span starts with Empty, filled
        // after correlation.
        let server = LoggingServer::new();
        let sink = Arc::new(RecorderSink::default());
        with_subscriber(server.clone(), || {
            server.activate(vec![sink.clone()]);

            let span = tracing::info_span!("mcp_connection", session_id = tracing::field::Empty,);
            let _guard = span.enter();

            // Before recording: no session_id.
            tracing::warn!("before record");

            // Record session_id (mimics resolve_correlation).
            span.record("session_id", "correlated-sess");

            // After recording: session_id present.
            tracing::warn!("after record");
        });
        let events = sink.snapshot();
        assert_eq!(events.len(), 2);
        assert!(events[0].session_id.is_none());
        assert_eq!(events[1].session_id.as_deref(), Some("correlated-sess"));
    }

    #[test]
    fn event_field_session_id_takes_priority() {
        let server = LoggingServer::new();
        let sink = Arc::new(RecorderSink::default());
        with_subscriber(server.clone(), || {
            server.activate(vec![sink.clone()]);

            let span = tracing::info_span!("session", session_id = "span-sess");
            let _guard = span.enter();
            tracing::warn!(session_id = "event-sess", "explicit field");
        });
        let events = sink.snapshot();
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].session_id.as_deref(), Some("event-sess"));
    }
}
