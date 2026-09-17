//! `edit` tool — port of `tool/edit.ts` (spec M4.3): the fuzzy replacer
//! chain, `trimDiff`, and the tool itself.
//!
//! The chain order and per-replacer semantics are binding (edit.ts:694-704):
//! each replacer lazily yields candidate `search` strings found in
//! `content`; the first *usable* candidate wins. Non-unique candidates are
//! skipped and iteration continues.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, OnceLock};

use serde::Deserialize;
use serde_json::{json, Value};

use crate::format::Formatter;
use crate::tool::bom;
use crate::tool::def::{
    define, Agents, AskRequest, BoxFuture, ExecuteResult, InstanceContext, MetadataInput,
    ToolCtxRef, ToolDef,
};
use crate::tool::diff::{create_two_files_patch, diff_line_counts};
use crate::tool::error::ToolError;
use crate::tool::external_directory::{assert_external_directory, ExternalOptions};
use crate::tool::ripgrep::{ts_relative, ts_resolve};
use crate::tool::truncate::Truncate;

// ---------------------------------------------------------------------------
// Seams shared by the edit + write tools.
// ---------------------------------------------------------------------------

/// The `LSP.Service` surface edit/write consume (lsp/lsp.ts:123-124). M4
/// provides the seam; the LSP client is M7.
pub trait Lsp: Send + Sync {
    /// `touchFile(input, "document")` — document warm-up.
    fn touch_file<'a>(&'a self, file: &'a str) -> BoxFuture<'a, ()>;
    /// `Record<file, LSPClient.Diagnostic[]>` — opaque JSON in M4.
    fn diagnostics<'a>(&'a self) -> BoxFuture<'a, Value>;
}

/// M4 default: no LSP server attached.
pub struct NoopLsp;

impl Lsp for NoopLsp {
    fn touch_file<'a>(&'a self, _file: &'a str) -> BoxFuture<'a, ()> {
        Box::pin(async {})
    }

    fn diagnostics<'a>(&'a self) -> BoxFuture<'a, Value> {
        Box::pin(async { json!({}) })
    }
}

/// `MAX_PER_FILE` (lsp/diagnostic.ts:3).
pub const MAX_PER_FILE: usize = 20;

/// `LSP.Diagnostic.report` (lsp/diagnostic.ts:20-27). Returns `""` when
/// there are no severity-1 errors (the TS falsy-empty contract).
pub fn diagnostic_report(file: &str, issues: &Value) -> String {
    let empty: Vec<Value> = Vec::new();
    let issues = issues.as_array().unwrap_or(&empty);
    let errors: Vec<&Value> = issues
        .iter()
        .filter(|item| {
            item.get("severity")
                .and_then(|s| s.as_f64())
                .is_some_and(|s| s == 1.0)
        })
        .collect();
    if errors.is_empty() {
        return String::new();
    }
    let lines: Vec<String> = errors
        .iter()
        .take(MAX_PER_FILE)
        .map(|item| diagnostic_pretty(item))
        .collect();
    let more = errors.len() as i64 - MAX_PER_FILE as i64;
    let suffix = if more > 0 {
        format!("\n... and {more} more")
    } else {
        String::new()
    };
    format!(
        "<diagnostics file=\"{file}\">\n{}{suffix}\n</diagnostics>",
        lines.join("\n")
    )
}

/// `LSP.Diagnostic.pretty` (lsp/diagnostic.ts:5-18).
fn diagnostic_pretty(diagnostic: &Value) -> String {
    let severity = diagnostic
        .get("severity")
        .and_then(|s| s.as_u64())
        .filter(|s| *s > 0)
        .unwrap_or(1);
    let severity = match severity {
        1 => "ERROR",
        2 => "WARN",
        3 => "INFO",
        4 => "HINT",
        _ => "undefined",
    };
    let line = diagnostic
        .pointer("/range/start/line")
        .and_then(|v| v.as_u64())
        .unwrap_or(0)
        + 1;
    let character = diagnostic
        .pointer("/range/start/character")
        .and_then(|v| v.as_u64())
        .unwrap_or(0)
        + 1;
    let message = diagnostic
        .get("message")
        .and_then(|m| m.as_str())
        .unwrap_or_default();
    format!("{severity} [{line}:{character}] {message}")
}

/// EventV2Bridge seam: `FileSystem.Event.Edited` and
/// `Watcher.Event.Updated` publications.
pub trait FileEvents: Send + Sync {
    fn edited<'a>(&'a self, file: &'a str) -> BoxFuture<'a, ()>;
    fn updated<'a>(&'a self, file: &'a str, event: &'a str) -> BoxFuture<'a, ()>;
}

/// M4 default: events go nowhere.
pub struct NoopFileEvents;

impl FileEvents for NoopFileEvents {
    fn edited<'a>(&'a self, _file: &'a str) -> BoxFuture<'a, ()> {
        Box::pin(async {})
    }

    fn updated<'a>(&'a self, _file: &'a str, _event: &'a str) -> BoxFuture<'a, ()> {
        Box::pin(async {})
    }
}

/// `Snapshot.FileDiff` — `{ file, patch, additions, deletions }`. The git
/// snapshot machinery behind it is M7.
#[derive(Debug, Clone, serde::Serialize, PartialEq, Eq)]
pub struct FileDiff {
    pub file: String,
    pub patch: String,
    pub additions: usize,
    pub deletions: usize,
}

async fn format_file(format: &Option<Arc<dyn Formatter>>, file: &str) -> bool {
    match format {
        Some(formatter) => formatter.file(file).await,
        None => false,
    }
}

async fn publish_edited(events: &Option<Arc<dyn FileEvents>>, file: &str) {
    if let Some(events) = events {
        events.edited(file).await;
    }
}

async fn publish_updated(events: &Option<Arc<dyn FileEvents>>, file: &str, event: &str) {
    if let Some(events) = events {
        events.updated(file, event).await;
    }
}

// ---------------------------------------------------------------------------
// Fuzzy replacer chain (edit.ts:217-659).
// ---------------------------------------------------------------------------

pub type ReplacerIter<'a> = Box<dyn Iterator<Item = String> + 'a>;
pub type Replacer = for<'a> fn(&'a str, &'a str) -> ReplacerIter<'a>;

/// Similarity thresholds for block anchor fallback matching (edit.ts:220-221).
const SINGLE_CANDIDATE_SIMILARITY_THRESHOLD: f64 = 0.65;
const MULTIPLE_CANDIDATES_SIMILARITY_THRESHOLD: f64 = 0.65;

