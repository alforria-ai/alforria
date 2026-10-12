//! The theme registry beyond the built-ins: custom `themes/*.json` files
//! (`context/theme.tsx:37-63`) and the terminal colours the `system`
//! theme is generated from.
//!
//! Custom themes are discovered once, on first use, from the global
//! config directory and every `.opencode` directory from the working
//! directory up to the root, later directories overriding earlier ones
//! (TS `discoverThemes`). They override built-ins of the same name
//! (`listThemes`: defaults < custom < system).

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::{OnceLock, RwLock};

use serde_json::Value;

use super::{ansi_to_rgba, hex, is_theme, TerminalColors};

#[derive(Default)]
struct Registry {
    custom: BTreeMap<String, Value>,
    terminal: TerminalColors,
    /// The terminal answered a probe (its colours are real, not the
    /// `COLORFGBG` / xterm fallbacks).
    probed: bool,
}

static REGISTRY: OnceLock<RwLock<Registry>> = OnceLock::new();

fn registry() -> &'static RwLock<Registry> {
    REGISTRY.get_or_init(|| {
        RwLock::new(Registry {
            // Unit tests stay hermetic: no user theme files.
            custom: if cfg!(test) {
                BTreeMap::new()
            } else {
                discover_themes(&theme_directories())
            },
            terminal: env_terminal_colors(std::env::var("COLORFGBG").ok().as_deref()),
            probed: false,
        })
    })
}

/// `Global.Path.config` (`$XDG_CONFIG_HOME/opencode`, else
/// `~/.config/opencode`) then `.opencode` in the working directory and
/// each of its ancestors (`context/theme.tsx:40-46`).
pub fn theme_directories() -> Vec<PathBuf> {
    let mut directories = Vec::new();
    let config_home = std::env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .filter(|path| path.is_absolute())
        .or_else(|| std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".config")));
    if let Some(config_home) = config_home {
        directories.push(config_home.join("opencode"));
    }
    if let Ok(cwd) = std::env::current_dir() {
        for ancestor in cwd.ancestors() {
            directories.push(ancestor.join(".opencode"));
        }
    }
    directories
}

/// `discoverThemes(directories)`: every `themes/*.json` (named after the
/// file stem) that parses and has a `theme` object. Unlike TS, one bad
/// file is skipped instead of dropping every custom theme.
pub fn discover_themes(directories: &[PathBuf]) -> BTreeMap<String, Value> {
    let mut themes = BTreeMap::new();
    for directory in directories {
        let Ok(entries) = std::fs::read_dir(directory.join("themes")) else {
            continue;
        };
        let mut files: Vec<PathBuf> = entries
            .filter_map(Result::ok)
            .map(|entry| entry.path())
            .filter(|path| path.extension().is_some_and(|ext| ext == "json"))
            .collect();
        files.sort();
        for file in files {
            if let Some((name, theme)) = read_theme(&file) {
                themes.insert(name, theme);
            }
        }
    }
    themes
}

fn read_theme(file: &Path) -> Option<(String, Value)> {
    let name = file.file_stem()?.to_str()?.to_string();
    let raw = std::fs::read_to_string(file).ok()?;
    let theme: Value = serde_json::from_str(&raw).ok()?;
    is_theme(&theme).then_some((name, theme))
}

/// Re-scan the theme directories (TS refreshes on `SIGUSR2`).
pub fn refresh_custom_themes() {
    set_custom_themes(discover_themes(&theme_directories()));
}

/// `setCustomThemes(themes)`.
pub fn set_custom_themes(themes: BTreeMap<String, Value>) {
    if let Ok(mut registry) = registry().write() {
        registry.custom = themes;
    }
}

/// `upsertTheme(name, theme)`: add or replace one custom theme; `false`
/// when it is not a theme.
pub fn upsert_custom_theme(name: &str, theme: Value) -> bool {
    if name.is_empty() || !is_theme(&theme) {
        return false;
    }
    match registry().write() {
        Ok(mut registry) => {
            registry.custom.insert(name.to_string(), theme);
            true
        }
        Err(_) => false,
    }
}

