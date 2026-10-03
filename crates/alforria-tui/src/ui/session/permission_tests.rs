//! Permission prompt tests (`scratchpad/specs/M8.md` M8.7).

use alforria_schema::permission_v1::{PermissionV1Reply, PermissionV1Request};
use alforria_schema::session_v1::V1SessionInfo;
use crossterm::event::{KeyCode, KeyModifiers};

use super::permission::{self, visible, PermissionStage};
use crate::state::route::Route;
use crate::state::update;
use crate::state::App;

fn session_info(id: &str, parent: Option<&str>) -> V1SessionInfo {
    let time = alforria_schema::session_v1::V1SessionTime {
        created: 0,
        updated: 0,
        compacting: None,
        archived: None,
    };
    V1SessionInfo {
        id: id.into(),
        slug: "x".into(),
        project_id: "prj".into(),
        workspace_id: None,
        directory: "/x".into(),
        path: None,
        parent_id: parent.map(str::to_string),
        summary: None,
        cost: None,
        tokens: None,
        share: None,
        title: "X".into(),
        agent: None,
        model: None,
        version: "1".into(),
        metadata: None,
        time,
        permission: None,
        revert: None,
    }
}

fn request(id: &str, session_id: &str, permission: &str) -> PermissionV1Request {
    PermissionV1Request {
        id: id.into(),
        session_id: session_id.into(),
        permission: permission.into(),
        patterns: Vec::new(),
        metadata: Default::default(),
        always: vec!["*".into()],
        tool: None,
    }
}

fn app_with_request(session_id: &str, request: PermissionV1Request) -> App {
    let mut app = App::new(
        crate::config::TuiConfig::default(),
        crate::state::Args::default(),
        None,
    );
    app.state.sync.session = vec![
        session_info("ses_parent", None),
        session_info("ses_child", Some("ses_parent")),
    ];
    app.state
        .sync
        .permission
        .insert(session_id.to_string(), vec![request]);
    app.state.route.navigate(Route::Session {
        session_id: "ses_parent".into(),
        prompt: None,
    });
    // The runtime reconciler runs after every update — the observe
    // reset of the permission state machine depends on it.
    crate::app::post_update(&mut app);
    app
}

fn press(app: &mut App, code: KeyCode) {
    update(
        app,
        crate::state::Msg::Key(crossterm::event::KeyEvent::new(code, KeyModifiers::NONE)),
    );
}

fn press_ctrl(app: &mut App, char: char) {
    update(
        app,
        crate::state::Msg::Key(crossterm::event::KeyEvent::new(
            KeyCode::Char(char),
            KeyModifiers::CONTROL,
        )),
    );
}

fn enter(app: &mut App) -> Vec<crate::state::Effect> {
    update(
        app,
        crate::state::Msg::Key(crossterm::event::KeyEvent::new(
            KeyCode::Enter,
            KeyModifiers::NONE,
        )),
    )
}

#[test]
fn child_requests_surface_on_the_parent() {
    let app = app_with_request("ses_child", request("per_1", "ses_child", "bash"));
    let head = visible(&app);
    assert_eq!(head.map(|request| request.id), Some("per_1".into()));

    // No requests on the parentless route → nothing renders.
    let mut app = app_with_request("ses_parent", request("per_1", "ses_parent", "bash"));
    app.state.sync.permission.remove("ses_parent");
    assert!(visible(&app).is_none());
}

#[test]
fn once_replies_once() {
    let mut app = app_with_request("ses_parent", request("per_1", "ses_parent", "bash"));
    let effects = enter(&mut app);
    assert_eq!(
        effects,
        vec![crate::state::Effect::PermissionReply {
            request_id: "per_1".into(),
            reply: PermissionV1Reply::Once,
            message: None,
        }]
    );
}

#[test]
fn always_goes_through_the_confirm_stage() {
    let mut app = app_with_request("ses_parent", request("per_1", "ses_parent", "bash"));
    press(&mut app, KeyCode::Right);
    let effects = enter(&mut app);
    assert!(
        effects.is_empty(),
        "the always stage confirms before replying"
    );
    assert_eq!(app.ui.permission.stage, PermissionStage::Always);

    // Confirm.
    let effects = enter(&mut app);
    assert_eq!(
        effects,
        vec![crate::state::Effect::PermissionReply {
            request_id: "per_1".into(),
            reply: PermissionV1Reply::Always,
            message: None,
        }]
    );
}