/// Levenshtein distance over characters (edit.ts:226-242).
fn levenshtein(a: &str, b: &str) -> usize {
    if a.is_empty() || b.is_empty() {
        return a.chars().count().max(b.chars().count());
    }
    let b_chars: Vec<char> = b.chars().collect();
    let mut matrix: Vec<Vec<usize>> = Vec::with_capacity(a.chars().count() + 1);
    for i in 0..=a.chars().count() {
        matrix.push((0..=b_chars.len()).collect());
        if i > 0 {
            matrix[i][0] = i;
        }
    }
    for (i, a_char) in a.chars().enumerate() {
        for (j, b_char) in b_chars.iter().enumerate() {
            let cost = usize::from(a_char != *b_char);
            matrix[i + 1][j + 1] = (matrix[i][j + 1] + 1)
                .min(matrix[i + 1][j] + 1)
                .min(matrix[i][j] + cost);
        }
    }
    matrix[a.chars().count()][b_chars.len()]
}

/// `SimpleReplacer` (edit.ts:244-246): yields `find` (exact).
fn simple_replacer<'a>(_content: &'a str, find: &'a str) -> ReplacerIter<'a> {
    Box::new(std::iter::once(find.to_string()))
}

/// `LineTrimmedReplacer` (edit.ts:248-286): per-line trim() match; yields
/// the original content block.
fn line_trimmed_replacer<'a>(content: &'a str, find: &'a str) -> ReplacerIter<'a> {
    let original_lines: Vec<&str> = content.split('\n').collect();
    let mut search_lines: Vec<&str> = find.split('\n').collect();
    if search_lines.last() == Some(&"") {
        search_lines.pop();
    }
    let search_len = search_lines.len();
    let matched: Vec<usize> = if original_lines.len() >= search_len {
        (0..=original_lines.len() - search_len)
            .filter(|&i| {
                search_lines
                    .iter()
                    .enumerate()
                    .all(|(j, search)| original_lines[i + j].trim() == search.trim())
            })
            .collect()
    } else {
        Vec::new()
    };
    Box::new(matched.into_iter().map(move |i| {
        let start: usize = original_lines[..i].iter().map(|l| l.len() + 1).sum();
        let end: usize = start
            + original_lines[i..i + search_len]
                .iter()
                .map(|l| l.len())
                .sum::<usize>()
            + search_len.saturating_sub(1);
        content[start..end].to_string()
    }))
}

/// `BlockAnchorReplacer` (edit.ts:288-425): first/last anchors; Levenshtein
/// similarity over the middle lines with a 0.65 threshold.
fn block_anchor_replacer<'a>(content: &'a str, find: &'a str) -> ReplacerIter<'a> {
    let original_lines: Vec<&str> = content.split('\n').collect();
    let mut search_lines: Vec<&str> = find.split('\n').collect();
    if search_lines.len() < 3 {
        return Box::new(std::iter::empty());
    }
    if search_lines.last() == Some(&"") {
        search_lines.pop();
    }
    let first_line_search = search_lines[0].trim().to_string();
    let last_line_search = search_lines[search_lines.len() - 1].trim().to_string();
    let search_block_size = search_lines.len();
    // maxLineDelta = Math.max(1, Math.floor(searchBlockSize * 0.25))
    let max_line_delta = 1.max(search_block_size / 4) as i64;

    // Collect all candidate positions where both anchors match.
    let mut candidates: Vec<(usize, usize)> = Vec::new();
    for i in 0..original_lines.len() {
        if original_lines[i].trim() != first_line_search {
            continue;
        }
        for (j, line) in original_lines.iter().enumerate().skip(i + 2) {
            if line.trim() == last_line_search {
                let actual_block_size = j - i + 1;
                if (actual_block_size as i64 - search_block_size as i64).abs() <= max_line_delta {
                    candidates.push((i, j));
                }
                break; // Only match the first occurrence of the last line
            }
        }
    }
    if candidates.is_empty() {
        return Box::new(std::iter::empty());
    }
    let substring_of = |start_line: usize, end_line: usize| -> String {
        let start: usize = original_lines[..start_line]
            .iter()
            .map(|l| l.len() + 1)
            .sum();
        let end: usize = start
            + original_lines[start_line..=end_line]
                .iter()
                .map(|l| l.len())
                .sum::<usize>()
            + (end_line - start_line);
        content[start..end].to_string()
    };

    let result: Option<String> = if candidates.len() == 1 {
        let (start_line, end_line) = candidates[0];
        let actual_block_size = end_line - start_line + 1;
        let lines_to_check =
            (search_block_size.saturating_sub(2)).min(actual_block_size.saturating_sub(2));
        let mut similarity = 0.0f64;
        if lines_to_check > 0 {
            let mut j = 1;
            while j < search_block_size - 1 && j < actual_block_size - 1 {
                let original_line = original_lines[start_line + j].trim();
                let search_line = search_lines[j].trim();
                let max_len = original_line
                    .chars()
                    .count()
                    .max(search_line.chars().count());
                if max_len == 0 {
                    j += 1;
                    continue;
                }
                let distance = levenshtein(original_line, search_line);
                similarity += (1.0 - distance as f64 / max_len as f64) / lines_to_check as f64;
                // Exit early when threshold is reached
                if similarity >= SINGLE_CANDIDATE_SIMILARITY_THRESHOLD {
                    break;
                }
                j += 1;
            }
        } else {
            similarity = 1.0;
        }
        if similarity >= SINGLE_CANDIDATE_SIMILARITY_THRESHOLD {
            Some(substring_of(start_line, end_line))
        } else {
            None
        }
    } else {
        let mut best_match: Option<(usize, usize)> = None;
        let mut max_similarity = -1.0f64;
        for candidate in &candidates {
            let (start_line, end_line) = *candidate;
            let actual_block_size = end_line - start_line + 1;
            let lines_to_check =
                (search_block_size.saturating_sub(2)).min(actual_block_size.saturating_sub(2));
            let mut similarity = 0.0f64;
            if lines_to_check > 0 {
                let mut j = 1;
                while j < search_block_size - 1 && j < actual_block_size - 1 {
                    let original_line = original_lines[start_line + j].trim();
                    let search_line = search_lines[j].trim();
                    let max_len = original_line
                        .chars()
                        .count()
                        .max(search_line.chars().count());
                    if max_len == 0 {
                        j += 1;
                        continue;
                    }
                    let distance = levenshtein(original_line, search_line);
                    similarity += 1.0 - distance as f64 / max_len as f64;
                    j += 1;
                }
                similarity /= lines_to_check as f64;
            } else {
                similarity = 1.0;
            }
            if similarity > max_similarity {
                max_similarity = similarity;
                best_match = Some(*candidate);
            }
        }
        if max_similarity >= MULTIPLE_CANDIDATES_SIMILARITY_THRESHOLD {
            best_match.map(|(start_line, end_line)| substring_of(start_line, end_line))
        } else {
            None
        }
    };
    Box::new(result.into_iter())
}

