//! Config precedence chain — port of `loadInstanceState` from
//! `packages/opencode/src/config/config.ts:328+` plus the global loader
//! (`config.ts:260-293`) and `ConfigPaths` (`paths.ts`).
//!
//! Sources merge in order (later wins, via `merge_config_concat_arrays`):
//!
//! 1. remote well-known configs (auth) — **seam only**, fetching is out of
//!    scope for M3 ([`AuthProviders`]);
//! 2. the global config dir: seed the default file when absent, then merge
//!    `config.json`, `opencode.json`, `opencode.jsonc`;
//! 3. legacy TOML dir (`<config>/config`) — recognized, skipped with a warn;
//! 4. `OPENCODE_CONFIG` env file;
//! 5. project files (`opencode.json[c]` walking `directory` up to
//!    `worktree`), deepest last;
//! 6. `.opencode` directory sources (config files + agent/command discovery);
//! 7. `OPENCODE_CONFIG_CONTENT` env;
//! 8. active-org account config — **seam only**;
//! 9. the managed config dir (MDM);
//! 10. post-merge normalization (mode fold, `OPENCODE_PERMISSION`, `tools`
//!     fold, username default, autoshare, compaction env flags).
//!
//! Merging happens over `serde_json::Value` *before* typed decode (M3 spec
//! §2.2), but each source is validated against the wire schema as it is
//! loaded so that global failures can degrade to `{}` while project
//! failures stay fatal.

use std::collections::HashMap;
use std::fs;
use std::io::ErrorKind;
use std::path::{Path, PathBuf};

use serde_json::{json, Map, Value};

use crate::merge::{merge_config_concat_arrays, merge_deep};
use crate::paths::GlobalPaths;
use crate::{parse_jsonc, CoreError};

use super::schema::{decode_config, Config, SCHEMA_REF};
use super::variable::{substitute, Missing, Source};

/// Content written to `.opencode/.gitignore` (`ensureGitignore`).
pub const GITIGNORE_CONTENTS: &str =
    "node_modules\npackage.json\npackage-lock.json\nbun.lock\n.gitignore";

// ---------------------------------------------------------------------------
// Seams
// ---------------------------------------------------------------------------

/// One auth-provider credential — a `(url, key, token)` triple.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AuthProvider {
    pub url: String,
    pub key: String,
    pub token: String,
}

/// Auth seam (M3.2): remote config *fetching* is out of scope — the loader
/// needs the provider list (whose `key`/`token` pairs feed `{env:VAR}`
/// substitution) and a way to obtain remote documents when a future
/// milestone provides one.
pub trait AuthProviders {
    /// All auth providers (`Auth.Service.all()`).
    fn all(&self) -> Vec<AuthProvider>;

    /// Fetch the merged remote config for one well-known auth provider.
    ///
    /// Returns `(wellknown_url, merged_remote_config)` where the merged
    /// config is `mergeConfig(wellknown.config, fetched)` from
    /// `{url}/.well-known/opencode` and the `remote_config` document it
    /// points at. Fetching is out of scope in M3 — the default returns
    /// `None`, which skips the remote merge entirely.
    fn wellknown_config(
        &self,
        _provider: &AuthProvider,
        _env: &HashMap<String, String>,
    ) -> Result<Option<(String, Value)>, CoreError> {
        Ok(None)
    }

    /// Active-org account config (`<url>/api/config`).
    ///
    /// Returns `(source, config)`. Seam only in M3 (spec STOP S5) — the
    /// default returns `None`.
    fn account_config(&self) -> Result<Option<(String, Value)>, CoreError> {
        Ok(None)
    }
}

/// The no-auth default: no providers, no remote configs.
#[derive(Debug, Clone, Copy, Default)]
pub struct NoAuth;

impl AuthProviders for NoAuth {
    fn all(&self) -> Vec<AuthProvider> {
        Vec::new()
    }
}

/// Agent/command Markdown discovery seam (M3.3 owns the real port).
///
/// Each method returns the *merged entry map* discovered under `dir`
/// (`ConfigCommand.load(dir)`, `ConfigAgent.load(dir)` and
/// `ConfigAgent.loadMode(dir)` — merged per directory with plain
/// `mergeDeep`, not the concat-arrays merge).
pub trait ConfigDiscovery {
    fn commands(&self, dir: &Path) -> Result<Value, CoreError>;
    fn agents(&self, dir: &Path) -> Result<Value, CoreError>;
    fn modes(&self, dir: &Path) -> Result<Value, CoreError>;
}

/// No-op discovery: returns empty entry maps. The M3.3 chunk replaces this
/// with the Markdown scanner.
#[derive(Debug, Clone, Copy, Default)]
pub struct NoopDiscovery;

impl ConfigDiscovery for NoopDiscovery {
    fn commands(&self, _dir: &Path) -> Result<Value, CoreError> {
        Ok(Value::Object(Map::new()))
    }

    fn agents(&self, _dir: &Path) -> Result<Value, CoreError> {
        Ok(Value::Object(Map::new()))
    }

    fn modes(&self, _dir: &Path) -> Result<Value, CoreError> {
        Ok(Value::Object(Map::new()))
    }
}

// ---------------------------------------------------------------------------
// Loader parameters
// ---------------------------------------------------------------------------