#[test]
fn always_cancel_returns_to_the_main_stage() {
    let mut app = app_with_request("ses_parent", request("per_1", "ses_parent", "bash"));
    press(&mut app, KeyCode::Right);
    enter(&mut app);
    press(&mut app, KeyCode::Esc);
    assert_eq!(app.ui.permission.stage, PermissionStage::Permission);
}

#[test]
fn escape_rejects_a_parentless_request() {
    let mut app = app_with_request("ses_parent", request("per_1", "ses_parent", "bash"));
    press(&mut app, KeyCode::Esc);
    assert_eq!(app.ui.permission.stage, PermissionStage::Permission);
    let effects = enter(&mut app);
    assert_eq!(
        effects,
        vec![crate::state::Effect::PermissionReply {
            request_id: "per_1".into(),
            reply: PermissionV1Reply::Reject,
            message: None,
        }]
    );
}

#[test]
fn subagent_reject_asks_for_a_message() {
    let mut app = app_with_request("ses_child", request("per_1", "ses_child", "bash"));
    press(&mut app, KeyCode::Right);
    press(&mut app, KeyCode::Right);
    let effects = enter(&mut app);
    assert!(effects.is_empty(), "reject waits for the textarea");
    assert_eq!(app.ui.permission.stage, PermissionStage::Reject);

    // Type the guidance message, then confirm.
    for char in "use a narrower glob".chars() {
        press(&mut app, KeyCode::Char(char));
    }
    let effects = enter(&mut app);
    assert_eq!(
        effects,
        vec![crate::state::Effect::PermissionReply {
            request_id: "per_1".into(),
            reply: PermissionV1Reply::Reject,
            message: Some("use a narrower glob".into()),
        }]
    );
}

#[test]
fn subagent_reject_stage_escape_cancels() {
    let mut app = app_with_request("ses_child", request("per_1", "ses_child", "bash"));
    press(&mut app, KeyCode::Right);
    press(&mut app, KeyCode::Right);
    enter(&mut app);
    press(&mut app, KeyCode::Esc);
    assert_eq!(app.ui.permission.stage, PermissionStage::Permission);
}

#[test]
fn app_exit_binding_rejects() {
    let mut app = app_with_request("ses_parent", request("per_1", "ses_parent", "bash"));
    press_ctrl(&mut app, 'c');
    let effects = enter(&mut app);
    assert_eq!(
        effects,
        vec![crate::state::Effect::PermissionReply {
            request_id: "per_1".into(),
            reply: PermissionV1Reply::Reject,
            message: None,
        }]
    );
}

#[test]
fn ctrl_f_toggles_fullscreen() {
    let mut app = app_with_request("ses_parent", request("per_1", "ses_parent", "bash"));
    press_ctrl(&mut app, 'f');
    assert!(app.ui.permission.expanded);
    press_ctrl(&mut app, 'f');
    assert!(!app.ui.permission.expanded);
}

#[test]
fn per_permission_bodies() {
    // The bash body shows the command from the tool part input.
    let mut app = app_with_request("ses_parent", request("per_1", "ses_parent", "bash"));
    app.ui.permission.request_id = Some("per_1".into());
    let rows = permission::lines(&app);
    let text = rows
        .iter()
        .map(|row| {
            row.spans
                .iter()
                .map(std::string::ToString::to_string)
                .collect::<String>()
        })
        .collect::<Vec<_>>()
        .join("\n");
    assert!(text.contains("Shell command"), "{text}");

    // The doom_loop body.
    let mut app = app_with_request("ses_parent", request("per_1", "ses_parent", "doom_loop"));
    app.ui.permission.request_id = Some("per_1".into());
    let rows = permission::lines(&app);
    let text = rows
        .iter()
        .map(|row| {
            row.spans
                .iter()
                .map(std::string::ToString::to_string)
                .collect::<String>()
        })
        .collect::<Vec<_>>()
        .join("\n");
    assert!(text.contains("Continue after repeated failures"), "{text}");
}

