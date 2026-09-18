//! Line diff + unified patch rendering — byte-parity port of the npm `diff`
//! package surface the TS reference uses (`createTwoFilesPatch` and
//! `diffLines`, diff@8.0.2). The Myers-style algorithm is a faithful port of
//! jsdiff's `Diff.diff` (including its tie-breaking and component merging) so
//! the rendered hunks match byte-for-byte (spec M4.3 S1).
//!
//! The npm `diff` package's `createTwoFilesPatch` is a unified diff with a
//! fixed header:
//!
//! ```text
//! Index: {path}
//! ===================================================================
//! --- {path}
//! +++ {path}
//! @@ -l,c +l,c @@
//! ```

use std::collections::HashMap;

use serde::Serialize;

/// Default context size of jsdiff's `structuredPatch`.
pub const CONTEXT: usize = 4;

/// A single `diffLines` change block: `count` lines (newlines kept) marked
/// `added` / `removed` (common when both are false).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Change {
    pub added: bool,
    pub removed: bool,
    pub count: usize,
    pub lines: Vec<String>,
}

/// jsdiff line tokenizer (diff/line.js `tokenize`): splits keeping the
/// trailing newline with each line (`\r` stays with its line, matching
/// `value.split(/(\n|\r\n)/)`).
fn tokenize(text: &str) -> Vec<String> {
    // `removeEmpty` in `Diff.diff` drops the token the JS pop rule misses.
    if text.is_empty() {
        return Vec::new();
    }
    let mut tokens: Vec<String> = Vec::new();
    let lines = text.split('\n').collect::<Vec<_>>();
    // All but the final element merge with their newline; a missing trailing
    // newline leaves the last line bare.
    let complete = lines.len() - 1;
    for line in &lines[..complete] {
        tokens.push(format!("{line}\n"));
    }
    if !text.ends_with('\n') {
        tokens.push(lines[complete].to_string());
    }
    tokens
}

/// Linked-list component of jsdiff's path chains.
#[derive(Debug, Clone)]
struct Component {
    count: usize,
    added: bool,
    removed: bool,
    previous: Option<usize>,
}

/// A path in the edit graph (`bestPath[diagonal]`).
#[derive(Debug, Clone)]
struct Path {
    old_pos: i64,
    last: Option<usize>,
}

fn add_to_path(
    path: &Path,
    added: bool,
    removed: bool,
    old_pos_inc: i64,
    arena: &mut Vec<Component>,
) -> Path {
    let old_pos = path.old_pos + old_pos_inc;
    let merged = match path.last {
        Some(index) => {
            let last = &arena[index];
            last.added == added && last.removed == removed
        }
        None => false,
    };
    let component = if merged {
        let last = &arena[path.last.expect("checked")];
        Component {
            count: last.count + 1,
            added,
            removed,
            previous: last.previous,
        }
    } else {
        Component {
            count: 1,
            added,
            removed,
            previous: path.last,
        }
    };
    arena.push(component);
    Path {
        old_pos,
        last: Some(arena.len() - 1),
    }
}

fn extract_common(
    path: &mut Path,
    old_tokens: &[String],
    new_tokens: &[String],
    diagonal_path: i64,
    arena: &mut Vec<Component>,
) -> i64 {
    let new_len = new_tokens.len() as i64;
    let old_len = old_tokens.len() as i64;
    let mut old_pos = path.old_pos;
    let mut new_pos = old_pos - diagonal_path;
    let mut common_count = 0i64;
    while new_pos + 1 < new_len
        && old_pos + 1 < old_len
        && old_tokens[(old_pos + 1) as usize] == new_tokens[(new_pos + 1) as usize]
    {
        new_pos += 1;
        old_pos += 1;
        common_count += 1;
    }
    if common_count > 0 {
        let component = Component {
            count: common_count as usize,
            added: false,
            removed: false,
            previous: path.last,
        };
        arena.push(component);
        path.last = Some(arena.len() - 1);
    }
    path.old_pos = old_pos;
    new_pos
}

