//! `ui/` — ratatui views. Each route renders into the full frame; redraws
//! are full-frame on every [`crate::state::App`] version bump.

pub mod dialogs;
pub mod diff;
pub mod home;
pub mod locale;
pub mod markdown;
pub mod session;
pub mod textarea;
pub mod theme;

pub use logo::{LOGO, SPINNER_FRAMES};

mod logo;

use ratatui::layout::Rect;
use ratatui::style::Style;
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Paragraph, Widget, Wrap};

use crate::state::route::Route;
use crate::state::{App, Toast, ToastVariant};

/// `SPINNER_FRAMES` interval (`component/spinner.tsx`).
const SPINNER_INTERVAL_MS: u64 = 80;

/// `view(app, frame)` (spec §2.1): a pure function of the state tree.
pub fn view(app: &mut App, frame: &mut ratatui::Frame) {
    let theme = app
        .ui
        .theme
        .resolve(&app.state.kv)
        .expect("builtin theme resolves");
    let area = frame.area();
    Block::new()
        .style(Style::new().bg(theme.background.to_color()))
        .render(area, frame.buffer_mut());
    match &app.state.route.data {
        Route::Home { .. } => home::render(app, frame, &theme, area),
        Route::Session { .. } => session::render(app, frame, &theme, area),
        Route::Plugin { id, .. } => render_plugin_missing(frame, &theme, area, id),
    }
    // The dialog stack overlays the route (`ui/dialog.tsx`); toasts
    // stay on top (their container is rendered after the dialog root).
    dialogs::render(app, frame, &theme, area);
    if let Some(toast) = app.ui.toasts.last() {
        render_toast(frame, &theme, area, toast);
    }
    if app.ui.startup_loading.visible() {
        render_startup_loading(app, frame, &theme, area);
    }
}

/// `PluginRouteMissing` (`component/plugin-route-missing.tsx`) — the plugin
/// runtime is a recorded non-goal (spec §6 N2).
fn render_plugin_missing(frame: &mut ratatui::Frame, theme: &theme::Theme, area: Rect, id: &str) {
    let [target] = ratatui::layout::Layout::vertical([ratatui::layout::Constraint::Fill(1)])
        .flex(ratatui::layout::Flex::Center)
        .areas(area);
    Paragraph::new(vec![
        Line::styled(
            format!("Unknown plugin route: {id}"),
            theme.warning.to_color(),
        ),
        Line::raw(""),
        Line::styled("go home", theme.text.to_color()),
    ])
    .render(target, frame.buffer_mut());
}

/// `ui/toast.tsx` — one current toast, top-right at `top: 2` / `right: 2`.
/// (TODO(M8.8): duration-based dismissal.)
fn render_toast(frame: &mut ratatui::Frame, theme: &theme::Theme, area: Rect, toast: &Toast) {
    let color = |variant: ToastVariant| match variant {
        ToastVariant::Info => theme.info,
        ToastVariant::Success => theme.success,
        ToastVariant::Warning => theme.warning,
        ToastVariant::Error => theme.error,
    };
    let max_width = std::cmp::min(60, area.width.saturating_sub(6));
    let rows = toast.title.as_ref().map_or(1, |_| 2);
    let columns = ratatui::layout::Layout::horizontal([
        ratatui::layout::Constraint::Fill(1),
        ratatui::layout::Constraint::Length(max_width),
        ratatui::layout::Constraint::Length(2),
    ])
    .split(area);
    let target = Rect {
        x: columns[1].x,
        y: area.y + 2,
        width: columns[1].width,
        height: rows as u16,
    };
    let mut lines: Vec<Line> = Vec::new();
    if let Some(title) = &toast.title {
        lines.push(Line::styled(
            title.clone(),
            Style::new()
                .fg(theme.text.to_color())
                .add_modifier(ratatui::style::Modifier::BOLD),
        ));
    }
    lines.push(Line::styled(toast.message.clone(), theme.text.to_color()));
    Paragraph::new(lines)
        .wrap(Wrap { trim: false })
        .style(Style::new().bg(theme.background_panel.to_color()))
        .block(
            Block::new()
                .borders(ratatui::widgets::Borders::LEFT | ratatui::widgets::Borders::RIGHT)
                .border_style(color(toast.variant).to_color())
                .padding(ratatui::widgets::Padding::horizontal(2)),
        )
        .render(target, frame.buffer_mut());
}

/// `StartupLoading` overlay (`component/startup-loading.tsx`): centered on
/// `bottom: 1` once the 500 ms wait elapses.
fn render_startup_loading(app: &App, frame: &mut ratatui::Frame, theme: &theme::Theme, area: Rect) {
    let text = app.ui.startup_loading.text();
    let animations = app
        .state
        .kv
        .get_bool(crate::state::kv::keys::ANIMATIONS_ENABLED, true);
    let frame_index = (app.ui.tick_ms / SPINNER_INTERVAL_MS) as usize;
    let label = if animations {
        format!(
            "{} {text}",
            logo::SPINNER_FRAMES[frame_index % logo::SPINNER_FRAMES.len()]
        )
    } else {
        format!("⋯ {text}")
    };
    let width = label.chars().count() as u16 + 2;
    let [row] = ratatui::layout::Layout::vertical([
        ratatui::layout::Constraint::Fill(1),
        ratatui::layout::Constraint::Length(1),
        ratatui::layout::Constraint::Length(1),
    ])
    .areas(area);
    let horizontal = ratatui::layout::Layout::horizontal([
        ratatui::layout::Constraint::Fill(1),
        ratatui::layout::Constraint::Length(width),
        ratatui::layout::Constraint::Fill(1),
    ])
    .split(row);
    Paragraph::new(Span::styled(
        label,
        Style::new()
            .fg(theme.text_muted.to_color())
            .bg(theme.background_panel.to_color()),
    ))
    .render(
        Rect {
            x: horizontal[1].x,
            y: row.y,
            width,
            height: 1,
        },
        frame.buffer_mut(),
    );
}
