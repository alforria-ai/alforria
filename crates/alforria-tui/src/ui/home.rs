//! `routes/home.tsx` — vertically centered logo + prompt (M8.3) plus
//! the `home_bottom` tips slot and the `home_footer` bar
//! (`feature-plugins/home/footer.tsx`, `tips.tsx`, `tips-view.tsx`).

use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::Style;
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Paragraph, Widget};
use serde_json::Value;

use super::theme::{tint, Rgba, Theme};
use crate::state::route::Route;
use crate::state::App;

/// `placeholder` (`home.tsx:17-20`).
pub const PLACEHOLDER_NORMAL: [&str; 3] = [
    "Fix a TODO in the codebase",
    "What is the tech stack of this project?",
    "Fix broken tests",
];
pub const PLACEHOLDER_SHELL: [&str; 3] = ["ls -la", "git status", "pwd"];

/// `placeholderText` (`prompt/index.tsx:1311-1319`).
pub fn placeholder_text(app: &App) -> String {
    let index = app.ui.home_placeholder % PLACEHOLDER_NORMAL.len();
    format!("Ask anything… \"{}\"", PLACEHOLDER_NORMAL[index])
}

/// `renderLine` (`component/logo.tsx:9-47`): `_` is a shadowed space, `^`
/// a shadowed `▀`, `~` a shadowed `▀`, `,` a shadowed `▄` — everything
/// else uses the foreground.
fn logo_spans(line: &str, fg: Rgba, bold: bool, theme: &Theme) -> Vec<Span<'static>> {
    let shadow = tint(theme.background, fg, 0.25);
    let mut style = Style::new().fg(fg.to_color());
    if bold {
        style = style.add_modifier(ratatui::style::Modifier::BOLD);
    }
    line.chars()
        .map(|char| match char {
            '_' => Span::styled(" ", Style::new().fg(fg.to_color()).bg(shadow.to_color())),
            '^' => Span::styled("▀", Style::new().fg(fg.to_color()).bg(shadow.to_color())),
            '~' => Span::styled("▀", Style::new().fg(shadow.to_color())),
            ',' => Span::styled("▄", Style::new().fg(shadow.to_color())),
            _ => Span::styled(char.to_string(), style),
        })
        .collect()
}

/// `Logo` (`component/logo.tsx:49-60`).
fn logo_lines(theme: &Theme) -> Vec<Line<'static>> {
    let logo = &crate::ui::LOGO;
    logo.left
        .iter()
        .zip(logo.right.iter())
        .map(|(left, right)| {
            let mut spans = logo_spans(left, theme.text_muted, false, theme);
            spans.push(Span::raw(" "));
            spans.extend(logo_spans(right, theme.text, true, theme));
            Line::from(spans)
        })
        .collect()
}

/// `Logo` column width: left half + gap + right half.
const LOGO_WIDTH: u16 = 47;

/// The `home_footer` bar (`footer.tsx:64-82`): paddingTop 1 + one
/// content row + paddingBottom 1.
const FOOTER_HEIGHT: u16 = 3;

/// The tips box `maxWidth={75}` (`feature-plugins/home/tips.tsx:26`).
const TIPS_MAX_WIDTH: u16 = 75;

/// The tips box `paddingTop={3}` (`tips.tsx:28`).
const TIPS_PADDING_TOP: u16 = 3;

/// The `● Tip ` prefix (`tips-view.tsx:152`).
const TIP_PREFIX: &str = "● Tip ";
const TIP_PREFIX_WIDTH: usize = 6;

/// The inner textarea width: the frame pads left+right by 2.
fn inner_width(area: Rect) -> u16 {
    area.width.saturating_sub(4).max(1)
}

/// The wrapped textarea rows: the live editor, or the seeded --prompt
/// input while the editor is empty.
fn rows(app: &App, area: Rect) -> crate::ui::textarea::Display {
    let seed = match &app.state.route.data {
        Route::Home {
            prompt: Some(seed), ..
        } if app.ui.prompt.is_empty() => Some(&seed.input),
        _ => None,
    };
    let Some(seed) = seed else {
        return app.ui.prompt.textarea.display(inner_width(area));
    };
    let mut rows: Vec<Vec<(char, Option<u64>)>> = vec![Vec::new()];
    for char in seed.chars() {
        if char == '\n' {
            rows.push(Vec::new());
        } else {
            rows.last_mut()
                .expect("rows starts with one row")
                .push((char, None));
        }
    }
    let cursor_row = rows.len() - 1;
    let cursor_col = rows.last().map(Vec::len).unwrap_or(0);
    crate::ui::textarea::Display {
        rows,
        cursor_row,
        cursor_col,
    }
}

/// The rendered prompt height: `paddingTop={1}` + the textarea rows +
/// the 1-row bottom cap (`prompt/index.tsx:1487-1512`).
fn prompt_height(rows: usize) -> u16 {
    (1 + rows.min(u16::MAX as usize) as u16).saturating_add(1)
}

