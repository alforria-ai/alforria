//! The workflows screen (an alforria extension, no TS reference): saved
//! workflows and live run progress, the terminal's counterpart of the web
//! client's Workflows view.
//!
//! A run lasts minutes, so it is something to watch rather than a dialog to
//! dismiss: this is a route of its own. Wide terminals show the list beside the
//! selected item; narrow ones show one pane at a time. Everything is keyed by
//! id, so a poll never moves the selection. Runs also surface in the session
//! they belong to ([`docked_lines`], [`sidebar_lines`], the `workflow` tool
//! row), so nobody has to open this screen to see that one is running.

use std::collections::HashSet;

use alforria_schema::workflow::{
    WorkflowAgent, WorkflowDefinition, WorkflowRun, WorkflowSource, WorkflowState,
};
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers, MouseButton, MouseEvent, MouseEventKind};
use ratatui::layout::Rect;
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Paragraph, Widget};
use serde_json::Value;

use crate::state::route::Route;
use crate::state::{App, Effect, PendingDialog, Toast, ToastVariant};
use crate::ui::dialogs::primitives::wrap_indented;
use crate::ui::locale;
use crate::ui::theme::{Rgba, Theme};
use crate::ui::SPINNER_FRAMES;

/// Below this width the two panes take turns.
const SPLIT_MIN_WIDTH: u16 = 90;
const LIST_WIDTH: u16 = 36;
const SPINNER_INTERVAL_MS: u64 = 80;
/// How often to poll: quick while something runs, slow while this screen is
/// open, so a run started elsewhere still shows up.
const ACTIVE_POLL_MS: u64 = 2000;
const IDLE_POLL_MS: u64 = 10_000;
/// A finished phase folds away beyond this many agents, unless one failed.
const FOLD_OVER: usize = 3;
const LOG_LINES: usize = 8;
/// How long the second press to quit counts as the answer.
const EXIT_WARNING_MS: u64 = 5000;
/// The runs of a session its sidebar lists.
const SIDEBAR_RUNS: usize = 5;

/// A new workflow's script, for `$EDITOR`.
pub fn new_script(name: &str) -> String {
    format!(
        r#"export const meta = {{
  name: '{name}',
  description: 'What this workflow does',
}}

// `args` holds what the run was started with.
phase('Plan')
const plan = await agent('Describe the plan for: ' + JSON.stringify(args), {{ label: 'plan' }})

phase('Work')
const results = await parallel([
  () => agent('Do the first half. Plan: ' + plan, {{ label: 'first' }}),
  () => agent('Do the second half. Plan: ' + plan, {{ label: 'second' }}),
])

return {{ plan, results }}
"#
    )
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Selected {
    Run(String),
    Saved(String),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Cursor {
    Phase(String),
    Agent(String),
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum Focus {
    #[default]
    List,
    Detail,
}

/// The screen's state, kept across visits, and the data behind every surface.
#[derive(Debug, Default)]
pub struct WorkflowsState {
    pub runs: Vec<WorkflowRun>,
    pub definitions: Vec<WorkflowDefinition>,
    pub loaded: bool,
    pub error: Option<String>,
    pub fetching: bool,
    polled_at: Option<u64>,
    /// A `workflow` tool call appeared: there may be a run to find out about.
    wake: bool,
    /// The first read, for a session resumed with a run still going.
    first_read: bool,
    /// When quitting was held back on account of a running workflow.
    exit_warned_at: Option<u64>,
    pub focus: Focus,
    pub selected: Option<Selected>,
    pub cursor: Option<Cursor>,
    /// Phases whose default fold was reversed, as `run\0title`.
    pub toggled: HashSet<String>,
    /// Agents showing their prompt and result.
    pub expanded: HashSet<String>,
    pub detail_scroll: usize,
    list_scroll: usize,
    /// What the last frame drew where, for the mouse.
    list_rows: Vec<(u16, Selected)>,
    detail_rows: Vec<(u16, Cursor)>,
    list_area: Rect,
    detail_area: Rect,
    detail_height: usize,
}

fn is_active(run: &WorkflowRun) -> bool {
    !run.state.is_terminal()
}

fn now_ms() -> f64 {
    crate::state::sync::now_epoch_ms() as f64
}

fn on_screen(app: &App) -> bool {
    matches!(app.state.route.data, Route::Workflows { .. })
}

// ------------------------------------------------------------------- data

/// A poll's answer. Finished runs are announced here, unless the user is
/// looking at them.
pub fn apply(
    app: &mut App,
    runs: Result<Vec<WorkflowRun>, String>,
    definitions: Option<Vec<WorkflowDefinition>>,
) -> Vec<Effect> {
    let mut effects = Vec::new();
    app.ui.workflows.fetching = false;
    match runs {
        Ok(runs) => {
            let visible = on_screen(app);
            let announced: Vec<(String, WorkflowState, Option<String>)> = runs
                .iter()
                .filter(|run| run.state.is_terminal())
                .filter(|run| {
                    app.ui
                        .workflows
                        .runs
                        .iter()
                        .any(|old| old.id == run.id && is_active(old))
                })
                .map(|run| (run.name.clone(), run.state, run.session_id.clone()))
                .collect();
            for (name, state, session) in announced {
                let (variant, word) = match state {
                    WorkflowState::Completed => (ToastVariant::Success, "completed"),
                    WorkflowState::Cancelled => (ToastVariant::Info, "stopped"),
                    _ => (ToastVariant::Error, "failed"),
                };
                let message = format!("Workflow {name} {word}");
                effects.push(crate::attention::ask_effect(
                    app,
                    session.as_deref(),
                    &message,
                ));
                if !visible {
                    app.show_toast(Toast {
                        title: None,
                        variant,
                        message,
                        duration_ms: 5000,
                    });
                }
            }
            app.ui.workflows.runs = runs;
            app.ui.workflows.error = None;
            app.ui.workflows.loaded = true;
        }
        Err(error) => app.ui.workflows.error = Some(error),
    }
    if let Some(definitions) = definitions {
        app.ui.workflows.definitions = definitions;
    }
    keep_selection_valid(app);
    app.version += 1;
    effects
}

fn keep_selection_valid(app: &mut App) {
    let state = &mut app.ui.workflows;
    let valid = match &state.selected {
        Some(Selected::Run(id)) => state.runs.iter().any(|run| &run.id == id),
        Some(Selected::Saved(name)) => state.definitions.iter().any(|item| &item.name == name),
        None => false,
    };
    if !valid {
        state.selected = default_selection(state);
        state.cursor = None;
        state.detail_scroll = 0;
    }
}

/// The running run, else the newest, else the first saved workflow.
fn default_selection(state: &WorkflowsState) -> Option<Selected> {
    state
        .runs
        .iter()
        .find(|run| is_active(run))
        .or(state.runs.first())
        .map(|run| Selected::Run(run.id.clone()))
        .or_else(|| {
            state
                .definitions
                .first()
                .map(|item| Selected::Saved(item.name.clone()))
        })
}

/// When the next poll is due, if one should happen at all.
fn poll_interval(app: &App) -> Option<u64> {
    let state = &app.ui.workflows;
    if state.wake || state.runs.iter().any(is_active) {
        return Some(ACTIVE_POLL_MS);
    }
    on_screen(app).then_some(IDLE_POLL_MS)
}

/// The poll, driven by the frame tick.
pub fn on_tick(app: &mut App, now_ms: u64) -> Option<Effect> {
    if app.ui.startup_ready && !app.ui.workflows.first_read {
        app.ui.workflows.first_read = true;
        app.ui.workflows.fetching = true;
        app.ui.workflows.polled_at = Some(now_ms);
        return Some(Effect::WorkflowsRefresh { definitions: true });
    }
    let interval = poll_interval(app)?;
    let state = &mut app.ui.workflows;
    if state.fetching || state.polled_at.is_some_and(|at| now_ms < at + interval) {
        return None;
    }
    state.polled_at = Some(now_ms);
    state.fetching = true;
    state.wake = false;
    Some(Effect::WorkflowsRefresh { definitions: false })
}

/// The moment the next poll is due, for the tick scheduler.
pub fn next_poll_at(app: &App, now_ms: u64) -> Option<u64> {
    let interval = poll_interval(app)?;
    Some(
        app.ui
            .workflows
            .polled_at
            .map_or(now_ms, |at| at + interval),
    )
}

/// A `workflow` tool call just changed: look for its run.
pub fn on_bus_event(app: &mut App, event: &alforria_schema::event_manifest::Event) {
    use alforria_schema::session_v1::V1Part;
    if let alforria_schema::event_manifest::Event::MessagePartUpdated(data) = event {
        if matches!(&data.part, V1Part::Tool { tool, .. } if tool == "workflow") {
            app.ui.workflows.wake = true;
        }
    }
}

/// Quitting stops the runs of the server inside this process, and a long run
/// is costly to lose by a stray ctrl+c: the first attempt only says so.
pub fn hold_exit(app: &mut App) -> bool {
    if !app.ui.workflows.runs.iter().any(is_active) {
        return false;
    }
    let now = app.ui.tick_ms;
    let warned = app
        .ui
        .workflows
        .exit_warned_at
        .is_some_and(|at| now.saturating_sub(at) < EXIT_WARNING_MS);
    if warned {
        return false;
    }
    app.ui.workflows.exit_warned_at = Some(now);
    app.show_toast(Toast {
        title: None,
        variant: ToastVariant::Warning,
        message: "A workflow is still running and quitting may stop it. Press again to quit."
            .to_string(),
        duration_ms: EXIT_WARNING_MS,
    });
    true
}

/// Whether something on this screen moves by itself.
pub fn animating(app: &App) -> bool {
    on_screen(app) && app.ui.workflows.runs.iter().any(is_active)
}

// -------------------------------------------------------------- navigation

/// Open the screen, or leave it if already there.
pub fn toggle(app: &mut App) -> Vec<Effect> {
    if on_screen(app) {
        leave(app);
        return Vec::new();
    }
    open(app, None)
}

/// Open the screen, on a given run if asked.
pub fn open(app: &mut App, run: Option<&str>) -> Vec<Effect> {
    if !on_screen(app) {
        let back = Box::new(app.state.route.data.clone());
        app.state.route.navigate(Route::Workflows { back });
    }
    let state = &mut app.ui.workflows;
    if let Some(run) = run {
        state.selected = Some(Selected::Run(run.to_string()));
        state.cursor = None;
        state.detail_scroll = 0;
        state.focus = Focus::Detail;
    } else if !matches!(state.selected, Some(Selected::Run(_))) {
        // Someone opening this while a run is going came to watch it.
        state.selected = default_selection(state);
        state.cursor = None;
        state.detail_scroll = 0;
        state.focus = Focus::List;
    }
    state.fetching = true;
    state.polled_at = None;
    vec![Effect::WorkflowsRefresh { definitions: true }]
}

fn leave(app: &mut App) {
    if let Route::Workflows { back } = &app.state.route.data {
        let back = (**back).clone();
        app.state.route.navigate(back);
    }
}

/// Select something and show it from the top.
fn select(state: &mut WorkflowsState, selected: Selected) {
    if state.selected.as_ref() != Some(&selected) {
        state.selected = Some(selected);
        state.cursor = None;
        state.detail_scroll = 0;
    }
}

fn selected_run(state: &WorkflowsState) -> Option<&WorkflowRun> {
    match &state.selected {
        Some(Selected::Run(id)) => state.runs.iter().find(|run| &run.id == id),
        _ => None,
    }
}

fn selected_saved(state: &WorkflowsState) -> Option<&WorkflowDefinition> {
    match &state.selected {
        Some(Selected::Saved(name)) => state.definitions.iter().find(|item| &item.name == name),
        _ => None,
    }
}

/// Everything in the list, in the order it is drawn.
fn list_items(state: &WorkflowsState) -> Vec<Selected> {
    state
        .runs
        .iter()
        .map(|run| Selected::Run(run.id.clone()))
        .chain(
            state
                .definitions
                .iter()
                .map(|item| Selected::Saved(item.name.clone())),
        )
        .collect()
}

fn move_list(state: &mut WorkflowsState, delta: i64) {
    let items = list_items(state);
    if items.is_empty() {
        return;
    }
    let at = state
        .selected
        .as_ref()
        .and_then(|selected| items.iter().position(|item| item == selected))
        .unwrap_or(0) as i64;
    let next = (at + delta).clamp(0, items.len() as i64 - 1) as usize;
    select(state, items[next].clone());
}

fn phase_key(run: &WorkflowRun, title: &str) -> String {
    format!("{}\0{title}", run.id)
}

/// The run's phases in order, including any only its agents mention.
fn phase_titles(run: &WorkflowRun) -> Vec<String> {
    let mut titles: Vec<String> = run.phases.iter().map(|phase| phase.title.clone()).collect();
    for agent in &run.agents {
        if !titles.contains(&agent.phase) {
            titles.push(agent.phase.clone());
        }
    }
    titles
}

fn agents_in<'a>(run: &'a WorkflowRun, title: &str) -> Vec<&'a WorkflowAgent> {
    run.agents
        .iter()
        .filter(|agent| agent.phase == title)
        .collect()
}

