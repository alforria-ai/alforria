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
//!   back), each joined with `opencode`.
//!
//! Deviation from TS: `global.ts` computes the XDG paths at module load from
//! `os.homedir()`, so `OPENCODE_TEST_HOME` only affects the `home` getter.
//! The Rust port derives all fallbacks from the resolved `home` — a
//! deliberately consistent reading of the same rules.

use std::env;
use std::ffi::OsString;
use std::path::{Path, PathBuf};

/// XDG-style global locations for opencode.
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
        GlobalPaths {
            config: xdg_or(env::var_os("XDG_CONFIG_HOME"), &home, ".config").join("opencode"),
            data: xdg_or(env::var_os("XDG_DATA_HOME"), &home, ".local/share").join("opencode"),
            cache: xdg_or(env::var_os("XDG_CACHE_HOME"), &home, ".cache").join("opencode"),
            state: xdg_or(env::var_os("XDG_STATE_HOME"), &home, ".local/state").join("opencode"),
            home,
        }
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
    fn falls_back_to_default_subdirs() {
        let paths = GlobalPaths::resolve(PathBuf::from("/home/user"));
        assert_eq!(paths.home, PathBuf::from("/home/user"));
        assert_eq!(paths.config, PathBuf::from("/home/user/.config/opencode"));
        assert_eq!(
            paths.data,
            PathBuf::from("/home/user/.local/share/opencode")
        );
        assert_eq!(paths.cache, PathBuf::from("/home/user/.cache/opencode"));
        assert_eq!(
            paths.state,
            PathBuf::from("/home/user/.local/state/opencode")
        );
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
