//! The host config as a server process read it.
//!
//! The loader reads every source from disk at each load, and an instance
//! is booted — its config loaded — whenever a directory is first opened.
//! The server's access policy, though, is read once at startup. Between
//! the two, the host files can change under a running server: a global
//! config that stops parsing used to count as empty (the host's permission
//! ceiling gone, the built-in `*: allow` in its place), and an agent with a
//! shell could rewrite `~/.config/alforria/*` and have the next instance
//! boot believe it.
//!
//! A [`HostSnapshot`] remembers what each host source — the global config
//! files, `~/.opencode`, the managed directory, and the markdown agents,
//! commands and modes beside them — last loaded as. A server registers one
//! for its global directories ([`HostSnapshot::install`]); every load over
//! those directories in the process then goes through it.
//!
//! * **Frozen** ([`HostSnapshot::freeze`]): an unattended, reachable or
//!   project-restricted server. Each host source is read once and never
//!   again, and [`HostMode`] is the one computed at startup. Only
//!   [`HostSnapshot::refresh`] — the server's own, operator-only config
//!   write — has the files read again, and a file that then fails to load
//!   keeps its previous reading. A source that never loaded is an error,
//!   not an empty config.
//! * **Not frozen**: a developer's own server. Sources are read at every
//!   load, as before, so an edit applies to the next directory opened; a
//!   source that stops loading keeps its last good reading.
//!
//! A process that registers none (`alforria run`, the one-shot commands,
//! a library user) loads as it always did.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, OnceLock};

use serde_json::Value;

use super::precedence::HostMode;
use crate::CoreError;

/// One host source: a file or directory, and which reading of it (the
/// loader substitutes with a different environment for the trusted load).
pub(crate) type SourceKey = (PathBuf, &'static str);

#[derive(Default)]
struct State {
    /// `Some`: frozen, with the mode to answer.
    frozen: Option<HostMode>,
    /// The last good reading of each source, and whether it is current
    /// (not current: [`HostSnapshot::refresh`] asked for another read).
    sources: HashMap<SourceKey, (Value, bool)>,
}

/// See the module documentation.
#[derive(Default)]
pub struct HostSnapshot {
    state: Mutex<State>,
}

fn registry() -> &'static Mutex<HashMap<PathBuf, Arc<HostSnapshot>>> {
    static REGISTRY: OnceLock<Mutex<HashMap<PathBuf, Arc<HostSnapshot>>>> = OnceLock::new();
    REGISTRY.get_or_init(Default::default)
}

impl HostSnapshot {
    /// A new, empty, unfrozen snapshot for the servers of this process
    /// whose global config directory is `config_dir`: a server start. It
    /// replaces the one an earlier start over the same directory left.
    pub fn install(config_dir: &Path) -> Arc<HostSnapshot> {
        let snapshot = Arc::new(HostSnapshot::default());
        registry()
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .insert(config_dir.to_path_buf(), snapshot.clone());
        snapshot
    }

    /// The snapshot registered for `config_dir`, if a server was started
    /// over it in this process.
    pub fn of(config_dir: &Path) -> Option<Arc<HostSnapshot>> {
        registry()
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .get(config_dir)
            .cloned()
    }

    /// From here on nothing is read again, and `mode` is the answer to
    /// how far a project's config is believed.
    pub fn freeze(&self, mode: HostMode) {
        self.state.lock().unwrap_or_else(|p| p.into_inner()).frozen = Some(mode);
    }

    /// The mode a frozen snapshot answers with.
    pub fn frozen_mode(&self) -> Option<HostMode> {
        self.state
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .frozen
            .clone()
    }

    pub fn is_frozen(&self) -> bool {
        self.frozen_mode().is_some()
    }

    /// Have every source read once more (the server wrote the global
    /// config itself). What then fails to load keeps its reading; the
    /// frozen mode does not change.
    pub fn refresh(&self) {
        let mut state = self.state.lock().unwrap_or_else(|p| p.into_inner());
        for (_, current) in state.sources.values_mut() {
            *current = false;
        }
    }

    /// The reading of `key`: the remembered one when frozen and current,
    /// else `load`'s — remembered when it succeeds, and replaced by the
    /// last good one when it fails.
    pub(crate) fn read(
        &self,
        key: SourceKey,
        load: impl FnOnce() -> Result<Value, CoreError>,
    ) -> Result<Value, CoreError> {
        {
            let state = self.state.lock().unwrap_or_else(|p| p.into_inner());
            if let (Some(_), Some((value, true))) = (&state.frozen, state.sources.get(&key)) {
                return Ok(value.clone());
            }
        }
        // Not under the lock: a load reads files and may take a while.
        let loaded = load();
        let mut state = self.state.lock().unwrap_or_else(|p| p.into_inner());
        match loaded {
            Ok(value) => {
                state.sources.insert(key, (value.clone(), true));
                Ok(value)
            }
            Err(error) => match state.sources.get_mut(&key) {
                Some((value, current)) => {
                    tracing::warn!(
                        "{}: {error}; keeping the host config as it last loaded",
                        key.0.display()
                    );
                    *current = true;
                    Ok(value.clone())
                }
                None => Err(error),
            },
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn key(name: &str) -> SourceKey {
        (PathBuf::from(name), "load")
    }

    fn broken() -> Result<Value, CoreError> {
        Err(CoreError::invalid(Path::new("opencode.json"), "not json"))
    }

    #[test]
    fn an_unfrozen_snapshot_rereads_and_falls_back_to_the_last_good_reading() {
        let snapshot = HostSnapshot::default();
        assert!(snapshot.read(key("a"), broken).is_err(), "never loaded");
        assert_eq!(snapshot.read(key("a"), || Ok(json!(1))).unwrap(), json!(1));
        assert_eq!(snapshot.read(key("a"), || Ok(json!(2))).unwrap(), json!(2));
        assert_eq!(snapshot.read(key("a"), broken).unwrap(), json!(2));
        // Another reading of the same path is its own source.
        assert!(snapshot
            .read((PathBuf::from("a"), "trusted"), broken)
            .is_err());
    }

    #[test]
    fn a_frozen_snapshot_reads_each_source_once_until_refreshed() {
        let snapshot = HostSnapshot::default();
        assert_eq!(snapshot.read(key("a"), || Ok(json!(1))).unwrap(), json!(1));
        snapshot.freeze(HostMode {
            restricted: true,
            ..HostMode::default()
        });
        assert!(snapshot.frozen_mode().unwrap().restricted);
        let unreachable = || -> Result<Value, CoreError> { panic!("read again") };
        assert_eq!(snapshot.read(key("a"), unreachable).unwrap(), json!(1));
        // A source first seen after the freeze is read once, then kept.
        assert_eq!(snapshot.read(key("b"), || Ok(json!(5))).unwrap(), json!(5));
        assert_eq!(snapshot.read(key("b"), unreachable).unwrap(), json!(5));
        assert!(snapshot.read(key("c"), broken).is_err());

        snapshot.refresh();
        assert_eq!(snapshot.read(key("a"), || Ok(json!(3))).unwrap(), json!(3));
        assert_eq!(snapshot.read(key("a"), unreachable).unwrap(), json!(3));
        // A refresh that finds a broken file keeps the reading, once.
        assert_eq!(snapshot.read(key("b"), broken).unwrap(), json!(5));
        assert_eq!(snapshot.read(key("b"), unreachable).unwrap(), json!(5));
        assert!(snapshot.frozen_mode().unwrap().restricted);
    }
}
