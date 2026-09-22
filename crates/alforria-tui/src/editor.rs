//! `editor.rs` — the `$EDITOR` bridge + prompt content normalization
//! (M8.6): `editor.ts` (`normalizePromptContent`, `openEditor`). The
//! IDE selection bridge is §6 N3 — the seam always returns `None`.

use std::path::Path;
use std::process::Command;

/// `normalizePromptContent` (`editor.ts:9-21`): strip one trailing
/// newline when the body has no others.
pub fn normalize_prompt_content(content: &str) -> String {
    if let Some(body) = content.strip_suffix("\r\n") {
        if !body.contains('\n') && !body.contains('\r') {
            return body.to_string();
        }
        return content.to_string();
    }
    if let Some(body) = content.strip_suffix('\n') {
        if !body.contains('\n') && !body.contains('\r') {
            return body.to_string();
        }
        return content.to_string();
    }
    content.to_string()
}

/// `openEditor` (`editor.ts:23-51`): write the value to a temp file,
/// suspend the terminal, spawn `$VISUAL || $EDITOR`, read the result.
/// `editor` is the command string (split on spaces) — a parameter so
/// tests can drive a fake editor.
pub fn open_editor_with(value: &str, editor: &str, cwd: Option<&Path>) -> Option<String> {
    if editor.trim().is_empty() {
        return None;
    }
    static COUNTER: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let directory = std::env::temp_dir();
    let unique = COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let file = directory.join(format!("{}-{unique}.md", millis()));
    std::fs::write(&file, value).ok()?;
    let result = run_editor(editor, &file, cwd);
    let content = std::fs::read_to_string(&file).ok();
    let _ = std::fs::remove_file(&file);
    match result {
        Ok(()) => content.filter(|content| !content.is_empty()).or(None),
        Err(_) => None,
    }
}

/// The production entry: `$VISUAL || $EDITOR`.
pub fn open_editor(value: &str, cwd: Option<&Path>) -> Option<String> {
    let editor = std::env::var("VISUAL")
        .or_else(|_| std::env::var("EDITOR"))
        .ok()?;
    open_editor_with(value, &editor, cwd)
}

fn run_editor(editor: &str, file: &Path, cwd: Option<&Path>) -> Result<(), String> {
    let mut parts = editor.split(' ');
    let program = parts.next().unwrap_or_default();
    if program.is_empty() {
        return Err("empty editor".to_string());
    }
    let cwd = cwd.filter(|cwd| cwd.exists());
    let status = Command::new(program)
        .args(parts.collect::<Vec<_>>())
        .arg(file)
        .current_dir(cwd.unwrap_or_else(|| Path::new(".")))
        .status();
    match status {
        Ok(status) if status.success() => Ok(()),
        Ok(status) => Err(format!("Editor exited with code {status}")),
        Err(error) => Err(error.to_string()),
    }
}

fn millis() -> u128 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalize_strips_one_trailing_newline() {
        assert_eq!(normalize_prompt_content("single line\n"), "single line");
        assert_eq!(normalize_prompt_content("single line\r\n"), "single line");
        assert_eq!(
            normalize_prompt_content("multi\nline\n"),
            "multi\nline\n",
            "body has other newlines"
        );
        assert_eq!(normalize_prompt_content("no newline"), "no newline");
    }

    #[test]
    fn editor_round_trip_with_a_fake_editor() {
        // The fake editor is `sed -i` — it rewrites the seeded content
        // deterministically. `openEditor` returns the raw file contents;
        // callers apply `normalize_prompt_content`.
        let editor = "sed -i s/old/new/g";
        let content = open_editor_with("the old text\n", editor, None).expect("editor ran");
        assert_eq!(normalize_prompt_content(&content), "the new text");
    }

    #[test]
    fn missing_editor_returns_none() {
        assert!(open_editor_with("x", "", None).is_none());
    }

    #[test]
    fn failing_editor_returns_none() {
        let editor = "false";
        assert_eq!(open_editor_with("x", editor, None), None);
    }
}
