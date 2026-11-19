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
    ///
    /// This is the *process entry point's* resolver: a command resolves the
    /// paths once and hands them on. Nothing a request handler or a test
    /// fixture reaches may call it — they use the paths their context was
    /// built over. To make that hold, a test run refuses to resolve the
    /// developer's own directories: see [`sandbox`].
    pub fn from_env() -> GlobalPaths {
        let home = env::var_os("OPENCODE_TEST_HOME")
            .map(PathBuf::from)
            .or_else(dirs::home_dir);
        let home = match home {
            Some(home) if !home.as_os_str().is_empty() => home,
            _ => PathBuf::from("."),
        };
        let paths = GlobalPaths::resolve(home);
        sandbox::guard(&paths, "GlobalPaths::from_env()");
        paths
    }

    /// Paths under a directory of their own in the system temporary
    /// directory, removed when the returned guard drops. The contexts that
    /// are not given real paths (the unwired ones tests build) run over
    /// these, so a handler that reads or writes a global location finds an
    /// empty one instead of the user's.
    pub fn detached() -> DetachedPaths {
        static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let n = NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.subsec_nanos())
            .unwrap_or(0);
        let root = env::temp_dir().join(format!(
            "alforria-detached-{}-{n}-{nanos:08x}",
            std::process::id()
        ));
        let paths = GlobalPaths {
            home: root.join("home"),
            config: root.join("config").join(APP_DIR),
            data: root.join("data").join(APP_DIR),
            cache: root.join("cache").join(APP_DIR),
            state: root.join("state").join(APP_DIR),
        };
        DetachedPaths { paths, root }
    }

    /// The four application directories, for checks that must cover each.
    pub fn locations(&self) -> [(&'static str, &Path); 4] {
        [
            ("config", self.config.as_path()),
            ("data", self.data.as_path()),
            ("cache", self.cache.as_path()),
            ("state", self.state.as_path()),
        ]
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

/// See [`GlobalPaths::detached`]. Nothing is created until something writes;
/// dropping removes whatever was.
#[derive(Debug)]
pub struct DetachedPaths {
    pub paths: GlobalPaths,
    root: PathBuf,
}

impl Drop for DetachedPaths {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.root);
    }
}

/// The guard that keeps test runs off the developer's own directories.
///
/// Tests of this repository once opened — and migrated — the real database:
/// a fixture or a handler called [`GlobalPaths::from_env`] and nothing
/// noticed. So a process that is a test run refuses to use global paths that
/// are not under a sandbox root, by panicking with a message that names the
/// call. Every harness goes through [`guard`] as well (the server's
/// `production_context`, the binary-spawning e2e harness), so the check does
/// not depend on which constructor a new test happens to use.
///
/// *Test run* means: this crate compiled for its own tests, or a debug build
/// that is a cargo test binary (the executable lives in a `deps` directory),
/// runs under nextest (`NEXTEST` is set — inherited by the binaries a test
/// spawns), or was told so with `ALFORRIA_TEST_GUARD=1`. Release builds never
/// check: an installed binary behaves the same wherever it is started.
///
/// *Sandbox root* means: the system temporary directory, or the directory
/// `ALFORRIA_TEST_SANDBOX` names (CI's isolation job designates one).
pub mod sandbox {
    use super::GlobalPaths;
    use std::path::{Path, PathBuf};

    /// Marks a process as a test run (see the module docs).
    pub const GUARD_ENV: &str = "ALFORRIA_TEST_GUARD";
    /// An extra sandbox root besides the temporary directory.
    pub const SANDBOX_ENV: &str = "ALFORRIA_TEST_SANDBOX";

    /// Whether this process is a test run.
    pub fn under_test() -> bool {
        if cfg!(test) {
            return true;
        }
        if !cfg!(debug_assertions) {
            return false;
        }
        static UNDER_TEST: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
        *UNDER_TEST.get_or_init(|| {
            let truthy = |name: &str| {
                std::env::var_os(name).is_some_and(|value| !value.is_empty() && value != "0")
            };
            truthy(GUARD_ENV)
                || truthy("NEXTEST")
                || std::env::current_exe().ok().is_some_and(|exe| {
                    exe.parent()
                        .and_then(Path::file_name)
                        .is_some_and(|dir| dir == "deps")
                })
        })
    }

    /// The directories a test run may use.
    pub fn roots() -> Vec<PathBuf> {
        let mut roots = vec![std::env::temp_dir()];
        if let Some(extra) = std::env::var_os(SANDBOX_ENV).filter(|value| !value.is_empty()) {
            roots.push(PathBuf::from(extra));
        }
        roots
    }

