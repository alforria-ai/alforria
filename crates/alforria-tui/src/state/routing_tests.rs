//! Key and paste routing (improvement plan T3): what the autocomplete,
//! the dialogs and the bottom-bar fields take before the keymap.

use alforria_schema::question_v1::{QuestionV1Info, QuestionV1Option, QuestionV1Request};
use alforria_schema::session_status::SessionStatusInfo;
use alforria_schema::session_v1::{V1SessionInfo, V1SessionTime};
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

use crate::state::prompt::AutocompleteOption;
use crate::state::route::Route;
use crate::state::{update, App, Args, Effect, Msg, PendingDialog};

fn app() -> App {
    App::new(crate::config::TuiConfig::default(), Args::default(), None)
}

fn key(app: &mut App, code: KeyCode, modifiers: KeyModifiers) -> Vec<Effect> {
    update(app, Msg::Key(KeyEvent::new(code, modifiers)))
}

fn session(id: &str) -> V1SessionInfo {
    V1SessionInfo {
        id: id.into(),
        slug: "x".into(),
        project_id: "prj".into(),
        workspace_id: None,
        directory: "/x".into(),
        path: None,
        parent_id: None,
        summary: None,
        cost: None,
        tokens: None,
        share: None,
        title: "X".into(),
        agent: None,
        model: None,
        version: "1".into(),
        metadata: None,
        time: V1SessionTime {
            created: 0,
            updated: 0,
            compacting: None,
            archived: None,
        },
        permission: None,
        revert: None,
    }
}

/// A busy session on screen with the `@` popup open over the prompt.
fn app_with_popup() -> App {
    let mut app = app();
    app.state.sync.session = vec![session("ses_a")];
    app.state
        .sync
        .session_status
        .insert("ses_a".into(), SessionStatusInfo::Busy);
    app.state.route.navigate(Route::Session {
        session_id: "ses_a".into(),
        prompt: None,
    });
    crate::app::post_update(&mut app);
    app.ui.prompt_focused = true;
    app.ui.prompt.textarea.set_text("@");
    app.ui.prompt.autocomplete.visible = Some('@');
    app.ui.prompt.autocomplete.index = 0;
    app.ui.prompt.autocomplete.options = ["build", "plan"]
        .iter()
        .map(|name| AutocompleteOption {
            display: format!("@{name}"),
            agent: Some(name.to_string()),
            ..AutocompleteOption::default()
        })
        .collect();
    app
}

#[test]
fn autocomplete_enter_selects_instead_of_submitting() {
    let mut app = app_with_popup();
    let effects = key(&mut app, KeyCode::Enter, KeyModifiers::NONE);
    assert!(
        !effects
            .iter()
            .any(|effect| matches!(effect, Effect::PromptSubmit { .. })),
        "{effects:?}"
    );
    assert!(!app.ui.prompt.autocomplete.visible());
    assert!(
        app.ui.prompt.input().contains("@build"),
        "{}",
        app.ui.prompt.input()
    );
}

#[test]
fn autocomplete_ctrl_p_moves_instead_of_opening_the_palette() {
    let mut app = app_with_popup();
    app.ui.prompt.autocomplete.selected = 1;
    key(&mut app, KeyCode::Char('p'), KeyModifiers::CONTROL);
    assert!(app.ui.dialogs.is_empty(), "the palette stayed closed");
    assert_eq!(app.ui.prompt.autocomplete.selected, 0);
}

#[test]
fn autocomplete_escape_hides_instead_of_interrupting() {
    let mut app = app_with_popup();
    let effects = key(&mut app, KeyCode::Esc, KeyModifiers::NONE);
    assert!(!app.ui.prompt.autocomplete.visible());
    assert!(
        !effects
            .iter()
            .any(|effect| matches!(effect, Effect::SessionAbort { .. })),
        "{effects:?}"
    );
    assert_eq!(app.ui.interrupt, 0, "no interrupt armed");
}

#[test]
fn space_reaches_space_bindings() {
    let app = app();
    let space = KeyEvent::new(KeyCode::Char(' '), KeyModifiers::NONE);
    assert!(app.keymap.matches("dialog.mcp.toggle", &space));
    // …and still types a space in the prompt.
    let mut app = app;
    app.ui.prompt_focused = true;
    key(&mut app, KeyCode::Char('a'), KeyModifiers::NONE);
    key(&mut app, KeyCode::Char(' '), KeyModifiers::NONE);
    key(&mut app, KeyCode::Char('b'), KeyModifiers::NONE);
    assert_eq!(app.ui.prompt.input(), "a b");
}

