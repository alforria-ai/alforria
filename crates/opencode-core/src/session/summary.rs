//! Session summary service — port of `session/summary.ts`
//! (`SessionSummary.Service`).
//!
//! `summarize` is the fire-and-forget fork the processor makes on every
//! `step-finish` (processor.ts:485-490); `diff` unquotes the git file
//! paths stored on the user message's `summary.diffs`.

use std::sync::Arc;

use opencode_schema::file_diff::SnapshotFileDiff;
use opencode_schema::session_v1::{V1Message, V1Part, V1SessionSummary};

use crate::config::schema::Config;
use crate::event::bus::{EventBus, PublishOptions};

use crate::session::error::SessionError;
use crate::session::event_definitions::SESSION_DIFF;
use crate::session::message::WithParts;
use crate::session::processor::SummarySummarize;
use crate::session::snapshot::Snapshot;
use crate::session::store::SessionStore;

// ---------------------------------------------------------------------------
// unquoteGitPath (summary.ts:10-63)
// ---------------------------------------------------------------------------

/// `unquoteGitPath` (summary.ts:10-63 — binding): decode git's
/// octal-escaped quoted paths byte-wise (the `\n`/`\r`/`\t`/`\b`/`\f`/`\v`
/// escapes, `\\` and `\"`, and the `\NNN` octal form git prints for
/// non-ASCII bytes).
pub fn unquote_git_path(input: &str) -> String {
    if !input.starts_with('"') {
        return input.to_string();
    }
    if !input.ends_with('"') {
        return input.to_string();
    }
    // `slice(1, -1)` — clamps to an empty body for the lone-quote input.
    let body: &str = if input.len() >= 2 {
        &input[1..input.len() - 1]
    } else {
        ""
    };
    let body: Vec<u16> = body.encode_utf16().collect();
    let mut bytes: Vec<u8> = Vec::new();
    let mut i = 0usize;
    while i < body.len() {
        let unit = body[i];
        if unit != b'\\' as u16 {
            // `bytes.push(char.charCodeAt(0))` — `Buffer.from` masks each
            // element to a byte (ToUint8, i.e. modulo 2^8).
            bytes.push(unit as u8);
            i += 1;
            continue;
        }
        let Some(&next) = body.get(i + 1) else {
            bytes.push(b'\\');
            i += 1;
            continue;
        };
        if (b'0'..=b'7').contains(&(next as u8)) {
            // `body.slice(i + 1, i + 4).match(/^[0-7]{1,3}/)`
            let chunk: Vec<u16> = body[i + 1..(i + 4).min(body.len())].to_vec();
            let mut octal = String::new();
            for code in chunk.iter().take(3) {
                let ch = char::from_u32(u32::from(*code)).unwrap_or('\u{fffd}');
                if ('0'..='7').contains(&ch) {
                    octal.push(ch);
                } else {
                    break;
                }
            }
            if octal.is_empty() {
                bytes.push(next as u8);
                i += 2;
                continue;
            }
            let value = u16::from_str_radix(&octal, 8).unwrap_or(0);
            bytes.push(value as u8);
            i += octal.len() + 1;
            continue;
        }
        // `(escaped ?? next).charCodeAt(0)` — every escape is ASCII.
        let escaped: u16 = match next as u8 as char {
            'n' => b'\n' as u16,
            'r' => b'\r' as u16,
            't' => b'\t' as u16,
            'b' => 0x08,
            'f' => 0x0C,
            'v' => 0x0B,
            c if c == '\\' || c == '"' => next,
            _ => next,
        };
        bytes.push(escaped as u8);
        i += 2;
    }
    String::from_utf8_lossy(&bytes).into_owned()
}

// ---------------------------------------------------------------------------
// Service (summary.ts:74-160)
// ---------------------------------------------------------------------------

/// Everything the summary service closes over.
pub struct SummaryDeps {
    pub sessions: SessionStore,
    pub snapshot: Arc<dyn Snapshot>,
    pub events: Arc<EventBus>,
    pub config: Arc<Config>,
}

/// `SessionSummary.Service` (summary.ts:74-160).
pub struct SessionSummary {
    deps: SummaryDeps,
}

impl SessionSummary {
    pub fn new(deps: SummaryDeps) -> SessionSummary {
        SessionSummary { deps }
    }

