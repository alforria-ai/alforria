//! PTY domain service — port of `packages/core/src/pty/pty.ts` plus the
//! per-location registry the route handlers resolve through.

pub mod protocol;
pub mod routes;
pub mod spawn;
pub mod ticket;
pub mod ws;

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use opencode_core::{CoreError, EventBus, PublishOptions};
use opencode_schema::location::LocationRef;
use opencode_schema::pty::{PtyCreateInput, PtyInfo, PtyStatus, PtyUpdateInput};

use crate::error::ServerError;
use crate::middleware::location::LocationContext;
use crate::pty::spawn::{Proc, SpawnBackend};

/// `BUFFER_LIMIT` (`pty.ts:14`) — UTF-16 code units.
const BUFFER_LIMIT: usize = 1024 * 1024 * 2;
/// `EXITED_LIMIT` (`pty.ts:17`).
const EXITED_LIMIT: usize = 25;

/// `Pty.NotFoundError` / `Pty.ExitedError` (`pty.ts:72-78`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PtyError {
    NotFound,
    Exited,
}

/// One `Subscriber` (`pty.ts:20-27`). The state is only mutated while the
/// service lock is held.
struct Subscriber {
    state: Mutex<SubscriberState>,
}

struct SubscriberState {
    on_data: Box<dyn Fn(&str) + Send>,
    on_end: Box<dyn Fn(Option<u64>) + Send>,
    active: bool,
    detached: bool,
    pending: Vec<String>,
    end: Option<Option<u64>>,
}

struct Session {
    info: PtyInfo,
    proc: Box<dyn Proc>,
    buffer: Vec<u16>,
    buffer_cursor: u64,
    cursor: u64,
    subscribers: HashMap<u64, Arc<Subscriber>>,
    next_subscriber: u64,
}

#[derive(Default)]
struct Inner {
    sessions: HashMap<String, Session>,
    exit_order: Vec<String>,
}

/// `Pty.Service` — one PTY registry per location
/// (`makeLocationNode`, keyed by `Location.Ref`).
pub struct PtyService {
    backend: Arc<dyn SpawnBackend>,
    events: Arc<EventBus>,
    location: LocationRef,
    directory: PathBuf,
    inner: Arc<Mutex<Inner>>,
}

impl PtyService {
    /// A standalone service with a private in-memory bus (tests).
    pub fn new(backend: Arc<dyn SpawnBackend>, directory: &str) -> PtyService {
        let storage =
            Arc::new(opencode_core::Storage::open_in_memory().expect("in-memory storage"));
        PtyService {
            backend,
            events: Arc::new(EventBus::new_shared(storage, None)),
            location: LocationRef {
                directory: directory.to_string(),
                workspace_id: None,
                project: None,
            },
            directory: PathBuf::from(directory),
            inner: Arc::new(Mutex::new(Inner::default())),
        }
    }

    /// `list` (`pty.ts:157-159`).
    pub fn list(&self) -> Vec<PtyInfo> {
        self.inner
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .sessions
            .values()
            .map(|session| session.info.clone())
            .collect()
    }

    /// `get` (`pty.ts:161-163`).
    pub fn get(&self, id: &str) -> Result<PtyInfo, PtyError> {
        self.inner
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .sessions
            .get(id)
            .map(|session| session.info.clone())
            .ok_or(PtyError::NotFound)
    }

