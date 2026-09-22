//! `routes/session/sidebar.tsx` — the 42-column right rail (M8.8):
//! title, workspace label, share url, the `sidebar_content` slot
//! (`feature-plugins/sidebar/{context,mcp,lsp,todo,files}.tsx`) and
//! the `sidebar_footer` slot (`feature-plugins/sidebar/footer.tsx`).

use ratatui::layout::Rect;
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Paragraph, Widget};
use serde_json::Value;

use crate::state::App;
use crate::ui::locale::{abbreviate_home, number, usd};
use crate::ui::theme::Theme;

use super::SIDEBAR_WIDTH;

/// The content box width (`sidebar.tsx:48`): 42 − padding 2/2 − the
/// inner `paddingRight={1}`.
pub const CONTENT_WIDTH: usize = (SIDEBAR_WIDTH - 4 - 1) as usize;

/// The footer/card box width: 42 − padding 2/2 (`sidebar.tsx:89`).
pub const CARD_WIDTH: usize = (SIDEBAR_WIDTH - 4) as usize;

/// The getting-started card copy column: the card padding 2/2, the ⬖
/// icon and its gap (`footer.tsx:42-47`).
pub const CARD_TEXT_WIDTH: usize = CARD_WIDTH - 4 - 2;

/// The `dismissed_getting_started` kv key (`feature-plugins/sidebar/footer.tsx:17`).
const DISMISSED_GETTING_STARTED: &str = "dismissed_getting_started";

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

/// The `sidebar_content` slot — the five feature-plugin sections
/// joined by the `gap={1}` of the content box (`sidebar.tsx:48-86`).
pub fn section_lines(app: &App, theme: &Theme, session_id: &str) -> Vec<Line<'static>> {
    let sections = vec![
        context_lines(app, theme, session_id),
        mcp_lines(app, theme),
        lsp_lines(app, theme),
        todo_lines(app, theme, session_id),
        files_lines(app, theme, session_id),
    ];
    let joined = sections.into_iter().filter(|lines| !lines.is_empty());
    let mut rows: Vec<Line> = Vec::new();
    for section in joined {
        if !rows.is_empty() {
            rows.push(Line::raw(""));
        }
        rows.extend(section);
    }
    rows
}

/// `internal:sidebar-context` (`feature-plugins/sidebar/context.tsx`)
/// — token/cost usage of the last assistant message.
fn context_lines(app: &App, theme: &Theme, session_id: &str) -> Vec<Line<'static>> {
    let cost = app
        .state
        .sync
        .session(session_id)
        .and_then(|session| session.cost)
        .unwrap_or(0.0);
    let mut tokens = 0.0;
    let mut percent: Option<i64> = None;
    if let Some(messages) = app.state.sync.message.get(session_id) {
        for message in messages.iter().rev() {
            let alforria_schema::session_v1::V1Message::Assistant {
                tokens: step,
                provider_id,
                model_id,
                ..
            } = message
            else {
                continue;
            };
            if step.output <= 0.0 {
                continue;
            }
            tokens = step.input + step.output + step.reasoning + step.cache.read + step.cache.write;
            let limit = app
                .state
                .sync
                .provider
                .iter()
                .find(|item| item.get("id").and_then(Value::as_str) == Some(provider_id))
                .and_then(|item| item.get("models"))
                .and_then(|models| models.get(model_id.as_str()))
                .and_then(|model| model.get("limit"))
                .and_then(|limit| limit.get("context"))
                .and_then(Value::as_f64);
            if let Some(limit) = limit {
                if limit > 0.0 {
                    percent = Some((tokens / limit * 100.0).round() as i64);
                }
            }
            break;
        }
    }
    vec![
        Line::styled(
            "Context",
            Style::new()
                .fg(theme.text.to_color())
                .add_modifier(Modifier::BOLD),
        ),
        Line::styled(
            format!("{} tokens", number(tokens as i64)),
            theme.text_muted.to_color(),
        ),
        Line::styled(
            format!("{}% used", percent.unwrap_or(0)),
            theme.text_muted.to_color(),
        ),
        Line::styled(format!("{} spent", usd(cost)), theme.text_muted.to_color()),
    ]
}

