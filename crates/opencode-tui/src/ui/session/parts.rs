//! The part renderers — `ui/session/parts.rs` (M8.5).
//!
//! TS reference: `routes/session/index.tsx:1578-2706` —
//! `PART_MAPPING` (text/tool/reasoning), `TextPart`, `ReasoningPart`
//! and the `ToolPart` dispatch over `toolDisplay`.

use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use serde_json::Value;

use opencode_schema::session_status::SessionStatusInfo;
use opencode_schema::session_v1::{AssistantError, V1Part, V1ToolState};

use super::super::markdown;
use crate::ui::diff;
use crate::ui::locale;
use crate::ui::session::Ctx;
use crate::ui::theme::Rgba;

/// `PART_MAPPING` — text, tool and reasoning only.
pub fn render_part(part: &V1Part, ctx: &Ctx) -> Vec<Line<'static>> {
    match part {
        V1Part::Text { .. } => render_text(part, ctx),
        V1Part::Reasoning { .. } => render_reasoning(part, ctx),
        V1Part::Tool { .. } => render_tool(part, ctx),
        _ => Vec::new(),
    }
}

// ------------------------------------------------------------- helpers

fn style(color: Rgba) -> Style {
    Style::new().fg(color.to_color())
}

fn span(text: impl Into<String>, color: Rgba) -> Span<'static> {
    Span::styled(text.into(), style(color))
}

fn blank() -> Line<'static> {
    Line::from("")
}

/// `paddingLeft={left}` — prepend `left` spaces to every line.
fn pad(mut lines: Vec<Line<'static>>, left: usize) -> Vec<Line<'static>> {
    let padding = " ".repeat(left);
    for line in &mut lines {
        line.spans.insert(0, Span::raw(padding.clone()));
    }
    lines
}

fn string_value(value: Option<&Value>) -> Option<&str> {
    value.and_then(Value::as_str)
}

fn number_value(value: Option<&Value>) -> Option<f64> {
    value.and_then(Value::as_f64).filter(|v| v.is_finite())
}

fn record_value(value: Option<&Value>) -> Option<&serde_json::Map<String, Value>> {
    value.and_then(Value::as_object)
}

/// `input()` — the `[key=value, …]` tag of primitive entries.
fn input_tag(input: &serde_json::Map<String, Value>, omit: &[&str]) -> String {
    locale::input_tag(input, omit)
}

// ------------------------------------------------------------- text

/// `TextPart` (`session/index.tsx:1686-1705`): trimmed text → markdown;
/// hidden when empty after the trim.
fn render_text(part: &V1Part, ctx: &Ctx) -> Vec<Line<'static>> {
    let V1Part::Text { text, .. } = part else {
        return Vec::new();
    };
    let text = text.trim();
    if text.is_empty() {
        return Vec::new();
    }
    let lines = markdown::render(text, ctx.width.saturating_sub(3), ctx.theme);
    let mut out = vec![blank()];
    out.extend(pad(lines, 3));
    out
}

// ---------------------------------------------------------- reasoning

/// `reasoningSummary` (`context/thinking.ts:12-20`): a leading bolded
/// title block separates from the body.
fn reasoning_summary(text: &str) -> (Option<String>, String) {
    let content = text.trim();
    let rest = content.strip_prefix("**").unwrap_or(content);
    if rest.len() == content.len() {
        return (None, content.to_string());
    }
    let mut end = None;
    for (index, char) in rest.char_indices() {
        if char == '*' {
            if index == 0 || rest[index - 1..index].chars().any(|c| c == '\n') {
                break;
            }
            if rest.as_bytes().get(index + 1) == Some(&b'*') {
                end = Some(index);
                break;
            }
        }
    }
    let Some(end) = end else {
        return (None, content.to_string());
    };
    let title = &rest[..end];
    let after = &rest[end + 2..];
    if title.contains('*') || title.contains('\n') {
        return (None, content.to_string());
    }
    let body = if after.starts_with("\n\n") || after.is_empty() {
        after.trim_end().to_string()
    } else {
        return (None, content.to_string());
    };
    (Some(title.trim().to_string()), body)
}

/// `ReasoningPart` (`session/index.tsx:1586-1684`).
fn render_reasoning(part: &V1Part, ctx: &Ctx) -> Vec<Line<'static>> {
    let V1Part::Reasoning {
        text,
        metadata,
        time,
        id,
        ..
    } = part
    else {
        return Vec::new();
    };
    let content = text.replace("[REDACTED]", "").trim().to_string();
    let opaque = content.is_empty() && metadata.is_some();
    if content.is_empty() && !opaque {
        return Vec::new();
    }
    // A finished message never animates: cleanup closes every running
    // part when the message ends, so a part still "running" here is a
    // persisted defect (a failed cleanup write, e.g. a storage error).
    let is_done = time.end.is_some() || ctx.message_done;
    let in_minimal = ctx.thinking_mode() == "hide";
    let duration_ms = match time.end {
        Some(end) => end.saturating_sub(time.start),
        None => 0,
    };
    let (title, body) = reasoning_summary(&content);
    let expanded = ctx.app.ui.expanded.contains(id);
    let open = !in_minimal || expanded;

    let fg = if open {
        markdown::blend_over(
            ctx.theme.background,
            ctx.theme.warning,
            ctx.theme.thinking_opacity,
        )
    } else {
        ctx.theme.warning
    };

    let mut lines = vec![blank()];
    if !is_done {
        let label = match &title {
            Some(title) => format!("Thinking: {title}"),
            None => "Thinking".to_string(),
        };
        lines.push(Line::from(vec![
            Span::styled(ctx.spin(), style(fg)),
            Span::raw(" "),
            span(label, fg),
        ]));
    } else {
        let mut detail: Vec<&str> = Vec::new();
        if let Some(title) = &title {
            detail.push(title);
        }
        let duration_text = locale::duration(duration_ms as i64);
        if is_done {
            detail.push(&duration_text);
        }
        let detail = detail.join(" · ");
        let text = if opaque {
            format!("Thought{detail}")
        } else {
            let prefix = if in_minimal && !opaque {
                if open {
                    "- "
                } else {
                    "+ "
                }
            } else {
                ""
            };
            match detail.is_empty() {
                false => format!("{prefix}Thought: {detail}"),
                true => format!("{prefix}Thought"),
            }
        };
        lines.push(Line::from(span(text, fg)));
    }

    if !opaque && open && !body.is_empty() {
        let body_lines = markdown::render(&body, ctx.width.saturating_sub(5), ctx.theme);
        lines.push(blank());
        lines.extend(pad(body_lines, 5));
    }
    pad(lines, 3)
}