/// `normalizeWhitespace` — `text.replace(/\s+/g, " ").trim()`
/// (edit.ts:428-429).
fn normalize_whitespace(text: &str) -> String {
    let replaced = replace_ws_runs(text);
    replaced.trim().to_string()
}

fn replace_ws_runs(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut in_ws = false;
    for c in text.chars() {
        if c.is_whitespace() {
            if !in_ws {
                out.push(' ');
                in_ws = true;
            }
        } else {
            out.push(c);
            in_ws = false;
        }
    }
    out
}

/// Escapes `[.*+?^${}()|[\]\\]` with a backslash (edit.ts:444).
fn escape_regex(word: &str) -> String {
    let mut out = String::with_capacity(word.len());
    for c in word.chars() {
        if matches!(
            c,
            '.' | '*' | '+' | '?' | '^' | '$' | '{' | '}' | '(' | ')' | '[' | ']' | '\\'
        ) {
            out.push('\\');
        }
        out.push(c);
    }
    out
}

/// `WhitespaceNormalizedReplacer` (edit.ts:427-469).
fn whitespace_normalized_replacer<'a>(content: &'a str, find: &'a str) -> ReplacerIter<'a> {
    let normalized_find = normalize_whitespace(find);
    let lines: Vec<&str> = content.split('\n').collect();
    let mut candidates: Vec<String> = Vec::new();

    // Handle single line matches
    for line in &lines {
        if normalize_whitespace(line) == normalized_find {
            candidates.push(line.to_string());
        } else {
            let normalized_line = normalize_whitespace(line);
            if normalized_line.contains(&normalized_find) {
                // Find the actual substring in the original line that matches
                let trimmed = find.trim();
                let words: Vec<&str> = if trimmed.is_empty() {
                    vec![""]
                } else {
                    trimmed.split_whitespace().collect()
                };
                let pattern = words
                    .iter()
                    .map(|word| escape_regex(word))
                    .collect::<Vec<_>>()
                    .join(r"\s+");
                if let Ok(regex) = regex::Regex::new(&pattern) {
                    if let Some(found) = regex.find(line) {
                        candidates.push(found.as_str().to_string());
                    }
                }
            }
        }
    }

    // Handle multi-line matches
    let find_lines: Vec<&str> = find.split('\n').collect();
    if find_lines.len() > 1 {
        for i in 0..lines.len().saturating_sub(find_lines.len() - 1) {
            if i + find_lines.len() > lines.len() {
                break;
            }
            let block = lines[i..i + find_lines.len()].join("\n");
            if normalize_whitespace(&block) == normalized_find {
                candidates.push(block);
            }
        }
    }
    Box::new(candidates.into_iter())
}