pub fn render(app: &App, frame: &mut ratatui::Frame, theme: &Theme, area: Rect) {
    let [main, footer_area] =
        Layout::vertical([Constraint::Fill(1), Constraint::Length(FOOTER_HEIGHT)]).areas(area);
    render_footer(app, frame, theme, footer_area);
    let padded = Rect {
        x: main.x.saturating_add(2),
        y: main.y,
        width: main.width.saturating_sub(4),
        height: main.height,
    };
    let rows = rows(app, padded).rows;
    let tips_width = padded.width.min(TIPS_MAX_WIDTH);
    let tips_lines = if tips_shown(app) {
        tip_lines(app, theme, tips_width)
    } else {
        Vec::new()
    };
    let tips_height = if tips_lines.is_empty() {
        0
    } else {
        TIPS_PADDING_TOP + tips_lines.len() as u16
    };
    let [_, _gap, logo, _, prompt, tips_area, _bottom] = Layout::vertical([
        Constraint::Fill(1),
        Constraint::Length(4),
        Constraint::Length(5),
        Constraint::Length(1),
        Constraint::Length(prompt_height(rows.len())),
        Constraint::Length(tips_height),
        Constraint::Fill(1),
    ])
    .areas(padded);
    Paragraph::new(logo_lines(theme))
        .render(center_horizontally(logo, LOGO_WIDTH), frame.buffer_mut());
    render_prompt(app, frame, theme, prompt);
    if !tips_lines.is_empty() {
        render_tips(frame, tips_area, tips_width, &tips_lines);
    }
}

fn center_horizontally(area: Rect, width: u16) -> Rect {
    Rect {
        x: area.x + area.width.saturating_sub(width) / 2,
        width: width.min(area.width),
        y: area.y,
        height: area.height,
    }
}

// ----------------------------------------------------------- home_footer

fn mcp_status(value: &Value, status: &str) -> bool {
    value.get("status").and_then(Value::as_str) == Some(status)
}

/// `Directory` (`feature-plugins/home/footer.tsx:12-25`): the
/// home-abbreviated destination directory plus `:branch` when a vcs
/// branch exists (the destination is the cwd).
fn footer_directory(app: &App) -> String {
    let directory = app
        .state
        .project
        .main_dir
        .clone()
        .or_else(|| app.state.project.instance_path.directory.clone())
        .unwrap_or_default();
    let home = app
        .state
        .project
        .instance_path
        .home
        .clone()
        .unwrap_or_default();
    let out = crate::ui::locale::abbreviate_home(&directory, &home);
    match app
        .state
        .sync
        .vcs
        .as_ref()
        .and_then(|vcs| vcs.get("branch"))
        .and_then(Value::as_str)
    {
        Some(branch) => format!("{out}:{branch}"),
        None => out,
    }
}

/// The `home_footer` slot (`feature-plugins/home/footer.tsx:64-82`):
/// Directory, Mcp, a flex-grow spacer, then the app version.
fn render_footer(app: &App, frame: &mut ratatui::Frame, theme: &Theme, area: Rect) {
    if area.width == 0 || area.height == 0 {
        return;
    }
    let [_, row, _] = Layout::vertical([Constraint::Length(1); 3]).areas(area);
    let mut spans: Vec<Span> = vec![Span::raw("  ")]; // paddingLeft=2
    spans.push(Span::styled(
        footer_directory(app),
        theme.text_muted.to_color(),
    ));
    if !app.state.sync.mcp.is_empty() {
        let err = app
            .state
            .sync
            .mcp
            .values()
            .any(|server| mcp_status(server, "failed"));
        let count = app
            .state
            .sync
            .mcp
            .values()
            .filter(|server| mcp_status(server, "connected"))
            .count();
        spans.push(Span::raw("  ")); // gap=2
        spans.push(Span::styled(
            "⊙ ",
            if err {
                theme.error
            } else if count > 0 {
                theme.success
            } else {
                theme.text_muted
            }
            .to_color(),
        ));
        spans.push(Span::styled(format!("{count} MCP"), theme.text.to_color()));
        spans.push(Span::raw(" ")); // gap=1
        spans.push(Span::styled("/status", theme.text_muted.to_color()));
    }
    let version = crate::ui::session::sidebar::INSTALLATION_VERSION;
    let used: usize = spans.iter().map(|span| span.content.chars().count()).sum();
    let remaining = (row.width as usize)
        .saturating_sub(used)
        .saturating_sub(version.chars().count())
        .saturating_sub(2); // paddingRight=2
    spans.push(Span::raw(" ".repeat(remaining)));
    spans.push(Span::styled(version, theme.text_muted.to_color()));
    Line::from(spans).render(row, frame.buffer_mut());
}

// ----------------------------------------------------------- home_bottom

const NO_MODELS_TIP: &str =
    "Run {highlight}/connect{/highlight} to add an AI provider and start coding";

/// `parse` (`feature-plugins/home/tips-view.tsx:40-60`).
fn parse_tip(tip: &str) -> Vec<(String, bool)> {
    let mut parts: Vec<(String, bool)> = Vec::new();
    let mut rest = tip;
    while let Some(start) = rest.find("{highlight}") {
        if start > 0 {
            parts.push((rest[..start].to_string(), false));
        }
        let after = &rest[start + "{highlight}".len()..];
        match after.find("{/highlight}") {
            Some(end) => {
                parts.push((after[..end].to_string(), true));
                rest = &after[end + "{/highlight}".len()..];
            }
            None => {
                rest = after;
                break;
            }
        }
    }
    if !rest.is_empty() {
        parts.push((rest.to_string(), false));
    }
    parts
}