// ------------------------------------------------------------- tools

/// `toolDisplay` (`session/index.tsx:2626-2644`).
fn tool_display(tool: &str) -> &'static str {
    const TOOLS: [&str; 14] = [
        "bash",
        "glob",
        "read",
        "grep",
        "webfetch",
        "websearch",
        "write",
        "edit",
        "task",
        "apply_patch",
        "todowrite",
        "question",
        "skill",
        "execute",
    ];
    TOOLS
        .iter()
        .find(|item| **item == tool)
        .copied()
        .unwrap_or("generic")
}

struct ToolParts<'a> {
    id: String,
    call_id: String,
    tool: String,
    state: &'a V1ToolState,
    metadata: serde_json::Map<String, Value>,
    input: serde_json::Map<String, Value>,
    output: Option<String>,
    error: Option<String>,
}

fn tool_parts<'a>(part: &'a V1Part) -> Option<ToolParts<'a>> {
    let V1Part::Tool {
        id,
        call_id,
        tool,
        state,
        ..
    } = part
    else {
        return None;
    };
    let (metadata, input, output, error) = match state {
        V1ToolState::Pending { input, .. } => (serde_json::Map::new(), input.clone(), None, None),
        V1ToolState::Running {
            input, metadata, ..
        } => (
            metadata.clone().unwrap_or_default(),
            input.clone(),
            None,
            None,
        ),
        V1ToolState::Completed {
            input,
            output,
            metadata,
            ..
        } => (metadata.clone(), input.clone(), Some(output.clone()), None),
        V1ToolState::Error {
            input,
            error,
            metadata,
            ..
        } => (
            metadata.clone().unwrap_or_default(),
            input.clone(),
            None,
            Some(error.clone()),
        ),
    };
    Some(ToolParts {
        id: id.to_string(),
        call_id: call_id.to_string(),
        tool: tool.clone(),
        state,
        metadata,
        input,
        output,
        error,
    })
}

/// `ToolPart` (`session/index.tsx:1709-1789`).
fn render_tool(part: &V1Part, ctx: &Ctx) -> Vec<Line<'static>> {
    let Some(tool) = tool_parts(part) else {
        return Vec::new();
    };
    // `shouldHide` — completed tools hide when details are off.
    if !ctx.show_details() && matches!(tool.state, V1ToolState::Completed { .. }) {
        return Vec::new();
    }
    match tool_display(&tool.tool) {
        "bash" => shell(&tool, ctx),
        "glob" => glob(&tool, ctx),
        "read" => read(&tool, ctx),
        "grep" => grep(&tool, ctx),
        "webfetch" => webfetch(&tool, ctx),
        "websearch" => websearch(&tool, ctx),
        "write" => write(&tool, ctx),
        "edit" => edit(&tool, ctx),
        "task" => task(&tool, ctx),
        "execute" => execute(&tool, ctx),
        "apply_patch" => apply_patch(&tool, ctx),
        "todowrite" => todowrite(&tool, ctx),
        "question" => question(&tool, ctx),
        "skill" => skill(&tool, ctx),
        _ => generic(&tool, ctx),
    }
}

// ---------------------------------------------------- inline/block rows

/// `InlineTool`/`InlineToolRow` (`session/index.tsx:1836-1992`).
#[allow(clippy::too_many_arguments)]
fn inline_tool(
    ctx: &Ctx,
    tool: &ToolParts,
    icon: &str,
    icon_color: Option<Rgba>,
    color: Option<Rgba>,
    complete: bool,
    pending: &str,
    failure: Option<&str>,
    spinner: bool,
    children: &str,
) -> Vec<Line<'static>> {
    let error = tool.error.as_deref();
    let denied = error.is_some_and(|error| {
        error.contains("QuestionRejectedError")
            || error.contains("rejected permission")
            || error.contains("specified a rule")
            || error.contains("user dismissed")
    });
    let failed = error.is_some() && !denied;
    let permission = ctx
        .app
        .state
        .sync
        .permission
        .get(ctx.session_id)
        .and_then(|requests| requests.first())
        .and_then(|request| request.tool.as_ref())
        .map(|request| request.call_id.as_str() == tool.call_id)
        .unwrap_or(false);

    let fg = color
        .or(permission.then_some(ctx.theme.warning))
        .or(failed.then_some(ctx.theme.error))
        .unwrap_or(if complete {
            ctx.theme.text_muted
        } else {
            ctx.theme.text
        });

    let mut modifier = Modifier::empty();
    if denied {
        modifier |= Modifier::CROSSED_OUT;
    }

    let mut lines = Vec::new();
    if !complete && !failed {
        lines.push(Line::from(Span::styled(
            format!("      ~ {pending}"),
            style(fg).add_modifier(modifier),
        )));
    } else if spinner {
        for row in children.split('\n') {
            lines.push(Line::from(vec![
                Span::styled("   ", style(fg)),
                Span::styled(ctx.spin(), style(fg)),
                Span::raw(" "),
                Span::styled(row.to_string(), style(fg).add_modifier(modifier)),
            ]));
        }
    } else {
        let content = if failed && !complete {
            failure.unwrap_or(children)
        } else {
            children
        };
        for row in content.split('\n') {
            lines.push(Line::from(vec![
                Span::styled("   ", style(fg)),
                Span::styled(
                    format!("{icon:<2}"),
                    style(icon_color.unwrap_or(fg)).add_modifier(modifier),
                ),
                Span::styled(row.to_string(), style(fg).add_modifier(modifier)),
            ]));
        }
    }

    if failed && ctx.app.ui.expanded_errors.contains(&tool.id) {
        for row in error.unwrap_or_default().split('\n') {
            lines.push(Line::from(Span::styled(
                format!("     {row}"),
                style(ctx.theme.error),
            )));
        }
    }
    lines
}

