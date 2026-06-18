// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 Mark Wells <contact@markwells.dev>

//! Single-slot paginated result cache for grep and glob servers.
//!
//! Caches the full unpaginated output of a query so that sequential
//! page fetches (page 1 → page 2 → …) don't re-run the pipeline.
//! Invalidated on parameter change, a searched root's generation bump
//! (`sed`/diagnostics/explicit invalidation), or any *witness path's* mtime
//! changing. The witness set is the files and directories the output depends
//! on: a **file's** mtime moves on a content edit; a **directory's** mtime moves
//! when an entry is added/removed/renamed in it (POSIX), which is the signal for
//! a membership change the per-file check can't see (a *new* file has no prior
//! entry to stat). Both are what catch a host `Edit`/`Write` between two
//! identical multi-page queries — host edits don't bump root generations, so
//! generation-gating alone would serve a stale cached page (bug #26 residual).
//! Single-page results skip caching.

use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};
use std::path::{Path, PathBuf};

use super::filesystem_manager::{FilesystemManager, mtime_nanos};
use super::pagination::paginate;

/// Cache key: hash of query parameters (excluding page number).
type CacheKey = u64;

/// A cached query result.
struct CachedResult {
    /// Hash of query parameters (pattern, glob, exclude, cwd, flags, budget).
    key: CacheKey,
    /// Full unpaginated output string.
    output: String,
    /// Generation snapshot: `(root, generation)` at query time.
    generations: Vec<(PathBuf, u64)>,
    /// mtime snapshot (`mtime_nanos`) of each *witness path* the output depends
    /// on, taken at query time: the rendered **files** (content) and the
    /// **directories** that were listed/walked (membership). A host
    /// `Edit`/`Write` does not bump a root generation (only `sed`/diagnostics
    /// do), so without this an identical repeated multi-page query after an
    /// external edit — or an add/remove that a per-file check can't see — would
    /// serve a stale page (bug #26 residual). Validated by re-stat on
    /// [`ResultCache::get`].
    witness_mtimes: Vec<(PathBuf, i64)>,
}

/// Single-slot result cache.
///
/// One instance per server (grep has one, glob has one). Only the most
/// recent query is cached — sequential page fetches always repeat the
/// same query parameters.
pub(super) struct ResultCache {
    slot: Option<CachedResult>,
    budget: usize,
}

impl ResultCache {
    /// Creates a new empty cache with the given page budget.
    pub(super) const fn new(budget: usize) -> Self {
        Self { slot: None, budget }
    }

    /// Attempts to serve a page from the cache.
    ///
    /// Returns `Some(paginated_output)` on cache hit with valid generations
    /// **and** unchanged witness mtimes, `None` on miss or stale data.
    pub(super) fn get(
        &self,
        key: CacheKey,
        page: usize,
        fs_manager: &FilesystemManager,
    ) -> Option<String> {
        let entry = self.slot.as_ref()?;
        if entry.key != key {
            return None;
        }

        // Validate generations — a bump means a sed/diagnostics/explicit
        // invalidation touched a searched root.
        for (root, snapshot_gen) in &entry.generations {
            if fs_manager.root_generation(root) != *snapshot_gen {
                return None;
            }
        }

        // Validate witness mtimes — catches a host Edit/Write (file content) and
        // an add/remove/rename in a listed/walked directory (membership), neither
        // of which bumps a generation. A removed path (stat fails → `None`) also
        // misses. Re-stat cost is bounded by the witness count and far below
        // re-running the pipeline.
        for (path, snapshot_mtime) in &entry.witness_mtimes {
            let current = std::fs::metadata(path).ok().map(|m| mtime_nanos(&m));
            if current != Some(*snapshot_mtime) {
                return None;
            }
        }

        Some(paginate(&entry.output, self.budget, page))
    }

    /// Empties the single cache slot.
    ///
    /// Called on root removal: the cached page may carry enrichment for a path
    /// that is no longer tracked, and the [`Self::get`] generation/witness gates
    /// can both still validate it (a read-only root's generation reverts to 0 on
    /// removal and unchanged witnesses re-stat equal), so the slot must be
    /// dropped outright rather than left for those gates to catch (bug M1).
    pub(super) fn clear(&mut self) {
        self.slot = None;
    }

    /// Stores a query result in the cache.
    ///
    /// Skips caching when the result fits in a single page (no page 2
    /// request will follow). `witnesses` is the set of paths the output depends
    /// on — the rendered files (content) and the directories that were
    /// listed/walked (membership); their mtimes are snapshotted so a later host
    /// edit, or an add/remove in one of those directories, invalidates the cache
    /// even without a generation bump.
    pub(super) fn put(
        &mut self,
        key: CacheKey,
        output: String,
        roots: &[PathBuf],
        witnesses: &[PathBuf],
        fs_manager: &FilesystemManager,
    ) {
        let total_pages = count_pages(&output, self.budget);
        if total_pages <= 1 {
            // Single-page results: no point caching.
            self.slot = None;
            return;
        }

        let generations: Vec<(PathBuf, u64)> = roots
            .iter()
            .map(|r| (r.clone(), fs_manager.root_generation(r)))
            .collect();

        let witness_mtimes: Vec<(PathBuf, i64)> = witnesses
            .iter()
            .filter_map(|p| {
                std::fs::metadata(p)
                    .ok()
                    .map(|m| (p.clone(), mtime_nanos(&m)))
            })
            .collect();

        self.slot = Some(CachedResult {
            key,
            output,
            generations,
            witness_mtimes,
        });
    }
}

