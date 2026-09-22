//! `component/dialog-session-list.tsx`, `dialog-session-rename.tsx`,
//! `routes/session/dialog-timeline.tsx`, `dialog-fork-from-timeline.tsx`,
//! `dialog-message.tsx`, `dialog-stash.tsx`, `dialog-tag.tsx` and
//! `dialog-move-session.tsx`.

use serde_json::Value;

use super::primitives::SelectOption;
use crate::state::route::Route;
use crate::state::{App, Effect};
use crate::ui::dialogs::DialogFrame;
use alforria_schema::session_v1::V1Message;

/// `DialogSessionList.options` (`dialog-session-list.tsx:157-289`) —
/// pinned first, then recency-grouped; the browse list is the sync store
/// (the port filters client-side; the TS server search is a recorded
/// divergence).
pub fn session_list_options(app: &App, frame: &DialogFrame) -> Vec<SelectOption> {
    let query = frame.select.filter.to_lowercase();
    let pinned: Vec<String> = app
        .state
        .local
        .session_pinned()
        .iter()
        .filter(|id| {
            app.state
                .sync
                .session
                .iter()
                .any(|session| session.id == **id)
        })
        .cloned()
        .collect();
    let mut options = Vec::new();
    let build = |id: &str, category: &str| -> Option<SelectOption> {
        let session = app
            .state
            .sync
            .session
            .iter()
            .find(|session| session.id == id)?;
        if session.parent_id.is_some() {
            return None;
        }
        if !query.is_empty() && !session.title.to_lowercase().contains(&query) {
            return None;
        }
        let is_deleting = frame.pending_delete.as_deref() == Some(session.id.as_str());
        let title = if is_deleting {
            // `Press ${deleteHint()} again to confirm`
            // (dialog-session-list.tsx:58,246).
            format!(
                "Press {} again to confirm",
                crate::ui::dialogs::key_hint(app, "session_delete").unwrap_or_default()
            )
        } else {
            session.title.clone()
        };

        // The busy spinner / quick-switch slot gutter
        // (dialog-session-list.tsx:231-240).
        let mut gutter = None;
        let status = app.state.sync.session_status.get(&session.id);
        let working = matches!(
            status,
            Some(alforria_schema::session_status::SessionStatusInfo::Busy)
                | Some(alforria_schema::session_status::SessionStatusInfo::Retry { .. })
        );
        if working {
            gutter = Some(app.session_spinner().to_string());
        } else {
            let slot = app
                .state
                .local
                .session_slots(&app.state.sync)
                .iter()
                .position(|slot| slot == &session.id);
            if let Some(slot) = slot {
                gutter = Some((slot + 1).to_string());
            }
        }
        Some(
            SelectOption::new(title)
                .with_value(session.id.clone())
                .with_category(category)
                .with_gutter(gutter),
        )
    };
    for id in &pinned {
        if let Some(option) = build(id, "Pinned") {
            options.push(option);
        }
    }
    let mut sorted: Vec<&alforria_schema::session_v1::V1SessionInfo> = app
        .state
        .sync
        .session
        .iter()
        .filter(|session| session.parent_id.is_none())
        .collect();
    sorted.sort_by_key(|session| std::cmp::Reverse(session.time.updated));
    for session in sorted {
        if pinned.contains(&session.id) {
            continue;
        }
        // "Today" or the date label (dialog-session-list.tsx:257).
        let category = {
            let secs = (session.time.updated / 1000) as i64;
            let now = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_secs() as i64)
                .unwrap_or(0);
            let day_of = |secs: i64| secs.div_euclid(86_400);
            let label = if day_of(secs) == day_of(now) {
                "Today".to_string()
            } else {
                // `new Date(...).toDateString()` — e.g. "Mon Sep 22 2026".
                let wd = ((day_of(secs) + 4) % 7 + 7) % 7;
                let weekdays = ["Sun", "Mon", "Tue", "Wed", "Thu", "Fri", "Sat"];
                let months = [
                    "Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov",
                    "Dec",
                ];
                // Civil-date conversion (days since epoch -> y/m/d).
                let z = day_of(secs) + 719_468;
                let era = z.div_euclid(146_097);
                let doe = z - era * 146_097;
                let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
                let year = yoe + era * 400;
                let yd = doe - (365 * yoe + yoe / 4 - yoe / 100);
                let mp = (5 * yd + 2) / 153;
                let day = yd - (153 * mp + 2) / 5 + 1;
                let month = if mp < 10 { mp + 3 } else { mp - 9 };
                let year = if month <= 2 { year + 1 } else { year };
                format!(
                    "{} {} {} {}",
                    weekdays[wd as usize],
                    months[(month as usize - 1) % 12],
                    day,
                    year
                )
            };
            label
        };
        if let Some(option) = build(&session.id, &category) {
            options.push(option);
        }
    }
    options
}