/// A finished phase with many agents folds away, unless one of them failed:
/// a big fan-out would otherwise push what is happening off the screen.
fn is_folded(state: &WorkflowsState, run: &WorkflowRun, title: &str) -> bool {
    let agents = agents_in(run, title);
    let finished = run
        .phases
        .iter()
        .find(|phase| phase.title == title)
        .is_some_and(|phase| phase.state.is_terminal());
    let by_default = finished
        && agents.len() > FOLD_OVER
        && agents
            .iter()
            .all(|agent| agent.state != WorkflowState::Failed);
    by_default != state.toggled.contains(&phase_key(run, title))
}

/// What the detail cursor can land on, in the order it is drawn.
fn cursor_items(state: &WorkflowsState, run: &WorkflowRun) -> Vec<Cursor> {
    let mut items = Vec::new();
    for title in phase_titles(run) {
        items.push(Cursor::Phase(title.clone()));
        if !is_folded(state, run, &title) {
            items.extend(
                agents_in(run, &title)
                    .iter()
                    .map(|agent| Cursor::Agent(agent.id.clone())),
            );
        }
    }
    items
}

fn move_cursor(state: &mut WorkflowsState, delta: i64) {
    let Some(run) = selected_run(state) else {
        state.detail_scroll = (state.detail_scroll as i64 + delta).max(0) as usize;
        return;
    };
    let items = cursor_items(state, run);
    if items.is_empty() {
        return;
    }
    let at = state
        .cursor
        .as_ref()
        .and_then(|cursor| items.iter().position(|item| item == cursor));
    let next = match at {
        Some(at) => (at as i64 + delta).clamp(0, items.len() as i64 - 1) as usize,
        None => 0,
    };
    state.cursor = Some(items[next].clone());
}

/// Moving into a run lands on what is happening: the first running or failed
/// agent if there is one.
fn enter_detail(state: &mut WorkflowsState) {
    state.focus = Focus::Detail;
    let Some(run) = selected_run(state) else {
        return;
    };
    if state.cursor.is_none() {
        let interesting = run
            .agents
            .iter()
            .find(|agent| agent.state == WorkflowState::Running)
            .or_else(|| {
                run.agents
                    .iter()
                    .find(|agent| agent.state == WorkflowState::Failed)
            })
            .map(|agent| Cursor::Agent(agent.id.clone()));
        let cursor = interesting.or_else(|| cursor_items(state, run).first().cloned());
        state.cursor = cursor;
    }
}

fn open_session(app: &mut App, session_id: &str) {
    app.state.route.navigate(Route::Session {
        session_id: session_id.to_string(),
        prompt: None,
    });
}

fn activate_cursor(app: &mut App) {
    let state = &mut app.ui.workflows;
    let Some(run) = selected_run(state) else {
        return;
    };
    match state.cursor.clone() {
        Some(Cursor::Phase(title)) => {
            let key = phase_key(run, &title);
            if !state.toggled.remove(&key) {
                state.toggled.insert(key);
            }
        }
        Some(Cursor::Agent(id)) => {
            let session = run
                .agents
                .iter()
                .find(|agent| agent.id == id)
                .and_then(|agent| agent.session_id.clone());
            match session {
                Some(session) => open_session(app, &session),
                None => app.show_toast(Toast {
                    title: None,
                    variant: ToastVariant::Info,
                    message: "This agent has not started yet".to_string(),
                    duration_ms: 3000,
                }),
            }
        }
        None => {}
    }
}

fn toggle_expanded(state: &mut WorkflowsState) {
    if let Some(Cursor::Agent(id)) = state.cursor.clone() {
        if !state.expanded.remove(&id) {
            state.expanded.insert(id);
        }
    }
}

/// The action keys shared by both panes.
fn action_key(app: &mut App, key: char) -> Option<Vec<Effect>> {
    let state = &app.ui.workflows;
    let run = selected_run(state).cloned();
    let saved = selected_saved(state).cloned();
    match key {
        'r' => {
            if let Some(saved) = saved {
                return Some(crate::ui::dialogs::open(
                    app,
                    PendingDialog::WorkflowRun {
                        name: Some(saved.name),
                        script: None,
                        title: saved_title(&saved.description),
                        args: String::new(),
                    },
                ));
            }
            let run = run?;
            // Run it again: by name while the workflow is still saved.
            let still_saved = state.definitions.iter().any(|item| item.name == run.name);
            Some(crate::ui::dialogs::open(
                app,
                PendingDialog::WorkflowRun {
                    title: run.description.clone(),
                    args: args_text(&run.args),
                    script: (!still_saved).then(|| run.script.clone()),
                    name: Some(run.name),
                },
            ))
        }
        's' => {
            let run = run.filter(is_active)?;
            Some(crate::ui::dialogs::open(
                app,
                PendingDialog::WorkflowStop {
                    run_id: run.id,
                    name: run.name,
                },
            ))
        }
        'o' => {
            let session = run?.session_id?;
            open_session(app, &session);
            Some(Vec::new())
        }
        'y' => {
            let result = run?.result?;
            Some(vec![Effect::ClipboardWrite {
                text: value_text(&result),
                success: Some(Toast {
                    title: None,
                    variant: ToastVariant::Info,
                    message: "Result copied to clipboard".to_string(),
                    duration_ms: 3000,
                }),
                failure: None,
            }])
        }
        'n' => Some(crate::ui::dialogs::open(app, PendingDialog::WorkflowNew)),
        'e' => {
            let saved = saved.filter(editable)?;
            Some(vec![Effect::WorkflowEdit {
                scope: scope_of(&saved).to_string(),
                name: saved.name,
                original: saved.script,
                overwrite: true,
            }])
        }
        'd' => {
            let saved = saved.filter(editable)?;
            Some(crate::ui::dialogs::open(
                app,
                PendingDialog::WorkflowDelete { name: saved.name },
            ))
        }
        _ => None,
    }
}