/// `BlockTool` (`session/index.tsx:1994-2044`).
fn block_tool(
    ctx: &Ctx,
    tool: &ToolParts,
    title: Option<String>,
    spinner: bool,
    children: Vec<Line<'static>>,
) -> Vec<Line<'static>> {
    let mut lines = vec![
        blank(),
        Line::from(Span::styled("┃", style(ctx.theme.background))),
    ];
    if let Some(title) = title {
        if spinner {
            lines.push(Line::from(vec![
                Span::styled("   ", style(ctx.theme.text_muted)),
                Span::styled(ctx.spin(), style(ctx.theme.text_muted)),
                Span::raw(" "),
                span(title.trim_start_matches("# "), ctx.theme.text_muted),
            ]));
        } else {
            lines.push(Line::from(Span::styled(
                format!("   {title}"),
                style(ctx.theme.text_muted),
            )));
        }
    }
    for mut line in children {
        line.spans
            .insert(0, Span::styled("┃", style(ctx.theme.background)));
        line.spans.insert(1, Span::raw("  "));
        lines.push(line);
    }
    if let Some(error) = &tool.error {
        lines.push(Line::from(Span::styled(
            format!("   {error}"),
            style(ctx.theme.error),
        )));
    }
    lines.push(Line::from(Span::styled("┃", style(ctx.theme.background))));
    lines
}

// -------------------------------------------------------- diagnostics

/// `parseDiagnostics` + the `Diagnostics` rows (`session/index.tsx:2583-2607`,
/// `:2692-2706`) — severity 1 only, max 3.
fn diagnostics_rows(ctx: &Ctx, diagnostics: Option<&Value>, file_path: &str) -> Vec<Line<'static>> {
    let Some(items) = record_value(diagnostics)
        .and_then(|map| map.get(file_path))
        .and_then(Value::as_array)
    else {
        return Vec::new();
    };
    items
        .iter()
        .filter_map(|item| {
            let item = record_value(Some(item))?;
            if item.get("severity")?.as_i64()? != 1 {
                return None;
            }
            let start = record_value(item.get("range"))?
                .get("start")
                .and_then(Value::as_object)?;
            let line = start.get("line")?.as_u64()?;
            let character = start.get("character")?.as_u64()?;
            let message = string_value(item.get("message"))?;
            Some((line + 1, character + 1, message.to_string()))
        })
        .take(3)
        .map(|(line, character, message)| {
            Line::from(Span::styled(
                format!("Error [{line}:{character}] {message}"),
                style(ctx.theme.error),
            ))
        })
        .collect()
}

// ---------------------------------------------------------- the tools

fn path_base(ctx: &Ctx) -> String {
    ctx.app
        .state
        .project
        .instance_path
        .directory
        .clone()
        .unwrap_or_default()
}

fn format_input_path(ctx: &Ctx, input: &serde_json::Map<String, Value>, key: &str) -> String {
    locale::format_path(string_value(input.get(key)), &path_base(ctx), "")
}

fn workdir_title(input: &serde_json::Map<String, Value>) -> Option<String> {
    let workdir = string_value(input.get("workdir"))?;
    if workdir.is_empty() || workdir == "." {
        return None;
    }
    Some(format!("# Running in {workdir}"))
}

/// `Shell` (`session/index.tsx:2046-2103`).
fn shell(tool: &ToolParts, ctx: &Ctx) -> Vec<Line<'static>> {
    let command = string_value(tool.input.get("command")).unwrap_or_default();
    if tool.metadata.get("output").is_none() {
        return inline_tool(
            ctx,
            tool,
            "$",
            None,
            None,
            !command.is_empty(),
            "Writing command…",
            None,
            false,
            command,
        );
    }
    let is_running = matches!(tool.state, V1ToolState::Running { .. });
    let output = strip_ansi(
        tool.metadata
            .get("output")
            .and_then(Value::as_str)
            .unwrap_or("")
            .trim(),
    );
    let max_lines = 10;
    let max_chars = max_lines * std::cmp::max(20, ctx.width.saturating_sub(6) as usize);
    let collapsed = locale::collapse_tool_output(&output, max_lines, max_chars);
    let expanded = ctx.app.ui.expanded.contains(&tool.id);
    let limited = if expanded || !collapsed.overflow {
        output.clone()
    } else {
        collapsed.output
    };

    let mut children = Vec::new();
    if is_running {
        children.push(Line::from(vec![
            Span::styled(ctx.spin(), style(ctx.theme.text)),
            Span::raw(" "),
            span(command, ctx.theme.text),
        ]));
    } else {
        children.push(Line::from(span(format!("$ {command}"), ctx.theme.text)));
    }
    if !output.is_empty() {
        for row in limited.split('\n') {
            children.push(Line::from(span(row, ctx.theme.text)));
        }
    }
    if collapsed.overflow {
        children.push(Line::from(span(
            if expanded {
                "Click to collapse"
            } else {
                "Click to expand"
            },
            ctx.theme.text_muted,
        )));
    }
    block_tool(ctx, tool, workdir_title(&tool.input), false, children)
}

/// `stripAnsi` — CSI escape sequences are stripped from shell output.
fn strip_ansi(input: &str) -> String {
    let mut out = String::with_capacity(input.len());
    let mut chars = input.chars().peekable();
    while let Some(char) = chars.next() {
        if char == '\x1b' && chars.peek() == Some(&'[') {
            for escape in chars.by_ref() {
                if escape.is_ascii_alphabetic() {
                    break;
                }
            }
            continue;
        }
        out.push(char);
    }
    out
}

/// `Write` (`session/index.tsx:2105-2135`).
fn write(tool: &ToolParts, ctx: &Ctx) -> Vec<Line<'static>> {
    let file_path = string_value(tool.input.get("filePath")).unwrap_or_default();
    let formatted = format_input_path(ctx, &tool.input, "filePath");
    if tool.metadata.get("diagnostics").is_none() {
        return inline_tool(
            ctx,
            tool,
            "←",
            None,
            None,
            !file_path.is_empty(),
            "Preparing write…",
            None,
            false,
            &format!("Write {formatted}"),
        );
    }
    let mut children = Vec::new();
    let code = string_value(tool.input.get("content")).unwrap_or_default();
    for (index, row) in code.split('\n').enumerate() {
        children.push(Line::from(vec![
            Span::styled(format!("{:>3} ", index + 1), style(ctx.theme.text_muted)),
            span(row, ctx.theme.text),
        ]));
    }
    children.extend(diagnostics_rows(
        ctx,
        tool.metadata.get("diagnostics"),
        file_path,
    ));
    block_tool(
        ctx,
        tool,
        Some(format!("# Wrote {formatted}")),
        false,
        children,
    )
}

