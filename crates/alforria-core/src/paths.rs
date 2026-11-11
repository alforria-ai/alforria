//! Global paths: `GlobalPaths` mirrors TS `Global.Path` from
//! `packages/core/src/global.ts`.
//!
//! Resolution order:
//!
//! * `home` honors `OPENCODE_TEST_HOME` (the TS getter seam), then the OS home
//!   directory;
//! * `config`/`data`/`cache` honor `XDG_CONFIG_HOME` / `XDG_DATA_HOME` /
//!   `XDG_CACHE_HOME`, falling back to `~/.config` / `~/.local/share` /
//!   `~/.cache` (xdg-basedir semantics: an empty env var is falsy and falls
//!   back), each joined with the application directory (see below).
//!
//! # `alforria` versus `opencode` directories
//!
//! alforria started out sharing upstream's `opencode` directories. Upstream
//! v2 migrates that database in place to a schema alforria cannot read and
//! writes config keys alforria rejects, so alforria now has directories of
//! its own. Each location resolves independently:
//!
//! 1. `<base>/alforria` when it exists;
//! 2. else `<base>/opencode` when *that* exists (an install that predates
//!    the split keeps working untouched);
//! 3. else `<base>/alforria` (a fresh install).
//!
//! Nothing is moved or copied implicitly: a data directory can be tens of
//! gigabytes. `alforria migrate` performs the move on request.
//!
//! Deviation from TS: `global.ts` computes the XDG paths at module load from
//! `os.homedir()`, so `OPENCODE_TEST_HOME` only affects the `home` getter.
//! The Rust port derives all fallbacks from the resolved `home` — a
//! deliberately consistent reading of the same rules.

use std::env;
use std::ffi::OsString;
use std::path::{Path, PathBuf};

/// The directory alforria owns under each XDG base.
pub const APP_DIR: &str = "alforria";
/// The upstream directory older installs share with opencode.
pub const LEGACY_DIR: &str = "opencode";

/// XDG-style global locations for alforria.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GlobalPaths {
    pub home: PathBuf,
    pub config: PathBuf,
    pub data: PathBuf,
    pub cache: PathBuf,
    pub state: PathBuf,
}

impl GlobalPaths {
    /// Resolve paths from the process environment.
    pub fn from_env() -> GlobalPaths {
        let home = env::var_os("OPENCODE_TEST_HOME")
            .map(PathBuf::from)
            .or_else(dirs::home_dir);
        let home = match home {
            Some(home) if !home.as_os_str().is_empty() => home,
            _ => PathBuf::from("."),
        };
        GlobalPaths::resolve(home)
    }

    /// Resolve paths against an explicit home directory (XDG env vars are
    /// still consulted; only their fallbacks use `home`).
    pub fn resolve(home: PathBuf) -> GlobalPaths {
        GlobalPaths::resolve_with(home, |key| env::var_os(key))
    }

    /// Same as [`GlobalPaths::resolve`], with the env accessor injected
    /// (tests pass `_|_ None` to be independent of the runner's XDG vars).
    pub fn resolve_with(home: PathBuf, getenv: impl Fn(&str) -> Option<OsString>) -> GlobalPaths {
        let bases = XdgBases::resolve(&home, getenv);
        GlobalPaths {
            config: app_dir(&bases.config),
            data: app_dir(&bases.data),
            cache: app_dir(&bases.cache),
            state: app_dir(&bases.state),
            home,
        }
    }

    /// The four locations under a fixed directory name, with no fallback:
    /// `alforria migrate` uses this for both ends of the move.
    pub fn named(home: PathBuf, name: &str) -> GlobalPaths {
        let bases = XdgBases::resolve(&home, |key| env::var_os(key));
        GlobalPaths {
            config: bases.config.join(name),
            data: bases.data.join(name),
            cache: bases.cache.join(name),
            state: bases.state.join(name),
            home,
        }
    }

    /// The home directory `from_env` resolves against.
    pub fn home_from_env() -> PathBuf {
        GlobalPaths::from_env().home
    }

