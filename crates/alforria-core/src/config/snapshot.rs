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
//!   again, and [`HostMode`] is the one computed at startup. The server's
//!   own, operator-only config write changes the remembered reading by
//!   exactly what the operator sent ([`HostSnapshot::apply_patch`]):
//!   nothing is read from disk for it, so a host file rewritten in the
//!   meantime — the one written or any other — is not adopted with it. A
//!   host source that was not there at startup (a `~/.opencode` created
//!   later) stays absent.
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
    /// The last good reading of each source.
    sources: HashMap<SourceKey, Value>,
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

    /// The operator changed the global config through the server
    /// (`PATCH /global/config`): merge `patch` (`merge_patch`: a `null`
    /// deletes its key) into what a frozen snapshot remembers the global
    /// config directory `config_dir` loaded as. Nothing is read from disk,
    /// and the frozen mode does not change. Not frozen: nothing to do, the
    /// next load reads the file the server just wrote. `{env:}` and
    /// `{file:}` in the patch are not substituted until a restart.
    pub fn apply_patch(&self, config_dir: &Path, patch: &Value) {
        let mut state = self.state.lock().unwrap_or_else(|p| p.into_inner());
        if state.frozen.is_none() {
            return;
        }
        for ((path, reading), value) in state.sources.iter_mut() {
            if path == config_dir && matches!(*reading, "load" | "trusted") {
                crate::merge_patch(value, patch);
            }
        }
    }

    /// The reading of `key`: the remembered one when frozen (empty for a
    /// source the freeze did not read),
    /// else `load`'s — remembered when it succeeds, and replaced by the
    /// last good one when it fails.
    pub(crate) fn read(
        &self,
        key: SourceKey,
        load: impl FnOnce() -> Result<Value, CoreError>,
    ) -> Result<Value, CoreError> {
        {
            let state = self.state.lock().unwrap_or_else(|p| p.into_inner());
            if state.frozen.is_some() {
                // What was not read by the freeze was not there: a host
                // file that appears afterwards is nobody's startup config.
                return Ok(state
                    .sources
                    .get(&key)
                    .cloned()
                    .unwrap_or_else(|| Value::Object(Default::default())));
            }
        }
        // Not under the lock: a load reads files and may take a while.
        let loaded = load();
        let mut state = self.state.lock().unwrap_or_else(|p| p.into_inner());
        match loaded {
            Ok(value) => {
                state.sources.insert(key, value.clone());
                Ok(value)
            }
            Err(error) => match state.sources.get(&key) {
                Some(value) => {
                    tracing::warn!(
                        "{}: {error}; keeping the host config as it last loaded",
                        key.0.display()
                    );
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
    fn a_frozen_snapshot_reads_each_source_once_and_changes_only_by_a_patch() {
        let snapshot = HostSnapshot::default();
        let dir = PathBuf::from("config");
        let global = |reading| (dir.clone(), reading);
        for reading in ["load", "trusted", "mode"] {
            snapshot
                .read(global(reading), || Ok(json!({ "a": 1, "b": 2 })))
                .unwrap();
        }
        assert_eq!(snapshot.read(key("x"), || Ok(json!(1))).unwrap(), json!(1));
        // Not frozen: a patch is nothing, the next load reads the file.
        snapshot.apply_patch(&dir, &json!({ "a": 9 }));
        snapshot.freeze(HostMode {
            restricted: true,
            ..HostMode::default()
        });
        let unreachable = || -> Result<Value, CoreError> { panic!("read again") };
        assert_eq!(
            snapshot.read(global("load"), unreachable).unwrap(),
            json!({ "a": 1, "b": 2 })
        );
        // A source first seen after the freeze was not there at startup.
        assert_eq!(snapshot.read(key("y"), unreachable).unwrap(), json!({}));

        snapshot.apply_patch(&dir, &json!({ "a": null, "c": 3 }));
        for reading in ["load", "trusted"] {
            assert_eq!(
                snapshot.read(global(reading), unreachable).unwrap(),
                json!({ "b": 2, "c": 3 })
            );
        }
        // Only the global config's readings: no other source, and not what
        // the mode was computed from.
        assert_eq!(snapshot.read(key("x"), unreachable).unwrap(), json!(1));
        assert_eq!(
            snapshot.read(global("mode"), unreachable).unwrap(),
            json!({ "a": 1, "b": 2 })
        );
        assert!(snapshot.frozen_mode().unwrap().restricted);
    }
}
