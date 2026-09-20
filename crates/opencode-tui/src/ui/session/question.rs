//! `routes/session/question.tsx` — the tabbed question prompt (M8.7).
//! Questions + a confirm tab (a single non-multiple question submits
//! immediately); answers submit via `question.reply`, the confirm tab
//! also rejects (`question.reject`).

use ratatui::layout::Rect;
use ratatui::style::Style;
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Padding, Paragraph, Widget};

use crate::state::route::Route;
use crate::state::{App, Effect};
use crate::ui::theme::{selected_foreground, Theme};
use opencode_schema::question_v1::{QuestionV1Answer, QuestionV1Request};

/// The question prompt state (`question.tsx:25-31`).
#[derive(Debug, Default)]
pub struct QuestionState {
    /// The request the state belongs to — a new request resets.
    pub request_id: Option<String>,
    /// The active tab (question index, or the confirm tab).
    pub tab: usize,
    /// The selected answer row.
    pub selected: usize,
    /// Whether the "Other" textarea has focus.
    pub editing: bool,
    /// Answers per question.
    pub answers: Vec<Vec<String>>,
    /// The custom ("Other") answer per question.
    pub custom: Vec<String>,
    /// The editing textarea content.
    pub input: String,
}

/// `questions()` (`session/index.tsx:235-238`) — like permissions, the
/// parent surfaces its children's questions.
pub fn visible(app: &App) -> Option<QuestionV1Request> {
    let Route::Session { session_id, .. } = &app.state.route.data else {
        return None;
    };
    let session = app.state.sync.session(session_id)?;
    if session.parent_id.is_some() {
        return None;
    }
    let parent = &session.id;
    let mut requests: Vec<QuestionV1Request> = Vec::new();
    for child in &app.state.sync.session {
        if child.parent_id.as_deref() == Some(parent.as_str()) || child.id == *parent {
            if let Some(list) = app.state.sync.question.get(&child.id) {
                requests.extend(list.iter().cloned());
            }
        }
    }
    requests.sort_by(|a, b| a.id.cmp(&b.id));
    requests.into_iter().next()
}

fn question_len(request: &QuestionV1Request) -> usize {
    request.questions.len()
}

/// `single()` (`question.tsx:22`).
fn single(request: &QuestionV1Request) -> bool {
    question_len(request) == 1 && request.questions[0].multiple != Some(true)
}

/// The tab count — questions + confirm (no confirm for single select).
fn tabs(request: &QuestionV1Request) -> usize {
    if single(request) {
        1
    } else {
        question_len(request) + 1
    }
}

fn confirm_tab(request: &QuestionV1Request, state: &QuestionState) -> bool {
    !single(request) && state.tab == question_len(request)
}

fn options_of(
    request: &QuestionV1Request,
    tab: usize,
) -> &[opencode_schema::question_v1::QuestionV1Option] {
    request
        .questions
        .get(tab)
        .map(|question| question.options.as_slice())
        .unwrap_or(&[])
}

fn multiple(request: &QuestionV1Request, tab: usize) -> bool {
    request
        .questions
        .get(tab)
        .and_then(|question| question.multiple)
        == Some(true)
}

fn custom_allowed(request: &QuestionV1Request, tab: usize) -> bool {
    request
        .questions
        .get(tab)
        .map(|question| question.custom != Some(false))
        .unwrap_or(true)
}

/// Reset the state when the head request changes.
pub fn observe(app: &mut App) {
    let request = visible(app);
    let request_id = request.as_ref().map(|request| request.id.clone());
    if request.is_none() {
        app.ui.question.request_id = None;
        return;
    }
    if app.ui.question.request_id != request_id {
        app.ui.question.request_id = request_id;
        let len = request.map(|request| question_len(&request)).unwrap_or(0);
        app.ui.question.tab = 0;
        app.ui.question.selected = 0;
        app.ui.question.editing = false;
        app.ui.question.answers = vec![Vec::new(); len];
        app.ui.question.custom = vec![String::new(); len];
        app.ui.question.input.clear();
    }
}

fn reply_effect(request: &QuestionV1Request, answers: Vec<QuestionV1Answer>) -> Effect {
    Effect::QuestionReply {
        request_id: request.id.clone(),
        answers,
    }
}

