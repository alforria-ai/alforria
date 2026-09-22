//! OpenAI ApplyPatch parser — port of `src/patch/index.ts` (spec M4.7).
//!
//! The pure parsing and in-memory derivation; filesystem work lives in the
//! `apply_patch` tool.

use crate::tool::bom;

#[derive(Debug, Clone, PartialEq)]
pub enum Hunk {
    Add {
        path: String,
        contents: String,
    },
    Delete {
        path: String,
    },
    Update {
        path: String,
        move_path: Option<String>,
        chunks: Vec<UpdateFileChunk>,
    },
}

#[derive(Debug, Clone, PartialEq, Default)]
pub struct UpdateFileChunk {
    pub old_lines: Vec<String>,
    pub new_lines: Vec<String>,
    pub change_context: Option<String>,
    pub is_end_of_file: bool,
}

/// `parsePatchHeader` (patch/index.ts:70-101).
#[derive(Debug)]
struct PatchHeader {
    file_path: String,
    move_path: Option<String>,
    next_idx: usize,
}

fn parse_patch_header(lines: &[String], start_idx: usize) -> Option<PatchHeader> {
    let line = lines.get(start_idx)?;

    if let Some(rest) = line.strip_prefix("*** Add File:") {
        let file_path = rest.trim();
        if file_path.is_empty() {
            return None;
        }
        return Some(PatchHeader {
            file_path: file_path.to_string(),
            move_path: None,
            next_idx: start_idx + 1,
        });
    }

    if let Some(rest) = line.strip_prefix("*** Delete File:") {
        let file_path = rest.trim();
        if file_path.is_empty() {
            return None;
        }
        return Some(PatchHeader {
            file_path: file_path.to_string(),
            move_path: None,
            next_idx: start_idx + 1,
        });
    }

    if let Some(rest) = line.strip_prefix("*** Update File:") {
        let file_path = rest.trim();
        if file_path.is_empty() {
            return None;
        }
        let mut move_path = None;
        let mut next_idx = start_idx + 1;
        if let Some(next) = lines.get(next_idx) {
            if let Some(move_to) = next.strip_prefix("*** Move to:") {
                move_path = Some(move_to.trim().to_string());
                next_idx += 1;
            }
        }
        return Some(PatchHeader {
            file_path: file_path.to_string(),
            move_path,
            next_idx,
        });
    }

    None
}

/// `parseUpdateFileChunks` (patch/index.ts:103-155).
fn parse_update_file_chunks(lines: &[String], start_idx: usize) -> (Vec<UpdateFileChunk>, usize) {
    let mut chunks = Vec::new();
    let mut i = start_idx;

    while i < lines.len() && !lines[i].starts_with("***") {
        if lines[i].starts_with("@@") {
            let context_line = lines[i][2..].trim().to_string();
            i += 1;

            let mut old_lines: Vec<String> = Vec::new();
            let mut new_lines: Vec<String> = Vec::new();
            let mut is_end_of_file = false;

            while i < lines.len() && !lines[i].starts_with("@@") && !lines[i].starts_with("***") {
                let change_line = &lines[i];

                if change_line == "*** End of File" {
                    is_end_of_file = true;
                    i += 1;
                    break;
                }

                if let Some(rest) = change_line.strip_prefix(' ') {
                    old_lines.push(rest.to_string());
                    new_lines.push(rest.to_string());
                } else if let Some(rest) = change_line.strip_prefix('-') {
                    old_lines.push(rest.to_string());
                } else if let Some(rest) = change_line.strip_prefix('+') {
                    new_lines.push(rest.to_string());
                }

                i += 1;
            }

            chunks.push(UpdateFileChunk {
                old_lines,
                new_lines,
                change_context: if context_line.is_empty() {
                    None
                } else {
                    Some(context_line)
                },
                is_end_of_file,
            });
        } else {
            i += 1;
        }
    }

    (chunks, i)
}

