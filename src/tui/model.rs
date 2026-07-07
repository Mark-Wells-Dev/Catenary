// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 Mark Wells <contact@markwells.dev>

//! The grid's semantic model: the four panes, the tree rows, and the
//! aggregating builders that turn a snapshot + findings into flat renderable
//! row lists.
//!
//! Building the model is where the density law lands: healthy roots aggregate
//! to one collapsed line each; problems sort ahead of noise; suggestions never
//! displace a problem. Rendering ([`super::render`]) is a pure function of these
//! rows — it lays out spans, it makes no policy decisions.

use std::collections::{BTreeMap, HashMap, HashSet};

use crate::health::{FindingCode, Severity};
use crate::state_snapshot::{ServerEntry, SessionEntry, Snapshot, Subagent};

use super::findings::{OwnedFinding, Owner};

/// The four panes of the master-detail grid, plus focus cycling.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Pane {
    /// Top-left — the root/server tree.
    RootTree,
    /// Bottom-left — the client/session tree.
    SessionTree,
    /// Top-right — the contextual detail pane.
    Detail,
    /// Bottom-right — the problems pane.
    Problems,
}

impl Pane {
    /// Tab order: the two trees, then detail, then problems.
    #[must_use]
    pub const fn next(self) -> Self {
        match self {
            Self::RootTree => Self::SessionTree,
            Self::SessionTree => Self::Problems,
            Self::Problems => Self::Detail,
            Self::Detail => Self::RootTree,
        }
    }

    /// Reverse tab order.
    #[must_use]
    pub const fn prev(self) -> Self {
        match self {
            Self::RootTree => Self::Detail,
            Self::Detail => Self::Problems,
            Self::Problems => Self::SessionTree,
            Self::SessionTree => Self::RootTree,
        }
    }

    /// Whether this pane holds a navigable list (trees + problems).
    #[must_use]
    pub const fn is_list(self) -> bool {
        matches!(self, Self::RootTree | Self::SessionTree | Self::Problems)
    }
}

/// The entity a detail pane renders for — the cursored node of a tree.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EntityKey {
    /// A tracked root (by path).
    Root(String),
    /// A configured server (by name), cursored at a specific instance id.
    Server {
        /// The configured server name.
        name: String,
        /// The cursored instance's scope id (`<name>@<root>`).
        id: String,
    },
    /// A host client (by name).
    Client(String),
    /// A live session (by id).
    Session(String),
}

/// A collapse toggle target — what `Enter`/click flips.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Toggle {
    /// Expand/collapse a root's server rows.
    Root(String),
    /// Expand/collapse a client's session rows.
    Client(String),
    /// Expand/collapse the dormant-inventory tail.
    Dormant,
}

/// A root header row — the lifecycle/RAM unit, one collapsed line by default.
#[derive(Debug, Clone)]
pub struct RootRow {
    /// Canonical root path.
    pub path: String,
    /// Contributor sources (`hook` / `mcp:*` / `worktree:*` / `ephemeral:*`).
    pub sources: Vec<String>,
    /// Whether the root is ephemeral (activity-mounted, idle-expiring).
    pub ephemeral: bool,
    /// Seconds until an ephemeral root expires, when tracked.
    pub idle_remaining_secs: Option<u64>,
    /// Whether this root is currently expanded (server rows shown).
    pub expanded: bool,
    /// Servers up (healthy/busy/initializing/probing) in this root.
    pub up: usize,
    /// Total server instances in this root.
    pub total: usize,
    /// Worst finding severity among this root's servers, if any.
    pub worst: Option<Severity>,
}

/// A client header row (session tree) — grouped by host CLI.
#[derive(Debug, Clone)]
pub struct ClientRow {
    /// Host CLI name (`claude` / `antigravity` / `opencode` / `unknown`).
    pub name: String,
    /// Whether the client's session rows are shown.
    pub expanded: bool,
    /// Live session count under this client.
    pub sessions: usize,
    /// Number of install-health findings at this client.
    pub issues: usize,
    /// Worst install-finding severity at this client, if any.
    pub worst: Option<Severity>,
}

