//! Question prompt tests (`scratchpad/specs/M8.md` M8.7).

use alforria_schema::question_v1::{QuestionV1Info, QuestionV1Option, QuestionV1Request};
use alforria_schema::session_v1::V1SessionInfo;
use crossterm::event::{KeyCode, KeyModifiers};

use super::question;
use crate::state::route::Route;
use crate::state::update;
use crate::state::{App, Effect};

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

fn option(label: &str) -> QuestionV1Option {
    QuestionV1Option {
        label: label.into(),
        description: String::new(),
    }
}

fn info(header: &str, options: &[&str], multiple: bool, custom: bool) -> QuestionV1Info {
    QuestionV1Info {
        question: format!("What {header}?"),
        header: header.into(),
        options: options.iter().map(|label| option(label)).collect(),
        multiple: Some(multiple),
        custom: Some(custom),
    }
}

fn request(id: &str, questions: Vec<QuestionV1Info>) -> QuestionV1Request {
    QuestionV1Request {
        id: id.into(),
        session_id: "ses_child".into(),
        questions,
        tool: None,
    }
}

fn app_with_request(questions: Vec<QuestionV1Info>) -> App {
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
        .question
        .insert("ses_child".into(), vec![request("que_1", questions)]);
    app.state.route.navigate(Route::Session {
        session_id: "ses_parent".into(),
        prompt: None,
    });
    crate::app::post_update(&mut app);
    app
}

fn press(app: &mut App, code: KeyCode) -> Vec<Effect> {
    update(
        app,
        crate::state::Msg::Key(crossterm::event::KeyEvent::new(code, KeyModifiers::NONE)),
    )
}

fn enter(app: &mut App) -> Vec<Effect> {
    update(
        app,
        crate::state::Msg::Key(crossterm::event::KeyEvent::new(
            KeyCode::Enter,
            KeyModifiers::NONE,
        )),
    )
}

fn question_replies(effects: Vec<Effect>) -> Vec<Effect> {
    effects
        .into_iter()
        .filter(|effect| {
            matches!(
                effect,
                Effect::QuestionReply { .. } | Effect::QuestionReject { .. }
            )
        })
        .collect()
}

#[test]
fn single_question_submits_immediately() {
    let mut app = app_with_request(vec![info("plan", &["Option A", "Option B"], false, false)]);
    let effects = question_replies(enter(&mut app));
    assert_eq!(
        effects,
        vec![Effect::QuestionReply {
            request_id: "que_1".into(),
            answers: vec![vec!["Option A".into()]],
        }]
    );
}

#[test]
fn single_question_selects_the_second_option() {
    let mut app = app_with_request(vec![info("plan", &["Option A", "Option B"], false, false)]);
    press(&mut app, KeyCode::Down);
    let effects = question_replies(enter(&mut app));
    assert_eq!(
        effects,
        vec![Effect::QuestionReply {
            request_id: "que_1".into(),
            answers: vec![vec!["Option B".into()]],
        }]
    );
}

#[test]
fn multi_select_goes_through_the_confirm_tab() {
    let mut app = app_with_request(vec![info("plan", &["Option A", "Option B"], true, false)]);
    // Multi: enter toggles, nothing submits.
    let effects = question_replies(enter(&mut app));
    assert!(effects.is_empty());
    // Tab to the confirm tab, then submit.
    press(&mut app, KeyCode::Tab);
    let effects = question_replies(enter(&mut app));
    assert_eq!(
        effects,
        vec![Effect::QuestionReply {
            request_id: "que_1".into(),
            answers: vec![vec!["Option A".into()]],
        }]
    );
}

