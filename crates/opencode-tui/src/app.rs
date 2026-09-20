//! `app.tsx` — the app shell: startup args, the global event handlers,
//! the terminal title, the `--continue`/`--fork` derivations, and the
//! exit epilogue. Everything here is a pure function over [`App`],
//! invoked from [`crate::state::update`] and the runtime loop.

use opencode_schema::event_manifest::Event;
use opencode_schema::session_v1::V1SessionInfo;
use serde_json::Value;

use crate::state::kv::keys;
use crate::state::route::Route;
use crate::state::sync::SyncStatus;
use crate::state::{App, Effect, PendingDialog, Toast, ToastVariant};
use crate::transport::events::BusEvent;

/// `Flag.OPENCODE_DISABLE_TERMINAL_TITLE` (`core/flag/flag.ts`): read at
/// startup, like the TS module-load read.
pub fn terminal_title_disabled_from_env() -> bool {
    std::env::var("OPENCODE_DISABLE_TERMINAL_TITLE").is_ok_and(|v| !v.is_empty() && v != "0")
}

/// `Flag.OPENCODE_FAST_BOOT`.
pub fn fast_boot_from_env() -> bool {
    std::env::var("OPENCODE_FAST_BOOT").is_ok_and(|v| !v.is_empty() && v != "0")
}

/// The `onMount` batch (`app.tsx:481-501`): `--agent`, `--model`
/// (invalid → warning toast), and `--session` navigation.
///
/// TS runs this at App mount — before the async bootstrap has populated
/// the sync store — so validation happens against an empty store, same
/// as here.
pub fn apply_args(app: &mut App) {
    let args = app.state.args.clone();
    if let Some(agent) = &args.agent {
        let sync = std::mem::take(&mut app.state.sync);
        if let Some(toast) = app.state.local.agent_set(agent, &sync) {
            app.show_toast(toast);
        }
        app.state.sync = sync;
    }
    if let Some(model) = &args.model {
        let parsed = crate::state::local::parse_model(model);
        if parsed.provider_id.is_empty() || parsed.model_id.is_empty() {
            app.show_toast(Toast {
                title: None,
                variant: ToastVariant::Warning,
                message: format!("Invalid model format: {model}"),
                duration_ms: 3000,
            });
        } else {
            let sync = std::mem::take(&mut app.state.sync);
            if let Some(toast) = app.state.local.model_set(&sync, parsed, true) {
                app.show_toast(toast);
            }
            app.state.sync = sync;
        }
    }
    if let Some(session_id) = &args.session_id {
        if !args.fork {
            app.state.route.navigate(Route::Session {
                session_id: session_id.clone(),
                prompt: None,
            });
        }
    }
}

/// The `createEffect` derivations that run after every update:
/// `--continue` (`app.tsx:503-524`), `--session --fork`
/// (`app.tsx:526-540`) and the empty-provider dialog transition
/// (`app.tsx:542-551`).
pub fn post_update(app: &mut App) -> Vec<Effect> {
    let mut effects = Vec::new();

    if !app.ui.continued
        && app.state.sync.status != Some(SyncStatus::Loading)
        && app.state.args.continue_
    {
        if let Some(session_id) = most_recent_parentless(app) {
            app.ui.continued = true;
            if app.state.args.fork {
                effects.push(Effect::SessionFork {
                    session_id,
                    navigate: true,
                });
            } else {
                app.state.route.navigate(Route::Session {
                    session_id,
                    prompt: None,
                });
            }
        }
    }

    if !app.ui.forked
        && app.state.sync.status == Some(SyncStatus::Complete)
        && app.state.args.session_id.is_some()
        && app.state.args.fork
    {
        app.ui.forked = true;
        if let Some(session_id) = app.state.args.session_id.clone() {
            effects.push(Effect::SessionFork {
                session_id,
                navigate: true,
            });
        }
    }

    let provider_empty =
        app.state.sync.status == Some(SyncStatus::Complete) && app.state.sync.provider.is_empty();
    if provider_empty && !app.ui.provider_empty {
        app.ui.dialog = Some(PendingDialog::ProviderConnect);
    }
    app.ui.provider_empty = provider_empty;

    effects
}

