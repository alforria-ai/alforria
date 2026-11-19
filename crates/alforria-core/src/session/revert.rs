//! Session revert — port of `session/revert.ts` (`SessionRevert.Service`).
//!
//! `revert` records the revert point on the session plus the snapshot
//! patch rollback; `unrevert` restores it; `cleanup` removes the messages
//! after the point. `cleanup` is what `prompt()` runs before every new
//! user message (prompt.ts:1056) — a pending revert materializes as
//! message removal.

use std::path::PathBuf;
use std::sync::Arc;

use alforria_schema::file_diff::SnapshotFileDiff;
use alforria_schema::session_v1::{V1Message, V1Part, V1SessionInfo, V1SessionRevert};

use crate::event::bus::{EventBus, PublishOptions};

use crate::session::error::SessionError;
use crate::session::event_definitions::SESSION_DIFF;
use crate::session::message::message_id;
use crate::session::run_state::SessionRunState;
use crate::session::snapshot::{CheckedWrite, PatchPart, Snapshot};
use crate::session::store::SessionStore;
use crate::session::summary::SessionSummary;

// ---------------------------------------------------------------------------
// session_diff storage (storage.ts write(["session_diff", id], …))
// ---------------------------------------------------------------------------

/// `storage.write(["session_diff", sessionID], diffs)` — the TS storage
/// layer persists key-value JSON files under `<data>/storage`; the write
/// is best-effort (`Effect.ignore`).
pub fn write_session_diff(
    data_dir: &std::path::Path,
    session_id: &str,
    diffs: &[SnapshotFileDiff],
) -> Result<(), std::io::Error> {
    let target = data_dir
        .join("storage")
        .join("session_diff")
        .join(format!("{session_id}.json"));
    if let Some(parent) = target.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let content = serde_json::to_string_pretty(diffs).unwrap_or_else(|_| "[]".to_string());
    std::fs::write(target, content)
}

// ---------------------------------------------------------------------------
// Service (revert.ts:28-136)
// ---------------------------------------------------------------------------

/// `RevertInput` (revert.ts:13-18).
#[derive(Debug, Clone)]
pub struct RevertInput {
    pub session_id: String,
    pub message_id: String,
    pub part_id: Option<String>,
}

/// A file a guarded session's revert means to write, before the guard has
/// judged it ([`SessionRevert::check_guarded`]).
struct Candidate {
    hash: String,
    file: String,
    delete_missing: bool,
}

/// Everything the revert service closes over.
pub struct RevertDeps {
    pub sessions: SessionStore,
    pub events: Arc<EventBus>,
    pub snapshot: Arc<dyn Snapshot>,
    pub summary: Arc<SessionSummary>,
    pub state: Arc<SessionRunState>,
    /// `Global.Path.data` — the storage key-value root.
    pub data_dir: PathBuf,
    /// alforria extension: what a guarded session may not write
    /// (`AgentRegistry::protected`). A revert restores files from snapshots
    /// directly, not through a tool, so it is judged here.
    pub protected: Arc<crate::protect::Protected>,
    /// The project's git worktree (`/` when it has none): with the
    /// session's directory, what a guarded revert stays inside
    /// ([`crate::session::caller::project_roots`]).
    pub worktree: PathBuf,
}

/// `SessionRevert.Service` (revert.ts:28-136).
pub struct SessionRevert {
    deps: RevertDeps,
}

impl SessionRevert {
    pub fn new(deps: RevertDeps) -> SessionRevert {
        SessionRevert { deps }
    }

    /// alforria extension: a guarded session — restricted, or started by
    /// outside input: the sessions whose tools write through the write
    /// guard — does not revert onto what the guard protects, and its
    /// revert writes nothing the guard was not asked about.
    ///
    /// Upstream's revert lets git write: `checkout <hash> -- <files>` for
    /// the reverted patches and `checkout-index -a -f` over the whole tree
    /// for a snapshot. Checking a listing first and then letting git write
    /// by its own rules would have the check and the write disagree
    /// (ignored files, anything the listing leaves out). So for a guarded
    /// session the listing *is* the write: every candidate is judged
    /// ([`crate::protect::Protected::judge`]), and exactly the judged files
    /// are then written, each at the landing that was judged and through
    /// its directory's handle ([`Snapshot::write_checked`]). What is not on
    /// the list is left untouched.
    ///
    /// If the guard refuses any candidate, or one would land outside the
    /// project (a directory on its way replaced by a link out of it), the
    /// whole revert is refused before anything is written: a revert that skipped files would leave
    /// the tree in a state no message of the session describes. No
    /// `edit_loader` grant is looked up — a grant is an agent's, and a
    /// revert has no agent. An unguarded session never comes here: its
    /// reverts are upstream's, git invocation for git invocation.
    fn check_guarded(
        &self,
        session: &V1SessionInfo,
        candidates: Vec<Candidate>,
    ) -> Result<Vec<CheckedWrite>, SessionError> {
        let mut refused: Vec<String> = Vec::new();
        let mut outside: Vec<String> = Vec::new();
        let mut checked: Vec<CheckedWrite> = Vec::new();
        let directory = std::path::Path::new(&session.directory);
        let roots = crate::session::caller::project_roots(directory, &self.deps.worktree);
        for candidate in candidates {
            let path = directory.join(&candidate.file);
            match self.deps.protected.judge(&path) {
                // The guard names what may not be written; where a revert
                // may write at all is the project. A directory on the way
                // that is now a link out of it lands the write outside.
                Ok(landing)
                    if !roots
                        .iter()
                        .any(|root| crate::protect::within(&landing, root)) =>
                {
                    if !outside.contains(&candidate.file) {
                        outside.push(candidate.file);
                    }
                }
                Ok(landing) => checked.push(CheckedWrite {
                    hash: candidate.hash,
                    file: candidate.file,
                    landing,
                    delete_missing: candidate.delete_missing,
                }),
                Err(_) if refused.contains(&candidate.file) => {}
                Err(_) => refused.push(candidate.file),
            }
        }
        if refused.is_empty() && outside.is_empty() {
            return Ok(checked);
        }
        refused.sort();
        outside.sort();
        let mut reasons = Vec::new();
        if !refused.is_empty() {
            reasons.push(format!(
                "the revert would rewrite files the agent's configuration is loaded from: {}",
                refused.join(", ")
            ));
        }
        if !outside.is_empty() {
            reasons.push(format!(
                "the revert would write outside the project, through a link: {}",
                outside.join(", ")
            ));
        }
        Err(SessionError::Refused(format!(
            "this session is restricted or was started by outside input, and {}. Nothing was \
             reverted.",
            reasons.join("; and ")
        )))
    }

