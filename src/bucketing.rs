// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 Mark Wells <contact@markwells.dev>

//! Two-stage bucketing for grep tier 3 and glob tier 3 output.
//!
//! Stage 1 ([`bucket_separators`]) groups strings by longest common prefix at
//! separator boundaries (`_`, `-`, `.`, space). Stage 2 ([`bucket_trie`])
//! applies trie-based radix compaction when separator structure is absent.
//!
//! The main entry point [`bucket`] runs stage 1, then optionally falls back to
//! stage 2 when `trie_fallback` is `true` and stage 1 produces only a single
//! catch-all bucket.

use std::collections::BTreeMap;

/// Minimum number of buckets the algorithm targets. Merge stops at this floor,
/// and the trie expands until it reaches it. The actual count may be lower when
/// the input has fewer distinct entries.
const MIN_BUCKETS: usize = 10;

/// A bucket produced by the bucketing algorithm.
pub struct Bucket {
    /// The prefix pattern (e.g., `"test_mcp_*"`).
    pub pattern: String,
    /// Number of entries in this bucket.
    pub count: usize,
    /// If expanded: entries with detail. If collapsed: `None`.
    pub entries: Option<Vec<BucketEntry>>,
}

/// A single entry within an expanded bucket.
pub struct BucketEntry {
    /// The full string (filename, matched text, etc.).
    pub value: String,
    /// Opaque context carried with this entry.
    pub context: Option<String>,
}

/// Main entry point.
///
/// Runs stage 1 (separator-aware). If `trie_fallback` is `true` and stage 1
/// produces a single catch-all `"*"` bucket, runs stage 2 (trie). Glob calls
/// with `trie_fallback = false`. Grep calls with `trie_fallback = true`.
#[must_use]
pub fn bucket(input: &[BucketEntry], budget: usize, trie_fallback: bool) -> Vec<Bucket> {
    if input.is_empty() {
        return Vec::new();
    }

    let mut buckets = bucket_separators(input, budget);

    // If separator bucketing produced a single catch-all and trie fallback is
    // requested, try the trie.
    if trie_fallback && buckets.len() == 1 && buckets[0].pattern == "*" {
        buckets = bucket_trie(input, budget);
    }

    collapse_to_budget(&mut buckets, budget);
    buckets
}

/// Estimate the rendered character cost of a bucket slice.
#[must_use]
pub fn rendered_size(buckets: &[Bucket]) -> usize {
    buckets.iter().map(bucket_rendered_size).sum()
}

// ---------------------------------------------------------------------------
// Stage 1: separator-aware bucketing
// ---------------------------------------------------------------------------

const SEPARATORS: &[char] = &['_', '-', '.', ' '];

/// Separator-aware bucketing.
///
/// Groups input strings by longest common prefix at separator boundaries,
/// then collapses to fit within `budget`.
#[must_use]
pub fn bucket_separators(input: &[BucketEntry], budget: usize) -> Vec<Bucket> {
    if input.is_empty() {
        return Vec::new();
    }

    // Find separator positions for each value and group by longest common
    // prefix at a separator boundary.
    let groups = group_by_separator_prefix(input);

    // If grouping is degenerate — single group holding everything, or every
    // entry in its own group (no shared separator prefix) — return a single
    // catch-all bucket.
    let has_useful_grouping = groups.values().any(|indices| indices.len() > 1);
    if !has_useful_grouping && input.len() > 1 {
        return vec![make_catch_all(input)];
    }

    let mut buckets = groups_to_buckets(&groups, input);

    collapse_to_budget(&mut buckets, budget);
    buckets
}

/// Group input entries by the longest common prefix up to a separator boundary.
///
/// Sort-based O(n log n): sort entries, compare adjacent pairs to find the
/// longest shared separator-boundary prefix, then group by that prefix.
fn group_by_separator_prefix(input: &[BucketEntry]) -> BTreeMap<String, Vec<usize>> {
    // Build (value, original_index) pairs and sort by value.
    let mut sorted: Vec<(usize, &str)> = input
        .iter()
        .enumerate()
        .map(|(i, e)| (i, e.value.as_str()))
        .collect();
    sorted.sort_by(|a, b| a.1.cmp(b.1));

    // For each entry, find the longest separator-boundary prefix shared with
    // either sorted neighbor. In sorted order, shared prefixes cluster.
    let mut prefixes: Vec<(usize, String)> = Vec::with_capacity(sorted.len());
    for (pos, &(idx, value)) in sorted.iter().enumerate() {
        let left = if pos > 0 {
            Some(sorted[pos - 1].1)
        } else {
            None
        };
        let right = if pos + 1 < sorted.len() {
            Some(sorted[pos + 1].1)
        } else {
            None
        };
        let prefix = longest_neighbor_separator_prefix(value, left, right);
        prefixes.push((idx, prefix));
    }

    let mut prefix_map: BTreeMap<String, Vec<usize>> = BTreeMap::new();
    for (idx, prefix) in prefixes {
        prefix_map.entry(prefix).or_default().push(idx);
    }

    prefix_map
}