/// Env-flag inputs to the loader (`Flag` in TS), captured explicitly so
/// tests do not mutate process state.
#[derive(Debug, Clone, Default)]
pub struct ConfigFlags {
    /// `OPENCODE_CONFIG` — a config file path.
    pub config: Option<String>,
    /// `OPENCODE_CONFIG_DIR` — an extra `.opencode`-style directory.
    pub config_dir: Option<String>,
    /// `OPENCODE_CONFIG_CONTENT` — inline config JSONC text.
    pub config_content: Option<String>,
    /// `OPENCODE_DISABLE_PROJECT_CONFIG`.
    pub disable_project_config: bool,
    /// `OPENCODE_DISABLE_MODELS_FETCH` (consumed by the catalog, carried
    /// for parity with the TS `Flag` surface).
    pub disable_models_fetch: bool,
    /// `OPENCODE_PERMISSION` — JSON merged into `permission`.
    pub permission: Option<String>,
    /// `OPENCODE_DISABLE_AUTOCOMPACT`.
    pub disable_autocompact: bool,
    /// `OPENCODE_DISABLE_PRUNE`.
    pub disable_prune: bool,
}

/// Inputs to [`ConfigLoader::load`] (`InstanceState` context in TS).
pub struct LoadParams {
    /// Instance cwd.
    pub directory: PathBuf,
    /// Walk/stop root for project files and `.opencode` discovery.
    pub worktree: Option<PathBuf>,
    /// Global paths (config/home); injectable for tests.
    pub paths: GlobalPaths,
    /// Managed (MDM) config dir override; defaults to
    /// `OPENCODE_TEST_MANAGED_CONFIG_DIR` or `/etc/opencode`.
    pub managed_config_dir: Option<PathBuf>,
    pub flags: ConfigFlags,
    pub auth: Box<dyn AuthProviders>,
    pub discovery: Box<dyn ConfigDiscovery>,
}

impl LoadParams {
    pub fn new(directory: PathBuf) -> LoadParams {
        LoadParams {
            directory,
            worktree: None,
            paths: GlobalPaths::from_env(),
            managed_config_dir: None,
            flags: ConfigFlags::default(),
            auth: Box::new(NoAuth),
            discovery: Box::new(super::agent::MarkdownDiscovery),
        }
    }

    pub fn worktree(mut self, worktree: PathBuf) -> LoadParams {
        self.worktree = Some(worktree);
        self
    }

    pub fn paths(mut self, paths: GlobalPaths) -> LoadParams {
        self.paths = paths;
        self
    }

    pub fn managed_config_dir(mut self, dir: PathBuf) -> LoadParams {
        self.managed_config_dir = Some(dir);
        self
    }

    pub fn flags(mut self, flags: ConfigFlags) -> LoadParams {
        self.flags = flags;
        self
    }

    pub fn auth(mut self, auth: Box<dyn AuthProviders>) -> LoadParams {
        self.auth = auth;
        self
    }

    pub fn discovery(mut self, discovery: Box<dyn ConfigDiscovery>) -> LoadParams {
        self.discovery = discovery;
        self
    }
}

// ---------------------------------------------------------------------------
// Loader
// ---------------------------------------------------------------------------

/// Loads and merges every config source into a wire-validated [`Config`].
#[derive(Debug, Clone, Copy, Default)]
pub struct ConfigLoader;

impl ConfigLoader {
    pub fn new() -> Self {
        ConfigLoader
    }

