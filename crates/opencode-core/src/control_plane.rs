//! Control-plane move-session — port of
//! `packages/core/src/control-plane/move-session.ts`.

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::Arc;

use crate::event::definition::{Definition, DurableSpec};
use crate::git::GitRunner;
use crate::session::SessionStore;
use crate::{Clock, EventBus};

/// `SessionEvent.Moved` (`schema-src/session-event.ts`) — a V2 durable
/// event, aggregate `sessionID`, manifest version 1.
pub const SESSION_NEXT_MOVED: Definition = Definition {
    r#type: "session.next.moved",
    durable: Some(DurableSpec {
        aggregate: "sessionID",
        version: 1,
    }),
};

/// `MoveSession.Input` (`move-session.ts:21-25`).
#[derive(Debug, Clone)]
pub struct Input {
    pub session_id: String,
    /// `destination.directory`.
    pub destination: String,
    pub move_changes: bool,
}

/// `MoveSession.Error` (`move-session.ts:28-61`).
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum Error {
    /// `SessionV2.NotFoundError`.
    #[error("Session not found: {session_id}")]
    NotFound { session_id: String },
    /// `DestinationProjectMismatchError`.
    #[error("DestinationProjectMismatchError: expected {expected}, actual {actual}")]
    DestinationProjectMismatch { expected: String, actual: String },
    #[error("ApplyChangesError: {message}")]
    ApplyChanges { message: String },
    #[error("CaptureChangesError: {message}")]
    CaptureChanges { message: String },
    #[error("ResetSourceChangesError: {directory:?}, {message}")]
    ResetSourceChanges { directory: PathBuf, message: String },
}

impl Error {
    /// `message(error)` (`handlers/control-plane.ts:30-37`).
    pub fn message(&self) -> String {
        match self {
            Error::NotFound { session_id } => format!("Session not found: {session_id}"),
            Error::DestinationProjectMismatch { .. } => {
                "Destination directory belongs to another project".to_string()
            }
            Error::ApplyChanges { .. } => {
                "Unable to apply your changes in the destination directory. The files may conflict with existing changes.".to_string()
            }
            Error::CaptureChanges { message } => message.clone(),
            Error::ResetSourceChanges { message, .. } => message.clone(),
        }
    }
}

/// The handler-facing failure: a typed error (400 `MoveSessionError`) or a
/// defect routed through the error middleware (500).
#[derive(Debug, thiserror::Error)]
pub enum MoveSessionError {
    #[error(transparent)]
    Known(#[from] Error),
    #[error("{0}")]
    Defect(String),
}

/// `path.relative(from, to)` with `/` separators — empty when equal
/// (`git.ts:737`, `move-session.ts:109`).
fn relative_scope(from: &Path, to: &Path) -> String {
    let skip = |c: &std::path::Component<'_>| !matches!(c, std::path::Component::CurDir);
    let from: Vec<_> = from.components().filter(skip).collect();
    let to: Vec<_> = to.components().filter(skip).collect();
    let mut common = 0;
    while common < from.len() && common < to.len() && from[common] == to[common] {
        common += 1;
    }
    let mut out = PathBuf::new();
    for _ in common..from.len() {
        out.push("..");
    }
    for part in &to[common..] {
        out.push(part.as_os_str());
    }
    out.display().to_string()
}

fn result_message(stderr: &str, text: &str, fallback: &str) -> String {
    if !stderr.trim().is_empty() {
        return stderr.trim().to_string();
    }
    if !text.trim().is_empty() {
        return text.trim().to_string();
    }
    fallback.to_string()
}

/// `Git.change.capture` (`git.ts:729-789`) — tracked diff + per-file
/// untracked diffs, joined with newlines. Empty string when nothing changed.
pub fn capture_changes(
    git: &dyn GitRunner,
    repository: &crate::git::Repository,
    path: &Path,
) -> Result<String, String> {
    let scope = relative_scope(&repository.worktree, path);
    let scope = if scope.is_empty() {
        ".".to_string()
    } else {
        scope
    };

    let tracked = git.run(
        Some(&repository.worktree),
        &["diff", "--binary", "HEAD", "--", &scope],
    );
    if tracked.exit_code != 0 {
        return Err(result_message(
            &tracked.stderr,
            &tracked.text,
            "Failed to capture tracked changes",
        ));
    }

    let untracked = git.run(
        Some(&repository.worktree),
        &[
            "ls-files",
            "--others",
            "--exclude-standard",
            "-z",
            "--",
            &scope,
        ],
    );
    if untracked.exit_code != 0 {
        return Err(result_message(
            &untracked.stderr,
            &untracked.text,
            "Failed to list untracked changes",
        ));
    }

    let mut created: Vec<String> = Vec::new();
    for file in untracked.text.split('\0').filter(|item| !item.is_empty()) {
        let result = git.run(
            Some(&repository.worktree),
            &["diff", "--binary", "--no-index", "--", "/dev/null", file],
        );
        // git diff --no-index returns 1 when differences were found.
        if result.exit_code != 0 && result.exit_code != 1 {
            return Err(result_message(
                &result.stderr,
                &result.text,
                &format!("Failed to capture untracked change: {file}"),
            ));
        }
        created.push(result.text);
    }
    let mut parts = vec![tracked.text];
    parts.extend(created);
    Ok(parts
        .into_iter()
        .filter(|part| !part.is_empty())
        .collect::<Vec<_>>()
        .join("\n"))
}

/// `Git.change.apply` (`git.ts:790-814`) — `git apply -` with the patch on
/// stdin.
pub fn apply_changes(path: &Path, changes: &str) -> Result<(), String> {
    let output = Command::new("git")
        .args(["apply", "-"])
        .current_dir(path)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .and_then(|mut child| {
            if let Some(stdin) = child.stdin.as_mut() {
                use std::io::Write;
                let _ = stdin.write_all(changes.as_bytes());
            }
            child.wait_with_output()
        });
    let output = match output {
        Ok(output) => output,
        Err(err) => return Err(err.to_string()),
    };
    if output.status.code() == Some(0) {
        return Ok(());
    }
    let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
    let text = String::from_utf8_lossy(&output.stdout).trim().to_string();
    if !stderr.is_empty() {
        return Err(stderr);
    }
    if !text.is_empty() {
        return Err(text);
    }
    Err("Failed to apply changes".to_string())
}

/// `Git.change.discard` `index` (`git.ts:816-826`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ChangeIndex {
    Preserve,
    Reset,
}