/// `Glob` (`session/index.tsx:2137-2148`).
fn glob(tool: &ToolParts, ctx: &Ctx) -> Vec<Line<'static>> {
    let pattern = string_value(tool.input.get("pattern")).unwrap_or_default();
    let mut children = format!("Glob \"{pattern}\"");
    if let Some(path) = string_value(tool.input.get("path")) {
        if !path.is_empty() {
            children.push_str(&format!(
                " in {}",
                locale::format_path(Some(path), &path_base(ctx), "")
            ));
        }
    }
    if let Some(count) = number_value(tool.metadata.get("count")) {
        children.push_str(&format!(
            " ({} {})",
            count,
            if count == 1.0 { "match" } else { "matches" }
        ));
    }
    inline_tool(
        ctx,
        tool,
        "✱",
        None,
        None,
        !pattern.is_empty(),
        "Finding files…",
        None,
        false,
        &children,
    )
}

/// `Read` (`session/index.tsx:2150-2183`).
fn read(tool: &ToolParts, ctx: &Ctx) -> Vec<Line<'static>> {
    let file_path = string_value(tool.input.get("filePath")).unwrap_or_default();
    let formatted = format_input_path(ctx, &tool.input, "filePath");
    let tag = input_tag(&tool.input, &["filePath"]);
    let is_running = matches!(tool.state, V1ToolState::Running { .. });
    let mut lines = inline_tool(
        ctx,
        tool,
        "→",
        None,
        None,
        !file_path.is_empty(),
        "Reading file…",
        None,
        is_running,
        &format!("Read {formatted} {tag}"),
    );
    let loaded: Vec<String> = match tool.state {
        V1ToolState::Completed { time, .. } if time.compacted.is_none() => tool
            .metadata
            .get("loaded")
            .and_then(Value::as_array)
            .map(|items| {
                items
                    .iter()
                    .filter_map(|item| item.as_str().map(str::to_string))
                    .collect()
            })
            .unwrap_or_default(),
        _ => Vec::new(),
    };
    for path in loaded {
        lines.push(Line::from(Span::styled(
            format!(
                "      ↳ Loaded {}",
                locale::format_path(Some(&path), &path_base(ctx), "")
            ),
            style(ctx.theme.text_muted),
        )));
    }
    lines
}

/// `Grep` (`session/index.tsx:2185-2196`).
fn grep(tool: &ToolParts, ctx: &Ctx) -> Vec<Line<'static>> {
    let pattern = string_value(tool.input.get("pattern")).unwrap_or_default();
    let mut children = format!("Grep \"{pattern}\"");
    if let Some(path) = string_value(tool.input.get("path")) {
        if !path.is_empty() {
            children.push_str(&format!(
                " in {}",
                locale::format_path(Some(path), &path_base(ctx), "")
            ));
        }
    }
    if let Some(matches) = number_value(tool.metadata.get("matches")) {
        children.push_str(&format!(
            " ({} {})",
            matches,
            if matches == 1.0 { "match" } else { "matches" }
        ));
    }
    inline_tool(
        ctx,
        tool,
        "✱",
        None,
        None,
        !pattern.is_empty(),
        "Searching content…",
        None,
        false,
        &children,
    )
}

/// `WebFetch` (`session/index.tsx:2198-2204`).
fn webfetch(tool: &ToolParts, ctx: &Ctx) -> Vec<Line<'static>> {
    let url = string_value(tool.input.get("url")).unwrap_or_default();
    inline_tool(
        ctx,
        tool,
        "%",
        None,
        None,
        !url.is_empty(),
        "Fetching from the web…",
        None,
        false,
        &format!("WebFetch {url}"),
    )
}

/// `WebSearch` (`session/index.tsx:2206-2213`).
fn websearch(tool: &ToolParts, ctx: &Ctx) -> Vec<Line<'static>> {
    let query = string_value(tool.input.get("query")).unwrap_or_default();
    let label =
        locale::web_search_provider_label(tool.metadata.get("provider").unwrap_or(&Value::Null));
    let mut children = format!("{label} \"{query}\"");
    if let Some(results) = number_value(tool.metadata.get("numResults")) {
        children.push_str(&format!(" ({results} results)"));
    }
    inline_tool(
        ctx,
        tool,
        "◈",
        None,
        None,
        !query.is_empty(),
        "Searching web…",
        None,
        false,
        &children,
    )
}