#[test]
fn multiple_questions_collect_all_answers() {
    let mut app = app_with_request(vec![
        info("first", &["A1", "A2"], false, false),
        info("second", &["B1", "B2"], false, false),
    ]);
    // Question 0 submits into the answers, moves to question 1.
    let effects = question_replies(enter(&mut app));
    assert!(effects.is_empty(), "{effects:?}");
    // Answer question 1 with its second option.
    press(&mut app, KeyCode::Down);
    let effects = question_replies(enter(&mut app));
    assert!(effects.is_empty(), "{effects:?}");
    // Answering the last question advances to the confirm tab; submit
    // both.
    let effects = question_replies(enter(&mut app));
    assert_eq!(
        effects,
        vec![Effect::QuestionReply {
            request_id: "que_1".into(),
            answers: vec![vec!["A1".into()], vec!["B2".into()],],
        }]
    );
}

#[test]
fn custom_answer_picks_the_other_row() {
    let mut app = app_with_request(vec![info("plan", &["Option A"], false, true)]);
    press(&mut app, KeyCode::Down); // The "Other" row.
    let effects = question_replies(enter(&mut app));
    assert!(effects.is_empty(), "{effects:?}");
    assert!(app.ui.question.editing);
    for char in "do it live".chars() {
        press(&mut app, KeyCode::Char(char));
    }
    let effects = question_replies(enter(&mut app));
    assert_eq!(
        effects,
        vec![Effect::QuestionReply {
            request_id: "que_1".into(),
            answers: vec![vec!["do it live".into()]],
        }]
    );
}

#[test]
fn escape_rejects_the_question() {
    let mut app = app_with_request(vec![info("plan", &["Option A"], true, false)]);
    let effects = press(&mut app, KeyCode::Esc);
    assert_eq!(
        effects,
        vec![Effect::QuestionReject {
            request_id: "que_1".into(),
        }]
    );
}

#[test]
fn visible_requests_surface_on_the_parent() {
    let app = app_with_request(vec![info("plan", &["Option A"], false, false)]);
    assert_eq!(
        question::visible(&app).map(|request| request.id),
        Some("que_1".into())
    );
}

#[test]
fn multi_question_custom_answer_advances_to_the_next_tab() {
    // `pick(text, true)` (`question.tsx:187-188`): a custom answer on a
    // multi-question request advances like any other pick — it must not
    // reply early with the other questions unanswered.
    let mut app = app_with_request(vec![
        info("first", &["A1", "A2"], false, true),
        info("second", &["B1", "B2"], false, false),
    ]);
    press(&mut app, KeyCode::Down); // The "Other" row.
    press(&mut app, KeyCode::Down);
    let effects = question_replies(enter(&mut app));
    assert!(effects.is_empty(), "{effects:?}");
    assert!(app.ui.question.editing);
    for char in "custom one".chars() {
        press(&mut app, KeyCode::Char(char));
    }
    let effects = question_replies(enter(&mut app));
    assert!(effects.is_empty(), "{effects:?}");
    assert!(!app.ui.question.editing);
    assert_eq!(app.ui.question.tab, 1, "custom answer advances the tab");
    assert_eq!(app.ui.question.custom[0], "custom one");
    // Answer question 1, land on confirm, submit both.
    let effects = question_replies(enter(&mut app));
    assert!(effects.is_empty(), "{effects:?}");
    let effects = question_replies(enter(&mut app));
    assert_eq!(
        effects,
        vec![Effect::QuestionReply {
            request_id: "que_1".into(),
            answers: vec![vec!["custom one".into()], vec!["B1".into()]],
        }]
    );
}

#[test]
fn ctrl_c_clears_the_editing_input_then_leaves_editing() {
    // `prompt.clear` on the question textarea (`question.tsx:126-137`).
    let mut app = app_with_request(vec![info("plan", &["Option A"], false, true)]);
    press(&mut app, KeyCode::Down); // The "Other" row.
    enter(&mut app);
    assert!(app.ui.question.editing);
    for char in "draft".chars() {
        press(&mut app, KeyCode::Char(char));
    }
    press(&mut app, KeyCode::Char('c'));
    assert!(app.ui.question.editing, "plain c still types a char");
    let ctrl_c_first = crate::state::Msg::Key(crossterm::event::KeyEvent::new(
        KeyCode::Char('c'),
        KeyModifiers::CONTROL,
    ));
    let ctrl_c_second = crate::state::Msg::Key(crossterm::event::KeyEvent::new(
        KeyCode::Char('c'),
        KeyModifiers::CONTROL,
    ));
    update(&mut app, ctrl_c_first);
    assert!(
        app.ui.question.editing,
        "first ctrl+c only clears the draft"
    );
    assert!(app.ui.question.input.is_empty());
    update(&mut app, ctrl_c_second);
    assert!(!app.ui.question.editing, "second ctrl+c leaves editing");
}