#[test]
fn the_always_stage_lists_patterns() {
    let mut request = request("per_1", "ses_parent", "bash");
    request.always = vec!["src/**".into()];
    let mut app = app_with_request("ses_parent", request);
    app.ui.permission.request_id = Some("per_1".into());
    app.ui.permission.stage = PermissionStage::Always;
    app.ui.permission.selected = 1;
    let rows = permission::lines(&app);
    let text = rows
        .iter()
        .map(|row| {
            row.spans
                .iter()
                .map(std::string::ToString::to_string)
                .collect::<String>()
        })
        .collect::<Vec<_>>()
        .join("\n");
    assert!(
        text.contains("This will allow the following patterns until Alforria is restarted"),
        "{text}"
    );
    assert!(text.contains("- src/**"), "{text}");
}

#[test]
fn buttons_are_mouse_clickable() {
    // Hover moves the selection and release activates the button
    // (`permission.tsx:676-693`).
    let mut app = app_with_request("ses_parent", request("per_1", "ses_parent", "bash"));
    app.ui.terminal_width = 80;
    app.ui.terminal_height = 24;
    let backend = ratatui::backend::TestBackend::new(80, 24);
    let mut terminal = ratatui::Terminal::new(backend).unwrap();
    terminal
        .draw(|frame| crate::ui::view(&mut app, frame))
        .unwrap();

    // The rendered button geometry — the third button is Reject.
    let row = app
        .ui
        .permission
        .clicks
        .first()
        .expect("the option row records its buttons")
        .row;
    let (column, width) = app.ui.permission.clicks[0].buttons[2];
    assert!(width > 0, "the Reject button has a span");

    // `onMouseOver` — hover moves the selection.
    update(
        &mut app,
        crate::state::Msg::Mouse(crossterm::event::MouseEvent {
            kind: crossterm::event::MouseEventKind::Moved,
            column: column + 1,
            row,
            modifiers: KeyModifiers::NONE,
        }),
    );
    assert_eq!(app.ui.permission.selected, 2);

    // `onMouseUp` — release activates: reject replies immediately.
    let effects = update(
        &mut app,
        crate::state::Msg::Mouse(crossterm::event::MouseEvent {
            kind: crossterm::event::MouseEventKind::Up(crossterm::event::MouseButton::Left),
            column: column + 1,
            row,
            modifiers: KeyModifiers::NONE,
        }),
    );
    assert_eq!(
        effects,
        vec![crate::state::Effect::PermissionReply {
            request_id: "per_1".into(),
            reply: PermissionV1Reply::Reject,
            message: None,
        }]
    );
}

// ------------------------------------------- keys beyond the prompt's own

fn key(app: &mut App, code: KeyCode, modifiers: KeyModifiers) -> Vec<crate::state::Effect> {
    update(
        app,
        crate::state::Msg::Key(crossterm::event::KeyEvent::new(code, modifiers)),
    )
}

fn replies(effects: &[crate::state::Effect]) -> Vec<&crate::state::Effect> {
    effects
        .iter()
        .filter(|effect| matches!(effect, crate::state::Effect::PermissionReply { .. }))
        .collect()
}

#[test]
fn global_openers_work_over_a_pending_permission() {
    // `permission.tsx` binds only its own keys; the app/session bindings
    // stay live in base mode (`app.tsx:968-971`).
    let mut app = app_with_request("ses_parent", request("per_1", "ses_parent", "bash"));
    let effects = key(&mut app, KeyCode::Char('p'), KeyModifiers::CONTROL);
    assert!(replies(&effects).is_empty());
    assert_eq!(
        app.ui.dialogs.top_kind(),
        Some(&crate::state::PendingDialog::CommandPalette)
    );
    crate::ui::dialogs::clear(&mut app);
    crate::app::post_update(&mut app);

    // A leader sequence completes in the keymap — its `l` is not the
    // prompt's "next option".
    key(&mut app, KeyCode::Char('x'), KeyModifiers::CONTROL);
    let effects = key(&mut app, KeyCode::Char('l'), KeyModifiers::NONE);
    assert!(replies(&effects).is_empty());
    assert_eq!(app.ui.permission.selected, 0);
    assert_eq!(
        app.ui.dialogs.top_kind(),
        Some(&crate::state::PendingDialog::SessionList)
    );
    assert!(visible(&app).is_some(), "still pending");
}

