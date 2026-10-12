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
            description: None,
            agent: Some(name.to_string()),
            command: None,
            tui_command: None,
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