fn saved_title(description: &Option<String>) -> Option<String> {
    description.clone().filter(|text| !text.is_empty())
}

fn editable(saved: &WorkflowDefinition) -> bool {
    matches!(
        saved.source,
        WorkflowSource::Project | WorkflowSource::Global
    )
}

fn scope_of(saved: &WorkflowDefinition) -> &'static str {
    if saved.source == WorkflowSource::Global {
        "global"
    } else {
        "project"
    }
}

fn args_text(args: &Value) -> String {
    match args {
        Value::Null => String::new(),
        Value::Object(fields) if fields.is_empty() => String::new(),
        Value::String(text) => text.clone(),
        other => other.to_string(),
    }
}

/// What the run dialog's text means: JSON if it parses, else a plain string,
/// which is what most scripts want (`args` is "the question").
pub fn parse_args(text: &str) -> Value {
    let text = text.trim();
    if text.is_empty() {
        return Value::Null;
    }
    serde_json::from_str(text).unwrap_or_else(|_| Value::String(text.to_string()))
}

fn value_text(value: &Value) -> String {
    match value {
        Value::String(text) => text.clone(),
        other => serde_json::to_string_pretty(other).unwrap_or_default(),
    }
}

/// The screen's keys. `None` leaves the key to the keymap, so the palette and
/// leader sequences keep working here.
pub fn handle_key(app: &mut App, key: &KeyEvent) -> Option<Vec<Effect>> {
    if !on_screen(app) || app.keymap.leader_active() {
        return None;
    }
    let plain = key.modifiers.difference(KeyModifiers::SHIFT).is_empty();
    if !plain {
        return None;
    }
    let focus = app.ui.workflows.focus;
    let split = app.ui.terminal_width >= SPLIT_MIN_WIDTH;
    match key.code {
        KeyCode::Esc => match focus {
            Focus::Detail => app.ui.workflows.focus = Focus::List,
            Focus::List => leave(app),
        },
        KeyCode::Up | KeyCode::Char('k') => step(app, -1),
        KeyCode::Down | KeyCode::Char('j') => step(app, 1),
        KeyCode::PageUp => page(app, -1),
        KeyCode::PageDown => page(app, 1),
        KeyCode::Tab if split => {
            if focus == Focus::Detail {
                app.ui.workflows.focus = Focus::List;
            } else {
                enter_detail(&mut app.ui.workflows);
            }
        }
        KeyCode::Right | KeyCode::Char('l') | KeyCode::Char(' ') if focus == Focus::Detail => {
            toggle_expanded(&mut app.ui.workflows)
        }
        KeyCode::Right | KeyCode::Char('l') => enter_detail(&mut app.ui.workflows),
        KeyCode::Left | KeyCode::Char('h') if focus == Focus::Detail => {
            app.ui.workflows.focus = Focus::List
        }
        KeyCode::Enter => match focus {
            Focus::Detail => activate_cursor(app),
            Focus::List if selected_saved(&app.ui.workflows).is_some() => {
                return action_key(app, 'r');
            }
            Focus::List => enter_detail(&mut app.ui.workflows),
        },
        KeyCode::Char(char) => return action_key(app, char).or(Some(Vec::new())),
        _ => return None,
    }
    Some(Vec::new())
}

fn step(app: &mut App, delta: i64) {
    let state = &mut app.ui.workflows;
    match state.focus {
        Focus::List => move_list(state, delta),
        Focus::Detail => move_cursor(state, delta),
    }
}

fn page(app: &mut App, direction: i64) {
    let state = &mut app.ui.workflows;
    let jump = state.detail_height.max(2) as i64 - 1;
    state.detail_scroll = (state.detail_scroll as i64 + direction * jump).max(0) as usize;
    state.cursor = None;
    if selected_saved(state).is_none() && state.focus == Focus::Detail {
        state.focus = Focus::List;
    }
}

/// The wheel scrolls what is under it; a click selects, and a second click on
/// the same row does what Enter would.
pub fn handle_mouse(app: &mut App, mouse: &MouseEvent) -> bool {
    if !on_screen(app) {
        return false;
    }
    let state = &mut app.ui.workflows;
    let position = ratatui::layout::Position::new(mouse.column, mouse.row);
    match mouse.kind {
        MouseEventKind::ScrollUp | MouseEventKind::ScrollDown => {
            let delta: i64 = if matches!(mouse.kind, MouseEventKind::ScrollUp) {
                -3
            } else {
                3
            };
            if state.list_area.contains(position) {
                move_list(state, delta.signum());
            } else {
                state.detail_scroll = (state.detail_scroll as i64 + delta).max(0) as usize;
            }
            true
        }
        MouseEventKind::Down(MouseButton::Left) => {
            if state.list_area.contains(position) {
                let hit = state
                    .list_rows
                    .iter()
                    .find(|(y, _)| *y == mouse.row)
                    .map(|(_, item)| item.clone());
                if let Some(item) = hit {
                    state.focus = Focus::List;
                    select(state, item);
                }
                return true;
            }
            if state.detail_area.contains(position) {
                let hit = state
                    .detail_rows
                    .iter()
                    .find(|(y, _)| *y == mouse.row)
                    .map(|(_, item)| item.clone());
                if let Some(item) = hit {
                    let again = state.cursor.as_ref() == Some(&item);
                    state.focus = Focus::Detail;
                    state.cursor = Some(item);
                    if again {
                        activate_cursor(app);
                    }
                }
                return true;
            }
            false
        }
        _ => false,
    }
}

// --------------------------------------------------------------- rendering

fn state_word(state: WorkflowState) -> &'static str {
    match state {
        WorkflowState::Queued => "queued",
        WorkflowState::Running => "running",
        WorkflowState::Completed => "completed",
        WorkflowState::Failed => "failed",
        WorkflowState::Cancelled => "stopped",
    }
}

fn state_color(theme: &Theme, state: WorkflowState) -> Rgba {
    match state {
        WorkflowState::Queued => theme.text_muted,
        WorkflowState::Running => theme.warning,
        WorkflowState::Completed => theme.success,
        WorkflowState::Failed => theme.error,
        WorkflowState::Cancelled => theme.text_muted,
    }
}

fn spinner(app: &App) -> &'static str {
    let animations = app
        .state
        .kv
        .get_bool(crate::state::kv::keys::ANIMATIONS_ENABLED, true);
    if !animations {
        return "•";
    }
    SPINNER_FRAMES[(app.ui.tick_ms / SPINNER_INTERVAL_MS) as usize % SPINNER_FRAMES.len()]
}

fn mark(app: &App, state: WorkflowState) -> &'static str {
    match state {
        WorkflowState::Queued => "·",
        WorkflowState::Running => spinner(app),
        WorkflowState::Completed => "✓",
        WorkflowState::Failed => "✗",
        WorkflowState::Cancelled => "-",
    }
}

fn elapsed(started: f64, finished: Option<f64>) -> String {
    let end = finished.unwrap_or_else(now_ms);
    locale::duration((end - started).max(0.0) as i64)
}

fn style(color: Rgba) -> Style {
    Style::new().fg(color.to_color())
}

fn fit(text: &str, width: usize) -> String {
    use unicode_width::UnicodeWidthStr;
    if text.width() <= width {
        return text.to_string();
    }
    let mut out = String::new();
    let mut used = 0;
    for char in text.chars() {
        let char_width = unicode_width::UnicodeWidthChar::width(char).unwrap_or(0);
        if used + char_width + 1 > width {
            break;
        }
        out.push(char);
        used += char_width;
    }
    out.push('…');
    out
}

fn first_line(text: &str) -> &str {
    text.lines()
        .find(|line| !line.trim().is_empty())
        .unwrap_or("")
        .trim()
}

