//! Session revert — port of `session/revert.ts` (`SessionRevert.Service`).
//!
//! `revert` records the revert point on the session plus the snapshot
//! patch rollback; `unrevert` restores it; `cleanup` removes the messages
//! after the point. `cleanup` is what `prompt()` runs before every new
//! user message (prompt.ts:1056) — a pending revert materializes as
//! message removal.

use std::path::PathBuf;
use std::sync::Arc;

use opencode_schema::file_diff::SnapshotFileDiff;
use opencode_schema::session_v1::{V1Message, V1Part, V1SessionInfo, V1SessionRevert};

use crate::event::bus::{EventBus, PublishOptions};

use crate::session::error::SessionError;
use crate::session::event_definitions::SESSION_DIFF;
use crate::session::message::message_id;
use crate::session::run_state::SessionRunState;
use crate::session::snapshot::{PatchPart, Snapshot};
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

/// Everything the revert service closes over.
pub struct RevertDeps {
    pub sessions: SessionStore,
    pub events: Arc<EventBus>,
    pub snapshot: Arc<dyn Snapshot>,
    pub summary: Arc<SessionSummary>,
    pub state: Arc<SessionRunState>,
    /// `Global.Path.data` — the storage key-value root.
    pub data_dir: PathBuf,
}

/// `SessionRevert.Service` (revert.ts:28-136).
pub struct SessionRevert {
    deps: RevertDeps,
}

impl SessionRevert {
    pub fn new(deps: RevertDeps) -> SessionRevert {
        SessionRevert { deps }
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

        rev.snapshot = match &session.revert {
            Some(previous) => previous.snapshot.clone(),
            None => self.deps.snapshot.track().await,
        };
        if let Some(previous) = session
            .revert
            .as_ref()
            .and_then(|revert| revert.snapshot.clone())
        {
            self.deps.snapshot.restore(&previous).await?;
        }
        self.deps.snapshot.revert(patches).await?;
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
            Some(opencode_schema::session_v1::V1SessionSummary {
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
            self.deps.snapshot.restore(&snapshot).await?;
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
    use opencode_schema::session_v1::V1Part;

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
        opencode_schema::session_v1::V1SessionInfo,
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
            &opencode_schema::session_v1::V1SessionSummary {
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