/// A flat, renderable tree row. Each row owns the data it renders, decoupling
/// rendering from snapshot indexing.
#[derive(Debug, Clone)]
#[allow(
    clippy::large_enum_variant,
    reason = "row counts are small (aggregated by root/client); boxing would churn every match"
)]
pub enum Row {
    /// A root header (root/server tree).
    Root(RootRow),
    /// A server instance under an expanded root.
    Server(ServerEntry),
    /// A finding rendered inline under its owning server/client node.
    InlineFinding {
        /// The finding severity (for glyph + color).
        severity: Severity,
        /// The one-line message.
        message: String,
        /// Indent depth (2 under a server, 2 under a client).
        depth: u8,
    },
    /// The dormant-inventory toggle line (root/server tree tail).
    DormantToggle {
        /// Number of dormant (configured-but-not-running) servers.
        count: usize,
        /// Whether the tail is expanded.
        expanded: bool,
    },
    /// A dormant server (name only) under an expanded dormant tail.
    Dormant(String),
    /// A client header (client/session tree).
    Client(ClientRow),
    /// A live session under an expanded client.
    Session(SessionEntry),
    /// A subagent sub-row under a session.
    Subagent(Subagent),
}

impl Row {
    /// Indent depth for rendering.
    #[must_use]
    pub const fn depth(&self) -> u8 {
        match self {
            Self::Root(_) | Self::Client(_) | Self::DormantToggle { .. } => 0,
            Self::Server(_) | Self::Dormant(_) | Self::Session(_) => 1,
            Self::Subagent(_) => 2,
            Self::InlineFinding { depth, .. } => *depth,
        }
    }

    /// Whether the cursor can land on this row.
    #[must_use]
    pub const fn selectable(&self) -> bool {
        !matches!(self, Self::InlineFinding { .. })
    }

    /// The detail-pane entity this row selects, if any.
    #[must_use]
    pub fn entity(&self) -> Option<EntityKey> {
        match self {
            Self::Root(r) => Some(EntityKey::Root(r.path.clone())),
            Self::Server(s) => Some(EntityKey::Server {
                name: s.server.clone(),
                id: s.id.clone(),
            }),
            Self::Client(c) => Some(EntityKey::Client(c.name.clone())),
            Self::Session(s) => Some(EntityKey::Session(s.id.clone())),
            _ => None,
        }
    }

    /// The collapse toggle this row flips on `Enter`/click, if any.
    #[must_use]
    pub fn toggle(&self) -> Option<Toggle> {
        match self {
            Self::Root(r) => Some(Toggle::Root(r.path.clone())),
            Self::Client(c) => Some(Toggle::Client(c.name.clone())),
            Self::DormantToggle { .. } => Some(Toggle::Dormant),
            _ => None,
        }
    }

    /// The yankable scope id (the `catenary query` bridge), if any.
    #[must_use]
    pub fn yank_text(&self) -> Option<String> {
        match self {
            Self::Server(s) => Some(s.id.clone()),
            Self::Session(s) => Some(s.id.clone()),
            Self::Subagent(s) => Some(s.id.clone()),
            Self::Root(r) => Some(r.path.clone()),
            _ => None,
        }
    }
}

/// The one-line health verdict, counting problems by tier.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Verdict {
    /// Fatal-tier problem count.
    pub fatal: usize,
    /// Error-tier problem count.
    pub error: usize,
    /// Warning-tier problem count.
    pub warning: usize,
    /// Suggestion count (never a problem, reported for the honest-green tail).
    pub suggestion: usize,
}

impl Verdict {
    /// Total problems (Fatal + Error + Warning) — the verdict count.
    #[must_use]
    pub const fn problems(&self) -> usize {
        self.fatal + self.error + self.warning
    }

    /// Whether the fleet is working (no problems).
    #[must_use]
    pub const fn is_working(&self) -> bool {
        self.problems() == 0
    }
}

/// Count findings into a [`Verdict`].
#[must_use]
pub fn verdict(findings: &[OwnedFinding]) -> Verdict {
    let mut v = Verdict::default();
    for f in findings {
        match f.finding.severity {
            Severity::Fatal => v.fatal += 1,
            Severity::Error => v.error += 1,
            Severity::Warning => v.warning += 1,
            Severity::Suggestion => v.suggestion += 1,
            Severity::Ok | Severity::Info => {}
        }
    }
    v
}

