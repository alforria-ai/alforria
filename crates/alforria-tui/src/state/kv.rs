//! `kv.tsx` — the `kv.json` store (M8.2).
//!
//! TS reference: `context/kv.tsx` (store) + `util/persistence.ts`
//! (`writeJsonAtomic`) + `core/src/util/flock.ts` (`Flock.withLock`).
//!
//! Wire compatibility: a TS TUI and a Rust TUI alternate over the same
//! state dir — same file names (`kv.json`), same JSON shapes, same
//! atomic-write discipline (temp file + rename) and the same on-disk
//! flock protocol (`<state>/locks/<sha1(key)>.lock` with `heartbeat` +
//! `meta.json`).

use std::fs;
use std::os::unix::fs::DirBuilderExt;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use anyhow::{bail, Context, Result};
use serde_json::{json, Map, Value};

// ------------------------------------------------------------- keys

/// kv keys the TUI reads, with their seed defaults (`kv.signal` sites
/// across the TS package). `paste_summary_enabled` derives its default
/// from config at each call site (`prompt/index.tsx:1209`), so it has no
/// constant here.
pub mod keys {
    pub const SESSION_DIRECTORY_FILTER_ENABLED: &str = "session_directory_filter_enabled";
    pub const TERMINAL_TITLE_ENABLED: &str = "terminal_title_enabled";
    pub const PASTE_SUMMARY_ENABLED: &str = "paste_summary_enabled";
    pub const ANIMATIONS_ENABLED: &str = "animations_enabled";
    pub const FILE_CONTEXT_ENABLED: &str = "file_context_enabled";
    pub const DIFF_WRAP_MODE: &str = "diff_wrap_mode";
    pub const SIDEBAR: &str = "sidebar";
    pub const TIMESTAMPS: &str = "timestamps";
    pub const TOOL_DETAILS_VISIBILITY: &str = "tool_details_visibility";
    pub const ASSISTANT_METADATA_VISIBILITY: &str = "assistant_metadata_visibility";
    pub const SCROLLBAR_VISIBLE: &str = "scrollbar_visible";
    pub const GENERIC_TOOL_OUTPUT_VISIBILITY: &str = "generic_tool_output_visibility";
    pub const SKIPPED_VERSION: &str = "skipped_version";
    pub const SHARE_CONSENT: &str = "share_consent";
    pub const THINKING_MODE: &str = "thinking_mode";
    pub const GO_UPSELL_FREE_TIER_LAST_SEEN_AT: &str = "go_upsell_last_seen_at";
    pub const GO_UPSELL_FREE_TIER_DONT_SHOW: &str = "go_upsell_dont_show";
    pub const GO_UPSELL_RATE_LIMIT_LAST_SEEN_AT: &str = "go_upsell_account_rate_limit_last_seen_at";
    pub const GO_UPSELL_RATE_LIMIT_DONT_SHOW: &str = "go_upsell_account_rate_limit_dont_show";
    pub const THEME: &str = "theme";
    pub const THEME_MODE: &str = "theme_mode";
    pub const THEME_MODE_LOCK: &str = "theme_mode_lock";
}

/// The seed default for `kv.signal(name, default)` — `None` for keys
/// with no constant default.
pub fn seed_default(key: &str) -> Option<Value> {
    match key {
        keys::SESSION_DIRECTORY_FILTER_ENABLED
        | keys::TERMINAL_TITLE_ENABLED
        | keys::ANIMATIONS_ENABLED
        | keys::FILE_CONTEXT_ENABLED
        | keys::TOOL_DETAILS_VISIBILITY
        | keys::ASSISTANT_METADATA_VISIBILITY => Some(json!(true)),
        keys::DIFF_WRAP_MODE => Some(json!("word")),
        keys::SIDEBAR => Some(json!("auto")),
        keys::TIMESTAMPS => Some(json!("hide")),
        keys::SCROLLBAR_VISIBLE | keys::GENERIC_TOOL_OUTPUT_VISIBILITY => Some(json!(false)),
        keys::THINKING_MODE => Some(json!("hide")),
        _ => None,
    }
}

// --------------------------------------------------------- persistence