/// `formatKeySequence` (`keymap.tsx:250-258`) — the transcript twin;
/// the first registered binding of a command, formatted for display.
fn command_shortcut(app: &App, command: &str) -> String {
    use crate::keymap::bindings::BindingValue;
    let Some(BindingValue::Alternatives(alternatives)) = app.keymap.bindings.get(command) else {
        return String::new();
    };
    let Some(sequence) = alternatives.first() else {
        return String::new();
    };
    sequence
        .iter()
        .map(|stroke| {
            let mut parts: Vec<String> = Vec::new();
            if stroke.ctrl {
                parts.push("ctrl".to_string());
            }
            if stroke.meta {
                parts.push("meta".to_string());
            }
            if stroke.super_key {
                parts.push("super".to_string());
            }
            if stroke.hyper {
                parts.push("hyper".to_string());
            }
            let single = stroke.key.chars().count() == 1;
            if stroke.shift {
                if single && stroke.key.chars().all(|c| c.is_ascii_lowercase()) {
                    parts.push(stroke.key.to_ascii_uppercase());
                    return parts.join("+");
                }
                parts.push("shift".to_string());
            }
            parts.push(stroke.key.clone());
            parts.join("+")
        })
        .collect::<Vec<_>>()
        .join(" ")
}

/// The `TIPS` array (`feature-plugins/home/tips-view.tsx:64-164`) plus
/// the platform tail tip, with resolved shortcut text.
fn tips_list(app: &App) -> Vec<String> {
    let shortcut = |command: &str| command_shortcut(app, command);
    let highlight = |text: &str| format!("{{highlight}}{text}{{/highlight}}");
    let press = |shortcut: String, text: &str| -> Option<String> {
        (!shortcut.is_empty()).then(|| format!("Press {} {text}", highlight(&shortcut)))
    };
    let command_text = |command: &str, shortcut: String| -> String {
        if shortcut.is_empty() {
            highlight(command)
        } else {
            format!("{} or {}", highlight(command), highlight(&shortcut))
        }
    };
    let agent_cycle = shortcut("agent.cycle");
    let input_paste = shortcut("prompt.paste");
    let editor_open = shortcut("prompt.editor");
    let model_list = shortcut("model.list");
    let theme_list = shortcut("theme.switch");
    let session_new = shortcut("session.new");
    let session_list = shortcut("session.list");
    let session_pin_toggle = shortcut("session.pin.toggle");
    let session_export = shortcut("session.export");
    let messages_copy = shortcut("messages.copy");
    let command_list = shortcut("command.palette.show");
    let leader = shortcut("leader");
    let model_cycle_recent = shortcut("model.cycle_recent");
    let session_sidebar_toggle = shortcut("session.sidebar.toggle");
    let session_first = shortcut("session.first");
    let session_last = shortcut("session.last");
    let input_newline = shortcut("input.newline");
    let prompt_clear = shortcut("prompt.clear");
    let session_interrupt = shortcut("session.interrupt");
    let session_parent = shortcut("session.parent");
    let session_child_first = shortcut("session.child.first");
    let session_child_previous = shortcut("session.child.previous");
    let session_child_next = shortcut("session.child.next");
    let session_timeline = shortcut("session.timeline");
    let messages_toggle_conceal = shortcut("session.toggle.conceal");
    let status_view = shortcut("alforria.status");
    let help_show = shortcut("help.show");
    let quick_switch_1 = shortcut("session.quick_switch.1");
    let quick_switch_9 = shortcut("session.quick_switch.9");
    let page_up = shortcut("session.page.up");
    let page_down = shortcut("session.page.down");
    let parent_child: Vec<String> = [
        session_parent,
        session_child_first,
        session_child_previous,
        session_child_next,
    ]
    .into_iter()
    .filter(|item| !item.is_empty())
    .map(|item| highlight(&item))
    .collect();
    let tips = vec![
        Some("Type {highlight}@{/highlight} followed by a filename to fuzzy search and attach files".to_string()),
        Some("Start a message with {highlight}!{/highlight} to run shell commands (e.g., {highlight}!ls -la{/highlight})".to_string()),
        press(agent_cycle, "to cycle between Build and Plan agents"),
        Some("Use {highlight}/undo{/highlight} to revert the last message and file changes".to_string()),
        Some("Use {highlight}/redo{/highlight} to restore previously undone messages and file changes".to_string()),
        Some("Run {highlight}/share{/highlight} to create a public opencode.ai link".to_string()),
        Some("Drag and drop images or PDFs into the terminal as context".to_string()),
        press(input_paste, "to paste images from your clipboard into the prompt"),
        Some(format!("Use {} to compose messages in your external editor", command_text("/editor", editor_open))),
        Some("Run {highlight}/init{/highlight} to auto-generate project rules based on your codebase".to_string()),
        Some(format!("Use {} to switch between available AI models", command_text("/models", model_list))),
        Some(format!(
            "Use {} to switch between {} built-in themes",
            command_text("/themes", theme_list),
            crate::ui::theme::all_themes().len()
        )),
        Some(format!("Use {} to start a fresh conversation session", command_text("/new", session_new))),
        Some(format!("Use {} to list, pin, and continue sessions", command_text("/sessions", session_list))),
        press(session_pin_toggle, "in the session list to pin one at the top"),
        (!quick_switch_1.is_empty() && !quick_switch_9.is_empty()).then(|| {
            format!(
                "Use {} through {} to switch pinned sessions",
                highlight(&quick_switch_1),
                highlight(&quick_switch_9)
            )
        }),
        Some("Run {highlight}/compact{/highlight} to summarize long sessions near context limits".to_string()),
        Some(format!("Use {} to save the conversation as Markdown", command_text("/export", session_export))),
        press(messages_copy, "to copy the assistant's last message to clipboard"),
        press(command_list.clone(), "to see all available actions and commands"),
        Some("Run {highlight}/connect{/highlight} to add API keys for 75+ supported LLM providers".to_string()),
        Some(format!(
            "The leader key is {}; combine with other keys for quick actions",
            highlight(&leader)
        )),
        press(model_cycle_recent, "to quickly switch between recently used models"),
        press(session_sidebar_toggle, "in a session to show or hide the sidebar panel"),
        (!page_up.is_empty() && !page_down.is_empty()).then(|| {
            format!(
                "Use {}/{} to navigate through conversation history",
                highlight(&page_up),
                highlight(&page_down)
            )
        }),
        press(session_first, "to jump to the beginning of the conversation"),
        press(session_last, "to jump to the most recent message"),
        press(input_newline, "to add newlines in your prompt"),
        press(prompt_clear, "when typing to clear the input field"),
        press(session_interrupt, "to stop the AI mid-response"),
        Some("Switch to {highlight}Plan{/highlight} agent for suggestions without making changes".to_string()),
        Some("Use {highlight}@agent-name{/highlight} in prompts to invoke specialized subagents".to_string()),
        (!parent_child.is_empty()).then(|| {
            format!("Use {} for parent/child sessions", parent_child.join(" / "))
        }),
        Some("Create {highlight}opencode.json{/highlight} for server settings, and {highlight}tui.json{/highlight} for TUI".to_string()),
        Some("Place TUI settings in {highlight}~/.config/opencode/tui.json{/highlight} for global config".to_string()),
        Some("Add {highlight}$schema{/highlight} to your config for autocomplete in your editor".to_string()),
        Some("Configure {highlight}model{/highlight} in config to set your default model".to_string()),
        Some("Override any keybind in {highlight}tui.json{/highlight} via the {highlight}keybinds{/highlight} section".to_string()),
        Some("Set any keybind to {highlight}none{/highlight} to disable it completely".to_string()),
        Some("Configure local or remote MCP servers in the {highlight}mcp{/highlight} config section".to_string()),
        Some("Add {highlight}.md{/highlight} files to {highlight}.opencode/commands/{/highlight} for reusable prompts".to_string()),
        Some("Use {highlight}$ARGUMENTS{/highlight}, {highlight}$1{/highlight}, {highlight}$2{/highlight} in custom commands for dynamic input".to_string()),
        Some("Use backticks to inject shell output (e.g., {highlight}`git status`{/highlight})".to_string()),
        Some("Add {highlight}.md{/highlight} files to {highlight}.opencode/agents/{/highlight} for specialized AI personas".to_string()),
        Some("Configure per-agent permissions for {highlight}edit{/highlight}, {highlight}bash{/highlight}, and {highlight}webfetch{/highlight} tools".to_string()),
        Some("Use patterns like {highlight}\"git *\": \"allow\"{/highlight} for granular bash permissions".to_string()),
        Some("Set {highlight}\"rm -rf *\": \"deny\"{/highlight} to block destructive commands".to_string()),
        Some("Configure {highlight}\"git push\": \"ask\"{/highlight} to require approval before pushing".to_string()),
        Some("Set {highlight}\"formatter\": true{/highlight} to enable built-in formatters".to_string()),
        Some("Set {highlight}\"formatter\": false{/highlight} to disable inherited formatters".to_string()),
        Some("Define custom formatter commands with file extensions in config".to_string()),
        Some("Set {highlight}\"lsp\": true{/highlight} to enable built-in LSP code analysis".to_string()),
        Some("Create {highlight}.ts{/highlight} files in {highlight}.opencode/tools/{/highlight} to define new LLM tools".to_string()),
        Some("Tool definitions can invoke scripts written in Python, Go, etc".to_string()),
        Some("Add {highlight}.ts{/highlight} files to {highlight}.opencode/plugins/{/highlight} for event hooks".to_string()),
        Some("Use plugins to send OS notifications when sessions complete".to_string()),
        Some("Create a plugin to prevent OpenCode from reading sensitive files".to_string()),
        Some("Use {highlight}opencode run{/highlight} for non-interactive scripting".to_string()),
        Some("Use {highlight}opencode --continue{/highlight} to resume the last session".to_string()),
        Some("Use {highlight}opencode run -f file.ts{/highlight} to attach files via CLI".to_string()),
        Some("Use {highlight}--format json{/highlight} for machine-readable output in scripts".to_string()),
        Some("Run {highlight}opencode serve{/highlight} for headless API access to OpenCode".to_string()),
        Some("Use {highlight}opencode run --attach{/highlight} to connect to a running server".to_string()),
        Some("Run {highlight}opencode upgrade{/highlight} to update to the latest version".to_string()),
        Some("Run {highlight}opencode auth list{/highlight} to see all configured providers".to_string()),
        Some("Run {highlight}opencode agent create{/highlight} for guided agent creation".to_string()),
        Some("Use {highlight}/opencode{/highlight} in GitHub issues/PRs to trigger AI actions".to_string()),
        Some("Run {highlight}opencode github install{/highlight} to set up the GitHub workflow".to_string()),
        Some("Comment {highlight}/opencode fix this{/highlight} on issues to auto-create PRs".to_string()),
        Some("Comment {highlight}/oc{/highlight} on PR code lines for targeted code reviews".to_string()),
        Some("Use {highlight}\"theme\": \"system\"{/highlight} to match your terminal's colors".to_string()),
        Some("Create JSON theme files in {highlight}.opencode/themes/{/highlight} directory".to_string()),
        Some("Themes support dark/light variants for both modes".to_string()),
        Some("Use numeric xterm color codes 0-255 in custom theme JSON".to_string()),
        Some("Use {highlight}{env:VAR_NAME}{/highlight} for environment variables in config".to_string()),
        Some("Use {highlight}{file:path}{/highlight} to include file contents in config values".to_string()),
        Some("Use {highlight}instructions{/highlight} in config to load additional rules files".to_string()),
        Some("Set agent {highlight}temperature{/highlight} from 0.0 (focused) to 1.0 (creative)".to_string()),
        Some("Configure {highlight}steps{/highlight} to limit agentic iterations per request".to_string()),
        Some("Set {highlight}\"tools\": {\"bash\": false}{/highlight} to disable specific tools".to_string()),
        Some("Set {highlight}\"mcp_*\": false{/highlight} to disable all tools from an MCP server".to_string()),
        Some("Override global tool settings per agent configuration".to_string()),
        Some("Set {highlight}\"share\": \"auto\"{/highlight} to automatically share all sessions".to_string()),
        Some("Set {highlight}\"share\": \"disabled\"{/highlight} to prevent any session sharing".to_string()),
        Some("Run {highlight}/unshare{/highlight} to remove a session from public access".to_string()),
        Some("Permission {highlight}doom_loop{/highlight} prevents infinite tool call loops".to_string()),
        Some("Permission {highlight}external_directory{/highlight} protects files outside project".to_string()),
        Some("Run {highlight}opencode debug config{/highlight} to troubleshoot configuration".to_string()),
        Some("Use {highlight}--print-logs{/highlight} flag to see detailed logs in stderr".to_string()),
        Some(format!("Use {} to jump to specific messages", command_text("/timeline", session_timeline))),
        press(messages_toggle_conceal, "to toggle code block visibility in messages"),
        Some(format!("Use {} to see system status info", command_text("/status", status_view))),
        Some("Enable {highlight}scroll_acceleration{/highlight} in {highlight}tui.json{/highlight} for smooth scrolling".to_string()),
        Some(match command_list.is_empty() {
            false => format!(
                "Toggle username display in chat via the command palette ({})",
                highlight(&command_list)
            ),
            true => "Toggle username display in chat via the command palette".to_string(),
        }),
        Some("Run {highlight}docker run -it --rm ghcr.io/anomalyco/opencode{/highlight} in a container".to_string()),
        Some("Use {highlight}/connect{/highlight} with OpenCode Zen for curated, tested models".to_string()),
        Some("Commit your project's {highlight}AGENTS.md{/highlight} file to Git for team sharing".to_string()),
        Some("Use {highlight}/review{/highlight} to review uncommitted changes, branches, or PRs".to_string()),
        Some(format!("Use {} to show the help dialog", command_text("/help", help_show))),
        Some("Use {highlight}/rename{/highlight} to rename the current session".to_string()),
        if cfg!(not(windows)) {
            press(shortcut("terminal.suspend"), "to suspend the terminal and return to your shell")
        } else {
            press(shortcut("input.undo"), "to undo changes in your prompt")
        },
    ];
    tips.into_iter().flatten().collect()
}