/// Find the longest separator-boundary prefix of `value` shared with either
/// sorted neighbor. Only needs to check left and right because sorted order
/// guarantees that the longest shared prefix is with an adjacent entry.
fn longest_neighbor_separator_prefix(
    value: &str,
    left: Option<&str>,
    right: Option<&str>,
) -> String {
    let sep_positions: Vec<usize> = value
        .char_indices()
        .filter(|(_, c)| SEPARATORS.contains(c))
        .map(|(i, _)| i)
        .collect();

    // Try from the deepest separator boundary backward.
    for &pos in sep_positions.iter().rev() {
        let candidate = &value[..=pos];
        let shared = left.is_some_and(|l| l.starts_with(candidate))
            || right.is_some_and(|r| r.starts_with(candidate));
        if shared {
            return candidate.to_owned();
        }
    }

    // No shared separator prefix — this entry stands alone.
    value.to_owned()
}

/// Convert grouped indices into `Bucket` values.
fn groups_to_buckets(groups: &BTreeMap<String, Vec<usize>>, input: &[BucketEntry]) -> Vec<Bucket> {
    let mut buckets = Vec::with_capacity(groups.len());

    for (prefix, indices) in groups {
        if indices.len() == 1 {
            // Single-entry group: show the full string, no wildcard.
            let entry = &input[indices[0]];
            buckets.push(Bucket {
                pattern: entry.value.clone(),
                count: 1,
                entries: Some(vec![BucketEntry {
                    value: entry.value.clone(),
                    context: entry.context.clone(),
                }]),
            });
        } else {
            let pattern = format!("{prefix}*");
            let entries: Vec<BucketEntry> = indices
                .iter()
                .map(|&i| BucketEntry {
                    value: input[i].value.clone(),
                    context: input[i].context.clone(),
                })
                .collect();
            let count = entries.len();
            buckets.push(Bucket {
                pattern,
                count,
                entries: Some(entries),
            });
        }
    }

    buckets
}

fn make_catch_all(input: &[BucketEntry]) -> Bucket {
    let entries: Vec<BucketEntry> = input
        .iter()
        .map(|e| BucketEntry {
            value: e.value.clone(),
            context: e.context.clone(),
        })
        .collect();
    Bucket {
        pattern: "*".to_owned(),
        count: entries.len(),
        entries: Some(entries),
    }
}

// ---------------------------------------------------------------------------
// Stage 2: trie-based radix compaction
// ---------------------------------------------------------------------------

/// Trie node for radix compaction.
struct TrieNode {
    children: BTreeMap<char, Self>,
    count: usize,
    terminal: bool,
}

impl TrieNode {
    const fn new() -> Self {
        Self {
            children: BTreeMap::new(),
            count: 0,
            terminal: false,
        }
    }

    fn insert(&mut self, s: &str) {
        self.count += 1;
        if let Some(c) = s.chars().next() {
            let rest = &s[c.len_utf8()..];
            self.children
                .entry(c)
                .or_insert_with(Self::new)
                .insert(rest);
        } else {
            self.terminal = true;
        }
    }
}

