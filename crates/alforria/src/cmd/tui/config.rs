//! The TUI config files (`config/tui.ts` + `config/tui-migrate.ts`).
//!
//! Precedence, lowest first: the global `tui.json[c]`, the
//! `OPENCODE_TUI_CONFIG` file, the project `tui.json[c]` files (root first,
//! so the closest wins), then each `.opencode` directory (and
//! `OPENCODE_CONFIG_DIR`). Before reading, legacy `theme` / `keybinds` /
//! `tui` keys move out of every `opencode.json[c]` into a sibling
//! `tui.json` (the original is backed up first). A file that does not
//! parse or validate is skipped with a warning, never fatal.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use serde_json::{Map, Value};

use alforria_core::GlobalPaths;
use alforria_tui::config::{validate_info, TuiConfig};

/// `TUI_SCHEMA_URL` (`tui-migrate.ts`).
const TUI_SCHEMA_URL: &str = "https://opencode.ai/tui.json";

/// The flags `config/tui.ts` reads (`Flag.*`), injectable for tests.
#[derive(Debug, Clone, Default)]
pub struct Flags {
    pub disable_project_config: bool,
    pub tui_config: Option<PathBuf>,
    pub config_dir: Option<PathBuf>,
    pub opencode_config: Option<PathBuf>,
}

impl Flags {
    pub fn from_env() -> Flags {
        let path = |key: &str| {
            std::env::var_os(key)
                .filter(|value| !value.is_empty())
                .map(PathBuf::from)
        };
        let truthy = |key: &str| {
            std::env::var(key)
                .map(|value| matches!(value.to_ascii_lowercase().as_str(), "true" | "1"))
                .unwrap_or(false)
        };
        Flags {
            disable_project_config: truthy("OPENCODE_DISABLE_PROJECT_CONFIG"),
            tui_config: path("OPENCODE_TUI_CONFIG"),
            config_dir: path("OPENCODE_CONFIG_DIR"),
            opencode_config: path("OPENCODE_CONFIG"),
        }
    }
}

/// `TuiConfig.get()` for the TUI started in `cwd`.
pub fn load(cwd: &Path, paths: &GlobalPaths, flags: &Flags) -> TuiConfig {
    let directories = directories(cwd, paths, flags);
    migrate(cwd, paths, flags, &directories);

    let mut files: Vec<PathBuf> = file_in_directory(&paths.config, "tui");
    if let Some(file) = &flags.tui_config {
        files.push(file.clone());
    }
    if !flags.disable_project_config {
        // `ConfigPaths.files("tui", cwd)`: `up` from cwd, reversed.
        let mut project = up(&["tui.jsonc", "tui.json"], cwd, None);
        project.reverse();
        files.extend(project);
    }
    for dir in &directories {
        let is_dir_source = dir.file_name().is_some_and(|name| name == ".opencode")
            || flags.config_dir.as_deref() == Some(dir.as_path());
        if is_dir_source {
            files.extend(file_in_directory(dir, "tui"));
        }
    }

    let mut merged = Value::Object(Map::new());
    for file in files {
        let data = load_file(&file);
        alforria_core::merge_deep(&mut merged, &data);
    }
    TuiConfig::from_info(&merged, cfg!(not(target_os = "windows")))
}

/// `ConfigPaths.fileInDirectory(dir, name)`.
fn file_in_directory(dir: &Path, name: &str) -> Vec<PathBuf> {
    vec![
        dir.join(format!("{name}.json")),
        dir.join(format!("{name}.jsonc")),
    ]
}

/// `FSUtil.up`: existing `targets`, from `start` up to `stop` (or `/`).
fn up(targets: &[&str], start: &Path, stop: Option<&Path>) -> Vec<PathBuf> {
    let mut found = Vec::new();
    let mut current = Some(start);
    while let Some(dir) = current {
        for target in targets {
            let candidate = dir.join(target);
            if candidate.exists() {
                found.push(candidate);
            }
        }
        if Some(dir) == stop {
            break;
        }
        current = dir.parent();
    }
    found
}

fn unique(paths: Vec<PathBuf>) -> Vec<PathBuf> {
    let mut seen = Vec::new();
    for path in paths {
        if !seen.contains(&path) {
            seen.push(path);
        }
    }
    seen
}

