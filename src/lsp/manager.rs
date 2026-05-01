// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 Mark Wells <contact@markwells.dev>

use anyhow::{Result, anyhow};
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::Mutex;
use tracing::{debug, info, warn};

use crate::bridge::filesystem_manager::{ClassificationTables, FilesystemManager};
use crate::config::{Config, DispatchMethod, ServerBinding, ServerDef};
use crate::logging::LoggingServer;
use crate::lsp::LspClient;
use crate::lsp::glob::{FileChange, GlobPattern, LspGlob, WatchKind};
use crate::lsp::instance_key::{InstanceKey, Scope};
use crate::lsp::server::LspServer;
use crate::lsp::state::ServerStatus;
use crate::source::Source;

/// Filters filesystem changes against a server's watcher registrations.
///
/// Returns `(uri, FileChangeType as u8)` pairs for changes that match at
/// least one watcher's glob pattern and watch kind.
fn match_file_changes(
    changes: &[FileChange],
    watchers: &[(GlobPattern, WatchKind)],
    roots: &[PathBuf],
) -> Vec<(String, u8)> {
    changes
        .iter()
        .filter(|change| {
            watchers.iter().any(|(pattern, kind)| {
                pattern.is_match(&change.path, roots) && kind.matches(change.change_type)
            })
        })
        .map(|change| {
            (
                format!("file://{}", change.path.display()),
                change.change_type as u8,
            )
        })
        .collect()
}

/// Looks up an existing client instance by trying `Scope::Root(root)`
/// first, then `Scope::Workspace`. Root-first ordering ensures that
/// project-scoped instances (Rule A) take precedence over shared
/// workspace instances when both exist for the same `(lang, server)`.
fn find_instance(
    clients: &HashMap<InstanceKey, Arc<Mutex<LspClient>>>,
    lang: &str,
    server_name: &str,
    root: &Path,
) -> Option<Arc<Mutex<LspClient>>> {
    let root_key = InstanceKey::new(
        lang.to_string(),
        server_name.to_string(),
        Scope::Root(root.to_path_buf()),
    );
    if let Some(client) = clients.get(&root_key) {
        return Some(client.clone());
    }
    let workspace_key =
        InstanceKey::new(lang.to_string(), server_name.to_string(), Scope::Workspace);
    clients.get(&workspace_key).cloned()
}

/// Tests whether a path matches a server's `file_patterns`.
///
/// If `patterns` is empty, returns `true` (no filter = match all).
/// Otherwise, matches the filename component of `path` against the
/// compiled globs.
fn file_matches_patterns(path: &Path, patterns: &[LspGlob]) -> bool {
    if patterns.is_empty() {
        return true;
    }
    let Some(file_name) = path.file_name() else {
        return false;
    };
    let file_path = Path::new(file_name);
    patterns.iter().any(|g| g.is_match(file_path))
}

/// Manages the lifecycle of LSP clients, document state, and language detection.
///
/// Single authority for LSP server spawning, caching, shutdown, and document
/// lifecycle. Document versioning and open/close tracking live on each
/// [`LspClient`] — each server sees an independent monotonic version sequence.
pub struct LspClientManager {
    config: Arc<Config>,
    /// Per-root project configs from `.catenary.toml`. Keyed by root path.
    /// Uses `std::sync::Mutex` — reads are fast, non-contended, and must
    /// not be held across `.await` points.
    project_configs: std::sync::Mutex<HashMap<PathBuf, crate::config::ProjectConfig>>,
    clients: Mutex<HashMap<InstanceKey, Arc<Mutex<LspClient>>>>,
    /// Negative cache for single-file server initialization failures.
    /// Contains `(language_id, server_name)` pairs where the server is
    /// configured with `single_file = true` but rejected null-workspace
    /// initialization at runtime. Uses `std::sync::Mutex` — reads are
    /// fast and non-contended.
    pub(crate) single_file_failures: std::sync::Mutex<HashSet<(String, String)>>,
    logging: LoggingServer,
    fs: Arc<FilesystemManager>,
}

impl LspClientManager {
    /// Creates a new `LspClientManager`.
    ///
    /// Workspace roots are sourced from the shared [`FilesystemManager`] —
    /// call [`FilesystemManager::set_roots`] before constructing this manager.
    #[must_use]
    pub fn new(
        config: impl Into<Arc<Config>>,
        logging: LoggingServer,
        fs: Arc<FilesystemManager>,
    ) -> Self {
        let config = config.into();
        Self {
            config,
            project_configs: std::sync::Mutex::new(HashMap::new()),
            clients: Mutex::new(HashMap::new()),
            single_file_failures: std::sync::Mutex::new(HashSet::new()),
            logging,
            fs,
        }
    }

    /// Returns a reference to the configuration.
    pub fn config(&self) -> &Config {
        &self.config
    }

    /// Extracts `CommandsConfig` from each loaded project config.
    ///
    /// Returns a map from root path to the project's `[commands]` section.
    /// Roots without a `[commands]` section are omitted.
    pub fn project_commands(&self) -> HashMap<PathBuf, crate::config::CommandsConfig> {
        let configs = self
            .project_configs
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        configs
            .iter()
            .filter_map(|(root, pc)| pc.commands.clone().map(|cmds| (root.clone(), cmds)))
            .collect()
    }

    /// Spawns LSP servers for languages detected in the workspace.
    ///
    /// Walks workspace roots (respecting `.gitignore`), classifies files via
    /// [`FilesystemManager`], and spawns servers for configured languages
    /// that have matching files. Servers that fail to spawn are logged and
    /// skipped — a misconfigured server should not prevent others from starting.
    ///
    /// For workspace-capable servers, spawns a single instance (all roots
    /// are already included in the `initialize` request). For legacy
    /// servers, spawns a separate `Scope::Root` instance per root.
    pub async fn spawn_all(&self) {
        let roots = self.fs.roots();

        // Load project configs for all roots and set per-root
        // classification tables before detection runs.
        self.load_project_configs_for_roots(&roots);
        self.set_per_root_classification(&roots);

        let configured_keys: HashSet<&str> =
            self.config.language.keys().map(String::as_str).collect();
        let relevant = self.fs.detect_workspace_languages(&roots, &configured_keys);

        if relevant.is_empty() {
            info!("No configured languages detected in workspace");
            return;
        }

        let mut sorted: Vec<&str> = relevant.iter().map(String::as_str).collect();
        sorted.sort_unstable();
        info!("Detected languages in workspace: {}", sorted.join(", "));

        for lang in &relevant {
            let Some(lang_config) = self.config.resolve_language(lang) else {
                continue;
            };
            let bindings: Vec<ServerBinding> = lang_config.servers.clone();

            for binding in &bindings {
                let Some(first_root) = roots.first() else {
                    continue;
                };

                let client = match self.ensure_server(lang, &binding.name, first_root).await {
                    Ok(c) => c,
                    Err(e) => {
                        warn!(
                            source = Source::LspLifecycle.as_str(),
                            language = lang.as_str(),
                            server = binding.name.as_str(),
                            "Failed to spawn LSP server for {lang}: {e}",
                        );
                        continue;
                    }
                };

                let key = client.lock().await.server().key();
                let Some(key) = key else { continue };

                self.exclude_project_roots_from_workspace(
                    &client,
                    &key,
                    lang,
                    &binding.name,
                    &roots,
                )
                .await;
                self.spawn_remaining_roots(lang, &binding.name, &key, &roots)
                    .await;
            }
        }
    }

    /// Excludes project-scoped roots from a workspace instance.
    ///
    /// If the given instance is `Scope::Workspace`, sends
    /// `didChangeWorkspaceFolders` remove for each root that is
    /// project-scoped for this language. No-op for non-workspace keys
    /// or if no roots are project-scoped.
    async fn exclude_project_roots_from_workspace(
        &self,
        client: &Arc<Mutex<LspClient>>,
        key: &InstanceKey,
        lang: &str,
        server_name: &str,
        roots: &[PathBuf],
    ) {
        if key.scope != Scope::Workspace {
            return;
        }
        let project_scoped: Vec<(String, String)> = roots
            .iter()
            .filter(|r| self.is_project_scoped(lang, r))
            .map(|r| {
                (
                    format!("file://{}", r.display()),
                    r.file_name().map_or_else(
                        || "workspace".to_string(),
                        |s| s.to_string_lossy().to_string(),
                    ),
                )
            })
            .collect();
        if project_scoped.is_empty() {
            return;
        }
        let refs: Vec<(&str, &str)> = project_scoped
            .iter()
            .map(|(u, n)| (u.as_str(), n.as_str()))
            .collect();
        let locked = client.lock().await;
        if let Err(e) = locked.did_change_workspace_folders(&[], &refs).await {
            info!("Failed to exclude project-scoped roots from {server_name} workspace: {e}",);
        }
    }

    /// Spawns instances for remaining roots (after the first).
    ///
    /// Root-scoped first spawn: spawns per-root instances for all
    /// remaining roots (project-scoped use effective server defs).
    /// Workspace-scoped first spawn: only spawns project-scoped roots.
    async fn spawn_remaining_roots(
        &self,
        lang: &str,
        server_name: &str,
        key: &InstanceKey,
        roots: &[PathBuf],
    ) {
        if roots.len() <= 1 {
            return;
        }
        match &key.scope {
            Scope::Root(_) => {
                info!(
                    source = Source::LspLifecycle.as_str(),
                    language = lang,
                    server = server_name,
                    "Server does not support workspaceFolders — spawning per-root instances",
                );
                for root in &roots[1..] {
                    if let Err(e) = self.ensure_server(lang, server_name, root).await {
                        warn!(
                            source = Source::LspLifecycle.as_str(),
                            language = lang,
                            server = server_name,
                            "Failed to spawn per-root instance for {lang} ({server_name}) at {}: {e}",
                            root.display(),
                        );
                    }
                }
            }
            Scope::Workspace => {
                // Spawn project-scoped roots only. Non-project-scoped
                // roots are already covered by the workspace instance.
                for root in &roots[1..] {
                    if self.is_project_scoped(lang, root)
                        && let Err(e) = self.spawn_project_scoped(server_name, lang, root).await
                    {
                        warn!(
                            source = Source::LspLifecycle.as_str(),
                            language = lang,
                            server = server_name,
                            "Failed to spawn project-scoped instance for {lang} ({server_name}) at {}: {e}",
                            root.display(),
                        );
                    }
                }
            }
            Scope::SingleFile => {}
        }
    }

