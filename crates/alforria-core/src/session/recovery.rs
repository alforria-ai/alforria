//! What a restart does about turns that were running when the last process
//! died (an alforria extension).
//!
//! Session status, background jobs and the processor's bookkeeping live in
//! memory, so a process that is killed mid-turn leaves the database holding
//! an assistant message that never completed and tool calls still marked
//! running. Nothing ever finishes them, and to a client the session looks
//! busy or silently idle. Where the host stops the machine by killing it,
//! that is every stop.
//!
//! Two parts:
//!
//! * [`RunMarkers`]: one small file per running session, written when a turn
//!   starts and removed when it ends, the way `/goal` keeps its state. A
//!   marker whose process is gone names a session to look at.
//! * [`repair`]: make that session's history say what happened. Tool calls
//!   left pending or running become errors marked `interrupted`, the
//!   assistant message is completed, and the status is idle.
//!
//! What happens next is `server.resume`'s to decide ([`ResumePolicy`]):
//! nothing, a note for the model to read on the next prompt, or a prompt
//! that resumes the turn. A turn is resumed at most once: the prompt that
//! resumes it is recognizable, and a turn started by one is never resumed.
//! `/goal` keeps its own rule: a goal comes back paused whatever this says.

use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::OnceLock;

use alforria_schema::session_status::SessionStatusInfo;
use alforria_schema::session_v1::{
    AssistantError, ToolStateErrorTime, V1Message, V1Part, V1ToolState,
};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use crate::config::schema::{Config, ResumeMode};
use crate::event::bus::PublishOptions;
use crate::event::definition::Definition;
use crate::session::error::SessionError;
use crate::session::message::{message_id, WithParts};
use crate::session::prompt_input::{ModelRef, PromptInput, PromptPartInput};
use crate::session::SessionServices;

/// Published once for each session a restart found mid-turn, after its
/// history was repaired: `{ sessionID, messageID?, incomplete, tools,
/// messages, resume, action }`.
pub const SESSION_INTERRUPTED: Definition = Definition::ephemeral("session.interrupted");

/// The error an assistant message interrupted by a restart is completed
/// with (a `MessageAbortedError`, as after a user's stop).
pub const RESTART_ERROR: &str = "Interrupted by a server restart";

/// The error a tool call interrupted by a restart is completed with.
pub const RESTART_TOOL_ERROR: &str = "Tool execution was interrupted by a server restart. Whether it took effect is unknown: check before relying on it or running it again.";

/// The text of the synthetic user message a shell command (`!cmd`) starts
/// its turn with (prompt.ts:495).
const SHELL_TURN: &str = "The following tool was executed by the user";

/// `server.resume`: what to do with a turn a restart interrupted, once its
/// history is repaired.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum ResumePolicy {
    /// Repair and leave it.
    #[default]
    None,
    /// Repair, and leave a note the model reads on the next prompt.
    Notify,
    /// Repair, and prompt the model to carry on, once.
    Continue,
}

impl ResumePolicy {
    pub fn from_config(config: &Config) -> ResumePolicy {
        match config.server.as_ref().and_then(|server| server.resume) {
            None | Some(ResumeMode::None) => ResumePolicy::None,
            Some(ResumeMode::Notify) => ResumePolicy::Notify,
            Some(ResumeMode::Continue) => ResumePolicy::Continue,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            ResumePolicy::None => "none",
            ResumePolicy::Notify => "notify",
            ResumePolicy::Continue => "continue",
        }
    }
}

// ---------------------------------------------------------------------------
// Process identity
// ---------------------------------------------------------------------------

/// Identifies this server process.
fn boot() -> &'static str {
    static BOOT: OnceLock<String> = OnceLock::new();
    BOOT.get_or_init(|| ulid::Ulid::new().to_string())
}

/// Identifies this boot of the machine, where that can be told (Linux).
fn machine_boot() -> Option<String> {
    std::fs::read_to_string("/proc/sys/kernel/random/boot_id")
        .ok()
        .map(|id| id.trim().to_string())
        .filter(|id| !id.is_empty())
}

/// A running turn, as recorded on disk.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RunMarker {
    pub session_id: String,
    /// The instance directory the session runs in.
    pub directory: PathBuf,
    /// The server process running the turn.
    pub boot: String,
    pub pid: u32,
    /// The machine boot that process belonged to.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub machine_boot: Option<String>,
    pub started_at: u64,
}