/// `Task` (`session/index.tsx:2215-2328`).
fn task(tool: &ToolParts, ctx: &Ctx) -> Vec<Line<'static>> {
    let description = string_value(tool.input.get("description")).unwrap_or_default();
    let subagent_type = string_value(tool.input.get("subagent_type")).unwrap_or("General");
    let background = tool.metadata.get("background").and_then(Value::as_bool) == Some(true);
    let child_id = string_value(tool.metadata.get("sessionId")).map(str::to_string);

    let mut tools: Vec<(String, V1ToolState)> = Vec::new();
    let mut duration = 0i64;
    if let Some(child_id) = child_id.as_deref() {
        let messages = ctx
            .app
            .state
            .sync
            .message
            .get(child_id)
            .cloned()
            .unwrap_or_default();
        for message in &messages {
            let parts = ctx
                .app
                .state
                .sync
                .part
                .get(message_id_of(message))
                .cloned()
                .unwrap_or_default();
            for part in parts {
                if let V1Part::Tool { tool, state, .. } = part {
                    tools.push((tool, state));
                }
            }
        }
        let first = messages.iter().find_map(|m| match m {
            opencode_schema::session_v1::V1Message::User { time, .. } => Some(time.created),
            _ => None,
        });
        let last = messages.iter().rev().find_map(|m| match m {
            opencode_schema::session_v1::V1Message::Assistant { time, .. } => time.completed,
            _ => None,
        });
        if let (Some(first), Some(last)) = (first, last) {
            duration = (last as f64 - first).max(0.0) as i64;
        }
    }

    let status = child_id
        .as_deref()
        .and_then(|id| ctx.app.state.sync.session_status.get(id));
    let retry = match status {
        Some(SessionStatusInfo::Retry {
            attempt, message, ..
        }) => Some((*attempt, message.clone())),
        _ => None,
    };
    let is_running = match status {
        Some(status) if background && !matches!(status, SessionStatusInfo::Idle) => true,
        _ => matches!(tool.state, V1ToolState::Running { .. }),
    };

    let mut content = vec![format!(
        "{} Task{} — {description}",
        locale::titlecase(subagent_type),
        if background { " (background)" } else { "" }
    )];
    let current = tools.iter().rev().find_map(|(tool, state)| match state {
        V1ToolState::Running { title, .. } => Some((tool.clone(), title.clone())),
        V1ToolState::Completed { title, .. } => Some((tool.clone(), Some(title.clone()))),
        _ => None,
    });
    if is_running {
        if let Some((attempt, message)) = &retry {
            content.push(format!(
                "↳ Retrying (attempt {attempt}) · {}",
                locale::truncate(message, 80)
            ));
        } else if !tools.is_empty() {
            match current {
                Some((tool, title)) => {
                    content.push(format!(
                        "↳ {} {}",
                        locale::titlecase(&tool),
                        title.unwrap_or_default()
                    ));
                }
                None => {
                    content.push(format!("↳ {}", format_subagent_toolcalls(tools.len())));
                }
            }
        }
    }
    if !is_running && matches!(tool.state, V1ToolState::Completed { .. }) {
        content.push(format!(
            "↳ {}",
            format_completed_subagent_detail(tools.len(), &locale::duration(duration))
        ));
    }

    inline_tool(
        ctx,
        tool,
        if matches!(tool.state, V1ToolState::Completed { .. }) {
            "✓"
        } else {
            "│"
        },
        None,
        retry.map(|_| ctx.theme.error),
        !description.is_empty(),
        "Delegating…",
        None,
        is_running,
        &content.join("\n"),
    )
}

fn message_id_of(message: &opencode_schema::session_v1::V1Message) -> &str {
    match message {
        opencode_schema::session_v1::V1Message::User { id, .. }
        | opencode_schema::session_v1::V1Message::Assistant { id, .. } => id,
    }
}

/// `formatSubagentToolcalls` (`session/index.tsx:2313-2315`).
fn format_subagent_toolcalls(count: usize) -> String {
    if count == 1 {
        "1 toolcall".to_string()
    } else {
        format!("{count} toolcalls")
    }
}

/// `formatCompletedSubagentDetail` (`session/index.tsx:2325-2328`).
fn format_completed_subagent_detail(toolcalls: usize, duration: &str) -> String {
    if toolcalls == 0 {
        duration.to_string()
    } else {
        format!("{} · {duration}", format_subagent_toolcalls(toolcalls))
    }
}

/// `executeCalls` + `Execute` (`session/index.tsx:2330-2388`).
fn execute(tool: &ToolParts, ctx: &Ctx) -> Vec<Line<'static>> {
    let is_loading = matches!(
        tool.state,
        V1ToolState::Pending { .. } | V1ToolState::Running { .. }
    );
    let calls = tool
        .metadata
        .get("toolCalls")
        .and_then(Value::as_array)
        .map(|items| {
            items
                .iter()
                .filter_map(|item| {
                    let item = record_value(Some(item))?;
                    let tool = string_value(item.get("tool"))?.to_string();
                    let status = string_value(item.get("status"))?.to_string();
                    if !["running", "completed", "error"].contains(&status.as_str()) {
                        return None;
                    }
                    Some((tool, status, record_value(item.get("input")).cloned()))
                })
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();
    let output = strip_ansi(tool.output.as_deref().unwrap_or("").trim());
    let has_runtime_error = tool.metadata.get("error").and_then(Value::as_bool) == Some(true);
    let preview = locale::collapse_tool_output(
        &output,
        4,
        4 * std::cmp::max(20, ctx.width.saturating_sub(6) as usize),
    )
    .output;
    let show_output = !output.is_empty() && has_runtime_error;

    let mut content = vec!["execute".to_string()];
    for (call_tool, status, input) in &calls {
        let args = input
            .as_ref()
            .map(|input| input_tag(input, &[]))
            .unwrap_or_default();
        content.push(format!(
            "↳ {call_tool}{args}{}",
            if status == "error" { " (failed)" } else { "" }
        ));
    }

    let mut lines = inline_tool(
        ctx,
        tool,
        if has_runtime_error {
            "✗"
        } else if matches!(tool.state, V1ToolState::Completed { .. }) {
            "✓"
        } else {
            "│"
        },
        None,
        if has_runtime_error {
            Some(ctx.theme.error)
        } else {
            None
        },
        true,
        "execute",
        None,
        is_loading,
        &content.join("\n"),
    );
    if show_output {
        for (index, row) in preview.split('\n').enumerate() {
            let prefix = if index == 0 { "↳ " } else { "  " };
            lines.push(Line::from(Span::styled(
                format!("      {prefix}{row}"),
                style(ctx.theme.error),
            )));
        }
    }
    lines
}

/// The `Edit` tool's `{replaceAll}` input slice.
fn replace_all_tag(tool: &ToolParts) -> String {
    let mut replace_all = serde_json::Map::new();
    if let Some(value) = tool.input.get("replaceAll") {
        replace_all.insert("replaceAll".to_string(), value.clone());
    }
    input_tag(&replace_all, &[])
}

/// `Edit` (`session/index.tsx:2390-2441`).
fn edit(tool: &ToolParts, ctx: &Ctx) -> Vec<Line<'static>> {
    let file_path = string_value(tool.input.get("filePath")).unwrap_or_default();
    let formatted = format_input_path(ctx, &tool.input, "filePath");
    if tool.metadata.get("diff").is_none() {
        return inline_tool(
            ctx,
            tool,
            "←",
            None,
            None,
            !file_path.is_empty(),
            "Preparing edit…",
            None,
            false,
            &format!("Edit {formatted} {}", replace_all_tag(tool)),
        );
    }
    let diff_content = string_value(tool.metadata.get("diff")).unwrap_or_default();
    let style = if ctx.app.config.diff_style == "stacked" {
        diff::DiffStyle::Stacked
    } else {
        diff::DiffStyle::Auto
    };
    let view = diff::view_for(style, ctx.width);
    let mut children = diff::render(
        diff_content,
        view,
        ctx.width.saturating_sub(4),
        ctx.diff_wrap_mode(),
        ctx.theme,
    );
    children.extend(diagnostics_rows(
        ctx,
        tool.metadata.get("diagnostics"),
        file_path,
    ));
    block_tool(
        ctx,
        tool,
        Some(format!("← Edit {formatted}")),
        false,
        pad(children, 1),
    )
}