/// A problems-pane row: a finding, its owner (for select-to-focus), and whether
/// it is a suggestion (the collapsed tail).
#[derive(Debug, Clone)]
pub struct ProblemRow {
    /// The finding's stable class — drives which guided mutation (if any) the
    /// action key offers for this row.
    pub code: FindingCode,
    /// Severity tier.
    pub severity: Severity,
    /// One-line message.
    pub message: String,
    /// Fix-it guidance, if any.
    pub fix_it: Option<String>,
    /// The owner a selection focuses the board on.
    pub owner: Owner,
    /// Whether this row is a suggestion (rendered in the collapsed tail).
    pub is_suggestion: bool,
}

/// Build the problems pane: problems sorted Fatal → Error → Warning (then by
/// finding code, then message), followed by suggestions as a tail that can
/// never displace a problem (density law).
#[must_use]
pub fn build_problems(findings: &[OwnedFinding]) -> Vec<ProblemRow> {
    let mut problems: Vec<&OwnedFinding> =
        findings.iter().filter(|f| f.finding.is_problem()).collect();
    problems.sort_by(|a, b| {
        a.finding
            .severity
            .rank()
            .cmp(&b.finding.severity.rank())
            .then_with(|| a.finding.code.as_str().cmp(b.finding.code.as_str()))
            .then_with(|| a.finding.message.cmp(&b.finding.message))
    });

    let mut suggestions: Vec<&OwnedFinding> = findings
        .iter()
        .filter(|f| f.finding.severity == Severity::Suggestion)
        .collect();
    suggestions.sort_by(|a, b| a.finding.message.cmp(&b.finding.message));

    problems
        .into_iter()
        .map(|f| to_problem_row(f, false))
        .chain(suggestions.into_iter().map(|f| to_problem_row(f, true)))
        .collect()
}

fn to_problem_row(f: &OwnedFinding, is_suggestion: bool) -> ProblemRow {
    ProblemRow {
        code: f.finding.code,
        severity: f.finding.severity,
        message: f.finding.message.clone(),
        fix_it: f.finding.fix_it.clone(),
        owner: f.owner.clone(),
        is_suggestion,
    }
}

/// The worst (lowest-rank) severity among a set of findings.
fn worst(sevs: impl Iterator<Item = Severity>) -> Option<Severity> {
    sevs.min_by_key(|s| s.rank())
}