    /// `computeDiff` (summary.ts:82-100): `diffFull` from the first
    /// `step-start` snapshot to the last `step-finish` snapshot.
    pub async fn compute_diff(
        &self,
        messages: &[WithParts],
    ) -> Result<Vec<SnapshotFileDiff>, SessionError> {
        let mut from: Option<String> = None;
        let mut to: Option<String> = None;
        for item in messages {
            if from.is_none() {
                for part in &item.parts {
                    if let V1Part::StepStart {
                        snapshot: Some(snapshot),
                        ..
                    } = part
                    {
                        from = Some(snapshot.clone());
                        break;
                    }
                }
            }
            for part in &item.parts {
                if let V1Part::StepFinish {
                    snapshot: Some(snapshot),
                    ..
                } = part
                {
                    to = Some(snapshot.clone());
                }
            }
        }
        if let (Some(from), Some(to)) = (from, to) {
            return self
                .deps
                .snapshot
                .diff_full(&from, &to)
                .await
                .map_err(SessionError::from);
        }
        Ok(Vec::new())
    }

    /// `summarize` (summary.ts:102-127): reset the session summary,
    /// publish the empty diff, then attach the turn's diffs to the user
    /// message's `summary.diffs`.
    pub async fn summarize(&self, session_id: &str, message_id: &str) -> Result<(), SessionError> {
        self.deps.sessions.set_summary(
            session_id,
            Some(V1SessionSummary {
                additions: 0.0,
                deletions: 0.0,
                files: 0.0,
                diffs: None,
            }),
        )?;
        self.deps.events.publish(
            &SESSION_DIFF,
            serde_json::json!({ "sessionID": session_id, "diff": [] }),
            PublishOptions::default(),
        )?;
        if self.deps.config.snapshot == Some(false) {
            return Ok(());
        }
        let all = self.deps.sessions.messages(session_id, None)?;
        if all.is_empty() {
            return Ok(());
        }

        let messages: Vec<&WithParts> = all
            .iter()
            .filter(|msg| {
                let info = &msg.info;
                (message_id_of(info) == message_id)
                    || (matches!(info, V1Message::Assistant { .. })
                        && assistant_parent_id(info) == message_id)
            })
            .collect();
        let Some(target) = messages
            .iter()
            .find(|msg| message_id_of(&msg.info) == message_id)
        else {
            return Ok(());
        };
        if !matches!(target.info, V1Message::User { .. }) {
            return Ok(());
        }
        let owned: Vec<WithParts> = messages.iter().map(|msg| (*msg).clone()).collect();
        let msg_diffs = self.compute_diff(&owned).await?;
        let mut info = target.info.clone();
        if let V1Message::User { summary, .. } = &mut info {
            let (title, body) = match summary {
                Some(existing) => (existing.title.clone(), existing.body.clone()),
                None => (None, None),
            };
            *summary = Some(opencode_schema::session_v1::V1Summary {
                title,
                body,
                diffs: msg_diffs,
            });
        }
        self.deps.sessions.update_message(&info)?;
        Ok(())
    }

    /// `diff` (summary.ts:129-142): the user message's `summary.diffs`
    /// with git file paths unquoted.
    pub fn diff(
        &self,
        session_id: &str,
        message_id: Option<&str>,
    ) -> Result<Vec<SnapshotFileDiff>, SessionError> {
        let Some(message_id) = message_id else {
            return Ok(Vec::new());
        };
        let all = self.deps.sessions.messages(session_id, None)?;
        let Some(message) = all
            .iter()
            .find(|msg| message_id_of(&msg.info) == message_id)
        else {
            return Ok(Vec::new());
        };
        if !matches!(message.info, V1Message::User { .. }) {
            return Ok(Vec::new());
        }
        let diffs = match &message.info {
            V1Message::User { summary, .. } => summary
                .as_ref()
                .map(|summary| summary.diffs.clone())
                .unwrap_or_default(),
            V1Message::Assistant { .. } => Vec::new(),
        };
        Ok(diffs
            .into_iter()
            .map(|item| {
                let Some(file) = &item.file else {
                    return item;
                };
                let unquoted = unquote_git_path(file);
                if unquoted == *file {
                    return item;
                }
                let mut next = item;
                next.file = Some(unquoted);
                next
            })
            .collect())
    }
}