#[test]
fn editing_renders_the_input_line_below_the_row() {
    // The editing textarea below the wildcard row (`question.tsx:427-448`)
    // is the only visible cue that editing started.
    let mut app = app_with_request(vec![info("plan", &["Option A"], false, true)]);
    press(&mut app, KeyCode::Down); // The "Other" row.
    enter(&mut app);
    assert!(app.ui.question.editing);
    let rows = question::lines(&app);
    let placeholder = rows
        .iter()
        .map(|line| line.to_string())
        .find(|line| line.starts_with("     Type your own answer"))
        .unwrap_or_default();
    assert_eq!(
        placeholder, "     Type your own answer",
        "indented placeholder row"
    );
    for char in "hello".chars() {
        press(&mut app, KeyCode::Char(char));
    }
    let rows = question::lines(&app);
    let typed = rows
        .iter()
        .map(|line| line.to_string())
        .find(|line| line.starts_with("     hello"))
        .unwrap_or_default();
    assert_eq!(typed, "     hello", "indented input row");
}

#[test]
fn options_are_mouse_clickable() {
    // Hover moves the selection and release submits the option
    // (`question.tsx:296-408`).
    let mut app = app_with_request(vec![info("plan", &["Option A", "Option B"], false, false)]);
    app.ui.terminal_width = 80;
    app.ui.terminal_height = 24;
    let backend = ratatui::backend::TestBackend::new(80, 24);
    let mut terminal = ratatui::Terminal::new(backend).unwrap();
    terminal
        .draw(|frame| crate::ui::view(&mut app, frame))
        .unwrap();

    // The second option row as rendered on screen.
    let row = app
        .ui
        .question
        .clicks
        .iter()
        .find(|(_, click)| matches!(click, question::Click::Option(1)))
        .map(|(row, _)| *row)
        .expect("the option row is clickable");

    // `onMouseOver` — hover moves the selection.
    update(
        &mut app,
        crate::state::Msg::Mouse(crossterm::event::MouseEvent {
            kind: crossterm::event::MouseEventKind::Moved,
            column: 4,
            row,
            modifiers: KeyModifiers::NONE,
        }),
    );
    assert_eq!(app.ui.question.selected, 1);

    // `onMouseUp` — release selects the option and submits it.
    let effects = question_replies(update(
        &mut app,
        crate::state::Msg::Mouse(crossterm::event::MouseEvent {
            kind: crossterm::event::MouseEventKind::Up(crossterm::event::MouseButton::Left),
            column: 4,
            row,
            modifiers: KeyModifiers::NONE,
        }),
    ));
    assert_eq!(
        effects,
        vec![Effect::QuestionReply {
            request_id: "que_1".into(),
            answers: vec![vec!["Option B".into()]],
        }]
    );
}

#[test]
fn tabs_are_mouse_clickable() {
    // Release on a tab selects it (`question.tsx:402-406`).
    let mut app = app_with_request(vec![
        info("plan", &["Option A", "Option B"], false, false),
        info("scope", &["Scope A", "Scope B"], false, false),
    ]);
    app.ui.terminal_width = 80;
    app.ui.terminal_height = 24;
    let backend = ratatui::backend::TestBackend::new(80, 24);
    let mut terminal = ratatui::Terminal::new(backend).unwrap();
    terminal
        .draw(|frame| crate::ui::view(&mut app, frame))
        .unwrap();

    let (row, click) = app
        .ui
        .question
        .clicks
        .iter()
        .find_map(|(row, click)| match click {
            question::Click::Tabs(spans) => Some((*row, spans.clone())),
            _ => None,
        })
        .expect("a tab row is clickable");
    let (column, _width, tab) = click[1];

    update(
        &mut app,
        crate::state::Msg::Mouse(crossterm::event::MouseEvent {
            kind: crossterm::event::MouseEventKind::Up(crossterm::event::MouseButton::Left),
            column: column + 1,
            row,
            modifiers: KeyModifiers::NONE,
        }),
    );
    assert_eq!(app.ui.question.tab, tab);
    assert_eq!(app.ui.question.selected, 0);
}