    /// `create` (`pty.ts:165-244`).
    pub fn create(&self, input: &PtyCreateInput) -> Result<PtyInfo, ServerError> {
        let id = format!("pty_{}", ulid::Ulid::new());
        let command = match input.command.as_deref() {
            Some(command) if !command.is_empty() => command.to_string(),
            _ => preferred_shell(&self.directory.clone())?,
        };
        let mut args = input.args.clone().unwrap_or_default();
        if is_login_shell(&command) {
            args.push("-l".to_string());
        }
        let cwd = input
            .cwd
            .clone()
            .filter(|cwd| !cwd.is_empty())
            .map(PathBuf::from)
            .unwrap_or_else(|| self.directory.clone());
        let mut env = input.env.clone().unwrap_or_default();
        env.insert("TERM".to_string(), "xterm-256color".to_string());
        env.insert("OPENCODE_TERMINAL".to_string(), "1".to_string());
        let proc = self
            .backend
            .spawn(
                &command,
                &args,
                &spawn::SpawnOpts {
                    cwd: cwd.clone(),
                    env,
                },
            )
            .map_err(|err| defect(&err))?;
        let info = PtyInfo {
            id: id.clone(),
            title: input
                .title
                .clone()
                .filter(|title| !title.is_empty())
                .unwrap_or_else(|| format!("Terminal {}", &id[id.len().saturating_sub(4)..])),
            command,
            args,
            cwd: cwd.display().to_string(),
            status: PtyStatus::Running,
            pid: u64::from(proc.pid()),
            exit_code: None,
        };
        {
            let inner = Arc::clone(&self.inner);
            let data_id = id.clone();
            proc.on_data(Box::new(move |chunk| {
                handle_data(&inner, &data_id, chunk);
            }));
        }
        {
            let inner = Arc::clone(&self.inner);
            let events = Arc::clone(&self.events);
            let location = self.location.clone();
            let exit_id = id.clone();
            proc.on_exit(Box::new(move |exit| {
                handle_exit(&inner, &events, &location, &exit_id, exit);
            }));
        }
        let mut inner = self.inner.lock().unwrap_or_else(|p| p.into_inner());
        inner.sessions.insert(
            id.clone(),
            Session {
                info: info.clone(),
                proc,
                buffer: Vec::new(),
                buffer_cursor: 0,
                cursor: 0,
                subscribers: HashMap::new(),
                next_subscriber: 0,
            },
        );
        drop(inner);
        publish(
            &self.events,
            &self.location,
            "pty.created",
            serde_json::json!({ "info": info }),
        );
        Ok(info)
    }

    /// `update` (`pty.ts:246-252`).
    pub fn update(&self, id: &str, input: &PtyUpdateInput) -> Result<PtyInfo, PtyError> {
        let info = {
            let mut inner = self.inner.lock().unwrap_or_else(|p| p.into_inner());
            let session = inner.sessions.get_mut(id).ok_or(PtyError::NotFound)?;
            if let Some(title) = input.title.as_ref().filter(|title| !title.is_empty()) {
                session.info.title = title.clone();
            }
            if let Some(size) = &input.size {
                if matches!(session.info.status, PtyStatus::Running) {
                    session.proc.resize(size.cols as u16, size.rows as u16);
                }
            }
            session.info.clone()
        };
        publish(
            &self.events,
            &self.location,
            "pty.updated",
            serde_json::json!({ "info": info }),
        );
        Ok(info)
    }

    /// `remove` (`pty.ts:152-154` + `removeSession` `:141-150`).
    pub fn remove(&self, id: &str) -> Result<(), PtyError> {
        let mut inner = self.inner.lock().unwrap_or_else(|p| p.into_inner());
        let session = inner.sessions.remove(id).ok_or(PtyError::NotFound)?;
        inner.exit_order.retain(|entry| entry != id);
        remove_session(session);
        publish(
            &self.events,
            &self.location,
            "pty.deleted",
            serde_json::json!({ "id": id }),
        );
        Ok(())
    }

    /// `write` (`pty.ts:254-257`).
    pub fn write(&self, id: &str, data: &str) -> Result<(), PtyError> {
        let mut inner = self.inner.lock().unwrap_or_else(|p| p.into_inner());
        let session = inner.sessions.get_mut(id).ok_or(PtyError::NotFound)?;
        if matches!(session.info.status, PtyStatus::Running) {
            session.proc.write(data);
        }
        Ok(())
    }