/// `parseAddFileContent` (patch/index.ts:157-174).
fn parse_add_file_content(lines: &[String], start_idx: usize) -> (String, usize) {
    let mut content = String::new();
    let mut i = start_idx;

    while i < lines.len() && !lines[i].starts_with("***") {
        if let Some(rest) = lines[i].strip_prefix('+') {
            content.push_str(rest);
            content.push('\n');
        }
        i += 1;
    }

    if let Some(stripped) = content.strip_suffix('\n') {
        content = stripped.to_string();
    }

    (content, i)
}

/// `stripHeredoc` (patch/index.ts:176-183): `cat <<'EOF'\n...\nEOF` → body.
/// Hand-rolled because the TS regex uses a backreference (unsupported by
/// the `regex` crate.
fn strip_heredoc(input: &str) -> String {
    // ^(?:cat\s+)?<<['"]?(\w+)['"]?\s*\n([\s\S]*?)\n\1\s*$
    let s = input;

    // (?:cat\s+)?
    let rest = match s.strip_prefix("cat") {
        Some(after_cat) if !after_cat.trim_start().is_empty() => after_cat.trim_start(),
        _ => s,
    };

    // <<
    let Some(after_angle) = rest.strip_prefix("<<") else {
        return s.to_string();
    };

    // ['"]?
    let quote = match after_angle.chars().next() {
        Some(c @ ('\'' | '"')) => Some(c),
        _ => None,
    };
    let word_start = match quote {
        Some(q) => after_angle.strip_prefix(q).unwrap_or(after_angle),
        None => after_angle,
    };

    // (\w+)
    let word_end = word_start
        .find(|c: char| !(c.is_alphanumeric() || c == '_'))
        .unwrap_or(word_start.len());
    if word_end == 0 {
        return s.to_string();
    }
    let word = &word_start[..word_end];

    // ['"]?
    let rest = match quote {
        Some(q) => word_start[word_end..]
            .strip_prefix(q)
            .unwrap_or(&word_start[word_end..]),
        None => &word_start[word_end..],
    };

    // \s*\n — everything before the first newline must be whitespace.
    let Some(newline) = rest.find('\n') else {
        return s.to_string();
    };
    if !rest[..newline].chars().all(char::is_whitespace) {
        return s.to_string();
    }
    let body_all = &rest[newline + 1..];

    // ([\s\S]*?)\n\1\s*$ — shortest body ending at \n<word>\s*$.
    let needle = format!("\n{word}");
    let mut at = 0;
    loop {
        match body_all[at..].find(&needle) {
            Some(offset) => {
                let body_end = at + offset;
                let tail = &body_all[body_end + needle.len()..];
                if tail.trim().is_empty() {
                    return body_all[..body_end].to_string();
                }
                at = body_end + 1;
            }
            None => return s.to_string(),
        }
    }
}

/// `parsePatch` (patch/index.ts:185-241): parse the OpenAI ApplyPatch
/// grammar into hunks. Errors with the exact TS messages.
pub fn parse_patch(patch_text: &str) -> Result<Vec<Hunk>, String> {
    let cleaned = strip_heredoc(patch_text.trim());
    let lines: Vec<String> = cleaned.split('\n').map(str::to_string).collect();
    let mut hunks = Vec::new();

    let begin_marker = "*** Begin Patch";
    let end_marker = "*** End Patch";

    let begin_idx = lines.iter().position(|line| line.trim() == begin_marker);
    let end_idx = lines.iter().position(|line| line.trim() == end_marker);

    let (begin_idx, end_idx) = match (begin_idx, end_idx) {
        (Some(begin), Some(end)) if begin < end => (begin, end),
        _ => {
            return Err("Invalid patch format: missing Begin/End markers".to_string());
        }
    };

    let mut i = begin_idx + 1;

    while i < end_idx {
        let Some(header) = parse_patch_header(&lines, i) else {
            i += 1;
            continue;
        };

        if lines[i].starts_with("*** Add File:") {
            let (contents, next_idx) = parse_add_file_content(&lines, header.next_idx);
            hunks.push(Hunk::Add {
                path: header.file_path,
                contents,
            });
            i = next_idx;
        } else if lines[i].starts_with("*** Delete File:") {
            hunks.push(Hunk::Delete {
                path: header.file_path,
            });
            i = header.next_idx;
        } else if lines[i].starts_with("*** Update File:") {
            let (chunks, next_idx) = parse_update_file_chunks(&lines, header.next_idx);
            hunks.push(Hunk::Update {
                path: header.file_path,
                move_path: header.move_path,
                chunks,
            });
            i = next_idx;
        } else {
            i += 1;
        }
    }

    Ok(hunks)
}