/// `Git.change.discard` `untracked` (`git.ts:827-839`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ChangeUntracked {
    Preserve,
    Remove,
}

/// `Git.change.discard` (`git.ts:816-839`).
pub fn discard_changes(
    git: &dyn GitRunner,
    repository: &crate::git::Repository,
    path: &Path,
    index: ChangeIndex,
    untracked: ChangeUntracked,
) -> Result<(), String> {
    let scope = relative_scope(&repository.worktree, path);
    let scope = if scope.is_empty() {
        ".".to_string()
    } else {
        scope
    };
    let restore_args: Vec<&str> = match index {
        ChangeIndex::Reset => vec!["checkout", "HEAD", "--", &scope],
        ChangeIndex::Preserve => vec!["checkout", "--", &scope],
    };
    let restore = git.run(Some(&repository.worktree), &restore_args);
    if restore.exit_code != 0 {
        return Err(result_message(
            &restore.stderr,
            &restore.text,
            "Failed to restore tracked changes",
        ));
    }
    if untracked == ChangeUntracked::Preserve {
        return Ok(());
    }
    let clean = git.run(Some(&repository.worktree), &["clean", "-fd", "--", &scope]);
    if clean.exit_code == 0 {
        return Ok(());
    }
    Err(result_message(
        &clean.stderr,
        &clean.text,
        "Failed to clean untracked changes",
    ))
}

/// `MoveSession.Service` (`move-session.ts:63-141`).
pub struct MoveSession {
    pub sessions: SessionStore,
    pub git: Arc<dyn GitRunner>,
    pub events: Arc<EventBus>,
    pub clock: Arc<dyn Clock>,
}

impl MoveSession {
    pub fn new(
        sessions: SessionStore,
        git: Arc<dyn GitRunner>,
        events: Arc<EventBus>,
        clock: Arc<dyn Clock>,
    ) -> MoveSession {
        MoveSession {
            sessions,
            git,
            events,
            clock,
        }
    }

    /// `moveSession` (`move-session.ts:77-138`).
    pub fn move_session(&self, input: &Input) -> Result<(), MoveSessionError> {
        move_session_inner(self, input)
    }
}