/// Computes a cache key from hashable query parameters.
///
/// The page number is excluded — only parameters that affect the
/// pipeline output are included.
pub(super) fn cache_key(params: &impl Hash) -> CacheKey {
    let mut hasher = DefaultHasher::new();
    params.hash(&mut hasher);
    hasher.finish()
}

/// Counts the number of pages the output would produce at the given budget.
fn count_pages(output: &str, budget: usize) -> usize {
    let lines: Vec<&str> = output.lines().collect();
    if lines.is_empty() {
        return 0;
    }

    let mut pages = 0usize;
    let mut current_len = 0;

    for line in &lines {
        let line_len = line.len() + 1;
        if current_len > 0 && current_len + line_len > budget {
            pages += 1;
            current_len = 0;
        }
        current_len += line_len;
    }
    // Final page.
    pages + 1
}

/// Grep cache parameters (excludes `page`).
#[derive(Hash)]
pub(super) struct GrepCacheParams<'a> {
    pub pattern: &'a str,
    pub paths: &'a [PathBuf],
    pub exclude: Option<&'a str>,
    pub include_gitignored: bool,
    pub include_hidden: bool,
    pub cwd: Option<&'a Path>,
    pub budget: usize,
}

/// Glob cache parameters (excludes `page`).
#[derive(Hash)]
pub(super) struct GlobCacheParams<'a> {
    pub paths: &'a [PathBuf],
    pub exclude: Option<&'a str>,
    pub include_gitignored: bool,
    pub include_hidden: bool,
    pub cwd: Option<&'a Path>,
    pub budget: usize,
}

#[cfg(test)]
#[allow(clippy::expect_used, reason = "tests use expect for clarity")]
mod tests {
    use super::*;

    fn make_fs_manager() -> FilesystemManager {
        FilesystemManager::new()
    }

    #[test]
    fn cache_hit_same_query() {
        let fs = make_fs_manager();
        let root = PathBuf::from("/project");
        fs.set_roots(vec![root.clone()]);

        let mut cache = ResultCache::new(20);
        let key = 42;

        // Output that spans multiple pages (each line is 7 chars + newline = 8).
        cache.put(
            key,
            "line 1\nline 2\nline 3\nline 4\nline 5\n".to_string(),
            &[root],
            &[],
            &fs,
        );

        // Should hit on same key.
        let page1 = cache
            .get(key, 1, &fs)
            .expect("cache should hit on same key");
        assert!(page1.contains("line 1"), "page 1 should have line 1");
    }

    #[test]
    fn cache_miss_on_param_change() {
        let fs = make_fs_manager();
        let root = PathBuf::from("/project");
        fs.set_roots(vec![root.clone()]);

        let mut cache = ResultCache::new(20);
        let key_a = 42;
        let key_b = 99;

        cache.put(
            key_a,
            "line 1\nline 2\nline 3\nline 4\nline 5\n".to_string(),
            &[root],
            &[],
            &fs,
        );

        // Different key should miss.
        let result = cache.get(key_b, 1, &fs);
        assert!(result.is_none(), "different key should miss");
    }

    #[test]
    fn cache_miss_on_generation_bump() {
        let fs = make_fs_manager();
        let root = PathBuf::from("/project");
        fs.set_roots(vec![root.clone()]);

        let mut cache = ResultCache::new(20);
        let key = 42;

        cache.put(
            key,
            "line 1\nline 2\nline 3\nline 4\nline 5\n".to_string(),
            std::slice::from_ref(&root),
            &[],
            &fs,
        );

        // Bump the generation (simulates a sed/diagnostics invalidation).
        fs.bump_generation_for_test(&root);

        let result = cache.get(key, 1, &fs);
        assert!(result.is_none(), "stale generation should miss");
    }

    #[test]
    fn single_page_skip_caching() {
        let fs = make_fs_manager();
        let root = PathBuf::from("/project");
        fs.set_roots(vec![root.clone()]);

        let mut cache = ResultCache::new(5000);
        let key = 42;

        // Short output that fits in one page.
        cache.put(key, "hello\n".to_string(), &[root], &[], &fs);

        // Single-page results should not be cached.
        let result = cache.get(key, 1, &fs);
        assert!(result.is_none(), "single-page result should not be cached");
    }

