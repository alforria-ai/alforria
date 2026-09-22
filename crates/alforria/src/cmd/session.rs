//! cli/cmd/session.ts port — the `session` command family: `list`
//! (table/json + pager) and `delete`.

use alforria_core::session::error::SessionError;
use alforria_core::ListInput;
use alforria_schema::session_v1::V1SessionInfo;
use clap::ArgMatches;

use crate::error::{CliError, TypedError};
use crate::ui::{style, Ui};

/// `pagerCmd()` (session.ts:17-42) — non-win32 is plain `less -R -S`.
pub fn pager_command() -> Vec<String> {
    vec!["less".to_string(), "-R".to_string(), "-S".to_string()]
}

/// Pager seam (session.ts:96-111): spawn with piped stdin, write the
/// output, wait for exit. `Err` falls back to printing.
pub trait Pager {
    fn page(&mut self, command: &[String], output: &str) -> Result<(), String>;
}

/// Production pager: `Process.spawn(pagerCmd(), { stdin: "pipe" })`.
pub struct ProcessPager;

impl Pager for ProcessPager {
    fn page(&mut self, command: &[String], output: &str) -> Result<(), String> {
        use std::io::Write;
        use std::process::Stdio;
        let Some((head, args)) = command.split_first() else {
            return Err("empty pager command".to_string());
        };
        let mut child = std::process::Command::new(head)
            .args(args)
            .stdin(Stdio::piped())
            .spawn()
            .map_err(|err| err.to_string())?;
        if let Some(stdin) = child.stdin.as_mut() {
            stdin
                .write_all(output.as_bytes())
                .and_then(|_| stdin.flush())
                .map_err(|err| err.to_string())?;
        }
        child.wait().map(|_| ()).map_err(|err| err.to_string())
    }
}

/// `Locale.truncate` (tui/util/locale.ts:61-63).
pub fn truncate(title: &str, len: usize) -> String {
    let chars: Vec<char> = title.chars().collect();
    if chars.len() <= len {
        return title.to_string();
    }
    chars[..len.saturating_sub(1)].iter().collect::<String>() + "…"
}

/// `Locale.time` — `toLocaleTimeString(…, { timeStyle: "short" })`, the
/// Node en-US default shape (`1:23 PM`).
fn time_string(input_millis: i64) -> String {
    use chrono::TimeZone;
    let time = chrono::Local
        .timestamp_millis_opt(input_millis)
        .single()
        .unwrap_or_else(chrono::Local::now);
    let formatted = time.format("%I:%M %p").to_string();
    formatted.trim_start_matches('0').to_string()
}

/// `Locale.todayTimeOrDateTime` (tui/util/locale.ts:17-28) — the local
/// time for today, `time · date` otherwise.
pub fn today_time_or_date_time(input_millis: i64, now_millis: i64) -> String {
    use chrono::TimeZone;
    let date = chrono::Local
        .timestamp_millis_opt(input_millis)
        .single()
        .expect("valid timestamp");
    let now = chrono::Local
        .timestamp_millis_opt(now_millis)
        .single()
        .expect("valid timestamp");
    if date.format("%Y-%m-%d").to_string() == now.format("%Y-%m-%d").to_string() {
        return time_string(input_millis);
    }
    let month = date.format("%-m");
    let day = date.format("%-d");
    let year = date.format("%Y");
    format!("{} · {}/{}/{}", time_string(input_millis), month, day, year)
}

/// `formatSessionTable` (session.ts:118-135).
pub fn format_session_table(sessions: &[V1SessionInfo], now_millis: i64) -> String {
    let max_id_width = sessions
        .iter()
        .map(|session| session.id.chars().count())
        .max()
        .unwrap_or(0)
        .max(20);
    let max_title_width = sessions
        .iter()
        .map(|session| session.title.chars().count())
        .max()
        .unwrap_or(0)
        .max(25);

    let header = format!(
        "Session ID{}  Title{}  Updated",
        " ".repeat(max_id_width - 10),
        " ".repeat(max_title_width - 5)
    );
    let mut lines = vec![header.clone(), "─".repeat(header.chars().count())];
    for session in sessions {
        let truncated_title = truncate(&session.title, max_title_width);
        let time = today_time_or_date_time(session.time.updated as i64, now_millis);
        let pad = |value: &str, width: usize| {
            let len = value.chars().count();
            format!("{}{}", value, " ".repeat(width.saturating_sub(len)))
        };
        lines.push(format!(
            "{}  {}  {}",
            pad(&session.id, max_id_width),
            pad(&truncated_title, max_title_width),
            time
        ));
    }
    lines.join("\n")
}