fn move_session_inner(service: &MoveSession, input: &Input) -> Result<(), MoveSessionError> {
    let current = match service.sessions.get(&input.session_id) {
        Ok(current) => current,
        Err(_) => {
            return Err(Error::NotFound {
                session_id: input.session_id.clone(),
            }
            .into())
        }
    };
    let directory = PathBuf::from(&input.destination);
    if current.directory == input.destination {
        return Ok(());
    }

    let source = crate::project::resolve(service.git.as_ref(), Path::new(&current.directory));
    let destination = crate::project::resolve(service.git.as_ref(), &directory);
    if current.project_id != destination.id {
        return Err(Error::DestinationProjectMismatch {
            expected: current.project_id.clone(),
            actual: destination.id.clone(),
        }
        .into());
    }

    let move_changes = input.move_changes && source.directory != destination.directory;
    let source_path = PathBuf::from(&current.directory);
    let source_repository = if move_changes {
        service.git.discover(&source_path)
    } else {
        None
    };
    let patch = match &source_repository {
        Some(repository) => capture_changes(service.git.as_ref(), repository, &source_path)
            .map_err(|message| Error::CaptureChanges { message })?,
        None => String::new(),
    };
    if !patch.is_empty() {
        if service.git.discover(&directory).is_none() {
            return Err(Error::ApplyChanges {
                message: "Destination is not a Git repository".to_string(),
            }
            .into());
        }
        apply_changes(&directory, &patch).map_err(|message| Error::ApplyChanges { message })?;
    }

    let subdirectory = relative_scope(&destination.directory, &directory);
    service
        .events
        .publish(
            &SESSION_NEXT_MOVED,
            serde_json::json!({
                "timestamp": i64::try_from(service.clock.now_ms()).unwrap_or(0),
                "sessionID": input.session_id,
                "location": { "directory": input.destination },
                "subdirectory": subdirectory,
            }),
            crate::PublishOptions::default(),
        )
        .map_err(|err| MoveSessionError::Defect(err.to_string()))?;

    if !patch.is_empty() {
        let repository =
            service
                .git
                .discover(&source_path)
                .ok_or_else(|| Error::ResetSourceChanges {
                    directory: source_path.clone(),
                    message: "Source is not a Git repository".to_string(),
                })?;
        discard_changes(
            service.git.as_ref(),
            &repository,
            &source_path,
            ChangeIndex::Preserve,
            ChangeUntracked::Remove,
        )
        .map_err(|message| Error::ResetSourceChanges {
            directory: source_path.clone(),
            message,
        })?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::git::SubprocessGit;
    use crate::session::store::CreateInput;
    use crate::session::{SessionContext, SessionStore};
    use crate::storage::test_support::TempDir;
    use crate::storage::Storage;

    fn git(dir: &Path, args: &[&str]) {
        let ok = Command::new("git")
            .args(args)
            .current_dir(dir)
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .map(|status| status.success())
            .unwrap_or(false);
        assert!(ok, "git {:?} failed in {}", args, dir.display());
    }

    fn init_repo(dir: &Path) {
        git(dir, &["init", "--quiet"]);
        git(dir, &["config", "core.autocrlf", "false"]);
        git(dir, &["config", "core.fsmonitor", "false"]);
        git(dir, &["config", "commit.gpgsign", "false"]);
        git(dir, &["config", "user.email", "test@opencode.test"]);
        git(dir, &["config", "user.name", "Test"]);
        std::fs::write(dir.join("tracked.txt"), "initial\n").unwrap();
        git(dir, &["add", "tracked.txt"]);
        git(dir, &["commit", "--quiet", "-m", "root"]);
    }

    struct NoJobs;
    impl crate::session::run_state::BackgroundJobs for NoJobs {
        fn list(
            &self,
        ) -> Result<Vec<crate::session::run_state::BackgroundJobInfo>, crate::CoreError> {
            Ok(Vec::new())
        }
        fn cancel(&self, _: &str) -> Result<(), crate::CoreError> {
            Ok(())
        }
    }

    fn harness(tag: &str) -> (TempDir, Arc<Storage>, MoveSession) {
        let temp = TempDir::new(tag);
        let storage = Arc::new(Storage::open_in_memory().unwrap());
        let events = Arc::new(EventBus::new_shared(
            storage.clone(),
            Some(Arc::new(
                crate::session::event_definitions::SessionManifest::new(),
            )),
        ));
        crate::register_projectors(&events);
        let sessions = SessionStore::new(
            events.clone(),
            storage.clone(),
            Arc::new(NoJobs),
            Arc::new(crate::catalog::SystemClock),
        );
        let service = MoveSession::new(
            sessions,
            Arc::new(SubprocessGit),
            events,
            Arc::new(crate::catalog::SystemClock),
        );
        (temp, storage, service)
    }

    fn create_session(
        service: &MoveSession,
        storage: &Arc<Storage>,
        worktree: &Path,
        directory: &str,
    ) -> String {
        let project_id = crate::project::resolve(&*service.git, worktree).id;
        storage
            .with_connection(|conn| {
                conn.execute(
                    "INSERT INTO project (id, worktree, sandboxes, time_created, time_updated)
                     VALUES (?1, ?2, ?3, ?4, ?5)
                     ON CONFLICT (id) DO NOTHING",
                    rusqlite::params![project_id, worktree.to_string_lossy(), "[]", 1, 1],
                )
            })
            .unwrap();
        let session = service
            .sessions
            .create(
                &SessionContext {
                    project_id,
                    directory: directory.into(),
                    worktree: worktree.to_path_buf(),
                    workspace_id: None,
                },
                &CreateInput {
                    id: None,
                    directory: Some(directory.to_string()),
                    ..CreateInput::default()
                },
            )
            .unwrap();
        session.id
    }

    #[test]
    fn relative_scope_paths() {
        assert_eq!(relative_scope(Path::new("/a/b"), Path::new("/a/b")), "");
        assert_eq!(relative_scope(Path::new("/a/b"), Path::new("/a/b/c")), "c");
        assert_eq!(relative_scope(Path::new("/a/b/c"), Path::new("/a/b")), "..");
        assert_eq!(
            relative_scope(Path::new("/a/b"), Path::new("/c/d")),
            "../../c/d"
        );
    }

    #[test]
    fn moves_session_changes_to_another_project_directory() {
        let (temp, storage, service) = harness("move-session");
        let root = temp.path().join("repo");
        std::fs::create_dir_all(&root).unwrap();
        init_repo(&root);
        let source = root.canonicalize().unwrap();
        let destination = temp.path().join("move-destination");
        git(
            &root,
            &[
                "worktree",
                "add",
                "--detach",
                destination.to_string_lossy().as_ref(),
                "HEAD",
            ],
        );
        let moved = destination.canonicalize().unwrap();

        std::fs::write(source.join("tracked.txt"), "changed\n").unwrap();
        std::fs::write(source.join("untracked.txt"), "new\n").unwrap();

        let session_id = create_session(&service, &storage, &source, &source.to_string_lossy());
        let input = Input {
            session_id: session_id.clone(),
            destination: moved.to_string_lossy().into_owned(),
            move_changes: true,
        };
        service.move_session(&input).unwrap();

        assert_eq!(
            std::fs::read_to_string(moved.join("tracked.txt")).unwrap(),
            "changed\n"
        );
        assert_eq!(
            std::fs::read_to_string(moved.join("untracked.txt")).unwrap(),
            "new\n"
        );
        assert_eq!(
            std::fs::read_to_string(source.join("tracked.txt")).unwrap(),
            "initial\n"
        );
        assert!(!source.join("untracked.txt").exists());
        let session = service.sessions.get(&session_id).unwrap();
        assert_eq!(session.directory, moved.to_string_lossy());
        assert_eq!(session.path, Some("".to_string()));
    }

    #[test]
    fn moves_within_a_checkout_without_transferring_existing_changes() {
        let (temp, storage, service) = harness("move-session-nested-same");
        let root = temp.path().join("repo");
        std::fs::create_dir_all(&root).unwrap();
        init_repo(&root);
        let source = root.canonicalize().unwrap();
        let destination = source.join("packages");
        std::fs::create_dir_all(&destination).unwrap();

        std::fs::write(source.join("tracked.txt"), "changed\n").unwrap();
        std::fs::write(source.join("untracked.txt"), "new\n").unwrap();

        let session_id = create_session(&service, &storage, &source, &source.to_string_lossy());
        let input = Input {
            session_id: session_id.clone(),
            destination: destination.to_string_lossy().into_owned(),
            move_changes: true,
        };
        service.move_session(&input).unwrap();

        // The source and destination share a checkout — nothing transfers.
        assert_eq!(
            std::fs::read_to_string(source.join("tracked.txt")).unwrap(),
            "changed\n"
        );
        assert_eq!(
            std::fs::read_to_string(source.join("untracked.txt")).unwrap(),
            "new\n"
        );
        let session = service.sessions.get(&session_id).unwrap();
        assert_eq!(
            session.directory,
            destination.to_string_lossy().into_owned()
        );
        assert_eq!(session.path, Some("packages".to_string()));
    }

    #[test]
    fn moves_nested_session_changes_without_cleaning_unrelated_files() {
        let (temp, storage, service) = harness("move-session-nested");
        let root = temp.path().join("repo");
        std::fs::create_dir_all(&root).unwrap();
        init_repo(&root);
        let source = root.canonicalize().unwrap();
        let packages = source.join("packages");
        std::fs::create_dir_all(&packages).unwrap();
        std::fs::write(packages.join("tracked.txt"), "initial\n").unwrap();
        std::fs::write(packages.join("staged.txt"), "initial\n").unwrap();
        git(
            &source,
            &["add", "packages/tracked.txt", "packages/staged.txt"],
        );
        git(&source, &["commit", "--quiet", "-m", "packages"]);
        let checkout = temp.path().join("move-nested-destination");
        git(
            &root,
            &[
                "worktree",
                "add",
                "--detach",
                checkout.to_string_lossy().as_ref(),
                "HEAD",
            ],
        );
        let moved = checkout.canonicalize().unwrap().join("packages");

        std::fs::write(packages.join("tracked.txt"), "changed\n").unwrap();
        std::fs::write(packages.join("staged.txt"), "staged\n").unwrap();
        git(&source, &["add", "packages/staged.txt"]);
        std::fs::write(packages.join("untracked.txt"), "new\n").unwrap();
        std::fs::write(source.join("tracked.txt"), "unrelated\n").unwrap();
        std::fs::write(source.join("untracked.txt"), "unrelated\n").unwrap();

        let session_id = create_session(&service, &storage, &source, &packages.to_string_lossy());
        let input = Input {
            session_id,
            destination: moved.to_string_lossy().into_owned(),
            move_changes: true,
        };
        service.move_session(&input).unwrap();

        assert_eq!(
            std::fs::read_to_string(moved.join("tracked.txt")).unwrap(),
            "changed\n"
        );
        assert_eq!(
            std::fs::read_to_string(moved.join("staged.txt")).unwrap(),
            "staged\n"
        );
        assert_eq!(
            std::fs::read_to_string(moved.join("untracked.txt")).unwrap(),
            "new\n"
        );
        assert_eq!(
            std::fs::read_to_string(packages.join("tracked.txt")).unwrap(),
            "initial\n"
        );
        assert!(!packages.join("untracked.txt").exists());
        // The staged state stays staged in the source index (index=Preserve).
        assert_eq!(
            std::fs::read_to_string(packages.join("staged.txt")).unwrap(),
            "staged\n"
        );
        let mut printed = Command::new("git")
            .args(["status", "--porcelain", "--", "packages/staged.txt"])
            .current_dir(&source)
            .output()
            .unwrap();
        assert_eq!(
            String::from_utf8_lossy(&printed.stdout).to_string(),
            "M  packages/staged.txt\n"
        );
        // Unrelated changes outside the session scope stay untouched.
        assert_eq!(
            std::fs::read_to_string(source.join("tracked.txt")).unwrap(),
            "unrelated\n"
        );
        assert_eq!(
            std::fs::read_to_string(source.join("untracked.txt")).unwrap(),
            "unrelated\n"
        );
        let _ = &mut printed;
    }

    #[test]
    fn missing_session_is_not_found() {
        let (_temp, _storage, service) = harness("move-session-missing");
        let input = Input {
            session_id: "ses_missing".to_string(),
            destination: "/tmp".to_string(),
            move_changes: false,
        };
        let err = service.move_session(&input).unwrap_err();
        assert!(matches!(
            err,
            MoveSessionError::Known(Error::NotFound { .. })
        ));
        assert_eq!(err.to_string(), "Session not found: ses_missing");
    }

    #[test]
    fn destination_in_another_project_is_rejected() {
        let (temp, storage, service) = harness("move-session-mismatch");
        let root = temp.path().join("repo");
        std::fs::create_dir_all(&root).unwrap();
        init_repo(&root);
        let source = root.canonicalize().unwrap();

        let session_id = create_session(&service, &storage, &source, &source.to_string_lossy());
        // A non-git destination resolves to the "global" project.
        let elsewhere = temp.path().join("elsewhere");
        std::fs::create_dir_all(&elsewhere).unwrap();
        let input = Input {
            session_id,
            destination: elsewhere.to_string_lossy().into_owned(),
            move_changes: false,
        };
        let err = service.move_session(&input).unwrap_err();
        let MoveSessionError::Known(err) = err else {
            panic!("expected a known error");
        };
        assert!(matches!(err, Error::DestinationProjectMismatch { .. }));
        assert_eq!(
            err.message(),
            "Destination directory belongs to another project"
        );
    }
}