/// `3/7 agents`, with the phase in progress: how far along a run is.
pub fn progress(run: &WorkflowRun) -> String {
    let finished = run
        .agents
        .iter()
        .filter(|agent| agent.state.is_terminal())
        .count();
    let mut parts = Vec::new();
    if is_active(run) {
        if let Some(phase) = run
            .phases
            .iter()
            .rev()
            .find(|phase| phase.state == WorkflowState::Running)
        {
            parts.push(phase.title.clone());
        }
    }
    if !run.agents.is_empty() {
        parts.push(format!("{finished}/{} agents", run.agents.len()));
    }
    let failed = run
        .agents
        .iter()
        .filter(|agent| agent.state == WorkflowState::Failed)
        .count();
    if failed > 0 {
        parts.push(format!("{failed} failed"));
    }
    parts.join(" · ")
}

/// What a running agent is doing, when its session's events are in.
fn agent_activity(app: &App, agent: &WorkflowAgent) -> Option<String> {
    use alforria_schema::session_v1::{V1Message, V1Part, V1ToolState};
    let session = agent.session_id.as_deref()?;
    let sync = &app.state.sync;
    let newest = sync
        .message
        .get(session)?
        .iter()
        .rev()
        .find_map(|message| {
            if let V1Message::Assistant { id, .. } = message {
                Some(id)
            } else {
                None
            }
        })?;
    // Between automatic resumes it sits idle on its error.
    let busy = matches!(
        sync.session_status.get(session),
        Some(alforria_schema::session_status::SessionStatusInfo::Busy)
    );
    let stopped_on = sync
        .message
        .get(session)?
        .iter()
        .rev()
        .find_map(|message| {
            if let V1Message::Assistant { error, .. } = message {
                Some(error.as_ref())
            } else {
                None
            }
        })?;
    if let (false, Some(error)) = (busy, stopped_on) {
        let error = crate::ui::session::parts::assistant_error_message(error);
        return Some(format!("resuming after an error: {}", first_line(&error)));
    }
    sync.part
        .get(newest.as_str())?
        .iter()
        .rev()
        .find_map(|part| match part {
            V1Part::Tool {
                tool,
                state: V1ToolState::Running { title, .. },
                ..
            } => Some(match title {
                Some(title) => format!("{tool} {title}"),
                None => tool.clone(),
            }),
            _ => None,
        })
}

/// The list rows: runs, then saved workflows. The selection's row is tracked
/// so the pane can follow it.
fn list_lines(
    app: &App,
    theme: &Theme,
    width: u16,
) -> (Vec<Line<'static>>, Vec<(usize, Selected)>) {
    let state = &app.ui.workflows;
    let width = width as usize;
    let focused = state.focus == Focus::List;
    let heading = |text: &str| {
        Line::styled(
            text.to_string(),
            style(theme.text_muted).add_modifier(Modifier::BOLD),
        )
    };
    let row_style = |selected: bool| {
        if selected && focused {
            Style::new()
                .bg(theme.primary.to_color())
                .fg(crate::ui::theme::selected_foreground(theme, Some(theme.primary)).to_color())
        } else if selected {
            Style::new()
                .bg(theme.background_element.to_color())
                .fg(theme.text.to_color())
        } else {
            style(theme.text)
        }
    };
    let mut lines = vec![heading("Runs")];
    let mut rows = Vec::new();
    if state.runs.is_empty() {
        lines.push(Line::styled("  No runs yet", style(theme.text_muted)));
    }
    for run in &state.runs {
        let item = Selected::Run(run.id.clone());
        let selected = state.selected.as_ref() == Some(&item);
        rows.push((lines.len(), item));
        let summary = if run.agents.is_empty() {
            state_word(run.state).to_string()
        } else {
            let finished = run
                .agents
                .iter()
                .filter(|agent| agent.state.is_terminal())
                .count();
            format!("{finished}/{}", run.agents.len())
        };
        let room = width.saturating_sub(summary.chars().count() + 5);
        let body = format!("{:<room$}", fit(&run.name, room));
        let base = row_style(selected);
        let mark_style = if selected && focused {
            base
        } else {
            base.fg(state_color(theme, run.state).to_color())
        };
        let summary_style = if selected {
            base
        } else {
            base.fg(theme.text_muted.to_color())
        };
        lines.push(Line::from(vec![
            Span::styled(format!(" {} ", mark(app, run.state)), mark_style),
            Span::styled(body, base),
            Span::styled(format!(" {summary} "), summary_style),
        ]));
    }
    lines.push(Line::raw(""));
    lines.push(heading("Saved"));
    if state.definitions.is_empty() {
        lines.push(Line::styled("  None saved", style(theme.text_muted)));
    }
    for saved in &state.definitions {
        let item = Selected::Saved(saved.name.clone());
        let selected = state.selected.as_ref() == Some(&item);
        rows.push((lines.len(), item));
        let source = match saved.source {
            WorkflowSource::Project => "project",
            WorkflowSource::Global => "user",
            WorkflowSource::External => "claude",
            WorkflowSource::Builtin => "built-in",
        };
        let room = width.saturating_sub(source.chars().count() + 5);
        let base = row_style(selected);
        let source_style = if selected {
            base
        } else {
            base.fg(theme.text_muted.to_color())
        };
        lines.push(Line::from(vec![
            Span::styled("   ", base),
            Span::styled(format!("{:<room$}", fit(&saved.name, room)), base),
            Span::styled(format!(" {source} "), source_style),
        ]));
    }
    (lines, rows)
}

fn cursor_style(theme: &Theme, focused: bool) -> Style {
    if focused {
        Style::new()
            .bg(theme.primary.to_color())
            .fg(crate::ui::theme::selected_foreground(theme, Some(theme.primary)).to_color())
    } else {
        Style::new().bg(theme.background_element.to_color())
    }
}

fn phase_counts(agents: &[&WorkflowAgent]) -> String {
    let count = |state| agents.iter().filter(|agent| agent.state == state).count();
    let mut parts = Vec::new();
    for (state, word) in [
        (WorkflowState::Running, "running"),
        (WorkflowState::Queued, "queued"),
        (WorkflowState::Completed, "done"),
        (WorkflowState::Failed, "failed"),
        (WorkflowState::Cancelled, "stopped"),
    ] {
        let found = count(state);
        if found > 0 {
            parts.push(format!("{found} {word}"));
        }
    }
    parts.join(" · ")
}

/// The detail of a run, and which line each cursor item is on.
fn run_lines(
    app: &App,
    theme: &Theme,
    run: &WorkflowRun,
    width: u16,
) -> (Vec<Line<'static>>, Vec<(usize, Cursor)>) {
    let state = &app.ui.workflows;
    let focused = state.focus == Focus::Detail;
    let muted = style(theme.text_muted);
    let wrap = width.saturating_sub(2);
    let mut lines = vec![Line::from(vec![
        Span::styled(
            run.name.clone(),
            style(theme.text).add_modifier(Modifier::BOLD),
        ),
        Span::styled(
            format!("  {}", state_word(run.state)),
            style(state_color(theme, run.state)),
        ),
        Span::styled(format!("  {}", elapsed(run.started, run.finished)), muted),
    ])];
    if let Some(description) = run.description.as_deref().filter(|text| !text.is_empty()) {
        wrap_indented(description, wrap, 0, &mut lines, muted);
    }
    if let Some(error) = &run.error {
        lines.push(Line::raw(""));
        wrap_indented(error, wrap, 0, &mut lines, style(theme.error));
    }
    lines.push(Line::raw(""));

    let mut anchors = Vec::new();
    let titles = phase_titles(run);
    if titles.is_empty() {
        let waiting = if is_active(run) {
            "Starting…"
        } else {
            "This run started no agents."
        };
        lines.push(Line::styled(waiting, muted));
    }
    for title in titles {
        let agents = agents_in(run, &title);
        let phase_state = run
            .phases
            .iter()
            .find(|phase| phase.title == title)
            .map(|phase| phase.state)
            .unwrap_or(WorkflowState::Queued);
        let folded = is_folded(state, run, &title);
        let item = Cursor::Phase(title.clone());
        let selected = state.cursor.as_ref() == Some(&item);
        anchors.push((lines.len(), item));
        let row = if selected {
            cursor_style(theme, focused)
        } else {
            Style::new()
        };
        let fold = if folded { "▸" } else { "▾" };
        lines.push(Line::from(vec![
            Span::styled(
                format!("{fold} {} ", mark(app, phase_state)),
                row.patch(style(state_color(theme, phase_state))),
            ),
            Span::styled(title.clone(), row.add_modifier(Modifier::BOLD)),
            Span::styled(format!("  {}", phase_counts(&agents)), row.patch(muted)),
        ]));
        if folded {
            continue;
        }
        for agent in agents {
            let item = Cursor::Agent(agent.id.clone());
            let selected = state.cursor.as_ref() == Some(&item);
            anchors.push((lines.len(), item));
            let row = if selected {
                cursor_style(theme, focused)
            } else {
                Style::new()
            };
            let time = if agent.state == WorkflowState::Queued {
                String::new()
            } else {
                format!("  {}", elapsed(agent.started, agent.finished))
            };
            let detail = match agent.state {
                WorkflowState::Running => agent_activity(app, agent),
                WorkflowState::Failed => agent
                    .error
                    .as_deref()
                    .map(|error| first_line(error).to_string()),
                _ => None,
            };
            let kind = format!("  @{}", agent.agent);
            let used =
                6 + agent.label.chars().count() + kind.chars().count() + time.chars().count();
            let room = (width as usize).saturating_sub(used + 3);
            let mut spans = vec![
                Span::styled(
                    format!("    {} ", mark(app, agent.state)),
                    row.patch(style(state_color(theme, agent.state))),
                ),
                Span::styled(agent.label.clone(), row.patch(style(theme.text))),
                Span::styled(kind, row.patch(muted)),
                Span::styled(time, row.patch(muted)),
            ];
            if let Some(detail) = detail.filter(|_| room > 8) {
                let color = if agent.state == WorkflowState::Failed {
                    theme.error
                } else {
                    theme.text_muted
                };
                spans.push(Span::styled(
                    format!("  {}", fit(&detail, room)),
                    row.patch(style(color)),
                ));
            }
            lines.push(Line::from(spans));
            if state.expanded.contains(&agent.id) {
                let indent = 6;
                let inner = wrap.saturating_sub(indent as u16);
                lines.push(Line::styled(
                    "      Prompt",
                    muted.add_modifier(Modifier::BOLD),
                ));
                wrap_indented(&agent.prompt, inner, indent, &mut lines, muted);
                if let Some(error) = &agent.error {
                    lines.push(Line::styled(
                        "      Error",
                        style(theme.error).add_modifier(Modifier::BOLD),
                    ));
                    wrap_indented(error, inner, indent, &mut lines, style(theme.error));
                }
                if let Some(result) = &agent.result {
                    lines.push(Line::styled(
                        "      Result",
                        style(theme.text).add_modifier(Modifier::BOLD),
                    ));
                    wrap_indented(result, inner, indent, &mut lines, style(theme.text));
                }
            }
        }
    }
    if !run.logs.is_empty() {
        lines.push(Line::raw(""));
        lines.push(Line::styled("Log", muted.add_modifier(Modifier::BOLD)));
        let skip = run.logs.len().saturating_sub(LOG_LINES);
        for log in &run.logs[skip..] {
            wrap_indented(&log.message, wrap, 2, &mut lines, muted);
        }
    }
    if let Some(result) = run.result.as_ref().filter(|result| !result.is_null()) {
        lines.push(Line::raw(""));
        lines.push(Line::styled(
            "Result",
            style(theme.text).add_modifier(Modifier::BOLD),
        ));
        wrap_indented(&value_text(result), wrap, 0, &mut lines, style(theme.text));
    }
    (lines, anchors)
}