fn build_values(
    last: Option<usize>,
    arena: &[Component],
    old_tokens: &[String],
    new_tokens: &[String],
) -> Vec<Change> {
    let mut components = Vec::new();
    let mut current = last;
    while let Some(index) = current {
        let component = arena[index].clone();
        current = component.previous;
        components.push(component);
    }
    components.reverse();

    let mut out = Vec::with_capacity(components.len());
    let mut new_pos = 0usize;
    let mut old_pos = 0usize;
    for component in components {
        if !component.removed {
            let lines = new_tokens[new_pos..new_pos + component.count].to_vec();
            new_pos += component.count;
            if !component.added {
                old_pos += component.count;
            }
            out.push(Change {
                added: component.added,
                removed: false,
                count: component.count,
                lines,
            });
        } else {
            let lines = old_tokens[old_pos..old_pos + component.count].to_vec();
            old_pos += component.count;
            out.push(Change {
                added: false,
                removed: true,
                count: component.count,
                lines,
            });
        }
    }
    out
}

/// `diffLines(old, new)` — the jsdiff line-mode diff.
pub fn diff_lines(old: &str, new: &str) -> Vec<Change> {
    let old_tokens = tokenize(old);
    let new_tokens = tokenize(new);
    let old_len = old_tokens.len() as i64;
    let new_len = new_tokens.len() as i64;
    let max_edit_length = old_len + new_len;

    let mut arena: Vec<Component> = Vec::new();
    let mut best: HashMap<i64, Path> = HashMap::new();

    let seed = Path {
        old_pos: -1,
        last: None,
    };
    let mut seed = seed;
    let new_pos = extract_common(&mut seed, &old_tokens, &new_tokens, 0, &mut arena);
    if seed.old_pos + 1 >= old_len && new_pos + 1 >= new_len {
        return build_values(seed.last, &arena, &old_tokens, &new_tokens);
    }
    best.insert(0, seed);

    let mut min_diagonal = i64::MIN;
    let mut max_diagonal = i64::MAX;
    let mut edit_length: i64 = 1;
    while edit_length <= max_edit_length {
        let mut diagonal = min_diagonal.max(-edit_length);
        let upper = max_diagonal.min(edit_length);
        while diagonal <= upper {
            let remove_path = best.get(&(diagonal - 1)).cloned();
            best.remove(&(diagonal - 1));
            let add_path = best.get(&(diagonal + 1)).cloned();

            let can_add = match &add_path {
                Some(path) => {
                    let pos = path.old_pos - diagonal;
                    (0..new_len).contains(&pos)
                }
                None => false,
            };
            let can_remove = match &remove_path {
                Some(path) => path.old_pos + 1 < old_len,
                None => false,
            };
            if !can_add && !can_remove {
                best.remove(&diagonal);
                diagonal += 2;
                continue;
            }
            let base_path = if !can_remove
                || (can_add
                    && remove_path.as_ref().expect("canRemove").old_pos
                        < add_path.as_ref().expect("canAdd").old_pos)
            {
                add_to_path(
                    add_path.as_ref().expect("canAdd"),
                    true,
                    false,
                    0,
                    &mut arena,
                )
            } else {
                add_to_path(
                    remove_path.as_ref().expect("canRemove"),
                    false,
                    true,
                    1,
                    &mut arena,
                )
            };
            let mut base_path = base_path;
            let new_pos = extract_common(
                &mut base_path,
                &old_tokens,
                &new_tokens,
                diagonal,
                &mut arena,
            );
            if base_path.old_pos + 1 >= old_len && new_pos + 1 >= new_len {
                return build_values(base_path.last, &arena, &old_tokens, &new_tokens);
            }
            if base_path.old_pos + 1 >= old_len {
                max_diagonal = max_diagonal.min(diagonal - 1);
            }
            if new_pos + 1 >= new_len {
                min_diagonal = min_diagonal.max(diagonal + 1);
            }
            best.insert(diagonal, base_path);
            diagonal += 2;
        }
        edit_length += 1;
    }

    // Unreachable: the edit distance never exceeds old+new.
    unreachable!("myers diff exceeded max edit length");
}

/// `Number.MAX_SAFE_INTEGER` — the infinite-context value the snapshot
/// `diffFull` patches pass (`structuredPatch(..., { context })`).
pub const MAX_SAFE_INTEGER: usize = 9007199254740991;

/// `structuredPatch` hunk (before the 0-line start fixup).
#[derive(Debug, Clone, Serialize)]
pub struct Hunk {
    pub old_start: i64,
    pub old_lines: i64,
    pub new_start: i64,
    pub new_lines: i64,
    pub lines: Vec<String>,
}