/// Handle a key while a question is pending. `None` = no request
/// pending.
pub fn handle_key(app: &mut App, key: &crossterm::event::KeyEvent) -> Option<Vec<Effect>> {
    let request = visible(app)?;
    let kind = key.code;
    let state = &mut app.ui.question;
    let tab_count = tabs(&request);
    let editing = state.editing;

    if editing && !confirm_tab(&request, state) {
        if kind == crossterm::event::KeyCode::Esc {
            state.editing = false;
            return Some(Vec::new());
        }
        match kind {
            crossterm::event::KeyCode::Backspace => {
                state.input.pop();
            }
            crossterm::event::KeyCode::Char(char) if !char.is_control() => {
                state.input.push(char);
            }
            _ => {}
        }
        if kind != crossterm::event::KeyCode::Enter {
            return Some(Vec::new());
        }
        let tab = state.tab;
        let multi = multiple(&request, tab);
        let text = state.input.trim().to_string();
        let prev = state.custom[tab].clone();
        if text.is_empty() {
            if !prev.is_empty() {
                state.custom[tab] = String::new();
                state.answers[tab].retain(|answer| *answer != prev);
            }
            state.editing = false;
            return Some(Vec::new());
        }
        if multi {
            state.custom[tab] = text.clone();
            state.answers[tab].retain(|answer| *answer != prev);
            if !state.answers[tab].contains(&text) {
                state.answers[tab].push(text);
            }
            state.editing = false;
            return Some(Vec::new());
        }
        // Non-multi: pick the custom answer.
        state.answers[tab] = vec![text.clone()];
        state.custom[tab] = text.clone();
        state.editing = false;
        let mut answers = vec![Vec::new(); question_len(&request)];
        answers[tab] = vec![text];
        return Some(vec![reply_effect(&request, answers)]);
    }

    let tab = state.tab;
    let options = options_of(&request, tab);
    let custom = custom_allowed(&request, tab);
    let total = options.len() + usize::from(custom);

    match kind {
        crossterm::event::KeyCode::Left | crossterm::event::KeyCode::Char('h') => {
            state.tab = (state.tab + tab_count - 1) % tab_count;
            state.selected = 0;
        }
        crossterm::event::KeyCode::Right | crossterm::event::KeyCode::Char('l') => {
            state.tab = (state.tab + 1) % tab_count;
            state.selected = 0;
        }
        crossterm::event::KeyCode::Tab => {
            state.tab = (state.tab + 1) % tab_count;
            state.selected = 0;
        }
        crossterm::event::KeyCode::BackTab => {
            state.tab = (state.tab + tab_count - 1) % tab_count;
            state.selected = 0;
        }
        crossterm::event::KeyCode::Esc => {
            if !editing || !confirm_tab(&request, state) {
                return Some(vec![Effect::QuestionReject {
                    request_id: request.id.clone(),
                }]);
            }
        }
        crossterm::event::KeyCode::Enter => {
            if confirm_tab(&request, state) {
                let answers: Vec<QuestionV1Answer> = state.answers.to_vec();
                return Some(vec![reply_effect(&request, answers)]);
            }
            return Some(select_option(app, &request));
        }
        crossterm::event::KeyCode::Up | crossterm::event::KeyCode::Char('k') => {
            state.selected = (state.selected + total - 1) % total.max(1);
        }
        crossterm::event::KeyCode::Down | crossterm::event::KeyCode::Char('j') => {
            state.selected = (state.selected + 1) % total.max(1);
        }
        crossterm::event::KeyCode::Char(digit @ '1'..='9') => {
            let index = (digit as u8 - b'1') as usize;
            if index < total.min(9) {
                state.selected = index;
                return Some(select_option(app, &request));
            }
        }
        _ => {
            if app.keymap.matches("app_exit", key) {
                return Some(vec![Effect::QuestionReject {
                    request_id: request.id.clone(),
                }]);
            }
            return Some(Vec::new());
        }
    }
    Some(Vec::new())
}