// ------------------------------------------- keys beyond the prompt's own

fn key(app: &mut App, code: KeyCode, modifiers: KeyModifiers) -> Vec<Effect> {
    update(
        app,
        crate::state::Msg::Key(crossterm::event::KeyEvent::new(code, modifiers)),
    )
}

#[test]
fn global_bindings_work_over_a_question_but_not_base_ones() {
    // The question mode (`question.tsx:128-131`) leaves the `app.global`
    // and `session.global` bindings live, not the base-mode `app` ones.
    let mut app = app_with_request(vec![info("plan", &["Option A", "Option B"], false, false)]);
    key(&mut app, KeyCode::Char('p'), KeyModifiers::CONTROL);
    assert!(app.ui.dialogs.is_empty(), "the palette is base-mode only");

    key(&mut app, KeyCode::Char('x'), KeyModifiers::CONTROL);
    let effects = key(&mut app, KeyCode::Char('l'), KeyModifiers::NONE);
    assert!(question_replies(effects).is_empty());
    assert_eq!(
        app.ui.dialogs.top_kind(),
        Some(&crate::state::PendingDialog::SessionList)
    );
    assert_eq!(app.ui.question.tab, 0, "the sequence's `l` is not next-tab");
}

#[test]
fn the_question_keeps_its_own_keys_and_dialog_keys_stay_out() {
    let mut app = app_with_request(vec![info("plan", &["Option A", "Option B"], false, false)]);
    // ctrl+d is `app.exit`, the question's reject — not the session
    // delete a list dialog binds it to.
    let effects = key(&mut app, KeyCode::Char('d'), KeyModifiers::CONTROL);
    assert_eq!(
        question_replies(effects),
        vec![Effect::QuestionReject {
            request_id: "que_1".into()
        }],
        "ctrl+d is `app.exit`: the question rejects"
    );

    let mut app = app_with_request(vec![info("plan", &["Option A", "Option B"], false, false)]);
    press(&mut app, KeyCode::Char('j'));
    assert_eq!(app.ui.question.selected, 1);
    // Behind a dialog none of them reach the question.
    let _ = crate::ui::dialogs::open(&mut app, crate::state::PendingDialog::SessionList);
    crate::app::post_update(&mut app);
    let mut effects = Vec::new();
    for code in [KeyCode::Char('k'), KeyCode::Char('1'), KeyCode::Esc] {
        effects.extend(key(&mut app, code, KeyModifiers::NONE));
    }
    assert!(question_replies(effects).is_empty());
    assert_eq!(app.ui.question.selected, 1);
    assert!(question::visible(&app).is_some());
}

#[test]
fn the_custom_answer_types_and_chords_fall_through() {
    let mut app = app_with_request(vec![info("plan", &["Option A"], false, true)]);
    // Select "Type your own answer" → editing.
    press(&mut app, KeyCode::Down);
    enter(&mut app);
    assert!(app.ui.question.editing);
    for char in "hjkl".chars() {
        press(&mut app, KeyCode::Char(char));
    }
    assert_eq!(app.ui.question.input, "hjkl");
    key(&mut app, KeyCode::Char('x'), KeyModifiers::CONTROL);
    key(&mut app, KeyCode::Char('l'), KeyModifiers::NONE);
    assert_eq!(
        app.ui.dialogs.top_kind(),
        Some(&crate::state::PendingDialog::SessionList)
    );
    assert_eq!(app.ui.question.input, "hjkl");
}