    /// Whether the session is a guarded one. A restriction that cannot be
    /// read counts as one, as in the loop.
    fn guarded(&self, session_id: &str) -> bool {
        let sessions = &self.deps.sessions;
        sessions.started_by_outside_input(session_id)
            || sessions.is_restricted(session_id).unwrap_or(true)
    }

    /// A guarded restore of `snapshot`: the files that differ from it now,
    /// as the snapshot service lists them. A file the snapshot does not
    /// hold is left where it is, as `checkout-index` leaves it.
    async fn restore_candidates(&self, snapshot: &str) -> Result<Vec<Candidate>, SessionError> {
        let patch = self.deps.snapshot.patch(snapshot).await?;
        Ok(patch
            .files
            .into_iter()
            .map(|file| Candidate {
                hash: snapshot.to_string(),
                file,
                delete_missing: false,
            })
            .collect())
    }

    /// `revert` (revert.ts:38-89 — binding).
    pub async fn revert(&self, input: RevertInput) -> Result<V1SessionInfo, SessionError> {
        self.deps.state.assert_not_busy(&input.session_id)?;
        let all = self.deps.sessions.messages(&input.session_id, None)?;
        let session = self.deps.sessions.get(&input.session_id)?;

        let mut last_user: Option<&V1Message> = None;
        let mut rev: Option<V1SessionRevert> = None;
        let mut patches: Vec<PatchPart> = Vec::new();
        for msg in &all {
            if matches!(msg.info, V1Message::User { .. }) {
                last_user = Some(&msg.info);
            }
            let mut remaining: Vec<&V1Part> = Vec::new();
            for part in &msg.parts {
                if rev.is_some() {
                    if let V1Part::Patch { hash, files, .. } = part {
                        patches.push(PatchPart {
                            hash: hash.clone(),
                            files: files.clone(),
                        });
                    }
                    continue;
                }
                if rev.is_none() {
                    if (message_id(&msg.info) == input.message_id && input.part_id.is_none())
                        || Some(part_id(part).to_string()) == input.part_id
                    {
                        let part_id = if remaining
                            .iter()
                            .any(|item| matches!(item, V1Part::Text { .. } | V1Part::Tool { .. }))
                        {
                            input.part_id.clone()
                        } else {
                            None
                        };
                        let message_id = match (&part_id, last_user) {
                            (None, Some(last_user)) => message_id(last_user).to_string(),
                            _ => message_id(&msg.info).to_string(),
                        };
                        rev = Some(V1SessionRevert {
                            message_id,
                            part_id,
                            snapshot: None,
                            diff: None,
                        });
                    }
                    remaining.push(part);
                }
            }
        }

        let Some(mut rev) = rev else {
            return Ok(session);
        };

        // A guarded session: judge every file before the first write (and
        // before a snapshot is taken for it), then write exactly those.
        // Nothing of this is computed for a session without a guard.
        let checked = if self.guarded(&session.id) {
            let mut candidates = Vec::new();
            if let Some(previous) = session
                .revert
                .as_ref()
                .and_then(|revert| revert.snapshot.as_deref())
            {
                candidates.extend(self.restore_candidates(previous).await?);
            }
            // One write per file, from the first patch that names it, as
            // `Snapshot::revert` reads the same list.
            let mut seen = std::collections::HashSet::new();
            for patch in &patches {
                for file in &patch.files {
                    if seen.insert(file.clone()) {
                        candidates.push(Candidate {
                            hash: patch.hash.clone(),
                            file: file.clone(),
                            delete_missing: true,
                        });
                    }
                }
            }
            Some(self.check_guarded(&session, candidates)?)
        } else {
            None
        };

        rev.snapshot = match session
            .revert
            .as_ref()
            .and_then(|previous| previous.snapshot.as_ref())
        {
            Some(previous) => Some(previous.clone()),
            None => self.deps.snapshot.checkpoint().await?,
        };
        match checked {
            Some(checked) => self.deps.snapshot.write_checked(checked).await?,
            None => {
                if let Some(previous) = session
                    .revert
                    .as_ref()
                    .and_then(|revert| revert.snapshot.clone())
                {
                    self.deps.snapshot.restore(&previous).await?;
                }
                self.deps.snapshot.revert(patches).await?;
            }
        }
        if let Some(snapshot) = &rev.snapshot {
            rev.diff = Some(self.deps.snapshot.diff(snapshot).await?);
        }

        let index = all
            .iter()
            .position(|msg| message_id(&msg.info) == rev.message_id);
        let range: &[crate::session::message::WithParts] = match index {
            Some(index) => &all[index..],
            None => &[],
        };
        let diffs = self.deps.summary.compute_diff(range).await?;
        // storage.write(["session_diff", sessionID], diffs) — Effect.ignore
        let _ = write_session_diff(&self.deps.data_dir, &input.session_id, &diffs);
        self.deps.events.publish(
            &SESSION_DIFF,
            serde_json::json!({ "sessionID": input.session_id, "diff": diffs }),
            PublishOptions::default(),
        )?;
        self.deps.sessions.set_revert(
            &input.session_id,
            Some(V1SessionRevert {
                message_id: rev.message_id.clone(),
                part_id: rev.part_id.clone(),
                snapshot: rev.snapshot.clone(),
                diff: rev.diff.clone(),
            }),
            Some(alforria_schema::session_v1::V1SessionSummary {
                additions: diffs.iter().map(|diff| diff.additions).sum(),
                deletions: diffs.iter().map(|diff| diff.deletions).sum(),
                files: diffs.len() as f64,
                diffs: None,
            }),
        )?;
        self.deps.sessions.get(&input.session_id)
    }

    /// `unrevert` (revert.ts:91-99).
    pub async fn unrevert(&self, session_id: &str) -> Result<V1SessionInfo, SessionError> {
        tracing::info!("unreverting");
        self.deps.state.assert_not_busy(session_id)?;
        let session = self.deps.sessions.get(session_id)?;
        let Some(revert) = session.revert.clone() else {
            return Ok(session);
        };
        if let Some(snapshot) = revert.snapshot {
            if self.guarded(session_id) {
                let candidates = self.restore_candidates(&snapshot).await?;
                let checked = self.check_guarded(&session, candidates)?;
                self.deps.snapshot.write_checked(checked).await?;
            } else {
                self.deps.snapshot.restore(&snapshot).await?;
            }
        }
        self.deps.sessions.clear_revert(session_id)?;
        self.deps.sessions.get(session_id)
    }

