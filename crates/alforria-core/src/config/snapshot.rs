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
//!   host source the startup found absent (a `~/.opencode` created later)
//!   stays absent; one it did not manage to read is an error for every
//!   load, never an empty config — and a server whose startup loads
//!   failed is not frozen at all (it refuses to listen).
//!
//! With a snapshot registered, frozen or not, a global config that has
//! never loaded is an error too, where a one-shot command still degrades
//! it to an empty config as upstream does.
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
pub type SourceKey = (PathBuf, &'static str);

#[derive(Default)]
struct State {
    /// `Some`: frozen, with the mode to answer.
    frozen: Option<HostMode>,
    /// The last good reading of each source. A source that was read and
    /// found absent is here, empty.
    sources: HashMap<SourceKey, Value>,
    /// Sources whose last read failed with no good reading before it.
    failed: std::collections::HashSet<SourceKey>,
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
    /// how far a project's config is believed. `absent` are the sources
    /// the startup loads could have asked for and did not, because their
    /// directory was not there: remembered as empty. A source whose read
    /// failed is not made absent by this — it stays an error.
    pub fn freeze(&self, mode: HostMode, absent: impl IntoIterator<Item = SourceKey>) {
        let mut state = self.state.lock().unwrap_or_else(|p| p.into_inner());
        for key in absent {
            if !state.failed.contains(&key) {
                state
                    .sources
                    .entry(key)
                    .or_insert_with(|| Value::Object(Default::default()));
            }
        }
        state.frozen = Some(mode);
    }

    /// The sources whose read failed with nothing good to fall back to.
    pub fn failed(&self) -> Vec<PathBuf> {
        let state = self.state.lock().unwrap_or_else(|p| p.into_inner());
        let mut failed: Vec<PathBuf> = state.failed.iter().map(|(path, _)| path.clone()).collect();
        failed.sort();
        failed.dedup();
        failed
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

    /// The reading of `key`: the remembered one when frozen (an error
    /// for a source the startup did not read),
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
                // Read at startup (or found absent then), or an error:
                // a source the startup did not read is not an empty one.
                return state.sources.get(&key).cloned().ok_or_else(|| {
                    CoreError::invalid(
                        &key.0,
                        "this host config source was not read when the server started; \
                         restart the server to apply it",
                    )
                });
            }
        }
        // Not under the lock: a load reads files and may take a while.
        let loaded = load();
        let mut state = self.state.lock().unwrap_or_else(|p| p.into_inner());
        match loaded {
            Ok(value) => {
                state.failed.remove(&key);
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
                None => {
                    state.failed.insert(key);
                    Err(error)
                }
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
        // A source whose read failed before the freeze.
        assert!(snapshot.read(key("broken"), broken).is_err());
        assert_eq!(snapshot.failed(), [PathBuf::from("broken")]);
        snapshot.freeze(
            HostMode {
                restricted: true,
                ..HostMode::default()
            },
            // What the startup could have read and found no directory for
            // — and, wrongly, the one that failed.
            [key("absent"), key("broken")],
        );
        let unreachable = || -> Result<Value, CoreError> { panic!("read again") };
        assert_eq!(
            snapshot.read(global("load"), unreachable).unwrap(),
            json!({ "a": 1, "b": 2 })
        );
        // Confirmed absent at startup: empty, and never read.
        assert_eq!(
            snapshot.read(key("absent"), unreachable).unwrap(),
            json!({})
        );
        // Failed at startup, or never asked for: an error, not empty.
        assert!(snapshot.read(key("broken"), unreachable).is_err());
        assert!(snapshot.read(key("never"), unreachable).is_err());

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