impl RunMarker {
    /// Whether the process running this turn is gone. Several alforria
    /// processes can share a data directory (a TUI beside `alforria serve`):
    /// a turn another live process is running is that process's.
    ///
    /// After a reboot a process id proves nothing: the new server is likely
    /// to have the very id the old one had. A marker from another boot of
    /// the machine, or carrying this process's id without being its own, is
    /// from a process that no longer exists. Where liveness cannot be told
    /// at all, the owner is taken to be alive.
    pub fn orphaned(&self) -> bool {
        if self.boot == boot() {
            return false;
        }
        if self.pid == std::process::id() {
            return true;
        }
        if let (Some(then), Some(now)) = (&self.machine_boot, machine_boot()) {
            if *then != now {
                return true;
            }
        }
        if cfg!(target_os = "linux") {
            !Path::new("/proc").join(self.pid.to_string()).exists()
        } else {
            false
        }
    }
}

/// The markers of running turns: `<data>/running/<session>.json`. Files, so
/// they need no schema change, and written through to disk, so one exists
/// for every turn that left anything behind even when the machine is killed
/// the moment after.
#[derive(Debug, Clone)]
pub struct RunMarkers {
    dir: PathBuf,
}

impl RunMarkers {
    pub fn new(data_dir: &Path) -> RunMarkers {
        RunMarkers {
            dir: data_dir.join("running"),
        }
    }

    fn path(&self, session_id: &str) -> Option<PathBuf> {
        let safe = !session_id.is_empty()
            && session_id
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-');
        safe.then(|| self.dir.join(format!("{session_id}.json")))
    }

    /// Record that this process is running a turn of `session_id`.
    pub fn begin(&self, session_id: &str, directory: &Path, now_ms: u64) {
        let Some(path) = self.path(session_id) else {
            return;
        };
        let marker = RunMarker {
            session_id: session_id.to_string(),
            directory: directory.to_path_buf(),
            boot: boot().to_string(),
            pid: std::process::id(),
            machine_boot: machine_boot(),
            started_at: now_ms,
        };
        if let Err(error) = self.write(&path, &marker) {
            tracing::warn!("could not record the running turn of {session_id}: {error}");
        }
    }

    fn write(&self, path: &Path, marker: &RunMarker) -> std::io::Result<()> {
        std::fs::create_dir_all(&self.dir)?;
        let text = serde_json::to_vec(marker).map_err(std::io::Error::other)?;
        let staged = path.with_extension(format!("{}.tmp", std::process::id()));
        {
            let mut file = std::fs::File::create(&staged)?;
            file.write_all(&text)?;
            file.sync_all()?;
        }
        std::fs::rename(&staged, path)?;
        // The rename is what must survive: sync the directory too, where a
        // directory can be opened.
        if let Ok(dir) = std::fs::File::open(&self.dir) {
            let _ = dir.sync_all();
        }
        Ok(())
    }

    /// The turn of `session_id` ended. Only this process's own marker goes.
    pub fn end(&self, session_id: &str) {
        let Some(path) = self.path(session_id) else {
            return;
        };
        if self.read(&path).is_some_and(|marker| marker.boot != boot()) {
            return;
        }
        let _ = std::fs::remove_file(path);
    }

    /// Forget a marker whoever wrote it, once what it pointed at is dealt
    /// with.
    pub fn remove(&self, session_id: &str) {
        if let Some(path) = self.path(session_id) {
            let _ = std::fs::remove_file(path);
        }
    }

    fn read(&self, path: &Path) -> Option<RunMarker> {
        serde_json::from_slice(&std::fs::read(path).ok()?).ok()
    }

    /// The markers of turns whose process is gone.
    pub fn orphans(&self) -> Vec<RunMarker> {
        let Ok(entries) = std::fs::read_dir(&self.dir) else {
            return Vec::new();
        };
        let mut orphans: Vec<RunMarker> = entries
            .flatten()
            .filter(|entry| entry.path().extension().is_some_and(|ext| ext == "json"))
            .filter_map(|entry| self.read(&entry.path()))
            .filter(RunMarker::orphaned)
            .collect();
        orphans.sort_by_key(|marker| marker.started_at);
        orphans
    }
}

// ---------------------------------------------------------------------------
// Repair
// ---------------------------------------------------------------------------

/// What started the turn a restart interrupted.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TurnKind {
    /// A prompt: the user's, or one the system sent on their behalf.
    Prompt,
    /// A `!command` the user ran.
    Shell,
    /// The prompt that resumed an earlier interrupted turn.
    Resume,
    /// A note about an earlier interrupted turn: nothing was running.
    Note,
}