/// `selectOption()` (`question.tsx:105-126`).
fn select_option(app: &mut App, request: &QuestionV1Request) -> Vec<Effect> {
    let state = &mut app.ui.question;
    let tab = state.tab;
    let options = options_of(request, tab);
    let multi = multiple(request, tab);
    let custom = custom_allowed(request, tab);
    let other = custom && state.selected == options.len();
    if other {
        let input = state.custom[tab].clone();
        if !multi {
            state.editing = true;
            state.input = input;
            return Vec::new();
        }
        let picked = !input.is_empty() && state.answers[tab].contains(&input);
        if picked {
            let index = state.answers[tab]
                .iter()
                .position(|answer| answer == &input);
            if let Some(index) = index {
                state.answers[tab].remove(index);
            }
            return Vec::new();
        }
        state.editing = true;
        state.input = input;
        return Vec::new();
    }
    let Some(option) = options.get(state.selected) else {
        return Vec::new();
    };
    let label = option.label.clone();
    if multi {
        let index = state.answers[tab]
            .iter()
            .position(|answer| answer == &label);
        if let Some(index) = index {
            state.answers[tab].remove(index);
        } else {
            state.answers[tab].push(label);
        }
        Vec::new()
    } else {
        pick(app, request, &label, true)
    }
}

/// `pick()` (`question.tsx:64-83`).
fn pick(app: &mut App, request: &QuestionV1Request, answer: &str, _custom: bool) -> Vec<Effect> {
    let state = &mut app.ui.question;
    let tab = state.tab;
    state.answers[tab] = vec![answer.to_string()];
    if single(request) {
        let mut answers = vec![Vec::new(); question_len(request)];
        answers[tab] = vec![answer.to_string()];
        return vec![reply_effect(request, answers)];
    }
    state.tab += 1;
    state.selected = 0;
    Vec::new()
}

fn app_theme(app: &App) -> Theme {
    app.ui
        .theme
        .resolve(&app.state.kv)
        .expect("builtin theme resolves")
}

/// The question box's rendered rows.
pub fn lines(app: &App) -> Vec<Line<'static>> {
    let theme = app_theme(app);
    let Some(request) = visible(app) else {
        return Vec::new();
    };
    let state = &app.ui.question;
    let mut rows: Vec<Line<'static>> = Vec::new();
    let single = single(&request);
    if !single {
        // The tab row (`question.tsx:296-352`).
        let mut spans: Vec<Span<'static>> = Vec::new();
        for (index, question) in request.questions.iter().enumerate() {
            let active = index == state.tab;
            let answered = !state
                .answers
                .get(index)
                .is_none_or(|answers| answers.is_empty());
            spans.push(Span::styled(
                format!(" {} ", question.header),
                Style::new()
                    .fg(if active {
                        selected_foreground(&theme, Some(theme.accent))
                    } else if answered {
                        theme.text
                    } else {
                        theme.text_muted
                    }
                    .to_color())
                    .bg(if active {
                        theme.accent.to_color()
                    } else {
                        theme.background_panel.to_color()
                    }),
            ));
        }
        spans.push(Span::styled(
            " Confirm ".to_string(),
            Style::new()
                .fg(if confirm_tab(&request, state) {
                    selected_foreground(&theme, Some(theme.accent))
                } else {
                    theme.text_muted
                }
                .to_color())
                .bg(if confirm_tab(&request, state) {
                    theme.accent.to_color()
                } else {
                    theme.background_panel.to_color()
                }),
        ));
        rows.push(Line::from(spans));
    }
    if !confirm_tab(&request, state) {
        let question = request
            .questions
            .get(state.tab)
            .map(|question| question.question.clone())
            .unwrap_or_default();
        let multi = multiple(&request, state.tab);
        rows.push(Line::styled(
            format!(
                "  {question}{}",
                if multi {
                    " (select all that apply)"
                } else {
                    ""
                }
            ),
            Style::new().fg(theme.text.to_color()),
        ));
        let options = options_of(&request, state.tab);
        let custom = custom_allowed(&request, state.tab);
        for (index, option) in options.iter().enumerate() {
            rows.push(option_row(app, &request, &theme, index, option));
        }
        if custom {
            rows.push(other_row(app, &request, &theme));
        }
    } else {
        // The confirm review (`question.tsx:459-479`).
        rows.push(Line::styled(
            "  Review",
            Style::new().fg(theme.text.to_color()),
        ));
        for (index, question) in request.questions.iter().enumerate() {
            let value = state
                .answers
                .get(index)
                .map(|answers| answers.join(", "))
                .unwrap_or_default();
            let color = if value.is_empty() {
                theme.error
            } else {
                theme.text
            };
            rows.push(Line::from(vec![
                Span::styled(
                    format!("  {}: ", question.header),
                    Style::new().fg(theme.text_muted.to_color()),
                ),
                Span::styled(
                    if value.is_empty() {
                        "(not answered)".to_string()
                    } else {
                        value
                    },
                    Style::new().fg(color.to_color()),
                ),
            ]));
        }
    }
    rows.push(Line::from(vec![
        Span::styled("enter ", Style::new().fg(theme.text.to_color())),
        Span::styled(
            if single || confirm_tab(&request, state) {
                "submit"
            } else {
                "confirm"
            },
            Style::new().fg(theme.text_muted.to_color()),
        ),
        Span::styled("   esc ", Style::new().fg(theme.text.to_color())),
        Span::styled("dismiss", Style::new().fg(theme.text_muted.to_color())),
    ]));
    rows
}