/// `formatSessionJSON` (session.ts:137-147) — key order preserved
/// (`id, title, updated, created, projectId, directory`).
#[derive(serde::Serialize)]
struct SessionJson<'a> {
    id: &'a str,
    title: &'a str,
    updated: u64,
    created: u64,
    #[serde(rename = "projectId")]
    project_id: &'a str,
    directory: &'a str,
}

pub fn format_session_json(sessions: &[V1SessionInfo]) -> String {
    let data: Vec<SessionJson<'_>> = sessions
        .iter()
        .map(|session| SessionJson {
            id: &session.id,
            title: &session.title,
            updated: session.time.updated,
            created: session.time.created,
            project_id: &session.project_id,
            directory: &session.directory,
        })
        .collect();
    serde_json::to_string_pretty(&data).unwrap_or_default()
}

/// `session list` (session.ts:70-116). Empty list → no output.
pub fn list(
    ui: &mut Ui,
    sessions: &[V1SessionInfo],
    max_count: Option<i64>,
    format: &str,
    now_millis: i64,
    pager: &mut dyn Pager,
) -> Result<(), TypedError> {
    if sessions.is_empty() {
        return Ok(());
    }
    let output = if format == "json" {
        format_session_json(sessions)
    } else {
        format_session_table(sessions, now_millis)
    };
    let should_paginate = ui.is_tty() && max_count.is_none() && format == "table";
    if should_paginate {
        if pager.page(&pager_command(), &output).is_err() {
            ui.write_stdout(&format!("{output}\n"));
        }
    } else {
        ui.write_stdout(&format!("{output}\n"));
    }
    Ok(())
}

/// `session delete` (session.ts:51-68).
pub fn remove(
    ui: &mut Ui,
    sessions: &alforria_core::SessionStore,
    session_id: &str,
) -> Result<(), TypedError> {
    sessions.remove(session_id).map_err(|err| match err {
        SessionError::NotFound(_) => {
            TypedError::Cli(CliError::new(format!("Session not found: {session_id}")))
        }
        other => TypedError::Cli(CliError::new(other.to_string())),
    })?;
    ui.println(&format!(
        "{}Session {session_id} deleted{}",
        style::TEXT_SUCCESS_BOLD,
        style::TEXT_NORMAL
    ));
    Ok(())
}