    /// `path` with its existing prefix canonicalised: the path itself often
    /// does not exist yet, and `/tmp` may be a symlink.
    fn normalise(path: &Path) -> PathBuf {
        let mut existing = path;
        let mut rest = Vec::new();
        loop {
            if let Ok(real) = existing.canonicalize() {
                let mut out = real;
                for part in rest.iter().rev() {
                    out.push(part);
                }
                return out;
            }
            match (existing.parent(), existing.file_name()) {
                (Some(parent), Some(name)) => {
                    rest.push(name.to_os_string());
                    existing = parent;
                }
                _ => return path.to_path_buf(),
            }
        }
    }

    /// Whether `path` lies under one of `roots`.
    pub fn is_under(path: &Path, roots: &[PathBuf]) -> bool {
        let path = normalise(path);
        roots.iter().any(|root| {
            let root = normalise(root);
            path.starts_with(&root) && path != root
        })
    }

    /// The first application directory of `paths` outside every sandbox
    /// root, if any.
    pub fn escape(paths: &GlobalPaths, roots: &[PathBuf]) -> Option<(&'static str, PathBuf)> {
        paths
            .locations()
            .into_iter()
            .find(|(_, path)| !is_under(path, roots))
            .map(|(name, path)| (name, path.to_path_buf()))
    }

    /// Panic when this is a test run and `paths` is not sandboxed. `what`
    /// names the caller for the message.
    #[track_caller]
    pub fn guard(paths: &GlobalPaths, what: &str) {
        if under_test() {
            require(paths, what);
        }
    }

    /// [`guard`] for one path — the database file a storage is opened on.
    #[track_caller]
    pub fn guard_path(path: &Path, what: &str) {
        if !under_test() {
            return;
        }
        let roots = roots();
        if !is_under(path, &roots) {
            panic!(
                "{what}: refusing to use {} in a test run — it is not under a sandbox root \
                 ({roots:?}). See CONTRIBUTING.md, \"Tests never touch your own files\".",
                path.display()
            );
        }
    }

    /// Panic unless `paths` is sandboxed, test run or not: for test
    /// harnesses, which know they are one.
    #[track_caller]
    pub fn require(paths: &GlobalPaths, what: &str) {
        require_in(paths, what, &roots());
    }

    /// [`require`] against explicit roots.
    #[track_caller]
    pub fn require_in(paths: &GlobalPaths, what: &str, roots: &[PathBuf]) {
        if let Some((name, path)) = escape(paths, roots) {
            panic!(
                "{what}: refusing to use the {name} directory {} in a test run — it is not under \
                 a sandbox root ({roots:?}). A test must build its context over a temporary home; \
                 see CONTRIBUTING.md, \"Tests never touch your own files\".",
                path.display()
            );
        }
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
    fn a_test_run_refuses_paths_outside_the_sandbox() {
        let roots = vec![PathBuf::from("/nonexistent/sandbox")];
        let inside = GlobalPaths::resolve_with(PathBuf::from("/nonexistent/sandbox/h"), |_| None);
        assert_eq!(sandbox::escape(&inside, &roots), None);
        // One location outside is enough, whichever it is.
        let outside = GlobalPaths {
            data: PathBuf::from("/home/someone/.local/share/opencode"),
            ..inside.clone()
        };
        assert_eq!(
            sandbox::escape(&outside, &roots),
            Some(("data", PathBuf::from("/home/someone/.local/share/opencode")))
        );
        // The root itself is not "under" it, and a sibling with the same
        // prefix is not either.
        assert!(!sandbox::is_under(
            Path::new("/nonexistent/sandbox"),
            &roots
        ));
        assert!(!sandbox::is_under(
            Path::new("/nonexistent/sandboxed/x"),
            &roots
        ));
        assert!(sandbox::under_test());
        let panic = std::panic::catch_unwind(|| sandbox::require_in(&outside, "a fixture", &roots))
            .expect_err("must refuse");
        let message = panic.downcast_ref::<String>().expect("message");
        assert!(message.contains("a fixture"), "{message}");
        assert!(
            message.contains("/home/someone/.local/share/opencode"),
            "{message}"
        );
    }

    #[test]
    fn detached_paths_live_under_the_temp_dir_and_clean_up() {
        let detached = GlobalPaths::detached();
        assert_eq!(sandbox::escape(&detached.paths, &sandbox::roots()), None);
        std::fs::create_dir_all(&detached.paths.data).unwrap();
        let data = detached.paths.data.clone();
        drop(detached);
        assert!(!data.exists());
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