/// Trie-based radix compaction.
///
/// Fallback for strings with no separator structure.
#[must_use]
pub fn bucket_trie(input: &[BucketEntry], budget: usize) -> Vec<Bucket> {
    if input.is_empty() {
        return Vec::new();
    }
    if input.len() == 1 {
        return vec![Bucket {
            pattern: input[0].value.clone(),
            count: 1,
            entries: Some(vec![BucketEntry {
                value: input[0].value.clone(),
                context: input[0].context.clone(),
            }]),
        }];
    }

    let mut root = TrieNode::new();
    for entry in input {
        root.insert(&entry.value);
    }

    // Progressively expand the trie deeper until we reach the target bucket
    // count or exhaust the trie. The depth bound is checked before expansion
    // so the boundary (depth == max_depth) is the last useful expansion.
    let max_depth = input.iter().map(|e| e.value.len()).max().unwrap_or(0);
    let mut buckets: Vec<(String, usize)> = Vec::new();
    let target = MIN_BUCKETS.min(input.len());
    let mut depth = 1;
    while buckets.len() < target && depth <= max_depth {
        buckets.clear();
        force_expand_depth(&root, String::new(), &mut buckets, depth);
        depth += 1;
    }

    // Map back to full Bucket structs with entries.
    let mut result: Vec<Bucket> = Vec::with_capacity(buckets.len());
    for (pattern, count) in &buckets {
        let prefix = pattern.trim_end_matches('*');
        let entries: Vec<BucketEntry> = input
            .iter()
            .filter(|e| e.value.starts_with(prefix))
            .map(|e| BucketEntry {
                value: e.value.clone(),
                context: e.context.clone(),
            })
            .collect();
        result.push(Bucket {
            pattern: pattern.clone(),
            count: *count,
            entries: if entries.is_empty() {
                None
            } else {
                Some(entries)
            },
        });
    }

    collapse_to_budget(&mut result, budget);
    result
}

/// Force-expand to a given depth to ensure minimum bucket count.
fn force_expand_depth(
    node: &TrieNode,
    prefix: String,
    buckets: &mut Vec<(String, usize)>,
    remaining_depth: usize,
) {
    if remaining_depth == 0 || node.children.is_empty() {
        let pattern = if node.count == 1 && node.terminal && node.children.is_empty() {
            prefix
        } else {
            format!("{prefix}*")
        };
        buckets.push((pattern, node.count));
        return;
    }

    for (&c, child) in &node.children {
        let mut child_prefix = prefix.clone();
        child_prefix.push(c);
        force_expand_depth(child, child_prefix, buckets, remaining_depth - 1);
    }
    if node.terminal {
        buckets.push((prefix, 1));
    }
}

// ---------------------------------------------------------------------------
// Collapse / degrade
// ---------------------------------------------------------------------------

/// When total rendered output exceeds budget, first collapse expanded buckets
/// (smallest first) to bare handles, then merge bare handles by widening
/// prefixes until output fits.
fn collapse_to_budget(buckets: &mut Vec<Bucket>, budget: usize) {
    // Phase 1: collapse expanded buckets to bare handles.
    while rendered_size(buckets) > budget {
        let smallest = buckets
            .iter()
            .enumerate()
            .filter(|(_, b)| b.entries.is_some() && b.count > 1)
            .min_by_key(|(_, b)| b.count)
            .map(|(i, _)| i);

        if let Some(i) = smallest {
            buckets[i].entries = None;
        } else {
            break;
        }
    }

    // Phase 2: merge bare handles by widening prefixes. Stop at MIN_BUCKETS
    // so the agent always has enough handles to navigate.
    while rendered_size(buckets) > budget && buckets.len() > MIN_BUCKETS {
        merge_closest_pair(buckets);
    }
}

/// Find the pair of adjacent buckets (sorted by pattern) with the longest
/// shared prefix and merge them into one wider bucket.
fn merge_closest_pair(buckets: &mut Vec<Bucket>) {
    if buckets.len() < 2 {
        return;
    }

    // Find the adjacent pair with the longest shared prefix.
    let mut best_idx = 0;
    let mut best_shared = 0;
    for i in 0..buckets.len() - 1 {
        let a = buckets[i].pattern.trim_end_matches('*');
        let b = buckets[i + 1].pattern.trim_end_matches('*');
        let shared = shared_prefix_len(a, b);
        if shared > best_shared {
            best_shared = shared;
            best_idx = i;
        }
    }

    // Merge buckets[best_idx] and buckets[best_idx + 1].
    let a_prefix = buckets[best_idx].pattern.trim_end_matches('*');
    let b_prefix = buckets[best_idx + 1].pattern.trim_end_matches('*');
    let common = &a_prefix[..shared_prefix_len(a_prefix, b_prefix)];
    let merged_pattern = if common.is_empty() {
        "*".to_owned()
    } else {
        format!("{common}*")
    };
    let merged_count = buckets[best_idx].count + buckets[best_idx + 1].count;

    buckets[best_idx] = Bucket {
        pattern: merged_pattern,
        count: merged_count,
        entries: None,
    };
    buckets.remove(best_idx + 1);
}