    /// `cleanup` (revert.ts:101-124): remove the messages after the revert
    /// point (respecting part-level revert), then clear the revert state.
    pub fn cleanup(&self, session: &V1SessionInfo) -> Result<(), SessionError> {
        let Some(revert) = &session.revert else {
            return Ok(());
        };
        let session_id = session.id.clone();
        let msgs = self.deps.sessions.messages(&session_id, None)?;
        let index = msgs
            .iter()
            .position(|msg| message_id(&msg.info) == revert.message_id);
        let target = index.map(|index| &msgs[index]);
        let remove = match index {
            Some(index) => &msgs[index + if revert.part_id.is_some() { 1 } else { 0 }..],
            None => &[],
        };
        for msg in remove {
            self.deps
                .sessions
                .remove_message(&session_id, message_id(&msg.info))?;
        }
        if let (Some(revert_part_id), Some(target)) = (&revert.part_id, target) {
            let idx = target
                .parts
                .iter()
                .position(|part| part_id(part) == *revert_part_id);
            if let Some(idx) = idx {
                for part in &target.parts[idx..] {
                    self.deps.sessions.remove_part(
                        &session_id,
                        message_id(&target.info),
                        part_id(part),
                    )?;
                }
            }
        }
        self.deps.sessions.clear_revert(&session_id)
    }
}