/// One numbered answer row (`question.tsx:363-399`).
fn option_row(
    app: &App,
    request: &QuestionV1Request,
    theme: &Theme,
    index: usize,
    option: &opencode_schema::question_v1::QuestionV1Option,
) -> Line<'static> {
    let state = &app.ui.question;
    let active = index == state.selected;
    let multi = multiple(request, state.tab);
    let picked = state
        .answers
        .get(state.tab)
        .map(|answers| answers.contains(&option.label))
        .unwrap_or(false);
    let label = if multi {
        format!("[{}] {}", if picked { "✓" } else { " " }, option.label)
    } else {
        option.label.clone()
    };
    if picked && !multi {
        // The single-select picked row keeps its text color and gets a
        // trailing check (`question.tsx:363-399`).
        return Line::from(vec![
            Span::styled(
                format!("  {}. ", index + 1),
                Style::new().fg(theme.text_muted.to_color()),
            ),
            Span::styled(label, Style::new().fg(theme.text.to_color())),
            Span::styled(" ✓", Style::new().fg(theme.success.to_color())),
        ]);
    }
    Line::from(Span::styled(
        format!("  {}. {label}", index + 1),
        Style::new().fg(if active {
            theme.secondary
        } else if picked {
            theme.success
        } else {
            theme.text
        }
        .to_color()),
    ))
}

/// The "Type your own answer" row (`question.tsx:400-454`).
fn other_row(app: &App, request: &QuestionV1Request, theme: &Theme) -> Line<'static> {
    let state = &app.ui.question;
    let options = options_of(request, state.tab);
    let multi = multiple(request, state.tab);
    let custom = custom_allowed(request, state.tab);
    let other = custom && state.selected == options.len();
    let picked = !state.custom[state.tab].is_empty()
        && state
            .answers
            .get(state.tab)
            .map(|answers| answers.contains(&state.custom[state.tab]))
            .unwrap_or(false);
    let label = if multi {
        format!("[{}] Type your own answer", if picked { "✓" } else { " " })
    } else {
        "Type your own answer".to_string()
    };
    if state.editing {
        return Line::from(vec![
            Span::styled(
                format!("  {}. ", options.len() + 1),
                Style::new().fg(theme.text_muted.to_color()),
            ),
            Span::styled(label, Style::new().fg(theme.secondary.to_color())),
            Span::styled(
                format!(" {}", state.input),
                Style::new().fg(theme.text.to_color()),
            ),
        ]);
    }
    Line::from(Span::styled(
        format!("  {}. {label}", options.len() + 1),
        Style::new().fg(if other {
            theme.secondary
        } else if picked {
            theme.success
        } else {
            theme.text
        }
        .to_color()),
    ))
}

pub fn height(app: &App) -> u16 {
    (lines(app).len() + 2) as u16
}

/// Render into the prompt slot.
pub fn render(app: &App, frame: &mut ratatui::Frame, theme: &Theme, area: Rect) {
    let rows = lines(app);
    Paragraph::new(rows)
        .block(
            Block::new()
                .borders(ratatui::widgets::Borders::LEFT)
                .border_style(theme.accent.to_color())
                .style(Style::new().bg(theme.background_panel.to_color()))
                .padding(Padding {
                    left: 1,
                    right: 3,
                    top: 1,
                    bottom: 1,
                }),
        )
        .render(area, frame.buffer_mut());
}