    /// Load and merge everything; returns the validated config plus the
    /// `.opencode` directory list (for later plugin/skill features).
    pub fn load(&self, params: &LoadParams) -> Result<(Config, Vec<PathBuf>), CoreError> {
        let auth_providers = params.auth.all();
        // authEnv: well-known provider credentials are visible to
        // `{env:VAR}` substitution in every later-loaded config file.
        let auth_env: HashMap<String, String> = auth_providers
            .iter()
            .map(|p| (p.key.clone(), p.token.clone()))
            .collect();

        let mut result = Value::Object(Map::new());

        // 1. Remote well-known configs (auth seam).
        for provider in &auth_providers {
            let Some((source, remote)) = params.auth.wellknown_config(provider, &auth_env)? else {
                continue;
            };
            let mut remote = remote;
            if !is_truthy(remote.get("$schema")) {
                if let Value::Object(fields) = &mut remote {
                    fields.insert("$schema".into(), Value::String(SCHEMA_REF.into()));
                }
            }
            let text = serde_json::to_string(&remote)
                .map_err(|e| CoreError::invalid(Path::new(&source), e.to_string()))?;
            let next = self.load_config(
                &text,
                Source::Virtual {
                    dir: parent_of(PathBuf::from(&source)),
                    source: source.clone(),
                },
                &auth_env,
            )?;
            merge_config_concat_arrays(&mut result, &next);
        }

        // 2. Global config dir (seeding + the three files). Global-load
        // failures degrade to `{}` with a warn — never fatal.
        let global = match self.load_global(params, &auth_env) {
            Ok(global) => global,
            Err(err) => {
                tracing::warn!("failed to load global config, using defaults: {err}");
                Value::Object(Map::new())
            }
        };
        merge_config_concat_arrays(&mut result, &global);

        // 4. OPENCODE_CONFIG env file (failures are fatal).
        if let Some(flag) = flags_value(&params.flags.config) {
            let next = self.load_file(Path::new(flag), &auth_env)?;
            merge_config_concat_arrays(&mut result, &next);
        }

        // 5. Project files, deepest last.
        if !params.flags.disable_project_config {
            let files = up(
                &["opencode.jsonc", "opencode.json"],
                &params.directory,
                params.worktree.as_deref(),
            );
            for file in files.into_iter().rev() {
                let next = self.load_file(&file, &auth_env)?;
                merge_config_concat_arrays(&mut result, &next);
            }
        }

        // result.agent ||= {}; result.mode ||= {}; result.plugin ||= []
        assign_if_falsy(&mut result, "agent", Value::Object(Map::new()));
        assign_if_falsy(&mut result, "mode", Value::Object(Map::new()));
        assign_if_falsy(&mut result, "plugin", Value::Array(Vec::new()));

        let directories = self.directories(params);

        // 6. .opencode directory sources.
        for dir in &directories {
            let dir_str = dir.to_string_lossy();
            let is_opencode_dir = dir_str.ends_with(".opencode")
                || flags_value(&params.flags.config_dir).is_some_and(|d| d == dir_str);
            if is_opencode_dir {
                for name in ["opencode.json", "opencode.jsonc"] {
                    let next = self.load_file(&dir.join(name), &auth_env)?;
                    merge_config_concat_arrays(&mut result, &next);
                    // result.agent ??= {}; result.mode ??= {}; result.plugin ??= []
                    assign_if_nullish(&mut result, "agent", Value::Object(Map::new()));
                    assign_if_nullish(&mut result, "mode", Value::Object(Map::new()));
                    assign_if_nullish(&mut result, "plugin", Value::Array(Vec::new()));
                }
            }

            ensure_gitignore(dir)?;

            // (npm install() calls are skipped in the Rust port.)

            // 6 (cont.) agent/command discovery — M3.3 seam.
            let commands = params.discovery.commands(dir)?;
            merge_deep(entry_or_empty(&mut result, "command"), &commands);
            let agents = params.discovery.agents(dir)?;
            merge_deep(entry_or_empty(&mut result, "agent"), &agents);
            let modes = params.discovery.modes(dir)?;
            merge_deep(entry_or_empty(&mut result, "agent"), &modes);
        }

        // 7. OPENCODE_CONFIG_CONTENT env.
        if let Some(content) = flags_value(&params.flags.config_content) {
            let next = self.load_config(
                content,
                Source::Virtual {
                    dir: params.directory.clone(),
                    source: "OPENCODE_CONFIG_CONTENT".to_owned(),
                },
                &auth_env,
            )?;
            merge_config_concat_arrays(&mut result, &next);
        }

        // 8. Active-org account config (auth seam).
        if let Some((source, account)) = params.auth.account_config()? {
            let text = serde_json::to_string(&account)
                .map_err(|e| CoreError::invalid(Path::new(&source), e.to_string()))?;
            let next = self.load_config(
                &text,
                Source::Virtual {
                    dir: parent_of(PathBuf::from(&source)),
                    source: source.clone(),
                },
                &auth_env,
            )?;
            merge_config_concat_arrays(&mut result, &next);
        }

        // 9. Managed config dir (MDM). Managed *preferences*
        // (mobileconfig) are macOS-specific; the Linux port reads the
        // managed-config dir only.
        let managed_dir = managed_config_dir(params);
        if managed_dir.exists() {
            for name in ["opencode.json", "opencode.jsonc"] {
                let next = self.load_file(&managed_dir.join(name), &HashMap::new())?;
                merge_config_concat_arrays(&mut result, &next);
            }
        }

        // 10. Post-merge normalization (config.ts:550-598).

        // mode entries fold into agent with mode "primary" forced.
        let modes = result
            .get("mode")
            .and_then(Value::as_object)
            .cloned()
            .unwrap_or_default();
        for (name, mode) in modes {
            let mut folded = mode;
            if let Value::Object(fields) = &mut folded {
                fields.insert("mode".into(), Value::String("primary".into()));
            }
            merge_deep(
                entry_or_empty(&mut result, "agent"),
                &json!({ name: folded }),
            );
        }

        // OPENCODE_PERMISSION parses as JSON and mergeDeeps into permission.
        if let Some(flag) = flags_value(&params.flags.permission) {
            match serde_json::from_str::<Value>(flag) {
                Ok(parsed) => {
                    let mut merged = nullish_or_empty(result.get("permission"));
                    merge_deep(&mut merged, &parsed);
                    result
                        .as_object_mut()
                        .unwrap()
                        .insert("permission".into(), merged);
                }
                Err(err) => {
                    tracing::warn!("OPENCODE_PERMISSION contains invalid JSON, skipping: {err}")
                }
            }
        }

        // tools folds into permission: enabled → "allow", else "deny";
        // write|edit|patch → edit; folded permissions merge *under* the
        // existing permission (existing wins).
        if let Some(Value::Object(tools)) = result.get("tools").cloned() {
            let mut perms = Map::new();
            for (tool, enabled) in tools {
                let action = if is_truthy(Some(&enabled)) {
                    "allow"
                } else {
                    "deny"
                };
                let key = match tool.as_str() {
                    "write" | "edit" | "patch" => "edit".to_owned(),
                    _ => tool,
                };
                perms.insert(key, Value::String(action.into()));
            }
            let mut merged = Value::Object(perms);
            let existing = nullish_or_empty(result.get("permission"));
            merge_deep(&mut merged, &existing);
            result
                .as_object_mut()
                .unwrap()
                .insert("permission".into(), merged);
        }

        // username defaults to the OS user name, else "user".
        if !is_truthy(result.get("username")) {
            result
                .as_object_mut()
                .unwrap()
                .insert("username".into(), Value::String(os_username()));
        }

        // autoshare === true && !share → share = "auto".
        if result.get("autoshare") == Some(&Value::Bool(true)) && !is_truthy(result.get("share")) {
            result
                .as_object_mut()
                .unwrap()
                .insert("share".into(), Value::String("auto".into()));
        }

        // compaction env flags: `{ ...compaction, auto|prune: false }`.
        if params.flags.disable_autocompact {
            let mut merged = result
                .get("compaction")
                .and_then(Value::as_object)
                .cloned()
                .unwrap_or_default();
            merged.insert("auto".into(), Value::Bool(false));
            result
                .as_object_mut()
                .unwrap()
                .insert("compaction".into(), Value::Object(merged));
        }
        if params.flags.disable_prune {
            let mut merged = result
                .get("compaction")
                .and_then(Value::as_object)
                .cloned()
                .unwrap_or_default();
            merged.insert("prune".into(), Value::Bool(false));
            result
                .as_object_mut()
                .unwrap()
                .insert("compaction".into(), Value::Object(merged));
        }

        let config = decode_config(&result, &params.directory)?;
        Ok((config, directories))
    }