pub fn custom_theme(name: &str) -> Option<Value> {
    registry().read().ok()?.custom.get(name).cloned()
}

pub fn custom_themes() -> Vec<(String, Value)> {
    registry()
        .read()
        .map(|registry| {
            registry
                .custom
                .iter()
                .map(|(name, theme)| (name.clone(), theme.clone()))
                .collect()
        })
        .unwrap_or_default()
}

/// Install the colours the `system` theme is generated from (a probe of
/// the live terminal: see [`super::probe::probe_terminal_colors`]).
pub fn set_terminal_colors(colors: TerminalColors) {
    if let Ok(mut registry) = registry().write() {
        registry.terminal = colors;
        registry.probed = true;
    }
}

/// The probed terminal colours, if a probe succeeded.
pub fn probed_colors() -> Option<TerminalColors> {
    let registry = registry().read().ok()?;
    registry.probed.then(|| registry.terminal.clone())
}

pub fn terminal_colors() -> TerminalColors {
    registry()
        .read()
        .map(|registry| registry.terminal.clone())
        .unwrap_or_default()
}

/// Without a probe, `COLORFGBG` (`fg;bg` or `fg;default;bg`, ANSI
/// indexes) still names the default foreground and background; the
/// rest of the palette falls back to the xterm defaults.
pub fn env_terminal_colors(colorfgbg: Option<&str>) -> TerminalColors {
    let mut colors = TerminalColors::default();
    let Some(value) = colorfgbg else {
        return colors;
    };
    let fields: Vec<&str> = value.split(';').map(str::trim).collect();
    let index = |field: Option<&&str>| {
        field
            .and_then(|field| field.parse::<i64>().ok())
            .filter(|index| (0..256).contains(index))
    };
    if fields.len() >= 2 {
        colors.default_foreground = index(fields.first()).map(|fg| hex(ansi_to_rgba(fg)));
        colors.default_background = index(fields.last()).map(|bg| hex(ansi_to_rgba(bg)));
    }
    colors
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn discovery_reads_theme_files_later_directories_winning() {
        let first = tempfile::tempdir().unwrap();
        let second = tempfile::tempdir().unwrap();
        let write = |dir: &Path, name: &str, body: &str| {
            std::fs::create_dir_all(dir.join("themes")).unwrap();
            std::fs::write(dir.join("themes").join(name), body).unwrap();
        };
        write(
            first.path(),
            "mine.json",
            r##"{"theme":{"primary":"#111111"}}"##,
        );
        write(
            first.path(),
            "shared.json",
            r##"{"theme":{"primary":"#222222"}}"##,
        );
        write(
            second.path(),
            "shared.json",
            r##"{"theme":{"primary":"#333333"}}"##,
        );
        write(second.path(), "broken.json", "{ not json");
        write(second.path(), "nottheme.json", r#"{"defs":{}}"#);
        write(second.path(), "notes.txt", "ignored");
        let themes = discover_themes(&[
            first.path().to_path_buf(),
            PathBuf::from("/nonexistent/alforria"),
            second.path().to_path_buf(),
        ]);
        assert_eq!(themes.keys().collect::<Vec<_>>(), vec!["mine", "shared"]);
        assert_eq!(themes["shared"]["theme"]["primary"], "#333333");
    }

    #[test]
    fn theme_directories_end_with_the_cwd_ancestors() {
        let directories = theme_directories();
        let cwd = std::env::current_dir().unwrap();
        assert!(directories.contains(&cwd.join(".opencode")));
        assert_eq!(directories.last(), Some(&PathBuf::from("/.opencode")));
    }

    #[test]
    fn colorfgbg_names_the_default_colors() {
        let colors = env_terminal_colors(Some("15;0"));
        assert_eq!(colors.default_foreground.as_deref(), Some("#ffffff"));
        assert_eq!(colors.default_background.as_deref(), Some("#000000"));
        let colors = env_terminal_colors(Some("0;default;15"));
        assert_eq!(colors.default_background.as_deref(), Some("#ffffff"));
        let colors = env_terminal_colors(Some("garbage"));
        assert_eq!(colors.default_background, None);
        assert_eq!(env_terminal_colors(None).default_foreground, None);
    }
}