/// `removeIndentation` — strips the minimum common leading whitespace
/// (edit.ts:472-484).
fn remove_indentation(text: &str) -> String {
    let lines: Vec<&str> = text.split('\n').collect();
    let non_empty: Vec<&str> = lines
        .iter()
        .copied()
        .filter(|line| !line.trim().is_empty())
        .collect();
    if non_empty.is_empty() {
        return text.to_string();
    }
    let min_indent = non_empty
        .iter()
        .map(|line| leading_whitespace_len(line))
        .min()
        .expect("non_empty is non-empty");
    lines
        .iter()
        .map(|line| {
            if line.trim().is_empty() {
                (*line).to_string()
            } else {
                skip_chars(line, min_indent).to_string()
            }
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// Length in characters of the leading whitespace of `line` (`/^(\s*)/`).
fn leading_whitespace_len(line: &str) -> usize {
    line.chars().take_while(|c| c.is_whitespace()).count()
}

/// Byte offset of `n` characters into `line`.
fn leading_byte_len(line: &str, n: usize) -> usize {
    line.char_indices()
        .nth(n)
        .map(|(i, _)| i)
        .unwrap_or(line.len())
}

fn skip_chars(text: &str, n: usize) -> &str {
    let offset = leading_byte_len(text, n);
    &text[offset..]
}

/// `IndentationFlexibleReplacer` (edit.ts:471-497).
fn indentation_flexible_replacer<'a>(content: &'a str, find: &'a str) -> ReplacerIter<'a> {
    let normalized_find = remove_indentation(find);
    let content_lines: Vec<&str> = content.split('\n').collect();
    let find_lines: Vec<&str> = find.split('\n').collect();

    let mut candidates: Vec<String> = Vec::new();
    if content_lines.len() >= find_lines.len() {
        for i in 0..=content_lines.len() - find_lines.len() {
            let block = content_lines[i..i + find_lines.len()].join("\n");
            if remove_indentation(&block) == normalized_find {
                candidates.push(block);
            }
        }
    }
    Box::new(candidates.into_iter())
}

/// `EscapeNormalizedReplacer`'s `unescapeString` (edit.ts:500-525): maps a
/// backslash followed by `n t r ' " \ ` \\ <newline> $` to its literal.
fn unescape_string(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut chars = s.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '\\' {
            if let Some(&next) = chars.peek() {
                let replacement = match next {
                    'n' => Some('\n'),
                    't' => Some('\t'),
                    'r' => Some('\r'),
                    '\'' | '"' | '`' | '\\' | '$' => Some(next),
                    '\n' => Some('\n'),
                    _ => None,
                };
                if let Some(r) = replacement {
                    out.push(r);
                    chars.next();
                    continue;
                }
            }
        }
        out.push(c);
    }
    out
}

/// `EscapeNormalizedReplacer` (edit.ts:499-546).
fn escape_normalized_replacer<'a>(content: &'a str, find: &'a str) -> ReplacerIter<'a> {
    let unescaped_find = unescape_string(find);
    let mut candidates: Vec<String> = Vec::new();

    // Try direct match with unescaped find string
    if content.contains(unescaped_find.as_str()) {
        candidates.push(unescaped_find.clone());
    }

    // Also try finding escaped versions in content that match unescaped find
    let lines: Vec<&str> = content.split('\n').collect();
    let find_lines: Vec<&str> = unescaped_find.split('\n').collect();
    if lines.len() >= find_lines.len() {
        for i in 0..=lines.len() - find_lines.len() {
            let block = lines[i..i + find_lines.len()].join("\n");
            if unescape_string(&block) == unescaped_find {
                candidates.push(block);
            }
        }
    }
    Box::new(candidates.into_iter())
}

/// `MultiOccurrenceReplacer` (edit.ts:548-560): yields `find` for every
/// exact occurrence.
fn multi_occurrence_replacer<'a>(content: &'a str, find: &'a str) -> ReplacerIter<'a> {
    if find.is_empty() {
        // Unreachable through `replace` (oldString is checked non-empty);
        // the TS generator would loop forever here.
        return Box::new(std::iter::empty());
    }
    let count = content.match_indices(find).count();
    Box::new(std::iter::repeat_n(find.to_string(), count))
}

/// `TrimmedBoundaryReplacer` (edit.ts:562-586).
fn trimmed_boundary_replacer<'a>(content: &'a str, find: &'a str) -> ReplacerIter<'a> {
    let trimmed_find = find.trim();
    if trimmed_find == find {
        return Box::new(std::iter::empty());
    }
    let mut candidates: Vec<String> = Vec::new();
    if content.contains(trimmed_find) {
        candidates.push(trimmed_find.to_string());
    }
    let lines: Vec<&str> = content.split('\n').collect();
    let find_lines: Vec<&str> = find.split('\n').collect();
    if lines.len() >= find_lines.len() {
        for i in 0..=lines.len() - find_lines.len() {
            let block = lines[i..i + find_lines.len()].join("\n");
            if block.trim() == trimmed_find {
                candidates.push(block);
            }
        }
    }
    Box::new(candidates.into_iter())
}

/// `ContextAwareReplacer` (edit.ts:588-644): anchors + >=50% trimmed-middle
/// equality.
fn context_aware_replacer<'a>(content: &'a str, find: &'a str) -> ReplacerIter<'a> {
    let content_lines: Vec<&str> = content.split('\n').collect();
    let mut find_lines: Vec<&str> = find.split('\n').collect();
    if find_lines.len() < 3 {
        return Box::new(std::iter::empty());
    }
    if find_lines.last() == Some(&"") {
        find_lines.pop();
    }
    let first_line = find_lines[0].trim().to_string();
    let last_line = find_lines[find_lines.len() - 1].trim().to_string();

    let mut candidates: Vec<String> = Vec::new();
    for i in 0..content_lines.len() {
        if content_lines[i].trim() != first_line {
            continue;
        }
        for j in (i + 2)..content_lines.len() {
            if content_lines[j].trim() == last_line {
                let block_lines = &content_lines[i..=j];
                if block_lines.len() == find_lines.len() {
                    let mut matching_lines = 0;
                    let mut total_non_empty_lines = 0;
                    for k in 1..block_lines.len() - 1 {
                        let block_line = block_lines[k].trim();
                        let find_line = find_lines[k].trim();
                        if !block_line.is_empty() || !find_line.is_empty() {
                            total_non_empty_lines += 1;
                            if block_line == find_line {
                                matching_lines += 1;
                            }
                        }
                    }
                    if total_non_empty_lines == 0
                        || matching_lines as f64 / total_non_empty_lines as f64 >= 0.5
                    {
                        candidates.push(block_lines.join("\n"));
                        break; // Only match the first occurrence
                    }
                }
                break;
            }
        }
    }
    Box::new(candidates.into_iter())
}

/// The replacer chain (edit.ts:694-704) — order is binding.
pub const REPLACERS: [Replacer; 9] = [
    simple_replacer,
    line_trimmed_replacer,
    block_anchor_replacer,
    whitespace_normalized_replacer,
    indentation_flexible_replacer,
    escape_normalized_replacer,
    trimmed_boundary_replacer,
    context_aware_replacer,
    multi_occurrence_replacer,
];

// ---------------------------------------------------------------------------
// replace() + trimDiff (edit.ts:646-737).
// ---------------------------------------------------------------------------

/// `replace` (edit.ts:682-729): runs the replacer chain against `content`,
/// replacing the first usable match with `new_string` (all matches when
/// `replace_all`). Errors carry the exact TS messages.
pub fn replace(
    content: &str,
    old_string: &str,
    new_string: &str,
    replace_all: bool,
) -> Result<String, String> {
    if old_string == new_string {
        return Err("No changes to apply: oldString and newString are identical.".to_string());
    }
    if old_string.is_empty() {
        return Err("oldString cannot be empty when editing an existing file. Provide the exact text to replace, or use write for an intentional full-file replacement.".to_string());
    }

    let mut not_found = true;
    for replacer in REPLACERS {
        for search in replacer(content, old_string) {
            let Some(index) = content.find(search.as_str()) else {
                continue;
            };
            not_found = false;
            if is_disproportionate_match(search.as_str(), old_string) {
                return Err("Refusing replacement because the matched span is much larger than oldString. Re-read the file and provide the full exact oldString for the intended replacement.".to_string());
            }
            if replace_all {
                return Ok(content.replace(search.as_str(), new_string));
            }
            let last_index = content.rfind(search.as_str());
            if last_index != Some(index) {
                continue;
            }
            let mut out = String::with_capacity(content.len());
            out.push_str(&content[..index]);
            out.push_str(new_string);
            out.push_str(&content[index + search.len()..]);
            return Ok(out);
        }
    }

    if not_found {
        Err("Could not find oldString in the file. It must match exactly, including whitespace, indentation, and line endings.".to_string())
    } else {
        Err("Found multiple matches for oldString. Provide more surrounding context to make the match unique.".to_string())
    }
}

/// `isDisproportionateMatch` (edit.ts:731-737).
fn is_disproportionate_match(search: &str, old_string: &str) -> bool {
    let old_lines = old_string.split('\n').count();
    let search_lines = search.split('\n').count();
    if search_lines >= (old_lines + 3).max(old_lines * 2) {
        return true;
    }
    if old_lines == 1 {
        return false;
    }
    let old_trimmed = old_string.trim().chars().count();
    let search_trimmed = search.trim().chars().count();
    search_trimmed > (old_trimmed + 500).max(old_trimmed * 4)
}

/// `trimDiff` (edit.ts:646-680): filters diff content lines, computes the
/// smallest common leading whitespace across non-blank trimmed content
/// lines, and slices that many characters off every content line.
pub fn trim_diff(diff: &str) -> String {
    let lines: Vec<&str> = diff.split('\n').collect();
    let is_content = |line: &str| {
        (line.starts_with('+') || line.starts_with('-') || line.starts_with(' '))
            && !line.starts_with("---")
            && !line.starts_with("+++")
    };
    let content_lines: Vec<&str> = lines.iter().copied().filter(|l| is_content(l)).collect();
    if content_lines.is_empty() {
        return diff.to_string();
    }

    let mut min = usize::MAX;
    for line in &content_lines {
        let content = &line[1..];
        if !content.trim().is_empty() {
            min = min.min(leading_whitespace_len(content));
        }
    }
    if min == usize::MAX || min == 0 {
        return diff.to_string();
    }

    let trimmed_lines = lines.iter().map(|line| {
        if is_content(line) {
            format!("{}{}", &line[..1], skip_chars(&line[1..], min))
        } else {
            (*line).to_string()
        }
    });
    trimmed_lines.collect::<Vec<_>>().join("\n")
}

// ---------------------------------------------------------------------------
// The edit tool (edit.ts:22-215).
// ---------------------------------------------------------------------------

/// `normalizeLineEndings` (edit.ts:22-24).
fn normalize_line_endings(text: &str) -> String {
    text.replace("\r\n", "\n")
}

/// `detectLineEnding` (edit.ts:26-28).
fn detect_line_ending(text: &str) -> &'static str {
    if text.contains("\r\n") {
        "\r\n"
    } else {
        "\n"
    }
}

/// `convertToLineEnding` (edit.ts:30-33).
fn convert_to_line_ending(text: &str, ending: &str) -> String {
    if ending == "\n" {
        text.to_string()
    } else {
        text.replace('\n', "\r\n")
    }
}

/// Per-file lock registry (edit.ts:35-45). TS never prunes map entries; the
/// Rust port keeps the quirk.
fn locks() -> &'static Mutex<HashMap<PathBuf, Arc<tokio::sync::Mutex<()>>>> {
    static LOCKS: OnceLock<Mutex<HashMap<PathBuf, Arc<tokio::sync::Mutex<()>>>>> = OnceLock::new();
    LOCKS.get_or_init(|| Mutex::new(HashMap::new()))
}