/// Build the top-left root/server tree from the snapshot + findings.
///
/// Roots are the collapse unit: healthy roots aggregate to one line
/// (`expanded_roots` empty → all collapsed). Roots sort problems-first. An
/// expanded root lists its server instances (problems-first) with each server's
/// findings inline. Dormant (configured-but-not-running) servers live behind
/// the tail toggle.
#[must_use]
#[allow(
    clippy::implicit_hasher,
    clippy::too_many_lines,
    reason = "callers use the default hasher; the builder is one cohesive pass"
)]
pub fn build_root_tree(
    snapshot: &Snapshot,
    findings: &[OwnedFinding],
    config: Option<&crate::config::Config>,
    expanded_roots: &HashSet<String>,
    dormant_expanded: bool,
) -> Vec<Row> {
    // server name → worst finding severity + its messages.
    let mut server_worst: HashMap<&str, Severity> = HashMap::new();
    let mut server_msgs: HashMap<&str, Vec<(Severity, &str)>> = HashMap::new();
    for f in findings {
        if let Owner::Server(name) = &f.owner {
            let e = server_worst.entry(name.as_str());
            e.and_modify(|s| {
                if f.finding.severity.rank() < s.rank() {
                    *s = f.finding.severity;
                }
            })
            .or_insert(f.finding.severity);
            server_msgs
                .entry(name.as_str())
                .or_default()
                .push((f.finding.severity, f.finding.message.as_str()));
        }
    }

    // Group live server instances by scope_root.
    let mut by_root: BTreeMap<String, Vec<&ServerEntry>> = BTreeMap::new();
    for s in &snapshot.servers {
        let root = if s.scope_root.is_empty() {
            "(single file)".to_string()
        } else {
            s.scope_root.clone()
        };
        by_root.entry(root).or_default().push(s);
    }
    // Ensure every tracked root appears even with no server instance.
    for r in &snapshot.roots {
        by_root.entry(r.path.clone()).or_default();
    }

    let root_meta: HashMap<&str, &crate::state_snapshot::RootEntry> = snapshot
        .roots
        .iter()
        .map(|r| (r.path.as_str(), r))
        .collect();

    // Build root summaries, sort problems-first then by path.
    let mut roots: Vec<RootRow> = by_root
        .iter()
        .map(|(path, servers)| {
            let up = servers.iter().filter(|s| is_up(&s.state)).count();
            let w = worst(
                servers
                    .iter()
                    .filter_map(|s| server_worst.get(s.server.as_str()).copied()),
            );
            let meta = root_meta.get(path.as_str());
            RootRow {
                path: path.clone(),
                sources: meta.map(|m| m.sources.clone()).unwrap_or_default(),
                ephemeral: meta.is_some_and(|m| m.ephemeral),
                idle_remaining_secs: meta.and_then(|m| m.idle_remaining_secs),
                expanded: expanded_roots.contains(path),
                up,
                total: servers.len(),
                worst: w,
            }
        })
        .collect();
    roots.sort_by(|a, b| {
        rank_opt(a.worst)
            .cmp(&rank_opt(b.worst))
            .then_with(|| a.path.cmp(&b.path))
    });

    let mut rows: Vec<Row> = Vec::new();
    for root in roots {
        let expanded = root.expanded;
        let path = root.path.clone();
        rows.push(Row::Root(root));
        if !expanded {
            continue;
        }
        let mut servers = by_root.get(&path).cloned().unwrap_or_default();
        servers.sort_by(|a, b| {
            rank_opt(server_worst.get(a.server.as_str()).copied())
                .cmp(&rank_opt(server_worst.get(b.server.as_str()).copied()))
                .then_with(|| a.server.cmp(&b.server))
        });
        for s in servers {
            rows.push(Row::Server((*s).clone()));
            if let Some(msgs) = server_msgs.get(s.server.as_str()) {
                for (severity, message) in msgs {
                    rows.push(Row::InlineFinding {
                        severity: *severity,
                        message: (*message).to_string(),
                        depth: 2,
                    });
                }
            }
        }
    }

    // Dormant inventory: configured servers with no live instance.
    if let Some(cfg) = config {
        let live: HashSet<&str> = snapshot.servers.iter().map(|s| s.server.as_str()).collect();
        let mut dormant: Vec<&str> = cfg
            .server
            .keys()
            .map(String::as_str)
            .filter(|n| !live.contains(n))
            .collect();
        dormant.sort_unstable();
        if !dormant.is_empty() {
            rows.push(Row::DormantToggle {
                count: dormant.len(),
                expanded: dormant_expanded,
            });
            if dormant_expanded {
                for name in dormant {
                    rows.push(Row::Dormant(name.to_string()));
                }
            }
        }
    }

    rows
}

/// Build the bottom-left client/session tree from the snapshot + findings.
///
/// Grouped by client; install-health findings render inline at the client node.
/// Clients default expanded (sessions are the point); collapsing hides a
/// client's sessions. Sessions carry subagent sub-rows where the host feeds
/// them.
#[must_use]
#[allow(clippy::implicit_hasher, reason = "callers use the default hasher")]
pub fn build_session_tree(
    snapshot: &Snapshot,
    findings: &[OwnedFinding],
    collapsed_clients: &HashSet<String>,
) -> Vec<Row> {
    // client name → its install findings.
    let mut client_findings: HashMap<&str, Vec<(Severity, &str)>> = HashMap::new();
    for f in findings {
        if let Owner::Client(name) = &f.owner {
            client_findings
                .entry(name.as_str())
                .or_default()
                .push((f.finding.severity, f.finding.message.as_str()));
        }
    }

    // Group sessions by client.
    let mut by_client: BTreeMap<String, Vec<&SessionEntry>> = BTreeMap::new();
    for s in &snapshot.sessions {
        let name = if s.client.name.is_empty() {
            "unknown".to_string()
        } else {
            s.client.name.clone()
        };
        by_client.entry(name).or_default().push(s);
    }
    // A client with a finding but no live session still shows (its install
    // health is the point).
    for name in client_findings.keys() {
        by_client.entry((*name).to_string()).or_default();
    }

    let mut rows: Vec<Row> = Vec::new();
    for (name, sessions) in &by_client {
        let expanded = !collapsed_clients.contains(name);
        let install = client_findings.get(name.as_str());
        let w = install.and_then(|fs| worst(fs.iter().map(|(s, _)| *s)));
        rows.push(Row::Client(ClientRow {
            name: name.clone(),
            expanded,
            sessions: sessions.len(),
            issues: install.map_or(0, Vec::len),
            worst: w,
        }));
        if !expanded {
            continue;
        }
        if let Some(fs) = install {
            for (severity, message) in fs {
                rows.push(Row::InlineFinding {
                    severity: *severity,
                    message: (*message).to_string(),
                    depth: 1,
                });
            }
        }
        let mut sessions = sessions.clone();
        sessions.sort_by(|a, b| b.last_seen.cmp(&a.last_seen));
        for s in sessions {
            rows.push(Row::Session((*s).clone()));
            for sub in &s.subagents {
                rows.push(Row::Subagent(sub.clone()));
            }
        }
    }
    rows
}