/// What [`repair`] found and fixed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Interrupted {
    pub session_id: String,
    /// The user message of the session's last turn.
    pub turn: Option<String>,
    pub kind: TurnKind,
    /// Whether that turn had not run to its end.
    pub incomplete: bool,
    /// Whether the session has a parent (a subagent's is never resumed:
    /// nobody is waiting for its answer any more).
    pub child: bool,
    /// Assistant messages that were completed.
    pub messages: usize,
    /// Tool calls that were marked interrupted.
    pub tools: usize,
}

impl Interrupted {
    /// Whether anything about the session was wrong or unfinished.
    pub fn found_anything(&self) -> bool {
        self.incomplete || self.messages > 0 || self.tools > 0
    }

    /// What `policy` asks for here. A turn that had finished needs nothing;
    /// a shell command and a subagent are not the model's to resume; and a
    /// turn that was itself a resumption is not resumed a second time, so
    /// a server that dies whenever it resumes cannot loop.
    pub fn action(&self, policy: ResumePolicy) -> ResumePolicy {
        if !self.incomplete || self.child {
            return ResumePolicy::None;
        }
        match (policy, self.kind) {
            (_, TurnKind::Shell | TurnKind::Note) => ResumePolicy::None,
            (ResumePolicy::None, _) => ResumePolicy::None,
            (ResumePolicy::Notify, _) => ResumePolicy::Notify,
            (ResumePolicy::Continue, TurnKind::Resume) => ResumePolicy::Notify,
            (ResumePolicy::Continue, TurnKind::Prompt) => ResumePolicy::Continue,
        }
    }
}

fn restart_kind(part: &V1Part) -> Option<TurnKind> {
    let V1Part::Text {
        metadata: Some(metadata),
        ..
    } = part
    else {
        return None;
    };
    match metadata.get("restart")?.get("kind")?.as_str()? {
        "resume" => Some(TurnKind::Resume),
        "note" => Some(TurnKind::Note),
        _ => None,
    }
}

fn turn_kind(user: &WithParts) -> TurnKind {
    if let Some(kind) = user.parts.iter().find_map(restart_kind) {
        return kind;
    }
    let shell = !user.parts.is_empty()
        && user.parts.iter().all(|part| {
            matches!(
                part,
                V1Part::Text { text, synthetic: Some(true), .. } if text == SHELL_TURN
            )
        });
    if shell {
        TurnKind::Shell
    } else {
        TurnKind::Prompt
    }
}

/// Whether the turn `user` started had not run to its end. The loop ends a
/// turn on a response that finished for any reason but a tool call, or on
/// an error; a response a restart cut off counts as unfinished however
/// often it is looked at.
fn turn_incomplete(turn: &[WithParts], user_id: &str) -> bool {
    let last = turn.iter().rev().find(|message| {
        matches!(
            &message.info,
            V1Message::Assistant { parent_id, .. } if parent_id == user_id
        )
    });
    let Some(last) = last else {
        return true;
    };
    match &last.info {
        V1Message::Assistant {
            error: Some(AssistantError::Aborted { message }),
            ..
        } => message == RESTART_ERROR,
        V1Message::Assistant { error: Some(_), .. } => false,
        V1Message::Assistant { finish, .. } => {
            matches!(finish.as_deref(), None | Some("tool-calls" | "unknown"))
        }
        V1Message::User { .. } => false,
    }
}