    /// `attach` (`pty.ts:259-310`). `on_data`/`on_end` fire from the
    /// backend's reader/waiter threads and must stay non-blocking.
    pub fn attach(
        &self,
        id: &str,
        cursor: Option<i64>,
        on_data: Box<dyn Fn(&str) + Send>,
        on_end: Box<dyn Fn(Option<u64>) + Send>,
    ) -> Result<Attachment, PtyError> {
        let subscriber = Arc::new(Subscriber {
            state: Mutex::new(SubscriberState {
                on_data,
                on_end,
                active: false,
                detached: false,
                pending: Vec::new(),
                end: None,
            }),
        });
        let mut inner = self.inner.lock().unwrap_or_else(|p| p.into_inner());
        let session = inner.sessions.get_mut(id).ok_or(PtyError::NotFound)?;
        if !matches!(session.info.status, PtyStatus::Running) {
            return Err(PtyError::Exited);
        }
        let start = session.buffer_cursor;
        let end = session.cursor;
        let from = match cursor {
            Some(-1) => end,
            Some(cursor) => cursor.max(0) as u64,
            None => 0,
        };
        let replay = if session.buffer.is_empty() || from >= end {
            Vec::new()
        } else {
            let offset = from.saturating_sub(start) as usize;
            if offset >= session.buffer.len() {
                Vec::new()
            } else {
                session.buffer[offset..].to_vec()
            }
        };
        let token = session.next_subscriber;
        session.next_subscriber += 1;
        session.subscribers.insert(token, Arc::clone(&subscriber));
        Ok(Attachment {
            inner: Arc::clone(&self.inner),
            session_id: id.to_string(),
            subscriber,
            replay,
            cursor: end,
        })
    }
}

/// `Pty.attach`'s return value (`pty.ts:61-70`).
pub struct Attachment {
    inner: Arc<Mutex<Inner>>,
    session_id: String,
    subscriber: Arc<Subscriber>,
    /// Retained output from the requested cursor — UTF-16 code units
    /// (TS `replay: string`).
    pub replay: Vec<u16>,
    /// Absolute output cursor after replay.
    pub cursor: u64,
}

impl Attachment {
    /// `activate` (`pty.ts:292-302`) — starts live delivery after the
    /// caller has applied replay and cursor metadata.
    pub fn activate(&self) {
        let _inner = self.inner.lock().unwrap_or_else(|p| p.into_inner());
        let mut state = self
            .subscriber
            .state
            .lock()
            .unwrap_or_else(|p| p.into_inner());
        if state.active || state.detached {
            return;
        }
        state.active = true;
        let pending = std::mem::take(&mut state.pending);
        for chunk in pending {
            (state.on_data)(&chunk);
        }
        if let Some(end) = state.end.take() {
            (state.on_end)(end);
        }
    }

    /// `detach` (`pty.ts:303-308`).
    pub fn detach(&self) {
        let mut inner = self.inner.lock().unwrap_or_else(|p| p.into_inner());
        if let Some(session) = inner.sessions.get_mut(&self.session_id) {
            session
                .subscribers
                .retain(|_, subscriber| !Arc::ptr_eq(subscriber, &self.subscriber));
        }
        let mut state = self
            .subscriber
            .state
            .lock()
            .unwrap_or_else(|p| p.into_inner());
        state.detached = true;
        state.pending.clear();
        state.end = None;
    }
}

/// The `pty.onData` path (`pty.ts:203-221`).
fn handle_data(inner: &Arc<Mutex<Inner>>, id: &str, chunk: &str) {
    let mut inner = inner.lock().unwrap_or_else(|p| p.into_inner());
    let Some(session) = inner.sessions.get_mut(id) else {
        return;
    };
    let units: Vec<u16> = chunk.encode_utf16().collect();
    session.cursor += units.len() as u64;
    for subscriber in session.subscribers.values() {
        let mut state = subscriber.state.lock().unwrap_or_else(|p| p.into_inner());
        if !state.active {
            state.pending.push(chunk.to_string());
        } else {
            (state.on_data)(chunk);
        }
    }
    session.buffer.extend(&units);
    if session.buffer.len() > BUFFER_LIMIT {
        let excess = session.buffer.len() - BUFFER_LIMIT;
        session.buffer.drain(..excess);
        session.buffer_cursor += excess as u64;
    }
}