pub fn run(matches: &ArgMatches, ui: &mut Ui) -> Result<(), TypedError> {
    match matches.subcommand_name() {
        Some("list") => {
            let instance = crate::instance::boot(None)?;
            let list_matches = matches.subcommand_matches("list").expect("list");
            let max_count = list_matches.get_one::<i64>("max-count").copied();
            let format = list_matches
                .get_one::<String>("format")
                .cloned()
                .unwrap_or_else(|| "table".to_string());
            let context = instance
                .services
                .instance_context(&instance.directory, None)
                .map_err(|err| TypedError::Cli(CliError::new(err.to_string())))?;
            let sessions = instance
                .services
                .sessions
                .list(
                    &context,
                    &ListInput {
                        roots: true,
                        limit: max_count,
                        ..ListInput::default()
                    },
                )
                .map_err(|err| TypedError::Cli(CliError::new(err.to_string())))?;
            let now = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|duration| duration.as_millis() as i64)
                .unwrap_or_default();
            let mut pager = ProcessPager;
            list(ui, &sessions, max_count, &format, now, &mut pager)
        }
        Some("delete") => {
            let instance = crate::instance::boot(None)?;
            let delete_matches = matches.subcommand_matches("delete").expect("delete");
            let session_id = delete_matches
                .get_one::<String>("sessionID")
                .expect("sessionID");
            remove(ui, &instance.services.sessions, session_id)
        }
        _ => unreachable!("session requires a subcommand"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use alforria_schema::session_v1::V1SessionTime;
    use serde_json::{json, Value};

    fn session(id: &str, title: &str, updated: u64) -> V1SessionInfo {
        V1SessionInfo {
            id: id.to_string(),
            slug: String::new(),
            project_id: "prj".to_string(),
            workspace_id: None,
            directory: "/repo".to_string(),
            path: None,
            parent_id: None,
            summary: None,
            cost: None,
            tokens: None,
            share: None,
            title: title.to_string(),
            agent: None,
            model: None,
            version: "1".to_string(),
            metadata: None,
            time: V1SessionTime {
                created: updated - 1,
                updated,
                compacting: None,
                archived: None,
            },
            permission: None,
            revert: None,
        }
    }

    #[test]
    fn table_formats_header_rule_and_rows() {
        let sessions = vec![
            session("ses_1234567890", "Fix the bug", 1_000_000_000_000),
            session(
                "ses_98765432109876543210",
                "A longer title here",
                1_000_000_000_000,
            ),
        ];
        let table = format_session_table(&sessions, 1_000_000_000_000);
        let lines: Vec<&str> = table.split('\n').collect();
        assert_eq!(
            lines[0],
            format!(
                "Session ID{}  Title{}  Updated",
                " ".repeat(24 - 10),
                " ".repeat(25 - 5)
            )
        );
        assert_eq!(lines[1], "─".repeat(lines[0].chars().count()));
        assert!(
            lines[2].starts_with("ses_1234567890              ")
                || lines[2].starts_with("ses_1234567890"),
            "{}",
            lines[2]
        );
        assert!(lines[2].contains("Fix the bug"), "{}", lines[2]);
    }

    #[test]
    fn table_pads_ids_to_max_width() {
        let sessions = vec![session(
            "ses_aaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
            "title",
            1_000_000_000_000,
        )];
        let table = format_session_table(&sessions, 1_000_000_000_000);
        let row = table.split('\n').nth(2).expect("row");
        assert!(
            row.starts_with("ses_aaaaaaaaaaaaaaaaaaaaaaaaaaaaaa  title"),
            "{row}"
        );
    }

    #[test]
    fn json_includes_expected_fields() {
        let sessions = vec![session("ses_1", "Hello", 1_000_000_000_000)];
        let output = format_session_json(&sessions);
        let parsed: Vec<Value> = serde_json::from_str(&output).unwrap();
        assert_eq!(
            parsed,
            vec![json!({
                "id": "ses_1",
                "title": "Hello",
                "updated": 1_000_000_000_000_u64,
                "created": 999_999_999_999_u64,
                "projectId": "prj",
                "directory": "/repo",
            })]
        );
        assert!(output.contains("\"projectId\""), "{output}");
    }

    #[test]
    fn truncate_appends_ellipsis() {
        assert_eq!(truncate("hello", 5), "hello");
        assert_eq!(truncate("hello!", 5), "hell…");
        assert_eq!(truncate("hi", 5), "hi");
    }

    #[test]
    fn today_uses_time_and_other_days_add_date() {
        let now = 1_700_000_000_000_i64;
        assert!(
            today_time_or_date_time(now, now).ends_with('M'),
            "{}",
            today_time_or_date_time(now, now)
        );
        let later = now + 24 * 60 * 60 * 1000;
        let formatted = today_time_or_date_time(now, later);
        assert!(formatted.contains(" · "), "{formatted}");
    }

    struct RecordingPager {
        outputs: Vec<String>,
    }

    impl Pager for RecordingPager {
        fn page(&mut self, _command: &[String], output: &str) -> Result<(), String> {
            self.outputs.push(output.to_string());
            Ok(())
        }
    }

    #[test]
    fn empty_session_list_prints_nothing() {
        let (mut ui, captured) = Ui::capture(false);
        let mut pager = RecordingPager {
            outputs: Vec::new(),
        };
        list(&mut ui, &[], None, "table", 0, &mut pager).unwrap();
        assert_eq!(captured.stdout(), "");
        assert!(pager.outputs.is_empty());
    }

    #[test]
    fn non_tty_prints_without_pager() {
        let sessions = vec![session("ses_1", "Hello", 1_000_000_000_000)];
        let (mut ui, captured) = Ui::capture(false);
        let mut pager = RecordingPager {
            outputs: Vec::new(),
        };
        list(
            &mut ui,
            &sessions,
            None,
            "table",
            1_000_000_000_000,
            &mut pager,
        )
        .unwrap();
        assert!(
            captured.stdout().starts_with("Session ID"),
            "{}",
            captured.stdout()
        );
        assert!(pager.outputs.is_empty());
    }

    #[test]
    fn tty_without_max_count_pipes_into_pager() {
        let sessions = vec![session("ses_1", "Hello", 1_000_000_000_000)];
        let (mut ui, captured) = Ui::capture(true);
        let mut pager = RecordingPager {
            outputs: Vec::new(),
        };
        list(
            &mut ui,
            &sessions,
            None,
            "table",
            1_000_000_000_000,
            &mut pager,
        )
        .unwrap();
        assert_eq!(captured.stdout(), "");
        assert_eq!(pager.outputs.len(), 1);
        assert!(pager.outputs[0].starts_with("Session ID"));
    }

    #[test]
    fn max_count_disables_pager_even_on_tty() {
        let sessions = vec![session("ses_1", "Hello", 1_000_000_000_000)];
        let (mut ui, captured) = Ui::capture(true);
        let mut pager = RecordingPager {
            outputs: Vec::new(),
        };
        list(
            &mut ui,
            &sessions,
            Some(10),
            "table",
            1_000_000_000_000,
            &mut pager,
        )
        .unwrap();
        assert!(captured.stdout().starts_with("Session ID"));
        assert!(pager.outputs.is_empty());
    }

    #[test]
    fn json_format_disables_pager() {
        let sessions = vec![session("ses_1", "Hello", 1_000_000_000_000)];
        let (mut ui, captured) = Ui::capture(true);
        let mut pager = RecordingPager {
            outputs: Vec::new(),
        };
        list(
            &mut ui,
            &sessions,
            None,
            "json",
            1_000_000_000_000,
            &mut pager,
        )
        .unwrap();
        assert!(captured.stdout().starts_with('['), "{}", captured.stdout());
        assert!(pager.outputs.is_empty());
    }

    #[test]
    fn remove_reports_missing_session_and_deletes_existing() {
        use std::sync::Arc;

        use alforria_core::storage::Storage;

        struct NoJobs;
        impl alforria_core::BackgroundJobs for NoJobs {
            fn list(
                &self,
            ) -> Result<Vec<alforria_core::BackgroundJobInfo>, alforria_core::CoreError>
            {
                Ok(Vec::new())
            }
            fn cancel(&self, _id: &str) -> Result<(), alforria_core::CoreError> {
                Ok(())
            }
        }
        let storage = Arc::new(Storage::open_in_memory().unwrap());
        // Seed the `global` project row the session references.
        storage.with_connection(|conn| {
            let _ = conn.execute(
                "INSERT INTO project (id, worktree, sandboxes, time_created, time_updated) VALUES ('global', '/repo', '[]', 1, 1)",
                [],
            );
        });
        let events = Arc::new(alforria_core::EventBus::new_shared(storage.clone(), None));
        alforria_core::register_projectors(&events);
        let sessions = alforria_core::SessionStore::new(
            events,
            storage,
            Arc::new(NoJobs),
            Arc::new(alforria_core::catalog::SystemClock),
        );
        // Not-found path.
        let (mut ui, _captured) = Ui::capture(false);
        let err = remove(&mut ui, &sessions, "ses_missing").unwrap_err();
        assert_eq!(
            crate::error::format_error(&err).unwrap(),
            "Session not found: ses_missing"
        );
        assert_eq!(err.exit_code(), 1);

        // Successful delete prints the green success line.
        let context = alforria_core::SessionContext {
            project_id: "global".to_string(),
            directory: std::path::PathBuf::from("/repo"),
            worktree: std::path::PathBuf::from("/repo"),
            workspace_id: None,
        };
        let created = sessions
            .create(
                &context,
                &alforria_core::CreateInput {
                    id: Some("ses_delete_me".to_string()),
                    directory: Some("/repo".to_string()),
                    ..alforria_core::CreateInput::default()
                },
            )
            .unwrap();
        let (mut ui, captured) = Ui::capture(false);
        remove(&mut ui, &sessions, &created.id).unwrap();
        assert_eq!(
            captured.stderr(),
            format!(
                "{}Session {} deleted{}\n",
                style::TEXT_SUCCESS_BOLD,
                created.id,
                style::TEXT_NORMAL
            )
        );
        assert!(sessions
            .list(
                &context,
                &ListInput {
                    roots: true,
                    ..ListInput::default()
                }
            )
            .unwrap()
            .is_empty(),);
    }

    #[test]
    fn failed_pager_falls_back_to_stdout() {
        struct FailingPager;
        impl Pager for FailingPager {
            fn page(&mut self, _command: &[String], _output: &str) -> Result<(), String> {
                Err("no less".to_string())
            }
        }
        let sessions = vec![session("ses_1", "Hello", 1_000_000_000_000)];
        let (mut ui, captured) = Ui::capture(true);
        list(
            &mut ui,
            &sessions,
            None,
            "table",
            1_000_000_000_000,
            &mut FailingPager,
        )
        .unwrap();
        assert!(captured.stdout().starts_with("Session ID"));
    }
}