/// `internal:sidebar-mcp` (`feature-plugins/sidebar/mcp.tsx`).
fn mcp_lines(app: &App, theme: &Theme) -> Vec<Line<'static>> {
    if app.state.sync.mcp.is_empty() {
        return Vec::new();
    }
    let mut rows = vec![Line::styled(
        "MCP",
        Style::new()
            .fg(theme.text.to_color())
            .add_modifier(Modifier::BOLD),
    )];
    for (name, item) in &app.state.sync.mcp {
        let status = item.get("status").and_then(Value::as_str).unwrap_or("");
        let color = match status {
            "connected" => theme.success,
            "failed" | "needs_client_registration" => theme.error,
            "needs_auth" => theme.warning,
            "disabled" => theme.text_muted,
            _ => theme.text_muted,
        };
        let error = item.get("error").and_then(Value::as_str).unwrap_or("");
        let status_text = match status {
            "connected" => "Connected".to_string(),
            "failed" => error.to_string(),
            "disabled" => "Disabled".to_string(),
            "needs_auth" => "Needs auth".to_string(),
            "needs_client_registration" => "Needs client ID".to_string(),
            other => other.to_string(),
        };
        rows.push(Line::from(vec![
            Span::styled("•", Style::new().fg(color.to_color())),
            Span::styled(format!(" {name} "), theme.text.to_color()),
            Span::styled(status_text, theme.text_muted.to_color()),
        ]));
    }
    rows
}

/// `internal:sidebar-lsp` (`feature-plugins/sidebar/lsp.tsx`).
fn lsp_lines(app: &App, theme: &Theme) -> Vec<Line<'static>> {
    let mut rows = vec![Line::styled(
        "LSP",
        Style::new()
            .fg(theme.text.to_color())
            .add_modifier(Modifier::BOLD),
    )];
    if app.state.sync.lsp.is_empty() {
        let off = !is_truthy(app.state.sync.config.get("lsp"));
        rows.push(Line::styled(
            if off {
                "LSPs are disabled"
            } else {
                "LSPs will activate as files are read"
            },
            theme.text_muted.to_color(),
        ));
        return rows;
    }
    for item in &app.state.sync.lsp {
        let status = item.get("status").and_then(Value::as_str);
        let dot = if status == Some("connected") {
            theme.success
        } else {
            theme.error
        };
        let id = item.get("id").and_then(Value::as_str).unwrap_or_default();
        let root = item.get("root").and_then(Value::as_str).unwrap_or_default();
        rows.push(Line::from(vec![
            Span::styled("•", Style::new().fg(dot.to_color())),
            Span::styled(format!(" {id} {root}"), theme.text_muted.to_color()),
        ]));
    }
    rows
}

/// `!config.lsp` (`sidebar/lsp.tsx:11`) — JS falsiness on a JSON value.
fn is_truthy(value: Option<&Value>) -> bool {
    match value {
        None | Some(Value::Null) => false,
        Some(Value::Bool(bool)) => *bool,
        Some(Value::Number(number)) => number.as_f64().unwrap_or(0.0) != 0.0,
        Some(Value::String(text)) => !text.is_empty(),
        Some(_) => true,
    }
}

/// `internal:sidebar-todo` (`feature-plugins/sidebar/todo.tsx`) — the
/// collapse toggle needs mouse state; the list renders expanded.
fn todo_lines(app: &App, theme: &Theme, session_id: &str) -> Vec<Line<'static>> {
    let Some(list) = app.state.sync.todo.get(session_id) else {
        return Vec::new();
    };
    if list.is_empty() || list.iter().all(|todo| todo.status == "completed") {
        return Vec::new();
    }
    let mut rows = vec![Line::styled(
        "Todo",
        Style::new()
            .fg(theme.text.to_color())
            .add_modifier(Modifier::BOLD),
    )];
    for todo in list {
        let mark = if todo.status == "completed" {
            "[✓] "
        } else if todo.status == "in_progress" {
            "[•] "
        } else {
            "[ ] "
        };
        let color = if todo.status == "in_progress" {
            theme.warning
        } else {
            theme.text_muted
        };
        rows.push(Line::styled(
            format!("{mark}{}", todo.content),
            color.to_color(),
        ));
    }
    rows
}