#[test]
fn the_prompt_keeps_its_own_keys() {
    let mut app = app_with_request("ses_parent", request("per_1", "ses_parent", "bash"));
    app.state.sync.session_status.insert(
        "ses_parent".into(),
        alforria_schema::session_status::SessionStatusInfo::Busy,
    );
    press(&mut app, KeyCode::Char('l'));
    assert_eq!(app.ui.permission.selected, 1);
    press(&mut app, KeyCode::Char('h'));
    assert_eq!(app.ui.permission.selected, 0);
    // Escape rejects — it does not interrupt the busy session.
    let effects = key(&mut app, KeyCode::Esc, KeyModifiers::NONE);
    assert_eq!(
        effects,
        vec![crate::state::Effect::PermissionReply {
            request_id: "per_1".into(),
            reply: PermissionV1Reply::Reject,
            message: None,
        }]
    );
    assert_eq!(app.ui.interrupt, 0);
}

#[test]
fn leader_q_rejects_instead_of_quitting() {
    // The prompt overrides `app.exit` (`permission.tsx:451-460`).
    let mut app = app_with_request("ses_parent", request("per_1", "ses_parent", "bash"));
    key(&mut app, KeyCode::Char('x'), KeyModifiers::CONTROL);
    let effects = key(&mut app, KeyCode::Char('q'), KeyModifiers::NONE);
    assert!(!app.ui.exit);
    assert_eq!(
        effects,
        vec![crate::state::Effect::PermissionReply {
            request_id: "per_1".into(),
            reply: PermissionV1Reply::Reject,
            message: None,
        }]
    );
}

#[test]
fn the_hidden_prompt_takes_no_input() {
    // The prompt is unmounted while the request shows
    // (`session/index.tsx:240-241`): no typing, no prompt bindings.
    let mut app = app_with_request("ses_parent", request("per_1", "ses_parent", "bash"));
    app.ui.prompt.textarea.set_text("draft");
    crate::app::post_update(&mut app);
    assert!(!app.ui.prompt_focused);
    for char in "xyz".chars() {
        key(&mut app, KeyCode::Char(char), KeyModifiers::NONE);
    }
    // `<leader>e` is the prompt's `prompt.editor`.
    key(&mut app, KeyCode::Char('x'), KeyModifiers::CONTROL);
    let effects = key(&mut app, KeyCode::Char('e'), KeyModifiers::NONE);
    assert!(
        !effects
            .iter()
            .any(|effect| matches!(effect, crate::state::Effect::OpenPromptEditor { .. })),
        "{effects:?}"
    );
    assert_eq!(app.ui.prompt.input(), "draft");
}

#[test]
fn the_reject_message_types_and_chords_fall_through() {
    let mut app = app_with_request("ses_parent", request("per_1", "ses_parent", "bash"));
    app.ui.permission.stage = PermissionStage::Reject;
    for char in "no thanks".chars() {
        press(&mut app, KeyCode::Char(char));
    }
    assert_eq!(app.ui.permission.reject_input, "no thanks");
    // A chord is not the textarea's: ctrl+p opens the palette.
    press_ctrl(&mut app, 'p');
    assert_eq!(
        app.ui.dialogs.top_kind(),
        Some(&crate::state::PendingDialog::CommandPalette)
    );
    assert_eq!(app.ui.permission.reject_input, "no thanks");
}

#[test]
fn the_prompt_keys_never_fire_from_behind_a_dialog() {
    let mut app = app_with_request("ses_parent", request("per_1", "ses_parent", "bash"));
    key(&mut app, KeyCode::Char('p'), KeyModifiers::CONTROL);
    let mut effects = Vec::new();
    for code in [
        KeyCode::Char('l'),
        KeyCode::Char('h'),
        KeyCode::Left,
        KeyCode::Right,
        KeyCode::Esc,
    ] {
        effects.extend(key(&mut app, code, KeyModifiers::NONE));
    }
    assert!(replies(&effects).is_empty(), "{effects:?}");
    assert!(app.ui.dialogs.is_empty(), "escape closed the palette only");
    assert_eq!(app.ui.permission.selected, 0);
    // Enter in a dialog is the dialog's: help closes, nothing replied.
    let _ = crate::ui::dialogs::open(&mut app, crate::state::PendingDialog::Help);
    let effects = key(&mut app, KeyCode::Enter, KeyModifiers::NONE);
    assert!(replies(&effects).is_empty(), "{effects:?}");
    assert!(app.ui.dialogs.is_empty());
    assert!(visible(&app).is_some(), "still pending");
}
