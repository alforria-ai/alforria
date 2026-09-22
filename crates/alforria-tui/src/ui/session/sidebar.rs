//! `routes/session/sidebar.tsx` — the 42-column right rail (M8.8):
//! title, workspace label, share url, version footer.

use ratatui::layout::Rect;
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Paragraph, Widget};

use crate::state::App;
use crate::ui::theme::Theme;

/// `InstallationVersion` — the build version stamp. The Rust port has no
/// bundled release channel, so the version stays at the "dev" mark the
/// debug dialog reports.
pub const INSTALLATION_VERSION: &str = "dev";

/// The workspace label dot color (`workspace-label.tsx:7-12`).
pub fn status_color(theme: &Theme, status: &str) -> crate::ui::theme::Rgba {
    match status {
        "connected" => theme.success,
        "error" => theme.error,
        _ => theme.text_muted,
    }
}

/// `WorkspaceLabel` (`component/workspace-label.tsx`): `● name (type)`
/// — the dot green on `connected`, red on `error`, muted otherwise. The
/// fallback renders the unknown workspace with the `error` status
/// (`sidebar.tsx:51-57`).
fn workspace_label(app: &App, theme: &Theme, workspace_id: &str) -> Line<'static> {
    let workspace = app
        .state
        .project
        .workspace
        .list
        .iter()
        .find(|workspace| {
            workspace.get("workspaceID").and_then(|v| v.as_str()) == Some(workspace_id)
                || workspace.get("id").and_then(|v| v.as_str()) == Some(workspace_id)
        })
        .cloned()
        .unwrap_or(serde_json::json!({ "type": "unknown", "name": workspace_id }));
    let status = app
        .state
        .project
        .workspace
        .status
        .get(workspace_id)
        .cloned()
        .unwrap_or_else(|| "error".to_string());
    Line::from(vec![
        Span::styled(
            "● ",
            Style::new().fg(status_color(theme, &status).to_color()),
        ),
        Span::styled(
            workspace
                .get("name")
                .and_then(|v| v.as_str())
                .unwrap_or_default()
                .to_string(),
            theme.text.to_color(),
        ),
        Span::styled(
            format!(
                " ({})",
                workspace
                    .get("type")
                    .and_then(|v| v.as_str())
                    .unwrap_or("unknown")
            ),
            theme.text_muted.to_color(),
        ),
    ])
}

/// The content rows — title, session id, workspace label, share url
/// (the `sidebar_title` slot, `sidebar.tsx:36-75`).
pub fn lines(app: &App, theme: &Theme, session_id: &str) -> Vec<Line<'static>> {
    let Some(session) = app.state.sync.session(session_id) else {
        return Vec::new();
    };
    let mut rows: Vec<Line> = Vec::new();
    rows.push(Line::from(Span::styled(
        session.title.clone(),
        Style::new()
            .fg(theme.text.to_color())
            .add_modifier(Modifier::BOLD),
    )));
    // `InstallationChannel !== "latest"` — the local build always shows
    // the session id.
    rows.push(Line::styled(
        session.id.clone(),
        theme.text_muted.to_color(),
    ));
    if let Some(workspace_id) = session.workspace_id.clone() {
        rows.push(workspace_label(app, theme, &workspace_id));
    }
    if let Some(share) = &session.share {
        rows.push(Line::styled(share.url.clone(), theme.text_muted.to_color()));
    }
    rows
}

/// The footer `• Alforria <version>` (`sidebar.tsx:81-95`) — green
/// bullet, two-tone bold wordmark (muted `Alfo` + `rria` in text),
/// muted version.
pub fn footer_line(theme: &Theme) -> Line<'static> {
    Line::from(vec![
        Span::styled("• ", Style::new().fg(theme.success.to_color())),
        Span::styled(
            "Alfo",
            Style::new()
                .fg(theme.text_muted.to_color())
                .add_modifier(Modifier::BOLD),
        ),
        Span::styled(
            "rria",
            Style::new()
                .fg(theme.text.to_color())
                .add_modifier(Modifier::BOLD),
        ),
        Span::styled(
            format!(" {INSTALLATION_VERSION}"),
            theme.text_muted.to_color(),
        ),
    ])
}

/// Render into the 42-wide rail: `backgroundPanel` fill, `paddingTop/1
/// paddingBottom/1 paddingLeft/2 paddingRight/2`, content top, footer
/// bottom (`sidebar.tsx:20-27`).
pub fn render(app: &App, frame: &mut ratatui::Frame, theme: &Theme, area: Rect, session_id: &str) {
    if area.width < 4 || area.height < 2 {
        return;
    }
    ratatui::widgets::Block::new()
        .style(Style::new().bg(theme.background_panel.to_color()))
        .render(area, frame.buffer_mut());
    let inner = Rect {
        x: area.x + 2,
        y: area.y + 1,
        width: area.width.saturating_sub(4),
        height: area.height.saturating_sub(2),
    };
    // The `gap={1}` between the title rows.
    let mut content: Vec<Line> = Vec::new();
    for (index, line) in lines(app, theme, session_id).into_iter().enumerate() {
        if index > 0 {
            content.push(Line::raw(""));
        }
        content.push(line);
    }
    let footer_height = 1;
    Paragraph::new(content).render(
        Rect {
            height: inner.height.saturating_sub(footer_height),
            ..inner
        },
        frame.buffer_mut(),
    );
    Paragraph::new(footer_line(theme)).render(
        Rect {
            y: inner.y + inner.height.saturating_sub(footer_height),
            height: footer_height,
            ..inner
        },
        frame.buffer_mut(),
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    fn app() -> App {
        let mut app = App::new(
            crate::config::TuiConfig::default(),
            crate::state::Args::default(),
            None,
        );
        app.state.sync.session = vec![crate::ui::session::tests::session_info(
            "ses_1",
            "Fix the build",
        )];
        app
    }

    fn text(line: &Line) -> String {
        line.spans
            .iter()
            .map(|span| span.content.to_string())
            .collect()
    }

    #[test]
    fn sidebar_rows_title_id_workspace_share() {
        let mut app = app();
        app.state.sync.session[0].workspace_id = Some("ws_main".into());
        app.state.sync.session[0].share = Some(alforria_schema::session_v1::V1SessionShare {
            url: "https://shr.test/1".into(),
        });
        app.state.project.workspace.list = vec![serde_json::json!({
            "id": "ws_main", "workspaceID": "ws_main",
            "type": "worktree", "name": "feature",
        })];
        app.state
            .project
            .workspace
            .status
            .insert("ws_main".into(), "connected".into());
        let theme = crate::ui::session::test_theme();
        let rows = lines(&app, &theme, "ses_1");
        let text: Vec<String> = rows.iter().map(|row| text(row)).collect();
        assert_eq!(
            text,
            vec![
                "Fix the build".to_string(),
                "ses_1".to_string(),
                "● feature (worktree)".to_string(),
                "https://shr.test/1".to_string(),
            ]
        );
        assert_eq!(status_color(&theme, "connected"), theme.success);
    }

    #[test]
    fn unknown_workspace_falls_back_to_error_status() {
        let mut app = app();
        app.state.sync.session[0].workspace_id = Some("ws_gone".into());
        let theme = crate::ui::session::test_theme();
        let rows = lines(&app, &theme, "ses_1");
        assert_eq!(text(&rows[2]), "● ws_gone (unknown)");
    }

    #[test]
    fn footer_wordmark() {
        let theme = crate::ui::session::test_theme();
        assert_eq!(text(&footer_line(&theme)), "• Alforria dev");
    }
}
