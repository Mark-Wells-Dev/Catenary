// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 Mark Wells <contact@markwells.dev>

use std::fmt::Write;

/// Formats the page header: `[page N/M]\n\n`.
pub(super) fn format_page_header(page: usize, total: usize) -> String {
    format!("[page {page}/{total}]\n\n")
}

/// Splits full output into pages and returns the requested page with a header.
///
/// Pages are split at line boundaries so no line is broken mid-way.
/// The budget is the maximum character count per page (excluding the header).
///
/// Edge behavior:
/// - Empty input returns `[page 1/1]\n\n`.
/// - Page 0 clamps to page 1.
/// - Pages beyond the last clamp to the last page.
pub(super) fn paginate(full: &str, budget: usize, page: usize) -> String {
    let lines: Vec<&str> = full.lines().collect();
    if lines.is_empty() {
        return format_page_header(1, 1);
    }

    // Build pages by accumulating lines until budget is hit.
    let mut pages: Vec<(usize, usize)> = Vec::new(); // (start_line, end_line) exclusive
    let mut start = 0;
    let mut current_len = 0;

    for (i, line) in lines.iter().enumerate() {
        let line_len = line.len() + 1; // +1 for newline
        if current_len > 0 && current_len + line_len > budget {
            pages.push((start, i));
            start = i;
            current_len = 0;
        }
        current_len += line_len;
    }
    // Final page — `start` is always a valid index here because
    // empty input is handled by the early return above.
    pages.push((start, lines.len()));

    let total = pages.len();
    // `pages` is non-empty (at least the final page was pushed) and
    // `idx` is clamped to `0..total`, so it's always a valid index.
    let idx = (page.max(1) - 1).min(total - 1);

    let (s, e) = pages[idx];
    let mut out = format_page_header(idx + 1, total);
    for &line in &lines[s..e] {
        let _ = writeln!(out, "{line}");
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_paginate_single_page() {
        let content = "line 1\nline 2\nline 3\n";
        let result = paginate(content, 5000, 1);
        assert!(
            result.starts_with("[page 1/1]"),
            "single-page result should show [page 1/1]: {result}"
        );
        assert!(
            result.contains("line 1"),
            "should contain content: {result}"
        );
    }

    #[test]
    fn test_paginate_multi_page() {
        // Each line is ~7 chars. Budget of 20 should give ~3 lines per page.
        let content = "line 1\nline 2\nline 3\nline 4\nline 5\nline 6\n";
        let result = paginate(content, 20, 1);
        assert!(
            result.contains("[page 1/"),
            "should have page header: {result}"
        );
        // Page 2 should have different content.
        let page2 = paginate(content, 20, 2);
        assert!(
            page2.contains("[page 2/"),
            "page 2 should have page header: {page2}"
        );
    }

    #[test]
    fn test_paginate_empty_input() {
        let result = paginate("", 100, 1);
        assert_eq!(
            result, "[page 1/1]\n\n",
            "empty input should give 1 empty page"
        );
    }

    #[test]
    fn test_paginate_exact_page_split() {
        // 6 lines, each "line N" = 6 chars, line_len = 6+1 = 7.
        // Budget of 21 fits exactly 3 lines (7+7+7 = 21). Fourth line
        // pushes to 28 > 21 → page break. Result: 2 pages of 3 lines.
        let content = "line 1\nline 2\nline 3\nline 4\nline 5\nline 6\n";

        let page1 = paginate(content, 21, 1);
        assert!(
            page1.starts_with("[page 1/2]"),
            "should be 2 pages: {page1:?}"
        );
        assert!(page1.contains("line 1\n"), "page 1 has line 1: {page1:?}");
        assert!(page1.contains("line 3\n"), "page 1 has line 3: {page1:?}");
        assert!(
            !page1.contains("line 4"),
            "page 1 should not have line 4: {page1:?}"
        );

        let page2 = paginate(content, 21, 2);
        assert!(page2.starts_with("[page 2/2]"), "page 2 header: {page2:?}");
        assert!(page2.contains("line 4\n"), "page 2 has line 4: {page2:?}");
        assert!(page2.contains("line 6\n"), "page 2 has line 6: {page2:?}");
        assert!(
            !page2.contains("line 3"),
            "page 2 should not have line 3: {page2:?}"
        );
    }

    #[test]
    fn test_paginate_single_line_exceeds_budget() {
        // A line longer than the budget should still appear on a page
        // (the `current_len > 0` guard prevents breaking an empty page).
        let content = "abcdefghij\nxy\n";
        // Budget of 5: "abcdefghij" has line_len=11, exceeds budget.
        // Without the `> 0` guard, an empty first page would be created.
        let page1 = paginate(content, 5, 1);
        assert!(
            page1.starts_with("[page 1/2]"),
            "should be 2 pages: {page1:?}"
        );
        assert!(
            page1.contains("abcdefghij\n"),
            "oversized line should be on page 1: {page1:?}"
        );
        assert!(
            !page1.contains("xy"),
            "page 1 should not contain second line: {page1:?}"
        );
    }

    #[test]
    fn test_paginate_budget_boundary_exact_fit() {
        // Two lines: "ab" (len 2) + newline = 3 chars each.
        let content = "ab\ncd\n";
        // Budget of 6 fits both lines exactly (3 + 3 = 6).
        let result = paginate(content, 6, 1);
        assert_eq!(
            result, "[page 1/1]\n\nab\ncd\n",
            "both lines should fit in one page"
        );

        // Budget of 5: first line takes 3, second would push to 6 > 5 → page break.
        let result = paginate(content, 5, 1);
        assert!(
            result.starts_with("[page 1/2]"),
            "budget 5 should split into 2 pages: {result:?}"
        );
        assert!(
            result.contains("ab\n"),
            "page 1 should contain first line: {result:?}"
        );
        assert!(
            !result.contains("cd"),
            "page 1 should not contain second line: {result:?}"
        );

        let page2 = paginate(content, 5, 2);
        assert!(page2.starts_with("[page 2/2]"), "page 2 header: {page2:?}");
        assert!(
            page2.contains("cd\n"),
            "page 2 should contain second line: {page2:?}"
        );
    }

    #[test]
    fn test_paginate_page_zero_clamps_to_one() {
        let content = "line 1\nline 2\n";
        let result = paginate(content, 5000, 0);
        assert!(
            result.starts_with("[page 1/1]"),
            "page 0 should clamp to page 1: {result:?}"
        );
    }

    #[test]
    fn test_paginate_beyond_last_clamps() {
        let content = "line 1\nline 2\n";
        let result = paginate(content, 5000, 99);
        // Should clamp to last page and show content.
        assert!(
            result.starts_with("[page 1/1]"),
            "beyond-last should clamp to last page: {result:?}"
        );
        assert!(result.contains("line 1"), "should show content: {result:?}");
        assert!(result.contains("line 2"), "should show content: {result:?}");
    }
}