    /// The global config dir: seed + merge `config.json`, `opencode.json`,
    /// `opencode.jsonc` (`config.ts:260-293`). Any failure propagates to the
    /// caller, which degrades the whole thing to `{}`.
    fn load_global(
        &self,
        params: &LoadParams,
        env: &HashMap<String, String>,
    ) -> Result<Value, CoreError> {
        // Seed the default global config with the schema for editor
        // completion, unless config is routed through env-provided paths.
        let unset = |flag: &Option<String>| flag.as_ref().is_none_or(|v| v.is_empty());
        if unset(&params.flags.config)
            && unset(&params.flags.config_dir)
            && unset(&params.flags.config_content)
        {
            let file = global_config_file(&params.paths.config);
            if !file.exists() {
                let seeded = json!({ "$schema": SCHEMA_REF });
                if let Ok(text) = serde_json::to_string_pretty(&seeded) {
                    let _ = fs::create_dir_all(&params.paths.config);
                    let _ = fs::write(&file, text);
                }
            }
        }

        let mut result = Value::Object(Map::new());
        for name in ["config.json", "opencode.json", "opencode.jsonc"] {
            let next = self.load_file(&params.paths.config.join(name), env)?;
            merge_config_concat_arrays(&mut result, &next);
        }

        // 3. Legacy TOML migration (`<config>/config`): not ported (M3 spec
        // S6) — recognize the directory and skip with a warn.
        let legacy = params.paths.config.join("config");
        if legacy.exists() {
            tracing::warn!(
                path = %legacy.display(),
                "legacy TOML config directory is not supported, skipping"
            );
        }

        Ok(result)
    }

    /// `ConfigPaths.directories` — global config dir, every `.opencode`
    /// from `directory` up to `worktree`, `.opencode` in home, then
    /// `OPENCODE_CONFIG_DIR`; unique, order-preserving.
    fn directories(&self, params: &LoadParams) -> Vec<PathBuf> {
        let mut list = vec![params.paths.config.clone()];
        if !params.flags.disable_project_config {
            list.extend(up(
                &[".opencode"],
                &params.directory,
                params.worktree.as_deref(),
            ));
        }
        list.extend(up(
            &[".opencode"],
            &params.paths.home,
            Some(params.paths.home.as_path()),
        ));
        if let Some(dir) = flags_value(&params.flags.config_dir) {
            list.push(PathBuf::from(dir));
        }

        let mut seen: Vec<PathBuf> = Vec::new();
        list.retain(|dir| {
            if seen.contains(dir) {
                false
            } else {
                seen.push(dir.clone());
                true
            }
        });
        list
    }

    /// `ConfigParse.jsonc` + substitution + schema validation for one config
    /// text. Returns the *raw* value — merging happens over Values (M3
    /// spec §2.2), decode only validates.
    fn load_config(
        &self,
        text: &str,
        source: Source,
        env: &HashMap<String, String>,
    ) -> Result<Value, CoreError> {
        let expanded = substitute(text, &source, env, Missing::Error)?;
        let source_path = source_name(&source);
        let value = parse_jsonc(&expanded, &source_path)?;
        decode_config(&value, &source_path)?;

        // Path-based configs get `$schema` seeded into the merged result
        // (TS `loadConfig` does this on every load; it also writes the file
        // back, which is not ported — it would destroy JSONC comments).
        let mut value = value;
        if matches!(source, Source::Path(_)) {
            if let Some(fields) = value.as_object_mut() {
                if !fields.contains_key("$schema") {
                    fields.insert("$schema".into(), Value::String(SCHEMA_REF.into()));
                }
            }
        }
        Ok(value)
    }

    /// `loadFile` — read, substitute, parse, validate. Missing or empty
    /// files contribute `{}`; failures are fatal to the caller.
    fn load_file(&self, path: &Path, env: &HashMap<String, String>) -> Result<Value, CoreError> {
        let text = match fs::read_to_string(path) {
            Ok(text) => text,
            Err(err) if err.kind() == ErrorKind::NotFound => return Ok(Value::Object(Map::new())),
            Err(err) if err.kind() == ErrorKind::PermissionDenied => {
                return Ok(Value::Object(Map::new()))
            }
            Err(err) => return Err(CoreError::invalid(path, err.to_string())),
        };
        if text.is_empty() {
            return Ok(Value::Object(Map::new()));
        }
        self.load_config(&text, Source::Path(path.to_path_buf()), env)
    }
}