/// `lock(filePath)` (edit.ts:37-45): one semaphore per resolved file path.
fn lock(file_path: &Path) -> Arc<tokio::sync::Mutex<()>> {
    // FSUtil.resolve: realpath with an ENOENT-style fallback.
    let resolved = std::fs::canonicalize(file_path).unwrap_or_else(|_| file_path.to_path_buf());
    let mut map = locks().lock().unwrap();
    Arc::clone(map.entry(resolved).or_default())
}

#[derive(Debug, Deserialize)]
pub struct EditParameters {
    #[serde(rename = "filePath")]
    pub file_path: String,
    #[serde(rename = "oldString")]
    pub old_string: String,
    #[serde(rename = "newString")]
    pub new_string: String,
    #[serde(rename = "replaceAll", default)]
    pub replace_all: bool,
}

/// Hand-authored JSON Schema (byte-matches the Effect-generated schema; see
/// `tests/golden/toolschema/edit.json`).
pub fn parameters() -> Value {
    json!({
        "$schema": "https://json-schema.org/draft/2020-12/schema",
        "type": "object",
        "properties": {
            "filePath": {
                "type": "string",
                "description": "The absolute path to the file to modify"
            },
            "oldString": {
                "type": "string",
                "description": "The text to replace"
            },
            "newString": {
                "type": "string",
                "description": "The text to replace it with (must be different from oldString)"
            },
            "replaceAll": {
                "type": "boolean",
                "description": "Replace all occurrences of oldString (default false)"
            }
        },
        "required": ["filePath", "oldString", "newString"]
    })
}

/// Build the `edit` tool. `lsp`/`format`/`events` are the M4.3 seams
/// (`None` = no LSP, no formatter, no event bridge).
pub fn edit_tool(
    truncate: Arc<dyn Truncate>,
    agents: Arc<dyn Agents>,
    lsp: Option<Arc<dyn Lsp>>,
    format: Option<Arc<dyn Formatter>>,
    events: Option<Arc<dyn FileEvents>>,
) -> ToolDef {
    define(
        "edit",
        include_str!("txt/edit.txt"),
        parameters(),
        None,
        truncate,
        agents,
        move |params: EditParameters, ctx: ToolCtxRef<'_>| {
            run(params, ctx, lsp.clone(), format.clone(), events.clone())
        },
    )
}

async fn ask_edit(
    ctx: &ToolCtxRef<'_>,
    instance: &InstanceContext,
    file_path: &Path,
    diff: &str,
) -> Result<(), ToolError> {
    ctx.ask
        .ask(AskRequest {
            permission: "edit".to_string(),
            patterns: vec![ts_relative(&instance.worktree, file_path)],
            always: vec!["*".to_string()],
            metadata: json!({
                "filepath": file_path.to_string_lossy(),
                "diff": diff,
            }),
        })
        .await
}

