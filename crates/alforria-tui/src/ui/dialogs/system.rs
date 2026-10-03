//! `component/dialog-mcp.tsx`, `dialog-theme-list.tsx`, `dialog-status.tsx`,
//! `dialog-skill.tsx`, `dialog-workspace-list.tsx` and the
//! workspace-set select.

use serde_json::Value;

use super::primitives::SelectOption;
use crate::state::App;
use crate::ui::dialogs::DialogFrame;
use crate::ui::theme::Theme;
use ratatui::style::{Modifier, Style};
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

/// `DialogThemeList.options` (`dialog-theme-list.tsx:8-13`) — sorted
/// case-insensitively, `current` marked (`:9,27`): the theme the dialog
/// opened on, not the one being previewed.
pub fn theme_options(app: &App, initial: Option<&str>) -> Vec<SelectOption> {
    let current = initial.unwrap_or(&app.ui.theme.active).to_string();
    let mut names: Vec<&str> = crate::ui::theme::all_themes()
        .into_iter()
        .map(|(name, _)| name)
        .collect();
    names.sort_by_key(|name| name.to_lowercase());
    names
        .into_iter()
        .map(|name| {
            SelectOption::new(name.to_string())
                .with_value(name.to_string())
                .with_current(name == current)
        })
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
            let option = SelectOption::new(title)
                .with_value(name)
                .with_category("Skills");
            if description.is_empty() {
                option
            } else {
                option.with_description(description)
            }
        })
        .collect()
}

/// `DialogWorkspaceList.options` (`dialog-workspace-list.tsx:34-66`).
pub fn workspace_options(app: &App, frame: &DialogFrame, theme: &Theme) -> Vec<SelectOption> {
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
            let name = workspace
                .get("name")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string();
            let is_deleting = frame.pending_delete.as_deref() == Some(id.as_str());
            let expanded = frame.expanded.contains(&id);
            let connected = app
                .state
                .project
                .workspace
                .status
                .get(&id)
                .map(String::as_str)
                == Some("connected");
            let mut option = SelectOption::new(if is_deleting {
                format!("Delete {name}? Press delete again")
            } else {
                name
            })
            .with_value(id)
            .with_gutter(Some("●".to_string()))
            .with_gutter_fg(Some(if connected {
                theme.success
            } else {
                theme.error
            }))
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

/// `DialogStatus.plugins` (`dialog-status.tsx:17-41`) — the
/// `config.plugin` entries parsed into (name, version) pairs.
fn status_plugins(config: &Value) -> Vec<(String, Option<String>)> {
    let mut result: Vec<(String, Option<String>)> = config
        .get("plugin")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|item| {
            let value = match item {
                Value::String(value) => Some(value.as_str()),
                Value::Array(array) => array.first().and_then(Value::as_str),
                _ => None,
            }?;
            Some(plugin_entry(value))
        })
        .collect();
    result.sort_by(|a, b| a.0.cmp(&b.0));
    result
}

/// One `config.plugin` entry — `file://` paths derive the name from
/// the filename (`dialog-status.tsx:21-38`), package ids split at the
/// last `@`.
fn plugin_entry(value: &str) -> (String, Option<String>) {
    if let Some(path) = value.strip_prefix("file://") {
        let mut parts: Vec<&str> = path.split('/').collect();
        let filename = parts.pop().filter(|part| !part.is_empty()).unwrap_or(path);
        if !filename.contains('.') {
            return (filename.to_string(), None);
        }
        let basename = filename.split('.').next().unwrap_or_default();
        if basename == "index" {
            let name = parts
                .pop()
                .filter(|part| !part.is_empty())
                .unwrap_or(basename);
            return (name.to_string(), None);
        }
        return (basename.to_string(), None);
    }
    match value.rfind('@') {
        Some(index) if index > 0 => {
            let name = value[..index].to_string();
            let version = value[index + 1..].to_string();
            (name, Some(version))
        }
        _ => (value.to_string(), Some("latest".to_string())),
    }
}