/// `globalConfigFile()` — first existing of `opencode.jsonc`,
/// `opencode.json`, `config.json`, else `opencode.jsonc`.
fn global_config_file(config_dir: &Path) -> PathBuf {
    for name in ["opencode.jsonc", "opencode.json", "config.json"] {
        let candidate = config_dir.join(name);
        if candidate.exists() {
            return candidate;
        }
    }
    config_dir.join("opencode.jsonc")
}

/// `FSUtil.up` — collect existing `targets` walking `start` up to (and
/// including) `stop` (or the filesystem root when `None`).
fn up(targets: &[&str], start: &Path, stop: Option<&Path>) -> Vec<PathBuf> {
    let mut result = Vec::new();
    let mut current = start.to_path_buf();
    loop {
        for target in targets {
            let search = current.join(target);
            if search.exists() {
                result.push(search);
            }
        }
        if Some(current.as_path()) == stop {
            break;
        }
        let Some(parent) = current.parent() else {
            break;
        };
        let parent = parent.to_path_buf();
        if parent == current || parent.as_os_str().is_empty() {
            break;
        }
        current = parent;
    }
    result
}

/// `ensureGitignore` — create `dir` and drop a `.gitignore` into it.
fn ensure_gitignore(dir: &Path) -> Result<(), CoreError> {
    fs::create_dir_all(dir).map_err(|e| CoreError::invalid(dir, e.to_string()))?;
    let gitignore = dir.join(".gitignore");
    if !gitignore.exists() {
        if let Err(err) = fs::write(&gitignore, GITIGNORE_CONTENTS) {
            if err.kind() != ErrorKind::PermissionDenied {
                return Err(CoreError::invalid(dir, err.to_string()));
            }
        }
    }
    Ok(())
}

/// `ConfigManaged.managedConfigDir()`.
fn managed_config_dir(params: &LoadParams) -> PathBuf {
    if let Some(dir) = &params.managed_config_dir {
        return dir.clone();
    }
    std::env::var("OPENCODE_TEST_MANAGED_CONFIG_DIR")
        .ok()
        .filter(|v| !v.is_empty())
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("/etc/opencode"))
}

/// JS truthiness for a JSON value (absent counts as falsy).
fn is_truthy(value: Option<&Value>) -> bool {
    match value {
        None | Some(Value::Null) => false,
        Some(Value::Bool(b)) => *b,
        Some(Value::String(s)) => !s.is_empty(),
        Some(Value::Number(n)) => n.as_f64().is_some_and(|n| n != 0.0),
        Some(Value::Array(_)) | Some(Value::Object(_)) => true,
    }
}

/// A flag that is set only when non-empty (JS strings are falsy when empty).
fn flags_value(flag: &Option<String>) -> Option<&str> {
    flag.as_deref().filter(|v| !v.is_empty())
}

fn source_name(source: &Source) -> PathBuf {
    match source {
        Source::Path(path) => path.clone(),
        Source::Virtual { source, .. } => PathBuf::from(source),
    }
}

fn parent_of(path: PathBuf) -> PathBuf {
    path.parent().map(Path::to_path_buf).unwrap_or(path)
}

/// `result[key] ||= value` — assign when the current value is falsy.
fn assign_if_falsy(result: &mut Value, key: &str, value: Value) {
    if !is_truthy(result.get(key)) {
        result.as_object_mut().unwrap().insert(key.into(), value);
    }
}

/// `result[key] ??= value` — assign when the current value is nullish.
fn assign_if_nullish(result: &mut Value, key: &str, value: Value) {
    if matches!(result.get(key), None | Some(Value::Null)) {
        result.as_object_mut().unwrap().insert(key.into(), value);
    }
}

/// A mutable entry that defaults to `{}` (`result.key ?? {}`).
fn entry_or_empty<'a>(result: &'a mut Value, key: &str) -> &'a mut Value {
    let entry = result
        .as_object_mut()
        .unwrap()
        .entry(key.to_owned())
        .or_insert_with(|| Value::Object(Map::new()));
    if !entry.is_object() {
        *entry = Value::Object(Map::new());
    }
    entry
}

/// `value ?? {}` — an object value, or `{}` when nullish/absent.
fn nullish_or_empty(value: Option<&Value>) -> Value {
    match value {
        Some(value @ Value::Object(_)) => value.clone(),
        _ => Value::Object(Map::new()),
    }
}