#[test]
fn app_global_bindings_work_inside_dialogs() {
    let mut app = app();
    let _ = crate::ui::dialogs::open(&mut app, PendingDialog::Help);
    assert!(!app.ui.dialogs.is_empty());
    // `<leader>l` — session list — from inside the help dialog.
    key(&mut app, KeyCode::Char('x'), KeyModifiers::CONTROL);
    key(&mut app, KeyCode::Char('l'), KeyModifiers::NONE);
    assert_eq!(app.ui.dialogs.top_kind(), Some(&PendingDialog::SessionList));
    // Other keys still belong to the dialog: Esc closes it.
    key(&mut app, KeyCode::Esc, KeyModifiers::NONE);
    assert!(app.ui.dialogs.is_empty());
}

#[test]
fn paste_goes_to_the_focused_question_field() {
    let mut app = app();
    app.state.sync.session = vec![session("ses_a")];
    app.state.sync.question.insert(
        "ses_a".into(),
        vec![QuestionV1Request {
            id: "que_1".into(),
            session_id: "ses_a".into(),
            questions: vec![QuestionV1Info {
                question: "What?".into(),
                header: "plan".into(),
                options: vec![QuestionV1Option {
                    label: "A".into(),
                    description: String::new(),
                }],
                multiple: Some(false),
                custom: Some(true),
            }],
            tool: None,
        }],
    );
    app.state.route.navigate(Route::Session {
        session_id: "ses_a".into(),
        prompt: None,
    });
    crate::app::post_update(&mut app);
    key(&mut app, KeyCode::Down, KeyModifiers::NONE);
    key(&mut app, KeyCode::Enter, KeyModifiers::NONE);
    assert!(app.ui.question.editing);
    update(&mut app, Msg::Paste("pasted answer".into()));
    assert_eq!(app.ui.question.input, "pasted answer");
    assert_eq!(app.ui.prompt.input(), "", "not the hidden prompt");
}

#[test]
fn shell_mode_resets_on_submit() {
    let mut app = App::new(
        crate::config::TuiConfig::default(),
        Args {
            model: Some("anthropic/claude".into()),
            ..Args::default()
        },
        None,
    );
    app.state.sync.agent = vec![serde_json::json!({"name": "build", "mode": "primary"})];
    app.state.sync.provider =
        vec![serde_json::json!({"id": "anthropic", "models": {"claude": {"id": "claude"}}})];
    app.state.sync.session = vec![session("ses_a")];
    app.state.route.navigate(Route::Session {
        session_id: "ses_a".into(),
        prompt: None,
    });
    app.ui.prompt_focused = true;
    app.ui
        .prompt
        .set_mode(crate::state::route::PromptMode::Shell);
    app.ui.prompt.textarea.set_text("ls");
    let effects = crate::state::prompt::submit(&mut app);
    assert!(
        effects
            .iter()
            .any(|effect| matches!(effect, Effect::PromptSubmit { .. })),
        "{effects:?}"
    );
    assert_eq!(app.ui.prompt.mode, crate::state::route::PromptMode::Normal);
}

fn mouse(
    app: &mut App,
    kind: crossterm::event::MouseEventKind,
    column: u16,
    row: u16,
) -> Vec<Effect> {
    update(
        app,
        Msg::Mouse(crossterm::event::MouseEvent {
            kind,
            column,
            row,
            modifiers: KeyModifiers::NONE,
        }),
    )
}

fn screen_with(lines: &[&str]) -> ratatui::buffer::Buffer {
    let mut buffer =
        ratatui::buffer::Buffer::empty(ratatui::layout::Rect::new(0, 0, 20, lines.len() as u16));
    for (row, line) in lines.iter().enumerate() {
        buffer.set_string(0, row as u16, line, ratatui::style::Style::new());
    }
    buffer
}

#[test]
fn a_drag_copies_on_release() {
    use crossterm::event::{MouseButton, MouseEventKind};
    let mut app = app();
    mouse(&mut app, MouseEventKind::Down(MouseButton::Left), 0, 0);
    // The frame drawn after the press is what gets copied.
    app.ui.selection.screen = Some(screen_with(&["copy this text", "and this"]));
    mouse(&mut app, MouseEventKind::Drag(MouseButton::Left), 2, 1);
    let effects = mouse(&mut app, MouseEventKind::Up(MouseButton::Left), 2, 1);
    let copied = effects.iter().find_map(|effect| match effect {
        Effect::ClipboardWrite { text, success, .. } => Some((text.clone(), success.clone())),
        _ => None,
    });
    let (text, toast) = copied.expect("copied on release");
    assert_eq!(text, "copy this text\nand");
    assert_eq!(toast.unwrap().message, "Copied to clipboard");
    assert!(!app.ui.selection.active(), "cleared after the copy");
}