/// `part.id` for any part.
fn part_id(part: &V1Part) -> &str {
    match part {
        V1Part::Text { id, .. }
        | V1Part::Subtask { id, .. }
        | V1Part::Reasoning { id, .. }
        | V1Part::File { id, .. }
        | V1Part::Tool { id, .. }
        | V1Part::StepStart { id, .. }
        | V1Part::StepFinish { id, .. }
        | V1Part::Snapshot { id, .. }
        | V1Part::Patch { id, .. }
        | V1Part::Agent { id, .. }
        | V1Part::Retry { id, .. }
        | V1Part::Compaction { id, .. } => id,
    }
}
// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::session::message::message_id;
    use crate::session::summary::{SessionSummary, SummaryDeps};
    use crate::session::test_support::{
        assistant_message, create_session, harness, text_part, user_message,
    };
    use alforria_schema::session_v1::V1Part;

    fn revert_service(h: &crate::session::test_support::Harness) -> SessionRevert {
        let summary = SessionSummary::new(SummaryDeps {
            sessions: h.services.sessions.clone(),
            snapshot: h.snapshot.clone(),
            events: h.services.events.clone(),
            config: Arc::new(h.config.clone()),
        });
        SessionRevert::new(RevertDeps {
            sessions: h.services.sessions.clone(),
            events: h.services.events.clone(),
            snapshot: h.snapshot.clone(),
            summary: Arc::new(summary),
            state: h.services.run_state.clone(),
            data_dir: h.temp.path().to_path_buf(),
            protected: h.services.agents.protected(),
            worktree: h.worktree.clone(),
        })
    }

    fn patch_part(
        session: &str,
        message: &str,
        id: &str,
        hash: &str,
        files: Vec<String>,
    ) -> V1Part {
        V1Part::Patch {
            id: id.to_string(),
            session_id: session.to_string(),
            message_id: message.to_string(),
            hash: hash.to_string(),
            files,
        }
    }

    /// A session with one user turn: `a.txt` was `hello` at the step-start
    /// snapshot, then a tool changed it to `goodbye` (recorded as a patch
    /// part).
    async fn scenario(
        name: &str,
    ) -> (
        crate::session::test_support::Harness,
        alforria_schema::session_v1::V1SessionInfo,
    ) {
        let h = harness(name, vec![]);
        let session = create_session(&h.services.sessions, &h.worktree);
        std::fs::write(h.worktree.join("a.txt"), "hello\n").unwrap();
        let snap = h.snapshot.track().await.unwrap();
        std::fs::write(h.worktree.join("a.txt"), "goodbye\n").unwrap();

        h.services
            .sessions
            .update_message(&user_message(&session.id, "msg_u1", 1.0))
            .unwrap();
        h.services
            .sessions
            .update_part(&text_part(&session.id, "msg_u1", "prt_u1", "fix the bug"))
            .unwrap();
        h.services
            .sessions
            .update_message(&assistant_message(
                &session.id,
                "msg_a1",
                "msg_u1",
                2,
                None,
                None,
            ))
            .unwrap();
        // An assistant text part precedes the patch part: part-level
        // reverts keep `partID` only when a text/tool part was already
        // emitted on the same message (revert.ts:59-63).
        h.services
            .sessions
            .update_part(&text_part(&session.id, "msg_a1", "prt_1_text", "done"))
            .unwrap();
        h.services
            .sessions
            .update_part(&patch_part(
                &session.id,
                "msg_a1",
                "prt_2_patch",
                &snap,
                vec![h.worktree.join("a.txt").to_string_lossy().into_owned()],
            ))
            .unwrap();
        (h, session)
    }

    #[tokio::test]
    async fn revert_rolls_back_files_and_records_state() {
        let (h, session) = scenario("revert-basic").await;
        let info = revert_service(&h)
            .revert(RevertInput {
                session_id: session.id.clone(),
                message_id: "msg_u1".to_string(),
                part_id: None,
            })
            .await
            .unwrap();

        // The patch was rolled back to its hash's contents.
        assert_eq!(
            std::fs::read_to_string(h.worktree.join("a.txt")).unwrap(),
            "hello\n"
        );

        // The revert point is recorded on the session.
        let revert = info.revert.as_ref().expect("revert state");
        assert_eq!(revert.message_id, "msg_u1");
        assert!(revert.part_id.is_none());
        assert!(revert.snapshot.is_some(), "snapshot tracked");
        assert!(
            revert
                .diff
                .as_deref()
                .map(|d| !d.is_empty())
                .unwrap_or(false),
            "diff of the reverted worktree recorded"
        );

        // `storage.write(["session_diff", sessionID], …)` — the JSON file
        // exists under <data>/storage/session_diff/<id>.json.
        let stored = h
            .temp
            .path()
            .join("storage")
            .join("session_diff")
            .join(format!("{}.json", session.id));
        assert!(stored.is_file(), "session diff file written");
        assert_eq!(
            serde_json::from_str::<Vec<SnapshotFileDiff>>(
                &std::fs::read_to_string(&stored).unwrap()
            )
            .unwrap(),
            Vec::new()
        );

        // Summary numbers reflect the (empty) revert diffs.
        assert_eq!(
            info.summary.as_ref().expect("session summary"),
            &alforria_schema::session_v1::V1SessionSummary {
                additions: 0.0,
                deletions: 0.0,
                files: 0.0,
                diffs: None,
            }
        );
    }

    #[tokio::test]
    async fn revert_part_level_points_at_the_part() {
        let (h, session) = scenario("revert-part").await;
        let info = revert_service(&h)
            .revert(RevertInput {
                session_id: session.id.clone(),
                message_id: "msg_a1".to_string(),
                part_id: Some("prt_2_patch".to_string()),
            })
            .await
            .unwrap();
        let revert = info.revert.as_ref().expect("revert state");
        assert_eq!(revert.message_id, "msg_a1");
        assert_eq!(revert.part_id.as_deref(), Some("prt_2_patch"));
    }

    #[tokio::test]
    async fn revert_without_match_returns_session_unchanged() {
        let (h, session) = scenario("revert-noop").await;
        let before = std::fs::read_to_string(h.worktree.join("a.txt")).unwrap();
        let info = revert_service(&h)
            .revert(RevertInput {
                session_id: session.id.clone(),
                message_id: "msg_missing".to_string(),
                part_id: None,
            })
            .await
            .unwrap();
        assert!(info.revert.is_none());
        assert_eq!(
            std::fs::read_to_string(h.worktree.join("a.txt")).unwrap(),
            before
        );
    }

    #[tokio::test]
    async fn revert_aborts_when_an_enabled_snapshot_cannot_capture_an_undo_point() {
        use crate::session::snapshot::{GitSnapshot, GitSnapshotInput};
        let (h, session) = scenario("revert-failed-capture").await;
        let unavailable = h.temp.path().join("unavailable");
        std::fs::write(&unavailable, "").unwrap();
        let mut service = revert_service(&h);
        service.deps.snapshot = GitSnapshot::with_cleanup(
            GitSnapshotInput {
                directory: h.worktree.clone(),
                worktree: h.worktree.clone(),
                project_id: "test".into(),
                vcs_is_git: true,
                snapshot_enabled: true,
                data: unavailable,
            },
            std::time::Duration::from_secs(3600),
            std::time::Duration::from_secs(3600),
        );
        let error = service
            .revert(RevertInput {
                session_id: session.id.clone(),
                message_id: "msg_u1".into(),
                part_id: None,
            })
            .await
            .unwrap_err();
        assert!(error
            .to_string()
            .contains("could not capture an undo snapshot"));
        assert_eq!(
            std::fs::read_to_string(h.worktree.join("a.txt")).unwrap(),
            "goodbye\n"
        );
        assert!(h
            .services
            .sessions
            .get(&session.id)
            .unwrap()
            .revert
            .is_none());
    }

    #[tokio::test]
    async fn failed_git_restoration_preserves_session_recovery_state() {
        use crate::session::snapshot::{GitSnapshot, GitSnapshotInput};
        let (h, session) = scenario("revert-missing-recovery").await;
        let snapshot = GitSnapshot::with_cleanup(
            GitSnapshotInput {
                directory: h.worktree.clone(),
                worktree: h.worktree.clone(),
                project_id: "test".into(),
                vcs_is_git: true,
                snapshot_enabled: true,
                data: h.temp.path().to_path_buf(),
            },
            std::time::Duration::from_secs(3600),
            std::time::Duration::from_secs(3600),
        );
        snapshot.track().await.unwrap();
        let recovery = V1SessionRevert {
            message_id: "msg_u1".into(),
            part_id: None,
            snapshot: Some("0000000000000000000000000000000000000001".into()),
            diff: Some("saved recovery diff".into()),
        };
        h.services
            .sessions
            .set_revert(&session.id, Some(recovery.clone()), None)
            .unwrap();
        let mut service = revert_service(&h);
        service.deps.snapshot = snapshot;
        assert!(service.unrevert(&session.id).await.is_err());
        assert_eq!(
            h.services.sessions.get(&session.id).unwrap().revert,
            Some(recovery.clone())
        );
        assert!(service
            .revert(RevertInput {
                session_id: session.id.clone(),
                message_id: "msg_u1".into(),
                part_id: None
            })
            .await
            .is_err());
        assert_eq!(
            h.services.sessions.get(&session.id).unwrap().revert,
            Some(recovery)
        );
        assert_eq!(
            std::fs::read_to_string(h.worktree.join("a.txt")).unwrap(),
            "goodbye\n"
        );
    }

    #[tokio::test]
    async fn unrevert_restores_the_tracked_snapshot() {
        let (h, session) = scenario("revert-unrevert").await;
        let service = revert_service(&h);
        service
            .revert(RevertInput {
                session_id: session.id.clone(),
                message_id: "msg_u1".to_string(),
                part_id: None,
            })
            .await
            .unwrap();
        assert_eq!(
            std::fs::read_to_string(h.worktree.join("a.txt")).unwrap(),
            "hello\n"
        );

        let info = service.unrevert(&session.id).await.unwrap();
        assert!(info.revert.is_none(), "revert state cleared");
        assert_eq!(
            std::fs::read_to_string(h.worktree.join("a.txt")).unwrap(),
            "goodbye\n"
        );
    }

    /// `scenario`, with an `AGENTS.md` the turn also "changed": it did not
    /// exist at the snapshot, so a revert of the turn would delete it.
    async fn scenario_touching_a_loader_file(
        name: &str,
    ) -> (
        crate::session::test_support::Harness,
        alforria_schema::session_v1::V1SessionInfo,
    ) {
        let (h, session) = scenario(name).await;
        std::fs::write(h.worktree.join("AGENTS.md"), "the operator's\n").unwrap();
        let V1Part::Patch { hash, .. } = h
            .services
            .sessions
            .messages(&session.id, None)
            .unwrap()
            .iter()
            .flat_map(|msg| msg.parts.clone())
            .find(|part| matches!(part, V1Part::Patch { .. }))
            .unwrap()
        else {
            unreachable!()
        };
        h.services
            .sessions
            .update_part(&patch_part(
                &session.id,
                "msg_a1",
                "prt_3_patch",
                &hash,
                vec![h.worktree.join("AGENTS.md").to_string_lossy().into_owned()],
            ))
            .unwrap();
        (h, session)
    }

    fn revert_all(session_id: &str) -> RevertInput {
        RevertInput {
            session_id: session_id.to_string(),
            message_id: "msg_u1".to_string(),
            part_id: None,
        }
    }

    /// Revert restores files from snapshots directly, not through a tool,
    /// so it used to go around the write guard. In a guarded session
    /// (restricted, or started by outside input) a revert or unrevert that
    /// would write a loader file is refused whole, before anything is
    /// restored; one that touches other files only goes through.
    #[tokio::test]
    async fn a_guarded_session_does_not_revert_onto_a_loader_file() {
        let read = |h: &crate::session::test_support::Harness, name: &str| {
            std::fs::read_to_string(h.worktree.join(name)).ok()
        };

        // Revert: the turn's patches name AGENTS.md.
        let (h, session) = scenario_touching_a_loader_file("revert-guarded").await;
        h.services.sessions.restrict(&session.id).unwrap();
        let service = revert_service(&h);
        let refused = service.revert(revert_all(&session.id)).await.unwrap_err();
        let SessionError::Refused(message) = &refused else {
            panic!("{refused:?}");
        };
        assert!(message.contains("AGENTS.md"), "{message}");
        assert!(!message.contains("a.txt"), "{message}");
        // Nothing was restored and the session is not marked reverted.
        assert_eq!(read(&h, "AGENTS.md").as_deref(), Some("the operator's\n"));
        assert_eq!(read(&h, "a.txt").as_deref(), Some("goodbye\n"));
        assert!(h
            .services
            .sessions
            .get(&session.id)
            .unwrap()
            .revert
            .is_none());

        // A guarded session whose revert touches other files only: it
        // reverts, and unreverts.
        let (h, session) = scenario("revert-guarded-plain").await;
        std::fs::write(h.worktree.join("AGENTS.md"), "the operator's\n").unwrap();
        h.services.sessions.restrict(&session.id).unwrap();
        let service = revert_service(&h);
        service.revert(revert_all(&session.id)).await.unwrap();
        assert_eq!(read(&h, "a.txt").as_deref(), Some("hello\n"));
        service.unrevert(&session.id).await.unwrap();
        assert_eq!(read(&h, "a.txt").as_deref(), Some("goodbye\n"));

        // Unrevert: the snapshot taken at the revert holds AGENTS.md, which
        // has changed since, so restoring it would rewrite the file.
        service.revert(revert_all(&session.id)).await.unwrap();
        std::fs::write(h.worktree.join("AGENTS.md"), "edited since\n").unwrap();
        let refused = service.unrevert(&session.id).await.unwrap_err();
        assert!(
            matches!(&refused, SessionError::Refused(message) if message.contains("AGENTS.md")),
            "{refused:?}"
        );
        assert_eq!(read(&h, "AGENTS.md").as_deref(), Some("edited since\n"));
        assert_eq!(read(&h, "a.txt").as_deref(), Some("hello\n"));
        assert!(h
            .services
            .sessions
            .get(&session.id)
            .unwrap()
            .revert
            .is_some());

        // A session started by outside input is guarded the same way.
        let (h, session) = scenario_touching_a_loader_file("revert-outside").await;
        h.services
            .sessions
            .mark(
                &session.id,
                crate::session::store::SessionMark::OutsideStart,
            )
            .unwrap();
        let refused = revert_service(&h).revert(revert_all(&session.id)).await;
        assert!(matches!(refused, Err(SessionError::Refused(_))));
        assert_eq!(read(&h, "AGENTS.md").as_deref(), Some("the operator's\n"));
    }

    /// A session without a guard reverts exactly as before, loader files
    /// included.
    #[tokio::test]
    async fn an_unguarded_session_reverts_loader_files_as_before() {
        let (h, session) = scenario_touching_a_loader_file("revert-unguarded").await;
        let service = revert_service(&h);
        let info = service.revert(revert_all(&session.id)).await.unwrap();
        assert!(info.revert.is_some());
        assert_eq!(
            std::fs::read_to_string(h.worktree.join("a.txt")).unwrap(),
            "hello\n"
        );
        assert!(!h.worktree.join("AGENTS.md").exists(), "rolled back");
        std::fs::write(h.worktree.join("AGENTS.md"), "edited since\n").unwrap();
        service.unrevert(&session.id).await.unwrap();
        assert_eq!(
            std::fs::read_to_string(h.worktree.join("AGENTS.md")).unwrap(),
            "the operator's\n"
        );
        assert_eq!(
            std::fs::read_to_string(h.worktree.join("a.txt")).unwrap(),
            "goodbye\n"
        );
    }

    /// A restricted session over a real git snapshot store: the files of
    /// `before` committed, a snapshot taken, `change` applied as the
    /// turn's edits and recorded as one patch part naming `patched`.
    struct GitScenario {
        h: crate::session::test_support::Harness,
        session: alforria_schema::session_v1::V1SessionInfo,
        service: SessionRevert,
        _data: crate::storage::test_support::TempDir,
    }

    impl GitScenario {
        fn write(&self, name: &str, content: &str) {
            let path = self.h.worktree.join(name);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(path, content).unwrap();
        }

        fn read(&self, name: &str) -> Option<String> {
            std::fs::read_to_string(self.h.worktree.join(name)).ok()
        }

        fn reverted(&self) -> bool {
            let sessions = &self.h.services.sessions;
            sessions.get(&self.session.id).unwrap().revert.is_some()
        }
    }

    async fn git_scenario(
        name: &str,
        guarded: bool,
        before: &[(&str, &str)],
        change: impl FnOnce(&std::path::Path),
        patched: &[&str],
    ) -> GitScenario {
        git_scenario_with(name, guarded, before, |_| {}, change, patched).await
    }

    /// [`git_scenario`], with `prepare` run on the files before the
    /// snapshot is taken.
    async fn git_scenario_with(
        name: &str,
        guarded: bool,
        before: &[(&str, &str)],
        prepare: impl FnOnce(&std::path::Path),
        change: impl FnOnce(&std::path::Path),
        patched: &[&str],
    ) -> GitScenario {
        use crate::git::{GitRunner, SubprocessGit};
        let h = harness(name, vec![]);
        let data = crate::storage::test_support::TempDir::new("revert-snap-data");
        let git = |args: &[&str]| SubprocessGit.run(Some(&h.worktree), args);
        assert_eq!(git(&["init", "--quiet"]).exit_code, 0, "git init");
        git(&["config", "user.email", "t@e.st"]);
        git(&["config", "user.name", "T"]);
        for (file, content) in before {
            let path = h.worktree.join(file);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(path, content).unwrap();
        }
        prepare(&h.worktree);
        git(&["add", "-A"]);
        git(&["commit", "--quiet", "--allow-empty", "-m", "x"]);
        let snapshot = crate::session::snapshot::GitSnapshot::with_cleanup(
            crate::session::snapshot::GitSnapshotInput {
                directory: h.worktree.clone(),
                worktree: h.worktree.clone(),
                project_id: "test".to_string(),
                vcs_is_git: true,
                snapshot_enabled: true,
                data: data.path().to_path_buf(),
            },
            std::time::Duration::from_secs(3600),
            std::time::Duration::from_secs(3600),
        );
        let session = create_session(&h.services.sessions, &h.worktree);
        let hash = snapshot.track().await.unwrap();
        change(&h.worktree);

        let sessions = &h.services.sessions;
        sessions
            .update_message(&user_message(&session.id, "msg_u1", 1.0))
            .unwrap();
        sessions
            .update_part(&text_part(&session.id, "msg_u1", "prt_u1", "do it"))
            .unwrap();
        sessions
            .update_message(&assistant_message(
                &session.id,
                "msg_a1",
                "msg_u1",
                2,
                None,
                None,
            ))
            .unwrap();
        sessions
            .update_part(&text_part(&session.id, "msg_a1", "prt_1_text", "done"))
            .unwrap();
        let files = patched
            .iter()
            .map(|file| h.worktree.join(file).to_string_lossy().into_owned())
            .collect();
        sessions
            .update_part(&patch_part(
                &session.id,
                "msg_a1",
                "prt_2_patch",
                &hash,
                files,
            ))
            .unwrap();
        if guarded {
            sessions.restrict(&session.id).unwrap();
        }
        let snapshot: Arc<dyn Snapshot> = snapshot;
        let summary = SessionSummary::new(SummaryDeps {
            sessions: h.services.sessions.clone(),
            snapshot: snapshot.clone(),
            events: h.services.events.clone(),
            config: Arc::new(h.config.clone()),
        });
        let service = SessionRevert::new(RevertDeps {
            sessions: h.services.sessions.clone(),
            events: h.services.events.clone(),
            snapshot,
            summary: Arc::new(summary),
            state: h.services.run_state.clone(),
            data_dir: h.temp.path().to_path_buf(),
            protected: h.services.agents.protected(),
            worktree: h.worktree.clone(),
        });
        GitScenario {
            h,
            session,
            service,
            _data: data,
        }
    }

    const BEFORE: &[(&str, &str)] = &[
        ("a.txt", "hello\n"),
        ("keep.txt", "k1\n"),
        ("AGENTS.md", "root v1\n"),
        ("local/AGENTS.md", "v1\n"),
        ("local/notes.txt", "n1\n"),
    ];

    /// A turn that changed `a.txt`; the revert of it has been made, and
    /// since then `local/` became git-ignored and files in it, and
    /// `keep.txt`, were edited. Unreverting restores the snapshot taken at
    /// the revert, which holds all of them.
    async fn reverted_then_edited(name: &str, guarded: bool) -> GitScenario {
        let s = git_scenario(
            name,
            guarded,
            BEFORE,
            |worktree| std::fs::write(worktree.join("a.txt"), "goodbye\n").unwrap(),
            &["a.txt"],
        )
        .await;
        s.service.revert(revert_all(&s.session.id)).await.unwrap();
        assert_eq!(s.read("a.txt").as_deref(), Some("hello\n"));
        s.write(".gitignore", "local/\n");
        s.write("local/AGENTS.md", "edited since\n");
        s.write("local/notes.txt", "n2\n");
        s.write("keep.txt", "k2\n");
        s
    }

    /// The check and the write are one list. A guarded session's restore
    /// used to check the snapshot service's listing — which leaves ignored
    /// files out — and then let `checkout-index -a -f` write the whole
    /// tree: a git-ignored loader file was rewritten unchecked. Now exactly
    /// the checked files are written, and what is not listed is untouched.
    #[tokio::test]
    async fn a_guarded_restore_writes_exactly_the_files_that_were_checked() {
        let s = reverted_then_edited("revert-git-exact", true).await;
        s.service.unrevert(&s.session.id).await.unwrap();
        assert!(!s.reverted());
        // Listed, ordinary: restored.
        assert_eq!(s.read("a.txt").as_deref(), Some("goodbye\n"));
        assert_eq!(s.read("keep.txt").as_deref(), Some("k1\n"));
        // Not listed (ignored): untouched — the loader file above all.
        assert_eq!(s.read("local/AGENTS.md").as_deref(), Some("edited since\n"));
        assert_eq!(s.read("local/notes.txt").as_deref(), Some("n2\n"));
        // Not in the snapshot: left, as a restore leaves it.
        assert_eq!(s.read(".gitignore").as_deref(), Some("local/\n"));
        assert_eq!(s.read("AGENTS.md").as_deref(), Some("root v1\n"));
    }

    /// A tracked loader file among the candidates refuses the whole
    /// unrevert, and nothing at all is written: not the ordinary files, not
    /// the ignored loader file.
    #[tokio::test]
    async fn a_refused_guarded_restore_writes_nothing() {
        let s = reverted_then_edited("revert-git-refused", true).await;
        s.write("AGENTS.md", "root edited\n");
        let refused = s.service.unrevert(&s.session.id).await.unwrap_err();
        let SessionError::Refused(message) = &refused else {
            panic!("{refused:?}");
        };
        assert!(message.contains("AGENTS.md"), "{message}");
        assert!(!message.contains("keep.txt"), "{message}");
        assert!(s.reverted(), "still reverted");
        for (file, content) in [
            ("a.txt", "hello\n"),
            ("keep.txt", "k2\n"),
            ("AGENTS.md", "root edited\n"),
            ("local/AGENTS.md", "edited since\n"),
            ("local/notes.txt", "n2\n"),
            (".gitignore", "local/\n"),
        ] {
            assert_eq!(s.read(file).as_deref(), Some(content), "{file}");
        }
    }

    /// The same state in a session without a guard: upstream's whole-tree
    /// restore, ignored files and loader files included.
    #[tokio::test]
    async fn an_unguarded_restore_is_the_whole_tree_as_before() {
        let s = reverted_then_edited("revert-git-unguarded", false).await;
        s.write("AGENTS.md", "root edited\n");
        s.service.unrevert(&s.session.id).await.unwrap();
        for (file, content) in [
            ("a.txt", "goodbye\n"),
            ("keep.txt", "k1\n"),
            ("AGENTS.md", "root v1\n"),
            ("local/AGENTS.md", "v1\n"),
            ("local/notes.txt", "n1\n"),
            (".gitignore", "local/\n"),
        ] {
            assert_eq!(s.read(file).as_deref(), Some(content), "{file}");
        }
    }

    /// A guarded revert of a turn that renamed, added, deleted and edited
    /// files: both ends of the rename, the addition and the deletion are
    /// each a listed file, restored or removed one by one; a file the turn
    /// changed that no patch lists is untouched. A rename *onto* a loader
    /// name refuses the lot.
    #[tokio::test]
    async fn a_guarded_revert_handles_renames_additions_and_deletions() {
        let before: &[(&str, &str)] = &[
            ("old.txt", "old\n"),
            ("gone.txt", "gone\n"),
            ("edit.txt", "e1\n"),
            ("unlisted.txt", "u1\n"),
            ("plain.md", "plain\n"),
        ];
        let change = |worktree: &std::path::Path| {
            std::fs::rename(worktree.join("old.txt"), worktree.join("new.txt")).unwrap();
            std::fs::remove_file(worktree.join("gone.txt")).unwrap();
            std::fs::write(worktree.join("added.txt"), "added\n").unwrap();
            std::fs::write(worktree.join("edit.txt"), "e2\n").unwrap();
            std::fs::write(worktree.join("unlisted.txt"), "u2\n").unwrap();
        };
        let patched = ["old.txt", "new.txt", "gone.txt", "added.txt", "edit.txt"];
        let s = git_scenario("revert-git-rename", true, before, change, &patched).await;
        s.service.revert(revert_all(&s.session.id)).await.unwrap();
        assert!(s.reverted());
        assert_eq!(s.read("old.txt").as_deref(), Some("old\n"));
        assert_eq!(s.read("new.txt"), None);
        assert_eq!(s.read("gone.txt").as_deref(), Some("gone\n"));
        assert_eq!(s.read("added.txt"), None);
        assert_eq!(s.read("edit.txt").as_deref(), Some("e1\n"));
        assert_eq!(s.read("unlisted.txt").as_deref(), Some("u2\n"));

        // `plain.md` renamed to `CLAUDE.md`: the destination is a loader
        // file the revert would delete.
        let change = |worktree: &std::path::Path| {
            std::fs::rename(worktree.join("plain.md"), worktree.join("CLAUDE.md")).unwrap();
            std::fs::write(worktree.join("edit.txt"), "e2\n").unwrap();
        };
        let patched = ["plain.md", "CLAUDE.md", "edit.txt"];
        let s = git_scenario("revert-git-rename-loader", true, before, change, &patched).await;
        let refused = s.service.revert(revert_all(&s.session.id)).await;
        assert!(
            matches!(&refused, Err(SessionError::Refused(message)) if message.contains("CLAUDE.md")),
            "{refused:?}"
        );
        assert!(!s.reverted());
        assert_eq!(s.read("plain.md"), None);
        assert_eq!(s.read("CLAUDE.md").as_deref(), Some("plain\n"));
        assert_eq!(s.read("edit.txt").as_deref(), Some("e2\n"));
    }

    /// `d` replaced by a link to a directory outside the project, holding
    /// one file `x`.
    #[cfg(unix)]
    fn swap_for_a_link_outside(
        s: &GitScenario,
        name: &str,
    ) -> (crate::storage::test_support::TempDir, std::path::PathBuf) {
        let outside = crate::storage::test_support::TempDir::new(name);
        let file = outside.path().join("x");
        std::fs::write(&file, "the operator's\n").unwrap();
        std::fs::remove_dir_all(s.h.worktree.join("d")).unwrap();
        std::os::unix::fs::symlink(outside.path(), s.h.worktree.join("d")).unwrap();
        (outside, file)
    }

    /// The guard judged where a write lands, but only against the loader
    /// and host locations: a patch naming `d/x`, with `d` since replaced by
    /// a link out of the project, landed outside it and the revert deleted
    /// the file there. A guarded revert is refused when a file it would
    /// write lands outside the project.
    #[cfg(unix)]
    #[tokio::test]
    async fn a_guarded_revert_does_not_delete_through_a_link_out_of_the_project() {
        let change = |worktree: &std::path::Path| {
            std::fs::create_dir_all(worktree.join("d")).unwrap();
            std::fs::write(worktree.join("d/x"), "the agent's\n").unwrap();
            std::fs::write(worktree.join("a.txt"), "goodbye\n").unwrap();
        };
        let before = &[("a.txt", "hello\n")];
        let s = git_scenario(
            "revert-git-out-delete",
            true,
            before,
            change,
            &["d/x", "a.txt"],
        )
        .await;
        let (_outside, file) = swap_for_a_link_outside(&s, "revert-out-delete");
        let refused = s.service.revert(revert_all(&s.session.id)).await;
        assert_eq!(
            std::fs::read_to_string(&file).ok().as_deref(),
            Some("the operator's\n"),
            "the file outside the project is untouched"
        );
        assert!(
            matches!(&refused, Err(SessionError::Refused(message)) if message.contains("d/x")),
            "{refused:?}"
        );
        assert!(!s.reverted());
        assert_eq!(
            s.read("a.txt").as_deref(),
            Some("goodbye\n"),
            "nothing written"
        );
    }

    /// The same link with a snapshot that holds `d/x`: the revert, and the
    /// unrevert after a revert, would have written the snapshot's bytes
    /// over the file outside.
    #[cfg(unix)]
    #[tokio::test]
    async fn a_guarded_revert_or_unrevert_does_not_overwrite_through_a_link_out_of_the_project() {
        let before = &[("d/x", "v1\n"), ("a.txt", "hello\n")];
        let change = |worktree: &std::path::Path| {
            std::fs::write(worktree.join("d/x"), "v2\n").unwrap();
            std::fs::write(worktree.join("a.txt"), "goodbye\n").unwrap();
        };
        let s = git_scenario(
            "revert-git-out-write",
            true,
            before,
            change,
            &["d/x", "a.txt"],
        )
        .await;
        let (_outside, file) = swap_for_a_link_outside(&s, "revert-out-write");
        let refused = s.service.revert(revert_all(&s.session.id)).await;
        assert_eq!(
            std::fs::read_to_string(&file).ok().as_deref(),
            Some("the operator's\n"),
            "revert: the file outside the project is untouched"
        );
        assert!(
            matches!(&refused, Err(SessionError::Refused(message)) if message.contains("d/x")),
            "{refused:?}"
        );
        assert!(!s.reverted());
        assert_eq!(
            s.read("a.txt").as_deref(),
            Some("goodbye\n"),
            "nothing written"
        );

        // Reverted while `d` was still a directory, linked out afterwards:
        // the unrevert restores the snapshot taken at the revert.
        let s = git_scenario(
            "revert-git-out-unrevert",
            true,
            before,
            change,
            &["d/x", "a.txt"],
        )
        .await;
        s.service.revert(revert_all(&s.session.id)).await.unwrap();
        assert_eq!(s.read("d/x").as_deref(), Some("v1\n"));
        let (_outside, file) = swap_for_a_link_outside(&s, "revert-out-unrevert");
        let refused = s.service.unrevert(&s.session.id).await;
        assert_eq!(
            std::fs::read_to_string(&file).ok().as_deref(),
            Some("the operator's\n"),
            "unrevert: the file outside the project is untouched"
        );
        assert!(
            matches!(&refused, Err(SessionError::Refused(message)) if message.contains("d/x")),
            "{refused:?}"
        );
        assert!(s.reverted(), "still reverted");
        assert_eq!(
            s.read("a.txt").as_deref(),
            Some("hello\n"),
            "nothing written"
        );
    }

    /// The executable bit of a restored file follows the snapshot, set on
    /// the handle that was written.
    #[cfg(unix)]
    #[tokio::test]
    async fn a_guarded_revert_restores_the_executable_bit() {
        use std::os::unix::fs::PermissionsExt;
        fn chmod(worktree: &std::path::Path, name: &str, mode: u32) {
            std::fs::set_permissions(worktree.join(name), std::fs::Permissions::from_mode(mode))
                .unwrap();
        }
        let s = git_scenario_with(
            "revert-git-exec",
            true,
            &[("run.sh", "#!/bin/sh\n"), ("data.txt", "d1\n")],
            |worktree| chmod(worktree, "run.sh", 0o755),
            |worktree| {
                std::fs::remove_file(worktree.join("run.sh")).unwrap();
                std::fs::write(worktree.join("run.sh"), "changed\n").unwrap();
                std::fs::write(worktree.join("data.txt"), "d2\n").unwrap();
                chmod(worktree, "data.txt", 0o755);
            },
            &["run.sh", "data.txt"],
        )
        .await;
        s.service.revert(revert_all(&s.session.id)).await.unwrap();
        let bits = |name: &str| {
            let meta = std::fs::metadata(s.h.worktree.join(name)).unwrap();
            meta.permissions().mode() & 0o111
        };
        assert_eq!(s.read("run.sh").as_deref(), Some("#!/bin/sh\n"));
        assert_ne!(bits("run.sh"), 0, "executable again");
        assert_eq!(s.read("data.txt").as_deref(), Some("d1\n"));
        assert_eq!(bits("data.txt"), 0, "not executable again");
    }

    #[tokio::test]
    async fn second_revert_reuses_the_previous_snapshot() {
        let (h, session) = scenario("revert-twice").await;
        let service = revert_service(&h);
        let first = service
            .revert(RevertInput {
                session_id: session.id.clone(),
                message_id: "msg_u1".to_string(),
                part_id: None,
            })
            .await
            .unwrap();
        let first_snapshot = first
            .revert
            .as_ref()
            .and_then(|revert| revert.snapshot.clone());

        // A second turn: another change recorded as a patch against the
        // restored contents.
        std::fs::write(h.worktree.join("a.txt"), "goodbye\nagain\n").unwrap();
        let second = service
            .revert(RevertInput {
                session_id: session.id.clone(),
                message_id: "msg_u1".to_string(),
                part_id: None,
            })
            .await
            .unwrap();
        let revert = second.revert.as_ref().expect("revert state");
        assert_eq!(
            revert.snapshot.clone(),
            first_snapshot,
            "rev.snapshot = session.revert?.snapshot ?? track()"
        );
    }

    #[tokio::test]
    async fn cleanup_removes_messages_from_the_revert_point() {
        let (h, session) = scenario("revert-cleanup").await;
        // Two more turns after the revert point.
        h.services
            .sessions
            .update_message(&user_message(&session.id, "msg_u2", 3.0))
            .unwrap();
        h.services
            .sessions
            .update_part(&text_part(&session.id, "msg_u2", "prt_u2", "and this"))
            .unwrap();
        h.services
            .sessions
            .update_message(&assistant_message(
                &session.id,
                "msg_a2",
                "msg_u2",
                4,
                None,
                None,
            ))
            .unwrap();
        let service = revert_service(&h);
        let info = service
            .revert(RevertInput {
                session_id: session.id.clone(),
                message_id: "msg_u2".to_string(),
                part_id: None,
            })
            .await
            .unwrap();

        service.cleanup(&info).unwrap();

        let msgs = h.services.sessions.messages(&session.id, None).unwrap();
        let ids: Vec<&str> = msgs.iter().map(|m| message_id(&m.info)).collect();
        // Message-level revert removes the target user message itself
        // (so it can be re-sent) plus everything after it.
        assert_eq!(ids, vec!["msg_u1", "msg_a1"]);
        assert!(h
            .services
            .sessions
            .get(&session.id)
            .unwrap()
            .revert
            .is_none());
    }

    #[tokio::test]
    async fn cleanup_part_level_truncates_the_target_parts() {
        let (h, session) = scenario("revert-cleanup-part").await;
        let service = revert_service(&h);
        let info = service
            .revert(RevertInput {
                session_id: session.id.clone(),
                message_id: "msg_a1".to_string(),
                part_id: Some("prt_2_patch".to_string()),
            })
            .await
            .unwrap();

        service.cleanup(&info).unwrap();

        // The assistant message keeps its parts up to (not including) the
        // revert part; the part itself is gone.
        let msgs = h.services.sessions.messages(&session.id, None).unwrap();
        let assistant = msgs
            .iter()
            .find(|msg| message_id(&msg.info) == "msg_a1")
            .expect("assistant message");
        let remaining: Vec<&str> = assistant.parts.iter().map(part_id).collect();
        assert_eq!(remaining, vec!["prt_1_text"]);
        assert!(h
            .services
            .sessions
            .get(&session.id)
            .unwrap()
            .revert
            .is_none());
    }
}
