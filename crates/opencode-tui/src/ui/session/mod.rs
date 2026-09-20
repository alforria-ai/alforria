//! `routes/session/index.tsx` — the session view (M8.5): the row-flex
//! layout (transcript scrollbox + bottom stack + sidebar), the
//! per-session render context and the entry point
//! `render`.

pub mod footer;
pub mod parts;
pub mod transcript;

use ratatui::layout::Rect;
use ratatui::style::Style;
use ratatui::text::{Line, Span};
use ratatui::widgets::Widget;

use super::theme::{selected_foreground, Rgba, Theme};
use crate::state::route::Route;
use crate::state::{App, SessionScroll};
use crate::ui::theme::Mode as ThemeMode;

/// The width of the right rail (`routes/session/index.tsx:1338-1357`).
pub const SIDEBAR_WIDTH: u16 = 42;

/// `wide` — the `>120` sidebar boundary.
pub fn wide(app: &App) -> bool {
    app.ui.terminal_width > 120
}

/// The per-session render context (`routes/session/index.tsx:157-175`).
pub struct Ctx<'a> {
    pub app: &'a App,
    pub theme: &'a Theme,
    /// `contentWidth` (`session/index.tsx:278`).
    pub width: u16,
    pub session_id: &'a str,
}

impl<'a> Ctx<'a> {
    pub fn new(app: &'a App, theme: &'a Theme, session_id: &'a str, width: u16) -> Ctx<'a> {
        Ctx {
            app,
            theme,
            width,
            session_id,
        }
    }

    pub fn thinking_mode(&self) -> &'static str {
        match self
            .app
            .state
            .kv
            .get(
                crate::state::kv::keys::THINKING_MODE,
                serde_json::json!("hide"),
            )
            .as_str()
        {
            Some("show") => "show",
            _ => "hide",
        }
    }

    pub fn show_details(&self) -> bool {
        self.app
            .state
            .kv
            .get_bool(crate::state::kv::keys::TOOL_DETAILS_VISIBILITY, true)
    }

    pub fn show_generic_tool_output(&self) -> bool {
        self.app.state.kv.get_bool(
            crate::state::kv::keys::GENERIC_TOOL_OUTPUT_VISIBILITY,
            false,
        )
    }

    pub fn conceal(&self) -> bool {
        self.app.ui.conceal
    }

    pub fn diff_wrap_mode(&self) -> super::diff::WrapMode {
        match self
            .app
            .state
            .kv
            .get(
                crate::state::kv::keys::DIFF_WRAP_MODE,
                serde_json::json!("word"),
            )
            .as_str()
        {
            Some("none") => super::diff::WrapMode::None,
            _ => super::diff::WrapMode::Word,
        }
    }

    /// The `<Spinner>` frame (`component/spinner.tsx`) — `⋯` when
    /// animations are off.
    pub fn spin(&self) -> &'static str {
        let animations = self
            .app
            .state
            .kv
            .get_bool(crate::state::kv::keys::ANIMATIONS_ENABLED, true);
        if animations {
            let index = (self.app.ui.tick_ms / 80) as usize % crate::ui::SPINNER_FRAMES.len();
            crate::ui::SPINNER_FRAMES[index]
        } else {
            "⋯"
        }
    }

    /// `local.agent.color(name)` resolved against the theme.
    pub fn agent_color(&self, name: &str) -> Rgba {
        match self.app.state.local.agent_color(name, &self.app.state.sync) {
            crate::state::local::AgentColor::Hex(hex) => {
                Rgba::from_hex(&hex).unwrap_or(self.theme.secondary)
            }
            crate::state::local::AgentColor::Theme(key) => {
                self.theme.get(&key).unwrap_or(self.theme.secondary)
            }
        }
    }

    /// `selectedForeground(theme, color)` — the QUEUED tag fg.
    pub fn selected_foreground(&self, color: Rgba) -> Rgba {
        selected_foreground(self.theme, Some(color))
    }
}

/// The route sessionID.
pub fn route_session_id(app: &App) -> Option<&str> {
    match &app.state.route.data {
        Route::Session { session_id, .. } => Some(session_id),
        _ => None,
    }
}