fn context_lines(lines: &[String]) -> Vec<String> {
    lines.iter().map(|line| format!(" {line}")).collect()
}

/// `structuredPatch` (patch/create.js): groups changes into unified-diff
/// hunks with `context` lines of surrounding context.
fn structured_hunks(diff: &[Change], context: usize) -> Vec<Hunk> {
    let mut hunks: Vec<Hunk> = Vec::new();
    let mut old_range_start: i64 = 0;
    let mut new_range_start: i64 = 0;
    let mut cur_range: Vec<String> = Vec::new();
    let mut old_line: i64 = 1;
    let mut new_line: i64 = 1;
    let n = diff.len();

    for (i, current) in diff.iter().enumerate() {
        let lines = &current.lines;
        if current.added || current.removed {
            if old_range_start == 0 {
                old_range_start = old_line;
                new_range_start = new_line;
                if i > 0 {
                    let prev = &diff[i - 1];
                    cur_range = if context > 0 {
                        context_lines(&prev.lines[prev.lines.len().saturating_sub(context)..])
                    } else {
                        Vec::new()
                    };
                    old_range_start -= cur_range.len() as i64;
                    new_range_start -= cur_range.len() as i64;
                }
            }
            let prefix = if current.added { '+' } else { '-' };
            for line in lines {
                cur_range.push(format!("{prefix}{line}"));
            }
            if current.added {
                new_line += lines.len() as i64;
            } else {
                old_line += lines.len() as i64;
            }
        } else {
            if old_range_start != 0 {
                if lines.len() <= context * 2 && i < n - 1 {
                    cur_range.extend(context_lines(lines));
                } else {
                    let context_size = lines.len().min(context);
                    cur_range.extend(context_lines(&lines[..context_size]));
                    hunks.push(Hunk {
                        old_start: old_range_start,
                        old_lines: old_line - old_range_start + context_size as i64,
                        new_start: new_range_start,
                        new_lines: new_line - new_range_start + context_size as i64,
                        lines: std::mem::take(&mut cur_range),
                    });
                    old_range_start = 0;
                    new_range_start = 0;
                    cur_range = Vec::new();
                }
            }
            // Identical context lines: both positions advance.
            old_line += lines.len() as i64;
            new_line += lines.len() as i64;
        }
    }

    // Sentinel `{ value: '', lines: [] }` appended by structuredPatch: close
    // any open hunk without trailing context.
    if old_range_start != 0 {
        hunks.push(Hunk {
            old_start: old_range_start,
            old_lines: old_line - old_range_start,
            new_start: new_range_start,
            new_lines: new_line - new_range_start,
            lines: std::mem::take(&mut cur_range),
        });
    }

    // Step 2: strip trailing newlines and mark missing final newlines.
    for hunk in &mut hunks {
        let mut i = 0;
        while i < hunk.lines.len() {
            if let Some(stripped) = hunk.lines[i].strip_suffix('\n') {
                hunk.lines[i] = stripped.to_string();
            } else {
                hunk.lines
                    .insert(i + 1, "\\ No newline at end of file".to_string());
                i += 1;
            }
            i += 1;
        }
    }

    hunks
}

/// `createTwoFilesPatch(fileName, fileName, old, new)` — both names are the
/// same file in every TS call site, so the `Index:` header is always emitted.
pub fn create_two_files_patch(name: &str, old: &str, new: &str) -> String {
    let diff = diff_lines(old, new);
    let hunks = structured_hunks(&diff, CONTEXT);

    let mut out: Vec<String> = Vec::new();
    out.push(format!("Index: {name}"));
    out.push("===================================================================".to_string());
    out.push(format!("--- {name}"));
    out.push(format!("+++ {name}"));
    push_hunks(hunks, &mut out);
    out.join("\n") + "\n"
}