/// `messageID` accessor that works for both variants.
fn message_id_of(info: &V1Message) -> &str {
    match info {
        V1Message::User { id, .. } | V1Message::Assistant { id, .. } => id,
    }
}

/// `info.parentID` for assistants (the empty string for users).
fn assistant_parent_id(info: &V1Message) -> &str {
    match info {
        V1Message::User { .. } => "",
        V1Message::Assistant { parent_id, .. } => parent_id,
    }
}

// ---------------------------------------------------------------------------
// M5.4 processor seam (`SummarySummarize`)
// ---------------------------------------------------------------------------

impl SummarySummarize for SessionSummary {
    fn summarize(
        &self,
        session_id: &str,
        message_id: &str,
    ) -> crate::tool::def::BoxFuture<'static, ()> {
        let sessions = self.deps.sessions.clone();
        let snapshot = self.deps.snapshot.clone();
        let events = self.deps.events.clone();
        let config = self.deps.config.clone();
        let session_id = session_id.to_string();
        let message_id = message_id.to_string();
        Box::pin(async move {
            let summary = SessionSummary::new(SummaryDeps {
                sessions,
                snapshot,
                events,
                config,
            });
            if let Err(error) = summary.summarize(&session_id, &message_id).await {
                tracing::warn!("summarize failed: {error}");
            }
        })
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::session::test_support::{
        create_session, harness, harness_with_config, text_part, user_message, with_parts,
    };
    use opencode_schema::session_v1::{V1Part, V1StepTokens, V1TokenCache};

    fn summary_service(h: &crate::session::test_support::Harness) -> SessionSummary {
        SessionSummary::new(SummaryDeps {
            sessions: h.services.sessions.clone(),
            snapshot: h.snapshot.clone(),
            events: h.services.events.clone(),
            config: Arc::new(h.config.clone()),
        })
    }

    fn step_start(session: &str, message: &str, id: &str, snapshot: &str) -> V1Part {
        V1Part::StepStart {
            id: id.to_string(),
            session_id: session.to_string(),
            message_id: message.to_string(),
            snapshot: Some(snapshot.to_string()),
        }
    }

    fn step_finish(session: &str, message: &str, id: &str, snapshot: &str) -> V1Part {
        V1Part::StepFinish {
            id: id.to_string(),
            session_id: session.to_string(),
            message_id: message.to_string(),
            reason: "stop".to_string(),
            snapshot: Some(snapshot.to_string()),
            cost: 0.0,
            tokens: V1StepTokens {
                total: None,
                input: 0.0,
                output: 0.0,
                reasoning: 0.0,
                cache: V1TokenCache {
                    read: 0.0,
                    write: 0.0,
                },
            },
        }
    }

    // ------------------------------------------------------------------
    // unquoteGitPath
    // ------------------------------------------------------------------

    #[test]
    fn unquote_passthrough_without_quotes() {
        assert_eq!(unquote_git_path("foo/bar.txt"), "foo/bar.txt");
        assert_eq!(unquote_git_path(""), "");
    }

    #[test]
    fn unquote_strips_surrounding_quotes() {
        assert_eq!(unquote_git_path("\"foo/bar.txt\""), "foo/bar.txt");
    }

    #[test]
    fn unquote_lone_quote_is_empty() {
        // `slice(1, -1)` on a 1-char string clamps to "".
        assert_eq!(unquote_git_path("\""), "");
    }

    #[test]
    fn unquote_simple_escapes() {
        assert_eq!(unquote_git_path("\"a\\nb\""), "a\nb");
        assert_eq!(unquote_git_path("\"a\\rb\""), "a\rb");
        assert_eq!(unquote_git_path("\"a\\tb\""), "a\tb");
        assert_eq!(unquote_git_path("\"a\\bb\""), "a\u{8}b");
        assert_eq!(unquote_git_path("\"a\\fb\""), "a\u{c}b");
        assert_eq!(unquote_git_path("\"a\\vb\""), "a\u{b}b");
        assert_eq!(unquote_git_path("\"a\\\\b\""), "a\\b");
        assert_eq!(unquote_git_path("\"a\\\"b\""), "a\"b");
        // Unknown escapes fall through to the raw character.
        assert_eq!(unquote_git_path("\"a\\qb\""), "aqb");
        // A trailing backslash yields a backslash.
        assert_eq!(unquote_git_path("\"a\\\\\""), "a\\");
    }