/// `DialogTimeline.options` (`dialog-timeline.tsx`): user messages with
/// a non-synthetic text part, newest first.
pub fn timeline_options(app: &App) -> Vec<SelectOption> {
    let Route::Session { session_id, .. } = &app.state.route.data else {
        return Vec::new();
    };
    let messages = app
        .state
        .sync
        .message
        .get(session_id)
        .cloned()
        .unwrap_or_default();
    let mut options = Vec::new();
    for message in &messages {
        if !matches!(message, V1Message::User { .. }) {
            continue;
        }
        if let Some(message_id) = message_id(message) {
            let Some(text) = message_text(app, message_id) else {
                continue;
            };
            options.push(
                SelectOption::new(text.replace('\n', " ")).with_value(message_id.to_string()),
            );
        }
    }
    options.reverse();
    options
}

/// `DialogForkFromTimeline.options`
/// (`dialog-fork-from-timeline.tsx:21-52`).
pub fn fork_options(app: &App) -> Vec<SelectOption> {
    let mut options = vec![SelectOption::new("Full session").with_value("full")];
    options.extend(timeline_options(app));
    options
}

/// `DialogMessage.options` (`dialog-message.tsx`).
pub fn message_options() -> Vec<SelectOption> {
    vec![
        SelectOption::new("Revert")
            .with_value("session.revert")
            .with_description("undo messages and file changes"),
        SelectOption::new("Copy")
            .with_value("message.copy")
            .with_description("message text to clipboard"),
        SelectOption::new("Fork")
            .with_value("session.fork")
            .with_description("create a new session"),
    ]
}

/// Run the selected `DialogMessage` action.
pub fn message_action(
    app: &mut App,
    session_id: &str,
    message_id: &str,
    option: &SelectOption,
) -> Vec<Effect> {
    match option.value.as_deref() {
        Some("session.revert") => {
            crate::ui::dialogs::clear(app);
            // Revert restores the input AND the file parts
            // (dialog-message.tsx:38-51).
            let mut input = String::new();
            let mut parts = Vec::new();
            for part in message_parts(app, message_id) {
                match part {
                    alforria_schema::session_v1::V1Part::Text {
                        text,
                        synthetic: Some(false),
                        ..
                    } => {
                        input.push_str(&text);
                    }
                    alforria_schema::session_v1::V1Part::File { .. } => {
                        if let Ok(value) = serde_json::to_value(&part) {
                            if let Some(part) = crate::state::prompt::PromptPart::from_value(&value)
                            {
                                parts.push(part);
                            }
                        }
                    }
                    _ => {}
                }
            }
            app.ui.prompt.textarea.set_text(&input);
            app.ui.prompt.parts = parts;
            vec![Effect::SessionRevert {
                session_id: session_id.to_string(),
                message_id: message_id.to_string(),
            }]
        }
        Some("message.copy") => {
            crate::ui::dialogs::clear(app);
            let mut text = String::new();
            for part in message_parts(app, message_id) {
                if let alforria_schema::session_v1::V1Part::Text {
                    text: part_text,
                    synthetic,
                    ..
                } = part
                {
                    if !synthetic.unwrap_or(false) {
                        text.push_str(&part_text);
                    }
                }
            }
            vec![Effect::ClipboardWrite {
                text,
                success: None,
                failure: None,
            }]
        }
        Some("session.fork") => vec![Effect::SessionForkFromMessage {
            session_id: session_id.to_string(),
            message_id: Some(message_id.to_string()),
            seed_prompt: true,
        }],
        _ => Vec::new(),
    }
}

fn message_id(message: &V1Message) -> Option<&str> {
    match message {
        V1Message::User { id, .. } | V1Message::Assistant { id, .. } => Some(id),
    }
}