/// Make a session whose turn a dead process was running consistent again.
/// `None` when the session no longer exists. Safe to run twice.
pub fn repair(
    services: &SessionServices,
    session_id: &str,
) -> Result<Option<Interrupted>, SessionError> {
    let session = match services.sessions.get(session_id) {
        Ok(session) => session,
        Err(SessionError::NotFound(_)) => return Ok(None),
        Err(error) => return Err(error),
    };
    let now = services.clock().now_ms();

    // What the turn was, judged before anything is changed.
    let user = services
        .sessions
        .find_message(session_id, &|message: &WithParts| {
            matches!(message.info, V1Message::User { .. })
        })?;
    let (turn, kind, incomplete) = match &user {
        None => (None, TurnKind::Prompt, false),
        Some(user) => {
            let id = message_id(&user.info).to_string();
            let kind = turn_kind(user);
            let messages = services.sessions.messages_since(session_id, &id)?;
            let incomplete = kind != TurnKind::Note && turn_incomplete(&messages, &id);
            (Some(id), kind, incomplete)
        }
    };

    // Tool calls nothing will ever settle, wherever in the session.
    let dangling = services.messages.dangling(session_id)?;
    for part in &dangling.tools {
        let V1Part::Tool {
            id,
            session_id,
            message_id,
            call_id,
            tool,
            state,
            metadata,
        } = part
        else {
            continue;
        };
        let (input, start, mut state_metadata) = match state {
            V1ToolState::Pending { input, .. } => (input.clone(), now, Default::default()),
            V1ToolState::Running {
                input,
                time,
                metadata,
                ..
            } => (
                input.clone(),
                time.start,
                metadata.clone().unwrap_or_default(),
            ),
            V1ToolState::Completed { .. } | V1ToolState::Error { .. } => continue,
        };
        // `interrupted` is what the loop reads to know this call is not
        // work still to be done.
        state_metadata.insert("interrupted".to_string(), Value::Bool(true));
        state_metadata.insert("restart".to_string(), Value::Bool(true));
        // An interrupted call that had produced output is shown to the
        // model as that output (message-v2.ts:325-347), so the output has
        // to say how it ended.
        if let Some(Value::String(output)) = state_metadata.get("output").cloned() {
            let output = output.trim_end();
            let separator = if output.is_empty() { "" } else { "\n\n" };
            state_metadata.insert(
                "output".to_string(),
                Value::String(format!("{output}{separator}[{RESTART_TOOL_ERROR}]")),
            );
        }
        services.sessions.update_part(&V1Part::Tool {
            id: id.clone(),
            session_id: session_id.clone(),
            message_id: message_id.clone(),
            call_id: call_id.clone(),
            tool: tool.clone(),
            state: V1ToolState::Error {
                input,
                error: RESTART_TOOL_ERROR.to_string(),
                metadata: Some(state_metadata),
                time: ToolStateErrorTime {
                    start,
                    end: now.max(start),
                },
            },
            metadata: metadata.clone(),
        })?;
    }

    // Assistant messages nothing will ever complete.
    for id in &dangling.assistants {
        let Ok(message) = services.sessions.get_message(session_id, id) else {
            continue;
        };
        for part in &message.parts {
            match part {
                V1Part::Text {
                    time: Some(time), ..
                } if time.end.is_none() => {
                    let mut part = part.clone();
                    if let V1Part::Text {
                        time: Some(time), ..
                    } = &mut part
                    {
                        time.end = Some(now.max(time.start));
                    }
                    services.sessions.update_part(&part)?;
                }
                V1Part::Reasoning { time, .. } if time.end.is_none() => {
                    let mut part = part.clone();
                    if let V1Part::Reasoning { time, .. } = &mut part {
                        time.end = Some(now.max(time.start));
                    }
                    services.sessions.update_part(&part)?;
                }
                _ => {}
            }
        }
        let mut info = message.info.clone();
        if let V1Message::Assistant {
            error,
            finish,
            time,
            ..
        } = &mut info
        {
            // A response that had finished only lacked its completion
            // time; one cut off mid-stream ends as an abort does.
            if error.is_none() && finish.is_none() {
                *error = Some(AssistantError::Aborted {
                    message: RESTART_ERROR.to_string(),
                });
            }
            time.completed = Some(now.max(time.created));
        }
        services.sessions.update_message(&info)?;
    }

    services
        .status
        .set(session_id, SessionStatusInfo::Idle)
        .map_err(SessionError::from)?;

    Ok(Some(Interrupted {
        session_id: session_id.to_string(),
        turn,
        kind,
        incomplete,
        child: session.parent_id.is_some(),
        messages: dangling.assistants.len(),
        tools: dangling.tools.len(),
    }))
}

/// Announce a repaired session.
pub fn announce(
    services: &SessionServices,
    interrupted: &Interrupted,
    policy: ResumePolicy,
    action: ResumePolicy,
) -> Result<(), SessionError> {
    let mut data = json!({
        "sessionID": interrupted.session_id,
        "incomplete": interrupted.incomplete,
        "tools": interrupted.tools,
        "messages": interrupted.messages,
        "resume": policy.as_str(),
        "action": match action {
            ResumePolicy::None => "repaired",
            ResumePolicy::Notify => "notified",
            ResumePolicy::Continue => "continued",
        },
    });
    if let Some(turn) = &interrupted.turn {
        data["messageID"] = json!(turn);
    }
    services
        .events
        .publish(&SESSION_INTERRUPTED, data, PublishOptions::default())?;
    Ok(())
}

// ---------------------------------------------------------------------------
// The messages a policy sends
// ---------------------------------------------------------------------------