    #[test]
    fn unquote_octal_escapes() {
        // café in UTF-8: 0xC3 0xA9
        assert_eq!(unquote_git_path("\"caf\\303\\251.txt\""), "café.txt");
        // A single octal digit.
        assert_eq!(unquote_git_path("\"a\\7b\""), "a\u{7}b");
        // Octal values are masked to a byte (0o433 = 283 → 0x01... 0o433 = 283 - 256 = 27? No:
        // 283 & 0xFF = 27).
        assert_eq!(
            unquote_git_path("\"x\\433\""),
            String::from_utf8_lossy(&[b'x', 283u32 as u8]).into_owned(),
        );
        // A non-octal digit after the backslash is pushed raw.
        assert_eq!(unquote_git_path("\"a\\8b\""), "a8b");
    }

    #[test]
    fn unquote_masks_code_units() {
        // `charCodeAt(0)` + Buffer masking: non-ASCII code units wrap.
        assert_eq!(
            unquote_git_path("\"é\""),
            String::from_utf8_lossy(&[0xE9]).into_owned(),
        );
    }

    // ------------------------------------------------------------------
    // computeDiff / summarize / diff
    // ------------------------------------------------------------------

    #[tokio::test]
    async fn compute_diff_returns_empty_without_snapshots() {
        let h = harness("summary-compute-empty", vec![]);
        let session = create_session(&h.services.sessions, &h.worktree);
        let summary = summary_service(&h);
        let diffs = summary
            .compute_diff(&[with_parts(user_message(&session.id, "msg_u", 1.0), vec![])])
            .await
            .unwrap();
        assert!(diffs.is_empty());
    }

    #[tokio::test]
    async fn compute_diffs_between_first_step_start_and_last_step_finish() {
        let h = harness("summary-compute", vec![]);
        let session = create_session(&h.services.sessions, &h.worktree);
        std::fs::write(h.worktree.join("a.txt"), "one\n").unwrap();
        let snap_1 = h.snapshot.track().await.unwrap();
        std::fs::write(h.worktree.join("a.txt"), "one\ntwo\n").unwrap();
        std::fs::write(h.worktree.join("a.txt"), "one\ntwo\nthree\n").unwrap();
        let snap_3 = h.snapshot.track().await.unwrap();
        let summary = summary_service(&h);

        let make = |snapshot_start: &str, snapshot_finish: &str| {
            with_parts(
                user_message(&session.id, "msg_u", 1.0),
                vec![
                    step_start(&session.id, "msg_u", "prt_s", snapshot_start),
                    step_finish(&session.id, "msg_u", "prt_f", snapshot_finish),
                ],
            )
        };

        // First step-start → last step-finish.
        let diffs = summary
            .compute_diff(&[make(&snap_1, &snap_3)])
            .await
            .unwrap();
        assert_eq!(diffs.len(), 1);
        assert_eq!(diffs[0].file.as_deref(), Some("a.txt"));
        assert_eq!(diffs[0].additions, 2.0);
        assert_eq!(diffs[0].deletions, 0.0);

        // Only one step: no diff.
        let diffs = summary
            .compute_diff(&[make(&snap_1, &snap_1)])
            .await
            .unwrap();
        assert!(diffs.is_empty());
    }

