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
