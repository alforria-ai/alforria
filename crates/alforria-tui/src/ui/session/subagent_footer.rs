//! `routes/session/subagent-footer.tsx` — the child-session bottom bar
//! (M8.8): the agent label, sibling index, token usage, and the
//! parent/prev/next navigation hints.

use ratatui::layout::Rect;
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Padding, Paragraph, Widget};

use alforria_schema::session_v1::V1Message;

use crate::state::App;
use crate::ui::theme::Theme;

/// The footer's border + padding box height: padding 1/1 + one row.
pub const HEIGHT: u16 = 3;

/// `subagentInfo().label` (`subagent-footer.ts:17-21`): the
/// `s.title.match(/@(\w+) subagent/)` match, titlecased, else
/// `"Subagent"`.
fn label(title: &str) -> String {
    let bytes = title.as_bytes();
    for (index, _) in title.char_indices() {
        if bytes[index] != b'@' {
            continue;
        }
        let rest = &title[index + 1..];
        let word: String = rest
            .chars()
            .take_while(|c| c.is_ascii_alphanumeric() || *c == '_')
            .collect();
        if !word.is_empty() && rest[word.len()..].starts_with(" subagent") {
            return crate::ui::locale::titlecase(&word);
        }
    }
    "Subagent".to_string()
}

/// `subagentInfo()` index/total (`subagent-footer.ts:23-35`).
fn sibling_index(app: &App, session_id: &str) -> (usize, usize) {
    let Some(session) = app.state.sync.session(session_id) else {
        return (0, 0);
    };
    let Some(parent_id) = &session.parent_id else {
        return (0, 0);
    };
    let mut siblings: Vec<&alforria_schema::session_v1::V1SessionInfo> = app
        .state
        .sync
        .session
        .iter()
        .filter(|session| session.parent_id.as_deref() == Some(parent_id))
        .collect();
    siblings.sort_by_key(|session| session.time.created);
    let index = siblings
        .iter()
        .position(|session| session.id == session_id)
        .unwrap_or(0);
    (index + 1, siblings.len())
}

/// The usage cluster — last assistant message with `tokens.output > 0`
/// (`subagent-footer.ts:37-58`).
fn usage(app: &App, session_id: &str) -> Option<String> {
    let last = app.state.sync.message.get(session_id)?.iter().rev().find(
        |message| matches!(message, V1Message::Assistant { tokens, .. } if tokens.output > 0.0),
    )?;
    let V1Message::Assistant {
        tokens,
        model_id,
        provider_id,
        ..
    } = last
    else {
        return None;
    };
    let total =
        tokens.input + tokens.output + tokens.reasoning + tokens.cache.read + tokens.cache.write;
    if total <= 0.0 {
        return None;
    }
    let context_limit = app
        .state
        .sync
        .provider
        .iter()
        .find(|provider| provider.get("id").and_then(|v| v.as_str()) == Some(provider_id.as_str()))
        .and_then(|provider| provider.get("models"))
        .and_then(|models| models.get(model_id.as_str()))
        .and_then(|model| model.get("limit"))
        .and_then(|limit| limit.get("context"))
        .and_then(|limit| limit.as_f64());
    let counted = crate::ui::locale::number(total as i64);
    let context = match context_limit {
        Some(limit) if limit > 0.0 => {
            format!("{counted} ({}%)", (total / limit * 100.0).round() as i64)
        }
        _ => counted,
    };
    let cost = app
        .state
        .sync
        .session(session_id)
        .and_then(|session| session.cost)
        .unwrap_or(0.0);
    let mut parts = vec![context];
    if cost > 0.0 {
        parts.push(crate::ui::locale::usd(cost));
    }
    Some(parts.join(" · "))
}