/// The OS user name for the `username` default (`os.userInfo().username`).
fn os_username() -> String {
    for var in ["USER", "LOGNAME"] {
        if let Ok(user) = std::env::var(var) {
            if !user.is_empty() {
                return user;
            }
        }
    }
    "user".to_owned()
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicU64, Ordering};

    use serde_json::json;

    use super::*;

    fn temp_dir() -> PathBuf {
        static COUNTER: AtomicU64 = AtomicU64::new(0);
        let dir = std::env::temp_dir().join(format!(
            "opencode-core-m32-{}-{}",
            std::process::id(),
            COUNTER.fetch_add(1, Ordering::Relaxed),
        ));
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn write(path: &Path, contents: &str) {
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, contents).unwrap();
    }

    /// Fixture layout: `<root>/config` (global dir), `<root>/home`,
    /// `<root>/work` (project tree).
    struct Fixture {
        root: PathBuf,
        params: LoadParams,
    }

    impl Fixture {
        fn new() -> Fixture {
            let root = temp_dir();
            let work = root.join("work");
            fs::create_dir_all(&work).unwrap();
            let params = LoadParams::new(work.clone())
                .worktree(work)
                .paths(GlobalPaths {
                    home: root.join("home"),
                    config: root.join("config"),
                    data: root.join("data"),
                    cache: root.join("cache"),
                });
            Fixture { root, params }
        }

        fn load(&self) -> Result<(Config, Vec<PathBuf>), CoreError> {
            ConfigLoader.load(&self.params)
        }
    }

    #[test]
    fn global_files_merge_in_order() {
        let f = Fixture::new();
        let cfg = f.root.join("config");
        write(
            &cfg.join("config.json"),
            &json!({"model": "a/b", "shell": "sh"}).to_string(),
        );
        write(
            &cfg.join("opencode.json"),
            &json!({"model": "c/d"}).to_string(),
        );
        write(
            &cfg.join("opencode.jsonc"),
            &json!({"small_model": "e/f"}).to_string(),
        );
        let (config, _) = f.load().unwrap();
        // opencode.json merges after config.json; opencode.jsonc last.
        assert_eq!(config.model.as_deref(), Some("c/d"));
        assert_eq!(config.shell.as_deref(), Some("sh"));
        assert_eq!(config.small_model.as_deref(), Some("e/f"));
    }

    #[test]
    fn seeds_default_global_config_when_absent() {
        let f = Fixture::new();
        let (config, _) = f.load().unwrap();
        assert!(
            f.params.paths.config.join("opencode.jsonc").exists(),
            "default global config file is seeded"
        );
        assert_eq!(
            config.schema.as_deref(),
            Some("https://opencode.ai/config.json")
        );

        // No seeding when OPENCODE_CONFIG routes config elsewhere.
        let root = temp_dir();
        fs::create_dir_all(root.join("work")).unwrap();
        write(
            &root.join("custom.json"),
            &json!({"username": "x"}).to_string(),
        );
        let params = LoadParams::new(root.join("work"))
            .paths(GlobalPaths {
                home: root.clone(),
                config: root.join("config"),
                data: root.clone(),
                cache: root.clone(),
            })
            .flags(ConfigFlags {
                config: Some(root.join("custom.json").display().to_string()),
                ..Default::default()
            });
        let _ = ConfigLoader.load(&params).unwrap();
        assert!(!params.paths.config.join("opencode.jsonc").exists());
    }

    #[test]
    fn legacy_toml_dir_is_skipped_with_warn() {
        let f = Fixture::new();
        fs::create_dir_all(f.root.join("config").join("config")).unwrap();
        let (config, _) = f.load().unwrap();
        assert_eq!(config.model, None);
    }

    #[test]
    fn opencode_config_flag_overrides_global() {
        let mut f = Fixture::new();
        write(
            &f.root.join("config").join("opencode.json"),
            &json!({"model": "global/m"}).to_string(),
        );
        let custom = f.root.join("custom.json");
        write(&custom, &json!({"model": "custom/m"}).to_string());
        f.params.flags.config = Some(custom.display().to_string());
        let (config, _) = f.load().unwrap();
        assert_eq!(config.model.as_deref(), Some("custom/m"));
    }

    #[test]
    fn deepest_project_file_wins() {
        let mut f = Fixture::new();
        let worktree = f.root.join("work");
        write(
            &worktree.join("opencode.json"),
            &json!({"username": "root"}).to_string(),
        );
        write(
            &worktree.join("sub").join("opencode.jsonc"),
            &json!({"username": "deep"}).to_string(),
        );
        f.params.directory = worktree.join("sub");
        let (config, _) = f.load().unwrap();
        assert_eq!(config.username.as_deref(), Some("deep"));
    }

    #[test]
    fn project_walk_stops_at_worktree() {
        let f = Fixture::new();
        let outside = f.root.join("work");
        let inside = f.root.join("elsewhere");
        fs::create_dir_all(&inside).unwrap();
        write(
            &outside.join("opencode.json"),
            &json!({"shell": "outside-sh"}).to_string(),
        );
        write(
            &inside.join("opencode.json"),
            &json!({"shell": "inside-sh"}).to_string(),
        );
        let mut params = LoadParams::new(inside.clone())
            .worktree(inside.clone())
            .paths(f.params.paths.clone());
        params.directory = inside.clone();
        params.worktree = Some(inside);
        let (config, _) = ConfigLoader.load(&params).unwrap();
        assert_eq!(config.shell.as_deref(), Some("inside-sh"));
    }

    #[test]
    fn project_file_errors_are_fatal() {
        let f = Fixture::new();
        write(&f.root.join("work").join("opencode.json"), "{ not json");
        let err = f.load().unwrap_err();
        assert!(matches!(err, CoreError::Jsonc { .. }), "{err:?}");
    }

    #[test]
    fn global_errors_degrade_to_empty() {
        let f = Fixture::new();
        write(&f.root.join("config").join("opencode.json"), "{ not json");
        write(
            &f.root.join("work").join("opencode.json"),
            &json!({"shell": "project-sh"}).to_string(),
        );
        let (config, _) = f.load().unwrap();
        assert_eq!(config.shell.as_deref(), Some("project-sh"));
        // The whole global contribution degraded to {}.
        assert_eq!(config.model, None);
    }

    #[test]
    fn opencode_dir_overrides_project_file() {
        let f = Fixture::new();
        let work = f.root.join("work");
        write(
            &work.join("opencode.json"),
            &json!({"username": "project"}).to_string(),
        );
        write(
            &work.join(".opencode").join("opencode.json"),
            &json!({"username": "opencode-dir"}).to_string(),
        );
        let (config, _) = f.load().unwrap();
        assert_eq!(config.username.as_deref(), Some("opencode-dir"));
        assert!(
            work.join(".opencode").join(".gitignore").exists(),
            "gitignore is ensured"
        );
    }

    #[test]
    fn config_content_env_is_merged() {
        let mut f = Fixture::new();
        f.params.flags.config_content = Some(json!({"model": "content/m"}).to_string());
        let (config, _) = f.load().unwrap();
        assert_eq!(config.model.as_deref(), Some("content/m"));
    }

    #[test]
    fn managed_config_dir_merges_last() {
        let mut f = Fixture::new();
        let managed = f.root.join("managed");
        write(
            &managed.join("opencode.json"),
            &json!({"model": "managed/m"}).to_string(),
        );
        write(
            &f.root.join("work").join("opencode.json"),
            &json!({"model": "project/m"}).to_string(),
        );
        f.params.managed_config_dir = Some(managed);
        let (config, _) = f.load().unwrap();
        assert_eq!(config.model.as_deref(), Some("managed/m"));
    }

    #[test]
    fn tools_fold_into_permission() {
        let f = Fixture::new();
        write(
            &f.root.join("work").join("opencode.json"),
            &json!({
                "tools": { "write": true, "read": true, "bash": false },
                "permission": { "bash": "ask" }
            })
            .to_string(),
        );
        let (config, _) = f.load().unwrap();
        let permission = config.permission.unwrap();
        // write → edit, enabled → "allow"
        assert_eq!(
            permission.rules.get("edit"),
            Some(&crate::config::schema::PermissionRule::Action(
                crate::config::schema::PermissionAction::Allow
            ))
        );
        assert!(permission.rules.contains_key("read"));
        // existing permission wins over the folded value (bash: false
        // folds to "deny", but the config's own "ask" overrides it).
        assert_eq!(
            permission.rules.get("bash"),
            Some(&crate::config::schema::PermissionRule::Action(
                crate::config::schema::PermissionAction::Ask
            ))
        );
    }

    #[test]
    fn permission_env_merges_into_permission() {
        let mut f = Fixture::new();
        write(
            &f.root.join("work").join("opencode.json"),
            &json!({"permission": {"edit": "allow"}}).to_string(),
        );
        f.params.flags.permission = Some(json!({"bash": {"git push": "deny"}}).to_string());
        let (config, _) = f.load().unwrap();
        let permission = config.permission.unwrap();
        assert!(permission.rules.contains_key("edit"));
        assert!(permission.rules.contains_key("bash"));
    }

    #[test]
    fn permission_env_invalid_json_is_skipped() {
        let mut f = Fixture::new();
        write(
            &f.root.join("work").join("opencode.json"),
            &json!({"permission": {"edit": "allow"}}).to_string(),
        );
        f.params.flags.permission = Some("not json".to_owned());
        let (config, _) = f.load().unwrap();
        assert!(config.permission.unwrap().rules.contains_key("edit"));
    }

    #[test]
    fn mode_folds_into_agent_as_primary() {
        let f = Fixture::new();
        write(
            &f.root.join("work").join("opencode.json"),
            &json!({
                "mode": { "plan": { "prompt": "p", "mode": "subagent" } },
                "agent": { "build": { "prompt": "b" } }
            })
            .to_string(),
        );
        let (config, _) = f.load().unwrap();
        let plan = &config.agent.unwrap()["plan"];
        // mode: "primary" is forced — a user-set value is overwritten.
        assert_eq!(plan.mode, Some(crate::config::schema::AgentMode::Primary));
        assert_eq!(plan.prompt.as_deref(), Some("p"));
        // The `mode` key stays in the result after the fold (TS quirk).
        assert_eq!(
            config.mode.unwrap()["plan"].mode,
            Some(crate::config::schema::AgentMode::Subagent)
        );
    }

    #[test]
    fn username_defaults_to_os_user() {
        let f = Fixture::new();
        let (config, _) = f.load().unwrap();
        let username = config.username.expect("username defaulted");
        assert!(!username.is_empty());

        write(
            &f.root.join("work").join("opencode.json"),
            &json!({"username": "explicit"}).to_string(),
        );
        let (config, _) = f.load().unwrap();
        assert_eq!(config.username.as_deref(), Some("explicit"));
    }

    #[test]
    fn autoshare_folds_share() {
        let f = Fixture::new();
        write(
            &f.root.join("work").join("opencode.json"),
            &json!({"autoshare": true}).to_string(),
        );
        let (config, _) = f.load().unwrap();
        assert_eq!(config.share, Some(crate::config::schema::Share::Auto));
    }

    #[test]
    fn compaction_env_flags() {
        let mut f = Fixture::new();
        write(
            &f.root.join("work").join("opencode.json"),
            &json!({"compaction": {"auto": true, "prune": true, "reserved": 1}}).to_string(),
        );
        f.params.flags.disable_autocompact = true;
        f.params.flags.disable_prune = true;
        let (config, _) = f.load().unwrap();
        let compaction = config.compaction.unwrap();
        assert_eq!(compaction.auto, Some(false));
        assert_eq!(compaction.prune, Some(false));
        assert_eq!(compaction.reserved, Some(1));
    }

    #[test]
    fn instructions_concat_dedupe() {
        let f = Fixture::new();
        write(
            &f.root.join("config").join("opencode.json"),
            &json!({"instructions": ["a.md", "b.md"]}).to_string(),
        );
        write(
            &f.root.join("work").join("opencode.json"),
            &json!({"instructions": ["b.md", "c.md"]}).to_string(),
        );
        let (config, _) = f.load().unwrap();
        assert_eq!(
            config.instructions,
            Some(vec!["a.md".into(), "b.md".into(), "c.md".into()])
        );
    }

    #[test]
    fn json_null_overrides() {
        let f = Fixture::new();
        write(
            &f.root.join("config").join("opencode.json"),
            &json!({"model": "global/m"}).to_string(),
        );
        write(
            &f.root.join("work").join("opencode.json"),
            &json!({"model": null}).to_string(),
        );
        let (config, _) = f.load().unwrap();
        assert_eq!(config.model, None);
    }

    #[test]
    fn schema_is_seeded_into_path_configs_in_memory() {
        let f = Fixture::new();
        write(
            &f.root.join("work").join("opencode.json"),
            &json!({"shell": "sh"}).to_string(),
        );
        let (config, _) = f.load().unwrap();
        assert_eq!(
            config.schema.as_deref(),
            Some("https://opencode.ai/config.json")
        );
    }

    #[test]
    fn substitution_runs_against_each_file() {
        let f = Fixture::new();
        let work = f.root.join("work");
        write(&work.join("model.txt"), "local/m");
        write(
            &work.join("opencode.json"),
            &json!({"model": "{file:model.txt}"}).to_string(),
        );
        let (config, _) = f.load().unwrap();
        assert_eq!(config.model.as_deref(), Some("local/m"));
    }

    #[test]
    fn config_content_substitutes_against_project_directory() {
        let mut f = Fixture::new();
        write(&f.params.directory.join("token.txt"), "content/m");
        f.params.flags.config_content = Some(json!({"model": "{file:token.txt}"}).to_string());
        let (config, _) = f.load().unwrap();
        assert_eq!(config.model.as_deref(), Some("content/m"));
    }

    #[test]
    fn disable_project_config_skips_project_files() {
        let mut f = Fixture::new();
        write(
            &f.root.join("work").join("opencode.json"),
            &json!({"shell": "project-sh"}).to_string(),
        );
        f.params.flags.disable_project_config = true;
        let (config, _) = f.load().unwrap();
        assert_eq!(config.shell, None);
    }

    #[test]
    fn directories_list_is_returned() {
        let f = Fixture::new();
        fs::create_dir_all(f.root.join("work").join(".opencode")).unwrap();
        fs::create_dir_all(f.root.join("home").join(".opencode")).unwrap();
        let (_, directories) = f.load().unwrap();
        assert!(directories.contains(&f.root.join("config")));
        assert!(directories.contains(&f.root.join("work").join(".opencode")));
        assert!(directories.contains(&f.root.join("home").join(".opencode")));
    }

    struct FakeAuth {
        providers: Vec<AuthProvider>,
        remote: Option<(String, Value)>,
    }

    #[test]
    fn auth_env_feeds_substitution() {
        let mut f = Fixture::new();
        write(
            &f.root.join("config").join("opencode.json"),
            &json!({"model": "{env:OPENCODE_M3_AUTH_KEY}"}).to_string(),
        );
        f.params.auth = Box::new(FakeAuth {
            providers: vec![AuthProvider {
                url: "https://acme.test".into(),
                key: "OPENCODE_M3_AUTH_KEY".into(),
                token: "tok123".into(),
            }],
            remote: None,
        });
        let (config, _) = f.load().unwrap();
        assert_eq!(config.model.as_deref(), Some("tok123"));
    }

    #[test]
    fn remote_wellknown_config_merges() {
        let mut f = Fixture::new();
        f.params.auth = Box::new(FakeAuth {
            providers: vec![AuthProvider {
                url: "https://acme.test".into(),
                key: "OPENCODE_M3_AUTH_KEY".into(),
                token: "tok123".into(),
            }],
            remote: Some((
                "https://acme.test/.well-known/opencode".into(),
                json!({"model": "remote/m"}),
            )),
        });
        let (config, _) = f.load().unwrap();
        assert_eq!(config.model.as_deref(), Some("remote/m"));
    }

    impl AuthProviders for FakeAuth {
        fn all(&self) -> Vec<AuthProvider> {
            self.providers.clone()
        }

        fn wellknown_config(
            &self,
            _provider: &AuthProvider,
            _env: &HashMap<String, String>,
        ) -> Result<Option<(String, Value)>, CoreError> {
            Ok(self.remote.clone())
        }
    }

    #[test]
    fn up_walks_and_stops() {
        let root = temp_dir();
        fs::create_dir_all(root.join("a").join("b")).unwrap();
        let stop = root.join("a");
        assert_eq!(
            up(&["x"], &root.join("a").join("b"), Some(&stop)),
            vec![root.join("a").join("b").join("x")]
                .into_iter()
                .filter(|p| p.exists())
                .collect::<Vec<_>>()
        );
        fs::create_dir_all(root.join("a").join("b").join("x")).unwrap();
        let found = up(&["x"], &root.join("a").join("b"), Some(&stop));
        assert_eq!(found, vec![root.join("a").join("b").join("x")]);
    }
}