/// The note left for the model under `notify`. Read with the next prompt;
/// it starts no turn.
const NOTE: &str = "<restart_note>\nThe server restarted while your previous turn was in progress, and that turn was not resumed. Tool calls that were running at that moment are marked as interrupted: they may or may not have taken effect. Keep this in mind when the user next writes, and check before repeating anything. Do not act on this note by itself.\n</restart_note>";

/// The prompt that resumes a turn under `continue`. Worded like the
/// subagent auto-resume ([`crate::session::retry::resume_note`]) so clients
/// show it the same way.
const RESUME: &str = "<auto_resume>\nYour previous attempt stopped with an error: the server restarted while you were working\n\nTool calls that were running at that moment are marked as interrupted: they may or may not have taken effect, so check before repeating them. Continue the task from where you left off. Do not redo work you have already finished.\n</auto_resume>";

fn restart_part(text: &str, kind: &str, turn: &str) -> PromptPartInput {
    PromptPartInput::Text {
        id: None,
        text: text.to_string(),
        synthetic: Some(true),
        ignored: None,
        time: None,
        metadata: json!({ "restart": { "kind": kind, "turn": turn } })
            .as_object()
            .cloned(),
    }
}

/// The prompt to send for `action` (`None` for [`ResumePolicy::None`], or
/// when the turn's user message is gone). It carries the interrupted turn's
/// agent, model and options, since a resumed turn runs under its newest
/// user message's.
pub fn follow_up(
    services: &SessionServices,
    interrupted: &Interrupted,
    action: ResumePolicy,
) -> Option<PromptInput> {
    let turn = interrupted.turn.as_deref()?;
    let (text, kind, no_reply) = match action {
        ResumePolicy::None => return None,
        ResumePolicy::Notify => (NOTE, "note", true),
        ResumePolicy::Continue => (RESUME, "resume", false),
    };
    let user = services
        .sessions
        .get_message(&interrupted.session_id, turn)
        .ok()?;
    let V1Message::User {
        agent,
        model,
        format,
        system,
        tools,
        ..
    } = &user.info
    else {
        return None;
    };
    Some(PromptInput {
        session_id: interrupted.session_id.clone(),
        message_id: None,
        model: Some(ModelRef {
            provider_id: model.provider_id.clone(),
            model_id: model.model_id.clone(),
        }),
        agent: Some(agent.clone()),
        no_reply: no_reply.then_some(true),
        tools: tools.clone(),
        format: format.clone(),
        system: system.clone(),
        variant: model.variant.clone(),
        parts: vec![restart_part(text, kind, turn)],
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::session::render::to_model_messages;
    use crate::session::test_support::{
        assistant_message, create_session, harness, test_model, text_part, user_message, Harness,
    };
    use alforria_llm::schema::messages::ContentPart;
    use alforria_schema::session_v1::{TextPartTime, ToolStateRunningTime};

    fn marker(boot: &str, pid: u32, machine_boot: Option<&str>) -> RunMarker {
        RunMarker {
            session_id: "ses_a".to_string(),
            directory: PathBuf::from("/repo"),
            boot: boot.to_string(),
            pid,
            machine_boot: machine_boot.map(str::to_string),
            started_at: 1,
        }
    }

    #[test]
    fn a_marker_is_orphaned_only_when_its_process_is_gone() {
        let here = machine_boot();
        // This process's own.
        assert!(!marker(boot(), std::process::id(), here.as_deref()).orphaned());
        // Another boot of the server under this process's id: after a
        // reboot the new server gets the id the old one had.
        assert!(marker("other", std::process::id(), here.as_deref()).orphaned());
        if cfg!(target_os = "linux") {
            // A process that does not exist.
            assert!(marker("other", u32::MAX - 7, here.as_deref()).orphaned());
            // A live process on this boot of the machine: another server
            // sharing the data directory.
            assert!(!marker("other", 1, here.as_deref()).orphaned());
            // The same id, from before the machine restarted.
            assert!(marker("other", 1, Some("another-boot")).orphaned());
        }
    }

    #[test]
    fn markers_are_written_through_and_only_orphans_are_reported() {
        let dir = crate::storage::test_support::TempDir::new("run-markers");
        let markers = RunMarkers::new(dir.path());
        assert!(markers.orphans().is_empty());
        markers.begin("ses_live", Path::new("/repo"), 5);
        let path = dir.path().join("running").join("ses_live.json");
        let written: RunMarker = serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
        assert_eq!(written.session_id, "ses_live");
        assert_eq!(written.directory, PathBuf::from("/repo"));
        assert_eq!(written.pid, std::process::id());
        // A turn this process is running is not an orphan.
        assert!(markers.orphans().is_empty());

        // One a dead process left.
        let dead = RunMarker {
            session_id: "ses_dead".to_string(),
            boot: "gone".to_string(),
            ..written.clone()
        };
        std::fs::write(
            dir.path().join("running").join("ses_dead.json"),
            serde_json::to_vec(&dead).unwrap(),
        )
        .unwrap();
        assert_eq!(markers.orphans(), vec![dead]);
        // Ending a turn clears this process's marker, never another's.
        markers.end("ses_dead");
        markers.end("ses_live");
        assert!(!path.exists());
        assert_eq!(markers.orphans().len(), 1);
        markers.remove("ses_dead");
        assert!(markers.orphans().is_empty());
        // An id that is not one cannot name a file elsewhere.
        markers.begin("../escape", Path::new("/repo"), 5);
        assert!(!dir.path().join("escape.json").exists());
    }

    fn tool(session: &str, message: &str, id: &str, state: V1ToolState) -> V1Part {
        V1Part::Tool {
            id: id.to_string(),
            session_id: session.to_string(),
            message_id: message.to_string(),
            call_id: format!("call_{id}"),
            tool: "bash".to_string(),
            state,
            metadata: None,
        }
    }

    fn input() -> alforria_schema::schema::JsonMap {
        json!({ "command": "sleep 60" })
            .as_object()
            .unwrap()
            .clone()
    }

    /// A session whose only turn was cut off: text still streaming, one
    /// tool running, one not yet started.
    fn interrupted_session(h: &Harness) -> String {
        let session = create_session(&h.services.sessions, &h.worktree);
        let id = session.id.clone();
        let sessions = &h.services.sessions;
        sessions
            .update_message(&user_message(&id, "msg_1", 1.0))
            .unwrap();
        sessions
            .update_part(&text_part(&id, "msg_1", "prt_1", "run the build"))
            .unwrap();
        sessions
            .update_message(&assistant_message(&id, "msg_2", "msg_1", 2, None, None))
            .unwrap();
        let mut streaming = text_part(&id, "msg_2", "prt_2", "Starting the bui");
        if let V1Part::Text { time, .. } = &mut streaming {
            *time = Some(TextPartTime {
                start: 3,
                end: None,
            });
        }
        sessions.update_part(&streaming).unwrap();
        sessions
            .update_part(&tool(
                &id,
                "msg_2",
                "prt_3",
                V1ToolState::Running {
                    input: input(),
                    title: None,
                    metadata: Some(json!({ "output": "" }).as_object().unwrap().clone()),
                    time: ToolStateRunningTime { start: 4 },
                },
            ))
            .unwrap();
        sessions
            .update_part(&tool(
                &id,
                "msg_2",
                "prt_4",
                V1ToolState::Pending {
                    input: input(),
                    raw: String::new(),
                },
            ))
            .unwrap();
        id
    }

    #[test]
    fn repair_closes_what_a_dead_process_left_open() {
        let h = harness("recovery-repair", Vec::new());
        let id = interrupted_session(&h);
        h.services.status.set(&id, SessionStatusInfo::Busy).unwrap();

        let found = repair(&h.services, &id).unwrap().unwrap();
        assert_eq!(
            found,
            Interrupted {
                session_id: id.clone(),
                turn: Some("msg_1".to_string()),
                kind: TurnKind::Prompt,
                incomplete: true,
                child: false,
                messages: 1,
                tools: 2,
            }
        );
        assert!(found.found_anything());

        let message = h.services.sessions.get_message(&id, "msg_2").unwrap();
        let V1Message::Assistant { error, time, .. } = &message.info else {
            panic!("an assistant message");
        };
        assert_eq!(
            serde_json::to_value(error).unwrap(),
            json!({ "name": "MessageAbortedError", "data": { "message": RESTART_ERROR } })
        );
        assert!(time.completed.is_some());
        for part in &message.parts {
            match part {
                V1Part::Text { time, .. } => assert!(time.as_ref().unwrap().end.is_some()),
                V1Part::Tool { state, .. } => {
                    let V1ToolState::Error {
                        error, metadata, ..
                    } = state
                    else {
                        panic!("an errored tool, got {state:?}");
                    };
                    assert_eq!(error, RESTART_TOOL_ERROR);
                    let metadata = metadata.as_ref().unwrap();
                    assert_eq!(metadata["interrupted"], json!(true));
                    assert_eq!(metadata["restart"], json!(true));
                    // Output the call had produced ends by saying so.
                    if let Some(output) = metadata.get("output") {
                        assert_eq!(*output, json!(format!("[{RESTART_TOOL_ERROR}]")));
                    }
                }
                other => panic!("unexpected part {other:?}"),
            }
        }
        assert!(matches!(
            h.services.status.get(&id),
            SessionStatusInfo::Idle
        ));

        // The history is valid for the next request: every tool call the
        // model made has a result.
        let history = h.services.sessions.messages(&id, None).unwrap();
        let rendered = to_model_messages(&history, &test_model().render(), None);
        let calls: Vec<&str> = rendered
            .iter()
            .flat_map(|message| &message.content)
            .filter_map(|part| match part {
                ContentPart::ToolCall { id, .. } => Some(id.as_str()),
                _ => None,
            })
            .collect();
        let results: Vec<&str> = rendered
            .iter()
            .flat_map(|message| &message.content)
            .filter_map(|part| match part {
                ContentPart::ToolResult { id, .. } => Some(id.as_str()),
                _ => None,
            })
            .collect();
        assert_eq!(calls, vec!["call_prt_3", "call_prt_4"]);
        assert_eq!(calls, results);
        assert!(serde_json::to_string(&rendered)
            .unwrap()
            .contains("interrupted by a server restart"));

        // A second pass finds nothing to fix and still knows the turn did
        // not finish.
        let again = repair(&h.services, &id).unwrap().unwrap();
        assert_eq!(
            again,
            Interrupted {
                messages: 0,
                tools: 0,
                ..found
            }
        );
        assert!(repair(&h.services, "ses_missing").unwrap().is_none());
    }

    #[test]
    fn a_repaired_session_is_announced() {
        let h = harness("recovery-announce", Vec::new());
        let id = interrupted_session(&h);
        let mut events = h.services.events.subscribe("session.interrupted");
        let found = repair(&h.services, &id).unwrap().unwrap();
        announce(
            &h.services,
            &found,
            ResumePolicy::Continue,
            found.action(ResumePolicy::Continue),
        )
        .unwrap();
        let event = events.try_recv().unwrap();
        assert_eq!(event.r#type, "session.interrupted");
        assert_eq!(
            event.data,
            json!({
                "sessionID": id,
                "messageID": "msg_1",
                "incomplete": true,
                "tools": 2,
                "messages": 1,
                "resume": "continue",
                "action": "continued",
            })
        );
    }

    #[test]
    fn a_turn_that_finished_is_left_alone() {
        let h = harness("recovery-finished", Vec::new());
        let session = create_session(&h.services.sessions, &h.worktree);
        let id = session.id.clone();
        let sessions = &h.services.sessions;
        sessions
            .update_message(&user_message(&id, "msg_1", 1.0))
            .unwrap();
        let mut done = assistant_message(&id, "msg_2", "msg_1", 2, None, Some("stop".to_string()));
        if let V1Message::Assistant { time, .. } = &mut done {
            time.completed = Some(3);
        }
        sessions.update_message(&done).unwrap();

        let found = repair(&h.services, &id).unwrap().unwrap();
        assert!(!found.incomplete);
        assert!(!found.found_anything());
        for policy in [
            ResumePolicy::None,
            ResumePolicy::Notify,
            ResumePolicy::Continue,
        ] {
            assert_eq!(found.action(policy), ResumePolicy::None);
        }
    }

    #[test]
    fn a_turn_is_resumed_once_and_only_where_that_makes_sense() {
        let turn = |kind, child| Interrupted {
            session_id: "ses_a".to_string(),
            turn: Some("msg_1".to_string()),
            kind,
            incomplete: true,
            child,
            messages: 1,
            tools: 0,
        };
        let prompt = turn(TurnKind::Prompt, false);
        assert_eq!(prompt.action(ResumePolicy::None), ResumePolicy::None);
        assert_eq!(prompt.action(ResumePolicy::Notify), ResumePolicy::Notify);
        assert_eq!(
            prompt.action(ResumePolicy::Continue),
            ResumePolicy::Continue
        );
        // The turn that died was itself a resumption: say so, do not try
        // again.
        let resumed = turn(TurnKind::Resume, false);
        assert_eq!(resumed.action(ResumePolicy::Continue), ResumePolicy::Notify);
        // A shell command and a subagent are not the model's to resume.
        for other in [turn(TurnKind::Shell, false), turn(TurnKind::Prompt, true)] {
            assert_eq!(other.action(ResumePolicy::Continue), ResumePolicy::None);
            assert_eq!(other.action(ResumePolicy::Notify), ResumePolicy::None);
        }
        assert_eq!(
            turn(TurnKind::Note, false).action(ResumePolicy::Continue),
            ResumePolicy::None
        );
    }

    #[test]
    fn the_follow_up_carries_the_turns_settings_and_is_recognized_afterwards() {
        let h = harness("recovery-follow-up", Vec::new());
        let id = interrupted_session(&h);
        let found = repair(&h.services, &id).unwrap().unwrap();

        assert!(follow_up(&h.services, &found, ResumePolicy::None).is_none());
        let note = follow_up(&h.services, &found, ResumePolicy::Notify).unwrap();
        assert_eq!(note.no_reply, Some(true));
        let resume = follow_up(&h.services, &found, ResumePolicy::Continue).unwrap();
        assert_eq!(resume.no_reply, None);
        assert_eq!(resume.agent.as_deref(), Some("build"));
        let model = resume.model.as_ref().unwrap();
        assert_eq!(
            (model.provider_id.as_str(), model.model_id.as_str()),
            ("anthropic", "claude")
        );
        let PromptPartInput::Text {
            text,
            synthetic,
            metadata,
            ..
        } = &resume.parts[0]
        else {
            panic!("a text part");
        };
        assert!(text.starts_with("<auto_resume>\nYour previous attempt stopped with an error: "));
        assert_eq!(*synthetic, Some(true));
        assert_eq!(
            metadata.as_ref().unwrap()["restart"],
            json!({ "kind": "resume", "turn": "msg_1" })
        );

        // Once that prompt is the session's newest user message, the turn it
        // starts is known for what it is.
        let sessions = &h.services.sessions;
        sessions
            .update_message(&user_message(&id, "msg_3", 5.0))
            .unwrap();
        let mut part = text_part(&id, "msg_3", "prt_9", text);
        if let V1Part::Text {
            synthetic,
            metadata: slot,
            ..
        } = &mut part
        {
            *synthetic = Some(true);
            *slot = metadata.clone();
        }
        sessions.update_part(&part).unwrap();
        let again = repair(&h.services, &id).unwrap().unwrap();
        assert_eq!(again.kind, TurnKind::Resume);
        assert!(again.incomplete);
        assert_eq!(again.action(ResumePolicy::Continue), ResumePolicy::Notify);
    }

    #[test]
    fn the_policy_reads_from_config() {
        let policy =
            |text: &str| ResumePolicy::from_config(&serde_json::from_str::<Config>(text).unwrap());
        assert_eq!(policy("{}"), ResumePolicy::None);
        assert_eq!(
            policy(r#"{"server": {"resume": "none"}}"#),
            ResumePolicy::None
        );
        assert_eq!(
            policy(r#"{"server": {"resume": "notify"}}"#),
            ResumePolicy::Notify
        );
        assert_eq!(
            policy(r#"{"server": {"resume": "continue"}}"#),
            ResumePolicy::Continue
        );
        assert!(serde_json::from_str::<Config>(r#"{"server": {"resume": "always"}}"#).is_err());
    }

    #[tokio::test]
    async fn a_running_turn_is_marked_for_as_long_as_it_runs() {
        use crate::session::run_state::Work;
        let h = harness("recovery-marked", Vec::new());
        let markers = RunMarkers::new(h.temp.path());
        h.services
            .run_state
            .track_runs(markers.clone(), h.worktree.clone());
        let path = h.temp.path().join("running").join("ses_run.json");
        let seen = std::sync::Arc::new(std::sync::Mutex::new(None));
        let work: Work<WithParts, SessionError> = {
            let (path, seen) = (path.clone(), seen.clone());
            std::sync::Arc::new(move || {
                let (path, seen) = (path.clone(), seen.clone());
                Box::pin(async move {
                    *seen.lock().unwrap() = std::fs::read(&path).ok();
                    Ok(WithParts {
                        info: user_message("ses_run", "msg_1", 1.0),
                        parts: Vec::new(),
                    })
                })
            })
        };
        h.services
            .run_state
            .ensure_running(
                "ses_run",
                tokio_util::sync::CancellationToken::new(),
                work.clone(),
                work,
            )
            .await
            .unwrap();
        let during: RunMarker =
            serde_json::from_slice(seen.lock().unwrap().as_ref().expect("a marker")).unwrap();
        assert_eq!(during.session_id, "ses_run");
        assert_eq!(during.directory, h.worktree);
        assert!(!path.exists(), "the marker goes when the turn ends");
    }
}