/// `useCommandShortcut(name)` — the first alternative's first stroke.
fn shortcut(app: &App, keybind: &str) -> String {
    let Some(crate::keymap::bindings::BindingValue::Alternatives(alternatives)) =
        app.keymap.bindings.get(keybind)
    else {
        return String::new();
    };
    let Some(stroke) = alternatives.first().and_then(|first| first.first()) else {
        return String::new();
    };
    let mut parts: Vec<String> = Vec::new();
    if stroke.ctrl {
        parts.push("ctrl".to_string());
    }
    if stroke.meta {
        parts.push("alt".to_string());
    }
    parts.push(stroke.key.clone());
    parts.join("+")
}

/// The footer row — one styled line.
pub fn line(app: &App, theme: &Theme, session_id: &str) -> Line<'static> {
    let title = app
        .state
        .sync
        .session(session_id)
        .map(|session| session.title.clone())
        .unwrap_or_default();
    let (index, total) = sibling_index(app, session_id);
    let mut left: Vec<Span> = vec![Span::styled(
        label(&title),
        Style::new()
            .fg(theme.text.to_color())
            .add_modifier(Modifier::BOLD),
    )];
    if total > 0 {
        left.push(Span::styled(
            format!(" ({index} of {total})"),
            theme.text_muted.to_color(),
        ));
    }
    if let Some(usage) = usage(app, session_id) {
        left.push(Span::styled(
            format!(" {usage}"),
            theme.text_muted.to_color(),
        ));
    }
    let right = format!(
        "Parent {}   Prev {}   Next {}",
        shortcut(app, "session_parent"),
        shortcut(app, "session_child_cycle_reverse"),
        shortcut(app, "session_child_cycle"),
    );
    let left_len: usize = left.iter().map(|span| span.content.len()).sum();
    let mut spans = left;
    spans.push(Span::raw(" ".repeat(
        right.len().saturating_sub(1).saturating_sub(left_len) + 1,
    )));
    spans.push(Span::styled(right, theme.text.to_color()));
    Line::from(spans)
}

/// Render into the bottom-stack row — left border, panel background,
/// padding 1/1/2/1 (`subagent-footer.tsx:100-108`).
pub fn render(app: &App, frame: &mut ratatui::Frame, theme: &Theme, area: Rect, session_id: &str) {
    if area.height == 0 || area.width == 0 {
        return;
    }
    Paragraph::new(line(app, theme, session_id))
        .style(Style::new().bg(theme.background_panel.to_color()))
        .block(
            Block::new()
                .borders(ratatui::widgets::Borders::LEFT)
                .border_style(theme.border.to_color())
                .padding(Padding {
                    left: 2,
                    right: 1,
                    top: 1,
                    bottom: 1,
                }),
        )
        .render(area, frame.buffer_mut());
}

#[cfg(test)]
mod tests {
    use super::*;

    fn text(line: &Line) -> String {
        line.spans
            .iter()
            .map(|span| span.content.to_string())
            .collect()
    }

    #[test]
    fn label_extracts_the_agent_from_the_title() {
        // `\w` excludes `-`, so the hyphenated title falls back (TS parity).
        assert_eq!(label("@web-fetch subagent"), "Subagent");
        assert_eq!(label("@build subagent"), "Build");
        assert_eq!(label("Plain title"), "Subagent");
    }

    #[test]
    fn footer_line_renders_index_and_hints() {
        let mut app = App::new(
            crate::config::TuiConfig::default(),
            crate::state::Args::default(),
            None,
        );
        app.state.sync.session = vec![
            crate::ui::session::tests::session_info("ses_parent", "Main"),
            crate::ui::session::tests::session_info("ses_child", "@build subagent"),
        ];
        app.state.sync.session[1].parent_id = Some("ses_parent".into());
        app.state.sync.session[1].time.created = 2;
        app.state
            .route
            .navigate(crate::state::route::Route::Session {
                session_id: "ses_child".into(),
                prompt: None,
            });
        let theme = crate::ui::session::test_theme();
        let text = text(&line(&app, &theme, "ses_child"));
        assert!(text.contains("Build (1 of 1)"), "{text}");
        assert!(text.contains("Parent up"), "{text}");
        assert!(text.contains("Prev left"), "{text}");
        assert!(text.contains("Next right"), "{text}");
    }
}