/// `internal:sidebar-files` (`feature-plugins/sidebar/files.tsx`) —
/// the modified-file rows with `+additions/-deletions` counts.
fn files_lines(app: &App, theme: &Theme, session_id: &str) -> Vec<Line<'static>> {
    let Some(list) = app.state.sync.session_diff.get(session_id) else {
        return Vec::new();
    };
    if list.is_empty() {
        return Vec::new();
    }
    let mut rows = vec![Line::styled(
        "Modified Files",
        Style::new()
            .fg(theme.text.to_color())
            .add_modifier(Modifier::BOLD),
    )];
    for item in list {
        let (additions, deletions) = (item.additions, item.deletions);
        let mut counts = Vec::new();
        if additions != 0.0 {
            counts.push(format!("+{}", js_number(additions)));
        }
        if deletions != 0.0 {
            counts.push(format!("-{}", js_number(deletions)));
        }
        let joined = counts.join(" ");
        let file = item.file.clone().unwrap_or_default();
        let truncated = truncate_left(
            &file,
            (CONTENT_WIDTH as i64 - 1 - joined.chars().count() as i64).max(2) as usize,
        );
        let padding = CONTENT_WIDTH
            .saturating_sub(truncated.chars().count())
            .saturating_sub(joined.chars().count());
        let mut spans = vec![Span::styled(truncated, theme.text_muted.to_color())];
        spans.push(Span::raw(" ".repeat(padding)));
        if additions != 0.0 {
            spans.push(Span::styled(
                format!("+{}", js_number(additions)),
                theme.diff_added.to_color(),
            ));
        }
        if deletions != 0.0 {
            if additions != 0.0 {
                spans.push(Span::raw(" "));
            }
            spans.push(Span::styled(
                format!("-{}", js_number(deletions)),
                theme.diff_removed.to_color(),
            ));
        }
        rows.push(Line::from(spans));
    }
    rows
}

/// `Locale.truncateLeft` (`util/locale.ts:66-69`).
fn truncate_left(input: &str, len: usize) -> String {
    if input.chars().count() <= len {
        return input.to_string();
    }
    let suffix: String = input
        .chars()
        .rev()
        .take(len.saturating_sub(1))
        .collect::<Vec<_>>()
        .into_iter()
        .rev()
        .collect();
    format!("…{suffix}")
}

/// `{item.additions}` — JS renders integral floats without a fraction.
fn js_number(input: f64) -> String {
    if input.fract() == 0.0 {
        format!("{}", input as i64)
    } else {
        format!("{input}")
    }
}

/// The `sidebar_footer` slot (`feature-plugins/sidebar/footer.tsx`):
/// the dismissible getting-started card, the `parent/name` session
/// path row and the version wordmark, joined by `gap={1}`.
pub fn footer_lines(app: &App, theme: &Theme, session_id: &str) -> Vec<Line<'static>> {
    let mut rows: Vec<Line> = Vec::new();
    let card = getting_started_lines(app, theme);
    let has_card = !card.is_empty();
    rows.extend(card);
    if has_card {
        rows.push(Line::raw(""));
    }
    rows.push(session_path_line(app, theme, session_id));
    rows.push(Line::raw(""));
    rows.push(footer_line(theme));
    rows
}

/// The `⬖ Getting started` card — shown while no usable provider is
/// configured and `dismissed_getting_started` is unset. The `✕`
/// dismiss is mouse-driven in the reference; the Rust TUI has no
/// mouse input, so the card cannot be dismissed (recorded
/// divergence).
fn getting_started_lines(app: &App, theme: &Theme) -> Vec<Line<'static>> {
    let has_provider = app.state.sync.provider.iter().any(|item| {
        item.get("id").and_then(Value::as_str) != Some("opencode")
            || item
                .get("models")
                .and_then(Value::as_object)
                .map(|models| {
                    models.values().any(|model| {
                        model
                            .get("cost")
                            .and_then(|cost| cost.get("input"))
                            .and_then(Value::as_f64)
                            != Some(0.0)
                    })
                })
                .unwrap_or(false)
    });
    let dismissed = app.state.kv.get_bool(DISMISSED_GETTING_STARTED, false);
    if has_provider || dismissed {
        return Vec::new();
    }

    let card = |spans: Vec<Span<'static>>| card_line(theme, spans);
    let bold_text = Style::new()
        .fg(theme.text.to_color())
        .add_modifier(Modifier::BOLD);

    // The card box spans the full footer width; its padding 2/2, the
    // ⬖ icon and the gap leave CARD_TEXT_WIDTH columns for the copy.
    let gap = " ".repeat(CARD_TEXT_WIDTH - "Getting started".len() - 1);
    let mut lines = vec![card(Vec::new())];
    lines.push(card(vec![
        Span::raw("  ⬖ "),
        Span::styled("Getting started", bold_text),
        Span::raw(gap),
        Span::styled("✕", theme.text_muted.to_color()),
    ]));
    for text in [
        "alforria includes free models so you can start immediately.",
        "Connect from 75+ providers to use other models, including Claude, GPT, Gemini etc",
    ] {
        for (index, line) in word_wrap(text, CARD_TEXT_WIDTH).into_iter().enumerate() {
            let lead = if index == 0 { "  ⬖ " } else { "    " };
            lines.push(card(vec![
                Span::raw(lead),
                Span::styled(line, theme.text_muted.to_color()),
            ]));
        }
    }
    let gap = " ".repeat(CARD_TEXT_WIDTH - "Connect provider".len() - "/connect".len());
    lines.push(card(vec![
        Span::raw("  ⬖ "),
        Span::styled("Connect provider", theme.text.to_color()),
        Span::raw(gap),
        Span::styled("/connect", theme.text_muted.to_color()),
    ]));
    lines.push(card(Vec::new()));
    lines
}