fn saved_lines(theme: &Theme, saved: &WorkflowDefinition, width: u16) -> Vec<Line<'static>> {
    let muted = style(theme.text_muted);
    let wrap = width.saturating_sub(2);
    let mut lines = vec![Line::styled(
        saved.name.clone(),
        style(theme.text).add_modifier(Modifier::BOLD),
    )];
    if let Some(description) = saved.description.as_deref().filter(|text| !text.is_empty()) {
        wrap_indented(description, wrap, 0, &mut lines, muted);
    }
    lines.push(Line::raw(""));
    if saved.source != WorkflowSource::Builtin {
        wrap_indented(&saved.path, wrap, 0, &mut lines, muted);
    }
    if !saved.phases.is_empty() {
        wrap_indented(
            &format!("Phases: {}", saved.phases.join(" → ")),
            wrap,
            0,
            &mut lines,
            muted,
        );
    }
    lines.push(Line::raw(""));
    for row in saved.script.lines() {
        lines.push(Line::styled(row.to_string(), style(theme.text)));
    }
    lines
}

/// What the keys do right now.
fn hints(app: &App) -> Vec<(&'static str, &'static str)> {
    let state = &app.ui.workflows;
    let mut keys = Vec::new();
    let run = selected_run(state);
    let saved = selected_saved(state);
    if state.focus == Focus::Detail && run.is_some() {
        match &state.cursor {
            Some(Cursor::Agent(_)) => {
                keys.push(("enter", "open"));
                keys.push(("space", "details"));
            }
            Some(Cursor::Phase(_)) => keys.push(("enter", "fold")),
            None => {}
        }
    } else if run.is_some() {
        keys.push(("enter", "agents"));
    } else if saved.is_some() {
        keys.push(("enter", "run"));
    }
    if run.is_some_and(is_active) {
        keys.push(("s", "stop"));
    } else if run.is_some() {
        keys.push(("r", "run again"));
    }
    if run.is_some_and(|run| run.session_id.is_some()) {
        keys.push(("o", "session"));
    }
    if run.is_some_and(|run| run.result.is_some()) {
        keys.push(("y", "copy result"));
    }
    if saved.is_some_and(editable) {
        keys.push(("e", "edit"));
        keys.push(("d", "delete"));
    }
    keys.push(("n", "new"));
    keys.push(("esc", "back"));
    keys
}

/// The screen.
pub fn render(app: &mut App, frame: &mut ratatui::Frame, theme: &Theme, area: Rect) {
    if area.width < 10 || area.height < 5 {
        return;
    }
    let inner = Rect {
        x: area.x + 2,
        y: area.y + 1,
        width: area.width.saturating_sub(4),
        height: area.height.saturating_sub(2),
    };
    let [head, body, foot] = ratatui::layout::Layout::vertical([
        ratatui::layout::Constraint::Length(2),
        ratatui::layout::Constraint::Fill(1),
        ratatui::layout::Constraint::Length(1),
    ])
    .areas(inner);

    let active = app
        .ui
        .workflows
        .runs
        .iter()
        .filter(|run| is_active(run))
        .count();
    let mut title = vec![Span::styled(
        "Workflows",
        style(theme.text).add_modifier(Modifier::BOLD),
    )];
    if active > 0 {
        title.push(Span::styled(
            format!("  {active} running"),
            style(theme.warning),
        ));
    }
    if let Some(error) = &app.ui.workflows.error {
        title.push(Span::styled(
            format!("  {}", fit(error, head.width.saturating_sub(24) as usize)),
            style(theme.error),
        ));
    }
    Paragraph::new(Line::from(title)).render(head, frame.buffer_mut());

    let empty = app.ui.workflows.loaded
        && app.ui.workflows.runs.is_empty()
        && app.ui.workflows.definitions.is_empty();
    if empty {
        let muted = style(theme.text_muted);
        let mut lines = vec![Line::styled("No workflows yet", style(theme.text))];
        wrap_indented(
            "A workflow is a script that fans work out to many agents. Press n to write one, or ask the agent to use a workflow.",
            body.width,
            0,
            &mut lines,
            muted,
        );
        Paragraph::new(lines).render(body, frame.buffer_mut());
    } else {
        let split = area.width >= SPLIT_MIN_WIDTH;
        let focus = app.ui.workflows.focus;
        let (list_area, detail_area) = if split {
            let [list, _, detail] = ratatui::layout::Layout::horizontal([
                ratatui::layout::Constraint::Length(LIST_WIDTH),
                ratatui::layout::Constraint::Length(2),
                ratatui::layout::Constraint::Fill(1),
            ])
            .areas(body);
            (list, detail)
        } else if focus == Focus::List {
            (body, Rect::default())
        } else {
            (Rect::default(), body)
        };
        render_list(app, frame, theme, list_area);
        render_detail(app, frame, theme, detail_area);
    }

    let mut keys = hints(app);
    let shown = |keys: &[(&str, &str)]| {
        keys.iter()
            .map(|(key, what)| key.chars().count() + what.chars().count() + 4)
            .sum::<usize>()
    };
    // The most useful keys are first and the way out is last: what doesn't fit
    // goes from just before it.
    while keys.len() > 1 && shown(&keys) > foot.width as usize {
        keys.remove(keys.len() - 2);
    }
    let hint_spans: Vec<Span<'static>> = keys
        .into_iter()
        .flat_map(|(key, what)| {
            [
                Span::styled(key.to_string(), style(theme.text)),
                Span::styled(format!(" {what}   "), style(theme.text_muted)),
            ]
        })
        .collect();
    Paragraph::new(Line::from(hint_spans)).render(foot, frame.buffer_mut());
}

fn render_list(app: &mut App, frame: &mut ratatui::Frame, theme: &Theme, area: Rect) {
    app.ui.workflows.list_area = area;
    app.ui.workflows.list_rows.clear();
    if area.is_empty() {
        return;
    }
    let (lines, rows) = list_lines(app, theme, area.width);
    let height = area.height as usize;
    let state = &mut app.ui.workflows;
    if let Some(line) = rows
        .iter()
        .find(|(_, item)| state.selected.as_ref() == Some(item))
        .map(|(line, _)| *line)
    {
        if line < state.list_scroll {
            state.list_scroll = line.saturating_sub(1);
        } else if line >= state.list_scroll + height {
            state.list_scroll = line + 1 - height;
        }
    }
    state.list_scroll = state.list_scroll.min(lines.len().saturating_sub(height));
    let top = state.list_scroll;
    state.list_rows = rows
        .into_iter()
        .filter(|(line, _)| *line >= top && *line < top + height)
        .map(|(line, item)| (area.y + (line - top) as u16, item))
        .collect();
    let visible: Vec<Line<'static>> = lines.into_iter().skip(top).take(height).collect();
    Paragraph::new(visible).render(area, frame.buffer_mut());
}