/// `toSorted((a, b) => b.time.updated - a.time.updated).find((x) =>
/// x.parentID === undefined)?.id` (`app.tsx:507-509`).
fn most_recent_parentless(app: &App) -> Option<String> {
    let mut sessions: Vec<&V1SessionInfo> = app
        .state
        .sync
        .session
        .iter()
        .filter(|s| s.parent_id.is_none())
        .collect();
    sessions.sort_by_key(|s| std::cmp::Reverse(s.time.updated));
    sessions.first().map(|s| s.id.clone())
}

/// The global event handlers (`app.tsx:987-1079`).
pub fn on_bus_event(app: &mut App, bus_event: BusEvent) -> Vec<Effect> {
    let BusEvent { event, metadata } = bus_event;
    let workspace_matches = metadata.workspace == app.state.project.workspace.current;
    match event {
        Event::TuiCommandExecute(evt) if workspace_matches => {
            // TODO(M8.4): keymap dispatchCommand(evt.command)
            let _ = evt;
        }
        Event::TuiToastShow(evt) if workspace_matches => {
            app.show_toast(Toast {
                title: evt.title.clone(),
                variant: match evt.variant {
                    opencode_schema::tui_event::TuiToastVariant::Info => ToastVariant::Info,
                    opencode_schema::tui_event::TuiToastVariant::Success => ToastVariant::Success,
                    opencode_schema::tui_event::TuiToastVariant::Warning => ToastVariant::Warning,
                    opencode_schema::tui_event::TuiToastVariant::Error => ToastVariant::Error,
                },
                message: evt.message.clone(),
                duration_ms: evt.duration.unwrap_or(5000),
            });
        }
        Event::TuiSessionSelect(evt) if workspace_matches => {
            app.state.route.navigate(Route::Session {
                session_id: evt.session_id.clone(),
                prompt: None,
            });
        }
        Event::SessionDeleted(evt) => {
            if let Route::Session { session_id, .. } = &app.state.route.data {
                if *session_id == evt.info.id {
                    app.state.route.navigate(Route::Home { prompt: None });
                    app.show_toast(Toast {
                        title: None,
                        variant: ToastVariant::Info,
                        message: "The current session was deleted".to_string(),
                        duration_ms: 5000,
                    });
                }
            }
        }
        Event::SessionError(evt) => {
            if !workspace_matches {
                return Vec::new();
            }
            if matches!(
                evt.error,
                opencode_schema::session_v1::AssistantError::Aborted { .. }
            ) {
                return Vec::new();
            }
            app.show_toast(Toast {
                title: None,
                variant: ToastVariant::Error,
                message: assistant_error_message(&evt.error),
                duration_ms: 5000,
            });
        }
        Event::InstallationUpdateAvailable(evt) => {
            let skipped = app.state.kv.get(keys::SKIPPED_VERSION, Value::Null);
            let skip = match skipped.as_str() {
                Some(skipped) => !is_version_greater(&evt.version, skipped),
                None => false,
            };
            if !skip {
                app.ui.dialog = Some(PendingDialog::UpdateAvailable {
                    version: evt.version.clone(),
                });
            }
        }
        _ => {}
    }
    Vec::new()
}

/// `errorMessage` (`app.tsx:154-167`): `error.data.message`, falling back
/// to JS `String(error)`.
fn assistant_error_message(error: &opencode_schema::session_v1::AssistantError) -> String {
    use opencode_schema::session_v1::AssistantError;
    match error {
        AssistantError::Auth { message, .. } | AssistantError::Unknown { message, .. } => {
            message.clone()
        }
        _ => "[object Object]".to_string(),
    }
}