/// `tips.tsx:26-33` — `(!first || !connected) && !hidden`.
fn tips_shown(app: &App) -> bool {
    let first = app.state.sync.session.is_empty();
    (!first || !crate::state::connected(app)) && !app.state.kv.get_bool("tips_hidden", false)
}

/// The wrapped, centered tips rows (`tips-view.tsx:150-161`).
fn tip_lines(app: &App, theme: &Theme, width: u16) -> Vec<Line<'static>> {
    let tip = if crate::state::connected(app) {
        let list = tips_list(app);
        list[app.ui.home_tip % list.len()].clone()
    } else {
        NO_MODELS_TIP.to_string()
    };
    let mut rows: Vec<Vec<(String, bool)>> = vec![Vec::new()];
    let mut used = TIP_PREFIX_WIDTH;
    for (text, highlighted) in parse_tip(&tip) {
        for word in text.split(' ') {
            if word.is_empty() {
                continue;
            }
            let empty = rows.last().expect("rows starts with one row").is_empty();
            if !empty && used + 1 + word.chars().count() > width as usize {
                rows.push(Vec::new());
                used = TIP_PREFIX_WIDTH;
            }
            let last = rows.last_mut().expect("rows starts with one row");
            if !last.is_empty() {
                last.push((" ".to_string(), highlighted));
                used += 1;
            }
            last.push((word.to_string(), highlighted));
            used += word.chars().count();
        }
    }
    rows.into_iter()
        .map(|row| {
            let row_width = row
                .iter()
                .map(|(text, _)| text.chars().count())
                .sum::<usize>()
                + TIP_PREFIX_WIDTH;
            let mut spans = vec![Span::raw(
                " ".repeat((width as usize).saturating_sub(row_width) / 2),
            )];
            spans.push(Span::styled(TIP_PREFIX, theme.warning.to_color()));
            for (text, highlighted) in row {
                spans.push(Span::styled(
                    text,
                    if highlighted {
                        theme.text
                    } else {
                        theme.text_muted
                    }
                    .to_color(),
                ));
            }
            Line::from(spans)
        })
        .collect()
}