/// `writeJsonAtomic` (`util/persistence.ts:12-21`): write to
/// `<file>.<pid>.<token>.tmp`, then rename. No trailing newline,
/// matching `JSON.stringify`.
pub(crate) fn write_json_atomic(path: &Path, value: &Value) -> Result<()> {
    let temporary = path.with_extension(format!(
        "{}.{}.{}.tmp",
        std::process::id(),
        lock_token(),
        path.extension()
            .map(|x| x.to_string_lossy())
            .unwrap_or_default(),
    ));
    if let Some(parent) = temporary.parent() {
        fs::create_dir_all(parent).ok();
    }
    let result = (|| -> Result<()> {
        fs::write(&temporary, serde_json::to_string(value)?)?;
        fs::rename(&temporary, path)?;
        Ok(())
    })();
    if result.is_err() {
        let _ = fs::remove_file(&temporary);
    }
    result
}

/// `readJson` (`util/persistence.ts:4-6`).
pub(crate) fn read_json(path: &Path) -> Result<Value> {
    Ok(serde_json::from_str(&fs::read_to_string(path)?)?)
}

// -------------------------------------------------------------- flock

const STALE_MS: u64 = 60_000;
const TIMEOUT_MS: u64 = 5 * 60_000;
const BASE_DELAY_MS: u64 = 100;
const MAX_DELAY_MS: u64 = 2_000;

static TOKEN_COUNTER: AtomicU64 = AtomicU64::new(0);

fn lock_token() -> String {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    format!(
        "{}-{}-{}",
        std::process::id(),
        nanos,
        TOKEN_COUNTER.fetch_add(1, Ordering::Relaxed)
    )
}

/// `Hash.fast` (`core/src/util/hash.ts:3-6`): sha1 hex.
fn sha1_hex(input: &str) -> String {
    use sha1::{Digest, Sha1};
    hex::encode(Sha1::digest(input.as_bytes()))
}

fn wall_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

fn mtime_ms(path: &Path) -> Option<u64> {
    fs::metadata(path)
        .and_then(|m| m.modified())
        .ok()
        .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
        .map(|d| d.as_millis() as u64)
}

/// Owned `Flock` lease. Release is token-checked like the TS original
/// (`flock.ts:250-266`).
pub(crate) struct FlockGuard {
    lock_dir: PathBuf,
    token: String,
}

impl FlockGuard {
    #[cfg(test)]
    pub fn release(self) {
        drop(self);
    }
}

impl Drop for FlockGuard {
    fn drop(&mut self) {
        if let Ok(raw) = fs::read_to_string(self.lock_dir.join("meta.json")) {
            let meta = serde_json::from_str::<Value>(&raw).unwrap_or(Value::Null);
            if meta.get("token").and_then(Value::as_str) != Some(self.token.as_str()) {
                return;
            }
        }
        let _ = fs::remove_dir_all(&self.lock_dir);
    }
}

fn is_stale(lock_dir: &Path) -> bool {
    let now = wall_ms();
    if let Some(mtime) = mtime_ms(&lock_dir.join("heartbeat")) {
        return now.saturating_sub(mtime) > STALE_MS;
    }
    if let Some(mtime) = mtime_ms(&lock_dir.join("meta.json")) {
        return now.saturating_sub(mtime) > STALE_MS;
    }
    match mtime_ms(lock_dir) {
        Some(mtime) => now.saturating_sub(mtime) > STALE_MS,
        None => false,
    }
}

fn try_acquire(lock_dir: &Path, token: &str) -> Result<bool> {
    match fs::DirBuilder::new().mode(0o700).create(lock_dir) {
        Ok(()) => {}
        Err(err) if err.kind() == std::io::ErrorKind::AlreadyExists => {
            if !is_stale(lock_dir) {
                return Ok(false);
            }
            let breaker = PathBuf::from(format!("{}.breaker", lock_dir.display()));
            match fs::DirBuilder::new().mode(0o700).create(&breaker) {
                Ok(()) => {
                    let acquired = (|| -> Result<bool> {
                        if !is_stale(lock_dir) {
                            return Ok(false);
                        }
                        let _ = fs::remove_dir_all(lock_dir);
                        match fs::DirBuilder::new().mode(0o700).create(lock_dir) {
                            Ok(()) => Ok(true),
                            Err(err) => {
                                if err.kind() == std::io::ErrorKind::AlreadyExists {
                                    return Ok(false);
                                }
                                Err(err.into())
                            }
                        }
                    })();
                    let _ = fs::remove_dir_all(&breaker);
                    if !acquired? {
                        return Ok(false);
                    }
                }
                Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(false),
                Err(_) => return Ok(false),
            }
        }
        Err(err) => return Err(err.into()),
    }
    let meta = json!({
        "token": token,
        "pid": std::process::id(),
        "hostname": std::env::var("HOSTNAME").unwrap_or_else(|_| "unknown".to_string()),
        "createdAt": wall_ms(),
    });
    fs::write(lock_dir.join("heartbeat"), "")?;
    fs::write(
        lock_dir.join("meta.json"),
        serde_json::to_string_pretty(&meta)?,
    )?;
    Ok(true)
}

