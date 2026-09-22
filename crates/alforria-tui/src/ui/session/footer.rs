//! `routes/session/footer.tsx` — the session footer bar (M8.5).
//!
//! Dead code at the pinned TS commit (imported nowhere — see the M8.5
//! spec note): the port still renders it at the bottom of the session
//! column, per the spec.

use ratatui::layout::Rect;
use ratatui::style::Style;
use ratatui::text::{Line, Span};
use ratatui::widgets::Widget;
use serde_json::Value;

use crate::state::App;
use crate::ui::theme::{Rgba, Theme};

use super::directory_label;

fn mcp_status(value: &Value, status: &str) -> bool {
    value.get("status").and_then(Value::as_str) == Some(status)
}

/// The footer (`routes/session/footer.tsx:9-91`): directory on the
/// left, the status cluster right-aligned (`gap={2}` between items).
pub fn render(app: &App, frame: &mut ratatui::Frame, theme: &Theme, area: Rect, session_id: &str) {
    if area.width == 0 || area.height == 0 {
        return;
    }
    let connected = crate::state::connected(app);

    // The right-hand cluster: `[Vec<(text, fg)>]`.
    let mut segments: Vec<Vec<(String, Rgba)>> = Vec::new();
    if app.ui.footer_welcome {
        segments.push(vec![
            ("Get started ".to_string(), theme.text),
            ("/connect".to_string(), theme.text_muted),
        ]);
    } else if connected {
        let permission_count = app
            .state
            .sync
            .permission
            .get(session_id)
            .map(|requests| requests.len())
            .unwrap_or(0);
        if permission_count > 0 {
            segments.push(vec![(
                format!(
                    "△ {permission_count} Permission{}",
                    if permission_count > 1 { "s" } else { "" }
                ),
                theme.warning,
            )]);
        }
        let lsp_count = app.state.sync.lsp.len();
        segments.push(vec![
            (
                "•".to_string(),
                if lsp_count > 0 {
                    theme.success
                } else {
                    theme.text_muted
                },
            ),
            (format!(" {lsp_count} LSP"), theme.text),
        ]);
        let mcp_count = app
            .state
            .sync
            .mcp
            .values()
            .filter(|value| mcp_status(value, "connected"))
            .count();
        if mcp_count > 0 {
            let mcp_error = app
                .state
                .sync
                .mcp
                .values()
                .any(|value| mcp_status(value, "failed"));
            segments.push(vec![
                (
                    "⊙".to_string(),
                    if mcp_error {
                        theme.error
                    } else {
                        theme.success
                    },
                ),
                (format!(" {mcp_count} MCP"), theme.text),
            ]);
        }
        segments.push(vec![("/status".to_string(), theme.text_muted)]);
    }

    let left = directory_label(app);
    let mut right: Vec<Span> = Vec::new();
    for (index, segment) in segments.iter().enumerate() {
        if index > 0 {
            right.push(Span::raw("  "));
        }
        for (text, color) in segment {
            right.push(Span::styled(
                text.clone(),
                Style::new().fg(color.to_color()),
            ));
        }
    }

    let right_len: usize = right.iter().map(|span| span.content.len()).sum();
    let remaining = (area.width as usize)
        .saturating_sub(left.len())
        .saturating_sub(right_len);
    let mut spans = vec![Span::styled(
        left,
        Style::new().fg(theme.text_muted.to_color()),
    )];
    spans.push(Span::raw(" ".repeat(remaining.max(1))));
    spans.extend(right);

    Line::from(spans).render(area, frame.buffer_mut());
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    fn app() -> App {
        App::new(
            crate::config::TuiConfig::default(),
            crate::state::Args::default(),
            None,
        )
    }

    fn render_footer(app: &mut App, width: u16) -> String {
        let backend = ratatui::backend::TestBackend::new(width, 1);
        let mut terminal = ratatui::Terminal::new(backend).unwrap();
        let theme = app
            .ui
            .theme
            .resolve(&app.state.kv)
            .expect("builtin theme resolves");
        terminal
            .draw(|frame| {
                let area = frame.area();
                render(app, frame, &theme, area, "ses_a")
            })
            .unwrap();
        let mut text = String::new();
        for x in 0..width {
            text.push_str(terminal.backend().buffer()[(x, 0)].symbol());
        }
        text
    }

    #[test]
    fn renders_welcome_when_disconnected() {
        let mut app = app();
        app.ui.footer_welcome = true;
        let text = render_footer(&mut app, 80);
        assert!(text.contains("Get started /connect"), "{text}");
    }

    #[test]
    fn renders_connected_cluster() {
        let mut app = app();
        // A non-opencode provider → connected (`use-connected.tsx`).
        app.state.sync.provider = vec![json!({ "id": "anthropic" })];
        app.state.sync.lsp = vec![json!({}), json!({})];
        app.state
            .sync
            .mcp
            .insert("server".to_string(), json!({ "status": "connected" }));
        app.state.sync.permission.insert(
            "ses_a".to_string(),
            vec![alforria_schema::permission_v1::PermissionV1Request {
                id: "per_1".to_string(),
                session_id: "ses_a".to_string(),
                permission: "bash".to_string(),
                patterns: vec![],
                metadata: serde_json::Map::new(),
                always: vec![],
                tool: None,
            }],
        );
        let text = render_footer(&mut app, 80);
        assert!(text.contains("1 Permission"), "{text}");
        assert!(text.contains("2 LSP"), "{text}");
        assert!(text.contains("1 MCP"), "{text}");
        assert!(text.contains("/status"), "{text}");
    }

    #[test]
    fn renders_mcp_error_dot_on_failure() {
        let mut app = app();
        app.state.sync.provider = vec![json!({ "id": "anthropic" })];
        app.state
            .sync
            .mcp
            .insert("bad".to_string(), json!({ "status": "failed" }));
        app.state
            .sync
            .mcp
            .insert("good".to_string(), json!({ "status": "connected" }));
        let text = render_footer(&mut app, 80);
        // The failed server keeps the connected count at 1 with the
        // error-colored dot; the cluster is still right-aligned.
        assert!(text.contains("1 MCP"), "{text}");
        assert!(text.trim_end().ends_with("/status"), "{text}");
    }
}