/// Result of [`derive_new_contents_from_chunks`].
#[derive(Debug, Clone, PartialEq)]
pub struct FileUpdate {
    pub unified_diff: String,
    pub content: String,
    pub bom: bool,
}

/// `deriveNewContentsFromChunks` (patch/index.ts:307-340): apply update
/// chunks to `original_text` and produce new content + unified diff.
pub fn derive_new_contents_from_chunks(
    file_path: &str,
    chunks: &[UpdateFileChunk],
    original_text: &str,
) -> Result<FileUpdate, String> {
    let original_content = bom::split(original_text);

    let mut original_lines: Vec<String> = original_content
        .text
        .split('\n')
        .map(str::to_string)
        .collect();

    // Drop trailing empty element for consistent line counting.
    if original_lines.last().is_some_and(|last| last.is_empty()) {
        original_lines.pop();
    }

    let replacements = compute_replacements(original_lines.clone(), file_path, chunks)?;
    let mut new_lines = apply_replacements(original_lines, &replacements);

    // Ensure trailing newline.
    if new_lines.is_empty() || !new_lines[new_lines.len() - 1].is_empty() {
        new_lines.push(String::new());
    }

    let joined = new_lines.join("\n");
    let next = bom::split(&joined);
    let new_content = next.text.clone();

    let unified_diff = generate_unified_diff(&original_content.text, &new_content);

    Ok(FileUpdate {
        unified_diff,
        content: new_content,
        bom: original_content.bom || next.bom,
    })
}

/// `computeReplacements` (patch/index.ts:342-396).
fn compute_replacements(
    original_lines: Vec<String>,
    file_path: &str,
    chunks: &[UpdateFileChunk],
) -> Result<Vec<(usize, usize, Vec<String>)>, String> {
    let mut replacements: Vec<(usize, usize, Vec<String>)> = Vec::new();
    let mut line_index: usize = 0;

    for chunk in chunks {
        // Handle context-based seeking.
        if let Some(change_context) = &chunk.change_context {
            let context_idx = seek_sequence(
                &original_lines,
                std::slice::from_ref(change_context),
                line_index,
                false,
            );
            match context_idx {
                Some(idx) => line_index = idx + 1,
                None => {
                    return Err(format!(
                        "Failed to find context '{change_context}' in {file_path}"
                    ));
                }
            }
        }

        // Handle pure addition (no old lines).
        if chunk.old_lines.is_empty() {
            let insertion_idx = if !original_lines.is_empty()
                && original_lines[original_lines.len() - 1].is_empty()
            {
                original_lines.len() - 1
            } else {
                original_lines.len()
            };
            replacements.push((insertion_idx, 0, chunk.new_lines.clone()));
            continue;
        }

        // Try to match old lines in the file.
        let mut pattern: Vec<String> = chunk.old_lines.clone();
        let mut new_slice: Vec<String> = chunk.new_lines.clone();
        let mut found = seek_sequence(&original_lines, &pattern, line_index, chunk.is_end_of_file);

        // Retry without trailing empty line if not found.
        if found.is_none() && !pattern.is_empty() && pattern[pattern.len() - 1].is_empty() {
            pattern.pop();
            if !new_slice.is_empty() && new_slice[new_slice.len() - 1].is_empty() {
                new_slice.pop();
            }
            found = seek_sequence(&original_lines, &pattern, line_index, chunk.is_end_of_file);
        }

        match found {
            Some(at) => {
                replacements.push((at, pattern.len(), new_slice));
                line_index = at + pattern.len();
            }
            None => {
                return Err(format!(
                    "Failed to find expected lines in {file_path}:\n{}",
                    chunk.old_lines.join("\n")
                ));
            }
        }
    }

    replacements.sort_by_key(|(start, _, _)| *start);
    Ok(replacements)
}