/// `ConfigPaths.directories(cwd)`.
fn directories(cwd: &Path, paths: &GlobalPaths, flags: &Flags) -> Vec<PathBuf> {
    let mut dirs = vec![paths.config.clone()];
    if !flags.disable_project_config {
        dirs.extend(up(&[".opencode"], cwd, None));
    }
    dirs.extend(up(&[".opencode"], &paths.home, Some(&paths.home)));
    if let Some(dir) = &flags.config_dir {
        dirs.push(dir.clone());
    }
    unique(dirs)
}

/// `normalize` (`config/tui.ts`): a nested `"tui"` object is flattened
/// into the top level (users mirroring the old `opencode.json` shape).
fn normalize(mut data: Map<String, Value>) -> Map<String, Value> {
    let Some(tui) = data.remove("tui") else {
        return data;
    };
    let Value::Object(tui) = tui else {
        return data;
    };
    let mut out = tui;
    for (key, value) in data {
        out.insert(key, value);
    }
    out
}

/// `loadFile` + `load`: read, `{env:}`/`{file:}` substitution, JSONC,
/// normalize, validate. Every failure is a warning and `{}`.
fn load_file(file: &Path) -> Value {
    let empty = Value::Object(Map::new());
    let text = match std::fs::read_to_string(file) {
        Ok(text) => text,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return empty,
        Err(error) => {
            alforria_tui::log::warn(&format!(
                "failed to read tui config {}: {error}",
                file.display()
            ));
            return empty;
        }
    };
    if text.trim().is_empty() {
        return empty;
    }
    let parsed = alforria_core::substitute(
        &text,
        &alforria_core::Source::Path(file.to_path_buf()),
        &HashMap::new(),
        alforria_core::Missing::Empty,
    )
    .map_err(|error| error.to_string())
    .and_then(|text| alforria_core::parse_jsonc(&text, file).map_err(|error| error.to_string()));
    let data = match parsed {
        Ok(Value::Object(data)) => Value::Object(normalize(data)),
        Ok(_) => return empty,
        Err(error) => {
            alforria_tui::log::warn(&format!(
                "skipping invalid tui config {}: {error}",
                file.display()
            ));
            return empty;
        }
    };
    if let Err(error) = validate_info(&data) {
        alforria_tui::log::warn(&format!(
            "skipping invalid tui config {}: {error}",
            file.display()
        ));
        return empty;
    }
    data
}

// ------------------------------------------------------------ migration

/// `opencodeFiles` (`tui-migrate.ts`).
fn opencode_files(
    cwd: &Path,
    paths: &GlobalPaths,
    flags: &Flags,
    dirs: &[PathBuf],
) -> Vec<PathBuf> {
    let mut files = file_in_directory(&paths.config, "opencode");
    let mut found = up(&["opencode.json", "opencode.jsonc"], cwd, None);
    // `findUp(…, { rootFirst: true })`.
    found.reverse();
    files.extend(found);
    for dir in dirs {
        files.extend(file_in_directory(dir, "opencode"));
    }
    if let Some(file) = &flags.opencode_config {
        files.push(file.clone());
    }
    unique(files)
        .into_iter()
        .filter(|file| file.exists())
        .collect()
}

/// `normalizeTui`: only the three keys that moved, each kept if valid.
fn normalize_tui(tui: &Map<String, Value>) -> Vec<(&'static str, Value)> {
    let mut out = Vec::new();
    if let Some(speed) = tui
        .get("scroll_speed")
        .filter(|v| v.as_f64().is_some_and(|n| n >= 0.001))
    {
        out.push(("scroll_speed", speed.clone()));
    }
    if let Some(acceleration) = tui
        .get("scroll_acceleration")
        .filter(|v| v.get("enabled").is_some_and(Value::is_boolean))
    {
        out.push((
            "scroll_acceleration",
            serde_json::json!({ "enabled": acceleration["enabled"] }),
        ));
    }
    if let Some(style) = tui
        .get("diff_style")
        .filter(|v| matches!(v.as_str(), Some("auto" | "stacked")))
    {
        out.push(("diff_style", style.clone()));
    }
    out
}