/// `ApplyPatch` (`session/index.tsx:2443-2517`).
fn apply_patch(tool: &ToolParts, ctx: &Ctx) -> Vec<Line<'static>> {
    let files = parse_apply_patch_files(tool.metadata.get("files"));
    if files.is_empty() {
        return inline_tool(
            ctx,
            tool,
            "%",
            None,
            None,
            false,
            "Preparing patch…",
            Some("Patch failed"),
            false,
            "Patch",
        );
    }
    let mut lines = Vec::new();
    for file in &files {
        let title = match file.kind.as_str() {
            "delete" => format!("# Deleted {}", file.relative_path),
            "add" => format!("# Created {}", file.relative_path),
            "move" => format!(
                "# Moved {} → {}",
                locale::format_path(Some(&file.file_path), &path_base(ctx), ""),
                file.relative_path
            ),
            _ => format!("← Patched {}", file.relative_path),
        };
        let mut children = Vec::new();
        if file.kind == "delete" {
            children.push(Line::from(Span::styled(
                format!(
                    "-{} line{}",
                    file.deletions,
                    if file.deletions != 1 { "s" } else { "" }
                ),
                style(ctx.theme.diff_removed),
            )));
        } else {
            let style = if ctx.app.config.diff_style == "stacked" {
                diff::DiffStyle::Stacked
            } else {
                diff::DiffStyle::Auto
            };
            let view = diff::view_for(style, ctx.width);
            children.extend(diff::render(
                &file.patch,
                view,
                ctx.width.saturating_sub(4),
                ctx.diff_wrap_mode(),
                ctx.theme,
            ));
            let diagnostics_path = file
                .move_path
                .clone()
                .unwrap_or_else(|| file.file_path.clone());
            children.extend(diagnostics_rows(
                ctx,
                tool.metadata.get("diagnostics"),
                &diagnostics_path,
            ));
        }
        lines.extend(block_tool(ctx, tool, Some(title), false, pad(children, 1)));
    }
    lines
}

/// `parseApplyPatchFiles` (`session/index.tsx:2652-2665`).
fn parse_apply_patch_files(value: Option<&Value>) -> Vec<ApplyPatchFile> {
    let Some(items) = value.and_then(Value::as_array) else {
        return Vec::new();
    };
    items
        .iter()
        .filter_map(|item| {
            let file = record_value(Some(item))?;
            Some(ApplyPatchFile {
                kind: string_value(file.get("type"))?.to_string(),
                relative_path: string_value(file.get("relativePath"))?.to_string(),
                file_path: string_value(file.get("filePath"))?.to_string(),
                patch: string_value(file.get("patch"))?.to_string(),
                deletions: number_value(file.get("deletions"))? as usize,
                move_path: string_value(file.get("movePath")).map(str::to_string),
            })
        })
        .collect()
}

struct ApplyPatchFile {
    kind: String,
    relative_path: String,
    file_path: String,
    patch: String,
    deletions: usize,
    move_path: Option<String>,
}

/// `TodoWrite` (`session/index.tsx:2519-2537`).
fn todowrite(tool: &ToolParts, ctx: &Ctx) -> Vec<Line<'static>> {
    let todos = parse_todos(tool.input.get("todos"));
    if parse_todos(tool.metadata.get("todos")).is_empty() {
        return inline_tool(
            ctx,
            tool,
            "⚙",
            None,
            None,
            false,
            "Updating todos…",
            Some("Todo update failed"),
            false,
            "Updating todos…",
        );
    }
    let mut children = Vec::new();
    for todo in &todos {
        let marker = match todo.0.as_str() {
            "completed" => "✓",
            "in_progress" => "•",
            _ => " ",
        };
        let color = if todo.0 == "in_progress" {
            ctx.theme.warning
        } else {
            ctx.theme.text_muted
        };
        children.push(Line::from(vec![
            Span::styled(format!("[{marker}] "), style(color)),
            span(&todo.1, color),
        ]));
    }
    block_tool(ctx, tool, Some("# Todos".to_string()), false, children)
}

/// `parseTodos` (`session/index.tsx:2667-2675`).
fn parse_todos(value: Option<&Value>) -> Vec<(String, String)> {
    value
        .and_then(Value::as_array)
        .map(|items| {
            items
                .iter()
                .filter_map(|item| {
                    let todo = record_value(Some(item))?;
                    Some((
                        string_value(todo.get("status"))?.to_string(),
                        string_value(todo.get("content"))?.to_string(),
                    ))
                })
                .collect()
        })
        .unwrap_or_default()
}

/// `Question` (`session/index.tsx:2539-2573`).
fn question(tool: &ToolParts, ctx: &Ctx) -> Vec<Line<'static>> {
    let questions = parse_questions(tool.input.get("questions"));
    let answers = parse_question_answers(tool.metadata.get("answers"));
    let count = questions.len();
    if let Some(answers) = answers {
        let mut children = Vec::new();
        for (index, question) in questions.iter().enumerate() {
            let answer = answers
                .get(index)
                .map(|answer| {
                    if answer.is_empty() {
                        "(no answer)".to_string()
                    } else {
                        answer.join(", ")
                    }
                })
                .unwrap_or_else(|| "(no answer)".to_string());
            children.push(Line::from(span(question, ctx.theme.text_muted)));
            children.push(Line::from(span(answer, ctx.theme.text)));
        }
        return block_tool(ctx, tool, Some("# Questions".to_string()), false, children);
    }
    inline_tool(
        ctx,
        tool,
        "→",
        None,
        None,
        !questions.is_empty(),
        "Asking questions…",
        None,
        false,
        &format!(
            "Asked {count} question{}",
            if count != 1 { "s" } else { "" }
        ),
    )
}

/// `parseQuestions` (`session/index.tsx:2677-2683`).
fn parse_questions(value: Option<&Value>) -> Vec<String> {
    value
        .and_then(Value::as_array)
        .map(|items| {
            items
                .iter()
                .filter_map(|item| {
                    string_value(record_value(Some(item))?.get("question")).map(str::to_string)
                })
                .collect()
        })
        .unwrap_or_default()
}