/// `DialogStatus` (`dialog-status.tsx`) — MCP/LSP/formatter/plugin list.
pub fn status_lines(app: &App, theme: &Theme, width: u16) -> Vec<ratatui::text::Line<'static>> {
    // `paddingLeft/Right 2`, `gap={1}` between the sections
    // (`dialog-status.tsx:44-56`).
    let mut lines = vec![super::primitives::header_line_padded(
        theme, "Status", "esc", width, 2,
    )];
    let text = Style::new().fg(theme.text.to_color());
    let muted = Style::new().fg(theme.text_muted.to_color());
    let bold_text = Style::new()
        .fg(theme.text.to_color())
        .add_modifier(Modifier::BOLD);
    lines.push(Line::raw(""));
    if app.state.sync.mcp.is_empty() {
        lines.push(Line::styled("  No MCP Servers".to_string(), text));
    } else {
        lines.push(Line::styled(
            format!("  {} MCP Servers", app.state.sync.mcp.len()),
            text,
        ));
        for (name, status) in app.state.sync.mcp.iter() {
            let status_value = status.get("status").and_then(Value::as_str);
            let error = status.get("error").and_then(Value::as_str);
            // The bullet colour map (`dialog-status.tsx:64-69`).
            let bullet = match status_value {
                Some("connected") => theme.success,
                Some("failed") => theme.error,
                Some("disabled") => theme.text_muted,
                Some("needs_auth") => theme.warning,
                Some("needs_client_registration") => theme.error,
                _ => theme.text,
            };
            let prose = match status_value {
                Some("connected") => "Connected".to_string(),
                Some("failed") => error.unwrap_or_default().to_string(),
                Some("disabled") => "Disabled in configuration".to_string(),
                Some("needs_auth") => {
                    format!("Needs authentication (run: opencode mcp auth {name})")
                }
                Some("needs_client_registration") => error.unwrap_or_default().to_string(),
                other => other.unwrap_or_default().to_string(),
            };
            lines.push(Line::from(vec![
                ratatui::text::Span::styled("  • ".to_string(), Style::new().fg(bullet.to_color())),
                ratatui::text::Span::styled(name.clone(), bold_text),
                ratatui::text::Span::styled(format!(" {prose}"), muted),
            ]));
        }
    }
    if !app.state.sync.lsp.is_empty() {
        lines.push(Line::raw(""));
        lines.push(Line::styled(
            format!("  {} LSP Servers", app.state.sync.lsp.len()),
            text,
        ));
        for item in &app.state.sync.lsp {
            let bullet = match item.get("status").and_then(Value::as_str) {
                Some("connected") => theme.success,
                Some("error") => theme.error,
                _ => theme.text,
            };
            lines.push(Line::from(vec![
                ratatui::text::Span::styled("  • ".to_string(), Style::new().fg(bullet.to_color())),
                ratatui::text::Span::styled(
                    item.get("id")
                        .and_then(Value::as_str)
                        .unwrap_or("")
                        .to_string(),
                    bold_text,
                ),
                ratatui::text::Span::styled(
                    format!(
                        " {}",
                        item.get("root").and_then(Value::as_str).unwrap_or("")
                    ),
                    muted,
                ),
            ]));
        }
    }
    let formatters: Vec<&Value> = app
        .state
        .sync
        .formatter
        .iter()
        .filter(|f| f.get("enabled").and_then(Value::as_bool) == Some(true))
        .collect();
    lines.push(Line::raw(""));
    if formatters.is_empty() {
        lines.push(Line::styled("  No Formatters".to_string(), text));
    } else {
        lines.push(Line::styled(
            format!("  {} Formatters", formatters.len()),
            text,
        ));
        for formatter in formatters {
            lines.push(Line::from(vec![
                ratatui::text::Span::styled(
                    "  • ".to_string(),
                    Style::new().fg(theme.success.to_color()),
                ),
                ratatui::text::Span::styled(
                    formatter
                        .get("name")
                        .and_then(Value::as_str)
                        .unwrap_or("")
                        .to_string(),
                    bold_text,
                ),
            ]));
        }
    }
    let plugins = status_plugins(&app.state.sync.config);
    lines.push(Line::raw(""));
    if plugins.is_empty() {
        lines.push(Line::styled("  No Plugins".to_string(), text));
    } else {
        lines.push(Line::styled(format!("  {} Plugins", plugins.len()), text));
        for (name, version) in plugins {
            let mut spans = vec![
                ratatui::text::Span::styled(
                    "  • ".to_string(),
                    Style::new().fg(theme.success.to_color()),
                ),
                ratatui::text::Span::styled(name, bold_text),
            ];
            if let Some(version) = version {
                spans.push(ratatui::text::Span::styled(format!(" @{version}"), muted));
            }
            lines.push(Line::from(spans));
        }
    }
    lines
}