/// `migrateTuiConfig`: per `opencode.json[c]`, move `theme`, `keybinds`
/// and the legacy `tui` block into a sibling `tui.json` unless one exists.
fn migrate(cwd: &Path, paths: &GlobalPaths, flags: &Flags, dirs: &[PathBuf]) {
    for file in opencode_files(cwd, paths, flags, dirs) {
        let Ok(source) = std::fs::read_to_string(&file) else {
            continue;
        };
        if source.is_empty() {
            continue;
        }
        let Ok(Value::Object(data)) = alforria_core::parse_jsonc(&source, &file) else {
            continue;
        };
        let theme = data.get("theme").filter(|v| v.is_string()).cloned();
        let keybinds = data.get("keybinds").filter(|v| v.is_object()).cloned();
        let tui = data
            .get("tui")
            .and_then(Value::as_object)
            .map(normalize_tui)
            .filter(|tui| !tui.is_empty());
        if theme.is_none() && keybinds.is_none() && tui.is_none() {
            continue;
        }
        let Some(dir) = file.parent() else {
            continue;
        };
        let target = dir.join("tui.json");
        if target.exists() {
            continue;
        }
        // `JSON.stringify(payload, null, 2)`, in TS's key order.
        let mut members: Vec<(&str, Value)> =
            vec![("$schema", Value::String(TUI_SCHEMA_URL.to_string()))];
        if let Some(theme) = theme {
            members.push(("theme", theme));
        }
        if let Some(keybinds) = keybinds {
            members.push(("keybinds", keybinds));
        }
        members.extend(tui.unwrap_or_default());
        if std::fs::write(&target, pretty_object(&members)).is_err() {
            continue;
        }
        backup_and_strip_legacy(&file, &source);
    }
}

/// A JSON object pretty-printed with two spaces, members in the given
/// order (serde_json's map would sort them).
fn pretty_object(members: &[(&str, Value)]) -> String {
    let body = members
        .iter()
        .map(|(key, value)| {
            let value = serde_json::to_string_pretty(value)
                .unwrap_or_default()
                .replace('\n', "\n  ");
            format!("  {}: {value}", Value::String(key.to_string()))
        })
        .collect::<Vec<_>>()
        .join(",\n");
    format!("{{\n{body}\n}}")
}

/// `backupAndStripLegacy`: keep the original as `*.tui-migration.bak`,
/// then remove the three keys from the file, comments and layout intact.
fn backup_and_strip_legacy(file: &Path, source: &str) {
    let backup = PathBuf::from(format!("{}.tui-migration.bak", file.display()));
    if !backup.exists() && std::fs::write(&backup, source).is_err() {
        return;
    }
    let mut text = source.to_string();
    for key in ["theme", "keybinds", "tui"] {
        if let Some(next) = remove_top_level_property(&text, key) {
            text = next;
        }
    }
    let _ = std::fs::write(file, text);
}

/// Skip whitespace and JSONC comments from `index`.
fn skip_trivia(bytes: &[u8], mut index: usize) -> usize {
    loop {
        while index < bytes.len() && bytes[index].is_ascii_whitespace() {
            index += 1;
        }
        if bytes[index..].starts_with(b"//") {
            while index < bytes.len() && bytes[index] != b'\n' {
                index += 1;
            }
        } else if bytes[index..].starts_with(b"/*") {
            index += 2;
            while index < bytes.len() && !bytes[index..].starts_with(b"*/") {
                index += 1;
            }
            index = (index + 2).min(bytes.len());
        } else {
            return index;
        }
    }
}

/// The end of the string literal starting at `index` (one past `"`).
fn string_end(bytes: &[u8], mut index: usize) -> usize {
    index += 1;
    while index < bytes.len() {
        match bytes[index] {
            b'\\' => index += 2,
            b'"' => return index + 1,
            _ => index += 1,
        }
    }
    bytes.len()
}