    /// Locations still resolving to the shared `opencode` directory.
    pub fn legacy_locations(&self) -> Vec<(&'static str, &Path)> {
        [
            ("config", self.config.as_path()),
            ("data", self.data.as_path()),
            ("cache", self.cache.as_path()),
            ("state", self.state.as_path()),
        ]
        .into_iter()
        .filter(|(_, path)| path.file_name().is_some_and(|name| name == LEGACY_DIR))
        .collect()
    }
}

struct XdgBases {
    config: PathBuf,
    data: PathBuf,
    cache: PathBuf,
    state: PathBuf,
}

impl XdgBases {
    fn resolve(home: &Path, getenv: impl Fn(&str) -> Option<OsString>) -> XdgBases {
        XdgBases {
            config: xdg_or(getenv("XDG_CONFIG_HOME"), home, ".config"),
            data: xdg_or(getenv("XDG_DATA_HOME"), home, ".local/share"),
            cache: xdg_or(getenv("XDG_CACHE_HOME"), home, ".cache"),
            state: xdg_or(getenv("XDG_STATE_HOME"), home, ".local/state"),
        }
    }
}

/// alforria's directory under an XDG base, or the legacy `opencode` one when
/// only that exists (module docs).
pub fn app_dir(base: &Path) -> PathBuf {
    let own = base.join(APP_DIR);
    if own.exists() {
        return own;
    }
    let legacy = base.join(LEGACY_DIR);
    if legacy.exists() {
        legacy
    } else {
        own
    }
}

/// xdg-basedir: `env.XDG_X_HOME || path.join(home, fallback)`. An empty env
/// value is falsy, so it falls back like an unset one.
fn xdg_or(value: Option<OsString>, home: &Path, fallback: &str) -> PathBuf {
    match value {
        Some(v) if !v.is_empty() => PathBuf::from(v),
        _ => home.join(fallback),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fresh_install_uses_alforria_directories() {
        let paths = GlobalPaths::resolve_with(PathBuf::from("/nonexistent/user"), |_| None);
        assert_eq!(paths.home, PathBuf::from("/nonexistent/user"));
        assert_eq!(
            paths.config,
            PathBuf::from("/nonexistent/user/.config/alforria")
        );
        assert_eq!(
            paths.data,
            PathBuf::from("/nonexistent/user/.local/share/alforria")
        );
        assert_eq!(
            paths.cache,
            PathBuf::from("/nonexistent/user/.cache/alforria")
        );
        assert_eq!(
            paths.state,
            PathBuf::from("/nonexistent/user/.local/state/alforria")
        );
        assert!(paths.legacy_locations().is_empty());
    }

    #[test]
    fn existing_opencode_directory_is_kept_until_alforria_exists() {
        let home = tempfile::tempdir().unwrap();
        let share = home.path().join(".local/share");
        std::fs::create_dir_all(share.join("opencode")).unwrap();
        let resolve = || GlobalPaths::resolve_with(home.path().to_path_buf(), |_| None);

        let paths = resolve();
        assert_eq!(paths.data, share.join("opencode"));
        // Each location decides for itself: config has no legacy directory.
        assert_eq!(paths.config, home.path().join(".config/alforria"));
        assert_eq!(
            paths.legacy_locations(),
            vec![("data", share.join("opencode").as_path())]
        );

        std::fs::create_dir_all(share.join("alforria")).unwrap();
        assert_eq!(resolve().data, share.join("alforria"));
    }

    #[test]
    fn env_overrides_home() {
        assert_eq!(
            xdg_or(
                Some("/custom/cfg".into()),
                Path::new("/home/user"),
                ".config"
            ),
            PathBuf::from("/custom/cfg"),
        );
    }

    #[test]
    fn empty_env_is_falsy() {
        assert_eq!(
            xdg_or(Some("".into()), Path::new("/home/user"), ".config"),
            PathBuf::from("/home/user/.config"),
        );
        assert_eq!(
            xdg_or(None, Path::new("/home/user"), ".config"),
            PathBuf::from("/home/user/.config"),
        );
    }
}