fn render_detail(app: &mut App, frame: &mut ratatui::Frame, theme: &Theme, area: Rect) {
    app.ui.workflows.detail_area = area;
    app.ui.workflows.detail_rows.clear();
    app.ui.workflows.detail_height = area.height as usize;
    if area.is_empty() {
        return;
    }
    let (lines, anchors) = {
        let state = &app.ui.workflows;
        if let Some(run) = selected_run(state) {
            run_lines(app, theme, run, area.width)
        } else if let Some(saved) = selected_saved(state) {
            (saved_lines(theme, saved, area.width), Vec::new())
        } else {
            (
                vec![Line::styled("Nothing selected", style(theme.text_muted))],
                Vec::new(),
            )
        }
    };
    let height = area.height as usize;
    let state = &mut app.ui.workflows;
    if let Some(line) = anchors
        .iter()
        .find(|(_, item)| state.cursor.as_ref() == Some(item))
        .map(|(line, _)| *line)
    {
        if line < state.detail_scroll {
            state.detail_scroll = line;
        } else if line >= state.detail_scroll + height {
            state.detail_scroll = line + 1 - height;
        }
    }
    state.detail_scroll = state.detail_scroll.min(lines.len().saturating_sub(height));
    let top = state.detail_scroll;
    state.detail_rows = anchors
        .into_iter()
        .filter(|(line, _)| *line >= top && *line < top + height)
        .map(|(line, item)| (area.y + (line - top) as u16, item))
        .collect();
    let visible: Vec<Line<'static>> = lines.into_iter().skip(top).take(height).collect();
    Paragraph::new(visible).render(area, frame.buffer_mut());
}

// ----------------------------------------------------------- in a session

fn session_runs<'a>(app: &'a App, session_id: &str) -> Vec<&'a WorkflowRun> {
    app.ui
        .workflows
        .runs
        .iter()
        .filter(|run| run.session_id.as_deref() == Some(session_id))
        .collect()
}

fn run_line(app: &App, theme: &Theme, run: &WorkflowRun) -> Line<'static> {
    let mut spans = vec![
        Span::styled(
            format!("{} ", mark(app, run.state)),
            style(state_color(theme, run.state)),
        ),
        Span::styled(run.name.clone(), style(theme.text)),
    ];
    let progress = progress(run);
    if !progress.is_empty() {
        spans.push(Span::styled(
            format!(" · {progress}"),
            style(theme.text_muted),
        ));
    }
    Line::from(spans)
}

/// The runs of this session still going, above the prompt, so nobody has to
/// open the screen to know.
pub fn docked_lines(app: &App, theme: &Theme, session_id: &str) -> Vec<Line<'static>> {
    let runs: Vec<&WorkflowRun> = session_runs(app, session_id)
        .into_iter()
        .filter(|run| is_active(run))
        .collect();
    if runs.is_empty() {
        return Vec::new();
    }
    let muted = style(theme.text_muted);
    let mut header = vec![Span::styled(
        "Workflows",
        style(theme.text).add_modifier(Modifier::BOLD),
    )];
    let view = app.keymap.shortcut("workflows.open");
    if !view.is_empty() {
        header.push(Span::styled(format!("  {view} to view"), muted));
    }
    let mut lines = vec![Line::from(header)];
    lines.extend(runs.iter().map(|run| run_line(app, theme, run)));
    lines
}