/// One full-width card row: the copy padded out with the
/// `backgroundElement` card background.
fn card_line(theme: &Theme, spans: Vec<Span<'static>>) -> Line<'static> {
    let width: usize = spans.iter().map(|span| span.content.chars().count()).sum();
    let mut spans = spans;
    spans.push(Span::raw(" ".repeat(CARD_WIDTH.saturating_sub(width))));
    Line::from(spans).style(Style::new().bg(theme.background_element.to_color()))
}

/// The `parent/name` session path row.
fn session_path_line(app: &App, theme: &Theme, session_id: &str) -> Line<'static> {
    let session = app.state.sync.session(session_id);
    let session_directory = session.map(|session| session.directory.clone());
    let directory = session_directory
        .clone()
        .filter(|directory| !directory.is_empty())
        .or_else(|| {
            app.state
                .project
                .instance_path
                .directory
                .clone()
                .filter(|directory| !directory.is_empty())
        })
        .or_else(|| {
            std::env::current_dir()
                .ok()
                .map(|path| path.display().to_string())
        });
    let home = app
        .state
        .project
        .instance_path
        .home
        .clone()
        .filter(|home| !home.is_empty())
        .or_else(|| std::env::var("HOME").ok());
    let text = match directory {
        Some(directory) => {
            let out = abbreviate_home(&directory, home.as_deref().unwrap_or(""));
            let branch = (session_directory.is_some()
                && session_directory == app.state.project.instance_path.directory)
                .then(|| {
                    app.state
                        .sync
                        .vcs
                        .as_ref()
                        .and_then(|vcs| vcs.get("branch"))
                        .and_then(Value::as_str)
                        .map(str::to_string)
                })
                .flatten();
            match branch {
                Some(branch) => format!("{out}:{branch}"),
                None => out,
            }
        }
        None => String::new(),
    };
    let (parent, name) = match text.rfind('/') {
        Some(index) => (text[..index].to_string(), text[index + 1..].to_string()),
        None => (String::new(), text.clone()),
    };
    Line::from(vec![
        Span::styled(format!("{parent}/"), theme.text_muted.to_color()),
        Span::styled(name, theme.text.to_color()),
    ])
}