/// The end of the JSON value starting at `index`.
fn value_end(bytes: &[u8], index: usize) -> usize {
    let mut depth = 0usize;
    let mut i = index;
    while i < bytes.len() {
        match bytes[i] {
            b'"' => {
                i = string_end(bytes, i);
                if depth == 0 {
                    return i;
                }
                continue;
            }
            b'/' if bytes[i..].starts_with(b"//") || bytes[i..].starts_with(b"/*") => {
                if depth == 0 {
                    return i;
                }
                i = skip_trivia(bytes, i);
                continue;
            }
            b'{' | b'[' => depth += 1,
            b'}' | b']' => {
                if depth == 0 {
                    return i;
                }
                depth -= 1;
                if depth == 0 {
                    return i + 1;
                }
            }
            b',' if depth == 0 => return i,
            byte if depth == 0 && byte.is_ascii_whitespace() => return i,
            _ => {}
        }
        i += 1;
    }
    i
}

/// jsonc-parser `modify(text, [key], undefined)`: drop one top-level
/// property and its separating comma.
fn remove_top_level_property(text: &str, key: &str) -> Option<String> {
    let bytes = text.as_bytes();
    let mut index = skip_trivia(bytes, 0);
    if bytes.get(index) != Some(&b'{') {
        return None;
    }
    index += 1;
    // The end of the previous member's value (for a trailing removal).
    let mut previous_end: Option<usize> = None;
    loop {
        index = skip_trivia(bytes, index);
        if index >= bytes.len() || bytes[index] != b'"' {
            return None;
        }
        let name_start = index;
        let name_end = string_end(bytes, index);
        let name: String = serde_json::from_str(&text[name_start..name_end]).ok()?;
        index = skip_trivia(bytes, name_end);
        if bytes.get(index) != Some(&b':') {
            return None;
        }
        index = skip_trivia(bytes, index + 1);
        let end = value_end(bytes, index);
        let after = skip_trivia(bytes, end);
        let has_comma = bytes.get(after) == Some(&b',');
        if name == key {
            let removed = if has_comma {
                // Up to the next member, so its indentation takes our place.
                let next = skip_trivia(bytes, after + 1);
                let next_line = text[..next].rfind('\n').map_or(next, |n| n + 1);
                let start_line = text[..name_start].rfind('\n').map_or(name_start, |n| n + 1);
                if next_line > start_line {
                    format!("{}{}", &text[..start_line], &text[next_line..])
                } else {
                    format!("{}{}", &text[..name_start], &text[next..])
                }
            } else {
                match previous_end {
                    // The last member: also drop the comma before it.
                    Some(previous) => format!("{}{}", &text[..previous], &text[end..]),
                    None => format!("{}{}", &text[..name_start], &text[end..]),
                }
            };
            return Some(removed);
        }
        if !has_comma {
            return None;
        }
        previous_end = Some(end);
        index = after + 1;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn paths(home: &Path) -> GlobalPaths {
        GlobalPaths::resolve_with(home.to_path_buf(), |_| None)
    }

    #[test]
    fn strips_legacy_keys_and_keeps_the_rest() {
        let source = "{\n  // keep me\n  \"theme\": \"nord\",\n  \"model\": \"a/b\",\n  \"tui\": { \"scroll_speed\": 2 }\n}\n";
        let once = remove_top_level_property(source, "theme").unwrap();
        assert_eq!(
            once,
            "{\n  // keep me\n  \"model\": \"a/b\",\n  \"tui\": { \"scroll_speed\": 2 }\n}\n"
        );
        let twice = remove_top_level_property(&once, "tui").unwrap();
        assert_eq!(twice, "{\n  // keep me\n  \"model\": \"a/b\"\n}\n");
        assert_eq!(remove_top_level_property(&twice, "nope"), None);
    }

    /// A fixture `tui.json` remaps a key, sets a theme and disables the
    /// mouse; all three reach the running TUI.
    #[test]
    fn tui_json_remaps_a_key_sets_the_theme_and_disables_the_mouse() {
        let home = tempfile::tempdir().unwrap();
        let paths = paths(home.path());
        std::fs::create_dir_all(&paths.config).unwrap();
        std::fs::write(
            paths.config.join("tui.json"),
            r#"{
                // JSONC, like TS
                "theme": "nord",
                "mouse": false,
                "keybinds": { "command_list": "ctrl+k" },
            }"#,
        )
        .unwrap();
        let project = home.path().join("work/repo");
        std::fs::create_dir_all(&project).unwrap();
        let config = load(&project, &paths, &Flags::default());
        assert!(!config.mouse, "mouse capture off");
        assert_eq!(config.theme.as_deref(), Some("nord"));

        let mut app =
            alforria_tui::state::App::new(config, alforria_tui::state::Args::default(), None);
        assert_eq!(app.ui.theme.active, "nord");
        let ctx = alforria_tui::keymap::DispatchContext {
            route_is_session: false,
            prompt_focused: false,
            foreground_tasks: false,
            prompt_mounted: true,
        };
        let key = |c| {
            crossterm::event::KeyEvent::new(
                crossterm::event::KeyCode::Char(c),
                crossterm::event::KeyModifiers::CONTROL,
            )
        };
        assert_eq!(
            app.keymap.dispatch(&ctx, &key('k'), 0),
            vec!["command.palette.show"]
        );
        assert!(
            app.keymap.dispatch(&ctx, &key('p'), 0).is_empty(),
            "ctrl+p unbound"
        );
    }

    #[test]
    fn precedence_env_project_and_dot_opencode() {
        let home = tempfile::tempdir().unwrap();
        let paths = paths(home.path());
        std::fs::create_dir_all(&paths.config).unwrap();
        std::fs::write(
            paths.config.join("tui.json"),
            r#"{"theme": "global", "scroll_speed": 7, "diff_style": "stacked"}"#,
        )
        .unwrap();
        let env_file = home.path().join("custom-tui.json");
        std::fs::write(&env_file, r#"{"theme": "env", "leader_timeout": 900}"#).unwrap();
        let repo = home.path().join("work/repo");
        std::fs::create_dir_all(repo.join(".opencode")).unwrap();
        std::fs::write(home.path().join("work/tui.json"), r#"{"theme": "outer"}"#).unwrap();
        std::fs::write(repo.join("tui.jsonc"), r#"{"tui": {"theme": "project"}}"#).unwrap();
        std::fs::write(repo.join(".opencode/tui.json"), r#"{"scroll_speed": 2}"#).unwrap();
        // An invalid file is skipped whole.
        std::fs::write(repo.join("tui.json"), r#"{"theme": "bad", "mouse": "no"}"#).unwrap();
        let flags = Flags {
            tui_config: Some(env_file),
            ..Flags::default()
        };
        let config = load(&repo, &paths, &flags);
        assert_eq!(
            config.theme.as_deref(),
            Some("project"),
            "closest project file wins"
        );
        assert_eq!(config.leader_timeout_ms, Some(900));
        assert_eq!(
            config.scroll_speed, 2.0,
            ".opencode beats the project files"
        );
        assert_eq!(config.diff_style, "stacked");
        assert!(config.mouse);
    }

    #[test]
    fn legacy_keys_migrate_out_of_opencode_json() {
        let home = tempfile::tempdir().unwrap();
        let paths = paths(home.path());
        let repo = home.path().join("repo");
        std::fs::create_dir_all(&repo).unwrap();
        let legacy = "{\n  \"model\": \"a/b\",\n  \"theme\": \"tokyonight\",\n  \"keybinds\": { \"leader\": \"ctrl+a\" },\n  \"tui\": { \"scroll_speed\": 4, \"diff_style\": \"nope\" }\n}\n";
        std::fs::write(repo.join("opencode.json"), legacy).unwrap();
        let config = load(&repo, &paths, &Flags::default());
        assert_eq!(config.theme.as_deref(), Some("tokyonight"));
        assert_eq!(config.scroll_speed, 4.0);
        assert_eq!(
            config.keybinds.get("leader").map(String::as_str),
            Some("ctrl+a")
        );
        let written = std::fs::read_to_string(repo.join("tui.json")).unwrap();
        assert!(
            written.starts_with("{\n  \"$schema\": \"https://opencode.ai/tui.json\",\n  \"theme\"")
        );
        assert!(
            !written.contains("diff_style"),
            "invalid legacy values stay behind"
        );
        assert_eq!(
            std::fs::read_to_string(repo.join("opencode.json.tui-migration.bak")).unwrap(),
            legacy
        );
        let stripped: Value =
            serde_json::from_str(&std::fs::read_to_string(repo.join("opencode.json")).unwrap())
                .unwrap();
        assert_eq!(stripped, serde_json::json!({"model": "a/b"}));
    }
}