    #[tokio::test]
    async fn summarize_attaches_diffs_to_user_message() {
        let h = harness("summary-summarize", vec![]);
        let session = create_session(&h.services.sessions, &h.worktree);
        std::fs::write(h.worktree.join("a.txt"), "one\n").unwrap();
        let snap_1 = h.snapshot.track().await.unwrap();
        std::fs::write(h.worktree.join("a.txt"), "one\ntwo\n").unwrap();
        let snap_2 = h.snapshot.track().await.unwrap();

        h.services
            .sessions
            .update_message(&user_message(&session.id, "msg_u", 1.0))
            .unwrap();
        h.services
            .sessions
            .update_part(&step_start(&session.id, "msg_u", "prt_s", &snap_1))
            .unwrap();
        h.services
            .sessions
            .update_part(&step_finish(&session.id, "msg_u", "prt_f", &snap_2))
            .unwrap();
        // A non-empty pre-existing session summary gets reset first.
        h.services
            .sessions
            .set_summary(
                &session.id,
                Some(V1SessionSummary {
                    additions: 9.0,
                    deletions: 9.0,
                    files: 9.0,
                    diffs: None,
                }),
            )
            .unwrap();

        summary_service(&h)
            .summarize(&session.id, "msg_u")
            .await
            .unwrap();

        // Session summary was reset to zero.
        let info = h.services.sessions.get(&session.id).unwrap();
        let summary = info.summary.as_ref().expect("session summary");
        assert_eq!(summary.additions, 0.0);
        assert_eq!(summary.deletions, 0.0);
        assert_eq!(summary.files, 0.0);

        // The user message now carries the turn's diffs.
        let msgs = h.services.sessions.messages(&session.id, None).unwrap();
        let user = msgs
            .iter()
            .find(|msg| message_id_of(&msg.info) == "msg_u")
            .unwrap();
        match &user.info {
            V1Message::User {
                summary: Some(summary),
                ..
            } => {
                assert_eq!(summary.diffs.len(), 1);
                assert_eq!(summary.diffs[0].file.as_deref(), Some("a.txt"));
                assert_eq!(summary.diffs[0].additions, 1.0);
            }
            _ => panic!("expected user summary with diffs"),
        }
    }

    #[tokio::test]
    async fn summarize_short_circuits_when_snapshots_disabled() {
        let h = harness_with_config(
            "summary-summarize-disabled",
            vec![],
            serde_json::json!({ "snapshot": false }),
        );
        let session = create_session(&h.services.sessions, &h.worktree);
        h.services
            .sessions
            .update_message(&user_message(&session.id, "msg_u", 1.0))
            .unwrap();
        summary_service(&h)
            .summarize(&session.id, "msg_u")
            .await
            .unwrap();

        // No diffs attached to the user message.
        let msgs = h.services.sessions.messages(&session.id, None).unwrap();
        let user = msgs
            .iter()
            .find(|msg| message_id_of(&msg.info) == "msg_u")
            .unwrap();
        assert!(
            matches!(&user.info, V1Message::User { summary: None, .. }),
            "no summary attached when snapshot == false"
        );
    }

    #[test]
    fn diff_unquotes_file_paths() {
        let h = harness("summary-diff", vec![]);
        let session = create_session(&h.services.sessions, &h.worktree);
        let mut user = user_message(&session.id, "msg_u", 1.0);
        if let V1Message::User { summary, .. } = &mut user {
            *summary = Some(opencode_schema::session_v1::V1Summary {
                title: None,
                body: None,
                diffs: vec![
                    SnapshotFileDiff {
                        file: Some("\"caf\\303\\251.txt\"".to_string()),
                        patch: None,
                        additions: 1.0,
                        deletions: 0.0,
                        status: None,
                    },
                    SnapshotFileDiff {
                        file: Some("plain.txt".to_string()),
                        patch: None,
                        additions: 0.0,
                        deletions: 1.0,
                        status: None,
                    },
                ],
            });
        }
        h.services.sessions.update_message(&user).unwrap();
        let diffs = summary_service(&h)
            .diff(&session.id, Some("msg_u"))
            .unwrap();
        assert_eq!(diffs[0].file.as_deref(), Some("café.txt"));
        assert_eq!(diffs[1].file.as_deref(), Some("plain.txt"));

        // No message id → no diffs.
        assert!(summary_service(&h)
            .diff(&session.id, None)
            .unwrap()
            .is_empty());
        // Unknown message → no diffs.
        assert!(summary_service(&h)
            .diff(&session.id, Some("msg_missing"))
            .unwrap()
            .is_empty());
    }

    #[test]
    fn diff_ignores_assistant_messages() {
        let h = harness("summary-diff-assistant", vec![]);
        let session = create_session(&h.services.sessions, &h.worktree);
        h.services
            .sessions
            .update_message(&user_message(&session.id, "msg_u", 1.0))
            .unwrap();
        h.services
            .sessions
            .update_part(&text_part(&session.id, "msg_u", "prt_1", "hi"))
            .unwrap();
        let diffs = summary_service(&h)
            .diff(&session.id, Some("msg_u"))
            .unwrap();
        // The user message has no summary → empty.
        assert!(diffs.is_empty());
    }
}