/// The `home_bottom` box (`tips.tsx:26-32`): `maxWidth={75}`,
/// `alignItems="center"`, `paddingTop={3}`.
fn render_tips(frame: &mut ratatui::Frame, area: Rect, width: u16, lines: &[Line<'static>]) {
    let box_area = center_horizontally(area, width);
    for (index, line) in lines.iter().enumerate() {
        let y = area.y + TIPS_PADDING_TOP + index as u16;
        if y >= area.bottom() {
            break;
        }
        line.render(
            Rect {
                y,
                height: 1,
                ..box_area
            },
            frame.buffer_mut(),
        );
    }
}

/// `Prompt` frame (`prompt/index.tsx:1352-1401`): left border +
/// `backgroundElement` fill + 2-col padding, a `SplitBorder` vertical
/// (`┃`) and the `╹`/`▀` bottom cap (`prompt/index.tsx:1487-1512`).
/// The textarea contents render over the seeded --prompt input (M8.6).
fn render_prompt(app: &App, frame: &mut ratatui::Frame, theme: &Theme, area: Rect) {
    let max_width = app
        .config
        .prompt_max_width(area.width.saturating_sub(4))
        .min(area.width);
    let display = rows(app, area);
    let placeholder = display.rows.iter().all(|row| row.is_empty());
    // `cursor.blinking` — the same block cursor as the session prompt.
    let blink = (app.ui.tick_ms / 530).is_multiple_of(2);
    let cursor = crate::ui::textarea::cursor_cell_style(theme);
    let mut lines: Vec<Line> = Vec::new();
    if placeholder {
        let mut spans: Vec<Span> = Vec::new();
        let text = placeholder_text(app);
        if blink {
            spans.push(Span::styled(
                text.chars().take(1).collect::<String>(),
                cursor,
            ));
        } else {
            spans.push(Span::styled(
                text.chars().take(1).collect::<String>(),
                theme.text_muted.to_color(),
            ));
        }
        spans.push(Span::styled(
            text.chars().skip(1).collect::<String>(),
            theme.text_muted.to_color(),
        ));
        lines.push(Line::from(spans));
    } else {
        for (row, cells) in display.rows.iter().enumerate() {
            let mut spans: Vec<Span> = Vec::new();
            for (column, (char, _mark)) in cells.iter().enumerate() {
                let mut style = Style::new().fg(theme.text.to_color());
                if blink && row == display.cursor_row && column == display.cursor_col {
                    style = cursor;
                }
                spans.push(Span::styled(char.to_string(), style));
            }
            if blink && row == display.cursor_row && display.cursor_col >= cells.len() {
                spans.push(Span::styled(" ", cursor));
            }
            if row == display.cursor_row && spans.is_empty() && blink {
                spans.push(Span::styled(" ", cursor));
            }
            lines.push(Line::from(spans));
        }
    }
    let prompt_area = center_horizontally(area, max_width);
    let main = Rect {
        height: prompt_area.height.saturating_sub(1),
        ..prompt_area
    };
    Paragraph::new(lines)
        .block(
            Block::new()
                .borders(ratatui::widgets::Borders::LEFT)
                .border_set(ratatui::symbols::border::Set {
                    vertical_left: "┃",
                    ..Default::default()
                })
                .border_style(theme.border.to_color())
                .style(Style::new().bg(theme.background_element.to_color()))
                .padding(ratatui::widgets::Padding {
                    left: 2,
                    right: 2,
                    top: 1,
                    bottom: 0,
                }),
        )
        .render(main, frame.buffer_mut());
    render_prompt_cap(
        frame,
        theme,
        Rect {
            y: prompt_area.y + prompt_area.height.saturating_sub(1),
            height: 1,
            ..prompt_area
        },
    );
    if app.ui.prompt.autocomplete.visible.is_some() {
        crate::ui::session::prompt::render_autocomplete(app, frame, theme, prompt_area);
    }
}

/// The 1-row bottom cap under the prompt (`prompt/index.tsx:1487-1512`):
/// a `╹` left border with a `▀` underline in `backgroundElement`.
fn render_prompt_cap(frame: &mut ratatui::Frame, theme: &Theme, area: Rect) {
    if area.width == 0 || area.height == 0 {
        return;
    }
    let (left, fill) = if theme.background_element.a != 0.0 {
        ("╹", "▀")
    } else {
        (" ", " ")
    };
    let mut spans = vec![Span::styled(left, theme.border.to_color())];
    let width = area.width.saturating_sub(1) as usize;
    if width > 0 {
        spans.push(Span::styled(
            fill.repeat(width),
            theme.background_element.to_color(),
        ));
    }
    Line::from(spans).render(area, frame.buffer_mut());
}

#[cfg(test)]
mod tests {
    use crate::config::{PromptMaxWidth, TuiConfig};

    use super::*;

    fn make_app() -> App {
        App::new(TuiConfig::default(), crate::state::Args::default(), None)
    }

    fn buffer_text(app: &App, width: u16, height: u16) -> Vec<String> {
        let backend = ratatui::backend::TestBackend::new(width, height);
        let mut terminal = ratatui::Terminal::new(backend).unwrap();
        let mut lines = Vec::new();
        terminal
            .draw(|frame| {
                let theme = app
                    .ui
                    .theme
                    .resolve(&app.state.kv)
                    .expect("builtin theme resolves");
                let area = frame.area();
                super::render(app, frame, &theme, area);
            })
            .unwrap();
        for y in 0..height {
            let mut line = String::new();
            for x in 0..width {
                line.push_str(terminal.backend().buffer()[(x, y)].symbol());
            }
            lines.push(line.trim_end().to_string());
        }
        lines
    }

    #[test]
    fn logo_renders_the_wordmark() {
        let lines = buffer_text(&make_app(), 80, 24);
        let joined = lines.join("\n");
        assert!(
            joined.contains(" ███  █     █████  ███  ████  ████  █████  ███"),
            "{joined}"
        );
        assert!(
            joined.contains("█████ █     ████  █   █ ████  ████    █   █████"),
            "{joined}"
        );
        assert!(
            joined.contains("█   █ █████ █      ███  █   █ █   █ █████ █   █"),
            "{joined}"
        );
    }

    #[test]
    fn prompt_renders_the_placeholder() {
        let lines = buffer_text(&make_app(), 80, 24);
        assert!(
            lines
                .iter()
                .any(|l| l.contains("Ask anything… \"Fix a TODO in the codebase\"")),
            "{lines:?}"
        );
    }

    /// Regression (battle-test round 4): the home prompt box renders the
    /// shared prompt editor — typed text must appear on the home screen.
    #[test]
    fn prompt_renders_typed_text() {
        let mut app = make_app();
        app.ui.prompt.textarea.insert_text("hello");
        let lines = buffer_text(&app, 80, 24);
        assert!(lines.iter().any(|l| l.contains("hello")), "{lines:?}");
    }

    #[test]
    fn placeholder_rolls_with_the_index() {
        let mut app = make_app();
        app.ui.home_placeholder = 1;
        let lines = buffer_text(&app, 80, 24);
        assert!(
            lines
                .iter()
                .any(|l| l.contains("Ask anything… \"What is the tech stack of this project?\"")),
            "{lines:?}"
        );
    }

    #[test]
    fn renders_at_three_widths() {
        for width in [60u16, 80, 200] {
            let lines = buffer_text(&make_app(), width, 24);
            assert!(
                lines
                    .iter()
                    .any(|l| l.contains("Ask anything… \"Fix a TODO in the codebase\"")),
                "{width}: {lines:?}"
            );
            assert!(
                lines.iter().any(|l| l.contains("█████")),
                "{width}: logo missing"
            );
        }
    }

    #[test]
    fn auto_prompt_width_scales() {
        let mut app = make_app();
        app.config = TuiConfig {
            prompt_max_width: PromptMaxWidth::Auto,
            ..TuiConfig::default()
        };
        let lines = buffer_text(&app, 140, 24);
        assert!(
            lines
                .iter()
                .any(|l| l.contains("Ask anything… \"Fix a TODO in the codebase\"")),
            "{lines:?}"
        );
    }

    #[test]
    fn golden_home_80x24() {
        let lines = buffer_text(&make_app(), 80, 24);
        let pad = |n| " ".repeat(n);
        assert_eq!(lines.len(), 24);
        // vertical: fill(2) + gap(4) + logo(5) + spacer(1) + prompt(3)
        // + tips(4) + fill(2) + footer(3)
        assert_eq!(
            lines[6],
            format!("{} ███  █     █████  ███  ████  ████  █████  ███", pad(16))
        );
        assert_eq!(
            lines[7],
            format!("{}█   █ █     █     █   █ █   █ █   █   █   █   █", pad(16))
        );
        assert_eq!(
            lines[8],
            format!("{}█████ █     ████  █   █ ████  ████    █   █████", pad(16))
        );
        assert_eq!(
            lines[9],
            format!("{}█   █ █     █     █   █ █  █  █  █    █   █   █", pad(16))
        );
        assert_eq!(
            lines[10],
            format!("{}█   █ █████ █      ███  █   █ █   █ █████ █   █", pad(16))
        );
        assert_eq!(lines[12], format!("{}┃", pad(2)));
        assert_eq!(
            lines[13],
            format!("{}┃  Ask anything… \"Fix a TODO in the codebase\"", pad(2))
        );
        assert_eq!(lines[14], format!("{}╹{}", pad(2), "▀".repeat(74)));
        assert_eq!(
            lines[18],
            format!(
                "{}● Tip Run /connect to add an AI provider and start coding",
                pad(11)
            )
        );
        assert_eq!(lines[22], format!("{}dev", pad(75)));
        for (row, line) in lines.iter().enumerate() {
            if ![6, 7, 8, 9, 10, 12, 13, 14, 18, 22].contains(&row) {
                assert_eq!(line, "", "row {row}: {}", lines[row]);
            }
        }
    }

    #[test]
    fn golden_home_styles() {
        // The left half renders muted, the right half bold in `text`.
        let app = make_app();
        let backend = ratatui::backend::TestBackend::new(80, 24);
        let mut terminal = ratatui::Terminal::new(backend).unwrap();
        terminal
            .draw(|frame| {
                let theme = app
                    .ui
                    .theme
                    .resolve(&app.state.kv)
                    .expect("builtin theme resolves");
                super::render(&app, frame, &theme, frame.area());
            })
            .unwrap();
        let buffer = terminal.backend().buffer();
        let cell = |column: u16| buffer[(column, 8)].clone();
        // Left half `█` in textMuted, no modifiers.
        assert_eq!(cell(20).fg, ratatui::style::Color::Rgb(0x80, 0x80, 0x80));
        assert_eq!(cell(20).modifier, ratatui::style::Modifier::empty());
        // Right half `█` in text + bold.
        assert_eq!(cell(40).fg, ratatui::style::Color::Rgb(0xee, 0xee, 0xee));
        assert_eq!(
            cell(40).modifier,
            ratatui::style::Modifier::BOLD,
            "right logo half is bold"
        );
    }

    #[test]
    fn placeholder_sets_are_the_ts_ones() {
        assert_eq!(
            PLACEHOLDER_NORMAL,
            [
                "Fix a TODO in the codebase",
                "What is the tech stack of this project?",
                "Fix broken tests",
            ]
        );
        assert_eq!(PLACEHOLDER_SHELL, ["ls -la", "git status", "pwd"]);
    }

    #[test]
    fn parse_tip_splits_highlight_marks() {
        let parts = parse_tip("Use {highlight}/undo{/highlight} to revert");
        assert_eq!(
            parts,
            vec![
                ("Use ".to_string(), false),
                ("/undo".to_string(), true),
                (" to revert".to_string(), false),
            ]
        );
    }

    #[test]
    fn tips_list_covers_the_ts_array() {
        let app = make_app();
        let tips = tips_list(&app);
        assert!(!tips.is_empty(), "{tips:?}");
        assert!(
            tips.contains(&"Type {highlight}@{/highlight} followed by a filename to fuzzy search and attach files".to_string()),
            "{tips:?}"
        );
        assert!(
            tips.contains(
                &"Use {highlight}/rename{/highlight} to rename the current session".to_string()
            ),
            "{tips:?}"
        );
        for tip in &tips {
            assert!(!tip.is_empty());
            assert!(
                !tip.contains("{highlight}") || tip.contains("{/highlight}"),
                "{tip}"
            );
        }
    }

    #[test]
    fn footer_renders_directory_mcp_and_version() {
        let mut app = make_app();
        app.state.project.main_dir = Some("/home/jon/repo".into());
        app.state.project.instance_path.home = Some("/home/jon".into());
        app.state.sync.vcs = Some(serde_json::json!({ "branch": "main" }));
        app.state.sync.mcp.insert(
            "a".to_string(),
            serde_json::json!({ "status": "connected" }),
        );
        app.state.sync.mcp.insert(
            "b".to_string(),
            serde_json::json!({ "status": "connected" }),
        );
        app.state
            .sync
            .mcp
            .insert("c".to_string(), serde_json::json!({ "status": "failed" }));
        let lines = buffer_text(&app, 80, 24);
        assert!(
            lines[22].starts_with("  ~/repo:main  ⊙ 2 MCP /status"),
            "{}",
            lines[22]
        );
        assert!(lines[22].ends_with("dev"), "{}", lines[22]);
    }

    #[test]
    fn tips_visibility_follows_the_ts_show_memo() {
        let mut app = make_app();
        // !connected → show (the NO_MODELS tip).
        assert!(tips_shown(&app));
        // connected + no sessions → hide.
        app.state.sync.provider = vec![serde_json::json!({ "id": "anthropic" })];
        assert!(!tips_shown(&app));
        // sessions exist → show again, now from the tips array.
        app.state.sync.session = vec![alforria_schema::session_v1::V1SessionInfo {
            id: "ses_1".into(),
            slug: "x".into(),
            project_id: "prj".into(),
            workspace_id: None,
            directory: "/x".into(),
            path: None,
            parent_id: None,
            summary: None,
            cost: None,
            tokens: None,
            share: None,
            title: "X".into(),
            agent: None,
            model: None,
            version: "1".into(),
            metadata: None,
            time: alforria_schema::session_v1::V1SessionTime {
                created: 0,
                updated: 0,
                compacting: None,
                archived: None,
            },
            permission: None,
            revert: None,
        }];
        assert!(tips_shown(&app));
        app.ui.home_tip = 0;
        let theme = app
            .ui
            .theme
            .resolve(&app.state.kv)
            .expect("builtin theme resolves");
        let lines = tip_lines(&app, &theme, 75);
        let rendered: Vec<String> = lines
            .iter()
            .flat_map(|line| line.spans.iter().map(|span| span.content.to_string()))
            .collect();
        assert!(
            rendered
                .iter()
                .all(|span| !span.contains("/connect to add an AI provider")),
            "connected + sessions picks from the tips array"
        );
    }
}