/// Sort rank for an optional severity — findings sort ahead of clean rows.
const fn rank_opt(s: Option<Severity>) -> u8 {
    match s {
        Some(sev) => sev.rank(),
        None => u8::MAX,
    }
}

/// Whether a live server-entry state is up (serving or coming up).
#[must_use]
pub fn is_up(state: &str) -> bool {
    matches!(state, "healthy" | "busy" | "initializing" | "probing")
}

#[cfg(test)]
#[allow(
    clippy::expect_used,
    clippy::panic,
    reason = "tests use expect/panic for readable assertions"
)]
mod tests {
    use super::*;
    use crate::health::{Finding, FindingCode};
    use crate::state_snapshot::{ClientInfo, RootEntry};

    fn server(server: &str, root: &str, state: &str) -> ServerEntry {
        ServerEntry {
            id: format!("{server}@{root}"),
            language: "rust".to_string(),
            server: server.to_string(),
            scope_root: root.to_string(),
            state: state.to_string(),
            state_since: crate::state_snapshot::now_iso(),
            ..ServerEntry::default()
        }
    }

    fn owned(code: FindingCode, sev: Severity, msg: &str, owner: Owner) -> OwnedFinding {
        OwnedFinding {
            finding: Finding::new(code, sev, msg),
            owner,
        }
    }

    #[test]
    fn healthy_fleet_is_one_collapsed_line_per_root() {
        let mut snap = Snapshot::default();
        // 3 roots, several servers each, all healthy.
        for r in 0..3 {
            for s in 0..7 {
                snap.servers.push(server(
                    &format!("srv{s}"),
                    &format!("/p/root{r}"),
                    "healthy",
                ));
            }
        }
        let rows = build_root_tree(&snap, &[], None, &HashSet::new(), false);
        // All collapsed → exactly one Row::Root per root, nothing else.
        assert_eq!(rows.len(), 3, "one collapsed line per root");
        assert!(rows.iter().all(|r| matches!(r, Row::Root(_))));
    }

    #[test]
    fn broken_server_sorts_its_root_first_and_carries_worst() {
        let mut snap = Snapshot::default();
        snap.servers.push(server("ok-ls", "/p/aaa", "healthy"));
        snap.servers.push(server("bad-ls", "/p/zzz", "failed"));
        let findings = vec![owned(
            FindingCode::ServerRoutedBroken,
            Severity::Fatal,
            "bad-ls: initialize failed",
            Owner::Server("bad-ls".to_string()),
        )];
        let rows = build_root_tree(&snap, &findings, None, &HashSet::new(), false);
        // The broken root (/p/zzz) sorts ahead of the healthy one despite path order.
        let Row::Root(first) = &rows[0] else {
            panic!("first row is a root")
        };
        assert_eq!(first.path, "/p/zzz");
        assert_eq!(first.worst, Some(Severity::Fatal));
    }

    #[test]
    fn expanded_root_lists_servers_with_inline_findings() {
        let mut snap = Snapshot::default();
        snap.servers.push(server("bad-ls", "/p/zzz", "failed"));
        let findings = vec![owned(
            FindingCode::ServerRoutedBroken,
            Severity::Fatal,
            "bad-ls: initialize failed",
            Owner::Server("bad-ls".to_string()),
        )];
        let expanded: HashSet<String> = std::iter::once("/p/zzz".to_string()).collect();
        let rows = build_root_tree(&snap, &findings, None, &expanded, false);
        // Root, Server, InlineFinding.
        assert!(matches!(rows[0], Row::Root(_)));
        assert!(matches!(rows[1], Row::Server(_)));
        assert!(matches!(rows[2], Row::InlineFinding { .. }));
    }