/// `isVersionGreater` (`app.tsx:169-184`).
pub fn is_version_greater(left: &str, right: &str) -> bool {
    fn parse(value: &str) -> (Vec<i64>, Option<String>) {
        let value = value.strip_prefix('v').unwrap_or(value);
        let (core, prerelease) = match value.split_once('-') {
            Some((core, prerelease)) => (core, Some(prerelease.to_string())),
            None => (value, None),
        };
        (
            core.split('.')
                .map(|part| part.parse::<i64>().unwrap_or(0))
                .collect(),
            prerelease,
        )
    }

    let (a_core, a_pre) = parse(left);
    let (b_core, b_pre) = parse(right);
    for index in 0..a_core.len().max(b_core.len()) {
        let difference =
            a_core.get(index).copied().unwrap_or(0) - b_core.get(index).copied().unwrap_or(0);
        if difference != 0 {
            return difference > 0;
        }
    }
    if a_pre == b_pre {
        return false;
    }
    match (&a_pre, &b_pre) {
        (None, _) => true,
        (_, None) => false,
        (Some(a), Some(b)) => compare_prerelease(a, b) == std::cmp::Ordering::Greater,
    }
}

/// `localeCompare(undefined, { numeric: true })`: digit runs compare
/// numerically, everything else byte-wise.
fn compare_prerelease(a: &str, b: &str) -> std::cmp::Ordering {
    fn parts(value: &str) -> Vec<&str> {
        let mut parts = Vec::new();
        let mut start = 0;
        let mut numeric_start = value.starts_with(|c: char| c.is_ascii_digit());
        for (index, char) in value.char_indices().skip(1) {
            if char.is_ascii_digit() != numeric_start {
                parts.push(&value[start..index]);
                start = index;
                numeric_start = char.is_ascii_digit();
            }
        }
        parts.push(&value[start..]);
        parts
    }
    let (a_parts, b_parts) = (parts(a), parts(b));
    for (left, right) in a_parts.iter().zip(b_parts.iter()) {
        match (left.parse::<i64>(), right.parse::<i64>()) {
            (Ok(left), Ok(right)) if left != right => return left.cmp(&right),
            (Ok(_), Ok(_)) => continue,
            _ if left != right => return left.cmp(right),
            _ => continue,
        }
    }
    a_parts.len().cmp(&b_parts.len()).then(a.cmp(b))
}

/// The terminal title effect (`app.tsx:455-478`): `OpenCode` on home and
/// default-titled sessions, `OC | <title truncated to 40>` otherwise.
/// `None` means "do not set" (disabled or env-suppressed).
pub fn terminal_title(app: &App, env_disabled: bool) -> Option<String> {
    if env_disabled || !app.state.kv.get_bool(keys::TERMINAL_TITLE_ENABLED, true) {
        return None;
    }
    match &app.state.route.data {
        Route::Home { .. } => Some("OpenCode".to_string()),
        Route::Session { session_id, .. } => {
            let session = app.state.sync.session(session_id);
            let Some(session) = session else {
                return Some("OpenCode".to_string());
            };
            if is_default_title(&session.title) {
                return Some("OpenCode".to_string());
            }
            let truncated = if session.title.chars().count() > 40 {
                let prefix: String = session.title.chars().take(37).collect();
                format!("{prefix}…")
            } else {
                session.title.clone()
            };
            Some(format!("OC | {truncated}"))
        }
        Route::Plugin { id, .. } => Some(format!("OC | {id}")),
    }
}

/// `isDefaultTitle` (`util/session.ts:1-3`):
/// `/^(New session - |Child session - )\d{4}-\d{2}-\d{2}T\d{2}:\d{2}:\d{2}\.\d{3}Z$/`.
pub fn is_default_title(title: &str) -> bool {
    for prefix in ["New session - ", "Child session - "] {
        if let Some(rest) = title.strip_prefix(prefix) {
            let bytes = rest.as_bytes();
            if bytes.len() == 24
                && bytes[4] == b'-'
                && bytes[7] == b'-'
                && bytes[10] == b'T'
                && bytes[13] == b':'
                && bytes[16] == b':'
                && bytes[19] == b'.'
                && bytes[23] == b'Z'
                && rest[..19]
                    .char_indices()
                    .all(|(i, c)| c.is_ascii_digit() || matches!(i, 4 | 7 | 10 | 13 | 16))
                && rest[20..23].chars().all(|c| c.is_ascii_digit())
            {
                return true;
            }
        }
    }
    false
}

