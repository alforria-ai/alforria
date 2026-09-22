//! `component/dialog-mcp.tsx`, `dialog-theme-list.tsx`, `dialog-status.tsx`,
//! `dialog-skill.tsx`, `dialog-workspace-list.tsx` and the
//! workspace-set select.

use serde_json::Value;

use super::primitives::SelectOption;
use crate::state::App;
use crate::ui::dialogs::DialogFrame;
use crate::ui::theme::Theme;
use ratatui::style::Style;
use ratatui::text::Line;

/// `DialogMcp.options` (`dialog-mcp.tsx:24-46`) — sorted by name.
pub fn mcp_options(app: &App) -> Vec<SelectOption> {
    let mut names: Vec<(&String, &Value)> = app.state.sync.mcp.iter().collect();
    names.sort_by(|a, b| a.0.cmp(b.0));
    names
        .into_iter()
        .map(|(name, status)| {
            let description = status
                .get("status")
                .and_then(Value::as_str)
                .map(str::to_string)
                .unwrap_or_default();
            SelectOption::new(name.clone())
                .with_value(name.clone())
                .with_description(description)
                .with_footer(if app.state.local.mcp_is_enabled(&app.state.sync, name) {
                    "✓ Enabled".to_string()
                } else {
                    "○ Disabled".to_string()
                })
        })
        .collect()
}

/// `DialogThemeList.options` (`dialog-theme-list.tsx:8-13`).
pub fn theme_options(app: &App) -> Vec<SelectOption> {
    let _ = app;
    crate::ui::theme::all_themes()
        .into_iter()
        .map(|(name, _)| SelectOption::new(name.to_string()).with_value(name.to_string()))
        .collect()
}

/// `DialogSkill.options` (`dialog-skill.tsx:29-43`) — sourced from the
/// command list's skill entries (the `app.skills` endpoint is not part
/// of the server seam; recorded divergence).
pub fn skill_options(app: &App) -> Vec<SelectOption> {
    let skills: Vec<(String, String)> = app
        .state
        .sync
        .command
        .iter()
        .filter(|command| command.get("source").and_then(Value::as_str) == Some("skill"))
        .filter_map(|command| {
            let name = command.get("name").and_then(Value::as_str)?;
            let description = command
                .get("description")
                .and_then(Value::as_str)
                .map(|description| description.split_whitespace().collect::<Vec<_>>().join(" "));
            Some((name.to_string(), description.unwrap_or_default()))
        })
        .collect();
    let max_width = skills
        .iter()
        .map(|(name, _)| name.chars().count())
        .max()
        .unwrap_or(0);
    skills
        .into_iter()
        .map(|(name, description)| {
            let mut title = name.clone();
            let padding = max_width.saturating_sub(name.chars().count());
            title.push_str(&" ".repeat(padding));
            let option = SelectOption::new(title).with_value(name);
            if description.is_empty() {
                option
            } else {
                option.with_description(description)
            }
        })
        .collect()
}

/// `DialogWorkspaceList.options` (`dialog-workspace-list.tsx:34-66`).
pub fn workspace_options(app: &App, frame: &DialogFrame) -> Vec<SelectOption> {
    let mut workspaces = app.state.project.workspace.list.clone();
    workspaces.sort_by(|a, b| {
        a.get("name")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .cmp(b.get("name").and_then(Value::as_str).unwrap_or_default())
    });
    workspaces
        .into_iter()
        .map(|workspace| {
            let id = workspace
                .get("id")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string();
            let expanded = frame.expanded.contains(&id);
            let mut option = SelectOption::new(
                workspace
                    .get("name")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_string(),
            )
            .with_value(id)
            .with_footer(
                workspace
                    .get("type")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_string(),
            );
            if expanded {
                if let Some(directory) = workspace.get("directory").and_then(Value::as_str) {
                    option = option.with_description(directory);
                }
            }
            option
        })
        .collect()
}

/// `openWorkspaceSelect` — `none` + the existing workspaces
/// (`dialog-workspace-create.tsx:72-87`).
pub fn workspace_set_options(app: &App) -> Vec<SelectOption> {
    let mut options = vec![SelectOption::new("Keep current workspace").with_value("none")];
    for workspace in app.state.project.workspace.list.clone() {
        let name = workspace
            .get("name")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string();
        let directory = workspace
            .get("directory")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string();
        options.push(SelectOption::new(name).with_value(directory));
    }
    options
}

/// `DialogStatus` (`dialog-status.tsx`) — MCP/LSP/formatter/plugin list.
pub fn status_lines(app: &App, theme: &Theme) -> Vec<ratatui::text::Line<'static>> {
    let mut lines = vec![super::primitives::header_line(theme, "Status", "esc")];
    let text = Style::new().fg(theme.text.to_color());
    let muted = Style::new().fg(theme.text_muted.to_color());
    if app.state.sync.mcp.is_empty() {
        lines.push(Line::styled("  No MCP Servers".to_string(), text));
    } else {
        lines.push(Line::styled(
            format!("  {} MCP Servers", app.state.sync.mcp.len()),
            text,
        ));
        for (name, status) in app.state.sync.mcp.iter() {
            lines.push(Line::from(ratatui::text::Span::styled(
                format!(
                    "  • {name} {}",
                    status.get("status").and_then(Value::as_str).unwrap_or("")
                ),
                muted,
            )));
        }
    }
    if !app.state.sync.lsp.is_empty() {
        lines.push(Line::styled(
            format!("  {} LSP Servers", app.state.sync.lsp.len()),
            text,
        ));
        for item in &app.state.sync.lsp {
            lines.push(Line::from(ratatui::text::Span::styled(
                format!(
                    "  • {} {}",
                    item.get("id").and_then(Value::as_str).unwrap_or(""),
                    item.get("root").and_then(Value::as_str).unwrap_or(""),
                ),
                muted,
            )));
        }
    }
    let formatters: Vec<&Value> = app
        .state
        .sync
        .formatter
        .iter()
        .filter(|f| f.get("enabled").and_then(Value::as_bool) == Some(true))
        .collect();
    if formatters.is_empty() {
        lines.push(Line::styled("  No Formatters".to_string(), text));
    } else {
        lines.push(Line::styled(
            format!("  {} Formatters", formatters.len()),
            text,
        ));
        for formatter in formatters {
            lines.push(Line::from(ratatui::text::Span::styled(
                format!(
                    "  • {}",
                    formatter.get("name").and_then(Value::as_str).unwrap_or(""),
                ),
                muted,
            )));
        }
    }
    lines
}