    /// A host `Edit`/`Write` to a rendered file advances its mtime but does not
    /// bump a root generation; the cache must still miss (bug #26 residual).
    #[test]
    fn cache_miss_on_file_mtime_change() {
        let dir = tempfile::tempdir().expect("tempdir");
        let file = dir.path().join("rendered.txt");
        std::fs::write(&file, "original\n").expect("write");

        let fs = make_fs_manager();
        fs.set_roots(vec![dir.path().to_path_buf()]);

        let mut cache = ResultCache::new(20);
        let key = 42;

        cache.put(
            key,
            "line 1\nline 2\nline 3\nline 4\nline 5\n".to_string(),
            &[dir.path().to_path_buf()],
            std::slice::from_ref(&file),
            &fs,
        );
        assert!(
            cache.get(key, 1, &fs).is_some(),
            "fresh cache should hit before any edit"
        );

        // Rewrite the rendered file with a strictly-newer mtime (no generation
        // bump — mirrors a host Edit/Write).
        std::fs::write(&file, "edited\n").expect("rewrite");
        let f = std::fs::File::options()
            .write(true)
            .open(&file)
            .expect("open");
        f.set_modified(std::time::SystemTime::now() + std::time::Duration::from_secs(10))
            .expect("set mtime");
        drop(f);

        assert!(
            cache.get(key, 1, &fs).is_none(),
            "an edit to a rendered file must invalidate the cache"
        );
    }

    /// A removed rendered file (stat fails) also invalidates the cache.
    #[test]
    fn cache_miss_on_rendered_file_removed() {
        let dir = tempfile::tempdir().expect("tempdir");
        let file = dir.path().join("rendered.txt");
        std::fs::write(&file, "original\n").expect("write");

        let fs = make_fs_manager();
        fs.set_roots(vec![dir.path().to_path_buf()]);

        let mut cache = ResultCache::new(20);
        let key = 42;
        cache.put(
            key,
            "line 1\nline 2\nline 3\nline 4\nline 5\n".to_string(),
            &[dir.path().to_path_buf()],
            std::slice::from_ref(&file),
            &fs,
        );

        std::fs::remove_file(&file).expect("remove");
        assert!(
            cache.get(key, 1, &fs).is_none(),
            "a removed rendered file must invalidate the cache"
        );
    }

    /// A directory witness invalidates the cache when its mtime moves — the
    /// signal for an entry being added/removed/renamed in it (a membership
    /// change the per-file check can't see, since a new file has no prior entry
    /// to stat).
    #[test]
    fn cache_miss_on_witness_dir_change() {
        let dir = tempfile::tempdir().expect("tempdir");
        let listed = dir.path().join("listed");
        std::fs::create_dir(&listed).expect("mkdir");
        std::fs::write(listed.join("a.txt"), "x\n").expect("seed entry");

        let fs = make_fs_manager();
        fs.set_roots(vec![dir.path().to_path_buf()]);

        let mut cache = ResultCache::new(20);
        let key = 7;
        cache.put(
            key,
            "line 1\nline 2\nline 3\nline 4\nline 5\n".to_string(),
            &[dir.path().to_path_buf()],
            std::slice::from_ref(&listed),
            &fs,
        );
        assert!(
            cache.get(key, 1, &fs).is_some(),
            "fresh cache should hit before the directory changes"
        );

        // Bump the listed directory's mtime, as adding/removing an entry would.
        let d = std::fs::File::open(&listed).expect("open dir");
        d.set_modified(std::time::SystemTime::now() + std::time::Duration::from_secs(10))
            .expect("set dir mtime");
        drop(d);

        assert!(
            cache.get(key, 1, &fs).is_none(),
            "a witnessed directory's mtime change must invalidate the cache"
        );
    }

    #[test]
    fn count_pages_basic() {
        // 5 lines of 7 chars each (+ newline = 8 per line_len).
        // Budget of 20: first two fit (8+8=16), third pushes to 24 > 20 → break.
        let output = "line 1\nline 2\nline 3\nline 4\nline 5\n";
        let pages = count_pages(output, 20);
        assert!(pages > 1, "should be multiple pages: {pages}");
    }

    #[test]
    fn count_pages_empty() {
        assert_eq!(count_pages("", 100), 0);
    }

    #[test]
    fn cache_key_differs_for_different_params() {
        let params_a = GrepCacheParams {
            pattern: "foo",
            paths: &[],
            exclude: None,
            include_gitignored: false,
            include_hidden: false,
            cwd: None,
            budget: 4000,
        };
        let params_b = GrepCacheParams {
            pattern: "bar",
            paths: &[],
            exclude: None,
            include_gitignored: false,
            include_hidden: false,
            cwd: None,
            budget: 4000,
        };
        assert_ne!(cache_key(&params_a), cache_key(&params_b));
    }

    #[test]
    fn cache_key_same_for_same_params() {
        let params_a = GrepCacheParams {
            pattern: "foo",
            paths: &[PathBuf::from("src/main.rs")],
            exclude: None,
            include_gitignored: false,
            include_hidden: true,
            cwd: Some(Path::new("/home/user/project")),
            budget: 4000,
        };
        let params_b = GrepCacheParams {
            pattern: "foo",
            paths: &[PathBuf::from("src/main.rs")],
            exclude: None,
            include_gitignored: false,
            include_hidden: true,
            cwd: Some(Path::new("/home/user/project")),
            budget: 4000,
        };
        assert_eq!(cache_key(&params_a), cache_key(&params_b));
    }
}