/// Length of the shared byte prefix between two strings.
fn shared_prefix_len(a: &str, b: &str) -> usize {
    a.bytes().zip(b.bytes()).take_while(|(x, y)| x == y).count()
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Rendered size of a single bucket.
fn bucket_rendered_size(b: &Bucket) -> usize {
    match &b.entries {
        None => {
            // Bare handle: "pattern (count)\n"
            b.pattern.len() + count_digits(b.count) + 4
        }
        Some(entries) if b.count == 1 => {
            // Single entry shown as full string.
            entries.first().map_or(0, |e| {
                e.value.len() + e.context.as_ref().map_or(0, |c| c.len() + 2) + 1
            })
        }
        Some(entries) => {
            // Pattern header + expanded entries.
            let header = b.pattern.len() + count_digits(b.count) + 4;
            let body: usize = entries
                .iter()
                .map(|e| {
                    // tab + value + optional context + newline
                    e.value.len() + e.context.as_ref().map_or(0, |c| c.len() + 2) + 2
                })
                .sum();
            header + body
        }
    }
}

/// Number of decimal digits in a `usize`.
const fn count_digits(n: usize) -> usize {
    if n == 0 {
        return 1;
    }
    let mut digits = 0;
    let mut val = n;
    while val > 0 {
        digits += 1;
        val /= 10;
    }
    digits
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Helper to make `BucketEntry` values without context.
    fn entries(values: &[&str]) -> Vec<BucketEntry> {
        values
            .iter()
            .map(|v| BucketEntry {
                value: (*v).to_owned(),
                context: None,
            })
            .collect()
    }

    #[test]
    fn test_separator_basic() {
        let input = entries(&["test_a_1", "test_a_2", "test_b_1", "test_b_2"]);
        let buckets = bucket_separators(&input, 10_000);
        assert!(
            buckets.len() >= 2,
            "expected at least 2 buckets, got {}: {:?}",
            buckets.len(),
            buckets.iter().map(|b| &b.pattern).collect::<Vec<_>>()
        );
        let patterns: Vec<&str> = buckets.iter().map(|b| b.pattern.as_str()).collect();
        assert!(
            patterns.iter().any(|p| p.contains("test_a_")),
            "missing test_a_* bucket: {patterns:?}"
        );
        assert!(
            patterns.iter().any(|p| p.contains("test_b_")),
            "missing test_b_* bucket: {patterns:?}"
        );
        for b in &buckets {
            if b.pattern.contains("test_a_") || b.pattern.contains("test_b_") {
                assert_eq!(b.count, 2, "bucket {} should have 2 entries", b.pattern);
            }
        }
    }

    #[test]
    fn test_separator_mixed_delimiters() {
        let input = entries(&[
            "config-dev-a",
            "config-dev-b",
            "config-prod-a",
            "config-prod-b",
            "data_file_1",
            "data_file_2",
        ]);
        let buckets = bucket_separators(&input, 10_000);
        assert!(
            buckets.len() >= 2,
            "expected at least 2 buckets, got {}: {:?}",
            buckets.len(),
            buckets.iter().map(|b| &b.pattern).collect::<Vec<_>>()
        );
    }

    #[test]
    fn test_separator_dot() {
        let input = entries(&["config.dev.json", "config.prod.json", "data.json"]);
        let buckets = bucket_separators(&input, 10_000);
        assert!(
            buckets.len() >= 2,
            "expected at least 2 buckets for dot-separated input, got {}: {:?}",
            buckets.len(),
            buckets.iter().map(|b| &b.pattern).collect::<Vec<_>>()
        );
        let has_config = buckets.iter().any(|b| b.pattern.starts_with("config."));
        assert!(
            has_config,
            "expected a config.* bucket: {:?}",
            buckets.iter().map(|b| &b.pattern).collect::<Vec<_>>()
        );
    }

    #[test]
    fn test_trie_basic() {
        let input = entries(&[
            "alpha",
            "alphabeta",
            "alphacat",
            "bravo",
            "bravocat",
            "charlie",
        ]);
        let buckets = bucket_trie(&input, 10_000);
        assert!(
            buckets.len() >= 2,
            "trie should produce at least 2 buckets, got {}: {:?}",
            buckets.len(),
            buckets.iter().map(|b| &b.pattern).collect::<Vec<_>>()
        );
    }

    #[test]
    fn test_trie_even_prefixes() {
        // Two clear prefixes: "aaa*" (3) and "bbb*" (3) — perfectly even.
        let input = entries(&["aaa1", "aaa2", "aaa3", "bbb1", "bbb2", "bbb3"]);
        let buckets = bucket_trie(&input, 10_000);
        assert_eq!(
            buckets.len(),
            6,
            "6 unique entries should each get their own bucket, got {}: {:?}",
            buckets.len(),
            buckets.iter().map(|b| &b.pattern).collect::<Vec<_>>()
        );
    }

    #[test]
    fn test_trie_skewed_prefixes() {
        // One huge cluster, one tiny — trie must still split into enough buckets.
        let mut values: Vec<String> = (0..50).map(|i| format!("a{i:03}")).collect();
        values.push("b1".to_owned());
        let input: Vec<BucketEntry> = values
            .iter()
            .map(|v| BucketEntry {
                value: v.clone(),
                context: None,
            })
            .collect();
        let buckets = bucket_trie(&input, 10_000);
        assert!(
            buckets.len() >= MIN_BUCKETS,
            "expected at least {MIN_BUCKETS} buckets, got {}: {:?}",
            buckets.len(),
            buckets
                .iter()
                .map(|b| (&b.pattern, b.count))
                .collect::<Vec<_>>()
        );
        // "b1" must appear as its own bucket, not merged into the "a*" cluster.
        assert!(
            buckets.iter().any(|b| b.pattern == "b1"),
            "expected standalone 'b1' bucket: {:?}",
            buckets.iter().map(|b| &b.pattern).collect::<Vec<_>>()
        );
    }

    #[test]
    fn test_budget_collapse() {
        // 100 strings, small budget. All multi-entry buckets should be bare
        // handles.
        let owned: Vec<String> = (0..100).map(|i| format!("item_{i}")).collect();
        let input: Vec<BucketEntry> = owned
            .iter()
            .map(|v| BucketEntry {
                value: v.clone(),
                context: None,
            })
            .collect();
        let buckets = bucket(&input, 50, false);
        for b in &buckets {
            if b.count > 1 {
                assert!(
                    b.entries.is_none(),
                    "bucket {} ({}) should be a bare handle at budget=50",
                    b.pattern,
                    b.count
                );
            }
        }
        // All entries share "item_" separator prefix → single bucket collapsed
        // to bare handle.
        assert_eq!(buckets.len(), 1);
        assert_eq!(buckets[0].pattern, "item_*");
        assert_eq!(buckets[0].count, 100);
        assert!(
            rendered_size(&buckets) <= 50,
            "rendered size {} should not exceed budget 50",
            rendered_size(&buckets)
        );
    }

    #[test]
    fn test_budget_expand() {
        let input = entries(&["alpha", "beta", "gamma"]);
        let buckets = bucket(&input, 10_000, true);
        let has_expanded = buckets.iter().any(|b| b.entries.is_some());
        assert!(
            has_expanded,
            "with large budget, some buckets should be expanded"
        );
    }

    #[test]
    fn test_minimum_bucket_count() {
        // Adversarial: all strings same prefix, no separators.
        // With 20 entries the trie should produce at least MIN_BUCKETS.
        let owned: Vec<String> = (0..20).map(|i| format!("x{i:02}")).collect();
        let input: Vec<BucketEntry> = owned
            .iter()
            .map(|v| BucketEntry {
                value: v.clone(),
                context: None,
            })
            .collect();
        let buckets = bucket_trie(&input, 10_000);
        assert!(
            buckets.len() >= MIN_BUCKETS,
            "trie must produce at least {MIN_BUCKETS} buckets, got {}: {:?}",
            buckets.len(),
            buckets
                .iter()
                .map(|b| (&b.pattern, b.count))
                .collect::<Vec<_>>()
        );
    }

    #[test]
    fn test_minimum_bucket_count_small_input() {
        // With fewer entries than MIN_BUCKETS, produce as many as we have.
        let owned: Vec<String> = (0..5).map(|i| format!("x{i}")).collect();
        let input: Vec<BucketEntry> = owned
            .iter()
            .map(|v| BucketEntry {
                value: v.clone(),
                context: None,
            })
            .collect();
        let buckets = bucket_trie(&input, 10_000);
        assert!(
            buckets.len() >= 2,
            "trie should still split small input, got {}: {:?}",
            buckets.len(),
            buckets
                .iter()
                .map(|b| (&b.pattern, b.count))
                .collect::<Vec<_>>()
        );
    }

    #[test]
    fn test_no_trie_fallback() {
        let input = entries(&["alpha", "alphabeta", "bravo"]);
        let buckets = bucket(&input, 10_000, false);
        assert_eq!(
            buckets.len(),
            1,
            "with trie_fallback=false and no separators, expected 1 catch-all, got {:?}",
            buckets.iter().map(|b| &b.pattern).collect::<Vec<_>>()
        );
        assert_eq!(buckets[0].pattern, "*");
    }

    #[test]
    fn test_trie_fallback() {
        let input = entries(&[
            "alpha",
            "alphabeta",
            "alphacat",
            "bravo",
            "bravocat",
            "charlie",
        ]);
        let buckets = bucket(&input, 10_000, true);
        assert!(
            buckets.len() >= 2,
            "with trie_fallback=true, should produce useful buckets, got {:?}",
            buckets.iter().map(|b| &b.pattern).collect::<Vec<_>>()
        );
    }

    #[test]
    fn test_rendered_size() {
        let buckets = vec![
            Bucket {
                pattern: "test_*".to_owned(),
                count: 5,
                entries: None,
            },
            Bucket {
                pattern: "data.json".to_owned(),
                count: 1,
                entries: Some(vec![BucketEntry {
                    value: "data.json".to_owned(),
                    context: None,
                }]),
            },
        ];
        // Bare handle: "test_*" (6) + count_digits(5) (1) + 4 = 11
        // Single entry: "data.json" (9) + no context (0) + 1 = 10
        assert_eq!(rendered_size(&buckets), 21);
    }

    #[test]
    fn test_adversarial_long_prefixes() {
        let long_prefix: String = "a".repeat(1500);
        let owned: Vec<String> = (0..10).map(|i| format!("{long_prefix}{i}")).collect();
        let input: Vec<BucketEntry> = owned
            .iter()
            .map(|v| BucketEntry {
                value: v.clone(),
                context: None,
            })
            .collect();
        let buckets = bucket(&input, 50_000, true);
        assert!(
            buckets.len() >= 2,
            "adversarial long prefixes should produce at least 2 buckets, got {}: {:?}",
            buckets.len(),
            buckets
                .iter()
                .map(|b| (&b.pattern, b.count))
                .collect::<Vec<_>>()
        );
    }

    #[test]
    fn test_bare_handle_merging() {
        // Many separator-based buckets — budget too small for expanded
        // entries but large enough that merging can reduce bare handles.
        let owned: Vec<String> = ('a'..='z')
            .flat_map(|c| (0..5).map(move |i| format!("grp_{c}_{i}")))
            .collect();
        let input: Vec<BucketEntry> = owned
            .iter()
            .map(|v| BucketEntry {
                value: v.clone(),
                context: None,
            })
            .collect();
        // Budget allows bare handles but not expanded entries.
        let buckets = bucket(&input, 200, false);
        // All multi-entry buckets should be collapsed.
        for b in &buckets {
            if b.count > 1 {
                assert!(
                    b.entries.is_none(),
                    "bucket {} should be a bare handle",
                    b.pattern
                );
            }
        }
        // Should have merged some but stayed above MIN_BUCKETS floor.
        assert!(
            buckets.len() >= MIN_BUCKETS,
            "merging should stop at MIN_BUCKETS floor, got {}: {:?}",
            buckets.len(),
            buckets
                .iter()
                .map(|b| (&b.pattern, b.count))
                .collect::<Vec<_>>()
        );
        // After merging, rendered size should not exceed the budget.
        assert!(
            rendered_size(&buckets) <= 200,
            "rendered size {} should not exceed budget 200",
            rendered_size(&buckets)
        );
    }

    #[test]
    fn test_trie_stops_at_target() {
        // 11 entries where force_expand at depth 2 gives exactly MIN_BUCKETS
        // buckets: "a0" has 2 children (x,y) while "a1"-"a9" have 1 each.
        // Depth 3 would give 11 buckets. The algorithm must stop at the target.
        let input = entries(&[
            "a0x", "a0y", "a1x", "a2x", "a3x", "a4x", "a5x", "a6x", "a7x", "a8x", "a9x",
        ]);
        let buckets = bucket_trie(&input, 10_000);
        assert_eq!(
            buckets.len(),
            MIN_BUCKETS,
            "should stop at MIN_BUCKETS={MIN_BUCKETS}, got {}: {:?}",
            buckets.len(),
            buckets
                .iter()
                .map(|b| (&b.pattern, b.count))
                .collect::<Vec<_>>()
        );
    }

    #[test]
    fn test_trie_needs_max_depth() {
        // "a" (length 1) and "ab" (length 2). The trie needs expansion at
        // depth == max_entry_length to separate them into individual buckets.
        let input = entries(&["a", "ab"]);
        let buckets = bucket_trie(&input, 10_000);
        assert_eq!(
            buckets.len(),
            2,
            "should produce 2 buckets at max depth, got {}: {:?}",
            buckets.len(),
            buckets
                .iter()
                .map(|b| (&b.pattern, b.count))
                .collect::<Vec<_>>()
        );
        assert!(
            buckets.iter().any(|b| b.pattern == "a"),
            "expected bare 'a' bucket: {:?}",
            buckets.iter().map(|b| &b.pattern).collect::<Vec<_>>()
        );
        assert!(
            buckets.iter().any(|b| b.pattern == "ab"),
            "expected bare 'ab' bucket: {:?}",
            buckets.iter().map(|b| &b.pattern).collect::<Vec<_>>()
        );
    }

    #[test]
    fn test_trie_exhausted() {
        // All identical entries — the trie can never produce enough buckets.
        let input: Vec<BucketEntry> = (0..5)
            .map(|_| BucketEntry {
                value: "ab".to_owned(),
                context: None,
            })
            .collect();
        let buckets = bucket_trie(&input, 10_000);
        assert_eq!(
            buckets.len(),
            1,
            "identical entries should produce 1 bucket"
        );
        assert_eq!(buckets[0].count, 5);
    }

    #[test]
    fn test_trie_wildcard_on_multi_entry_buckets() {
        // "ab" and "abc" share a prefix — the bucket at the 'b' node
        // must use a wildcard pattern ("ab*") even though 'b' is terminal.
        let input = entries(&["ab", "abc", "xy"]);
        let buckets = bucket_trie(&input, 10_000);
        for b in &buckets {
            if b.count > 1 {
                assert!(
                    b.pattern.ends_with('*'),
                    "multi-entry bucket should have wildcard pattern, got {:?} (count {})",
                    b.pattern,
                    b.count
                );
            }
        }
    }

    #[test]
    fn test_trie_non_terminal_prefix_uses_wildcard() {
        // 10 entries each with a unique 1-char prefix followed by a suffix.
        // At depth 1 the algorithm hits the target (MIN_BUCKETS). Each node
        // is count=1, terminal=false, children non-empty — the pattern must
        // be a wildcard ("a*") not a bare prefix ("a").
        let input = entries(&[
            "ax", "bx", "cx", "dx", "ex", "fx", "gx", "hx", "ix", "jx",
        ]);
        let buckets = bucket_trie(&input, 10_000);
        assert_eq!(buckets.len(), MIN_BUCKETS);
        for b in &buckets {
            assert!(
                b.pattern.ends_with('*'),
                "non-terminal prefix bucket should use wildcard, got {:?}",
                b.pattern
            );
        }
    }

    // ------------------------------------------------------------------
    // Helper function unit tests
    // ------------------------------------------------------------------

    #[test]
    fn test_count_digits() {
        assert_eq!(count_digits(0), 1);
        assert_eq!(count_digits(1), 1);
        assert_eq!(count_digits(9), 1);
        assert_eq!(count_digits(10), 2);
        assert_eq!(count_digits(99), 2);
        assert_eq!(count_digits(100), 3);
        assert_eq!(count_digits(1000), 4);
    }

    #[test]
    fn test_shared_prefix_len() {
        assert_eq!(shared_prefix_len("abc", "abd"), 2);
        assert_eq!(shared_prefix_len("abc", "xyz"), 0);
        assert_eq!(shared_prefix_len("abc", "abc"), 3);
        assert_eq!(shared_prefix_len("", "abc"), 0);
        assert_eq!(shared_prefix_len("abc", ""), 0);
    }

    #[test]
    fn test_bucket_rendered_size_with_context() {
        // Single entry with context exercises the count == 1 match guard
        // and the context size arithmetic.
        let single = Bucket {
            pattern: "file.rs".to_owned(),
            count: 1,
            entries: Some(vec![BucketEntry {
                value: "file.rs".to_owned(),
                context: Some("mod utils".to_owned()),
            }]),
        };
        // value (7) + context (9) + ": " (2) + newline (1) = 19
        assert_eq!(bucket_rendered_size(&single), 19);

        // Expanded multi-entry with mixed context.
        let expanded = Bucket {
            pattern: "grp_*".to_owned(),
            count: 2,
            entries: Some(vec![
                BucketEntry {
                    value: "grp_a".to_owned(),
                    context: Some("x".to_owned()),
                },
                BucketEntry {
                    value: "grp_b".to_owned(),
                    context: None,
                },
            ]),
        };
        // header: "grp_*" (5) + count_digits(2) (1) + 4 = 10
        // entry 1: "grp_a" (5) + "x" (1) + 2 + 2 = 10
        // entry 2: "grp_b" (5) + 0 + 2 = 7
        // total: 10 + 10 + 7 = 27
        assert_eq!(bucket_rendered_size(&expanded), 27);
    }

    // ------------------------------------------------------------------
    // merge_closest_pair unit tests
    // ------------------------------------------------------------------

    #[test]
    fn test_merge_closest_pair_basic() {
        let mut buckets = vec![
            Bucket {
                pattern: "abc*".to_owned(),
                count: 3,
                entries: None,
            },
            Bucket {
                pattern: "abd*".to_owned(),
                count: 2,
                entries: None,
            },
        ];
        merge_closest_pair(&mut buckets);
        assert_eq!(buckets.len(), 1);
        assert_eq!(buckets[0].pattern, "ab*");
        assert_eq!(buckets[0].count, 5);
    }

    #[test]
    fn test_merge_selects_longest_shared_prefix() {
        let mut buckets = vec![
            Bucket {
                pattern: "aaa*".to_owned(),
                count: 1,
                entries: None,
            },
            Bucket {
                pattern: "bbb*".to_owned(),
                count: 1,
                entries: None,
            },
            Bucket {
                pattern: "bbc*".to_owned(),
                count: 1,
                entries: None,
            },
        ];
        merge_closest_pair(&mut buckets);
        // "bbb" and "bbc" share "bb" (2 chars) > "aaa"/"bbb" share "" (0 chars).
        assert_eq!(buckets.len(), 2);
        assert_eq!(buckets[0].pattern, "aaa*");
        assert_eq!(buckets[1].pattern, "bb*");
        assert_eq!(buckets[1].count, 2);
    }

    // ------------------------------------------------------------------
    // collapse_to_budget boundary tests
    // ------------------------------------------------------------------

    #[test]
    fn test_collapse_preserves_at_budget() {
        // When rendered_size == budget, no collapse should happen.
        let mut buckets = vec![Bucket {
            pattern: "a*".to_owned(),
            count: 2,
            entries: Some(vec![
                BucketEntry {
                    value: "a1".to_owned(),
                    context: None,
                },
                BucketEntry {
                    value: "a2".to_owned(),
                    context: None,
                },
            ]),
        }];
        // header (2+1+4=7) + entry (2+2=4) + entry (2+2=4) = 15
        let budget = rendered_size(&buckets);
        assert_eq!(budget, 15);
        collapse_to_budget(&mut buckets, budget);
        assert!(
            buckets[0].entries.is_some(),
            "should not collapse when rendered_size equals budget"
        );
    }

    #[test]
    fn test_collapse_preserves_single_entry_buckets() {
        let mut buckets = vec![
            Bucket {
                pattern: "only.rs".to_owned(),
                count: 1,
                entries: Some(vec![BucketEntry {
                    value: "only.rs".to_owned(),
                    context: None,
                }]),
            },
            Bucket {
                pattern: "test_*".to_owned(),
                count: 3,
                entries: Some(vec![
                    BucketEntry {
                        value: "test_a".to_owned(),
                        context: None,
                    },
                    BucketEntry {
                        value: "test_b".to_owned(),
                        context: None,
                    },
                    BucketEntry {
                        value: "test_c".to_owned(),
                        context: None,
                    },
                ]),
            },
        ];
        // Force collapse of test_* but not tight enough to merge.
        collapse_to_budget(&mut buckets, 20);
        assert!(
            buckets
                .iter()
                .any(|b| b.count == 1 && b.entries.is_some()),
            "single-entry bucket should not be collapsed"
        );
    }

    #[test]
    fn test_collapse_phase2_preserves_at_budget() {
        // All bare handles, total rendered_size == budget, > MIN_BUCKETS count.
        // Phase 2 should not merge.
        let mut buckets: Vec<Bucket> = (0..12)
            .map(|i| Bucket {
                pattern: format!("{i:02}*"),
                count: 2,
                entries: None,
            })
            .collect();
        let budget = rendered_size(&buckets);
        collapse_to_budget(&mut buckets, budget);
        assert_eq!(
            buckets.len(),
            12,
            "should not merge when rendered_size equals budget"
        );
    }

    #[test]
    fn test_collapse_respects_min_buckets_floor() {
        // Many bare handles with impossibly tight budget — merging must stop
        // at MIN_BUCKETS even though the budget is never satisfied.
        let mut buckets: Vec<Bucket> = ('a'..='z')
            .map(|c| Bucket {
                pattern: format!("{c}*"),
                count: 2,
                entries: None,
            })
            .collect();
        collapse_to_budget(&mut buckets, 1);
        assert_eq!(
            buckets.len(),
            MIN_BUCKETS,
            "merging should stop at MIN_BUCKETS floor"
        );
    }
}