/// `parseQuestionAnswers` (`session/index.tsx:2685-2690`) — `None`
/// when the metadata is absent.
fn parse_question_answers(value: Option<&Value>) -> Option<Vec<Vec<String>>> {
    value.and_then(Value::as_array).map(|items| {
        items
            .iter()
            .map(|item| match item.as_array() {
                Some(entries) => entries
                    .iter()
                    .filter_map(|entry| entry.as_str().map(str::to_string))
                    .collect(),
                None => Vec::new(),
            })
            .collect()
    })
}

/// `Skill` (`session/index.tsx:2575-2581`).
fn skill(tool: &ToolParts, ctx: &Ctx) -> Vec<Line<'static>> {
    let name = string_value(tool.input.get("name")).unwrap_or_default();
    inline_tool(
        ctx,
        tool,
        "→",
        None,
        None,
        !name.is_empty(),
        "Loading skill…",
        None,
        false,
        &format!("Skill \"{name}\""),
    )
}

/// `GenericTool` (`session/index.tsx:1798-1834`).
fn generic(tool: &ToolParts, ctx: &Ctx) -> Vec<Line<'static>> {
    let output = tool.output.as_deref().unwrap_or("").trim().to_string();
    if !output.is_empty() && ctx.show_generic_tool_output() {
        let max_lines = 3;
        let max_chars = max_lines * std::cmp::max(20, ctx.width.saturating_sub(6) as usize);
        let collapsed = locale::collapse_tool_output(&output, max_lines, max_chars);
        let expanded = ctx.app.ui.expanded.contains(&tool.id);
        let limited = if expanded || !collapsed.overflow {
            output.clone()
        } else {
            collapsed.output
        };
        let mut children: Vec<Line<'static>> = limited
            .split('\n')
            .map(|row| Line::from(span(row, ctx.theme.text)))
            .collect();
        if collapsed.overflow {
            children.push(Line::from(span(
                if expanded {
                    "Click to collapse"
                } else {
                    "Click to expand"
                },
                ctx.theme.text_muted,
            )));
        }
        return block_tool(
            ctx,
            tool,
            Some(format!("# {} {}", tool.tool, input_tag(&tool.input, &[]))),
            false,
            children,
        );
    }
    inline_tool(
        ctx,
        tool,
        "⚙",
        None,
        None,
        true,
        "Writing command…",
        None,
        false,
        &format!("{} {}", tool.tool, input_tag(&tool.input, &[])),
    )
}

/// `errorMessage` (`util/error.ts:136-156`) applied to the decoded
/// `AssistantError` union: every variant except `OutputLength` carries
/// a `data.message` string, which wins. `OutputLength` has neither a
/// `message` nor a `data.message`, and `String(error)` is
/// `"[object Object]"` (excluded), so the pretty-printed JSON of
/// `{name, data}` from `errorFormat` (`util/error.ts:108-134`) is
/// returned.
pub fn assistant_error_message(error: &AssistantError) -> String {
    let fallback = |name: &str| format!("{{\n  \"name\": \"{name}\",\n  \"data\": {{}}\n}}");
    match error {
        AssistantError::Auth { message, .. } => message.clone(),
        AssistantError::Unknown { message, .. } => message.clone(),
        AssistantError::OutputLength {} => fallback("MessageOutputLengthError"),
        AssistantError::Aborted { message } => message.clone(),
        AssistantError::StructuredOutput { message, .. } => message.clone(),
        AssistantError::ContextOverflow { message, .. } => message.clone(),
        AssistantError::ContentFilter { message } => message.clone(),
        AssistantError::Api { message, .. } => message.clone(),
    }
}

#[cfg(test)]
mod tests {
    use ratatui::text::Line;
    use serde_json::json;

    use opencode_schema::session_v1::{
        ReasoningTime, ToolStateCompletedTime, ToolStateRunningTime, V1ToolState,
    };

    use super::*;
    use crate::state::App;

    fn app() -> App {
        App::new(
            crate::config::TuiConfig::default(),
            crate::state::Args::default(),
            None,
        )
    }

    fn theme() -> crate::ui::theme::Theme {
        let app = app();
        app.ui
            .theme
            .resolve(&app.state.kv)
            .expect("builtin theme resolves")
    }