/// Acquire the cross-process lock for `key` (`flock.ts:acquire`).
pub(crate) fn acquire_flock(state_dir: &Path, key: &str) -> Result<FlockGuard> {
    let root = state_dir.join("locks");
    fs::create_dir_all(&root).with_context(|| format!("lock root for {key}"))?;
    let lock_dir = root.join(format!("{}.lock", sha1_hex(key)));
    let token = lock_token();
    let deadline = Instant::now() + Duration::from_millis(TIMEOUT_MS);
    let mut delay = BASE_DELAY_MS;
    loop {
        if try_acquire(&lock_dir, &token)? {
            return Ok(FlockGuard { lock_dir, token });
        }
        if Instant::now() > deadline {
            bail!("Timed out waiting for lock: {key}");
        }
        // `jitter(flock.ts:110-114)` — ±30%.
        let jitter = (delay * 3 / 10) as i64;
        let spread = rand_jitter() % (2 * jitter + 1) - jitter;
        std::thread::sleep(Duration::from_millis((delay as i64 + spread).max(0) as u64));
        delay = (delay * 17 / 10).min(MAX_DELAY_MS);
    }
}

fn rand_jitter() -> i64 {
    // xorshift over the nanosecond clock — only feeds ±30% timing jitter.
    let mut x = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.subsec_nanos() as u64)
        .unwrap_or(0x9e3779b9)
        | 1;
    x ^= x << 13;
    x ^= x >> 7;
    x ^= x << 17;
    (x % 1000) as i64
}

/// `Flock.withLock(key, fn)` (`flock.ts:withLock`).
pub(crate) fn with_flock<T>(
    state_dir: &Path,
    key: &str,
    f: impl FnOnce() -> Result<T>,
) -> Result<T> {
    let _guard = acquire_flock(state_dir, key)?;
    f()
}

// ---------------------------------------------------------------- store

/// The `kv.json` store — `context/kv.tsx`. `file` is `None` for
/// in-memory instances (unit tests).
pub struct Kv {
    data: Map<String, Value>,
    file: Option<PathBuf>,
    state_dir: Option<PathBuf>,
}

impl Default for Kv {
    fn default() -> Kv {
        Kv {
            data: Map::new(),
            file: None,
            state_dir: None,
        }
    }
}

impl Kv {
    /// Load `<state>/kv.json` under flock (`kv.tsx:22-31`). Read errors
    /// are logged by TS and leave the store empty.
    pub fn load(state_dir: &Path) -> Kv {
        let file = state_dir.join("kv.json");
        let lock = format!("tui-kv:{}", file.display());
        let data = with_flock(state_dir, &lock, || read_json(&file))
            .and_then(|value| match value {
                Value::Object(map) => Ok(map),
                _ => bail!("kv.json is not an object"),
            })
            .unwrap_or_else(|error| {
                eprintln!("Failed to read Kv state: {error}");
                Map::new()
            });
        Kv {
            data,
            file: Some(file),
            state_dir: Some(state_dir.to_path_buf()),
        }
    }

    /// A store with no persistence behind it.
    pub fn in_memory() -> Kv {
        Kv::default()
    }

    /// `kv.get(key, defaultValue)` (`kv.tsx:51-53`) — `store[key] ??
    /// defaultValue`.
    pub fn get(&self, key: &str, default: Value) -> Value {
        match self.data.get(key) {
            Some(value) if !value.is_null() => value.clone(),
            _ => default,
        }
    }

    /// Convenience wrapper for `kv.get(key, true)` truthiness checks.
    pub fn get_bool(&self, key: &str, default: bool) -> bool {
        self.get(key, json!(default)) == json!(true)
    }

    /// `kv.signal` seeding (`kv.tsx:40-50`): write the default into the
    /// in-memory store when absent — no persistence.
    pub fn get_seeded(&mut self, key: &str, default: Value) -> Value {
        if self.data.get(key).is_none() {
            self.data.insert(key.to_string(), default.clone());
        }
        self.get(key, default)
    }

    /// `kv.set(key, value)` (`kv.tsx:54-62`): mutate, snapshot-clone the
    /// whole store, serialized atomic write under flock.
    pub fn set(&mut self, key: &str, value: Value) {
        self.data.insert(key.to_string(), value);
        self.persist();
    }