/// `wrapMode="word"` — greedy word wrap for the card copy.
fn word_wrap(input: &str, width: usize) -> Vec<String> {
    let mut lines = Vec::new();
    let mut current = String::new();
    for word in input.split(' ') {
        if current.is_empty() {
            current.push_str(word);
        } else if current.chars().count() + 1 + word.chars().count() <= width {
            current.push(' ');
            current.push_str(word);
        } else {
            lines.push(current);
            current = word.to_string();
        }
    }
    lines.push(current);
    lines
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
    // The title slot (`sidebar.tsx:56-83`) plus the `sidebar_content`
    // slot (`sidebar.tsx:85`), joined by the box `gap={1}`.
    let mut content = lines(app, theme, session_id);
    content.push(Line::raw(""));
    content.extend(section_lines(app, theme, session_id));
    let footer = footer_lines(app, theme, session_id);
    // The footer box `paddingTop={1}` (`sidebar.tsx:89`).
    let footer_top = inner.y + inner.height.saturating_sub(footer.len() as u16);
    Paragraph::new(content).render(
        Rect {
            height: footer_top.saturating_sub(inner.y),
            width: inner.width.saturating_sub(1),
            ..inner
        },
        frame.buffer_mut(),
    );
    Paragraph::new(footer).render(
        Rect {
            y: footer_top,
            height: inner
                .height
                .saturating_sub(footer_top.saturating_sub(inner.y)),
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

    #[test]
    fn getting_started_card_only_without_provider() {
        let mut app = app();
        let theme = crate::ui::session::test_theme();
        // No provider at all → the card shows.
        let rows = footer_lines(&app, &theme, "ses_1");
        let flat: Vec<String> = rows.iter().map(|row| text(row)).collect();
        assert!(
            flat.iter().any(|row| row.contains("Getting started")),
            "{flat:?}"
        );
        assert!(
            flat.iter().any(|row| row.contains("Connect provider")),
            "{flat:?}"
        );
        // A non-opencode provider hides the card.
        app.state.sync.provider = vec![serde_json::json!({ "id": "anthropic" })];
        let rows = footer_lines(&app, &theme, "ses_1");
        assert!(
            !rows.iter().any(|row| text(row).contains("Getting started")),
            "{rows:?}"
        );
        // The opencode free models alone do not count as a usable
        // provider — the card stays.
        app.state.sync.provider = vec![serde_json::json!({
            "id": "opencode",
            "models": { "free": { "cost": { "input": 0 } } },
        })];
        let rows = footer_lines(&app, &theme, "ses_1");
        assert!(
            rows.iter().any(|row| text(row).contains("Getting started")),
            "{rows:?}"
        );
        // A paid model behind the opencode provider id counts.
        app.state.sync.provider = vec![serde_json::json!({
            "id": "opencode",
            "models": { "paid": { "cost": { "input": 5 } } },
        })];
        let rows = footer_lines(&app, &theme, "ses_1");
        assert!(
            !rows.iter().any(|row| text(row).contains("Getting started")),
            "{rows:?}"
        );
        // The dismissed_getting_started kv flag hides the card.
        app.state.sync.provider = Vec::new();
        app.state
            .kv
            .set(DISMISSED_GETTING_STARTED, serde_json::Value::Bool(true));
        let rows = footer_lines(&app, &theme, "ses_1");
        assert!(
            !rows.iter().any(|row| text(row).contains("Getting started")),
            "{rows:?}"
        );
    }

    #[test]
    fn footer_path_row_abbreviates_home() {
        let mut app = app();
        app.state.sync.session[0].directory = "/home/jon/wt-theme".into();
        app.state.project.instance_path.home = Some("/home/jon".into());
        let theme = crate::ui::session::test_theme();
        let rows = footer_lines(&app, &theme, "ses_1");
        let flat: Vec<String> = rows.iter().map(|row| text(row)).collect();
        let path = flat
            .iter()
            .rev()
            .find(|row| row.ends_with("wt-theme"))
            .expect("path row");
        assert!(path.starts_with("~/"), "{path}");
    }

    #[test]
    fn content_sections_render_context_lsp_and_files() {
        let mut app = app();
        app.state.sync.provider = vec![serde_json::json!({
            "id": "anthropic",
            "models": { "claude": { "limit": { "context": 1000 } } },
        })];
        app.state.sync.lsp = vec![
            serde_json::json!({ "id": "rust-analyzer", "root": "/wt", "status": "connected" }),
        ];
        app.state.sync.mcp.insert(
            "server".to_string(),
            serde_json::json!({ "status": "connected" }),
        );
        app.state.sync.todo.insert(
            "ses_1".to_string(),
            vec![alforria_schema::session_todo::TodoInfo {
                content: "port the sidebar".into(),
                status: "in_progress".into(),
                priority: "high".into(),
            }],
        );
        app.state.sync.session_diff.insert(
            "ses_1".to_string(),
            vec![alforria_schema::file_diff::SnapshotFileDiff {
                file: Some("crates/alforria-tui/src/lib.rs".into()),
                patch: None,
                additions: 5.0,
                deletions: 2.0,
                status: None,
            }],
        );
        let theme = crate::ui::session::test_theme();
        let rows = section_lines(&app, &theme, "ses_1");
        let flat: Vec<String> = rows.iter().map(|row| text(row)).collect();
        assert!(flat.iter().any(|row| row.contains("Context")), "{flat:?}");
        assert!(flat.iter().any(|row| row.contains("tokens")), "{flat:?}");
        assert!(flat.iter().any(|row| row.contains("MCP")), "{flat:?}");
        assert!(
            flat.iter().any(|row| row.contains("server Connected")),
            "{flat:?}"
        );
        assert!(
            flat.iter().any(|row| row.contains("rust-analyzer /wt")),
            "{flat:?}"
        );
        assert!(flat.iter().any(|row| row.contains("Todo")), "{flat:?}");
        assert!(
            flat.iter().any(|row| row.contains("[•] port the sidebar")),
            "{flat:?}"
        );
        assert!(
            flat.iter().any(|row| row.contains("Modified Files")),
            "{flat:?}"
        );
        assert!(flat.iter().any(|row| row.contains("+5 -2")), "{flat:?}");
    }
}