fn message_parts(app: &App, message_id: &str) -> Vec<alforria_schema::session_v1::V1Part> {
    app.state
        .sync
        .part
        .get(message_id)
        .cloned()
        .unwrap_or_default()
}

fn message_text(app: &App, message_id: &str) -> Option<String> {
    for part in message_parts(app, message_id) {
        if let alforria_schema::session_v1::V1Part::Text {
            text,
            synthetic,
            ignored,
            ..
        } = part
        {
            if !synthetic.unwrap_or(false) && !ignored.unwrap_or(false) {
                return Some(text);
            }
        }
    }
    None
}

/// `DialogStash.options` (`dialog-stash.tsx:30-52`) — most recent first.
pub fn stash_options(app: &App, frame: &DialogFrame) -> Vec<SelectOption> {
    let stash = &app.ui.prompt.stash;
    let mut options = Vec::new();
    for (index, entry) in stash.list().iter().enumerate().rev() {
        let first_line = entry.entry.input.split('\n').next().unwrap_or("").trim();
        let preview: String = first_line.chars().take(50).collect();
        let is_deleting = frame.pending_delete == Some(index.to_string());
        options.push(
            SelectOption::new(if is_deleting {
                // `Press ${deleteHint()} again to confirm`
                // (dialog-stash.tsx:35,45).
                format!(
                    "Press {} again to confirm",
                    crate::ui::dialogs::key_hint(app, "stash_delete").unwrap_or_default()
                )
            } else {
                preview
            })
            .with_value(index.to_string()),
        );
    }
    options
}

/// `stash_delete` — the double-press delete (`dialog-stash.tsx:75-91`).
pub fn stash_delete(app: &mut App, value: &str) {
    let Ok(index) = value.parse::<usize>() else {
        return;
    };
    let index = stash_index(app, index);
    let pending = app
        .ui
        .dialogs
        .top()
        .and_then(|frame| frame.pending_delete.clone());
    if pending.as_deref() == Some(value) {
        if index < app.ui.prompt.stash.list().len() {
            app.ui.prompt.stash.remove(index);
        }
        if let Some(frame) = app.ui.dialogs.top_mut() {
            frame.pending_delete = None;
        }
        return;
    }
    if let Some(frame) = app.ui.dialogs.top_mut() {
        frame.pending_delete = Some(value.to_string());
    }
}

/// The stash list is displayed most-recent-first — the value index maps
/// back onto the underlying list (`entries.toReversed()`).
fn stash_index(_app: &App, value: usize) -> usize {
    value
}

/// `onSelect` — remove the entry and restore it (`dialog-stash.tsx:61-70`).
pub fn stash_pop(app: &mut App, value: &str) {
    let Ok(index) = value.parse::<usize>() else {
        return;
    };
    if index >= app.ui.prompt.stash.list().len() {
        return;
    }
    let entry = app.ui.prompt.stash.list()[index].clone();
    let entry = crate::state::prompt::PromptEntry {
        input: entry.entry.input.clone(),
        mode: None,
        parts: entry.entry.parts.clone(),
    };
    app.ui.prompt.stash.remove(index);
    app.ui.prompt.textarea.set_text(&entry.input);
    app.ui.prompt.parts = entry.parts;
    app.ui.prompt.restore_extmarks_from_parts();
    app.ui.prompt.textarea.buffer_end(false);
    crate::ui::dialogs::clear(app);
}

/// `DialogMoveSession.options` (`dialog-move-session.tsx:93-186`) — the
/// fetched project directories, current first.
pub fn move_options(app: &App, _frame: &DialogFrame) -> Vec<SelectOption> {
    let Some(directories) = app.ui.move_directories.as_ref() else {
        return vec![SelectOption::new("Loading project directories…")];
    };
    if directories.is_empty() {
        return vec![SelectOption::new("No project directories found")];
    }
    directories
        .iter()
        .filter_map(|directory| {
            let location = directory.get("directory").and_then(Value::as_str)?;
            Some(
                SelectOption::new(crate::ui::locale::abbreviate_home(location, ""))
                    .with_value(location.to_string())
                    .with_category("Other"),
            )
        })
        .collect()
}

/// `DialogSubagent` (`dialog-subagent.tsx`).
pub fn subagent_options() -> Vec<SelectOption> {
    vec![SelectOption::new("Open")
        .with_value("subagent.view")
        .with_description("the subagent's session")]
}