/// The session's runs in its sidebar, newest first.
pub fn sidebar_lines(app: &App, theme: &Theme, session_id: &str) -> Vec<Line<'static>> {
    let runs = session_runs(app, session_id);
    if runs.is_empty() {
        return Vec::new();
    }
    let mut lines = vec![Line::styled(
        "Workflows",
        style(theme.text).add_modifier(Modifier::BOLD),
    )];
    lines.extend(
        runs.iter()
            .take(SIDEBAR_RUNS)
            .map(|run| run_line(app, theme, run)),
    );
    lines
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::state::{update, Args, Msg};
    use alforria_schema::workflow::WorkflowPhase;
    use ratatui::backend::TestBackend;
    use ratatui::Terminal;
    use serde_json::json;

    fn app() -> App {
        App::new(crate::config::TuiConfig::default(), Args::default(), None)
    }

    fn agent(id: &str, phase: &str, state: WorkflowState) -> WorkflowAgent {
        WorkflowAgent {
            id: id.to_string(),
            session_id: Some(format!("ses_{id}")),
            phase: phase.to_string(),
            label: format!("label {id}"),
            agent: "general".to_string(),
            prompt: format!("prompt of {id}"),
            state,
            result: (state == WorkflowState::Completed).then(|| format!("result of {id}")),
            error: (state == WorkflowState::Failed).then(|| format!("error of {id}\nmore")),
            started: 1000.0,
            finished: state.is_terminal().then_some(4000.0),
        }
    }

    fn phase(title: &str, state: WorkflowState) -> WorkflowPhase {
        WorkflowPhase {
            title: title.to_string(),
            state,
            started: 1000.0,
            finished: state.is_terminal().then_some(4000.0),
        }
    }

    fn run(id: &str, state: WorkflowState) -> WorkflowRun {
        WorkflowRun {
            id: id.to_string(),
            name: format!("flow-{id}"),
            description: Some("what it does".to_string()),
            directory: "/x".to_string(),
            session_id: Some("ses_parent".to_string()),
            state,
            args: json!({ "question": "why" }),
            script: "return 1".to_string(),
            phases: vec![
                phase("Plan", WorkflowState::Completed),
                phase("Work", state),
            ],
            agents: vec![
                agent("a", "Plan", WorkflowState::Completed),
                agent("b", "Work", WorkflowState::Running),
                agent("c", "Work", WorkflowState::Failed),
                agent("d", "Work", WorkflowState::Queued),
            ],
            logs: Vec::new(),
            result: None,
            error: None,
            started: 1000.0,
            finished: state.is_terminal().then_some(4000.0),
        }
    }

    fn saved(name: &str, source: WorkflowSource) -> WorkflowDefinition {
        WorkflowDefinition {
            name: name.to_string(),
            description: Some("saved one".to_string()),
            path: format!("/x/.opencode/workflows/{name}.js"),
            source,
            phases: vec!["Plan".to_string()],
            script: "export const meta = {}\nreturn 1".to_string(),
        }
    }

    fn opened(runs: Vec<WorkflowRun>, definitions: Vec<WorkflowDefinition>) -> App {
        let mut app = app();
        app.ui.terminal_width = 120;
        app.ui.terminal_height = 40;
        open(&mut app, None);
        apply(&mut app, Ok(runs), Some(definitions));
        app
    }

    fn key(app: &mut App, code: KeyCode) -> Vec<Effect> {
        update(app, Msg::Key(KeyEvent::new(code, KeyModifiers::NONE)))
    }

    fn screen(app: &mut App, width: u16, height: u16) -> Vec<String> {
        let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
        terminal.draw(|frame| crate::ui::view(app, frame)).unwrap();
        let buffer = terminal.backend().buffer().clone();
        (0..height)
            .map(|y| {
                (0..width)
                    .map(|x| buffer[(x, y)].symbol().to_string())
                    .collect::<String>()
                    .trim_end()
                    .to_string()
            })
            .collect()
    }

    fn shows(rows: &[String], text: &str) -> bool {
        rows.iter().any(|row| row.contains(text))
    }

    #[test]
    fn progress_says_where_a_run_is() {
        assert_eq!(
            progress(&run("r", WorkflowState::Running)),
            "Work · 2/4 agents · 1 failed"
        );
        assert_eq!(
            progress(&run("r", WorkflowState::Completed)),
            "2/4 agents · 1 failed"
        );
    }

    #[test]
    fn opening_leaves_for_where_it_came_from() {
        let mut app = app();
        app.state.route.navigate(Route::Session {
            session_id: "ses_1".to_string(),
            prompt: None,
        });
        let effects = toggle(&mut app);
        assert!(matches!(
            effects.as_slice(),
            [Effect::WorkflowsRefresh { definitions: true }]
        ));
        assert!(on_screen(&app));
        key(&mut app, KeyCode::Esc);
        assert_eq!(
            app.state.route.data,
            Route::Session {
                session_id: "ses_1".to_string(),
                prompt: None
            }
        );
    }

    #[test]
    fn the_list_starts_on_the_running_run_and_a_poll_keeps_the_selection() {
        let mut app = opened(
            vec![
                run("done", WorkflowState::Completed),
                run("live", WorkflowState::Running),
            ],
            vec![saved("mine", WorkflowSource::Project)],
        );
        assert_eq!(
            app.ui.workflows.selected,
            Some(Selected::Run("live".to_string()))
        );
        key(&mut app, KeyCode::Up);
        assert_eq!(
            app.ui.workflows.selected,
            Some(Selected::Run("done".to_string()))
        );
        apply(
            &mut app,
            Ok(vec![
                run("new", WorkflowState::Running),
                run("done", WorkflowState::Completed),
            ]),
            None,
        );
        assert_eq!(
            app.ui.workflows.selected,
            Some(Selected::Run("done".to_string()))
        );
    }

    #[test]
    fn quitting_during_a_run_asks_for_a_second_press() {
        let ctrl_c = || Msg::Key(KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL));
        let mut app = app();
        apply(&mut app, Ok(vec![run("r", WorkflowState::Running)]), None);
        update(&mut app, ctrl_c());
        assert!(!app.ui.exit, "the first press only warns");
        assert!(app
            .ui
            .toasts
            .last()
            .unwrap()
            .message
            .contains("still running"));
        update(&mut app, ctrl_c());
        assert!(app.ui.exit);

        let mut idle = self::tests::app();
        apply(
            &mut idle,
            Ok(vec![run("r", WorkflowState::Completed)]),
            None,
        );
        update(&mut idle, ctrl_c());
        assert!(idle.ui.exit, "nothing to lose");
    }

    #[test]
    fn the_selected_row_stands_out() {
        let mut app = opened(vec![run("live", WorkflowState::Running)], Vec::new());
        let theme = crate::ui::session::test_theme();
        let mut terminal = Terminal::new(TestBackend::new(120, 30)).unwrap();
        terminal
            .draw(|frame| crate::ui::view(&mut app, frame))
            .unwrap();
        let buffer = terminal.backend().buffer().clone();
        let y = (0..30)
            .find(|y| {
                (0..40)
                    .map(|x| buffer[(x, *y)].symbol().to_string())
                    .collect::<String>()
                    .contains("flow-live")
            })
            .expect("the run's row");
        assert_eq!(buffer[(10, y)].bg, theme.primary.to_color());
    }

    #[test]
    fn opening_during_a_run_shows_the_run_not_a_saved_workflow() {
        // The first read found only the saved workflow; then a run started.
        let mut app = app();
        apply(
            &mut app,
            Ok(Vec::new()),
            Some(vec![saved("mine", WorkflowSource::Project)]),
        );
        apply(
            &mut app,
            Ok(vec![run("live", WorkflowState::Running)]),
            None,
        );
        assert_eq!(
            app.ui.workflows.selected,
            Some(Selected::Saved("mine".to_string()))
        );
        open(&mut app, None);
        assert_eq!(
            app.ui.workflows.selected,
            Some(Selected::Run("live".to_string()))
        );
    }

    #[test]
    fn coming_back_keeps_the_run_being_looked_at() {
        let mut app = opened(
            vec![
                run("live", WorkflowState::Running),
                run("old", WorkflowState::Completed),
            ],
            Vec::new(),
        );
        key(&mut app, KeyCode::Down);
        key(&mut app, KeyCode::Esc);
        open(&mut app, None);
        assert_eq!(
            app.ui.workflows.selected,
            Some(Selected::Run("old".to_string()))
        );
    }

    #[test]
    fn the_way_out_is_always_in_the_key_hints() {
        let mut app = opened(
            vec![run("live", WorkflowState::Running)],
            vec![saved("mine", WorkflowSource::Project)],
        );
        let rows = screen(&mut app, 40, 20);
        assert!(shows(&rows, "esc back"), "{rows:#?}");
    }

    #[test]
    fn the_screen_shows_phases_agents_and_failures() {
        let mut app = opened(
            vec![run("live", WorkflowState::Running)],
            vec![saved("mine", WorkflowSource::Project)],
        );
        let rows = screen(&mut app, 120, 30);
        assert!(shows(&rows, "Workflows  1 running"), "{rows:#?}");
        assert!(shows(&rows, "flow-live"), "{rows:#?}");
        assert!(shows(&rows, "mine"), "{rows:#?}");
        assert!(shows(&rows, "Plan"), "{rows:#?}");
        assert!(shows(&rows, "1 running · 1 queued · 1 failed"), "{rows:#?}");
        assert!(shows(&rows, "label b"), "{rows:#?}");
        assert!(shows(&rows, "error of c"), "{rows:#?}");
        assert!(!shows(&rows, "more"), "only the first line: {rows:#?}");
        assert!(shows(&rows, "s stop"), "{rows:#?}");
    }

    #[test]
    fn narrow_terminals_show_one_pane_at_a_time() {
        let mut app = opened(vec![run("live", WorkflowState::Running)], Vec::new());
        app.ui.terminal_width = 60;
        let rows = screen(&mut app, 60, 24);
        assert!(shows(&rows, "flow-live"), "{rows:#?}");
        assert!(!shows(&rows, "label b"), "{rows:#?}");
        key(&mut app, KeyCode::Enter);
        let rows = screen(&mut app, 60, 24);
        assert!(shows(&rows, "label b"), "{rows:#?}");
        key(&mut app, KeyCode::Esc);
        let rows = screen(&mut app, 60, 24);
        assert!(!shows(&rows, "label b"), "{rows:#?}");
        assert!(on_screen(&app));
    }

    #[test]
    fn entering_a_run_lands_on_the_running_agent_and_enter_opens_it() {
        let mut app = opened(vec![run("live", WorkflowState::Running)], Vec::new());
        key(&mut app, KeyCode::Enter);
        assert_eq!(app.ui.workflows.focus, Focus::Detail);
        assert_eq!(
            app.ui.workflows.cursor,
            Some(Cursor::Agent("b".to_string()))
        );
        key(&mut app, KeyCode::Enter);
        assert_eq!(
            app.state.route.data,
            Route::Session {
                session_id: "ses_b".to_string(),
                prompt: None
            }
        );
    }

    #[test]
    fn space_shows_an_agents_prompt_and_result() {
        let mut app = opened(vec![run("live", WorkflowState::Running)], Vec::new());
        key(&mut app, KeyCode::Enter);
        key(&mut app, KeyCode::Up);
        key(&mut app, KeyCode::Up);
        assert_eq!(
            app.ui.workflows.cursor,
            Some(Cursor::Agent("a".to_string()))
        );
        let rows = screen(&mut app, 120, 30);
        assert!(!shows(&rows, "result of a"), "{rows:#?}");
        key(&mut app, KeyCode::Char(' '));
        let rows = screen(&mut app, 120, 30);
        assert!(shows(&rows, "prompt of a"), "{rows:#?}");
        assert!(shows(&rows, "result of a"), "{rows:#?}");
    }

    #[test]
    fn a_big_finished_phase_folds_away_and_enter_unfolds_it() {
        let mut big = run("r", WorkflowState::Running);
        big.agents = (0..5)
            .map(|index| agent(&format!("p{index}"), "Plan", WorkflowState::Completed))
            .chain([agent("b", "Work", WorkflowState::Running)])
            .collect();
        let mut app = opened(vec![big], Vec::new());
        let rows = screen(&mut app, 120, 30);
        assert!(!shows(&rows, "label p0"), "{rows:#?}");
        assert!(shows(&rows, "5 done"), "{rows:#?}");
        key(&mut app, KeyCode::Enter);
        // From the running agent up to the Work phase, then the Plan phase.
        key(&mut app, KeyCode::Up);
        key(&mut app, KeyCode::Up);
        assert_eq!(
            app.ui.workflows.cursor,
            Some(Cursor::Phase("Plan".to_string()))
        );
        key(&mut app, KeyCode::Enter);
        let rows = screen(&mut app, 120, 30);
        assert!(shows(&rows, "label p0"), "{rows:#?}");
    }

    #[test]
    fn a_failed_agent_keeps_its_phase_open() {
        let mut failed = run("r", WorkflowState::Failed);
        failed.phases = vec![phase("Work", WorkflowState::Failed)];
        failed.agents = (0..5)
            .map(|index| agent(&format!("p{index}"), "Work", WorkflowState::Completed))
            .chain([agent("bad", "Work", WorkflowState::Failed)])
            .collect();
        let mut app = opened(vec![failed], Vec::new());
        let rows = screen(&mut app, 120, 30);
        assert!(shows(&rows, "label bad"), "{rows:#?}");
    }

    #[test]
    fn enter_on_a_saved_workflow_asks_for_its_arguments_and_starts_it() {
        let mut app = opened(Vec::new(), vec![saved("mine", WorkflowSource::Project)]);
        key(&mut app, KeyCode::Enter);
        assert!(matches!(
            app.ui.dialogs.top_kind(),
            Some(PendingDialog::WorkflowRun { name: Some(name), .. }) if name == "mine"
        ));
        let rows = screen(&mut app, 120, 30);
        assert!(shows(&rows, "Run mine"), "{rows:#?}");
        update(&mut app, Msg::Paste("what changed".to_string()));
        let effects = key(&mut app, KeyCode::Enter);
        assert!(
            effects.iter().any(|effect| matches!(
                effect,
                Effect::WorkflowStart { name: Some(name), script: None, args }
                    if name == "mine" && args == &json!("what changed")
            )),
            "{effects:?}"
        );
    }

    #[test]
    fn running_again_offers_the_same_arguments() {
        let mut app = opened(
            vec![run("old", WorkflowState::Completed)],
            vec![saved("flow-old", WorkflowSource::Project)],
        );
        key(&mut app, KeyCode::Char('r'));
        assert!(matches!(
            app.ui.dialogs.top_kind(),
            Some(PendingDialog::WorkflowRun { name: Some(_), script: None, args, .. })
                if args == r#"{"question":"why"}"#
        ));
        let frame = app.ui.dialogs.top().unwrap();
        assert_eq!(frame.input, r#"{"question":"why"}"#);
    }

    #[test]
    fn arguments_are_json_when_they_parse_and_text_when_not() {
        assert_eq!(parse_args(""), Value::Null);
        assert_eq!(parse_args(r#"{"a":1}"#), json!({ "a": 1 }));
        assert_eq!(parse_args("what changed"), json!("what changed"));
    }

    #[test]
    fn stopping_asks_first() {
        let mut app = opened(vec![run("live", WorkflowState::Running)], Vec::new());
        let effects = key(&mut app, KeyCode::Char('s'));
        assert!(effects.is_empty(), "{effects:?}");
        assert!(matches!(
            app.ui.dialogs.top_kind(),
            Some(PendingDialog::WorkflowStop { .. })
        ));
        // The default button is Cancel.
        let effects = key(&mut app, KeyCode::Enter);
        assert!(effects.is_empty(), "{effects:?}");
        key(&mut app, KeyCode::Char('s'));
        key(&mut app, KeyCode::Tab);
        let effects = key(&mut app, KeyCode::Enter);
        assert!(
            effects.iter().any(
                |effect| matches!(effect, Effect::WorkflowAbort { run_id } if run_id == "live")
            ),
            "{effects:?}"
        );
    }

    #[test]
    fn a_finished_run_cannot_be_stopped() {
        let mut app = opened(vec![run("old", WorkflowState::Completed)], Vec::new());
        key(&mut app, KeyCode::Char('s'));
        assert!(app.ui.dialogs.is_empty());
    }

    #[test]
    fn only_project_and_user_workflows_can_be_edited_or_deleted() {
        let mut app = opened(
            Vec::new(),
            vec![
                saved("builtin", WorkflowSource::Builtin),
                saved("mine", WorkflowSource::Project),
            ],
        );
        assert_eq!(
            app.ui.workflows.selected,
            Some(Selected::Saved("builtin".to_string()))
        );
        assert!(key(&mut app, KeyCode::Char('e')).is_empty());
        key(&mut app, KeyCode::Char('d'));
        assert!(app.ui.dialogs.is_empty());
        key(&mut app, KeyCode::Down);
        let effects = key(&mut app, KeyCode::Char('e'));
        assert!(
            effects.iter().any(|effect| matches!(
                effect,
                Effect::WorkflowEdit { name, overwrite: true, .. } if name == "mine"
            )),
            "{effects:?}"
        );
        key(&mut app, KeyCode::Char('d'));
        assert!(matches!(
            app.ui.dialogs.top_kind(),
            Some(PendingDialog::WorkflowDelete { name }) if name == "mine"
        ));
    }

    #[test]
    fn a_new_workflow_needs_a_valid_name_then_opens_the_editor() {
        let mut app = opened(Vec::new(), Vec::new());
        let rows = screen(&mut app, 120, 30);
        assert!(shows(&rows, "No workflows yet"), "{rows:#?}");
        key(&mut app, KeyCode::Char('n'));
        update(&mut app, Msg::Paste("bad name".to_string()));
        let effects = key(&mut app, KeyCode::Enter);
        assert!(effects.is_empty(), "{effects:?}");
        assert!(matches!(
            app.ui.dialogs.top_kind(),
            Some(PendingDialog::WorkflowNew)
        ));
        let frame = app.ui.dialogs.top_mut().unwrap();
        frame.input.clear();
        update(&mut app, Msg::Paste("audit-routes".to_string()));
        let effects = key(&mut app, KeyCode::Enter);
        assert!(
            effects.iter().any(|effect| matches!(
                effect,
                Effect::WorkflowEdit { name, overwrite: false, original, .. }
                    if name == "audit-routes" && original.contains("audit-routes")
            )),
            "{effects:?}"
        );
    }

    #[test]
    fn keys_the_screen_does_not_use_reach_the_keymap() {
        let mut app = opened(vec![run("live", WorkflowState::Running)], Vec::new());
        update(
            &mut app,
            Msg::Key(KeyEvent::new(KeyCode::Char('p'), KeyModifiers::CONTROL)),
        );
        assert!(matches!(
            app.ui.dialogs.top_kind(),
            Some(PendingDialog::CommandPalette)
        ));
    }

    #[test]
    fn typing_here_does_not_reach_the_hidden_prompt() {
        let mut app = opened(vec![run("live", WorkflowState::Running)], Vec::new());
        key(&mut app, KeyCode::Char('z'));
        update(&mut app, Msg::Paste("pasted".to_string()));
        assert_eq!(app.ui.prompt.input(), "");
    }

    #[test]
    fn a_finished_run_is_announced_once_and_not_while_watching_it() {
        let mut app = app();
        apply(&mut app, Ok(vec![run("r", WorkflowState::Running)]), None);
        let effects = apply(&mut app, Ok(vec![run("r", WorkflowState::Failed)]), None);
        assert!(
            effects
                .iter()
                .any(|effect| matches!(effect, Effect::Attention { message, .. } if message == "Workflow flow-r failed")),
            "{effects:?}"
        );
        assert_eq!(
            app.ui.toasts.last().unwrap().message,
            "Workflow flow-r failed"
        );
        app.ui.toasts.clear();
        assert!(apply(&mut app, Ok(vec![run("r", WorkflowState::Failed)]), None).is_empty());

        let mut app = opened(vec![run("r", WorkflowState::Running)], Vec::new());
        apply(&mut app, Ok(vec![run("r", WorkflowState::Completed)]), None);
        assert!(app.ui.toasts.is_empty());
    }

    #[test]
    fn polling_is_quick_while_running_slow_on_screen_and_off_otherwise() {
        let mut app = app();
        assert!(on_tick(&mut app, 0).is_none(), "nothing to watch");
        apply(&mut app, Ok(vec![run("r", WorkflowState::Running)]), None);
        assert!(on_tick(&mut app, 100).is_some());
        assert!(on_tick(&mut app, 200).is_none(), "one in flight");
        apply(&mut app, Ok(vec![run("r", WorkflowState::Running)]), None);
        assert!(on_tick(&mut app, 200).is_none(), "not due yet");
        assert!(on_tick(&mut app, 100 + ACTIVE_POLL_MS).is_some());
        apply(&mut app, Ok(vec![run("r", WorkflowState::Completed)]), None);
        assert!(on_tick(&mut app, 100_000).is_none(), "nothing running");
        open(&mut app, None);
        apply(&mut app, Ok(vec![run("r", WorkflowState::Completed)]), None);
        let later = 100_000 + IDLE_POLL_MS;
        assert!(on_tick(&mut app, later).is_some());
    }

    #[test]
    fn a_failed_poll_keeps_the_last_snapshot_and_says_so() {
        let mut app = opened(vec![run("live", WorkflowState::Running)], Vec::new());
        apply(&mut app, Err("connection refused".to_string()), None);
        let rows = screen(&mut app, 120, 30);
        assert!(shows(&rows, "flow-live"), "{rows:#?}");
        assert!(shows(&rows, "connection refused"), "{rows:#?}");
    }

    #[test]
    fn a_session_shows_its_running_workflows() {
        let mut app = app();
        apply(
            &mut app,
            Ok(vec![
                run("live", WorkflowState::Running),
                run("old", WorkflowState::Completed),
            ]),
            None,
        );
        let theme = crate::ui::session::test_theme();
        let text = |lines: Vec<Line<'static>>| -> Vec<String> {
            lines
                .iter()
                .map(|line| {
                    line.spans
                        .iter()
                        .map(|span| span.content.as_ref())
                        .collect()
                })
                .collect()
        };
        let docked = text(docked_lines(&app, &theme, "ses_parent"));
        assert_eq!(docked.len(), 2, "{docked:?}");
        assert!(docked[0].contains("ctrl+x w"), "{docked:?}");
        assert!(
            docked[1].contains("flow-live · Work · 2/4 agents"),
            "{docked:?}"
        );
        assert_eq!(text(sidebar_lines(&app, &theme, "ses_parent")).len(), 3);
        assert!(docked_lines(&app, &theme, "ses_other").is_empty());
    }

    #[test]
    fn no_screen_size_panics() {
        let mut app = opened(
            vec![run("live", WorkflowState::Running)],
            vec![saved("mine", WorkflowSource::Project)],
        );
        for (width, height) in [(1, 1), (5, 3), (20, 6), (40, 10), (90, 5), (200, 60)] {
            app.ui.terminal_width = width;
            screen(&mut app, width, height);
        }
    }
}