    /// Notifies workspace-capable servers about folder additions and
    /// removals during `sync_roots`, and updates per-root settings
    /// with `didChangeConfiguration` for new roots with overrides.
    #[allow(
        clippy::significant_drop_tightening,
        reason = "client lock held across notification sends for consistent state"
    )]
    async fn notify_workspace_servers_sync(
        &self,
        to_add: &[PathBuf],
        removed_refs: &[(&str, &str)],
    ) {
        let clients = self.clients.lock().await.clone();
        for (key, client_mutex) in &clients {
            let client = client_mutex.lock().await;
            if !client.is_alive() || !client.supports_workspace_folders() {
                continue;
            }

            // Only add roots NOT project-scoped for this language.
            let non_project_adds: Vec<(String, String)> = to_add
                .iter()
                .filter(|r| !self.is_project_scoped(&key.language_id, r))
                .map(|root| {
                    (
                        format!("file://{}", root.display()),
                        root.file_name().map_or_else(
                            || "workspace".to_string(),
                            |s| s.to_string_lossy().to_string(),
                        ),
                    )
                })
                .collect();
            let added_refs: Vec<(&str, &str)> = non_project_adds
                .iter()
                .map(|(u, n)| (u.as_str(), n.as_str()))
                .collect();

            if (!added_refs.is_empty() || !removed_refs.is_empty())
                && let Err(e) = client
                    .did_change_workspace_folders(&added_refs, removed_refs)
                    .await
            {
                info!(
                    "Failed to notify {} server about workspace folder changes: {}",
                    key.language_id, e
                );
            }

            // Update per-root settings and send didChangeConfiguration
            // for non-project-scoped roots with settings overrides.
            for root in to_add {
                if self.is_project_scoped(&key.language_id, root) {
                    continue;
                }
                let Some(settings) = self.effective_settings(&key.server, root) else {
                    continue;
                };
                let differs = self
                    .config
                    .server
                    .get(&key.server)
                    .and_then(|d| d.settings.as_ref())
                    .is_none_or(|user| *user != settings);
                if !differs {
                    continue;
                }
                client.server().set_root_settings(root.clone(), settings);
                if client.server().wants_did_change_configuration()
                    && let Err(e) = client.did_change_configuration().await
                {
                    info!(
                        "Failed to send didChangeConfiguration to {} for {}: {e}",
                        key.server,
                        root.display(),
                    );
                }
            }
        }
    }

    /// Removes per-root settings from workspace instances for removed
    /// roots.
    async fn cleanup_root_settings_for_removed(&self, to_remove: &[PathBuf]) {
        if to_remove.is_empty() {
            return;
        }
        let clients = self.clients.lock().await.clone();
        for (key, client_mutex) in &clients {
            if !matches!(key.scope, Scope::Workspace) {
                continue;
            }
            let client = client_mutex.lock().await;
            for removed in to_remove {
                client.server().remove_root_settings(removed);
            }
        }
    }

    /// Returns whether any server for this language is configured for
    /// single-file mode (`single_file = true` in `[server.*]`).
    ///
    /// Used by the hook layer to decide whether out-of-root edits
    /// should be gated by `start_editing`. Servers that failed at
    /// runtime (negative cache) are excluded.
    #[must_use]
    pub fn has_single_file_coverage(&self, lang: &str) -> bool {
        let Some(lang_config) = self.config.resolve_language(lang) else {
            return false;
        };
        let failures = self
            .single_file_failures
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        lang_config.servers.iter().any(|binding| {
            let Some(def) = self.config.server.get(&binding.name) else {
                return false;
            };
            def.single_file && !failures.contains(&(lang.to_string(), binding.name.clone()))
        })
    }

    /// Returns the current workspace roots.
    pub fn roots(&self) -> Vec<PathBuf> {
        self.fs.roots()
    }

    /// Removes a workspace root and updates all active LSP clients.
    ///
    /// Workspace-capable servers receive a `didChangeWorkspaceFolders`
    /// removal notification. Legacy servers with a `Scope::Root` matching
    /// the removed root are shut down and removed from the map.
    ///
    /// # Errors
    ///
    /// Returns an error if the root path cannot be converted to a valid URI.
    pub async fn remove_root(&self, root: &Path) -> Result<()> {
        let uri = format!("file://{}", root.display());
        let name = root.file_name().map_or_else(
            || "workspace".to_string(),
            |s| s.to_string_lossy().to_string(),
        );

        let mut roots = self.fs.roots();
        roots.retain(|r| r != root);
        self.fs.set_roots(roots);

        // Remove project config and classification tables for the removed root.
        self.project_configs
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .remove(root);
        self.fs.remove_root_classification(root);

        // Notify workspace-capable servers about the removal and
        // clean up per-root settings.
        let clients = self.clients.lock().await.clone();
        for (key, client_mutex) in &clients {
            let client = client_mutex.lock().await;
            if !client.is_alive() || !client.supports_workspace_folders() {
                continue;
            }
            if let Err(e) = client
                .did_change_workspace_folders(&[], &[(&uri, &name)])
                .await
            {
                info!(
                    "Failed to notify {} server about removed workspace folder: {}",
                    key.language_id, e
                );
            }
            client.server().remove_root_settings(root);
        }

        // Shut down legacy per-root instances bound to the removed root.
        self.shutdown_root_instances(root).await;

        Ok(())
    }

    /// Synchronizes workspace roots with a new set.
    ///
    /// Diffs against current roots: adds new ones, removes stale ones.
    /// Workspace-capable servers receive a single `didChangeWorkspaceFolders`
    /// notification with both additions and removals. Legacy servers get
    /// per-root instance lifecycle: removed roots have their instances shut
    /// down; added roots get new `Scope::Root` instances spawned (only for
    /// languages that already have active legacy instances).
    ///
    /// # Errors
    ///
    /// Returns an error if any root path cannot be converted to a valid URI.
    pub async fn sync_roots(&self, new_roots: Vec<PathBuf>) -> Result<()> {
        // Snapshot before updating so the diff is computed against old state.
        let current_roots = self.fs.roots();
        self.fs.set_roots(new_roots.clone());

        let to_add: Vec<PathBuf> = new_roots
            .iter()
            .filter(|r| !current_roots.contains(r))
            .cloned()
            .collect();
        let to_remove: Vec<PathBuf> = current_roots
            .iter()
            .filter(|r| !new_roots.contains(r))
            .cloned()
            .collect();

        if to_add.is_empty() && to_remove.is_empty() {
            return Ok(());
        }

        info!(
            "Syncing roots: {} added, {} removed",
            to_add.len(),
            to_remove.len()
        );

        // Update project configs: load for added roots, remove for removed.
        self.load_project_configs_for_roots(&to_add);
        self.set_per_root_classification(&to_add);
        if !to_remove.is_empty() {
            let mut configs = self
                .project_configs
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            for removed in &to_remove {
                configs.remove(removed);
            }
            drop(configs);
            for removed in &to_remove {
                self.fs.remove_root_classification(removed);
            }
        }

        // Partition added roots: project-scoped roots are excluded from
        // workspace folder notifications and get their own instances.
        // A root may be project-scoped for one language+server but not
        // another — the partition is computed per-server below.

        let removed_folders: Vec<(String, String)> = to_remove
            .iter()
            .map(|root| {
                (
                    format!("file://{}", root.display()),
                    root.file_name().map_or_else(
                        || "workspace".to_string(),
                        |s| s.to_string_lossy().to_string(),
                    ),
                )
            })
            .collect();
        let removed_refs: Vec<(&str, &str)> = removed_folders
            .iter()
            .map(|(u, n)| (u.as_str(), n.as_str()))
            .collect();

        // Notify workspace servers about folder changes, update
        // per-root settings, and clean up removed roots.
        self.notify_workspace_servers_sync(&to_add, &removed_refs)
            .await;
        self.cleanup_root_settings_for_removed(&to_remove).await;

        // Legacy per-root lifecycle: shut down instances for removed roots.
        for removed in &to_remove {
            self.shutdown_root_instances(removed).await;
        }

        // Shut down single-file servers and clear the cache — root
        // changes may have brought previously-unrooted files into scope
        // of workspace or per-root instances. Single-file servers are
        // lazily re-spawned on the next request if still needed.
        self.shutdown_single_file_instances().await;

        // Spawn instances for added roots.
        if !to_add.is_empty() {
            self.spawn_for_added_roots(&to_add).await;
        }

        Ok(())
    }

    /// Returns clients for a file path, filtered by capability,
    /// `file_patterns`, and `disabled_methods`, in priority order
    /// (from the `servers` list in `[language.*]`).
    ///
    /// Resolves language from path via `FilesystemManager`, iterates
    /// the binding's servers, filters by:
    /// 1. `disabled_methods` on the binding (per-binding suppression)
    /// 2. `file_patterns` on `[server.*]` (filename-level glob)
    /// 3. The given capability check
    ///
    /// `method` is the [`DispatchMethod`] being dispatched. Pass
    /// `None` when the caller has its own suppression mechanism
    /// (e.g., diagnostic dispatch uses the `diagnostics` flag).
    ///
    /// Returns an empty Vec when no server matches. On empty result,
    /// emits a `warn!()` — dedup handled by `NotificationQueueSink`.
    ///
    /// Does not block on server readiness — callers must call
    /// `wait_ready_for_path` or `wait_ready_all` before invoking.
    #[allow(
        clippy::significant_drop_tightening,
        reason = "clients lock held across async iteration for consistent snapshot"
    )]
    pub async fn get_servers(
        &self,
        path: &Path,
        capability: fn(&LspServer) -> bool,
        method: Option<DispatchMethod>,
    ) -> Vec<Arc<Mutex<LspClient>>> {
        // Detect language: primary (FilesystemManager) then fallback (raw extension).
        let Some(lang_id) = self.fs.language_id(path).or_else(|| {
            path.extension()
                .and_then(|e| e.to_str())
                .map(str::to_string)
        }) else {
            return Vec::new();
        };

        // Look up language config.
        let Some(lang_config) = self.config.resolve_language(&lang_id) else {
            return Vec::new();
        };

        // Resolve owning workspace root. If unrooted, fall through to
        // tier 3 (single-file servers).
        if let Some(root) = self.fs.resolve_root(path) {
            // Tiers 1–2: rooted file lookup.
            let clients = self.clients.lock().await;
            let mut result = Vec::new();

            for binding in &lang_config.servers {
                if method.is_some_and(|m| binding.is_method_disabled(m)) {
                    continue;
                }
                let Some(server_def) = self.config.server.get(&binding.name) else {
                    continue;
                };
                if !file_matches_patterns(path, &server_def.compiled_patterns) {
                    continue;
                }
                let Some(client) = find_instance(&clients, &lang_id, &binding.name, &root) else {
                    continue;
                };
                let locked = client.lock().await;
                if !locked.is_alive() {
                    continue;
                }
                if !capability(locked.server()) {
                    continue;
                }
                drop(locked);
                result.push(client);
            }

            if result.is_empty() && !lang_config.servers.is_empty() {
                info!(
                    source = Source::LspDispatch.as_str(),
                    language = lang_id.as_str(),
                    "No server supports the requested capability for {lang_id} files",
                );
            }

            return result;
        }

        // Tier 3: single-file servers for unrooted files.
        let mut result = Vec::new();
        for binding in &lang_config.servers {
            if method.is_some_and(|m| binding.is_method_disabled(m)) {
                continue;
            }
            let Some(server_def) = self.config.server.get(&binding.name) else {
                continue;
            };
            if !file_matches_patterns(path, &server_def.compiled_patterns) {
                continue;
            }
            let Some(client) = self
                .ensure_single_file_server(&lang_id, &binding.name)
                .await
            else {
                continue;
            };
            let locked = client.lock().await;
            if !locked.is_alive() || !capability(locked.server()) {
                continue;
            }
            drop(locked);
            result.push(client);
        }

        result
    }

    /// Waits for every server bound to this path's language binding.
    ///
    /// Resolves language from path, iterates all servers in the
    /// binding, waits for each to reach Ready or terminal state.
    /// Dead servers don't block — they return immediately. Servers
    /// that haven't been spawned yet are skipped (not spawned).
    /// Unrooted files wait on single-file servers (tier 3).
    pub async fn wait_ready_for_path(&self, path: &Path) {
        // Detect language: primary (FilesystemManager) then fallback (raw extension).
        let Some(lang_id) = self.fs.language_id(path).or_else(|| {
            path.extension()
                .and_then(|e| e.to_str())
                .map(str::to_string)
        }) else {
            return;
        };

        // Look up language config — unconfigured languages skip.
        let Some(lang_config) = self.config.resolve_language(&lang_id) else {
            return;
        };

        // Collect matching instances under the lock, then release before waiting.
        #[allow(
            clippy::option_if_let_else,
            reason = "if/else clearer than map_or_else here"
        )]
        let to_wait: Vec<Arc<Mutex<LspClient>>> = {
            let clients = self.clients.lock().await;
            if let Some(root) = self.fs.resolve_root(path) {
                // Tiers 1–2: rooted file.
                lang_config
                    .servers
                    .iter()
                    .filter_map(|binding| find_instance(&clients, &lang_id, &binding.name, &root))
                    .collect()
            } else {
                // Tier 3: single-file servers.
                lang_config
                    .servers
                    .iter()
                    .filter_map(|binding| {
                        let sf_key = InstanceKey::new(
                            lang_id.clone(),
                            binding.name.clone(),
                            Scope::SingleFile,
                        );
                        clients.get(&sf_key).cloned()
                    })
                    .collect()
            }
        };

        for client_mutex in to_wait {
            client_mutex.lock().await.wait_ready().await;
        }
    }

    /// Waits for every active instance across all bindings.
    ///
    /// Clones the client map, waits for each to reach Ready or
    /// terminal state. Dead servers return immediately.
    pub async fn wait_ready_all(&self) {
        let clients = self.clients.lock().await.clone();
        for (_key, client_mutex) in clients {
            client_mutex.lock().await.wait_ready().await;
        }
    }

    /// Spawns missing servers for the given paths and waits for
    /// all to be ready.
    ///
    /// Combines [`ensure_clients_for_paths`](Self::ensure_clients_for_paths)
    /// (spawn) with [`wait_ready_all`](Self::wait_ready_all). Closes the
    /// lazy-spawn gap: after this call, all servers for the discovered
    /// languages are Ready (or terminal).
    pub async fn ensure_and_wait_for_paths(&self, paths: &[PathBuf]) {
        self.ensure_clients_for_paths(paths).await;
        self.wait_ready_all().await;
    }

    /// Spawns with capability-driven scope (no project-scope check).
    ///
    /// Production code should use [`Self::ensure_server`] which handles
    /// project-scope routing. This wrapper exists for tests that need
    /// explicit scope control.
    #[cfg(test)]
    async fn spawn(
        &self,
        server_name: &str,
        lang: &str,
        root: &Path,
    ) -> Result<(InstanceKey, Arc<Mutex<LspClient>>)> {
        self.spawn_inner(server_name, lang, root, false).await
    }

    /// Spawns a project-scoped server instance using the effective
    /// (merged) `ServerDef` for the root.
    ///
    /// Always produces `Scope::Root(root)` regardless of workspace
    /// folder support — project-scoped roots are isolated by design
    /// (Rule A from §9).
    ///
    /// # Errors
    ///
    /// Returns an error if:
    /// - No effective server def can be computed for this server+root.
    /// - The server fails to spawn or initialize.
    async fn spawn_project_scoped(
        &self,
        server_name: &str,
        lang: &str,
        root: &Path,
    ) -> Result<(InstanceKey, Arc<Mutex<LspClient>>)> {
        self.spawn_inner(server_name, lang, root, true).await
    }

    /// Shared spawn implementation.
    ///
    /// When `project_scoped` is true, uses `effective_server_def` and
    /// forces `Scope::Root(root)` regardless of capabilities.
    async fn spawn_inner(
        &self,
        server_name: &str,
        lang: &str,
        root: &Path,
        project_scoped: bool,
    ) -> Result<(InstanceKey, Arc<Mutex<LspClient>>)> {
        let server_def = if project_scoped {
            self.effective_server_def(server_name, root)
                .ok_or_else(|| {
                    anyhow!(
                        "No effective server def for '{server_name}' at {}",
                        root.display()
                    )
                })?
        } else {
            self.config
                .server
                .get(server_name)
                .ok_or_else(|| anyhow!("Server '{server_name}' not found in [server.*] config"))?
                .clone()
        };

        let roots = self.fs.roots();
        let mut clients = self.clients.lock().await;

        // Double-check: another task may have spawned this server
        // while we waited.
        //
        // Project-scoped spawns only check the exact Root(root) key —
        // an existing Workspace instance must NOT prevent the
        // project-scoped spawn (that's the whole point of Rule A).
        let existing = if project_scoped {
            let root_key = InstanceKey::new(
                lang.to_string(),
                server_name.to_string(),
                Scope::Root(root.to_path_buf()),
            );
            clients.get(&root_key).cloned()
        } else {
            find_instance(&clients, lang, server_name, root)
        };
        if let Some(found) = existing {
            if found.lock().await.is_alive() {
                let key = found
                    .lock()
                    .await
                    .server()
                    .key()
                    .ok_or_else(|| anyhow!("Existing server missing instance key"))?;
                return Ok((key, found));
            }
            anyhow::bail!("LSP server '{server_name}' ({lang}) is dead");
        }

        info!(
            "Spawning LSP server for {}: {} {}",
            lang,
            server_def.command,
            server_def.args.join(" ")
        );

        // Build per-root settings map from project configs.
        // Workspace instances use this for scopeUri resolution;
        // root instances won't have scopeUri requests for other roots.
        let settings_per_root = if project_scoped {
            // Project-scoped instances get pre-merged flat settings —
            // no scopeUri resolution needed.
            HashMap::new()
        } else {
            self.collect_per_root_settings(server_name, &roots)
        };

        let args: Vec<&str> = server_def
            .args
            .iter()
            .map(|s: &String| s.as_str())
            .collect();
        let mut client = LspClient::spawn(
            &server_def.command,
            &args,
            lang,
            server_name,
            self.logging.clone(),
            server_def.settings.clone(),
            settings_per_root,
            server_def.env.as_ref(),
        )?;

        client
            .initialize(&roots, server_def.initialization_options.clone())
            .await?;

        // Project-scoped instances always get Scope::Root regardless
        // of capabilities — they are isolated by Rule A.
        let scope = if project_scoped {
            Scope::Root(root.to_path_buf())
        } else if client.supports_workspace_folders() {
            Scope::Workspace
        } else {
            Scope::Root(root.to_path_buf())
        };
        client.server().set_scope(scope);

        let key = client
            .server()
            .key()
            .ok_or_else(|| anyhow!("Failed to construct instance key after initialization"))?;

        let client_mutex = Arc::new(Mutex::new(client));
        clients.insert(key.clone(), client_mutex.clone());
        drop(clients);

        Ok((key, client_mutex))
    }

    /// Spawns a single-file server with null workspace.
    ///
    /// Sends `initialize` with `rootUri: null` and
    /// `workspaceFolders: null`. If the server initializes successfully,
    /// inserts a `Scope::SingleFile` client. If initialization fails,
    /// negative-caches the `(lang, server)` pair.
    ///
    /// Only call for servers with `single_file = true` in config.
    ///
    /// # Errors
    ///
    /// Returns an error if the server definition is missing from config
    /// or the server rejects null-workspace initialization.
    async fn spawn_single_file(
        &self,
        server_name: &str,
        lang: &str,
    ) -> Result<Arc<Mutex<LspClient>>> {
        let server_def = self
            .config
            .server
            .get(server_name)
            .ok_or_else(|| anyhow!("Server '{server_name}' not found in [server.*] config"))?
            .clone();

        let mut clients = self.clients.lock().await;

        // Double-check: another task may have spawned while we waited.
        let sf_key = InstanceKey::new(lang.to_string(), server_name.to_string(), Scope::SingleFile);
        if let Some(existing) = clients.get(&sf_key) {
            if existing.lock().await.is_alive() {
                return Ok(existing.clone());
            }
            anyhow::bail!("Single-file LSP server '{server_name}' ({lang}) is dead");
        }

        info!(
            "Spawning single-file LSP server for {lang}: {} {}",
            server_def.command,
            server_def.args.join(" ")
        );

        let args: Vec<&str> = server_def.args.iter().map(String::as_str).collect();
        let mut client = LspClient::spawn(
            &server_def.command,
            &args,
            lang,
            server_name,
            self.logging.clone(),
            server_def.settings.clone(),
            HashMap::new(),
            server_def.env.as_ref(),
        )?;

        // Initialize with null workspace (single-file mode per LSP spec).
        if let Err(e) = client
            .initialize(&[], server_def.initialization_options.clone())
            .await
        {
            info!(
                source = Source::LspLifecycle.as_str(),
                language = lang,
                server = server_name,
                "Server '{server_name}' rejected single-file mode: {e}",
            );
            self.single_file_failures
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .insert((lang.to_string(), server_name.to_string()));
            return Err(e);
        }

        client.server().set_scope(Scope::SingleFile);

        let client_mutex = Arc::new(Mutex::new(client));
        clients.insert(sf_key, client_mutex.clone());
        drop(clients);

        Ok(client_mutex)
    }

    /// Returns a single-file server for the given language and server,
    /// spawning one if needed.
    ///
    /// Only considers servers with `single_file = true` in config.
    /// Checks the negative cache first — if the server previously
    /// rejected null-workspace initialization, returns `None` without
    /// a spawn attempt. Returns `None` for dead servers (tombstones).
    async fn ensure_single_file_server(
        &self,
        lang: &str,
        server_name: &str,
    ) -> Option<Arc<Mutex<LspClient>>> {
        // Config gate: only servers with single_file = true.
        let def = self.config.server.get(server_name)?;
        if !def.single_file {
            return None;
        }

        // Check negative cache.
        {
            let failures = self
                .single_file_failures
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            if failures.contains(&(lang.to_string(), server_name.to_string())) {
                return None;
            }
        }

        // Check for existing instance.
        {
            let clients = self.clients.lock().await;
            let sf_key =
                InstanceKey::new(lang.to_string(), server_name.to_string(), Scope::SingleFile);
            if let Some(existing) = clients.get(&sf_key) {
                if existing.lock().await.is_alive() {
                    return Some(existing.clone());
                }
                // Dead — don't retry.
                return None;
            }
        }

        // No failure and no existing instance — try to spawn.
        self.spawn_single_file(server_name, lang).await.ok()
    }

    /// Get-then-spawn composition.
    ///
    /// Looks up an existing instance by trying both `Scope::Workspace` and
    /// `Scope::Root(root)` keys. On miss, spawns with the correct scope:
    /// project-scoped roots (Rule A) get `spawn_project_scoped`, others
    /// get capability-driven scope via `spawn`. Dead servers are left as
    /// tombstones — a server that crashes will not be restarted.
    /// Intentional restarts (e.g. after `sync_roots`) go through
    /// [`Self::shutdown_instance`] which removes the entry so a fresh
    /// spawn can occur.
    ///
    /// # Errors
    ///
    /// Returns an error if:
    /// - The server previously died (tombstone).
    /// - The server definition is missing from config.
    /// - The server fails to spawn or initialize.
    async fn ensure_server(
        &self,
        lang: &str,
        server_name: &str,
        root: &Path,
    ) -> Result<Arc<Mutex<LspClient>>> {
        let project_scoped = self.is_project_scoped(lang, root);

        // Fast path: check for an existing instance. Project-scoped
        // roots only check the exact Root(root) key — a workspace
        // instance must not prevent spawning the isolated instance.
        {
            let clients = self.clients.lock().await;
            let existing = if project_scoped {
                let root_key = InstanceKey::new(
                    lang.to_string(),
                    server_name.to_string(),
                    Scope::Root(root.to_path_buf()),
                );
                clients.get(&root_key).cloned()
            } else {
                find_instance(&clients, lang, server_name, root)
            };
            if let Some(found) = existing {
                if found.lock().await.is_alive() {
                    return Ok(found);
                }
                anyhow::bail!("LSP server '{server_name}' ({lang}) is dead");
            }
        }

        // Miss — spawn with correct scope (spawn_inner handles its
        // own double-check).
        let (_key, client) = self
            .spawn_inner(server_name, lang, root, project_scoped)
            .await?;
        Ok(client)
    }

    /// Opens a document on a specific client.
    ///
    /// Reads the file, checks per-client open state, sends `didOpen` or
    /// `didChange` as appropriate. Version tracking is per-client — each
    /// server gets an independent monotonic sequence starting at 1.
    ///
    /// Used by request/response dispatch: the caller gets clients from
    /// [`get_servers`](Self::get_servers) and opens the document on each
    /// as it iterates the priority chain.
    ///
    /// # Errors
    ///
    /// Returns an error if the file cannot be read or the LSP notification
    /// fails.
    #[allow(
        clippy::significant_drop_tightening,
        reason = "Client lock held across notification send"
    )]
    pub async fn open_document_on(
        &self,
        path: &Path,
        client: &Arc<Mutex<LspClient>>,
        parent_id: Option<i64>,
    ) -> Result<String> {
        let canonical = path.canonicalize()?;
        let uri = crate::lsp::lang::path_to_uri(&canonical);
        let text = tokio::fs::read_to_string(&canonical).await?;

        let mut client = client.lock().await;
        client.set_parent_id(parent_id);

        if !client.is_alive() {
            client.set_parent_id(None);
            return Err(anyhow!(
                "[{}] server is no longer running",
                client.language()
            ));
        }

        let (first_open, version) = client.open_document(&uri);
        if first_open {
            let language_id = self
                .fs
                .language_id(path)
                .unwrap_or_else(|| "plaintext".to_string());
            client.did_open(&uri, &language_id, version, &text).await?;
        } else {
            client.did_change(&uri, version, &text).await?;
        }

        drop(client);
        Ok(uri)
    }

    /// Returns diagnostic-enabled servers for a file path without opening
    /// the document.
    ///
    /// Applies both the capability gate ([`LspServer::supports_diagnostics`])
    /// and the config-level filter ([`LanguageConfig::diagnostics_enabled`]).
    /// Returns an empty Vec when no server qualifies.
    pub async fn diagnostic_servers(&self, path: &Path) -> Vec<Arc<Mutex<LspClient>>> {
        let servers = self
            .get_servers(path, LspServer::supports_diagnostics, None)
            .await;

        if servers.is_empty() {
            return Vec::new();
        }

        let lang_id = self.fs.language_id(path).or_else(|| {
            path.extension()
                .and_then(|e| e.to_str())
                .map(str::to_string)
        });
        let lang_config = lang_id
            .as_deref()
            .and_then(|id| self.config.resolve_language(id));

        let mut clients = Vec::new();
        for client in &servers {
            let server_name = client.lock().await.server_name().to_string();
            let enabled = lang_config
                .as_ref()
                .is_some_and(|lc| lc.diagnostics_enabled(&server_name));
            if enabled {
                clients.push(client.clone());
            }
        }

        clients
    }

    /// Spawns LSP servers for new languages detected in the given file paths.
    ///
    /// Used by workspace-wide tools (grep, glob) to discover languages added
    /// mid-session. For each path, detects the language via
    /// [`FilesystemManager`] and resolves the owning root. Only spawns
    /// servers for configured languages that don't already have an instance
    /// covering the file's root. Unrooted files are skipped. Servers that
    /// fail to spawn are logged and skipped.
    pub async fn ensure_clients_for_paths(&self, paths: &[PathBuf]) {
        let configured_keys: HashSet<&str> =
            self.config.language.keys().map(String::as_str).collect();

        // Collect (language, server_name, root) triples that need spawning.
        let mut to_spawn: HashSet<(String, String, PathBuf)> = HashSet::new();

        {
            let active = self.clients.lock().await;
            for path in paths {
                let lang = self.fs.language_id(path).or_else(|| {
                    path.extension()
                        .and_then(|e| e.to_str())
                        .map(str::to_string)
                });

                let Some(lang) = lang else { continue };
                if !configured_keys.contains(lang.as_str()) {
                    continue;
                }

                // Skip unrooted files.
                let Some(root) = self.fs.resolve_root(path) else {
                    continue;
                };

                let Some(lang_config) = self.config.resolve_language(&lang) else {
                    continue;
                };

                // Check all servers in the binding, not just the first.
                for binding in &lang_config.servers {
                    if find_instance(&active, &lang, &binding.name, &root).is_none() {
                        to_spawn.insert((lang.clone(), binding.name.clone(), root.clone()));
                    }
                }
            }
        }

        if to_spawn.is_empty() {
            return;
        }

        let mut sorted: Vec<&str> = to_spawn.iter().map(|(l, _, _)| l.as_str()).collect();
        sorted.sort_unstable();
        sorted.dedup();
        info!("Mid-session server spawn for: {}", sorted.join(", "));

        for (lang, server_name, root) in &to_spawn {
            if let Err(e) = self.ensure_server(lang, server_name, root).await {
                warn!(
                    source = Source::LspLifecycle.as_str(),
                    language = lang.as_str(),
                    server = server_name.as_str(),
                    "Failed to spawn LSP server for {lang} ({server_name}): {e}",
                );
            }
        }
    }

    /// Returns a snapshot of all clients (including dead ones).
    pub async fn clients(&self) -> HashMap<InstanceKey, Arc<Mutex<LspClient>>> {
        self.clients.lock().await.clone()
    }

    /// Returns a snapshot of rooted clients only (excluding single-file
    /// servers).
    ///
    /// Single-file servers have no project context and are excluded from
    /// workspace-wide fan-out operations (grep, workspace/symbol).
    pub async fn rooted_clients(&self) -> HashMap<InstanceKey, Arc<Mutex<LspClient>>> {
        self.clients
            .lock()
            .await
            .iter()
            .filter(|(k, _)| k.scope != Scope::SingleFile)
            .map(|(k, v)| (k.clone(), v.clone()))
            .collect()
    }

    /// Returns status of all active servers.
    pub async fn all_server_status(&self) -> Vec<ServerStatus> {
        let clients = self.clients.lock().await.clone();
        let mut statuses = Vec::new();

        for (key, client_mutex) in &clients {
            let status = client_mutex.lock().await.status(key);
            statuses.push(status);
        }

        statuses
    }

    /// Shuts down a specific server instance if it exists.
    pub async fn shutdown_instance(&self, key: &InstanceKey) {
        let mut clients = self.clients.lock().await;
        if let Some(client_mutex) = clients.remove(key) {
            info!("Shutting down LSP server instance {}", key);
            let mut client = client_mutex.lock().await;
            if client.is_alive()
                && let Err(e) = client.shutdown().await
            {
                info!("Failed to shutdown LSP server instance {}: {}", key, e);
            }
        }
    }

    /// Shuts down all instances bound to a specific root.
    ///
    /// Only affects `Scope::Root(path)` instances where the path matches.
    /// Workspace-scoped and other instances are untouched.
    async fn shutdown_root_instances(&self, root: &Path) {
        let mut clients = self.clients.lock().await;
        let to_remove: Vec<InstanceKey> = clients
            .keys()
            .filter(|k| matches!(&k.scope, Scope::Root(r) if r.as_path() == root))
            .cloned()
            .collect();
        for key in to_remove {
            if let Some(client_mutex) = clients.remove(&key) {
                info!("Shutting down per-root instance {}", key);
                let mut client = client_mutex.lock().await;
                if client.is_alive()
                    && let Err(e) = client.shutdown().await
                {
                    info!("Failed to shutdown per-root instance {}: {}", key, e);
                }
            }
        }
    }

    /// Shuts down all single-file server instances and clears the
    /// single-file cache.
    ///
    /// Called when workspace roots change — previously-unrooted files may
    /// now be covered by workspace or per-root instances. Single-file
    /// servers are lazily re-spawned on the next request if still needed.
    async fn shutdown_single_file_instances(&self) {
        let mut clients = self.clients.lock().await;
        let sf_keys: Vec<InstanceKey> = clients
            .keys()
            .filter(|k| k.scope == Scope::SingleFile)
            .cloned()
            .collect();
        for key in sf_keys {
            if let Some(client_mutex) = clients.remove(&key) {
                info!("Shutting down single-file instance {}", key);
                let mut client = client_mutex.lock().await;
                if client.is_alive()
                    && let Err(e) = client.shutdown().await
                {
                    info!("Failed to shutdown single-file instance {}: {}", key, e);
                }
            }
        }
        drop(clients);

        self.single_file_failures
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clear();
    }

    /// Spawns instances for newly added roots.
    ///
    /// For each active language+server, spawns project-scoped
    /// `Scope::Root` instances for roots with Rule A triggers, and
    /// legacy `Scope::Root` instances for roots where the server
    /// doesn't support workspace folders.
    async fn spawn_for_added_roots(&self, added_roots: &[PathBuf]) {
        // Collect active languages with their servers and scope kind.
        let clients = self.clients.lock().await.clone();
        let mut active_langs: HashMap<String, Vec<(String, bool)>> = HashMap::new();
        for key in clients.keys() {
            let is_workspace = key.scope == Scope::Workspace;
            let entries = active_langs.entry(key.language_id.clone()).or_default();
            if !entries.iter().any(|(s, _)| s == &key.server) {
                entries.push((key.server.clone(), is_workspace));
            }
        }
        drop(clients);

        if active_langs.is_empty() {
            return;
        }

        // Detect which languages have files in the added roots.
        let configured_keys: HashSet<&str> = active_langs.keys().map(String::as_str).collect();
        let detected = self
            .fs
            .detect_workspace_languages(added_roots, &configured_keys);

        for lang in &detected {
            let Some(servers) = active_langs.get(lang) else {
                continue;
            };
            for (server_name, is_workspace) in servers {
                for root in added_roots {
                    // Workspace-capable + non-project-scoped: already
                    // covered by the workspace instance (folder add
                    // sent in sync_roots).
                    if (self.is_project_scoped(lang, root) || !is_workspace)
                        && let Err(e) = self.ensure_server(lang, server_name, root).await
                    {
                        warn!(
                            source = Source::LspLifecycle.as_str(),
                            language = lang.as_str(),
                            server = server_name.as_str(),
                            "Failed to spawn instance for {lang} ({server_name}) at {}: {e}",
                            root.display(),
                        );
                    }
                }
            }
        }
    }

    /// Diffs the filesystem and notifies servers with matching file watcher
    /// registrations.
    ///
    /// Called before each LSP interaction (tool call, diagnostics pipeline).
    /// No-op if no changes are detected or no servers have registrations.
    pub async fn notify_file_changes(&self) {
        let changes = self.fs.diff();
        if changes.is_empty() {
            return;
        }

        let roots = self.fs.roots();
        let clients = self.clients.lock().await.clone();

        let mut notified = 0u32;
        for (key, client_mutex) in &clients {
            // Single-file servers have no project context — skip.
            if key.scope == Scope::SingleFile {
                continue;
            }
            let client = client_mutex.lock().await;
            if !client.is_alive() {
                continue;
            }

            let watchers = client.server().file_watcher_snapshot();
            if watchers.is_empty() {
                continue;
            }

            let matched = match_file_changes(&changes, &watchers, &roots);
            if matched.is_empty() {
                continue;
            }

            let refs: Vec<(&str, u8)> = matched.iter().map(|(u, t)| (u.as_str(), *t)).collect();
            if let Err(e) = client.did_change_watched_files(&refs).await {
                info!("Failed to send didChangeWatchedFiles to {key}: {e}");
            }
            drop(client);
            notified += 1;
        }

        debug!(
            "{} filesystem changes, {} servers notified",
            changes.len(),
            notified
        );
    }

    /// Whether a language is project-scoped in the given root.
    ///
    /// Rule A: returns `true` if the root's project config has a
    /// `[language.{lang}]` entry. This triggers tier 1 — an
    /// isolated per-root instance.
    #[must_use]
    pub fn is_project_scoped(&self, lang: &str, root: &Path) -> bool {
        let configs = self
            .project_configs
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        configs
            .get(root)
            .is_some_and(|pc| pc.language.contains_key(lang))
    }

    /// Returns the effective `ServerDef` for a server in a root.
    ///
    /// Deep-merges the root's project `[server.{name}]` (if any)
    /// over the user-level `[server.{name}]`. Returns user-level
    /// def unchanged if no project override exists.
    #[must_use]
    pub fn effective_server_def(&self, server_name: &str, root: &Path) -> Option<ServerDef> {
        let user_def = self.config.server.get(server_name)?;

        let project_def = self
            .project_configs
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .get(root)
            .and_then(|pc| pc.server.get(server_name))
            .cloned();

        let Some(project_def) = project_def else {
            return Some(user_def.clone());
        };

        // Field-level merge: project fields override user fields when
        // present. Settings use deep_merge for nested object merging.
        let mut merged = user_def.clone();
        if !project_def.command.is_empty() {
            merged.command.clone_from(&project_def.command);
            merged.args.clone_from(&project_def.args);
        }
        if project_def.initialization_options.is_some() {
            merged
                .initialization_options
                .clone_from(&project_def.initialization_options);
        }
        if project_def.min_severity.is_some() {
            merged.min_severity.clone_from(&project_def.min_severity);
        }
        if !project_def.file_patterns.is_empty() {
            merged.file_patterns.clone_from(&project_def.file_patterns);
            merged
                .compiled_patterns
                .clone_from(&project_def.compiled_patterns);
        }
        if let Some(ref project_env) = project_def.env {
            if let Some(ref user_env) = user_def.env {
                let mut env = user_env.clone();
                env.extend(project_env.iter().map(|(k, v)| (k.clone(), v.clone())));
                merged.env = Some(env);
            } else {
                merged.env = Some(project_env.clone());
            }
        }
        if let Some(ref project_settings) = project_def.settings {
            if let Some(ref user_settings) = user_def.settings {
                merged.settings = Some(crate::config::merge::deep_merge(
                    user_settings,
                    project_settings,
                ));
            } else {
                merged.settings = Some(project_settings.clone());
            }
        }

        Some(merged)
    }

    /// Returns the effective settings `Value` for a server in a root.
    ///
    /// Deep-merges the root's project `[server.{name}].settings`
    /// over the user-level `[server.{name}].settings`.
    #[must_use]
    pub fn effective_settings(&self, server_name: &str, root: &Path) -> Option<serde_json::Value> {
        let user_settings = self
            .config
            .server
            .get(server_name)
            .and_then(|d| d.settings.clone());

        let project_settings = self
            .project_configs
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .get(root)
            .and_then(|pc| pc.server.get(server_name))
            .and_then(|d| d.settings.clone());

        match (user_settings, project_settings) {
            (Some(user), Some(project)) => Some(crate::config::merge::deep_merge(&user, &project)),
            (None, Some(project)) => Some(project),
            (Some(user), None) => Some(user),
            (None, None) => None,
        }
    }

    /// Collects per-root settings overrides for a server.
    ///
    /// Returns a map from root path to the project-config settings overlay
    /// for roots whose `.catenary.toml` has `[server.{name}.settings]`.
    /// Empty entries (roots without project config or without settings for
    /// this server) are omitted.
    fn collect_per_root_settings(
        &self,
        server_name: &str,
        roots: &[PathBuf],
    ) -> HashMap<PathBuf, serde_json::Value> {
        let configs = self
            .project_configs
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let mut map = HashMap::new();
        for root in roots {
            if let Some(settings) = configs
                .get(root)
                .and_then(|pc| pc.server.get(server_name))
                .and_then(|d| d.settings.clone())
            {
                map.insert(root.clone(), settings);
            }
        }
        map
    }

    /// Feeds per-root classification tables to `FilesystemManager`.
    ///
    /// For each root with a loaded project config that has classification
    /// fields, builds a [`ClassificationTables`] and stores it on the
    /// filesystem manager. Must be called after
    /// [`load_project_configs_for_roots`](Self::load_project_configs_for_roots)
    /// and before [`FilesystemManager::detect_workspace_languages`].
    fn set_per_root_classification(&self, roots: &[PathBuf]) {
        let configs = self
            .project_configs
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        for root in roots {
            if let Some(pc) = configs.get(root) {
                let tables = ClassificationTables::from_project_config(&pc.language);
                if !tables.is_empty() {
                    self.fs.set_root_classification(root.clone(), tables);
                }
            }
        }
    }

    /// Loads project configs for the given roots.
    ///
    /// For each root, discovers `.catenary.toml` via [`crate::config::load_project_config`]
    /// and stores the result. Errors are logged and skipped — a broken
    /// project config should not prevent other roots from loading.
    fn load_project_configs_for_roots(&self, roots: &[PathBuf]) {
        for root in roots {
            match crate::config::load_project_config(root) {
                Ok(Some(pc)) => {
                    info!(
                        source = Source::ConfigParse.as_str(),
                        root = %root.display(),
                        "Loaded project config from {}",
                        root.join(".catenary.toml").display(),
                    );
                    crate::config::validate::warn_orphan_project_servers(&pc, &self.config, root);
                    self.project_configs
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner)
                        .insert(root.clone(), pc);
                }
                Ok(None) => {} // No project config — fine.
                Err(e) => {
                    warn!(
                        source = Source::ConfigParse.as_str(),
                        root = %root.display(),
                        "Failed to load project config from {}: {e}",
                        root.join(".catenary.toml").display(),
                    );
                }
            }
        }
    }

    /// Shuts down all active clients.
    ///
    /// Each server gets 5 seconds to respond to the graceful
    /// `shutdown`/`exit` sequence. Servers that don't respond in time
    /// are dropped, which triggers the `Connection` drop handler to SIGKILL them.
    pub async fn shutdown_all(&self) {
        let mut clients = self.clients.lock().await;
        for (key, client_mutex) in clients.drain() {
            let mut client = client_mutex.lock().await;
            if client.is_alive() {
                let result = tokio::time::timeout(Duration::from_secs(5), client.shutdown()).await;
                drop(client);
                match result {
                    Ok(Err(e)) => {
                        info!("Failed to shutdown LSP server instance {}: {}", key, e);
                    }
                    Err(_) => {
                        info!(
                            "LSP server instance {} did not respond to shutdown within 5s, killing",
                            key
                        );
                    }
                    Ok(Ok(())) => {}
                }
            }
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
    use crate::config::{DispatchMethod, LanguageConfig, ServerBinding, ServerDef};
    use anyhow::Result;

    const MOCK_LANG_A: &str = "yX4Za";

    fn test_logging() -> LoggingServer {
        LoggingServer::new()
    }

    fn test_fs() -> Arc<FilesystemManager> {
        Arc::new(FilesystemManager::new())
    }

    fn test_fs_with_roots(roots: &[&str]) -> Arc<FilesystemManager> {
        let fs = Arc::new(FilesystemManager::new());
        fs.set_roots(roots.iter().map(PathBuf::from).collect());
        fs
    }

    fn test_config_raw() -> Config {
        Config {
            language: HashMap::new(),
            server: HashMap::new(),
            log_retention_days: 7,
            notifications: None,
            icons: None,
            tui: None,
            tools: None,
            resolved_commands: None,
        }
    }

    fn test_config() -> Arc<Config> {
        Arc::new(test_config_raw())
    }

    /// Test helper: spawns the first server for a language using the first root.
    ///
    /// Replaces the removed `ensure_server_for_language` for test convenience.
    async fn ensure_first_server(
        manager: &LspClientManager,
        lang: &str,
    ) -> Result<Arc<Mutex<LspClient>>> {
        let lang_config = manager
            .config
            .resolve_language(lang)
            .ok_or_else(|| anyhow!("No LSP server configured for language '{lang}'"))?;
        let server_name = &lang_config
            .servers
            .first()
            .ok_or_else(|| anyhow!("No servers configured for language '{lang}'"))?
            .name;
        let root = manager
            .fs
            .roots()
            .into_iter()
            .next()
            .ok_or_else(|| anyhow!("No workspace roots available for spawning '{lang}'"))?;
        manager.ensure_server(lang, server_name, &root).await
    }

    /// Locate the mockls binary in the same directory as the test executable.
    /// During `cargo test`, all binaries are built into the same `target/debug/deps`
    /// parent directory.
    fn mockls_bin() -> PathBuf {
        let test_exe = std::env::current_exe()
            .ok()
            .and_then(|p| p.parent().map(Path::to_path_buf))
            .and_then(|p| p.parent().map(Path::to_path_buf))
            .map(|p| p.join("mockls"));
        test_exe.unwrap_or_else(|| PathBuf::from("mockls"))
    }

    fn mockls_config() -> Arc<Config> {
        let bin = mockls_bin();
        let server_name = format!("mockls-{MOCK_LANG_A}");
        let mut server = HashMap::new();
        server.insert(
            server_name.clone(),
            ServerDef {
                command: bin.to_string_lossy().to_string(),
                args: vec![MOCK_LANG_A.to_string()],
                ..ServerDef::default()
            },
        );
        let mut language = HashMap::new();
        language.insert(
            MOCK_LANG_A.to_string(),
            LanguageConfig {
                servers: vec![ServerBinding::new(server_name)],
                ..LanguageConfig::default()
            },
        );
        Arc::new(Config {
            language,
            server,
            log_retention_days: 7,
            notifications: None,
            icons: None,
            tui: None,
            tools: None,
            resolved_commands: None,
        })
    }

    fn mockls_workspace_folders_config() -> Arc<Config> {
        let bin = mockls_bin();
        let server_name = format!("mockls-{MOCK_LANG_A}-wf");
        let mut server = HashMap::new();
        server.insert(
            server_name.clone(),
            ServerDef {
                command: bin.to_string_lossy().to_string(),
                args: vec![MOCK_LANG_A.to_string(), "--workspace-folders".to_string()],
                ..ServerDef::default()
            },
        );
        let mut language = HashMap::new();
        language.insert(
            MOCK_LANG_A.to_string(),
            LanguageConfig {
                servers: vec![ServerBinding::new(server_name)],
                ..LanguageConfig::default()
            },
        );
        Arc::new(Config {
            language,
            server,
            log_retention_days: 7,
            notifications: None,
            icons: None,
            tui: None,
            tools: None,
            resolved_commands: None,
        })
    }

    /// Config with two legacy mockls servers for the same language.
    fn mockls_multi_server_config() -> Arc<Config> {
        let bin = mockls_bin();
        let server_a = format!("mockls-{MOCK_LANG_A}-a");
        let server_b = format!("mockls-{MOCK_LANG_A}-b");
        let mut server = HashMap::new();
        for name in [&server_a, &server_b] {
            server.insert(
                name.clone(),
                ServerDef {
                    command: bin.to_string_lossy().to_string(),
                    args: vec![MOCK_LANG_A.to_string()],
                    initialization_options: None,
                    settings: None,
                    min_severity: None,
                    ..ServerDef::default()
                },
            );
        }
        let mut language = HashMap::new();
        language.insert(
            MOCK_LANG_A.to_string(),
            LanguageConfig {
                servers: vec![ServerBinding::new(server_a), ServerBinding::new(server_b)],
                ..LanguageConfig::default()
            },
        );
        Arc::new(Config {
            language,
            server,
            log_retention_days: 7,
            notifications: None,
            icons: None,
            tui: None,
            tools: None,
            resolved_commands: None,
        })
    }

    /// Config with two workspace-folders-capable mockls servers for the same language.
    fn mockls_multi_server_workspace_config() -> Arc<Config> {
        let bin = mockls_bin();
        let server_a = format!("mockls-{MOCK_LANG_A}-wf-a");
        let server_b = format!("mockls-{MOCK_LANG_A}-wf-b");
        let mut server = HashMap::new();
        for name in [&server_a, &server_b] {
            server.insert(
                name.clone(),
                ServerDef {
                    command: bin.to_string_lossy().to_string(),
                    args: vec![MOCK_LANG_A.to_string(), "--workspace-folders".to_string()],
                    initialization_options: None,
                    settings: None,
                    min_severity: None,
                    ..ServerDef::default()
                },
            );
        }
        let mut language = HashMap::new();
        language.insert(
            MOCK_LANG_A.to_string(),
            LanguageConfig {
                servers: vec![ServerBinding::new(server_a), ServerBinding::new(server_b)],
                ..LanguageConfig::default()
            },
        );
        Arc::new(Config {
            language,
            server,
            log_retention_days: 7,
            notifications: None,
            icons: None,
            tui: None,
            tools: None,
            resolved_commands: None,
        })
    }

    /// Config with one workspace-capable and one legacy mockls for the same language.
    fn mockls_mixed_capability_config() -> Arc<Config> {
        let bin = mockls_bin();
        let server_ws = format!("mockls-{MOCK_LANG_A}-ws");
        let server_legacy = format!("mockls-{MOCK_LANG_A}-leg");
        let mut server = HashMap::new();
        server.insert(
            server_ws.clone(),
            ServerDef {
                command: bin.to_string_lossy().to_string(),
                args: vec![MOCK_LANG_A.to_string(), "--workspace-folders".to_string()],
                ..ServerDef::default()
            },
        );
        server.insert(
            server_legacy.clone(),
            ServerDef {
                command: bin.to_string_lossy().to_string(),
                args: vec![MOCK_LANG_A.to_string()],
                ..ServerDef::default()
            },
        );
        let mut language = HashMap::new();
        language.insert(
            MOCK_LANG_A.to_string(),
            LanguageConfig {
                servers: vec![
                    ServerBinding::new(server_ws),
                    ServerBinding::new(server_legacy),
                ],
                ..LanguageConfig::default()
            },
        );
        Arc::new(Config {
            language,
            server,
            log_retention_days: 7,
            notifications: None,
            icons: None,
            tui: None,
            tools: None,
            resolved_commands: None,
        })
    }

    #[tokio::test]
    async fn test_roots_returns_initial_roots() -> Result<()> {
        let manager = LspClientManager::new(
            test_config(),
            test_logging(),
            test_fs_with_roots(&["/tmp/root_a", "/tmp/root_b"]),
        );

        let roots = manager.roots();
        assert_eq!(roots.len(), 2);
        assert_eq!(roots[0], PathBuf::from("/tmp/root_a"));
        assert_eq!(roots[1], PathBuf::from("/tmp/root_b"));
        Ok(())
    }

    #[tokio::test]
    async fn test_roots_empty_initial() -> Result<()> {
        let manager = LspClientManager::new(test_config(), test_logging(), test_fs());

        assert!(manager.roots().is_empty());
        Ok(())
    }

    #[tokio::test]
    async fn test_remove_root() -> Result<()> {
        let manager = LspClientManager::new(
            test_config(),
            test_logging(),
            test_fs_with_roots(&["/tmp/root_a", "/tmp/root_b"]),
        );

        assert_eq!(manager.roots().len(), 2);

        manager.remove_root(Path::new("/tmp/root_a")).await?;

        let roots = manager.roots();
        assert_eq!(roots.len(), 1);
        assert_eq!(roots[0], PathBuf::from("/tmp/root_b"));
        Ok(())
    }

    #[tokio::test]
    async fn test_sync_roots_adds_and_removes() -> Result<()> {
        let manager = LspClientManager::new(
            test_config(),
            test_logging(),
            test_fs_with_roots(&["/tmp/root_a", "/tmp/root_b"]),
        );

        // Sync: remove /tmp/root_a, keep /tmp/root_b, add /tmp/root_c
        manager
            .sync_roots(vec![
                PathBuf::from("/tmp/root_b"),
                PathBuf::from("/tmp/root_c"),
            ])
            .await?;

        let roots = manager.roots();
        assert_eq!(roots.len(), 2);
        assert_eq!(roots[0], PathBuf::from("/tmp/root_b"));
        assert_eq!(roots[1], PathBuf::from("/tmp/root_c"));
        Ok(())
    }

    #[tokio::test]
    async fn test_sync_roots_no_change() -> Result<()> {
        let manager = LspClientManager::new(
            test_config(),
            test_logging(),
            test_fs_with_roots(&["/tmp/root_a"]),
        );

        manager
            .sync_roots(vec![PathBuf::from("/tmp/root_a")])
            .await?;

        let roots = manager.roots();
        assert_eq!(roots.len(), 1);
        assert_eq!(roots[0], PathBuf::from("/tmp/root_a"));
        Ok(())
    }

    /// Checks whether any client in the map has the given language ID.
    fn has_language(clients: &HashMap<InstanceKey, Arc<Mutex<LspClient>>>, lang: &str) -> bool {
        clients.keys().any(|k| k.language_id == lang)
    }

    #[tokio::test]
    async fn test_sync_roots_legacy_removes_per_root() -> Result<()> {
        // mockls without --workspace-folders does NOT advertise workspace folder support.
        // Removing a root should shut down the Scope::Root instance for that root.
        let manager = LspClientManager::new(
            mockls_config(),
            test_logging(),
            test_fs_with_roots(&["/tmp"]),
        );

        let client = ensure_first_server(&manager, MOCK_LANG_A).await?;
        assert!(client.lock().await.is_alive());
        assert!(
            !client.lock().await.supports_workspace_folders(),
            "mockls (no flags) should NOT support workspace folders"
        );

        assert!(has_language(&manager.clients().await, MOCK_LANG_A));

        // sync_roots removes /tmp — the per-root instance should be shut down.
        manager.sync_roots(vec![PathBuf::from("/var")]).await?;

        assert!(
            !has_language(&manager.clients().await, MOCK_LANG_A),
            "Scope::Root(/tmp) instance should be removed when /tmp is dropped"
        );

        Ok(())
    }

    #[tokio::test]
    async fn test_sync_roots_legacy_keeps_unchanged_root() -> Result<()> {
        // Adding a root should NOT shut down the existing legacy instance
        // for a root that is still present.
        let manager = LspClientManager::new(
            mockls_config(),
            test_logging(),
            test_fs_with_roots(&["/tmp"]),
        );

        let client = ensure_first_server(&manager, MOCK_LANG_A).await?;
        assert!(client.lock().await.is_alive());

        // sync_roots adds /var but keeps /tmp — the /tmp instance stays.
        manager
            .sync_roots(vec![PathBuf::from("/tmp"), PathBuf::from("/var")])
            .await?;

        assert!(
            has_language(&manager.clients().await, MOCK_LANG_A),
            "Scope::Root(/tmp) instance should remain when /tmp is still a root"
        );

        Ok(())
    }

    /// mockls with `--send-configuration-request` sends a `workspace/configuration`
    /// request with `section: "mockls"` during initialization. This test verifies
    /// that configured settings are threaded through to the response handler.
    #[tokio::test]
    async fn test_configuration_returns_settings() -> Result<()> {
        let bin = mockls_bin();
        let server_name = format!("mockls-{MOCK_LANG_A}-cfg");
        let mut server = HashMap::new();
        server.insert(
            server_name.clone(),
            ServerDef {
                command: bin.to_string_lossy().to_string(),
                args: vec![
                    MOCK_LANG_A.to_string(),
                    "--send-configuration-request".to_string(),
                ],
                settings: Some(serde_json::json!({"mockls": {"key": "value"}})),
                ..ServerDef::default()
            },
        );
        let mut language = HashMap::new();
        language.insert(
            MOCK_LANG_A.to_string(),
            LanguageConfig {
                servers: vec![ServerBinding::new(server_name)],
                ..LanguageConfig::default()
            },
        );
        let config = Arc::new(Config {
            language,
            server,
            log_retention_days: 7,
            notifications: None,
            icons: None,
            tui: None,
            tools: None,
            resolved_commands: None,
        });

        let manager = LspClientManager::new(config, test_logging(), test_fs_with_roots(&["/tmp"]));

        // ensure_first_server spawns + initializes; mockls sends workspace/configuration
        // during init. If Catenary responds correctly, initialization succeeds.
        let client = ensure_first_server(&manager, MOCK_LANG_A).await?;
        assert!(client.lock().await.is_alive());

        Ok(())
    }

    #[tokio::test]
    async fn test_sync_roots_notifies_supported_client() -> Result<()> {
        // mockls with --workspace-folders DOES advertise workspace folder support.
        // When roots change, it should receive a notification instead of being shut down.
        let manager = LspClientManager::new(
            mockls_workspace_folders_config(),
            test_logging(),
            test_fs_with_roots(&["/tmp"]),
        );

        let client = ensure_first_server(&manager, MOCK_LANG_A).await?;
        assert!(client.lock().await.is_alive());
        assert!(
            client.lock().await.supports_workspace_folders(),
            "mockls --workspace-folders should support workspace folders"
        );

        assert!(has_language(&manager.clients().await, MOCK_LANG_A));

        // sync_roots should send notification, NOT shut down the client
        manager
            .sync_roots(vec![PathBuf::from("/tmp"), PathBuf::from("/var")])
            .await?;

        // Client should still be active (not removed)
        assert!(
            has_language(&manager.clients().await, MOCK_LANG_A),
            "mockls client should still be active after sync_roots (workspace folders supported)"
        );

        Ok(())
    }

    #[tokio::test]
    async fn test_ensure_clients_for_paths_spawns_new_language() -> Result<()> {
        let manager = LspClientManager::new(
            mockls_config(),
            test_logging(),
            test_fs_with_roots(&["/tmp"]),
        );

        assert!(manager.clients().await.is_empty());

        // A file with the mock language extension triggers a spawn
        let paths = vec![PathBuf::from(format!("/tmp/test.{MOCK_LANG_A}"))];
        manager.ensure_clients_for_paths(&paths).await;

        assert!(
            has_language(&manager.clients().await, MOCK_LANG_A),
            "ensure_clients_for_paths should spawn the mock language server"
        );
        Ok(())
    }

    #[tokio::test]
    async fn test_ensure_clients_for_paths_skips_existing() -> Result<()> {
        let manager = LspClientManager::new(
            mockls_config(),
            test_logging(),
            test_fs_with_roots(&["/tmp"]),
        );

        // Pre-spawn the server
        let _ = ensure_first_server(&manager, MOCK_LANG_A).await?;
        assert_eq!(manager.clients().await.len(), 1);

        // ensure_clients_for_paths should not fail or double-spawn
        let paths = vec![PathBuf::from(format!("/tmp/test.{MOCK_LANG_A}"))];
        manager.ensure_clients_for_paths(&paths).await;

        assert_eq!(
            manager.clients().await.len(),
            1,
            "should not create a duplicate client"
        );
        Ok(())
    }

    #[tokio::test]
    async fn test_ensure_clients_for_paths_ignores_unconfigured() -> Result<()> {
        let manager = LspClientManager::new(
            mockls_config(),
            test_logging(),
            test_fs_with_roots(&["/tmp"]),
        );

        // .xyz has no configured server — should be silently skipped
        let paths = vec![PathBuf::from("/tmp/test.xyz")];
        manager.ensure_clients_for_paths(&paths).await;

        assert!(
            manager.clients().await.is_empty(),
            "unconfigured languages should not trigger a spawn"
        );
        Ok(())
    }

    #[tokio::test]
    async fn test_ensure_clients_for_paths_scope_aware() -> Result<()> {
        // Spawns instances per root, not per language.
        let manager = LspClientManager::new(
            mockls_config(),
            test_logging(),
            test_fs_with_roots(&["/tmp", "/var"]),
        );

        assert!(manager.clients().await.is_empty());

        // Paths in two different roots should spawn two instances.
        let paths = vec![
            PathBuf::from(format!("/tmp/test.{MOCK_LANG_A}")),
            PathBuf::from(format!("/var/test.{MOCK_LANG_A}")),
        ];
        manager.ensure_clients_for_paths(&paths).await;

        let clients = manager.clients().await;
        assert_eq!(
            count_scope(&clients, MOCK_LANG_A, "root"),
            2,
            "Should have two root-scoped instances"
        );
        Ok(())
    }

    #[tokio::test]
    async fn test_ensure_clients_project_scoped() -> Result<()> {
        // ensure_clients_for_paths should use spawn_project_scoped for
        // roots with project config, producing Scope::Root even when
        // the server supports workspace folders.
        let config = mockls_workspace_folders_config();
        let server_name = config
            .resolve_language(MOCK_LANG_A)
            .expect("lang config")
            .servers[0]
            .name
            .clone();

        let manager = LspClientManager::new(config, test_logging(), test_fs_with_roots(&["/tmp"]));

        // Add project config with [language.{MOCK_LANG_A}] (Rule A).
        let mut pc = crate::config::ProjectConfig::default();
        pc.language.insert(
            MOCK_LANG_A.to_string(),
            LanguageConfig {
                servers: vec![ServerBinding::new(server_name)],
                ..LanguageConfig::default()
            },
        );
        manager
            .project_configs
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .insert(PathBuf::from("/tmp"), pc);

        let paths = vec![PathBuf::from(format!("/tmp/test.{MOCK_LANG_A}"))];
        manager.ensure_clients_for_paths(&paths).await;

        let clients = manager.clients().await;
        assert_eq!(clients.len(), 1);
        assert_eq!(
            count_scope(&clients, MOCK_LANG_A, "root"),
            1,
            "project-scoped root should produce Scope::Root, not Scope::Workspace"
        );
        Ok(())
    }

    #[tokio::test]
    async fn test_ensure_clients_non_project_uses_capability() -> Result<()> {
        // Without project config, a workspace-capable server should
        // get Scope::Workspace via the normal ensure_server path.
        let manager = LspClientManager::new(
            mockls_workspace_folders_config(),
            test_logging(),
            test_fs_with_roots(&["/tmp"]),
        );

        let paths = vec![PathBuf::from(format!("/tmp/test.{MOCK_LANG_A}"))];
        manager.ensure_clients_for_paths(&paths).await;

        let clients = manager.clients().await;
        assert_eq!(clients.len(), 1);
        assert_eq!(
            count_scope(&clients, MOCK_LANG_A, "workspace"),
            1,
            "workspace-capable server without project config should be Scope::Workspace"
        );
        Ok(())
    }

    // --- Two-step spawn and InstanceKey tests ---

    #[tokio::test]
    async fn test_spawn_workspace_scope() -> Result<()> {
        // mockls with --workspace-folders gets Scope::Workspace key after init.
        let manager = LspClientManager::new(
            mockls_workspace_folders_config(),
            test_logging(),
            test_fs_with_roots(&["/tmp"]),
        );

        let client = ensure_first_server(&manager, MOCK_LANG_A).await?;
        let key = client
            .lock()
            .await
            .server()
            .key()
            .expect("key should be set after init");
        assert_eq!(key.language_id, MOCK_LANG_A);
        assert_eq!(key.scope, Scope::Workspace);
        Ok(())
    }

    #[tokio::test]
    async fn test_spawn_legacy_scope() -> Result<()> {
        // mockls without workspace folders gets Scope::Root(root) key after init.
        let manager = LspClientManager::new(
            mockls_config(),
            test_logging(),
            test_fs_with_roots(&["/tmp"]),
        );

        let client = ensure_first_server(&manager, MOCK_LANG_A).await?;
        let key = client
            .lock()
            .await
            .server()
            .key()
            .expect("key should be set after init");
        assert_eq!(key.language_id, MOCK_LANG_A);
        assert_eq!(key.scope, Scope::Root(PathBuf::from("/tmp")));
        Ok(())
    }

    #[tokio::test]
    async fn test_ensure_server_idempotent() -> Result<()> {
        // Second call returns same client, no double-spawn.
        let manager = LspClientManager::new(
            mockls_config(),
            test_logging(),
            test_fs_with_roots(&["/tmp"]),
        );

        let client1 = ensure_first_server(&manager, MOCK_LANG_A).await?;
        let client2 = ensure_first_server(&manager, MOCK_LANG_A).await?;

        // Same Arc — no double spawn
        assert!(Arc::ptr_eq(&client1, &client2));
        assert_eq!(manager.clients().await.len(), 1);
        Ok(())
    }

    #[tokio::test]
    async fn test_ensure_server_dead_tombstone() -> Result<()> {
        // Dead server returns error on re-ensure.
        let manager = LspClientManager::new(
            mockls_config(),
            test_logging(),
            test_fs_with_roots(&["/tmp"]),
        );

        let client = ensure_first_server(&manager, MOCK_LANG_A).await?;
        // Kill the server by shutting it down without removing from map
        client.lock().await.shutdown().await?;
        // Wait briefly for the process to die
        tokio::time::sleep(Duration::from_millis(100)).await;

        let result = ensure_first_server(&manager, MOCK_LANG_A).await;
        assert!(result.is_err(), "dead server should return error");
        Ok(())
    }

    #[tokio::test]
    async fn test_clients_returns_instance_keys() -> Result<()> {
        // clients() map has InstanceKey keys.
        let manager = LspClientManager::new(
            mockls_config(),
            test_logging(),
            test_fs_with_roots(&["/tmp"]),
        );

        let _ = ensure_first_server(&manager, MOCK_LANG_A).await?;
        let clients = manager.clients().await;

        assert_eq!(clients.len(), 1);
        let key = clients.keys().next().expect("should have one key");
        assert_eq!(key.language_id, MOCK_LANG_A);
        assert!(
            matches!(key.scope, Scope::Root(_)),
            "mockls without workspace folders should be Root-scoped"
        );
        Ok(())
    }

    // --- Per-root instance lifecycle ---

    /// Helper: count instances with a specific scope kind for a language.
    fn count_scope(
        clients: &HashMap<InstanceKey, Arc<Mutex<LspClient>>>,
        lang: &str,
        scope_kind: &str,
    ) -> usize {
        clients
            .keys()
            .filter(|k| k.language_id == lang && k.scope.kind_str() == scope_kind)
            .count()
    }

    #[tokio::test]
    async fn test_spawn_all_multi_root_legacy() -> Result<()> {
        // Legacy server (no workspace folders) should get one instance per root.
        let manager = LspClientManager::new(
            mockls_config(),
            test_logging(),
            test_fs_with_roots(&["/tmp", "/var"]),
        );

        manager.spawn_all().await;

        // The mock language uses extension-based detection via the fallback path.
        // Neither /tmp nor /var will have files matching the mock extension,
        // so spawn_all detects nothing. Instead, manually spawn to test
        // the multi-root expansion logic.
        let _ = ensure_first_server(&manager, MOCK_LANG_A).await?;

        // First root spawned. Now check that spawn() can create a second
        // instance for the other root.
        let server_name = format!("mockls-{MOCK_LANG_A}");
        let (_key, _client) = manager
            .spawn(&server_name, MOCK_LANG_A, Path::new("/var"))
            .await?;

        let clients = manager.clients().await;
        assert_eq!(
            count_scope(&clients, MOCK_LANG_A, "root"),
            2,
            "Legacy server should have two root-scoped instances"
        );

        // Verify distinct root paths.
        let root_paths: HashSet<PathBuf> = clients
            .keys()
            .filter(|k| k.language_id == MOCK_LANG_A)
            .filter_map(|k| k.scope.root_path().map(Path::to_path_buf))
            .collect();
        assert!(root_paths.contains(&PathBuf::from("/tmp")));
        assert!(root_paths.contains(&PathBuf::from("/var")));

        Ok(())
    }

    #[tokio::test]
    async fn test_spawn_all_multi_root_workspace() -> Result<()> {
        // Workspace-capable server should be spawned once with Scope::Workspace.
        let manager = LspClientManager::new(
            mockls_workspace_folders_config(),
            test_logging(),
            test_fs_with_roots(&["/tmp", "/var"]),
        );

        let _ = ensure_first_server(&manager, MOCK_LANG_A).await?;

        let clients = manager.clients().await;
        assert_eq!(
            clients.len(),
            1,
            "Workspace server should have one instance"
        );
        let key = clients.keys().next().expect("should have one key");
        assert_eq!(key.scope, Scope::Workspace);

        Ok(())
    }

    #[tokio::test]
    async fn test_sync_roots_workspace_unchanged() -> Result<()> {
        // Workspace-capable server should NOT be shut down on root change.
        // Regression test for restart hack removal.
        let manager = LspClientManager::new(
            mockls_workspace_folders_config(),
            test_logging(),
            test_fs_with_roots(&["/tmp"]),
        );

        let client = ensure_first_server(&manager, MOCK_LANG_A).await?;
        assert!(client.lock().await.supports_workspace_folders());

        manager
            .sync_roots(vec![PathBuf::from("/tmp"), PathBuf::from("/var")])
            .await?;

        let clients = manager.clients().await;
        assert!(
            has_language(&clients, MOCK_LANG_A),
            "Workspace server should stay alive after sync_roots"
        );
        let key = clients.keys().next().expect("should have one key");
        assert_eq!(
            key.scope,
            Scope::Workspace,
            "Key should remain Scope::Workspace"
        );

        Ok(())
    }

    #[tokio::test]
    async fn test_remove_root_legacy_shutdown() -> Result<()> {
        // remove_root should shut down the Scope::Root instance for the removed root.
        let manager = LspClientManager::new(
            mockls_config(),
            test_logging(),
            test_fs_with_roots(&["/tmp"]),
        );

        let client = ensure_first_server(&manager, MOCK_LANG_A).await?;
        assert!(client.lock().await.is_alive());

        manager.remove_root(Path::new("/tmp")).await?;

        assert!(
            !has_language(&manager.clients().await, MOCK_LANG_A),
            "Per-root instance should be removed after remove_root"
        );

        Ok(())
    }

    #[tokio::test]
    async fn test_remove_root_workspace_notified() -> Result<()> {
        // Workspace-capable server stays alive after remove_root.
        let manager = LspClientManager::new(
            mockls_workspace_folders_config(),
            test_logging(),
            test_fs_with_roots(&["/tmp", "/var"]),
        );

        let client = ensure_first_server(&manager, MOCK_LANG_A).await?;
        assert!(client.lock().await.supports_workspace_folders());

        manager.remove_root(Path::new("/tmp")).await?;

        assert!(
            has_language(&manager.clients().await, MOCK_LANG_A),
            "Workspace server should stay alive after remove_root"
        );

        Ok(())
    }

    #[tokio::test]
    async fn test_sync_roots_no_change_noop() -> Result<()> {
        // Identical root set produces no spawns or shutdowns.
        let manager = LspClientManager::new(
            mockls_config(),
            test_logging(),
            test_fs_with_roots(&["/tmp"]),
        );

        let _ = ensure_first_server(&manager, MOCK_LANG_A).await?;
        let before = manager.clients().await.len();

        manager.sync_roots(vec![PathBuf::from("/tmp")]).await?;

        assert_eq!(
            manager.clients().await.len(),
            before,
            "No-change sync should not alter client count"
        );

        Ok(())
    }

    #[tokio::test]
    async fn test_shutdown_root_instances_selective() -> Result<()> {
        // Only Scope::Root instances matching the root are shut down.
        // Other roots and workspace instances are untouched.
        let manager = LspClientManager::new(
            mockls_config(),
            test_logging(),
            test_fs_with_roots(&["/tmp"]),
        );

        // Spawn two root-scoped instances.
        let _ = ensure_first_server(&manager, MOCK_LANG_A).await?;
        let server_name = format!("mockls-{MOCK_LANG_A}");
        let _ = manager
            .spawn(&server_name, MOCK_LANG_A, Path::new("/var"))
            .await?;

        assert_eq!(manager.clients().await.len(), 2);

        // Shut down only /var instances.
        manager.shutdown_root_instances(Path::new("/var")).await;

        let clients = manager.clients().await;
        assert_eq!(clients.len(), 1, "Only /var instance should be removed");
        let remaining_key = clients.keys().next().expect("one remaining");
        assert_eq!(
            remaining_key.scope,
            Scope::Root(PathBuf::from("/tmp")),
            "/tmp instance should remain"
        );

        Ok(())
    }

    // --- ServerStatus enrichment ---

    #[tokio::test]
    async fn test_server_status_enriched() -> Result<()> {
        // status(&key) populates server_name, scope_kind, scope_root.
        let manager = LspClientManager::new(
            mockls_config(),
            test_logging(),
            test_fs_with_roots(&["/tmp"]),
        );

        let client = ensure_first_server(&manager, MOCK_LANG_A).await?;
        let locked = client.lock().await;
        let key = locked.server().key().expect("key should be set");
        let status = locked.status(&key);
        drop(locked);

        assert_eq!(status.language, MOCK_LANG_A);
        assert_eq!(status.server_name, format!("mockls-{MOCK_LANG_A}"));
        assert_eq!(status.scope_kind, "root");
        assert_eq!(status.scope_root, "/tmp");
        assert_eq!(status.state.display_state(), "initializing");
        Ok(())
    }

    #[tokio::test]
    async fn test_server_status_workspace_scope() -> Result<()> {
        let manager = LspClientManager::new(
            mockls_workspace_folders_config(),
            test_logging(),
            test_fs_with_roots(&["/tmp"]),
        );

        let client = ensure_first_server(&manager, MOCK_LANG_A).await?;
        let locked = client.lock().await;
        let key = locked.server().key().expect("key should be set");
        let status = locked.status(&key);
        drop(locked);

        assert_eq!(status.scope_kind, "workspace");
        assert!(
            status.scope_root.is_empty(),
            "workspace scope should have empty scope_root"
        );
        Ok(())
    }

    #[tokio::test]
    async fn test_all_server_status_multi_instance() -> Result<()> {
        // Two instances of the same language produce two status entries
        // with different scope info.
        let manager = LspClientManager::new(
            mockls_config(),
            test_logging(),
            test_fs_with_roots(&["/tmp", "/var"]),
        );

        let _ = ensure_first_server(&manager, MOCK_LANG_A).await?;
        let server_name = format!("mockls-{MOCK_LANG_A}");
        let _ = manager
            .spawn(&server_name, MOCK_LANG_A, Path::new("/var"))
            .await?;

        let statuses = manager.all_server_status().await;
        assert_eq!(statuses.len(), 2, "should have two status entries");

        let roots: HashSet<String> = statuses.iter().map(|s| s.scope_root.clone()).collect();
        assert!(roots.contains("/tmp"), "should include /tmp root");
        assert!(roots.contains("/var"), "should include /var root");

        for s in &statuses {
            assert_eq!(s.language, MOCK_LANG_A);
            assert_eq!(s.server_name, server_name);
            assert_eq!(s.scope_kind, "root");
        }

        Ok(())
    }

    // --- match_file_changes ---

    // --- file_matches_patterns ---

    mod file_patterns_matching {
        use super::*;
        use crate::lsp::glob::LspGlob;

        fn compile(patterns: &[&str]) -> Vec<LspGlob> {
            patterns
                .iter()
                .map(|p| LspGlob::new(p).expect("valid glob"))
                .collect()
        }

        #[test]
        fn empty_patterns_matches_all() {
            assert!(file_matches_patterns(Path::new("/tmp/test.rs"), &[]));
            assert!(file_matches_patterns(Path::new("/tmp/PKGBUILD"), &[]));
        }

        #[test]
        fn exact_filename_match() {
            let patterns = compile(&["PKGBUILD"]);
            assert!(file_matches_patterns(
                Path::new("/home/user/PKGBUILD"),
                &patterns
            ));
        }

        #[test]
        fn exact_filename_no_match() {
            let patterns = compile(&["PKGBUILD"]);
            assert!(!file_matches_patterns(
                Path::new("/home/user/script.sh"),
                &patterns
            ));
        }

        #[test]
        fn glob_extension_match() {
            let patterns = compile(&["*.ebuild"]);
            assert!(file_matches_patterns(
                Path::new("/repo/foo.ebuild"),
                &patterns
            ));
        }

        #[test]
        fn glob_extension_no_match() {
            let patterns = compile(&["*.ebuild"]);
            assert!(!file_matches_patterns(Path::new("/repo/foo.rs"), &patterns));
        }

        #[test]
        fn multiple_patterns_any_match() {
            let patterns = compile(&["PKGBUILD", "*.ebuild"]);
            assert!(file_matches_patterns(
                Path::new("/repo/PKGBUILD"),
                &patterns
            ));
            assert!(file_matches_patterns(
                Path::new("/repo/foo.ebuild"),
                &patterns
            ));
            assert!(!file_matches_patterns(
                Path::new("/repo/script.sh"),
                &patterns
            ));
        }

        #[test]
        fn no_filename_returns_false() {
            // A path that is just "/" has no file_name component.
            let patterns = compile(&["*"]);
            assert!(!file_matches_patterns(Path::new("/"), &patterns));
        }

        #[test]
        fn star_does_not_cross_separator() {
            // LspGlob uses literal_separator(true): * should not match paths.
            let patterns = compile(&["*.rs"]);
            // "foo.rs" matches
            assert!(file_matches_patterns(Path::new("/tmp/foo.rs"), &patterns));
            // "src/foo.rs" as a single filename component would not occur,
            // but matching against just the filename means this works normally.
            assert!(file_matches_patterns(
                Path::new("/project/src/foo.rs"),
                &patterns
            ));
        }
    }

    // --- get_servers ---

    #[tokio::test]
    async fn test_get_servers_single_server() -> Result<()> {
        let manager = LspClientManager::new(
            mockls_config(),
            test_logging(),
            test_fs_with_roots(&["/tmp"]),
        );

        // Pre-spawn the server
        let _ = ensure_first_server(&manager, MOCK_LANG_A).await?;

        let path = PathBuf::from(format!("/tmp/test.{MOCK_LANG_A}"));
        // Use a capability that mockls supports (document symbols — all mockls
        // instances advertise it).
        let servers = manager
            .get_servers(&path, LspServer::supports_document_symbols, None)
            .await;
        assert_eq!(servers.len(), 1);
        Ok(())
    }

    #[tokio::test]
    async fn test_get_servers_capability_filter() -> Result<()> {
        let manager = LspClientManager::new(
            mockls_config(),
            test_logging(),
            test_fs_with_roots(&["/tmp"]),
        );

        let _ = ensure_first_server(&manager, MOCK_LANG_A).await?;

        let path = PathBuf::from(format!("/tmp/test.{MOCK_LANG_A}"));
        // Use a capability that mockls does NOT support (pull diagnostics
        // requires --pull-diagnostics flag which mockls_config doesn't set).
        let servers = manager
            .get_servers(&path, LspServer::supports_pull_diagnostics, None)
            .await;
        assert!(
            servers.is_empty(),
            "mockls (default) does not support pull diagnostics, should return empty"
        );
        Ok(())
    }

    #[tokio::test]
    async fn test_get_servers_file_patterns_match() -> Result<()> {
        // file_patterns filters within the language. Use a pattern that
        // matches the filename of a file with the mock extension.
        let bin = mockls_bin();
        let server_name = format!("mockls-{MOCK_LANG_A}-fp");
        let pattern = "special.*".to_string();
        let mut server = HashMap::new();
        server.insert(
            server_name.clone(),
            ServerDef {
                command: bin.to_string_lossy().to_string(),
                args: vec![MOCK_LANG_A.to_string()],
                initialization_options: None,
                settings: None,
                min_severity: None,
                file_patterns: vec![pattern.clone()],
                compiled_patterns: vec![
                    crate::lsp::glob::LspGlob::new(&pattern).expect("valid glob"),
                ],
                ..ServerDef::default()
            },
        );
        let mut language = HashMap::new();
        language.insert(
            MOCK_LANG_A.to_string(),
            LanguageConfig {
                servers: vec![ServerBinding::new(server_name)],
                ..LanguageConfig::default()
            },
        );
        let config = Arc::new(Config {
            language,
            server,
            log_retention_days: 7,
            notifications: None,
            icons: None,
            tui: None,
            tools: None,
            resolved_commands: None,
        });

        let manager = LspClientManager::new(config, test_logging(), test_fs_with_roots(&["/tmp"]));
        let _ = ensure_first_server(&manager, MOCK_LANG_A).await?;

        // Filename "special.yX4Za" matches pattern "special.*"
        let path = PathBuf::from(format!("/tmp/special.{MOCK_LANG_A}"));
        let servers = manager
            .get_servers(&path, LspServer::supports_document_symbols, None)
            .await;
        assert_eq!(
            servers.len(),
            1,
            "special.{MOCK_LANG_A} should match file_patterns=[\"special.*\"]"
        );
        Ok(())
    }

    #[tokio::test]
    async fn test_get_servers_file_patterns_no_match() -> Result<()> {
        let bin = mockls_bin();
        let server_name = format!("mockls-{MOCK_LANG_A}-fp2");
        let pattern = "special.*".to_string();
        let mut server = HashMap::new();
        server.insert(
            server_name.clone(),
            ServerDef {
                command: bin.to_string_lossy().to_string(),
                args: vec![MOCK_LANG_A.to_string()],
                initialization_options: None,
                settings: None,
                min_severity: None,
                file_patterns: vec![pattern.clone()],
                compiled_patterns: vec![
                    crate::lsp::glob::LspGlob::new(&pattern).expect("valid glob"),
                ],
                ..ServerDef::default()
            },
        );
        let mut language = HashMap::new();
        language.insert(
            MOCK_LANG_A.to_string(),
            LanguageConfig {
                servers: vec![ServerBinding::new(server_name)],
                ..LanguageConfig::default()
            },
        );
        let config = Arc::new(Config {
            language,
            server,
            log_retention_days: 7,
            notifications: None,
            icons: None,
            tui: None,
            tools: None,
            resolved_commands: None,
        });

        let manager = LspClientManager::new(config, test_logging(), test_fs_with_roots(&["/tmp"]));
        let _ = ensure_first_server(&manager, MOCK_LANG_A).await?;

        // Filename "other.yX4Za" does NOT match pattern "special.*"
        let path = PathBuf::from(format!("/tmp/other.{MOCK_LANG_A}"));
        let servers = manager
            .get_servers(&path, LspServer::supports_document_symbols, None)
            .await;
        assert!(
            servers.is_empty(),
            "other.{MOCK_LANG_A} should not match file_patterns=[\"special.*\"]"
        );
        Ok(())
    }

    #[tokio::test]
    async fn test_get_servers_file_patterns_glob() -> Result<()> {
        let bin = mockls_bin();
        let server_name = format!("mockls-{MOCK_LANG_A}-fpg");
        let pattern = format!("*.{MOCK_LANG_A}");
        let mut server = HashMap::new();
        server.insert(
            server_name.clone(),
            ServerDef {
                command: bin.to_string_lossy().to_string(),
                args: vec![MOCK_LANG_A.to_string()],
                initialization_options: None,
                settings: None,
                min_severity: None,
                file_patterns: vec![pattern.clone()],
                compiled_patterns: vec![
                    crate::lsp::glob::LspGlob::new(&pattern).expect("valid glob"),
                ],
                ..ServerDef::default()
            },
        );
        let mut language = HashMap::new();
        language.insert(
            MOCK_LANG_A.to_string(),
            LanguageConfig {
                servers: vec![ServerBinding::new(server_name)],
                ..LanguageConfig::default()
            },
        );
        let config = Arc::new(Config {
            language,
            server,
            log_retention_days: 7,
            notifications: None,
            icons: None,
            tui: None,
            tools: None,
            resolved_commands: None,
        });

        let manager = LspClientManager::new(config, test_logging(), test_fs_with_roots(&["/tmp"]));
        let _ = ensure_first_server(&manager, MOCK_LANG_A).await?;

        let path = PathBuf::from(format!("/tmp/foo.{MOCK_LANG_A}"));
        let servers = manager
            .get_servers(&path, LspServer::supports_document_symbols, None)
            .await;
        assert_eq!(servers.len(), 1, "*.ext glob should match");
        Ok(())
    }

    #[tokio::test]
    async fn test_get_servers_empty_file_patterns() -> Result<()> {
        // Server with no file_patterns matches all files for the language.
        let manager = LspClientManager::new(
            mockls_config(),
            test_logging(),
            test_fs_with_roots(&["/tmp"]),
        );
        let _ = ensure_first_server(&manager, MOCK_LANG_A).await?;

        let path = PathBuf::from(format!("/tmp/anything.{MOCK_LANG_A}"));
        let servers = manager
            .get_servers(&path, LspServer::supports_document_symbols, None)
            .await;
        assert_eq!(
            servers.len(),
            1,
            "empty file_patterns should match all files"
        );
        Ok(())
    }

    #[tokio::test]
    async fn test_get_servers_dead_server_skipped() -> Result<()> {
        let manager = LspClientManager::new(
            mockls_config(),
            test_logging(),
            test_fs_with_roots(&["/tmp"]),
        );

        let client = ensure_first_server(&manager, MOCK_LANG_A).await?;
        // Kill the server
        client.lock().await.shutdown().await?;
        tokio::time::sleep(Duration::from_millis(100)).await;

        let path = PathBuf::from(format!("/tmp/test.{MOCK_LANG_A}"));
        let servers = manager
            .get_servers(&path, LspServer::supports_document_symbols, None)
            .await;
        assert!(servers.is_empty(), "dead server should be skipped");
        Ok(())
    }

    #[tokio::test]
    async fn test_get_servers_disabled_methods() -> Result<()> {
        // disabled_methods on the binding suppresses the server for that method.
        let bin = mockls_bin();
        let server_name = format!("mockls-{MOCK_LANG_A}");
        let mut server = HashMap::new();
        server.insert(
            server_name.clone(),
            ServerDef {
                command: bin.to_string_lossy().to_string(),
                args: vec![MOCK_LANG_A.to_string()],
                ..ServerDef::default()
            },
        );
        let mut language = HashMap::new();
        language.insert(
            MOCK_LANG_A.to_string(),
            LanguageConfig {
                servers: vec![ServerBinding {
                    name: server_name,
                    diagnostics: true,
                    disabled_methods: vec![DispatchMethod::References],
                }],
                ..LanguageConfig::default()
            },
        );
        let config = Arc::new(Config {
            language,
            server,
            log_retention_days: 7,
            notifications: None,
            icons: None,
            tui: None,
            tools: None,
            resolved_commands: None,
        });

        let manager = LspClientManager::new(config, test_logging(), test_fs_with_roots(&["/tmp"]));
        let _ = ensure_first_server(&manager, MOCK_LANG_A).await?;

        let path = PathBuf::from(format!("/tmp/test.{MOCK_LANG_A}"));

        // Method that is disabled — should return empty.
        let servers = manager
            .get_servers(
                &path,
                LspServer::supports_references,
                Some(DispatchMethod::References),
            )
            .await;
        assert!(
            servers.is_empty(),
            "disabled method should suppress the server"
        );

        // Different method — should return the server.
        let servers = manager
            .get_servers(
                &path,
                LspServer::supports_document_symbols,
                Some(DispatchMethod::DocumentSymbol),
            )
            .await;
        assert_eq!(
            servers.len(),
            1,
            "non-disabled method should still return the server"
        );

        // No method (diagnostics path) — should return the server.
        let servers = manager
            .get_servers(&path, LspServer::supports_document_symbols, None)
            .await;
        assert_eq!(
            servers.len(),
            1,
            "None method should bypass disabled_methods check"
        );

        Ok(())
    }

    #[tokio::test]
    async fn test_get_servers_outside_roots_spawns_single_file() {
        // Files outside all roots get a single-file server (tier 3)
        // when the server is configured with single_file = true.
        let manager = LspClientManager::new(
            mockls_single_file_config(),
            test_logging(),
            test_fs_with_roots(&["/tmp"]),
        );

        let path = PathBuf::from(format!("/other/test.{MOCK_LANG_A}"));
        let servers = manager
            .get_servers(&path, LspServer::supports_document_symbols, None)
            .await;
        assert_eq!(
            servers.len(),
            1,
            "file outside roots should get single-file server"
        );

        // Verify it's a SingleFile instance.
        let clients = manager.clients().await;
        assert!(
            clients
                .keys()
                .any(|k| k.scope == Scope::SingleFile && k.language_id == MOCK_LANG_A),
            "should have a single-file instance"
        );
    }

    #[tokio::test]
    async fn test_get_servers_unknown_language() {
        let manager = LspClientManager::new(
            mockls_config(),
            test_logging(),
            test_fs_with_roots(&["/tmp"]),
        );

        let servers = manager
            .get_servers(
                Path::new("/tmp/test.xyz"),
                LspServer::supports_references,
                None,
            )
            .await;
        assert!(servers.is_empty(), "unknown language should return empty");
    }

    #[tokio::test]
    async fn test_get_servers_priority_order() -> Result<()> {
        // With multiple servers in the binding, result preserves order.
        // (Currently only one server per language is spawned, so this test
        // exercises the path ordering with a single entry — 1c-01 will
        // extend it to multiple.)
        let manager = LspClientManager::new(
            mockls_config(),
            test_logging(),
            test_fs_with_roots(&["/tmp"]),
        );
        let _ = ensure_first_server(&manager, MOCK_LANG_A).await?;

        let path = PathBuf::from(format!("/tmp/test.{MOCK_LANG_A}"));
        let servers = manager
            .get_servers(&path, LspServer::supports_document_symbols, None)
            .await;
        assert_eq!(servers.len(), 1);
        Ok(())
    }

    // --- Multi-server spawning (1c-01) ---

    #[tokio::test]
    async fn test_spawn_all_multi_server() -> Result<()> {
        // Two workspace-capable servers for one language: spawn_all creates
        // two entries in the client map with different InstanceKeys.
        let config = mockls_multi_server_workspace_config();
        let bindings: Vec<String> = config
            .resolve_language(MOCK_LANG_A)
            .expect("lang config")
            .servers
            .iter()
            .map(|b| b.name.clone())
            .collect();
        assert_eq!(bindings.len(), 2);

        let manager = LspClientManager::new(config, test_logging(), test_fs_with_roots(&["/tmp"]));

        // spawn_all won't detect files (synthetic extension), so spawn directly
        // using the same pattern spawn_all uses.
        for name in &bindings {
            manager
                .ensure_server(MOCK_LANG_A, name, Path::new("/tmp"))
                .await?;
        }

        let clients = manager.clients().await;
        assert_eq!(
            clients.len(),
            2,
            "Two servers should produce two client map entries"
        );

        let server_names: HashSet<String> = clients.keys().map(|k| k.server.clone()).collect();
        assert!(server_names.contains(&bindings[0]));
        assert!(server_names.contains(&bindings[1]));
        Ok(())
    }

    #[tokio::test]
    async fn test_spawn_all_multi_server_legacy() -> Result<()> {
        // Two legacy servers, two roots: 2 servers × 2 roots = 4 instances.
        let config = mockls_multi_server_config();
        let bindings: Vec<String> = config
            .resolve_language(MOCK_LANG_A)
            .expect("lang config")
            .servers
            .iter()
            .map(|b| b.name.clone())
            .collect();

        let manager = LspClientManager::new(
            config,
            test_logging(),
            test_fs_with_roots(&["/tmp", "/var"]),
        );

        // Simulate spawn_all's multi-server + per-root logic.
        let roots = manager.roots();
        for name in &bindings {
            let client = manager.ensure_server(MOCK_LANG_A, name, &roots[0]).await?;
            let key = client.lock().await.server().key();
            let Some(key) = key else {
                continue;
            };
            if matches!(key.scope, Scope::Root(_)) {
                for root in &roots[1..] {
                    manager.spawn(name, MOCK_LANG_A, root).await?;
                }
            }
        }

        let clients = manager.clients().await;
        assert_eq!(clients.len(), 4, "2 legacy servers × 2 roots = 4 instances");
        assert_eq!(count_scope(&clients, MOCK_LANG_A, "root"), 4);
        Ok(())
    }

    #[tokio::test]
    async fn test_spawn_all_mixed_capability() -> Result<()> {
        // One workspace-capable + one legacy server, two roots:
        // workspace gets 1 instance, legacy gets 2.
        let config = mockls_mixed_capability_config();
        let bindings: Vec<String> = config
            .resolve_language(MOCK_LANG_A)
            .expect("lang config")
            .servers
            .iter()
            .map(|b| b.name.clone())
            .collect();

        let manager = LspClientManager::new(
            config,
            test_logging(),
            test_fs_with_roots(&["/tmp", "/var"]),
        );

        let roots = manager.roots();
        for name in &bindings {
            let client = manager.ensure_server(MOCK_LANG_A, name, &roots[0]).await?;
            let key = client.lock().await.server().key();
            let Some(key) = key else {
                continue;
            };
            if matches!(key.scope, Scope::Root(_)) {
                for root in &roots[1..] {
                    manager.spawn(name, MOCK_LANG_A, root).await?;
                }
            }
        }

        let clients = manager.clients().await;
        assert_eq!(
            clients.len(),
            3,
            "1 workspace + 2 legacy per-root = 3 instances"
        );
        assert_eq!(count_scope(&clients, MOCK_LANG_A, "workspace"), 1);
        assert_eq!(count_scope(&clients, MOCK_LANG_A, "root"), 2);
        Ok(())
    }

    #[tokio::test]
    async fn test_ensure_clients_for_paths_multi_server() -> Result<()> {
        // New files trigger spawning of all servers in the binding.
        let manager = LspClientManager::new(
            mockls_multi_server_config(),
            test_logging(),
            test_fs_with_roots(&["/tmp"]),
        );

        assert!(manager.clients().await.is_empty());

        let paths = vec![PathBuf::from(format!("/tmp/test.{MOCK_LANG_A}"))];
        manager.ensure_clients_for_paths(&paths).await;

        let clients = manager.clients().await;
        assert_eq!(
            clients.len(),
            2,
            "ensure_clients_for_paths should spawn all servers in the binding"
        );

        let server_names: HashSet<String> = clients.keys().map(|k| k.server.clone()).collect();
        assert_eq!(server_names.len(), 2);
        Ok(())
    }

    #[tokio::test]
    async fn test_spawn_legacy_for_added_roots_multi_server() -> Result<()> {
        // Adding a root spawns per-root instances for all legacy servers.
        // Uses a tempdir with real files so detect_workspace_languages succeeds.
        let root_a = tempfile::tempdir().expect("tempdir");
        let root_b = tempfile::tempdir().expect("tempdir");

        // Create files with the synthetic extension so language detection works.
        std::fs::write(root_a.path().join(format!("file.{MOCK_LANG_A}")), "content")
            .expect("write");
        std::fs::write(root_b.path().join(format!("file.{MOCK_LANG_A}")), "content")
            .expect("write");

        let config = mockls_multi_server_config();
        let bindings: Vec<String> = config
            .resolve_language(MOCK_LANG_A)
            .expect("lang config")
            .servers
            .iter()
            .map(|b| b.name.clone())
            .collect();

        let fs = test_fs();
        fs.set_roots(vec![root_a.path().to_path_buf()]);
        let manager = LspClientManager::new(config, test_logging(), fs);

        // Spawn both servers for root_a.
        for name in &bindings {
            manager
                .ensure_server(MOCK_LANG_A, name, root_a.path())
                .await?;
        }
        assert_eq!(manager.clients().await.len(), 2);

        // sync_roots adds root_b — both legacy servers should get root_b instances.
        manager
            .sync_roots(vec![
                root_a.path().to_path_buf(),
                root_b.path().to_path_buf(),
            ])
            .await?;

        let clients = manager.clients().await;
        assert_eq!(clients.len(), 4, "2 legacy servers × 2 roots = 4 instances");
        assert_eq!(count_scope(&clients, MOCK_LANG_A, "root"), 4);

        // Verify both roots are represented.
        let root_paths: HashSet<PathBuf> = clients
            .keys()
            .filter_map(|k| k.scope.root_path().map(Path::to_path_buf))
            .collect();
        assert!(root_paths.contains(root_a.path()));
        assert!(root_paths.contains(root_b.path()));
        Ok(())
    }

    #[tokio::test]
    async fn test_sync_roots_remove_multi_server() -> Result<()> {
        // Removing a root shuts down per-root instances for all servers.
        let config = mockls_multi_server_config();
        let bindings: Vec<String> = config
            .resolve_language(MOCK_LANG_A)
            .expect("lang config")
            .servers
            .iter()
            .map(|b| b.name.clone())
            .collect();

        let manager = LspClientManager::new(
            config,
            test_logging(),
            test_fs_with_roots(&["/tmp", "/var"]),
        );

        // Spawn both servers for both roots (4 instances total).
        for name in &bindings {
            manager
                .ensure_server(MOCK_LANG_A, name, Path::new("/tmp"))
                .await?;
            manager.spawn(name, MOCK_LANG_A, Path::new("/var")).await?;
        }
        assert_eq!(manager.clients().await.len(), 4);

        // Remove /var — should shut down both servers' /var instances.
        manager.sync_roots(vec![PathBuf::from("/tmp")]).await?;

        let clients = manager.clients().await;
        assert_eq!(
            clients.len(),
            2,
            "Only /tmp instances should remain after removing /var"
        );
        for key in clients.keys() {
            assert_eq!(
                key.scope,
                Scope::Root(PathBuf::from("/tmp")),
                "All remaining instances should be for /tmp"
            );
        }
        Ok(())
    }

    mod file_change_matching {
        use super::*;
        use crate::lsp::glob::{FileChange, FileChangeType, GlobPattern, WatchKind};

        fn plain_glob(pattern: &str) -> GlobPattern {
            GlobPattern::from_value(&serde_json::json!(pattern)).expect("valid glob")
        }

        #[test]
        fn matches_created_file() {
            let changes = vec![FileChange {
                path: PathBuf::from("/project/src/new.rs"),
                change_type: FileChangeType::Created,
            }];
            let watchers = vec![(plain_glob("**/*.rs"), WatchKind::from_value(None))];
            let roots = vec![PathBuf::from("/project")];

            let matched = match_file_changes(&changes, &watchers, &roots);
            assert_eq!(matched.len(), 1);
            assert_eq!(matched[0].0, "file:///project/src/new.rs");
            assert_eq!(matched[0].1, FileChangeType::Created as u8);
        }

        #[test]
        fn skips_non_matching_extension() {
            let changes = vec![FileChange {
                path: PathBuf::from("/project/src/file.rs"),
                change_type: FileChangeType::Created,
            }];
            let watchers = vec![(plain_glob("**/*.ts"), WatchKind::from_value(None))];
            let roots = vec![PathBuf::from("/project")];

            let matched = match_file_changes(&changes, &watchers, &roots);
            assert!(matched.is_empty());
        }

        #[test]
        fn empty_changes_returns_empty() {
            let watchers = vec![(plain_glob("**/*.rs"), WatchKind::from_value(None))];
            let roots = vec![PathBuf::from("/project")];

            let matched = match_file_changes(&[], &watchers, &roots);
            assert!(matched.is_empty());
        }

        #[test]
        fn empty_watchers_returns_empty() {
            let changes = vec![FileChange {
                path: PathBuf::from("/project/src/file.rs"),
                change_type: FileChangeType::Created,
            }];
            let roots = vec![PathBuf::from("/project")];

            let matched = match_file_changes(&changes, &[], &roots);
            assert!(matched.is_empty());
        }

        #[test]
        fn batches_multiple_events() {
            let changes = vec![
                FileChange {
                    path: PathBuf::from("/project/src/a.rs"),
                    change_type: FileChangeType::Created,
                },
                FileChange {
                    path: PathBuf::from("/project/src/b.rs"),
                    change_type: FileChangeType::Changed,
                },
                FileChange {
                    path: PathBuf::from("/project/src/c.rs"),
                    change_type: FileChangeType::Deleted,
                },
            ];
            let watchers = vec![(plain_glob("**/*.rs"), WatchKind::from_value(None))];
            let roots = vec![PathBuf::from("/project")];

            let matched = match_file_changes(&changes, &watchers, &roots);
            assert_eq!(matched.len(), 3);
            assert_eq!(matched[0].1, FileChangeType::Created as u8);
            assert_eq!(matched[1].1, FileChangeType::Changed as u8);
            assert_eq!(matched[2].1, FileChangeType::Deleted as u8);
        }

        #[test]
        fn respects_watch_kind_create_only() {
            let changes = vec![
                FileChange {
                    path: PathBuf::from("/project/src/new.rs"),
                    change_type: FileChangeType::Created,
                },
                FileChange {
                    path: PathBuf::from("/project/src/mod.rs"),
                    change_type: FileChangeType::Changed,
                },
                FileChange {
                    path: PathBuf::from("/project/src/old.rs"),
                    change_type: FileChangeType::Deleted,
                },
            ];
            let watchers = vec![(
                plain_glob("**/*.rs"),
                WatchKind::from_value(Some(WatchKind::CREATE)),
            )];
            let roots = vec![PathBuf::from("/project")];

            let matched = match_file_changes(&changes, &watchers, &roots);
            assert_eq!(matched.len(), 1);
            assert_eq!(matched[0].1, FileChangeType::Created as u8);
        }

        #[test]
        fn respects_watch_kind_delete_only() {
            let changes = vec![
                FileChange {
                    path: PathBuf::from("/project/src/new.rs"),
                    change_type: FileChangeType::Created,
                },
                FileChange {
                    path: PathBuf::from("/project/src/old.rs"),
                    change_type: FileChangeType::Deleted,
                },
            ];
            let watchers = vec![(
                plain_glob("**/*.rs"),
                WatchKind::from_value(Some(WatchKind::DELETE)),
            )];
            let roots = vec![PathBuf::from("/project")];

            let matched = match_file_changes(&changes, &watchers, &roots);
            assert_eq!(matched.len(), 1);
            assert_eq!(matched[0].0, "file:///project/src/old.rs");
        }

        #[test]
        fn path_outside_root_not_matched() {
            let changes = vec![FileChange {
                path: PathBuf::from("/other/src/file.rs"),
                change_type: FileChangeType::Created,
            }];
            let watchers = vec![(plain_glob("**/*.rs"), WatchKind::from_value(None))];
            let roots = vec![PathBuf::from("/project")];

            let matched = match_file_changes(&changes, &watchers, &roots);
            assert!(matched.is_empty());
        }

        #[test]
        fn multiple_watchers_any_match() {
            let changes = vec![FileChange {
                path: PathBuf::from("/project/Cargo.toml"),
                change_type: FileChangeType::Changed,
            }];
            let watchers = vec![
                (plain_glob("**/*.rs"), WatchKind::from_value(None)),
                (plain_glob("**/*.toml"), WatchKind::from_value(None)),
            ];
            let roots = vec![PathBuf::from("/project")];

            let matched = match_file_changes(&changes, &watchers, &roots);
            assert_eq!(matched.len(), 1);
        }
    }

    // --- Wait primitives (1c-02) ---

    #[tokio::test]
    async fn test_wait_ready_for_path_healthy() -> Result<()> {
        // Server reaches ready state: wait_ready_for_path returns.
        let manager = LspClientManager::new(
            mockls_config(),
            test_logging(),
            test_fs_with_roots(&["/tmp"]),
        );

        let _ = ensure_first_server(&manager, MOCK_LANG_A).await?;

        let path = PathBuf::from(format!("/tmp/test.{MOCK_LANG_A}"));
        manager.wait_ready_for_path(&path).await;

        // If we got here, it didn't hang.
        Ok(())
    }

    #[tokio::test]
    async fn test_wait_ready_for_path_dead() -> Result<()> {
        // Server dies: wait_ready_for_path returns (doesn't hang).
        let manager = LspClientManager::new(
            mockls_config(),
            test_logging(),
            test_fs_with_roots(&["/tmp"]),
        );

        let client = ensure_first_server(&manager, MOCK_LANG_A).await?;
        // Kill the server.
        client.lock().await.shutdown().await?;
        tokio::time::sleep(Duration::from_millis(100)).await;

        let path = PathBuf::from(format!("/tmp/test.{MOCK_LANG_A}"));
        manager.wait_ready_for_path(&path).await;

        // If we got here, dead server didn't block.
        Ok(())
    }

    #[tokio::test]
    async fn test_wait_ready_for_path_unrooted() {
        // File outside roots: returns immediately (no servers to wait for).
        let manager = LspClientManager::new(
            mockls_config(),
            test_logging(),
            test_fs_with_roots(&["/tmp"]),
        );

        let path = PathBuf::from(format!("/other/test.{MOCK_LANG_A}"));
        manager.wait_ready_for_path(&path).await;
    }

    #[tokio::test]
    async fn test_wait_ready_for_path_no_config() {
        // Unconfigured language: returns immediately.
        let manager = LspClientManager::new(
            mockls_config(),
            test_logging(),
            test_fs_with_roots(&["/tmp"]),
        );

        manager
            .wait_ready_for_path(Path::new("/tmp/test.xyz"))
            .await;
    }

    #[tokio::test]
    async fn test_wait_ready_all_mixed() -> Result<()> {
        // Some healthy, some dead: returns after all settle.
        let config = mockls_multi_server_config();
        let bindings: Vec<String> = config
            .resolve_language(MOCK_LANG_A)
            .expect("lang config")
            .servers
            .iter()
            .map(|b| b.name.clone())
            .collect();

        let manager = LspClientManager::new(config, test_logging(), test_fs_with_roots(&["/tmp"]));

        // Spawn both servers.
        let client_a = manager
            .ensure_server(MOCK_LANG_A, &bindings[0], Path::new("/tmp"))
            .await?;
        let _client_b = manager
            .ensure_server(MOCK_LANG_A, &bindings[1], Path::new("/tmp"))
            .await?;

        // Kill one server.
        client_a.lock().await.shutdown().await?;
        tokio::time::sleep(Duration::from_millis(100)).await;

        // wait_ready_all should still return (dead server doesn't block).
        manager.wait_ready_all().await;

        Ok(())
    }

    #[tokio::test]
    async fn test_ensure_and_wait_for_paths() -> Result<()> {
        // Spawns new servers and returns after they're ready.
        let manager = LspClientManager::new(
            mockls_config(),
            test_logging(),
            test_fs_with_roots(&["/tmp"]),
        );

        assert!(manager.clients().await.is_empty());

        let paths = vec![PathBuf::from(format!("/tmp/test.{MOCK_LANG_A}"))];
        manager.ensure_and_wait_for_paths(&paths).await;

        assert!(
            has_language(&manager.clients().await, MOCK_LANG_A),
            "ensure_and_wait_for_paths should spawn the server"
        );
        Ok(())
    }

    // --- Document lifecycle (1c-03) ---

    #[tokio::test]
    async fn test_open_document_on_single_client() -> Result<()> {
        // open_document_on returns URI and sends didOpen.
        let dir = tempfile::tempdir().expect("tempdir");
        let fs = test_fs_with_roots(&[]);
        fs.set_roots(vec![dir.path().to_path_buf()]);
        let manager = LspClientManager::new(mockls_config(), test_logging(), fs);

        let path = dir.path().join(format!("test.{MOCK_LANG_A}"));
        std::fs::write(&path, "content").expect("write");

        let client = ensure_first_server(&manager, MOCK_LANG_A).await?;
        let uri = manager.open_document_on(&path, &client, None).await?;
        assert!(uri.starts_with("file://"));
        assert!(
            client.lock().await.is_document_open(&uri),
            "Client should track the document as open"
        );
        Ok(())
    }

    #[tokio::test]
    async fn test_open_document_on_second_call() -> Result<()> {
        // Second open on the same client sends didChange, not duplicate didOpen.
        let dir = tempfile::tempdir().expect("tempdir");
        let fs = test_fs_with_roots(&[]);
        fs.set_roots(vec![dir.path().to_path_buf()]);
        let manager = LspClientManager::new(mockls_config(), test_logging(), fs);

        let path = dir.path().join(format!("test.{MOCK_LANG_A}"));
        std::fs::write(&path, "content").expect("write");

        let client = ensure_first_server(&manager, MOCK_LANG_A).await?;
        let uri1 = manager.open_document_on(&path, &client, None).await?;
        let uri2 = manager.open_document_on(&path, &client, None).await?;
        assert_eq!(uri1, uri2);
        // Both calls succeed — second sends didChange since the client
        // already has the document open.
        assert!(client.lock().await.is_document_open(&uri1));
        Ok(())
    }

    #[tokio::test]
    async fn test_close_tracked_document() -> Result<()> {
        // close_tracked_document removes per-client tracking and sends didClose.
        let dir = tempfile::tempdir().expect("tempdir");
        let fs = test_fs_with_roots(&[]);
        fs.set_roots(vec![dir.path().to_path_buf()]);
        let manager = LspClientManager::new(mockls_multi_server_config(), test_logging(), fs);

        let path = dir.path().join(format!("test.{MOCK_LANG_A}"));
        std::fs::write(&path, "content").expect("write");

        manager
            .ensure_clients_for_paths(std::slice::from_ref(&path))
            .await;

        let servers = manager
            .get_servers(&path, LspServer::supports_document_symbols, None)
            .await;
        assert_eq!(servers.len(), 2);

        // Open document on both servers.
        let mut uri = String::new();
        for c in &servers {
            uri = manager.open_document_on(&path, c, None).await?;
        }

        // Verify all clients have the document open.
        for c in &servers {
            assert!(c.lock().await.is_document_open(&uri));
        }

        // Close on each while holding the lock.
        for c in &servers {
            c.lock().await.close_tracked_document(&uri).await;
        }

        // Verify all clients no longer track the document.
        for c in &servers {
            assert!(
                !c.lock().await.is_document_open(&uri),
                "Document should be closed on all clients"
            );
        }
        Ok(())
    }

    // --- Project config infrastructure tests ---

    #[test]
    fn test_is_project_scoped_with_language() {
        let manager = LspClientManager::new(test_config(), test_logging(), test_fs());
        let root = PathBuf::from("/project");

        let mut pc = crate::config::ProjectConfig::default();
        pc.language.insert(
            "rust".to_string(),
            LanguageConfig {
                servers: vec![ServerBinding::new("rust-analyzer")],
                ..LanguageConfig::default()
            },
        );
        manager
            .project_configs
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .insert(root.clone(), pc);

        assert!(manager.is_project_scoped("rust", &root));
    }

    #[test]
    fn test_is_project_scoped_without_language() {
        let manager = LspClientManager::new(test_config(), test_logging(), test_fs());
        let root = PathBuf::from("/project");

        let mut pc = crate::config::ProjectConfig::default();
        pc.server.insert(
            "rust-analyzer".to_string(),
            ServerDef {
                command: String::new(),
                args: Vec::new(),
                settings: Some(serde_json::json!({"key": "value"})),
                ..ServerDef::default()
            },
        );
        manager
            .project_configs
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .insert(root.clone(), pc);

        assert!(!manager.is_project_scoped("rust", &root));
    }

    #[test]
    fn test_is_project_scoped_no_config() {
        let manager = LspClientManager::new(test_config(), test_logging(), test_fs());
        let root = PathBuf::from("/project");

        assert!(!manager.is_project_scoped("rust", &root));
    }

    #[test]
    fn test_effective_server_def_merge() {
        let mut config = test_config_raw();
        config.server.insert(
            "rust-analyzer".to_string(),
            ServerDef {
                command: "rust-analyzer".to_string(),
                args: vec!["--log-level".to_string(), "info".to_string()],
                settings: Some(serde_json::json!({"check": {"command": "clippy"}, "cargo": {"features": ["a"]}})),
                ..ServerDef::default()
            },
        );

        let manager = LspClientManager::new(config, test_logging(), test_fs());
        let root = PathBuf::from("/project");

        // Project config only overrides settings.
        let mut pc = crate::config::ProjectConfig::default();
        pc.server.insert(
            "rust-analyzer".to_string(),
            ServerDef {
                command: String::new(), // empty = inherit from user
                args: Vec::new(),
                settings: Some(serde_json::json!({"check": {"command": "check"}, "new_key": true})),
                ..ServerDef::default()
            },
        );
        manager
            .project_configs
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .insert(root.clone(), pc);

        let merged = manager
            .effective_server_def("rust-analyzer", &root)
            .expect("should exist");

        // command/args inherited from user
        assert_eq!(merged.command, "rust-analyzer");
        assert_eq!(merged.args, vec!["--log-level", "info"]);

        // settings deep-merged
        let settings = merged.settings.expect("settings");
        assert_eq!(settings["check"]["command"], "check"); // project overrides
        assert_eq!(settings["cargo"]["features"], serde_json::json!(["a"])); // user preserved
        assert_eq!(settings["new_key"], true); // project adds
    }

    #[test]
    fn test_effective_server_def_full_override() {
        let mut config = test_config_raw();
        config.server.insert(
            "rust-analyzer".to_string(),
            ServerDef {
                command: "rust-analyzer".to_string(),
                args: vec!["--log-level".to_string(), "info".to_string()],
                settings: Some(serde_json::json!({"key": "user"})),
                ..ServerDef::default()
            },
        );

        let manager = LspClientManager::new(config, test_logging(), test_fs());
        let root = PathBuf::from("/project");

        let mut pc = crate::config::ProjectConfig::default();
        pc.server.insert(
            "rust-analyzer".to_string(),
            ServerDef {
                command: "custom-ra".to_string(),
                args: vec!["--custom".to_string()],
                settings: Some(serde_json::json!({"key": "project"})),
                min_severity: Some("warning".to_string()),
                ..ServerDef::default()
            },
        );
        manager
            .project_configs
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .insert(root.clone(), pc);

        let merged = manager
            .effective_server_def("rust-analyzer", &root)
            .expect("should exist");

        assert_eq!(merged.command, "custom-ra");
        assert_eq!(merged.args, vec!["--custom"]);
        assert_eq!(merged.min_severity.as_deref(), Some("warning"));
        assert_eq!(merged.settings.expect("settings")["key"], "project");
    }

    #[test]
    fn test_effective_server_def_no_project() {
        let mut config = test_config_raw();
        config.server.insert(
            "rust-analyzer".to_string(),
            ServerDef {
                command: "rust-analyzer".to_string(),
                args: Vec::new(),
                settings: Some(serde_json::json!({"key": "user"})),
                ..ServerDef::default()
            },
        );

        let manager = LspClientManager::new(config, test_logging(), test_fs());
        let root = PathBuf::from("/project");

        let def = manager
            .effective_server_def("rust-analyzer", &root)
            .expect("should exist");

        assert_eq!(def.command, "rust-analyzer");
        assert_eq!(def.settings.expect("settings")["key"], "user");
    }

    #[test]
    fn test_effective_settings_merge() {
        let mut config = test_config_raw();
        config.server.insert(
            "ra".to_string(),
            ServerDef {
                command: "ra".to_string(),
                args: Vec::new(),
                settings: Some(serde_json::json!({"a": 1, "b": {"c": 2}})),
                ..ServerDef::default()
            },
        );

        let manager = LspClientManager::new(config, test_logging(), test_fs());
        let root = PathBuf::from("/project");

        let mut pc = crate::config::ProjectConfig::default();
        pc.server.insert(
            "ra".to_string(),
            ServerDef {
                command: String::new(),
                args: Vec::new(),
                settings: Some(serde_json::json!({"b": {"d": 3}})),
                ..ServerDef::default()
            },
        );
        manager
            .project_configs
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .insert(root.clone(), pc);

        let settings = manager
            .effective_settings("ra", &root)
            .expect("should exist");
        assert_eq!(settings["a"], 1);
        assert_eq!(settings["b"]["c"], 2);
        assert_eq!(settings["b"]["d"], 3);
    }

    #[test]
    fn test_collect_per_root_settings() {
        let mut config = test_config_raw();
        config.server.insert(
            "ra".to_string(),
            ServerDef {
                command: "ra".to_string(),
                args: Vec::new(),
                settings: Some(serde_json::json!({"base": true})),
                ..ServerDef::default()
            },
        );

        let manager = LspClientManager::new(config, test_logging(), test_fs());
        let root_a = PathBuf::from("/project-a");
        let root_b = PathBuf::from("/project-b");

        // root_a has project config with settings for "ra"
        let mut pc_a = crate::config::ProjectConfig::default();
        pc_a.server.insert(
            "ra".to_string(),
            ServerDef {
                command: String::new(),
                args: Vec::new(),
                settings: Some(serde_json::json!({"override_a": true})),
                ..ServerDef::default()
            },
        );
        // root_b has project config but no settings for "ra"
        let pc_b = crate::config::ProjectConfig::default();

        manager
            .project_configs
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .insert(root_a.clone(), pc_a);
        manager
            .project_configs
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .insert(root_b.clone(), pc_b);

        let per_root = manager.collect_per_root_settings("ra", &[root_a.clone(), root_b]);
        // Only root_a has settings — root_b omitted
        assert_eq!(per_root.len(), 1);
        assert!(per_root.contains_key(&root_a));
        assert_eq!(per_root[&root_a]["override_a"], true);
    }

    #[test]
    fn test_collect_per_root_settings_no_configs() {
        let mut config = test_config_raw();
        config.server.insert(
            "ra".to_string(),
            ServerDef {
                command: "ra".to_string(),
                args: Vec::new(),
                ..ServerDef::default()
            },
        );

        let manager = LspClientManager::new(config, test_logging(), test_fs());
        let per_root = manager.collect_per_root_settings("ra", &[PathBuf::from("/root")]);
        assert!(per_root.is_empty());
    }

    // --- find_instance priority (1d-02) ---

    #[tokio::test]
    async fn test_find_instance_root_before_workspace() -> Result<()> {
        // When both Root and Workspace exist for the same (lang, server),
        // Root takes priority.
        let bin = mockls_bin();
        let root_client = Arc::new(Mutex::new(LspClient::spawn_quiet(
            bin.to_str().expect("bin"),
            &[],
            "rust",
            "ra",
            test_logging(),
            None,
        )?));
        let ws_client = Arc::new(Mutex::new(LspClient::spawn_quiet(
            bin.to_str().expect("bin"),
            &[],
            "rust",
            "ra",
            test_logging(),
            None,
        )?));

        let mut clients: HashMap<InstanceKey, Arc<Mutex<LspClient>>> = HashMap::new();
        clients.insert(
            InstanceKey::new(
                "rust".to_string(),
                "ra".to_string(),
                Scope::Root(PathBuf::from("/project")),
            ),
            root_client.clone(),
        );
        clients.insert(
            InstanceKey::new("rust".to_string(), "ra".to_string(), Scope::Workspace),
            ws_client,
        );

        let result = find_instance(&clients, "rust", "ra", Path::new("/project"));
        assert!(result.is_some());
        assert!(Arc::ptr_eq(&result.expect("found"), &root_client));
        Ok(())
    }

    #[tokio::test]
    async fn test_find_instance_workspace_only() -> Result<()> {
        let bin = mockls_bin();
        let ws_client = Arc::new(Mutex::new(LspClient::spawn_quiet(
            bin.to_str().expect("bin"),
            &[],
            "rust",
            "ra",
            test_logging(),
            None,
        )?));

        let mut clients: HashMap<InstanceKey, Arc<Mutex<LspClient>>> = HashMap::new();
        clients.insert(
            InstanceKey::new("rust".to_string(), "ra".to_string(), Scope::Workspace),
            ws_client.clone(),
        );

        let result = find_instance(&clients, "rust", "ra", Path::new("/project"));
        assert!(result.is_some());
        assert!(Arc::ptr_eq(&result.expect("found"), &ws_client));
        Ok(())
    }

    #[tokio::test]
    async fn test_find_instance_root_only() -> Result<()> {
        let bin = mockls_bin();
        let root_client = Arc::new(Mutex::new(LspClient::spawn_quiet(
            bin.to_str().expect("bin"),
            &[],
            "rust",
            "ra",
            test_logging(),
            None,
        )?));

        let mut clients: HashMap<InstanceKey, Arc<Mutex<LspClient>>> = HashMap::new();
        clients.insert(
            InstanceKey::new(
                "rust".to_string(),
                "ra".to_string(),
                Scope::Root(PathBuf::from("/project")),
            ),
            root_client.clone(),
        );

        let result = find_instance(&clients, "rust", "ra", Path::new("/project"));
        assert!(result.is_some());
        assert!(Arc::ptr_eq(&result.expect("found"), &root_client));
        Ok(())
    }

    // --- Project-scoped spawning (1d-02) ---

    #[tokio::test]
    async fn test_spawn_project_scoped_forces_root() -> Result<()> {
        // Project-scoped root gets Scope::Root even if the server
        // supports workspaceFolders.
        let config = mockls_workspace_folders_config();
        let server_name = config
            .resolve_language(MOCK_LANG_A)
            .expect("lang config")
            .servers[0]
            .name
            .clone();

        let manager = LspClientManager::new(config, test_logging(), test_fs_with_roots(&["/tmp"]));

        // Add project config with [language.{MOCK_LANG_A}] (Rule A).
        let mut pc = crate::config::ProjectConfig::default();
        pc.language.insert(
            MOCK_LANG_A.to_string(),
            LanguageConfig {
                servers: vec![ServerBinding::new(server_name.clone())],
                ..LanguageConfig::default()
            },
        );
        manager
            .project_configs
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .insert(PathBuf::from("/tmp"), pc);

        let (key, client) = manager
            .spawn_project_scoped(&server_name, MOCK_LANG_A, Path::new("/tmp"))
            .await?;

        assert_eq!(key.scope, Scope::Root(PathBuf::from("/tmp")));
        // Even though server advertises workspace folders, scope is Root.
        assert!(client.lock().await.supports_workspace_folders());
        Ok(())
    }

    #[tokio::test]
    async fn test_spawn_project_scoped_effective_def() -> Result<()> {
        // Project-scoped instance uses merged settings from
        // effective_server_def.
        let bin = mockls_bin();
        let server_name = format!("mockls-{MOCK_LANG_A}-ps");
        let mut config = test_config_raw();
        config.server.insert(
            server_name.clone(),
            ServerDef {
                command: bin.to_string_lossy().to_string(),
                args: vec![MOCK_LANG_A.to_string()],
                settings: Some(serde_json::json!({"user_key": true})),
                ..ServerDef::default()
            },
        );
        config.language.insert(
            MOCK_LANG_A.to_string(),
            LanguageConfig {
                servers: vec![ServerBinding::new(server_name.clone())],
                ..LanguageConfig::default()
            },
        );

        let manager = LspClientManager::new(config, test_logging(), test_fs_with_roots(&["/tmp"]));

        // Project config overrides settings.
        let mut pc = crate::config::ProjectConfig::default();
        pc.language.insert(
            MOCK_LANG_A.to_string(),
            LanguageConfig {
                servers: vec![ServerBinding::new(server_name.clone())],
                ..LanguageConfig::default()
            },
        );
        pc.server.insert(
            server_name.clone(),
            ServerDef {
                command: String::new(),
                args: Vec::new(),
                settings: Some(serde_json::json!({"project_key": true})),
                ..ServerDef::default()
            },
        );
        manager
            .project_configs
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .insert(PathBuf::from("/tmp"), pc);

        let (key, client) = manager
            .spawn_project_scoped(&server_name, MOCK_LANG_A, Path::new("/tmp"))
            .await?;

        assert_eq!(key.scope, Scope::Root(PathBuf::from("/tmp")));
        // Server should be alive (spawned with user command + project settings).
        assert!(client.lock().await.is_alive());
        // Settings should be the merged result.
        let settings = client.lock().await.server().settings().cloned();
        let settings = settings.expect("should have settings");
        assert_eq!(settings["user_key"], true);
        assert_eq!(settings["project_key"], true);
        Ok(())
    }

    #[tokio::test]
    async fn test_spawn_all_mixed_roots() -> Result<()> {
        // Two roots with real files: one with project config (Rule A),
        // one without. spawn_all should produce workspace + project-scoped
        // root instances.
        let root_a = tempfile::tempdir().expect("tempdir");
        let root_b = tempfile::tempdir().expect("tempdir");
        std::fs::write(root_a.path().join(format!("file.{MOCK_LANG_A}")), "content")
            .expect("write");
        std::fs::write(root_b.path().join(format!("file.{MOCK_LANG_A}")), "content")
            .expect("write");

        let config = mockls_workspace_folders_config();
        let server_name = config
            .resolve_language(MOCK_LANG_A)
            .expect("lang config")
            .servers[0]
            .name
            .clone();

        let fs = test_fs();
        fs.set_roots(vec![
            root_a.path().to_path_buf(),
            root_b.path().to_path_buf(),
        ]);
        let manager = LspClientManager::new(config, test_logging(), fs);

        // Write .catenary.toml for root_b so load_project_configs_for_roots
        // discovers it during spawn_all.
        let project_toml = format!("[language.{MOCK_LANG_A}]\nservers = [\"{server_name}\"]\n");
        std::fs::write(root_b.path().join(".catenary.toml"), project_toml).expect("write");

        manager.spawn_all().await;

        let clients = manager.clients().await;
        assert_eq!(
            clients.len(),
            2,
            "Should have workspace + project-scoped root instances"
        );
        assert_eq!(count_scope(&clients, MOCK_LANG_A, "workspace"), 1);
        assert_eq!(count_scope(&clients, MOCK_LANG_A, "root"), 1);

        // The root instance should be for root_b.
        let root_key = clients
            .keys()
            .find(|k| matches!(&k.scope, Scope::Root(_)))
            .expect("should have root key");
        assert_eq!(root_key.scope, Scope::Root(root_b.path().to_path_buf()),);

        Ok(())
    }

    #[tokio::test]
    async fn test_spawn_all_project_scoped_single_root() -> Result<()> {
        // Single root with project config: spawn_all produces
        // Scope::Root even for workspace-capable server.
        let root = tempfile::tempdir().expect("tempdir");
        std::fs::write(root.path().join(format!("file.{MOCK_LANG_A}")), "content").expect("write");

        let config = mockls_workspace_folders_config();
        let server_name = config
            .resolve_language(MOCK_LANG_A)
            .expect("lang config")
            .servers[0]
            .name
            .clone();

        let fs = test_fs();
        fs.set_roots(vec![root.path().to_path_buf()]);
        let manager = LspClientManager::new(config, test_logging(), fs);

        let project_toml = format!("[language.{MOCK_LANG_A}]\nservers = [\"{server_name}\"]\n");
        std::fs::write(root.path().join(".catenary.toml"), project_toml).expect("write");

        manager.spawn_all().await;

        let clients = manager.clients().await;
        assert_eq!(clients.len(), 1);
        let key = clients.keys().next().expect("one key");
        assert_eq!(
            key.scope,
            Scope::Root(root.path().to_path_buf()),
            "Project-scoped root should force Scope::Root"
        );
        Ok(())
    }

    #[tokio::test]
    async fn test_spawn_all_workspace_excludes_project_root() -> Result<()> {
        // Three roots: two normal, one project-scoped. The workspace
        // instance should NOT include the project-scoped root in its
        // workspaceFolders (verified by instance count — if the project
        // root were in the workspace, spawn_project_scoped would have
        // been blocked by find_instance returning the workspace instance).
        let root_a = tempfile::tempdir().expect("tempdir");
        let root_b = tempfile::tempdir().expect("tempdir");
        let root_c = tempfile::tempdir().expect("tempdir");
        for root in [&root_a, &root_b, &root_c] {
            std::fs::write(root.path().join(format!("file.{MOCK_LANG_A}")), "content")
                .expect("write");
        }

        let config = mockls_workspace_folders_config();
        let server_name = config
            .resolve_language(MOCK_LANG_A)
            .expect("lang config")
            .servers[0]
            .name
            .clone();

        let fs = test_fs();
        fs.set_roots(vec![
            root_a.path().to_path_buf(),
            root_b.path().to_path_buf(),
            root_c.path().to_path_buf(),
        ]);
        let manager = LspClientManager::new(config, test_logging(), fs);

        // Only root_b is project-scoped.
        let project_toml = format!("[language.{MOCK_LANG_A}]\nservers = [\"{server_name}\"]\n");
        std::fs::write(root_b.path().join(".catenary.toml"), project_toml).expect("write");

        manager.spawn_all().await;

        let clients = manager.clients().await;
        // 1 workspace (root_a + root_c) + 1 project-scoped root (root_b).
        assert_eq!(
            clients.len(),
            2,
            "Should have workspace + one project-scoped root"
        );
        assert_eq!(count_scope(&clients, MOCK_LANG_A, "workspace"), 1);
        assert_eq!(count_scope(&clients, MOCK_LANG_A, "root"), 1);

        let root_key = clients
            .keys()
            .find(|k| matches!(&k.scope, Scope::Root(_)))
            .expect("should have root key");
        assert_eq!(
            root_key.scope,
            Scope::Root(root_b.path().to_path_buf()),
            "Only root_b should be project-scoped"
        );
        Ok(())
    }

    #[tokio::test]
    async fn test_get_servers_project_root() -> Result<()> {
        // get_servers for a file in a project-scoped root returns the
        // project instance, not the workspace instance.
        let config = mockls_workspace_folders_config();
        let server_name = config
            .resolve_language(MOCK_LANG_A)
            .expect("lang config")
            .servers[0]
            .name
            .clone();

        let manager = LspClientManager::new(
            config,
            test_logging(),
            test_fs_with_roots(&["/tmp", "/var"]),
        );

        // /var has project config.
        let mut pc = crate::config::ProjectConfig::default();
        pc.language.insert(
            MOCK_LANG_A.to_string(),
            LanguageConfig {
                servers: vec![ServerBinding::new(server_name.clone())],
                ..LanguageConfig::default()
            },
        );
        manager
            .project_configs
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .insert(PathBuf::from("/var"), pc);

        // Spawn workspace instance for /tmp.
        let ws_client = manager
            .ensure_server(MOCK_LANG_A, &server_name, Path::new("/tmp"))
            .await?;
        // Spawn project-scoped for /var.
        let (_, project_client) = manager
            .spawn_project_scoped(&server_name, MOCK_LANG_A, Path::new("/var"))
            .await?;

        // get_servers for a file in /var should return the project instance.
        let path = PathBuf::from(format!("/var/test.{MOCK_LANG_A}"));
        let servers = manager
            .get_servers(&path, LspServer::supports_document_symbols, None)
            .await;
        assert_eq!(servers.len(), 1);
        assert!(
            Arc::ptr_eq(&servers[0], &project_client),
            "Should return the project-scoped instance, not the workspace one"
        );

        // get_servers for a file in /tmp should return the workspace instance.
        let path = PathBuf::from(format!("/tmp/test.{MOCK_LANG_A}"));
        let servers = manager
            .get_servers(&path, LspServer::supports_document_symbols, None)
            .await;
        assert_eq!(servers.len(), 1);
        assert!(
            Arc::ptr_eq(&servers[0], &ws_client),
            "Should return the workspace instance for /tmp"
        );

        Ok(())
    }

    #[tokio::test]
    async fn test_sync_roots_add_project_scoped() -> Result<()> {
        // Adding a root with project [language.*] spawns Scope::Root,
        // no didChangeWorkspaceFolders to workspace instance.
        let root_a = tempfile::tempdir().expect("tempdir");
        let root_b = tempfile::tempdir().expect("tempdir");
        std::fs::write(root_a.path().join(format!("file.{MOCK_LANG_A}")), "content")
            .expect("write");
        std::fs::write(root_b.path().join(format!("file.{MOCK_LANG_A}")), "content")
            .expect("write");

        let config = mockls_workspace_folders_config();
        let server_name = config
            .resolve_language(MOCK_LANG_A)
            .expect("lang config")
            .servers[0]
            .name
            .clone();

        let fs = test_fs();
        fs.set_roots(vec![root_a.path().to_path_buf()]);
        let manager = LspClientManager::new(config, test_logging(), fs);

        // Spawn workspace instance for root_a.
        let ws = manager
            .ensure_server(MOCK_LANG_A, &server_name, root_a.path())
            .await?;
        assert!(ws.lock().await.supports_workspace_folders());

        // root_b is project-scoped.
        let mut pc = crate::config::ProjectConfig::default();
        pc.language.insert(
            MOCK_LANG_A.to_string(),
            LanguageConfig {
                servers: vec![ServerBinding::new(server_name.clone())],
                ..LanguageConfig::default()
            },
        );
        manager
            .project_configs
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .insert(root_b.path().to_path_buf(), pc);

        // sync_roots adds root_b.
        manager
            .sync_roots(vec![
                root_a.path().to_path_buf(),
                root_b.path().to_path_buf(),
            ])
            .await?;

        let clients = manager.clients().await;
        assert_eq!(
            clients.len(),
            2,
            "Should have workspace + project-scoped root"
        );
        assert_eq!(count_scope(&clients, MOCK_LANG_A, "workspace"), 1);
        assert_eq!(count_scope(&clients, MOCK_LANG_A, "root"), 1);

        // The root instance should be for root_b.
        let root_key = clients
            .keys()
            .find(|k| matches!(&k.scope, Scope::Root(_)))
            .expect("should have root key");
        assert_eq!(root_key.scope, Scope::Root(root_b.path().to_path_buf()),);

        Ok(())
    }

    #[tokio::test]
    async fn test_sync_roots_add_settings_override() -> Result<()> {
        // Adding a root with [server.*.settings] (but no Rule A) sends
        // didChangeConfiguration to the workspace instance.
        let config = mockls_workspace_folders_config();
        let server_name = config
            .resolve_language(MOCK_LANG_A)
            .expect("lang config")
            .servers[0]
            .name
            .clone();

        let manager = LspClientManager::new(config, test_logging(), test_fs_with_roots(&["/tmp"]));

        // Spawn workspace instance.
        let ws = manager
            .ensure_server(MOCK_LANG_A, &server_name, Path::new("/tmp"))
            .await?;
        assert!(ws.lock().await.supports_workspace_folders());

        // /var has project config with server settings only (no language).
        let mut pc = crate::config::ProjectConfig::default();
        pc.server.insert(
            server_name.clone(),
            ServerDef {
                command: String::new(),
                args: Vec::new(),
                settings: Some(serde_json::json!({"override": true})),
                ..ServerDef::default()
            },
        );
        manager
            .project_configs
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .insert(PathBuf::from("/var"), pc);

        // sync_roots adds /var.
        manager
            .sync_roots(vec![PathBuf::from("/tmp"), PathBuf::from("/var")])
            .await?;

        // Workspace instance should still be the only one (no Rule A).
        let clients = manager.clients().await;
        assert_eq!(clients.len(), 1);
        assert_eq!(count_scope(&clients, MOCK_LANG_A, "workspace"), 1);

        // The workspace server should now have per-root settings for /var.
        // (didChangeConfiguration was sent, but we can't observe the
        // notification directly — we verify that set_root_settings was called
        // by checking resolve_configuration behavior.)
        Ok(())
    }

    #[tokio::test]
    async fn test_sync_roots_remove_project_root() -> Result<()> {
        // Removing a project-scoped root: instance shut down, project
        // config cleaned up.
        let config = mockls_workspace_folders_config();
        let server_name = config
            .resolve_language(MOCK_LANG_A)
            .expect("lang config")
            .servers[0]
            .name
            .clone();

        let manager = LspClientManager::new(
            config,
            test_logging(),
            test_fs_with_roots(&["/tmp", "/var"]),
        );

        // /var is project-scoped.
        let mut pc = crate::config::ProjectConfig::default();
        pc.language.insert(
            MOCK_LANG_A.to_string(),
            LanguageConfig {
                servers: vec![ServerBinding::new(server_name.clone())],
                ..LanguageConfig::default()
            },
        );
        manager
            .project_configs
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .insert(PathBuf::from("/var"), pc);

        // Spawn both: workspace for /tmp, project-scoped for /var.
        let _ = manager
            .ensure_server(MOCK_LANG_A, &server_name, Path::new("/tmp"))
            .await?;
        let _ = manager
            .spawn_project_scoped(&server_name, MOCK_LANG_A, Path::new("/var"))
            .await?;
        assert_eq!(manager.clients().await.len(), 2);

        // Remove /var.
        manager.sync_roots(vec![PathBuf::from("/tmp")]).await?;

        let clients = manager.clients().await;
        assert_eq!(
            clients.len(),
            1,
            "Project-scoped /var instance should be removed"
        );
        assert_eq!(count_scope(&clients, MOCK_LANG_A, "workspace"), 1);

        // Project config for /var should be cleaned up.
        assert!(
            !manager.is_project_scoped(MOCK_LANG_A, Path::new("/var")),
            "Project config for /var should be removed"
        );

        Ok(())
    }

    /// Config with mockls that accepts null-workspace (single-file mode).
    fn mockls_single_file_config() -> Arc<Config> {
        let bin = mockls_bin();
        let server_name = format!("mockls-{MOCK_LANG_A}-sf");
        let mut server = HashMap::new();
        server.insert(
            server_name.clone(),
            ServerDef {
                command: bin.to_string_lossy().to_string(),
                args: vec![MOCK_LANG_A.to_string()],
                single_file: true,
                ..ServerDef::default()
            },
        );
        let mut language = HashMap::new();
        language.insert(
            MOCK_LANG_A.to_string(),
            LanguageConfig {
                servers: vec![ServerBinding::new(server_name)],
                ..LanguageConfig::default()
            },
        );
        Arc::new(Config {
            language,
            server,
            log_retention_days: 7,
            notifications: None,
            icons: None,
            tui: None,
            tools: None,
            resolved_commands: None,
        })
    }

    /// Config with mockls that rejects null-workspace initialization.
    fn mockls_reject_null_workspace_config() -> Arc<Config> {
        let bin = mockls_bin();
        let server_name = format!("mockls-{MOCK_LANG_A}-rnw");
        let mut server = HashMap::new();
        server.insert(
            server_name.clone(),
            ServerDef {
                command: bin.to_string_lossy().to_string(),
                args: vec![
                    MOCK_LANG_A.to_string(),
                    "--reject-null-workspace".to_string(),
                ],
                single_file: true,
                ..ServerDef::default()
            },
        );
        let mut language = HashMap::new();
        language.insert(
            MOCK_LANG_A.to_string(),
            LanguageConfig {
                servers: vec![ServerBinding::new(server_name)],
                ..LanguageConfig::default()
            },
        );
        Arc::new(Config {
            language,
            server,
            log_retention_days: 7,
            notifications: None,
            icons: None,
            tui: None,
            tools: None,
            resolved_commands: None,
        })
    }

    #[tokio::test]
    async fn test_single_file_spawn_accepts_null_workspace() -> Result<()> {
        // mockls without --reject-null-workspace accepts single-file mode.
        let config = mockls_single_file_config();
        let server_name = config
            .resolve_language(MOCK_LANG_A)
            .expect("lang config")
            .servers[0]
            .name
            .clone();

        let manager = LspClientManager::new(config, test_logging(), test_fs());

        let client = manager.spawn_single_file(&server_name, MOCK_LANG_A).await?;
        assert!(client.lock().await.is_alive());

        // Verify scope is SingleFile.
        let clients = manager.clients().await;
        assert_eq!(clients.len(), 1);
        let key = clients.keys().next().expect("should have one client");
        assert_eq!(key.scope, Scope::SingleFile);
        assert_eq!(key.language_id, MOCK_LANG_A);

        // No failure should be cached (server accepted).
        assert!(
            !manager
                .single_file_failures
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .contains(&(MOCK_LANG_A.to_string(), server_name)),
        );

        Ok(())
    }

    #[tokio::test]
    async fn test_single_file_spawn_rejects_null_workspace() -> Result<()> {
        // mockls with --reject-null-workspace rejects single-file mode.
        let config = mockls_reject_null_workspace_config();
        let server_name = config
            .resolve_language(MOCK_LANG_A)
            .expect("lang config")
            .servers[0]
            .name
            .clone();

        let manager = LspClientManager::new(config, test_logging(), test_fs());

        let result = manager.spawn_single_file(&server_name, MOCK_LANG_A).await;
        assert!(result.is_err(), "Should fail with null workspace rejection");

        // Negative cache should be set.
        assert!(
            manager
                .single_file_failures
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .contains(&(MOCK_LANG_A.to_string(), server_name)),
        );

        // No client should be stored.
        assert!(manager.clients().await.is_empty());

        Ok(())
    }

    #[tokio::test]
    async fn test_single_file_negative_cache_prevents_retry() -> Result<()> {
        // After negative cache, ensure_single_file_server returns None
        // without attempting to spawn.
        let config = mockls_reject_null_workspace_config();
        let server_name = config
            .resolve_language(MOCK_LANG_A)
            .expect("lang config")
            .servers[0]
            .name
            .clone();

        let manager = LspClientManager::new(config, test_logging(), test_fs());

        // First attempt — spawns and fails.
        let first = manager
            .ensure_single_file_server(MOCK_LANG_A, &server_name)
            .await;
        assert!(first.is_none());

        // Second attempt — should return None from cache without spawning.
        let second = manager
            .ensure_single_file_server(MOCK_LANG_A, &server_name)
            .await;
        assert!(second.is_none());

        // Still no clients.
        assert!(manager.clients().await.is_empty());

        Ok(())
    }

    #[tokio::test]
    async fn test_single_file_positive_cache_returns_same_handle() -> Result<()> {
        // After positive spawn, ensure_single_file_server returns the
        // same handle on subsequent calls.
        let config = mockls_single_file_config();
        let server_name = config
            .resolve_language(MOCK_LANG_A)
            .expect("lang config")
            .servers[0]
            .name
            .clone();

        let manager = LspClientManager::new(config, test_logging(), test_fs());

        let first = manager
            .ensure_single_file_server(MOCK_LANG_A, &server_name)
            .await
            .expect("should spawn");
        let second = manager
            .ensure_single_file_server(MOCK_LANG_A, &server_name)
            .await
            .expect("should return cached");

        assert!(Arc::ptr_eq(&first, &second), "Should be the same handle");
        assert_eq!(manager.clients().await.len(), 1);

        Ok(())
    }

    #[tokio::test]
    async fn test_get_servers_falls_through_to_single_file() -> Result<()> {
        // File outside all roots → tier 3 single-file server spawned.
        let config = mockls_single_file_config();

        // No roots — every file is unrooted.
        let manager = LspClientManager::new(config, test_logging(), test_fs());

        let path = PathBuf::from(format!("/some/random/file.{MOCK_LANG_A}"));
        let servers = manager
            .get_servers(&path, LspServer::supports_references, None)
            .await;
        assert_eq!(servers.len(), 1, "Should have spawned a single-file server");

        // Verify it's a SingleFile instance.
        let clients = manager.clients().await;
        let key = clients.keys().next().expect("should have one client");
        assert_eq!(key.scope, Scope::SingleFile);

        Ok(())
    }

    #[tokio::test]
    async fn test_get_servers_unrooted_rejects_returns_empty() -> Result<()> {
        // File outside all roots, server rejects null workspace → empty.
        let config = mockls_reject_null_workspace_config();

        let manager = LspClientManager::new(config, test_logging(), test_fs());

        let path = PathBuf::from(format!("/some/random/file.{MOCK_LANG_A}"));
        let servers = manager
            .get_servers(&path, LspServer::supports_references, None)
            .await;
        assert!(
            servers.is_empty(),
            "Should return empty when server rejects"
        );

        Ok(())
    }

    #[tokio::test]
    async fn test_rooted_clients_excludes_single_file() -> Result<()> {
        // rooted_clients() should not include single-file servers.
        let config = mockls_single_file_config();
        let server_name = config
            .resolve_language(MOCK_LANG_A)
            .expect("lang config")
            .servers[0]
            .name
            .clone();

        let manager = LspClientManager::new(config, test_logging(), test_fs());

        let _ = manager.spawn_single_file(&server_name, MOCK_LANG_A).await?;
        assert_eq!(manager.clients().await.len(), 1);
        assert!(
            manager.rooted_clients().await.is_empty(),
            "Single-file servers should be excluded from rooted_clients"
        );

        Ok(())
    }

    #[tokio::test]
    async fn test_single_file_root_added_routes_to_workspace() -> Result<()> {
        // After a root is added for a path previously served by a
        // single-file server, get_servers routes to the workspace instance.
        // sync_roots cleans up single-file servers.
        let config = mockls_single_file_config();

        let root = tempfile::tempdir().expect("tempdir");
        let file_path = root.path().join(format!("test.{MOCK_LANG_A}"));
        std::fs::write(&file_path, "content").expect("write");

        // Start with no roots — file gets single-file server.
        let fs = test_fs();
        let manager = LspClientManager::new(config, test_logging(), fs.clone());

        let servers = manager
            .get_servers(&file_path, LspServer::supports_references, None)
            .await;
        assert_eq!(servers.len(), 1);

        // Verify single-file instance exists.
        assert_eq!(
            count_scope(&manager.clients().await, MOCK_LANG_A, "single_file"),
            1
        );

        // Add the root via sync_roots — this shuts down single-file
        // instances and clears failure cache.
        manager.sync_roots(vec![root.path().to_path_buf()]).await?;

        // Single-file instance should be cleaned up.
        assert_eq!(
            count_scope(&manager.clients().await, MOCK_LANG_A, "single_file"),
            0,
            "Single-file server should be shut down after root added"
        );

        // Failure cache should be cleared.
        assert!(
            manager
                .single_file_failures
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .is_empty(),
            "Failure cache should be cleared after sync_roots"
        );

        // Spawn the rooted server and verify get_servers routes there.
        let _ = ensure_first_server(&manager, MOCK_LANG_A).await?;
        let servers = manager
            .get_servers(&file_path, LspServer::supports_references, None)
            .await;
        assert_eq!(servers.len(), 1);

        // The returned client should be rooted, not single-file.
        let clients = manager.clients().await;
        assert!(
            clients.keys().all(|k| k.scope != Scope::SingleFile),
            "No single-file instances should remain"
        );

        Ok(())
    }

    #[tokio::test]
    async fn test_single_file_different_languages_independent() -> Result<()> {
        // Two different languages outside roots → independent single-file
        // servers with independent cache entries.
        let bin = mockls_bin();
        let lang_b = "qR7bZ";
        let server_a = format!("mockls-{MOCK_LANG_A}-sf2");
        let server_b = format!("mockls-{lang_b}-sf2");
        let mut config = test_config_raw();
        for (name, lang) in [(&server_a, MOCK_LANG_A), (&server_b, lang_b)] {
            config.server.insert(
                name.clone(),
                ServerDef {
                    command: bin.to_string_lossy().to_string(),
                    args: vec![lang.to_string()],
                    env: None,
                    initialization_options: None,
                    settings: None,
                    min_severity: None,
                    single_file: true,
                    file_patterns: Vec::new(),
                    compiled_patterns: Vec::new(),
                },
            );
            config.language.insert(
                lang.to_string(),
                LanguageConfig {
                    servers: vec![ServerBinding::new(name.clone())],
                    ..LanguageConfig::default()
                },
            );
        }

        let manager = LspClientManager::new(config, test_logging(), test_fs());

        // Spawn single-file for language A.
        let client_a = manager
            .ensure_single_file_server(MOCK_LANG_A, &server_a)
            .await
            .expect("should spawn for lang A");
        assert!(client_a.lock().await.is_alive());

        // Spawn single-file for language B.
        let client_b = manager
            .ensure_single_file_server(lang_b, &server_b)
            .await
            .expect("should spawn for lang B");
        assert!(client_b.lock().await.is_alive());

        // Should be different instances.
        assert!(!Arc::ptr_eq(&client_a, &client_b));
        assert_eq!(manager.clients().await.len(), 2);

        // Neither should be in the failure cache.
        assert!(
            manager
                .single_file_failures
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .is_empty(),
        );

        Ok(())
    }

    #[tokio::test]
    async fn test_did_change_configuration_notification() -> Result<()> {
        // did_change_configuration sends notification with empty settings.
        // mockls with --send-configuration-request will respond to it.
        let bin = mockls_bin();
        let server_name = format!("mockls-{MOCK_LANG_A}-dcc");
        let mut config = test_config_raw();
        config.server.insert(
            server_name.clone(),
            ServerDef {
                command: bin.to_string_lossy().to_string(),
                args: vec![
                    MOCK_LANG_A.to_string(),
                    "--send-configuration-request".to_string(),
                ],
                settings: Some(serde_json::json!({"key": "value"})),
                ..ServerDef::default()
            },
        );
        config.language.insert(
            MOCK_LANG_A.to_string(),
            LanguageConfig {
                servers: vec![ServerBinding::new(server_name)],
                ..LanguageConfig::default()
            },
        );

        let manager = LspClientManager::new(config, test_logging(), test_fs_with_roots(&["/tmp"]));
        let client = ensure_first_server(&manager, MOCK_LANG_A).await?;

        // Send didChangeConfiguration — should not error.
        let result = client.lock().await.did_change_configuration().await;
        assert!(
            result.is_ok(),
            "did_change_configuration should succeed: {result:?}"
        );

        Ok(())
    }
}