fn run(
    params: EditParameters,
    ctx: ToolCtxRef<'_>,
    lsp: Option<Arc<dyn Lsp>>,
    format: Option<Arc<dyn Formatter>>,
    events: Option<Arc<dyn FileEvents>>,
) -> BoxFuture<'_, Result<ExecuteResult, ToolError>> {
    Box::pin(async move {
        if params.file_path.is_empty() {
            return Err(ToolError::Failed("filePath is required".to_string()));
        }
        if params.old_string == params.new_string {
            return Err(ToolError::Failed(
                "No changes to apply: oldString and newString are identical.".to_string(),
            ));
        }

        let instance = ctx.instance.clone();
        let file_path = ts_resolve(&instance.directory, &params.file_path);
        let file_str = file_path.to_string_lossy().to_string();
        assert_external_directory(&ctx, Some(&file_str), ExternalOptions::default()).await?;

        let mut diff;
        let content_old;
        let mut content_new;

        // Per-file semaphore (edit.ts:88): held for the whole body.
        let semaphore = lock(&file_path);
        let _guard = semaphore.lock().await;

        if params.old_string.is_empty() {
            let existed = tokio::fs::metadata(&file_path).await.is_ok();
            if existed {
                return Err(ToolError::Failed("oldString cannot be empty when editing an existing file. Provide the exact text to replace, or use write for an intentional full-file replacement.".to_string()));
            }
            let next = bom::split(&params.new_string);
            let desired_bom = next.bom;
            content_old = String::new();
            content_new = next.text;
            diff = trim_diff(&create_two_files_patch(
                &file_str,
                &content_old,
                &content_new,
            ));
            ask_edit(&ctx, &instance, &file_path, &diff).await?;
            bom::write_with_dirs(&file_path, &bom::join(&content_new, desired_bom)).await?;
            if format_file(&format, &file_str).await {
                content_new = bom::sync_file(&file_path, desired_bom).await?;
            }
            publish_edited(&events, &file_str).await;
            publish_updated(&events, &file_str, "add").await;
        } else {
            let info = match tokio::fs::metadata(&file_path).await {
                Ok(info) => info,
                Err(_) => return Err(ToolError::Failed(format!("File {file_str} not found"))),
            };
            if info.is_dir() {
                return Err(ToolError::Failed(format!(
                    "Path is a directory, not a file: {file_str}"
                )));
            }
            let source = bom::read_file(&file_path).await?;
            content_old = source.text;

            let ending = detect_line_ending(&content_old);
            let old = convert_to_line_ending(&normalize_line_endings(&params.old_string), ending);
            let replacement =
                convert_to_line_ending(&normalize_line_endings(&params.new_string), ending);

            let next = bom::split(
                &replace(&content_old, &old, &replacement, params.replace_all)
                    .map_err(ToolError::Failed)?,
            );
            let desired_bom = source.bom || next.bom;
            content_new = next.text;

            diff = trim_diff(&create_two_files_patch(
                &file_str,
                &normalize_line_endings(&content_old),
                &normalize_line_endings(&content_new),
            ));
            ask_edit(&ctx, &instance, &file_path, &diff).await?;

            bom::write_with_dirs(&file_path, &bom::join(&content_new, desired_bom)).await?;
            if format_file(&format, &file_str).await {
                content_new = bom::sync_file(&file_path, desired_bom).await?;
            }
            publish_edited(&events, &file_str).await;
            publish_updated(&events, &file_str, "change").await;
            diff = trim_diff(&create_two_files_patch(
                &file_str,
                &normalize_line_endings(&content_old),
                &normalize_line_endings(&content_new),
            ));
        }

        let (additions, deletions) = diff_line_counts(&content_old, &content_new);
        let filediff = FileDiff {
            file: file_str.clone(),
            patch: diff.clone(),
            additions,
            deletions,
        };
        ctx.metadata
            .metadata(MetadataInput {
                title: None,
                metadata: Some(json!({
                    "diff": diff,
                    "filediff": filediff,
                    "diagnostics": {},
                })),
            })
            .await?;

        let mut output = "Edit applied successfully.".to_string();
        let diagnostics = match &lsp {
            Some(lsp) => {
                lsp.touch_file(&file_str).await;
                lsp.diagnostics().await
            }
            None => json!({}),
        };
        // FSUtil.normalizePath is identity on non-win32.
        let issues = diagnostics.get(&file_str).cloned().unwrap_or(json!([]));
        let block = diagnostic_report(&file_str, &issues);
        if !block.is_empty() {
            output.push_str(&format!(
                "\n\nLSP errors detected in this file, please fix:\n{block}"
            ));
        }

        Ok(ExecuteResult {
            title: ts_relative(&instance.worktree, &file_path),
            metadata: json!({
                "diagnostics": diagnostics,
                "diff": diff,
                "filediff": filediff,
            }),
            output,
            attachments: None,
        })
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tool::def::Extra;
    use crate::tool::ripgrep::test_support::{ctx, fixed_agents, instance, RecordingAsk};
    use crate::tool::truncate::TruncateService;
    use std::sync::atomic::{AtomicUsize, Ordering};

    struct FixedLsp {
        diagnostics: Value,
    }

    impl Lsp for FixedLsp {
        fn touch_file<'a>(&'a self, _file: &'a str) -> BoxFuture<'a, ()> {
            Box::pin(async {})
        }

        fn diagnostics<'a>(&'a self) -> BoxFuture<'a, Value> {
            Box::pin(async move { self.diagnostics.clone() })
        }
    }

    #[derive(Default)]
    struct RecordingEvents {
        calls: Mutex<Vec<String>>,
    }

    impl RecordingEvents {
        fn calls(&self) -> Vec<String> {
            self.calls.lock().unwrap().clone()
        }
    }

    impl FileEvents for RecordingEvents {
        fn edited<'a>(&'a self, file: &'a str) -> BoxFuture<'a, ()> {
            Box::pin(async move {
                self.calls.lock().unwrap().push(format!("edited:{file}"));
            })
        }

        fn updated<'a>(&'a self, file: &'a str, event: &'a str) -> BoxFuture<'a, ()> {
            Box::pin(async move {
                self.calls
                    .lock()
                    .unwrap()
                    .push(format!("updated:{file}:{event}"));
            })
        }
    }

    fn tool(dir: &Path, lsp: Option<Arc<dyn Lsp>>, events: Arc<dyn FileEvents>) -> ToolDef {
        edit_tool(
            Arc::new(TruncateService::default_limits(dir.join("tool-output"))),
            fixed_agents(),
            lsp,
            None,
            Some(events),
        )
    }

    async fn call(
        dir: &Path,
        lsp: Option<Arc<dyn Lsp>>,
        events: Arc<dyn FileEvents>,
        args: Value,
    ) -> (
        Result<ExecuteResult, ToolError>,
        Vec<crate::tool::def::AskRequest>,
        Vec<crate::tool::def::MetadataInput>,
    ) {
        let def = tool(dir, lsp, events);
        let ask = RecordingAsk::new();
        let inst = instance(dir);
        let extra = Extra::default();
        let c = ctx(&ask, &inst, &extra);
        let result = (def.execute)(args, c).await;
        let metadata = ask.metadata_calls.lock().unwrap().clone();
        (result, ask.requests(), metadata)
    }

    fn events() -> Arc<RecordingEvents> {
        Arc::new(RecordingEvents::default())
    }

    fn write(dir: &Path, name: &str, contents: &str) -> PathBuf {
        let path = dir.join(name);
        std::fs::write(&path, contents).unwrap();
        path
    }

    #[test]
    fn golden_parameters_schema() {
        let golden = std::fs::read_to_string(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/tests/golden/toolschema/edit.json"
        ))
        .unwrap();
        let golden: Value = serde_json::from_str(&golden).unwrap();
        assert_eq!(parameters(), golden);
    }

    #[test]
    fn replace_exact() {
        assert_eq!(
            replace("hello world", "hello", "goodbye", false).unwrap(),
            "goodbye world"
        );
    }

    #[test]
    fn replace_whitespace_drifted() {
        assert_eq!(replace("a  b", "a b", "X", false).unwrap(), "X");
        assert_eq!(replace("x  a  b  y", "a b", "Z", false).unwrap(), "x  Z  y");
    }

    #[test]
    fn replace_indentation_drifted() {
        // LineTrimmedReplacer matches the block and yields the original
        // indentation for the replacement.
        assert_eq!(
            replace("  hello\n    world\n", "hello\n  world", "X", false).unwrap(),
            "X\n"
        );
        // IndentationFlexibleReplacer directly: blocks dedent to the same
        // text even when their absolute indentation differs.
        let candidates: Vec<String> = indentation_flexible_replacer(
            "    fn main() {\n      let x = 1;\n    }\n",
            "  fn main() {\n    let x = 1;\n  }\n",
        )
        .collect();
        assert_eq!(
            candidates,
            vec!["    fn main() {\n      let x = 1;\n    }\n".to_string()]
        );
    }

    #[test]
    fn replace_escaped_newline() {
        assert_eq!(
            replace("line1\nline2", "line1\\nline2", "X", false).unwrap(),
            "X"
        );
        // Escaped content matching through the block scan: the literal
        // `\t` in the content unescapes to the real tab in `find`.
        assert_eq!(replace("x\\ty", "x\ty", "X", false).unwrap(), "X");
    }

    #[test]
    fn replace_multi_occurrence() {
        assert_eq!(replace("x a x b x", "x", "y", true).unwrap(), "y a y b y");
        // Without replaceAll, multiple occurrences are refused.
        assert_eq!(
            replace("x a x", "x", "y", false).unwrap_err(),
            "Found multiple matches for oldString. Provide more surrounding context to make the match unique."
        );
    }

    #[test]
    fn replace_skips_non_unique_until_found() {
        // SimpleReplacer's candidate "cat" is non-unique; LineTrimmedReplacer
        // yields the first "  cat" block which is unique.
        assert_eq!(
            replace("  cat\nmid\n cat", "cat", "NEW", false).unwrap(),
            "NEW\nmid\n cat"
        );
    }

    #[test]
    fn replace_not_found_message() {
        assert_eq!(
            replace("abc", "zzz", "x", false).unwrap_err(),
            "Could not find oldString in the file. It must match exactly, including whitespace, indentation, and line endings."
        );
    }

    #[test]
    fn replace_guards_match_ts_messages() {
        assert_eq!(
            replace("abc", "a", "a", false).unwrap_err(),
            "No changes to apply: oldString and newString are identical."
        );
        assert_eq!(
            replace("abc", "", "a", false).unwrap_err(),
            "oldString cannot be empty when editing an existing file. Provide the exact text to replace, or use write for an intentional full-file replacement."
        );
    }

    #[test]
    fn replace_refuses_disproportionate_match() {
        let content = format!("x{}y", " ".repeat(600));
        let err = replace(&content, "x\ny", "X", false).unwrap_err();
        assert_eq!(
            err,
            "Refusing replacement because the matched span is much larger than oldString. Re-read the file and provide the full exact oldString for the intended replacement."
        );
    }

    #[test]
    fn is_disproportionate_match_rules() {
        // searchLines >= max(oldLines + 3, oldLines * 2)
        assert!(is_disproportionate_match("a\nb\nc\nd\ne\nf", "x"));
        // single-line old strings never hit the length rule
        assert!(!is_disproportionate_match(&"x".repeat(10_000), "seed"));
        // trimmed length > max(old.trim + 500, old.trim * 4) with old_lines > 1
        assert!(is_disproportionate_match(
            &format!("x\n{}", "y".repeat(1_000)),
            "x\ny"
        ));
        assert!(!is_disproportionate_match("x\ny\nz", "x\ny\nz"));
    }

    #[test]
    fn block_anchor_replacer_fuzzy_match() {
        // Levenshtein("middle drift", "middle draft") = 1/12 -> similarity
        // 0.92 >= 0.65, so the drifted middle line still matches on anchors.
        let content = "start\nmiddle draft\nend\nfiller\n";
        let found: Vec<String> =
            block_anchor_replacer(content, "start\nmiddle drift\nend\n").collect();
        assert_eq!(found, vec!["start\nmiddle draft\nend".to_string()]);
    }

    #[test]
    fn trim_diff_strips_common_indent() {
        let diff = "Index: f\n@@ -1 +1 @@\n+    hello\n-    world\n";
        assert_eq!(trim_diff(diff), "Index: f\n@@ -1 +1 @@\n+hello\n-world\n");
    }

    #[test]
    fn trim_diff_keeps_diff_without_content_lines() {
        let diff = "Index: f\n======\n--- f\n+++ f\n";
        assert_eq!(trim_diff(diff), diff);
    }

    #[test]
    fn trim_diff_zero_indent_is_a_no_op() {
        let diff = "+hello world\n-    bye bye\n";
        assert_eq!(trim_diff(diff), diff);
    }

    #[tokio::test]
    async fn edits_file_content() {
        let temp = crate::storage::test_support::TempDir::new("edit-basic");
        let file = write(temp.path(), "main.txt", "hello\nworld\n");
        let events = events();
        let (result, requests, metadata_calls) = call(
            temp.path(),
            None,
            events.clone(),
            json!({
                "filePath": file.to_string_lossy(),
                "oldString": "hello",
                "newString": "goodbye",
            }),
        )
        .await;

        let result = result.unwrap();
        assert_eq!(result.output, "Edit applied successfully.");
        assert_eq!(result.title, "main.txt");
        assert_eq!(std::fs::read_to_string(&file).unwrap(), "goodbye\nworld\n");

        // Ask request: permission edit, worktree-relative pattern.
        assert_eq!(requests.len(), 1);
        assert_eq!(requests[0].permission, "edit");
        assert_eq!(requests[0].patterns, vec!["main.txt".to_string()]);
        assert_eq!(requests[0].always, vec!["*".to_string()]);
        assert_eq!(
            requests[0].metadata["filepath"],
            json!(file.to_string_lossy())
        );
        let diff = requests[0].metadata["diff"].as_str().unwrap();
        assert!(diff.contains("-hello"), "{diff}");
        assert!(diff.contains("+goodbye"), "{diff}");

        // ctx.metadata carries { diff, filediff, diagnostics: {} }.
        assert_eq!(metadata_calls.len(), 1);
        let meta = metadata_calls[0].metadata.as_ref().unwrap();
        assert_eq!(meta["diagnostics"], json!({}));
        assert_eq!(meta["filediff"]["additions"], json!(1));
        assert_eq!(meta["filediff"]["deletions"], json!(1));
        assert_eq!(
            meta["filediff"]["file"],
            json!(file.to_string_lossy().to_string())
        );

        // Result metadata repeats diff + filediff with the real diagnostics.
        assert_eq!(result.metadata["diff"], json!(diff));
        assert_eq!(result.metadata["diagnostics"], json!({}));
        assert_eq!(
            result.metadata["filediff"],
            json!({
                "file": file.to_string_lossy().to_string(),
                "patch": diff,
                "additions": 1,
                "deletions": 1,
            })
        );

        let calls = events.calls();
        assert_eq!(
            calls,
            vec![
                format!("edited:{}", file.display()),
                format!("updated:{}:change", file.display()),
            ]
        );
        std::fs::remove_dir_all(temp.path()).ok();
    }

    #[tokio::test]
    async fn crlf_files_stay_crlf() {
        let temp = crate::storage::test_support::TempDir::new("edit-crlf");
        let file = write(temp.path(), "crlf.txt", "a\r\nb\r\nc\r\n");
        let (result, _, _) = call(
            temp.path(),
            None,
            events(),
            json!({
                "filePath": file.to_string_lossy(),
                "oldString": "b\r\n",
                "newString": "B\r\n",
            }),
        )
        .await;

        assert_eq!(result.unwrap().output, "Edit applied successfully.");
        assert_eq!(std::fs::read_to_string(&file).unwrap(), "a\r\nB\r\nc\r\n");
        std::fs::remove_dir_all(temp.path()).ok();
    }

    #[tokio::test]
    async fn bom_is_preserved_end_to_end() {
        let temp = crate::storage::test_support::TempDir::new("edit-bom");
        let file = write(temp.path(), "bom.txt", "\u{feff}hello\nworld\n");
        let (result, _, _) = call(
            temp.path(),
            None,
            events(),
            json!({
                "filePath": file.to_string_lossy(),
                "oldString": "hello",
                "newString": "goodbye",
            }),
        )
        .await;

        result.unwrap();
        assert_eq!(
            std::fs::read_to_string(&file).unwrap(),
            "\u{feff}goodbye\nworld\n"
        );
        std::fs::remove_dir_all(temp.path()).ok();
    }

    #[tokio::test]
    async fn edit_errors() {
        let temp = crate::storage::test_support::TempDir::new("edit-errors");
        let (result, _, _) = call(
            temp.path(),
            None,
            events(),
            json!({
                "filePath": temp.path().join("missing.txt").to_string_lossy(),
                "oldString": "a",
                "newString": "b",
            }),
        )
        .await;
        assert_eq!(
            result.unwrap_err().to_string(),
            format!(
                "File {} not found",
                temp.path().join("missing.txt").display()
            )
        );

        let dir = temp.path().join("adir");
        std::fs::create_dir(&dir).unwrap();
        let (result, _, _) = call(
            temp.path(),
            None,
            events(),
            json!({
                "filePath": dir.to_string_lossy(),
                "oldString": "a",
                "newString": "b",
            }),
        )
        .await;
        assert_eq!(
            result.unwrap_err().to_string(),
            format!("Path is a directory, not a file: {}", dir.display())
        );

        let file = write(temp.path(), "exists.txt", "content\n");
        let (result, _, _) = call(
            temp.path(),
            None,
            events(),
            json!({
                "filePath": file.to_string_lossy(),
                "oldString": "",
                "newString": "b",
            }),
        )
        .await;
        assert_eq!(
            result.unwrap_err().to_string(),
            "oldString cannot be empty when editing an existing file. Provide the exact text to replace, or use write for an intentional full-file replacement."
        );

        let (result, _, _) = call(
            temp.path(),
            None,
            events(),
            json!({
                "filePath": file.to_string_lossy(),
                "oldString": "same",
                "newString": "same",
            }),
        )
        .await;
        assert_eq!(
            result.unwrap_err().to_string(),
            "No changes to apply: oldString and newString are identical."
        );
        std::fs::remove_dir_all(temp.path()).ok();
    }

    #[tokio::test]
    async fn empty_old_string_creates_a_new_file() {
        let temp = crate::storage::test_support::TempDir::new("edit-create");
        let file = temp.path().join("created.txt");
        let events = events();
        let (result, requests, _) = call(
            temp.path(),
            None,
            events.clone(),
            json!({
                "filePath": file.to_string_lossy(),
                "oldString": "",
                "newString": "fresh\ncontent\n",
            }),
        )
        .await;

        let result = result.unwrap();
        assert_eq!(result.output, "Edit applied successfully.");
        assert_eq!(std::fs::read_to_string(&file).unwrap(), "fresh\ncontent\n");
        assert_eq!(requests[0].patterns, vec!["created.txt".to_string()]);
        assert_eq!(
            events.calls(),
            vec![
                format!("edited:{}", file.display()),
                format!("updated:{}:add", file.display()),
            ]
        );
        std::fs::remove_dir_all(temp.path()).ok();
    }

    #[tokio::test]
    async fn lsp_errors_append_to_output() {
        let temp = crate::storage::test_support::TempDir::new("edit-lsp");
        let file = write(temp.path(), "lsp.txt", "hello\n");
        let file_str = file.to_string_lossy().to_string();
        let lsp = Arc::new(FixedLsp {
            diagnostics: json!({
                file_str: [
                    {
                        "severity": 1,
                        "message": "oops",
                        "range": { "start": { "line": 0, "character": 2 } },
                    },
                ],
            }),
        });
        let (result, _, _) = call(
            temp.path(),
            Some(lsp),
            events(),
            json!({
                "filePath": file.to_string_lossy(),
                "oldString": "hello",
                "newString": "goodbye",
            }),
        )
        .await;

        let output = result.unwrap().output;
        assert!(output.starts_with("Edit applied successfully."), "{output}");
        assert!(
            output.contains(
                "\n\nLSP errors detected in this file, please fix:\n<diagnostics file=\""
            ),
            "{output}"
        );
        assert!(output.contains("ERROR [1:3] oops"), "{output}");
        std::fs::remove_dir_all(temp.path()).ok();
    }

    #[tokio::test]
    async fn per_file_lock_serializes_same_file_only() {
        let temp = crate::storage::test_support::TempDir::new("edit-lock");
        let a = temp.path().join("a.txt");
        let b = temp.path().join("b.txt");
        assert!(!Arc::ptr_eq(&lock(&a), &lock(&b)));

        let counter = Arc::new(AtomicUsize::new(0));
        let max_seen = Arc::new(AtomicUsize::new(0));
        let semaphore = lock(&a);
        let mut handles = Vec::new();
        for _ in 0..10 {
            let semaphore = Arc::clone(&semaphore);
            let counter = Arc::clone(&counter);
            let max_seen = Arc::clone(&max_seen);
            handles.push(tokio::spawn(async move {
                let _guard = semaphore.lock().await;
                let current = counter.fetch_add(1, Ordering::SeqCst) + 1;
                max_seen.fetch_max(current, Ordering::SeqCst);
                tokio::time::sleep(std::time::Duration::from_millis(1)).await;
                counter.fetch_sub(1, Ordering::SeqCst);
            }));
        }
        for handle in handles {
            handle.await.unwrap();
        }
        assert_eq!(max_seen.load(Ordering::SeqCst), 1);
        std::fs::remove_dir_all(temp.path()).ok();
    }
}