/// `render(app, frame, theme, area)` — the session layout
/// (`session/index.tsx:1157-1361`): a row of [transcript column,
/// sidebar]; the column is a vertical stack of [transcript scrollbox,
/// bottom stack]; the footer bar sits below.
pub fn render(app: &mut App, frame: &mut ratatui::Frame, theme: &Theme, area: Rect) {
    let Some(session_id) = route_session_id(app).map(str::to_string) else {
        return;
    };
    if app.state.sync.session(&session_id).is_none() {
        // `<Show when={session()}>` renders nothing.
        return;
    }

    let wide = wide(app);
    // `sidebarVisible()` (`session/index.tsx:271-276`).
    let sidebar_visible = crate::command::sidebar_visible(app);
    let content_width = area
        .width
        .saturating_sub(if sidebar_visible { SIDEBAR_WIDTH } else { 0 })
        .saturating_sub(4);

    let row = ratatui::layout::Layout::horizontal([
        ratatui::layout::Constraint::Fill(1),
        ratatui::layout::Constraint::Length(if sidebar_visible { SIDEBAR_WIDTH } else { 0 }),
    ])
    .split(area);
    let column = row[0];
    let sidebar_area = row[1];

    // Row padding: paddingBottom={1} paddingLeft={2} paddingRight={2}.
    let column = Rect {
        x: column.x + 2,
        width: column.width.saturating_sub(4),
        y: column.y,
        height: column.height.saturating_sub(1),
    };

    // The bottom stack: permission > question > subagent footer > prompt
    // (TODO(M8.7): permission + question; TODO(M8.8): subagent footer;
    // TODO(M8.6): the prompt editor) + the footer bar.
    let prompt_height = 3;
    let footer_height = 1;
    let vertical = ratatui::layout::Layout::vertical([
        ratatui::layout::Constraint::Fill(1),
        ratatui::layout::Constraint::Length(prompt_height),
        ratatui::layout::Constraint::Length(footer_height),
    ])
    .split(column);

    transcript::render(app, frame, theme, vertical[0], &session_id, content_width);
    render_prompt_frame(app, frame, theme, vertical[1], content_width);
    footer::render(app, frame, theme, vertical[2], &session_id);

    if sidebar_visible && !sidebar_area.is_empty() {
        if !wide {
            // Dimmed backdrop (`RGBA.fromInts(0, 0, 0, 70)`) — approximated
            // by drawing the backdrop dark under the sidebar column.
            let dim = Rect {
                width: SIDEBAR_WIDTH,
                ..sidebar_area
            };
            ratatui::widgets::Block::new()
                .style(Style::new().bg(theme.background.to_color()))
                .render(dim, frame.buffer_mut());
        }
        // TODO(M8.8): the sidebar content (`routes/session/sidebar.tsx`).
        render_sidebar_placeholder(frame, theme, sidebar_area);
    }
}

fn render_sidebar_placeholder(frame: &mut ratatui::Frame, theme: &Theme, area: Rect) {
    let lines = vec![Line::from(Span::styled(
        " Side panel",
        Style::new().fg(theme.text_muted.to_color()),
    ))];
    ratatui::widgets::Paragraph::new(lines).render(area, frame.buffer_mut());
}

/// The prompt frame — the border + `backgroundElement` box of
/// `component/prompt/index.tsx:1352-1401`. The editor lands with
/// TODO(M8.6).
fn render_prompt_frame(
    app: &App,
    frame: &mut ratatui::Frame,
    theme: &Theme,
    area: Rect,
    width: u16,
) {
    let max_width = app.config.prompt_max_width(width).min(area.width);
    let centered = Rect {
        x: area.x + area.width.saturating_sub(max_width) / 2,
        width: max_width,
        ..area
    };
    ratatui::widgets::Paragraph::new(ratatui::text::Text::default())
        .block(
            ratatui::widgets::Block::new()
                .borders(ratatui::widgets::Borders::LEFT)
                .border_style(theme.border.to_color())
                .style(Style::new().bg(theme.background_element.to_color()))
                .padding(ratatui::widgets::Padding {
                    left: 2,
                    right: 2,
                    top: 1,
                    bottom: 0,
                }),
        )
        .render(centered, frame.buffer_mut());
}

/// Memoize the transcript geometry back into the scroll state so the
/// scroll commands can address it (the retained-scrollbox equivalent).
pub fn memo_scroll(
    scroll: &mut SessionScroll,
    session_id: &str,
    content_height: usize,
    viewport_height: usize,
    children: Vec<(String, usize)>,
) {
    if scroll.session.as_deref() != Some(session_id) {
        scroll.session = Some(session_id.to_string());
        scroll.sticky = true;
    }
    scroll.content_height = content_height;
    scroll.viewport_height = viewport_height;
    scroll.children = children;
    if scroll.sticky {
        scroll.y = scroll.max_y();
    } else {
        scroll.y = scroll.y.min(scroll.max_y());
        scroll.sticky = scroll.y >= scroll.max_y();
    }
}

/// `useDirectory()` (`context/directory.ts`) — the footer's left side.
pub fn directory_label(app: &App) -> String {
    let directory = app
        .state
        .project
        .instance_path
        .directory
        .clone()
        .unwrap_or_default();
    let result = super::locale::abbreviate_home(&directory, "");
    match app
        .state
        .sync
        .vcs
        .as_ref()
        .and_then(|vcs| vcs.get("branch"))
        .and_then(serde_json::Value::as_str)
    {
        Some(branch) => format!("{result}:{branch}"),
        None => result,
    }
}

/// The theme mode toggle used by the sidebar (kept for M8.8 parity).
pub fn theme_mode(app: &App) -> ThemeMode {
    app.ui.theme.mode
}

pub fn test_theme() -> Theme {
    let mut kv = crate::state::kv::Kv::in_memory();
    crate::ui::theme::ThemeStore::init(&mut kv, None)
        .resolve(&kv)
        .unwrap()
}