/// `applyReplacements` (patch/index.ts:398-415) — in reverse order.
fn apply_replacements(
    lines: Vec<String>,
    replacements: &[(usize, usize, Vec<String>)],
) -> Vec<String> {
    let mut result = lines;
    for (start_idx, old_len, new_segment) in replacements.iter().rev() {
        result.splice(*start_idx..start_idx + old_len, std::iter::empty());
        for (j, line) in new_segment.iter().enumerate() {
            result.insert(start_idx + j, line.clone());
        }
    }
    result
}

/// `normalizeUnicode` (patch/index.ts:418-425): smart punctuation → ASCII.
fn normalize_unicode(text: &str) -> String {
    text.replace(['\u{2018}', '\u{2019}', '\u{201a}', '\u{201b}'], "'")
        .replace(['\u{201c}', '\u{201d}', '\u{201e}', '\u{201f}'], "\"")
        .replace(
            [
                '\u{2010}', '\u{2011}', '\u{2012}', '\u{2013}', '\u{2014}', '\u{2015}',
            ],
            "-",
        )
        .replace('\u{2026}', "...")
        .replace('\u{a0}', " ")
}

type Comparator = fn(&str, &str) -> bool;

/// `tryMatch` (patch/index.ts:429-458).
fn try_match(
    lines: &[String],
    pattern: &[String],
    start_index: usize,
    compare: Comparator,
    eof: bool,
) -> Option<usize> {
    // If EOF anchor, try matching from end of file first.
    if eof && lines.len() >= pattern.len() {
        let from_end = lines.len() - pattern.len();
        if from_end >= start_index
            && pattern
                .iter()
                .enumerate()
                .all(|(j, line)| compare(&lines[from_end + j], line))
        {
            return Some(from_end);
        }
    }

    // Forward search from startIndex.
    if pattern.is_empty() || lines.len() < pattern.len() {
        return None;
    }
    let mut i = start_index;
    while i + pattern.len() <= lines.len() {
        let matches = pattern
            .iter()
            .enumerate()
            .all(|(j, line)| compare(&lines[i + j], line));
        if matches {
            return Some(i);
        }
        i += 1;
    }
    None
}

/// `seekSequence` (patch/index.ts:460-484): four passes — exact, rstrip,
/// trim, normalized.
fn seek_sequence(
    lines: &[String],
    pattern: &[String],
    start_index: usize,
    eof: bool,
) -> Option<usize> {
    if pattern.is_empty() {
        return None;
    }

    // Pass 1: exact match.
    if let Some(at) = try_match(lines, pattern, start_index, |a, b| a == b, eof) {
        return Some(at);
    }

    // Pass 2: rstrip (trim trailing whitespace).
    if let Some(at) = try_match(
        lines,
        pattern,
        start_index,
        |a, b| a.trim_end() == b.trim_end(),
        eof,
    ) {
        return Some(at);
    }

    // Pass 3: trim (both ends).
    if let Some(at) = try_match(
        lines,
        pattern,
        start_index,
        |a, b| a.trim() == b.trim(),
        eof,
    ) {
        return Some(at);
    }

    // Pass 4: normalized (Unicode punctuation to ASCII).
    try_match(
        lines,
        pattern,
        start_index,
        |a, b| normalize_unicode(a.trim()) == normalize_unicode(b.trim()),
        eof,
    )
}