/// `formatPatch(structuredPatch(name, name, old, new, "", "", { context }))`
/// (diff@8 patch/create.js). Unlike `createTwoFilesPatch` the headers are
/// the empty-string `oldHeader`/`newHeader`, so `formatPatch` appends the
/// `\t` separator for both file lines.
pub fn format_patch(name: &str, old: &str, new: &str, context: usize) -> String {
    let diff = diff_lines(old, new);
    let hunks = structured_hunks(&diff, context);

    let mut out: Vec<String> = Vec::new();
    out.push(format!("Index: {name}"));
    out.push("===================================================================".to_string());
    out.push(format!("--- {name}\t"));
    out.push(format!("+++ {name}\t"));
    push_hunks(hunks, &mut out);
    out.join("\n") + "\n"
}

/// The `formatPatch` hunk loop — 0-size ranges start one lower.
/// (`Index:` already pushed by the callers.)
fn push_hunks(hunks: Vec<Hunk>, out: &mut Vec<String>) {
    for mut hunk in hunks {
        if hunk.old_lines == 0 {
            hunk.old_start -= 1;
        }
        if hunk.new_lines == 0 {
            hunk.new_start -= 1;
        }
        out.push(format!(
            "@@ -{},{} +{},{} @@",
            hunk.old_start, hunk.old_lines, hunk.new_start, hunk.new_lines
        ));
        out.extend(hunk.lines);
    }
}

/// Counts of added / removed lines over the whole line diff — the sum over
/// the `diffLines` change blocks (edit.ts:177-180).
pub fn diff_line_counts(old: &str, new: &str) -> (usize, usize) {
    let mut additions = 0usize;
    let mut deletions = 0usize;
    for change in diff_lines(old, new) {
        if change.added {
            additions += change.count;
        }
        if change.removed {
            deletions += change.count;
        }
    }
    (additions, deletions)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    #[derive(serde::Deserialize)]
    struct Golden {
        name: String,
        old: String,
        new: String,
        patch: String,
    }

    fn goldens() -> Vec<Golden> {
        let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/golden/diff/npm-diff.json");
        let raw = std::fs::read_to_string(path).unwrap();
        serde_json::from_str(&raw).unwrap()
    }

    fn fuzz_goldens() -> Vec<Golden> {
        let path =
            Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/golden/diff/npm-diff-fuzz.json");
        let raw = std::fs::read_to_string(path).unwrap();
        serde_json::from_str(&raw).unwrap()
    }

    #[test]
    fn create_two_files_patch_matches_npm_goldens() {
        for (i, case) in goldens().iter().enumerate() {
            assert_eq!(
                create_two_files_patch(&case.name, &case.old, &case.new),
                case.patch,
                "golden case {i}"
            );
        }
    }

    #[test]
    fn create_two_files_patch_matches_npm_fuzz_goldens() {
        // Randomized fixtures captured from the real npm `diff` package
        // (deterministic seed) — protects the byte-parity contract (S1)
        // across hunk merges, "\ No newline" markers and `\r\n` lines.
        for (i, case) in fuzz_goldens().iter().enumerate() {
            assert_eq!(
                create_two_files_patch(&case.name, &case.old, &case.new),
                case.patch,
                "fuzz case {i}"
            );
        }
    }

    fn snapshot_goldens() -> Vec<Golden> {
        let path =
            Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/golden/diff/npm-diff-snapshot.json");
        let raw = std::fs::read_to_string(path).unwrap();
        serde_json::from_str(&raw).unwrap()
    }

    #[test]
    fn format_patch_infinite_context_matches_npm_goldens() {
        for (i, case) in snapshot_goldens().iter().enumerate() {
            assert_eq!(
                format_patch(&case.name, &case.old, &case.new, MAX_SAFE_INTEGER),
                case.patch,
                "snapshot golden case {i}"
            );
        }
    }

    #[test]
    fn diff_line_counts_matches_change_blocks() {
        // "line1" kept, "line2" -> "line4" (1+1), "line3" kept, "line5" added.
        assert_eq!(
            diff_line_counts("line1\nline2\nline3\n", "line1\nline4\nline3\nline5\n"),
            (2, 1)
        );
        assert_eq!(diff_line_counts("same\n", "same\n"), (0, 0));
        assert_eq!(diff_line_counts("", "a\nb\n"), (2, 0));
        assert_eq!(diff_line_counts("a\nb\n", ""), (0, 2));
    }

    #[test]
    fn identical_input_yields_headers_only() {
        assert_eq!(
            create_two_files_patch("/f", "same\n", "same\n"),
            "Index: /f\n===================================================================\n--- /f\n+++ /f\n"
        );
    }
}