/// The `pty.onExit` path (`pty.ts:223-240`).
fn handle_exit(
    inner: &Arc<Mutex<Inner>>,
    events: &Arc<EventBus>,
    location: &LocationRef,
    id: &str,
    exit: spawn::Exit,
) {
    let mut inner = inner.lock().unwrap_or_else(|p| p.into_inner());
    let Some(session) = inner.sessions.get_mut(id) else {
        return;
    };
    if matches!(session.info.status, PtyStatus::Exited) {
        return;
    }
    session.info.status = PtyStatus::Exited;
    session.info.exit_code = exit.exit_code;
    notify_end(session, exit.exit_code);
    inner.exit_order.push(id.to_string());
    publish(
        events,
        location,
        "pty.exited",
        serde_json::json!({ "id": id, "exitCode": exit.exit_code.unwrap_or(0) }),
    );
    while inner.exit_order.len() > EXITED_LIMIT {
        let Some(oldest) = inner.exit_order.first().cloned() else {
            break;
        };
        if let Some(session) = inner.sessions.remove(&oldest) {
            inner.exit_order.remove(0);
            remove_session(session);
            publish(
                events,
                location,
                "pty.deleted",
                serde_json::json!({ "id": oldest }),
            );
        } else {
            break;
        }
    }
}

/// `teardown` (`pty.ts:116-125`) — kill + `notifyEnd({})`. The session must
/// already be out of the map.
fn remove_session(mut session: Session) {
    if matches!(session.info.status, PtyStatus::Running) {
        session.proc.kill();
    }
    notify_end(&mut session, None);
}

/// `notifyEnd` (`pty.ts:103-114`).
fn notify_end(session: &mut Session, exit_code: Option<u64>) {
    for subscriber in session.subscribers.values() {
        let mut state = subscriber.state.lock().unwrap_or_else(|p| p.into_inner());
        if !state.active {
            state.end = Some(exit_code);
        } else {
            (state.on_end)(exit_code);
        }
    }
    session.subscribers.clear();
}

fn defect(message: &str) -> ServerError {
    ServerError::Core(CoreError::Storage(message.to_string()))
}