/// `sessionEpilogue` (`util/presentation.ts:29-41`): the exit pointer the
/// session route keeps set (`routes/session/index.tsx:201-205`).
pub fn epilogue(app: &App) -> Option<String> {
    let Route::Session { session_id, .. } = &app.state.route.data else {
        return None;
    };
    let session = app.state.sync.session(session_id)?;
    let title = truncate(&session.title, 50);
    Some(session_epilogue(&title, Some(&session.id)))
}

/// `Locale.truncate` (`util/locale.ts:61-64`).
pub fn truncate(value: &str, len: usize) -> String {
    if value.chars().count() <= len {
        return value.to_string();
    }
    let prefix: String = value.chars().take(len - 1).collect();
    format!("{prefix}…")
}

const RESET: &str = "\x1b[0m";
const BOLD: &str = "\x1b[1m";
const DIM: &str = "\x1b[90m";

/// `wordmark` + `sessionEpilogue` (`util/presentation.ts:6-38`).
pub fn session_epilogue(title: &str, session_id: Option<&str>) -> String {
    let logo = crate::ui::LOGO;
    // (fg, shadow-fg, bg) escape triplets — left half vs right half.
    fn draw(line: &str, fg: &str, shadow: &str, bg: &str) -> String {
        line.chars()
            .map(|char| match char {
                '_' => format!("{bg} {RESET}"),
                '^' => format!("{fg}{bg}▀{RESET}"),
                '~' => format!("{shadow}▀{RESET}"),
                ' ' => " ".to_string(),
                _ => format!("{fg}{char}{RESET}"),
            })
            .collect()
    }

    let mut out = String::new();
    for (left, right) in logo.left.iter().zip(logo.right.iter()) {
        let left = draw(left, DIM, "\x1b[38;5;235m", "\x1b[48;5;235m");
        let right = draw(right, RESET, "\x1b[38;5;238m", "\x1b[48;5;238m");
        out.push_str("  ");
        out.push_str(&left);
        out.push(' ');
        out.push_str(&right);
        out.push('\n');
    }
    out.push('\n');
    let weak = |label: &str| format!("{DIM}{label:<10}{RESET}");
    out.push_str(&format!("  {}{BOLD}{title}{RESET}\n", weak("Session")));
    out.push_str(&format!(
        "  {}{BOLD}opencode -s {}{RESET}\n",
        weak("Continue"),
        session_id.unwrap_or_default()
    ));
    out.push('\n');
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::state::Args;

    fn app_with(args: Args) -> App {
        App::new(crate::config::TuiConfig::default(), args, None)
    }

    fn session(id: &str, title: &str, updated: i64, parent_id: Option<&str>) -> V1SessionInfo {
        V1SessionInfo {
            id: id.to_string(),
            slug: "x".into(),
            project_id: "prj".into(),
            workspace_id: None,
            directory: "/x".into(),
            path: None,
            parent_id: parent_id.map(str::to_string),
            summary: None,
            cost: None,
            tokens: None,
            share: None,
            title: title.into(),
            agent: None,
            model: None,
            version: "1".into(),
            metadata: None,
            time: opencode_schema::session_v1::V1SessionTime {
                created: updated as u64,
                updated: updated as u64,
                compacting: None,
                archived: None,
            },
            permission: None,
            revert: None,
        }
    }

    #[test]
    fn terminal_title_matrix() {
        let mut app = app_with(Args::default());
        assert_eq!(terminal_title(&app, false), Some("OpenCode".to_string()));

        app.state.route.navigate(Route::Session {
            session_id: "ses_1".into(),
            prompt: None,
        });
        app.state.sync.session = vec![session(
            "ses_1",
            "New session - 2026-09-19T10:00:00.000Z",
            1,
            None,
        )];
        assert_eq!(terminal_title(&app, false), Some("OpenCode".to_string()));

        app.state.sync.session[0].title = "Fix the build".into();
        assert_eq!(
            terminal_title(&app, false),
            Some("OC | Fix the build".to_string())
        );

        app.state.sync.session[0].title =
            "A title that is way longer than forty characters in total".into();
        assert_eq!(
            terminal_title(&app, false),
            Some("OC | A title that is way longer than forty…".to_string())
        );

        app.state.route.navigate(Route::Plugin {
            id: "diff".into(),
            data: None,
        });
        assert_eq!(terminal_title(&app, false), Some("OC | diff".to_string()));
    }

    #[test]
    fn terminal_title_disabled() {
        let mut app = app_with(Args::default());
        assert_eq!(terminal_title(&app, true), None);
        app.state
            .kv
            .set(keys::TERMINAL_TITLE_ENABLED, Value::Bool(false));
        assert_eq!(terminal_title(&app, false), None);
    }

    #[test]
    fn default_title_matches_ts_regex() {
        assert!(is_default_title("New session - 2026-09-19T10:00:00.000Z"));
        assert!(is_default_title("Child session - 2026-09-19T10:00:00.000Z"));
        assert!(!is_default_title("Custom title"));
        assert!(!is_default_title("New session - not-a-date"));
    }

    #[test]
    fn args_agent_model_session_matrix() {
        let mut app = app_with(Args {
            agent: Some("plan".into()),
            model: Some("bogus".into()),
            ..Args::default()
        });
        apply_args(&mut app);
        // The sync store is still loading at mount — same as TS.
        assert_eq!(
            app.ui.toasts.last().map(|t| t.message.clone()),
            Some("Invalid model format: bogus".to_string())
        );

        let mut app = app_with(Args {
            model: Some("anthropic/claude".into()),
            ..Args::default()
        });
        app.state.sync.provider = vec![serde_json::json!({
            "id": "anthropic",
            "models": {"claude": {"id": "claude"}},
        })];
        app.state.sync.agent = vec![serde_json::json!({
            "name": "build", "mode": "primary",
        })];
        apply_args(&mut app);
        assert!(app.ui.toasts.is_empty());
        assert_eq!(
            app.state
                .local
                .model_current(&app.state.sync, &app.state.args),
            Some(crate::state::local::ModelRef {
                provider_id: "anthropic".into(),
                model_id: "claude".into(),
            })
        );

        let mut app = app_with(Args {
            session_id: Some("ses_9".into()),
            ..Args::default()
        });
        apply_args(&mut app);
        assert_eq!(
            app.state.route.data,
            Route::Session {
                session_id: "ses_9".into(),
                prompt: None,
            }
        );

        let mut app = app_with(Args {
            session_id: Some("ses_9".into()),
            fork: true,
            ..Args::default()
        });
        apply_args(&mut app);
        assert_eq!(app.state.route.data, Route::Home { prompt: None });
    }

    #[test]
    fn continue_navigates_to_most_recent_parentless_session() {
        let mut app = app_with(Args {
            continue_: true,
            ..Args::default()
        });
        app.state.sync.status = Some(SyncStatus::Partial);
        app.state.sync.session = vec![
            session("ses_a", "A", 10, None),
            session("ses_b", "B", 30, Some("ses_a")),
            session("ses_c", "C", 20, None),
        ];
        let effects = post_update(&mut app);
        assert!(effects.is_empty());
        assert_eq!(
            app.state.route.data,
            Route::Session {
                session_id: "ses_c".into(),
                prompt: None,
            }
        );
        // idempotent — `continued` gates
        app.state.sync.session.clear();
        post_update(&mut app);
        assert_eq!(
            app.state.route.data,
            Route::Session {
                session_id: "ses_c".into(),
                prompt: None,
            }
        );
    }

    #[test]
    fn continue_with_fork_emits_a_fork_effect() {
        let mut app = app_with(Args {
            continue_: true,
            fork: true,
            ..Args::default()
        });
        app.state.sync.status = Some(SyncStatus::Partial);
        app.state.sync.session = vec![session("ses_a", "A", 10, None)];
        let effects = post_update(&mut app);
        assert_eq!(effects.len(), 1);
        assert!(matches!(
            effects[0],
            Effect::SessionFork { session_id: ref s, navigate: true } if s == "ses_a"
        ));
        // The route stays on the --continue dummy until the fork succeeds.
        assert_eq!(
            app.state.route.data,
            Route::Session {
                session_id: "dummy".into(),
                prompt: None,
            }
        );
    }

    #[test]
    fn session_fork_waits_for_complete_status() {
        let mut app = app_with(Args {
            session_id: Some("ses_9".into()),
            fork: true,
            ..Args::default()
        });
        app.state.sync.status = Some(SyncStatus::Partial);
        assert!(post_update(&mut app).is_empty());
        app.state.sync.status = Some(SyncStatus::Complete);
        let effects = post_update(&mut app);
        assert_eq!(effects.len(), 1);
        assert!(matches!(
            effects[0],
            Effect::SessionFork { session_id: ref s, navigate: true } if s == "ses_9"
        ));
    }

    #[test]
    fn empty_providers_open_the_connect_dialog_once() {
        let mut app = app_with(Args::default());
        app.state.sync.status = Some(SyncStatus::Complete);
        post_update(&mut app);
        assert_eq!(app.ui.dialog, Some(PendingDialog::ProviderConnect));

        // The dialog is only opened on the transition into empty.
        app.ui.dialog = None;
        post_update(&mut app);
        assert_eq!(app.ui.dialog, None);

        app.state.sync.provider = vec![serde_json::json!({"id": "anthropic"})];
        post_update(&mut app);
        app.state.sync.provider.clear();
        post_update(&mut app);
        assert_eq!(app.ui.dialog, Some(PendingDialog::ProviderConnect));
    }

    #[test]
    fn version_compare_follows_ts() {
        assert!(is_version_greater("1.2.0", "1.1.9"));
        assert!(is_version_greater("v2.0", "1.9.9"));
        assert!(!is_version_greater("1.0.0", "1.0.0"));
        // A prerelease is not greater than its release (app.tsx:181).
        assert!(!is_version_greater("1.0.1-rc.1", "1.0.1"));
        assert!(!is_version_greater("1.0.0", "1.0.1-rc.1"));
        assert!(is_version_greater("2.0", "2.0-rc.1"));
        assert!(is_version_greater("1.0.1-rc.2", "1.0.1-rc.1"));
    }

    #[test]
    fn epilogue_contains_title_and_session_id() {
        let mut app = app_with(Args::default());
        assert_eq!(epilogue(&app), None);
        app.state.route.navigate(Route::Session {
            session_id: "dummy".into(),
            prompt: None,
        });
        app.state.sync.session = vec![session("dummy", "Demo session", 1, None)];
        let text = epilogue(&app).expect("epilogue");
        assert!(text.contains("Demo session"));
        assert!(text.contains("opencode -s dummy"));
        assert!(text.contains("\x1b[1mDemo session\x1b[0m"));

        app.state.sync.session[0].title = "x".repeat(60);
        let text = epilogue(&app).expect("epilogue");
        assert!(text.contains(&format!("{}…", "x".repeat(49))));
        assert!(!text.contains(&"x".repeat(50)));
    }

    #[test]
    fn truncated_titles_slice_by_characters() {
        assert_eq!(truncate("hello", 10), "hello");
        assert_eq!(truncate("héllo wörld", 6), "héllo…");
    }
}