    pub fn data(&self) -> &Map<String, Value> {
        &self.data
    }

    pub fn file(&self) -> Option<&Path> {
        self.file.as_deref()
    }

    fn persist(&self) {
        let (Some(file), Some(state_dir)) = (&self.file, &self.state_dir) else {
            return;
        };
        let snapshot = self.data.clone();
        let file = file.clone();
        let lock = format!("tui-kv:{}", file.display());
        let result = with_flock(state_dir, &lock, || {
            write_json_atomic(&file, &Value::Object(snapshot.clone()))
        });
        if let Err(error) = result {
            eprintln!("Failed to write Kv state: {error}");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn get_returns_default_when_missing() {
        let mut kv = Kv::in_memory();
        assert_eq!(kv.get("missing", json!(true)), json!(true));
        kv.set("present", json!(false));
        assert_eq!(kv.get("present", json!(true)), json!(false));
    }

    #[test]
    fn null_value_falls_back_to_default() {
        let mut kv = Kv::in_memory();
        kv.set("null", Value::Null);
        assert_eq!(kv.get("null", json!("fallback")), json!("fallback"));
    }

    #[test]
    fn seeding_writes_the_default_without_persisting() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("kv.json");
        let mut kv = Kv {
            data: Map::new(),
            file: Some(path.clone()),
            state_dir: Some(dir.path().to_path_buf()),
        };
        kv.get_seeded("seeded", json!(42));
        assert_eq!(kv.get("seeded", json!(0)), json!(42));
        assert!(!path.exists(), "signal seeding must not persist");
        // `kv.set` snapshots the whole store (`kv.tsx:54-59`), so the
        // seeded default rides along once any write happens.
        kv.set("persisted", json!("x"));
        let round = Kv::load(dir.path());
        assert_eq!(round.get("seeded", json!(0)), json!(42));
        assert_eq!(round.get("persisted", json!("y")), json!("x"));
    }

    #[test]
    fn round_trips_through_disk_with_exact_shape() {
        let dir = tempfile::tempdir().unwrap();
        let mut kv = Kv::load(dir.path());
        kv.set("session_directory_filter_enabled", json!(false));
        kv.set("nested", json!({"a": [1, 2]}));
        kv.set("removed_later", json!(1));
        kv.set("removed_later", Value::Null);
        let round = Kv::load(dir.path());
        assert_eq!(round.get("nested", json!(null)), json!({"a": [1, 2]}));
        assert!(!round.get_bool("session_directory_filter_enabled", true));
        // TS JSON.stringify drops nothing — undefined keys never exist.
        assert_eq!(round.data().get("removed_later"), Some(&Value::Null));
        let raw = std::fs::read_to_string(dir.path().join("kv.json")).unwrap();
        assert!(!raw.ends_with('\n'), "JSON.stringify has no newline");
        assert!(!raw.contains(' '), "serde_json compacts like stringify");
    }

    #[test]
    fn flock_takes_over_a_stale_lock() {
        let dir = tempfile::tempdir().unwrap();
        let lock_dir = dir
            .path()
            .join("locks")
            .join(format!("{}.lock", sha1_hex("abc")));
        fs::create_dir_all(&lock_dir).unwrap();
        fs::write(lock_dir.join("heartbeat"), "").unwrap();
        // Age the heartbeat beyond STALE_MS by rewriting mtime.
        let old = std::time::SystemTime::now() - Duration::from_millis(STALE_MS + 5000);
        let file = std::fs::File::options()
            .write(true)
            .open(lock_dir.join("heartbeat"))
            .unwrap();
        file.set_modified(old).unwrap();
        let guard = acquire_flock(dir.path(), "abc").expect("takes over stale lock");
        assert!(lock_dir.join("meta.json").exists());
        guard.release();
        assert!(!lock_dir.exists(), "release removes the lock dir");
    }

    #[test]
    fn seed_defaults_table() {
        assert_eq!(seed_default(keys::DIFF_WRAP_MODE), Some(json!("word")));
        assert_eq!(seed_default(keys::SIDEBAR), Some(json!("auto")));
        assert_eq!(seed_default(keys::TIMESTAMPS), Some(json!("hide")));
        assert_eq!(seed_default(keys::SCROLLBAR_VISIBLE), Some(json!(false)));
        assert_eq!(seed_default(keys::ANIMATIONS_ENABLED), Some(json!(true)));
        assert_eq!(seed_default("nope"), None);
    }
}