#[test]
fn ctrl_y_copies_a_kept_selection() {
    use crossterm::event::{MouseButton, MouseEventKind};
    let mut app = app();
    app.ui.copy_on_select_disabled = true;
    mouse(&mut app, MouseEventKind::Down(MouseButton::Left), 0, 0);
    app.ui.selection.screen = Some(screen_with(&["kept"]));
    mouse(&mut app, MouseEventKind::Drag(MouseButton::Left), 3, 0);
    let effects = mouse(&mut app, MouseEventKind::Up(MouseButton::Left), 3, 0);
    assert!(effects.is_empty(), "no copy on release: {effects:?}");
    assert!(app.ui.selection.active(), "the selection stays");
    let effects = key(&mut app, KeyCode::Char('y'), KeyModifiers::CONTROL);
    assert!(
        effects.iter().any(|effect| matches!(
            effect,
            Effect::ClipboardWrite { text, .. } if text == "kept"
        )),
        "{effects:?}"
    );
    assert!(!app.ui.selection.active());
}

/// A child session on screen, its parent known.
fn app_on_child() -> App {
    let mut app = app();
    let child = V1SessionInfo {
        parent_id: Some("ses_parent".into()),
        ..session("ses_child")
    };
    app.state.sync.session = vec![child, session("ses_parent")];
    app.state.route.navigate(Route::Session {
        session_id: "ses_child".into(),
        prompt: None,
    });
    crate::app::post_update(&mut app);
    app
}

#[test]
fn a_child_session_has_no_prompt_to_type_or_paste_into() {
    let mut app = app_on_child();
    assert!(!app.ui.prompt_focused);
    key(&mut app, KeyCode::Char('x'), KeyModifiers::NONE);
    update(&mut app, Msg::Paste("pasted".into()));
    assert_eq!(app.ui.prompt.input(), "");
}

#[test]
fn up_leaves_a_child_session_without_recalling_the_prompt_history() {
    let mut app = app_on_child();
    let commands = app.keymap.dispatch(
        &crate::keymap::DispatchContext {
            route_is_session: true,
            prompt_focused: app.ui.prompt_focused,
            foreground_tasks: false,
            prompt_mounted: !crate::ui::session::prompt_replaced(&app),
        },
        &KeyEvent::new(KeyCode::Up, KeyModifiers::NONE),
        0,
    );
    assert!(commands.contains(&"session.parent"), "{commands:?}");
    assert!(
        !commands
            .iter()
            .any(|command| command.starts_with("input.") || command.starts_with("prompt.")),
        "{commands:?}"
    );
    key(&mut app, KeyCode::Up, KeyModifiers::NONE);
    assert!(matches!(
        &app.state.route.data,
        Route::Session { session_id, .. } if session_id == "ses_parent"
    ));
    assert_eq!(app.ui.prompt.input(), "");
}

#[test]
fn the_subagent_hint_names_its_key() {
    let app = app();
    assert_eq!(app.keymap.shortcut("session.child.first"), "ctrl+x down");
    assert_eq!(app.keymap.shortcut("session_child_first"), "ctrl+x down");
    assert_eq!(app.keymap.shortcut("no.such.command"), "");
}

fn task_part(metadata: serde_json::Value) -> alforria_schema::session_v1::V1Part {
    serde_json::from_value(serde_json::json!({
        "id": "prt_task",
        "sessionID": "ses_parent",
        "messageID": "msg_1",
        "type": "tool",
        "callID": "call_1",
        "tool": "task",
        "state": {
            "status": "completed",
            "input": {},
            "output": "",
            "title": "",
            "metadata": metadata,
            "time": { "start": 1, "end": 2 },
        },
    }))
    .expect("a task part")
}

#[test]
fn clicking_a_task_row_opens_its_child_session() {
    let part = task_part(serde_json::json!({ "sessionId": "ses_child" }));
    let child = crate::ui::session::parts::task_child_session(&part);
    assert_eq!(child.as_deref(), Some("ses_child"));
    assert_eq!(
        crate::ui::session::parts::task_child_session(&task_part(serde_json::json!({}))),
        None
    );

    let mut app = app();
    app.state.sync.session = vec![session("ses_parent")];
    app.state.route.navigate(Route::Session {
        session_id: "ses_parent".into(),
        prompt: None,
    });
    app.ui.session_scroll.sticky = false;
    app.ui.session_scroll.clicks = vec![crate::state::ClickTarget {
        id: "prt_task".into(),
        start: 0,
        end: 3,
        child_session: child,
        workflow_run: None,
    }];
    update(
        &mut app,
        Msg::Mouse(crossterm::event::MouseEvent {
            kind: crossterm::event::MouseEventKind::Up(crossterm::event::MouseButton::Left),
            column: 0,
            row: 1,
            modifiers: KeyModifiers::NONE,
        }),
    );
    assert!(matches!(
        &app.state.route.data,
        Route::Session { session_id, .. } if session_id == "ses_child"
    ));
}