    fn plain(lines: &[Line<'_>]) -> String {
        lines
            .iter()
            .map(|line| {
                line.spans
                    .iter()
                    .map(|span| span.content.as_ref())
                    .collect::<String>()
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    fn tool_part(id: &str, tool: &str, state: V1ToolState) -> V1Part {
        V1Part::Tool {
            id: id.to_string(),
            session_id: "ses_a".to_string(),
            message_id: "msg_1".to_string(),
            call_id: "call_1".to_string(),
            tool: tool.to_string(),
            state,
            metadata: None,
        }
    }

    #[test]
    fn text_part_renders_markdown() {
        let app = app();
        let theme = theme();
        let ctx = Ctx::new(&app, &theme, "ses_a", 80);
        let part = V1Part::Text {
            id: "prt_1".into(),
            session_id: "ses_a".into(),
            message_id: "msg_1".into(),
            text: "**bold** statement".into(),
            synthetic: None,
            ignored: None,
            time: None,
            metadata: None,
        };
        let text = plain(&render_part(&part, &ctx));
        assert!(text.contains("bold"), "{text}");
    }

    #[test]
    fn bash_pending_renders_inline() {
        let app = app();
        let theme = theme();
        let ctx = Ctx::new(&app, &theme, "ses_a", 80);
        // `complete` is the command string — a present command renders
        // the icon row (`:2046-2081`), an empty one the pending row.
        let part = tool_part(
            "prt_1",
            "bash",
            V1ToolState::Pending {
                input: json!({ "command": "echo hi" }).as_object().unwrap().clone(),
                raw: String::new(),
            },
        );
        let text = plain(&render_part(&part, &ctx));
        assert!(text.contains("$ echo hi"), "{text}");
        let part = tool_part(
            "prt_1",
            "bash",
            V1ToolState::Pending {
                input: serde_json::Map::new(),
                raw: String::new(),
            },
        );
        let text = plain(&render_part(&part, &ctx));
        assert!(text.contains("~ Writing command…"), "{text}");
    }

    #[test]
    fn bash_overflow_hint_flips_with_expanded() {
        let mut app = app();
        app.ui.expanded.insert("prt_1".to_string());
        let theme = theme();
        let ctx = Ctx::new(&app, &theme, "ses_a", 80);
        let output = (1..=60)
            .map(|n| n.to_string())
            .collect::<Vec<_>>()
            .join("\n");
        let part = tool_part(
            "prt_1",
            "bash",
            V1ToolState::Completed {
                input: json!({ "command": "seq 1 60" })
                    .as_object()
                    .unwrap()
                    .clone(),
                output: String::new(),
                title: "seq 1 60".into(),
                metadata: json!({ "output": output }).as_object().unwrap().clone(),
                time: ToolStateCompletedTime {
                    start: 0,
                    end: 1,
                    compacted: None,
                },
                attachments: None,
            },
        );
        let text = plain(&render_part(&part, &ctx));
        assert!(text.contains("Click to collapse"), "{text}");
        assert!(text.contains("60"), "{text}");
    }

    #[test]
    fn bash_completed_renders_block() {
        let app = app();
        let theme = theme();
        let ctx = Ctx::new(&app, &theme, "ses_a", 80);
        let part = tool_part(
            "prt_1",
            "bash",
            V1ToolState::Completed {
                input: json!({ "command": "echo hi" }).as_object().unwrap().clone(),
                output: String::new(),
                title: "echo hi".into(),
                metadata: json!({ "output": "hello\nworld" })
                    .as_object()
                    .unwrap()
                    .clone(),
                time: ToolStateCompletedTime {
                    start: 0,
                    end: 1,
                    compacted: None,
                },
                attachments: None,
            },
        );
        let text = plain(&render_part(&part, &ctx));
        assert!(text.contains("$ echo hi"), "{text}");
        assert!(text.contains("hello"), "{text}");
        assert!(text.contains("world"), "{text}");
    }

    #[test]
    fn completed_tools_hide_when_details_off() {
        let mut app = app();
        app.state.kv.set(
            crate::state::kv::keys::TOOL_DETAILS_VISIBILITY,
            json!(false),
        );
        let theme = theme();
        let ctx = Ctx::new(&app, &theme, "ses_a", 80);
        let part = tool_part(
            "prt_1",
            "bash",
            V1ToolState::Completed {
                input: serde_json::Map::new(),
                output: String::new(),
                title: String::new(),
                metadata: serde_json::Map::new(),
                time: ToolStateCompletedTime {
                    start: 0,
                    end: 1,
                    compacted: None,
                },
                attachments: None,
            },
        );
        assert!(render_part(&part, &ctx).is_empty());
    }

    #[test]
    fn glob_renders_match_count() {
        let app = app();
        let theme = theme();
        let ctx = Ctx::new(&app, &theme, "ses_a", 80);
        let part = tool_part(
            "prt_1",
            "glob",
            V1ToolState::Running {
                input: json!({ "pattern": "**/*.rs" }).as_object().unwrap().clone(),
                title: None,
                metadata: json!({ "count": 3 }).as_object().cloned(),
                time: ToolStateRunningTime { start: 0 },
            },
        );
        let text = plain(&render_part(&part, &ctx));
        assert!(text.contains("Glob \"**/*.rs\" (3 matches)"), "{text}");
    }

    #[test]
    fn reasoning_running_renders_thinking() {
        let app = app();
        let theme = theme();
        let ctx = Ctx::new(&app, &theme, "ses_a", 80);
        let part = V1Part::Reasoning {
            id: "prt_r".into(),
            session_id: "ses_a".into(),
            message_id: "msg_1".into(),
            text: "let me think".into(),
            metadata: None,
            time: ReasoningTime {
                start: 0,
                end: None,
            },
        };
        let text = plain(&render_part(&part, &ctx));
        assert!(text.contains("Thinking"), "{text}");
    }

    #[test]
    fn reasoning_on_a_finished_message_never_spins() {
        // A part left "running" on a completed/errored message is a
        // persisted defect (a failed cleanup write) — it must not
        // animate forever.
        let app = app();
        let theme = theme();
        let mut ctx = Ctx::new(&app, &theme, "ses_a", 80);
        ctx.message_done = true;
        let part = V1Part::Reasoning {
            id: "prt_r".into(),
            session_id: "ses_a".into(),
            message_id: "msg_1".into(),
            text: "stuck mid-thought".into(),
            metadata: None,
            time: ReasoningTime {
                start: 0,
                end: None,
            },
        };
        let text = plain(&render_part(&part, &ctx));
        assert!(!text.contains("Thinking"), "{text}");
        assert!(text.contains("Thought"), "{text}");
    }

    #[test]
    fn reasoning_done_renders_thought_with_duration() {
        let app = app();
        let theme = theme();
        let ctx = Ctx::new(&app, &theme, "ses_a", 80);
        let part = V1Part::Reasoning {
            id: "prt_r".into(),
            session_id: "ses_a".into(),
            message_id: "msg_1".into(),
            text: "done thinking".into(),
            metadata: None,
            time: ReasoningTime {
                start: 0,
                end: Some(1_500),
            },
        };
        let text = plain(&render_part(&part, &ctx));
        // Hide mode renders collapsed (`+ `) with the `Thought: <detail>`
        // header (`session/index.tsx:1664-1667`).
        assert!(text.contains("+ Thought: 1.5s"), "{text}");
    }

    #[test]
    fn generic_tool_renders_inline_without_output_visibility() {
        let app = app();
        let theme = theme();
        let ctx = Ctx::new(&app, &theme, "ses_a", 80);
        let part = tool_part(
            "prt_1",
            "some_plugin",
            V1ToolState::Completed {
                input: json!({ "key": "value" }).as_object().unwrap().clone(),
                output: "ignored output".into(),
                title: String::new(),
                metadata: serde_json::Map::new(),
                time: ToolStateCompletedTime {
                    start: 0,
                    end: 1,
                    compacted: None,
                },
                attachments: None,
            },
        );
        let text = plain(&render_part(&part, &ctx));
        assert!(text.contains("⚙ some_plugin [key=value]"), "{text}");
        assert!(!text.contains("ignored output"), "{text}");
    }
}