    #[test]
    fn session_tree_groups_by_client_with_subagents() {
        let mut snap = Snapshot::default();
        snap.sessions.push(SessionEntry {
            id: "sess-1".to_string(),
            client: ClientInfo {
                name: "claude".to_string(),
                version: None,
            },
            last_seen: crate::state_snapshot::now_iso(),
            subagents: vec![Subagent {
                id: "agent-a".to_string(),
                started_at: crate::state_snapshot::now_iso(),
            }],
            ..SessionEntry::default()
        });
        let rows = build_session_tree(&snap, &[], &HashSet::new());
        assert!(matches!(&rows[0], Row::Client(c) if c.name == "claude"));
        assert!(matches!(rows[1], Row::Session(_)));
        assert!(matches!(rows[2], Row::Subagent(_)));
    }

    #[test]
    fn client_install_finding_renders_inline() {
        let mut snap = Snapshot::default();
        snap.sessions.push(SessionEntry {
            id: "sess-1".to_string(),
            client: ClientInfo {
                name: "antigravity".to_string(),
                version: None,
            },
            last_seen: crate::state_snapshot::now_iso(),
            ..SessionEntry::default()
        });
        let findings = vec![owned(
            FindingCode::HooksStale,
            Severity::Warning,
            "antigravity hooks are stale",
            Owner::Client("antigravity".to_string()),
        )];
        let rows = build_session_tree(&snap, &findings, &HashSet::new());
        assert!(matches!(&rows[0], Row::Client(c) if c.worst == Some(Severity::Warning)));
        assert!(matches!(rows[1], Row::InlineFinding { .. }));
    }

    #[test]
    fn verdict_counts_problems_and_suggestions() {
        let findings = vec![
            owned(
                FindingCode::ServerRoutedBroken,
                Severity::Fatal,
                "a",
                Owner::Global,
            ),
            owned(
                FindingCode::ConfigUnknownKey,
                Severity::Warning,
                "b",
                Owner::Global,
            ),
            owned(
                FindingCode::ServerInstallSuggestion,
                Severity::Suggestion,
                "c",
                Owner::Global,
            ),
        ];
        let v = verdict(&findings);
        assert_eq!(v.fatal, 1);
        assert_eq!(v.warning, 1);
        assert_eq!(v.suggestion, 1);
        assert_eq!(v.problems(), 2);
        assert!(!v.is_working());
    }

    #[test]
    fn problems_sort_fatal_first_suggestions_tail() {
        let findings = vec![
            owned(
                FindingCode::ServerInstallSuggestion,
                Severity::Suggestion,
                "install foo",
                Owner::Global,
            ),
            owned(
                FindingCode::ConfigUnknownKey,
                Severity::Warning,
                "warn",
                Owner::Global,
            ),
            owned(
                FindingCode::ServerRoutedBroken,
                Severity::Fatal,
                "fatal",
                Owner::Global,
            ),
        ];
        let rows = build_problems(&findings);
        assert_eq!(rows.len(), 3);
        assert_eq!(rows[0].severity, Severity::Fatal);
        assert_eq!(rows[1].severity, Severity::Warning);
        assert_eq!(rows[2].severity, Severity::Suggestion);
        assert!(rows[2].is_suggestion, "suggestion is in the tail");
        assert!(!rows[0].is_suggestion);
    }

    #[test]
    fn ephemeral_root_metadata_flows_into_row() {
        let mut snap = Snapshot::default();
        snap.roots.push(RootEntry {
            path: "/p/eph".to_string(),
            ephemeral: true,
            sources: vec!["ephemeral:query".to_string()],
            idle_remaining_secs: Some(42),
        });
        let rows = build_root_tree(&snap, &[], None, &HashSet::new(), false);
        let Row::Root(r) = &rows[0] else {
            panic!("root row")
        };
        assert!(r.ephemeral);
        assert_eq!(r.idle_remaining_secs, Some(42));
        assert_eq!(r.sources, vec!["ephemeral:query".to_string()]);
    }
}