fn publish(
    events: &EventBus,
    location: &LocationRef,
    r#type: &'static str,
    data: serde_json::Value,
) {
    let _ = events.publish(
        &opencode_core::event::definition::Definition::ephemeral(r#type),
        data,
        PublishOptions {
            location: Some(location.clone()),
            ..PublishOptions::default()
        },
    );
}

// ---------------------------------------------------------------------------
// Shell discovery (`packages/core/src/shell.ts`)
// ---------------------------------------------------------------------------

/// `META`'s `login: true` set (`shell.ts:5-14`).
fn is_login_shell(file: &str) -> bool {
    matches!(
        Path::new(file)
            .file_stem()
            .map(|n| n.to_string_lossy().to_lowercase())
            .as_deref(),
        Some("bash" | "dash" | "fish" | "ksh" | "sh" | "zsh")
    )
}

/// `Shell.preferred(configShell?)` (`shell.ts:205-212`) — the caller passes
/// the location directory so the configured shell can be resolved.
fn preferred_shell(directory: &Path) -> Result<String, ServerError> {
    let config = crate::routes::v1::util::load_config(directory)?;
    Ok(select_shell(config.shell.as_deref()))
}

fn select_shell(config_shell: Option<&str>) -> String {
    match config_shell {
        Some(shell) => resolve_shell(shell).unwrap_or_else(fallback_shell),
        None => std::env::var("SHELL")
            .ok()
            .and_then(|shell| resolve_shell(&shell))
            .unwrap_or_else(fallback_shell),
    }
}

fn resolve_shell(file: &str) -> Option<String> {
    let path = Path::new(file);
    if path.is_absolute() {
        return path.is_file().then(|| file.to_string());
    }
    which(file)
}

fn fallback_shell() -> String {
    if cfg!(target_os = "macos") {
        "/bin/zsh".to_string()
    } else {
        which("bash").unwrap_or_else(|| "/bin/sh".to_string())
    }
}

/// `util/which` — PATH search for an executable file.
fn which(cmd: &str) -> Option<String> {
    let path = std::env::var("PATH").ok()?;
    for dir in path.split(':') {
        let candidate = Path::new(dir).join(cmd);
        if candidate.is_file() {
            return Some(candidate.display().to_string());
        }
    }
    None
}

/// `Shell.list()` (`shell.ts:223-227`) — the `/pty/shells` wire shape.
pub fn shells() -> Vec<serde_json::Value> {
    let paths = unix_shell_paths();
    paths
        .iter()
        .filter(|shell| resolve_shell(shell).is_some())
        .map(|shell| {
            let name = Path::new(shell)
                .file_stem()
                .map(|n| n.to_string_lossy().to_lowercase())
                .unwrap_or_default();
            let acceptable = !matches!(name.as_str(), "nu" | "fish" | "powershell" | "pwsh");
            let name = if resolve_shell(&name).is_some() {
                name
            } else {
                shell.to_string()
            };
            serde_json::json!({
                "path": shell,
                "name": name,
                "acceptable": acceptable,
            })
        })
        .collect()
}

fn unix_shell_paths() -> Vec<String> {
    let text = std::fs::read_to_string("/etc/shells").unwrap_or_default();
    let entries: Vec<String> = text
        .lines()
        .filter(|line| !line.trim().is_empty() && !line.starts_with('#'))
        .map(|line| line.trim().to_string())
        .collect();
    if entries.is_empty() {
        ["/bin/bash", "/bin/zsh", "/bin/sh"]
            .iter()
            .map(|shell| shell.to_string())
            .collect()
    } else {
        entries
    }
}

// ---------------------------------------------------------------------------
// Per-location registry
// ---------------------------------------------------------------------------

type LocationKey = (PathBuf, Option<String>);
type PtyServiceMap = HashMap<LocationKey, Arc<PtyService>>;

/// One [`PtyService`] per `Location.Ref` (TS location-scoped `Pty.node`).
#[derive(Clone)]
pub struct PtyRegistry {
    backend: Arc<dyn SpawnBackend>,
    entries: Arc<Mutex<PtyServiceMap>>,
}

impl PtyRegistry {
    pub fn new(backend: Arc<dyn SpawnBackend>) -> PtyRegistry {
        PtyRegistry {
            backend,
            entries: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    /// Resolve (and lazily create) the PTY service for a location.
    pub fn resolve(&self, location: &LocationContext) -> Arc<PtyService> {
        let key = (location.directory.clone(), location.workspace_id.clone());
        let mut entries = self.entries.lock().unwrap_or_else(|p| p.into_inner());
        if let Some(service) = entries.get(&key) {
            return Arc::clone(service);
        }
        let service = Arc::new(PtyService {
            backend: Arc::clone(&self.backend),
            events: Arc::clone(&location.services.events),
            location: LocationRef {
                directory: location.directory.display().to_string(),
                workspace_id: location.workspace_id.clone(),
                project: None,
            },
            directory: location.directory.clone(),
            inner: Arc::new(Mutex::new(Inner::default())),
        });
        entries.insert(key, Arc::clone(&service));
        service
    }
}

impl Default for PtyRegistry {
    fn default() -> PtyRegistry {
        PtyRegistry::new(spawn::portable_backend())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn spawn_backend() -> Arc<dyn SpawnBackend> {
        spawn::portable_backend()
    }

    #[test]
    fn login_shell_matrix() {
        assert!(is_login_shell("/bin/bash"));
        assert!(is_login_shell("/usr/bin/zsh"));
        assert!(is_login_shell("sh"));
        assert!(!is_login_shell("/bin/nu"));
        assert!(!is_login_shell("/opt/powershell"));
    }

    #[test]
    fn which_finds_bin_sh() {
        assert!(which("sh").is_some());
        assert!(which("definitely-not-a-command-xyz").is_none());
    }

    #[test]
    fn shells_list_has_the_wire_shape() {
        let shells = shells();
        assert!(!shells.is_empty());
        for shell in &shells {
            assert!(shell.get("path").is_some());
            assert!(shell.get("name").is_some());
            assert!(shell.get("acceptable").is_some());
        }
    }

    #[test]
    fn create_lists_updates_and_removes_a_real_pty() {
        let service = PtyService::new(spawn_backend(), "/tmp");
        let created = service
            .create(&PtyCreateInput {
                command: Some("/bin/sh".to_string()),
                args: Some(vec!["-c".to_string(), "sleep 5".to_string()]),
                cwd: None,
                title: Some("test".to_string()),
                env: None,
            })
            .unwrap();
        assert!(created.id.starts_with("pty_"));
        assert_eq!(created.status, PtyStatus::Running);
        assert!(created.pid > 0);

        let found = service.get(&created.id).unwrap();
        assert_eq!(found.title, "test");

        let updated = service
            .update(
                &created.id,
                &PtyUpdateInput {
                    title: Some("renamed".to_string()),
                    size: None,
                },
            )
            .unwrap();
        assert_eq!(updated.title, "renamed");

        service.remove(&created.id).unwrap();
        assert_eq!(service.get(&created.id), Err(PtyError::NotFound));
        assert!(service.remove(&created.id).is_err());
    }

    #[test]
    fn exited_sessions_retain_output_until_removed() {
        let service = PtyService::new(spawn_backend(), "/tmp");
        let created = service
            .create(&PtyCreateInput {
                command: Some("/bin/sh".to_string()),
                args: Some(vec!["-c".to_string(), "echo hi; exit 4".to_string()]),
                cwd: None,
                title: None,
                env: None,
            })
            .unwrap();
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        loop {
            let info = service.get(&created.id).unwrap();
            if info.status == PtyStatus::Exited {
                assert_eq!(info.exit_code, Some(4));
                break;
            }
            assert!(std::time::Instant::now() < deadline, "session must exit");
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
    }

    #[test]
    fn attach_replays_buffer_from_cursor() {
        let service = PtyService::new(spawn_backend(), "/tmp");
        let created = service
            .create(&PtyCreateInput {
                command: Some("/bin/sh".to_string()),
                args: Some(vec![
                    "-c".to_string(),
                    "sleep 0.2; echo hello; sleep 30".to_string(),
                ]),
                cwd: None,
                title: None,
                env: None,
            })
            .unwrap();
        let wait_for_output = |min: usize| {
            let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
            loop {
                let cursor = {
                    let inner = service.inner.lock().unwrap_or_else(|p| p.into_inner());
                    inner
                        .sessions
                        .get(&created.id)
                        .map(|session| session.cursor)
                        .unwrap_or(0)
                };
                if cursor as usize >= min {
                    return cursor;
                }
                assert!(std::time::Instant::now() < deadline, "output must arrive");
                std::thread::sleep(std::time::Duration::from_millis(20));
            }
        };
        let cursor = wait_for_output("hello".len());

        let attachment = service
            .attach(&created.id, None, Box::new(|_| {}), Box::new(|_| {}))
            .unwrap();
        let replay = String::from_utf16_lossy(&attachment.replay);
        assert!(replay.contains("hello"));
        assert!(attachment.cursor >= cursor);

        let tailed = service
            .attach(&created.id, Some(-1), Box::new(|_| {}), Box::new(|_| {}))
            .unwrap();
        assert!(tailed.replay.is_empty());

        let ahead = service
            .attach(
                &created.id,
                Some(i64::try_from(attachment.cursor + 1000).unwrap()),
                Box::new(|_| {}),
                Box::new(|_| {}),
            )
            .unwrap();
        assert!(ahead.replay.is_empty());
        service.remove(&created.id).unwrap();
    }

    #[test]
    fn attach_reports_not_found_and_exited() {
        let service = PtyService::new(spawn_backend(), "/tmp");
        assert!(matches!(
            service.attach("pty_missing", None, Box::new(|_| {}), Box::new(|_| {})),
            Err(PtyError::NotFound)
        ));
        let created = service
            .create(&PtyCreateInput {
                command: Some("/bin/sh".to_string()),
                args: Some(vec!["-c".to_string(), "true".to_string()]),
                cwd: None,
                title: None,
                env: None,
            })
            .unwrap();
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        loop {
            let info = service.get(&created.id).unwrap();
            if info.status == PtyStatus::Exited {
                break;
            }
            assert!(std::time::Instant::now() < deadline);
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
        assert!(matches!(
            service.attach(&created.id, None, Box::new(|_| {}), Box::new(|_| {})),
            Err(PtyError::Exited)
        ));
    }
}