/// `generateUnifiedDiff` (patch/index.ts:486-511).
fn generate_unified_diff(old_content: &str, new_content: &str) -> String {
    let old_lines: Vec<&str> = old_content.split('\n').collect();
    let new_lines: Vec<&str> = new_content.split('\n').collect();

    let mut diff = "@@ -1 +1 @@\n".to_string();

    let max_len = old_lines.len().max(new_lines.len());
    let mut has_changes = false;

    for i in 0..max_len {
        let old_line = old_lines.get(i).copied().unwrap_or("");
        let new_line = new_lines.get(i).copied().unwrap_or("");

        if old_line != new_line {
            if !old_line.is_empty() {
                diff.push_str(&format!("-{old_line}\n"));
            }
            if !new_line.is_empty() {
                diff.push_str(&format!("+{new_line}\n"));
            }
            has_changes = true;
        } else if !old_line.is_empty() {
            diff.push_str(&format!(" {old_line}\n"));
        }
    }

    if has_changes {
        diff
    } else {
        String::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_add_file() {
        let patch = "*** Begin Patch\n*** Add File: a.txt\n+hello\n+world\n*** End Patch";
        let hunks = parse_patch(patch).unwrap();
        assert_eq!(
            hunks,
            vec![Hunk::Add {
                path: "a.txt".to_string(),
                contents: "hello\nworld".to_string(),
            }]
        );
    }

    #[test]
    fn parse_delete_file() {
        let patch = "*** Begin Patch\n*** Delete File: a.txt\n*** End Patch";
        assert_eq!(
            parse_patch(patch).unwrap(),
            vec![Hunk::Delete {
                path: "a.txt".to_string()
            }]
        );
    }

    #[test]
    fn parse_update_file_with_move() {
        let patch = concat!(
            "*** Begin Patch\n",
            "*** Update File: a.txt\n",
            "*** Move to: b.txt\n",
            "@@\n",
            " context\n",
            "-old\n",
            "+new\n",
            "*** End Patch",
        );
        let hunks = parse_patch(patch).unwrap();
        assert_eq!(
            hunks,
            vec![Hunk::Update {
                path: "a.txt".to_string(),
                move_path: Some("b.txt".to_string()),
                chunks: vec![UpdateFileChunk {
                    old_lines: vec!["context".to_string(), "old".to_string()],
                    new_lines: vec!["context".to_string(), "new".to_string()],
                    change_context: None,
                    is_end_of_file: false,
                }],
            }]
        );
    }

    #[test]
    fn parse_end_of_file_marker() {
        // NOTE: in the pinned TS the `*** End of File` branch is dead code:
        // the inner while loop exits on startsWith("***") before the marker
        // check, so `is_end_of_file` is never set. The port preserves this.
        let patch = concat!(
            "*** Begin Patch\n",
            "*** Update File: a.txt\n",
            "@@\n",
            " keep\n",
            "*** End of File\n",
            "*** End Patch",
        );
        let hunks = parse_patch(patch).unwrap();
        let Hunk::Update { chunks, .. } = &hunks[0] else {
            panic!("expected update");
        };
        assert!(!chunks[0].is_end_of_file);
    }

    #[test]
    fn parse_change_context() {
        let patch = concat!(
            "*** Begin Patch\n",
            "*** Update File: a.txt\n",
            "@@ around here\n",
            "-old\n",
            "+new\n",
            "*** End Patch",
        );
        let hunks = parse_patch(patch).unwrap();
        let Hunk::Update { chunks, .. } = &hunks[0] else {
            panic!("expected update");
        };
        assert_eq!(chunks[0].change_context, Some("around here".to_string()));
    }

    #[test]
    fn missing_markers_error() {
        let err = parse_patch("no patch here").unwrap_err();
        assert_eq!(
            err,
            "Invalid patch format: missing Begin/End markers".to_string()
        );
    }

    #[test]
    fn empty_patch_yields_no_hunks() {
        let hunks = parse_patch("*** Begin Patch\n*** End Patch").unwrap();
        assert!(hunks.is_empty());
    }

    #[test]
    fn heredoc_is_stripped() {
        let patch = "cat <<'EOF'\n*** Begin Patch\n*** Add File: a.txt\n+x\n*** End Patch\nEOF";
        let hunks = parse_patch(patch).unwrap();
        assert_eq!(
            hunks,
            vec![Hunk::Add {
                path: "a.txt".to_string(),
                contents: "x".to_string(),
            }]
        );
    }

    #[test]
    fn derive_simple_update() {
        let update = derive_new_contents_from_chunks(
            "a.txt",
            &[UpdateFileChunk {
                old_lines: vec!["two".to_string()],
                new_lines: vec!["TWO".to_string()],
                change_context: None,
                is_end_of_file: false,
            }],
            "one\ntwo\nthree\n",
        )
        .unwrap();
        assert_eq!(update.content, "one\nTWO\nthree\n");
        assert!(update.unified_diff.contains("-two"));
        assert!(update.unified_diff.contains("+TWO"));
    }

    #[test]
    fn derive_missing_lines_error() {
        let err = derive_new_contents_from_chunks(
            "a.txt",
            &[UpdateFileChunk {
                old_lines: vec!["missing".to_string()],
                new_lines: vec!["x".to_string()],
                change_context: None,
                is_end_of_file: false,
            }],
            "one\ntwo\n",
        )
        .unwrap_err();
        assert!(
            err.starts_with("Failed to find expected lines in a.txt:"),
            "{err}"
        );
    }

    #[test]
    fn derive_context_seek_error() {
        let err = derive_new_contents_from_chunks(
            "a.txt",
            &[UpdateFileChunk {
                old_lines: vec!["one".to_string()],
                new_lines: vec!["x".to_string()],
                change_context: Some("nope".to_string()),
                is_end_of_file: false,
            }],
            "one\ntwo\n",
        )
        .unwrap_err();
        assert_eq!(err, "Failed to find context 'nope' in a.txt");
    }

    #[test]
    fn derive_whitespace_fallback_matching() {
        // Trailing-whitespace mismatch falls back to trim matching.
        let update = derive_new_contents_from_chunks(
            "a.txt",
            &[UpdateFileChunk {
                old_lines: vec!["two".to_string()],
                new_lines: vec!["TWO".to_string()],
                change_context: None,
                is_end_of_file: false,
            }],
            "one\ntwo   \nthree\n",
        )
        .unwrap();
        assert_eq!(update.content, "one\nTWO\nthree\n");
    }

    #[test]
    fn derive_pure_addition_appends() {
        let update = derive_new_contents_from_chunks(
            "a.txt",
            &[UpdateFileChunk {
                old_lines: vec![],
                new_lines: vec!["one".to_string(), "two".to_string()],
                change_context: None,
                is_end_of_file: false,
            }],
            "line\n",
        )
        .unwrap();
        assert_eq!(update.content, "line\none\ntwo\n");
    }

    #[test]
    fn derive_end_of_file_anchor() {
        // Two identical "one" lines; the EOF anchor matches the last.
        let update = derive_new_contents_from_chunks(
            "a.txt",
            &[UpdateFileChunk {
                old_lines: vec!["one".to_string()],
                new_lines: vec!["uno".to_string()],
                change_context: None,
                is_end_of_file: true,
            }],
            "one\none\none\n",
        )
        .unwrap();
        assert_eq!(update.content, "one\none\nuno\n");
    }

    #[test]
    fn derive_unicode_normalization() {
        let update = derive_new_contents_from_chunks(
            "a.txt",
            &[UpdateFileChunk {
                old_lines: vec!["it's".to_string()],
                new_lines: vec!["ok".to_string()],
                change_context: None,
                is_end_of_file: false,
            }],
            "it\u{2019}s\n",
        )
        .unwrap();
        assert_eq!(update.content, "ok\n");
    }

    #[test]
    fn derive_bom_preserved() {
        let update = derive_new_contents_from_chunks(
            "a.txt",
            &[UpdateFileChunk {
                old_lines: vec!["two".to_string()],
                new_lines: vec!["TWO".to_string()],
                change_context: None,
                is_end_of_file: false,
            }],
            "one\ntwo\n",
        )
        .unwrap();
        assert!(!update.bom);
    }
}
