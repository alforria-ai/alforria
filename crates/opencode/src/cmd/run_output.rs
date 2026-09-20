//! `cli/cmd/run/tool.ts` inline display rules + run.ts:73-124 output
//! primitives — the formatted-mode rendering surface for the event loop.

use serde_json::Value;

use crate::ui;

/// `ToolInline` (tool.ts:71-78).
#[derive(Debug, Clone, PartialEq, Default)]
pub struct Inline {
    pub icon: String,
    pub title: String,
    pub description: Option<String>,
    pub block: bool,
    pub body: Option<String>,
}

impl Inline {
    fn new_inline(icon: &str, title: String) -> Self {
        Inline {
            icon: icon.to_string(),
            title,
            description: None,
            block: false,
            body: None,
        }
    }

    fn new_block(icon: &str, title: String, body: Option<String>) -> Self {
        Inline {
            icon: icon.to_string(),
            title,
            description: None,
            block: true,
            body,
        }
    }
}

fn text(value: Option<&Value>) -> String {
    value
        .filter(|v| v.is_string())
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string()
}

fn dict(value: Option<&Value>) -> serde_json::Map<String, Value> {
    value
        .filter(|v| v.is_object())
        .and_then(|v| v.as_object())
        .cloned()
        .unwrap_or_default()
}

/// `info(data, skip)` — `[key=value, key2=value2]` of string/number/boolean
/// entries (tool.ts:177-193).
fn info(data: &serde_json::Map<String, Value>, skip: &[&str]) -> String {
    let list = data
        .iter()
        .filter(|(key, val)| {
            !skip.contains(&key.as_str())
                && (val.is_string() || val.is_number() || val.is_boolean())
        })
        .map(|(key, val)| match val {
            Value::String(string) => format!("{key}={string}"),
            Value::Bool(flag) => format!("{key}={flag}"),
            other => format!("{key}={other}"),
        })
        .collect::<Vec<_>>();
    if list.is_empty() {
        return String::new();
    }
    format!("[{}]", list.join(", "))
}

/// `count(n, label)` — `1 match`, `2 matches` (tool.ts:225-227).
fn count(n: i64, label: &str) -> String {
    format!("{n} {label}{}", if n == 1 { "" } else { "es" })
}

/// `Locale.titlecase` — uppercase every word-boundary character.
fn titlecase(input: &str) -> String {
    let mut out = String::with_capacity(input.len());
    let mut at_boundary = true;
    for ch in input.chars() {
        if ch.is_alphanumeric() {
            out.push(if at_boundary {
                ch.to_uppercase().next().unwrap_or(ch)
            } else {
                ch
            });
            at_boundary = false;
        } else {
            out.push(ch);
            at_boundary = true;
        }
    }
    out
}

/// `webSearchProviderLabel` (tool/websearch.ts:39-43).
fn web_search_provider_label(provider: Option<&Value>) -> String {
    match provider.and_then(|v| v.as_str()) {
        Some("parallel") => "Parallel Web Search".to_string(),
        Some("exa") => "Exa Web Search".to_string(),
        _ => "Web Search".to_string(),
    }
}

/// `toolPath` — cwd-relative path, `~` for home with `opts.home`
/// (tool.ts:250-270).
fn tool_path(input: Option<&Value>, home: bool) -> String {
    let Some(input) = input.and_then(|v| v.as_str()) else {
        return String::new();
    };
    let cwd = std::env::current_dir().unwrap_or_default();
    let path = std::path::Path::new(input);
    let abs = if path.is_absolute() {
        path.to_path_buf()
    } else {
        cwd.join(path)
    };
    let rel = match abs.strip_prefix(&cwd) {
        Ok(rel) => rel.display().to_string(),
        // Outside the cwd — `relative()` yields a `..` path, so the absolute
        // path is returned verbatim.
        _ => return abs.display().to_string(),
    };
    if rel.is_empty() {
        return ".".to_string();
    }
    if !rel.starts_with("..") {
        return rel;
    }
    if home {
        if let Some(home_dir) = std::env::var_os("HOME") {
            let home_dir = std::path::PathBuf::from(home_dir);
            if abs == home_dir {
                return "~".to_string();
            }
            if let Ok(rest) = abs.strip_prefix(&home_dir) {
                return format!("~/{}", rest.display());
            }
        }
    }
    abs.display().to_string()
}

/// `fallbackInline` (tool.ts:232-241).
fn fallback_inline(ctx: &ToolFrame) -> Inline {
    let title = text(ctx.state.get("title"));
    let title = if !title.is_empty() {
        title
    } else if !ctx.input.is_empty() {
        serde_json::to_string(&Value::Object(ctx.input.clone())).unwrap_or_default()
    } else {
        "Unknown".to_string()
    };
    Inline::new_inline("⚙", format!("{} {title}", ctx.name))
}

/// One per-tool render input — the `frame(part)` closure
/// (tool.ts:1247-1257).
struct ToolFrame {
    name: String,
    input: serde_json::Map<String, Value>,
    metadata: serde_json::Map<String, Value>,
    state: serde_json::Map<String, Value>,
    status: String,
}

fn frame(part: &Value) -> ToolFrame {
    let state = dict(part.get("state"));
    ToolFrame {
        name: text(part.get("tool")),
        input: dict(state.get("input")),
        metadata: dict(state.get("metadata")),
        status: text(state.get("status")),
        state,
    }
}

fn str_of(map: &serde_json::Map<String, Value>, key: &str) -> String {
    text(map.get(key))
}

/// `runGlob`/`runGrep` share the `"{name} "pattern" in path · N matches"`
/// shape (tool.ts:265-296).
fn run_glob_grep(name: &str, ctx: &ToolFrame, meta_key: &str) -> Inline {
    let root = str_of(&ctx.input, "path");
    let suffix = if root.is_empty() {
        String::new()
    } else {
        format!("in {}", tool_path(Some(&Value::String(root)), false))
    };
    let description = match ctx.metadata.get(meta_key).and_then(|v| v.as_i64()) {
        Some(matches) if !suffix.is_empty() => format!("{suffix} · {}", count(matches, "match")),
        Some(matches) => count(matches, "match"),
        None => suffix,
    };
    let title = format!("{name} \"{}\"", str_of(&ctx.input, "pattern"),);
    Inline::new_inline("✱", title).map_description(description)
}

/// Helper to attach a description when non-empty.
trait MapDescription {
    fn map_description(self, description: String) -> Inline;
}

impl MapDescription for Inline {
    fn map_description(mut self, description: String) -> Inline {
        if !description.is_empty() {
            self.description = Some(description);
        }
        self
    }
}

fn lsp_title(input: &serde_json::Map<String, Value>, home: bool) -> String {
    let op = match input.get("operation").and_then(|v| v.as_str()) {
        Some(op) if !op.is_empty() => op.to_string(),
        _ => "request".to_string(),
    };
    let file = input
        .get("filePath")
        .and_then(|v| v.as_str())
        .map(|file| tool_path(Some(&Value::String(file.to_string())), home))
        .unwrap_or_default();
    let line = input.get("line").and_then(|v| v.as_i64());
    let character = input.get("character").and_then(|v| v.as_i64());
    let pos = match (line, character) {
        (Some(line), Some(character)) => format!(":{line}:{character}"),
        _ => String::new(),
    };
    if file.is_empty() {
        return format!("LSP {op}");
    }
    format!("LSP {op} {file}{pos}")
}

/// `runLsp` (tool.ts:381-386).
fn run_lsp(ctx: &ToolFrame) -> Inline {
    let title = text(ctx.state.get("title"));
    Inline::new_inline(
        "→",
        if title.is_empty() {
            lsp_title(&ctx.input, false)
        } else {
            title
        },
    )
}

/// `runTask` (tool.ts:328-344).
fn run_task(ctx: &ToolFrame) -> Inline {
    let raw_kind = str_of(&ctx.input, "subagent_type");
    let kind = titlecase(if raw_kind.is_empty() {
        "unknown"
    } else {
        &raw_kind
    });
    let desc = str_of(&ctx.input, "description");
    let icon = if ctx.status == "error" {
        "✗"
    } else if ctx.status == "running" {
        "•"
    } else {
        "✓"
    };
    let title = if desc.is_empty() {
        format!("{kind} Task")
    } else {
        desc.clone()
    };
    let description = if desc.is_empty() {
        String::new()
    } else {
        format!("{kind} Agent")
    };
    Inline::new_inline(icon, title).map_description(description)
}

/// `runTodo` (tool.ts:347-370).
fn run_todo(ctx: &ToolFrame) -> Inline {
    let body = ctx
        .input
        .get("todos")
        .and_then(|v| v.as_array())
        .map(|items| {
            items
                .iter()
                .filter_map(|item| {
                    let content = item.get("content").and_then(|v| v.as_str())?;
                    if content.is_empty() {
                        return None;
                    }
                    let status = item.get("status").and_then(|v| v.as_str());
                    let mark = if status == Some("completed") {
                        "[✓]"
                    } else if status == Some("in_progress") {
                        "[•]"
                    } else {
                        "[ ]"
                    };
                    Some(format!("{mark} {content}"))
                })
                .collect::<Vec<_>>()
                .join("\n")
        })
        .unwrap_or_default();
    Inline::new_block("#", "Todos".to_string(), Some(body))
}

/// `runWebSearch` (tool.ts:317-324).
fn run_web_search(ctx: &ToolFrame) -> Inline {
    let title = web_search_provider_label(ctx.metadata.get("provider"));
    let query = str_of(&ctx.input, "query");
    Inline::new_inline(
        "◈",
        if query.is_empty() {
            title
        } else {
            format!("{title} \"{query}\"")
        },
    )
}

/// `runPatch` (tool.ts:354-366).
fn run_patch(ctx: &ToolFrame) -> Inline {
    let files = ctx
        .metadata
        .get("files")
        .and_then(|v| v.as_array())
        .map(|files| files.len())
        .unwrap_or(0);
    if files == 0 {
        return Inline::new_inline("%", "Patch".to_string());
    }
    Inline::new_inline(
        "%",
        format!("Patch {files} file{}", if files == 1 { "" } else { "s" }),
    )
}

/// `toolInlineInfo` (tool.ts:1300-1311): the per-tool display rule table.
pub fn tool_inline_info(part: &Value) -> Inline {
    let ctx = frame(part);
    let input = &ctx.input;
    let status_completed = ctx.status == "completed";
    match ctx.name.as_str() {
        "invalid" => Inline::new_block(
            "✗",
            {
                let title = text(ctx.state.get("title"));
                if title.is_empty() {
                    "Invalid Tool".to_string()
                } else {
                    title
                }
            },
            status_completed.then(|| text(ctx.state.get("output"))),
        ),
        "bash" => Inline::new_block(
            "$",
            str_of(input, "command"),
            status_completed.then(|| text(ctx.state.get("output")).trim().to_string()),
        ),
        "write" => Inline::new_block(
            "←",
            format!("Write {}", tool_path(input.get("filePath"), false)),
            status_completed.then(|| text(ctx.state.get("output"))),
        ),
        "edit" => Inline::new_block(
            "←",
            format!("Edit {}", tool_path(input.get("filePath"), false)),
            ctx.metadata
                .get("diff")
                .and_then(|v| v.as_str())
                .map(str::to_string),
        ),
        "apply_patch" => run_patch(&ctx),
        "batch" => {
            let calls = input
                .get("tool_calls")
                .and_then(|v| v.as_array())
                .map(|calls| calls.len())
                .unwrap_or(0);
            let title = {
                let state_title = text(ctx.state.get("title"));
                if !state_title.is_empty() {
                    state_title
                } else if calls > 0 {
                    format!("Batch {calls} tool{}", if calls == 1 { "" } else { "s" })
                } else {
                    "Batch".to_string()
                }
            };
            Inline::new_block(
                "#",
                title,
                status_completed.then(|| text(ctx.state.get("output"))),
            )
        }
        "task" => run_task(&ctx),
        "todowrite" => run_todo(&ctx),
        "question" => {
            let total = input
                .get("questions")
                .and_then(|v| v.as_array())
                .map(|questions| questions.len())
                .unwrap_or(0);
            Inline::new_inline(
                "→",
                format!(
                    "Asked {total} question{}",
                    if total == 1 { "" } else { "s" }
                ),
            )
        }
        "read" => {
            let description = info(input, &["filePath"]);
            Inline::new_inline(
                "→",
                format!("Read {}", tool_path(input.get("filePath"), false)),
            )
            .map_description(description)
        }
        "glob" => run_glob_grep("Glob", &ctx, "count"),
        "grep" => run_glob_grep("Grep", &ctx, "matches"),
        "list" => {
            let dir = str_of(input, "path");
            let title = if dir.is_empty() {
                "List".to_string()
            } else {
                format!("List {}", tool_path(Some(&Value::String(dir)), false))
            };
            Inline::new_inline("→", title)
        }
        "lsp" => run_lsp(&ctx),
        "webfetch" => {
            let url = str_of(input, "url");
            Inline::new_inline(
                "%",
                if url.is_empty() {
                    "WebFetch".to_string()
                } else {
                    format!("WebFetch {url}")
                },
            )
        }
        "websearch" => run_web_search(&ctx),
        "skill" => Inline::new_inline("→", format!("Skill \"{}\"", str_of(input, "name"))),
        "plan_exit" => {
            let title = text(ctx.state.get("title"));
            Inline::new_block(
                "→",
                if title.is_empty() {
                    "Switching to build agent".to_string()
                } else {
                    title
                },
                status_completed.then(|| text(ctx.state.get("output"))),
            )
        }
        _ => fallback_inline(&ctx),
    }
}

/// run.ts:73-76 — `UI.println` over the joined style strings.
pub fn inline_line(info: &Inline) -> String {
    let mut title_suffix = String::new();
    if let Some(description) = &info.description {
        title_suffix.push_str(&format!(
            "{} {description}{}",
            ui::style::TEXT_DIM,
            ui::style::TEXT_NORMAL
        ));
    }
    format!(
        "{}{} {}{}{}",
        ui::style::TEXT_NORMAL,
        info.icon,
        ui::style::TEXT_NORMAL,
        info.title,
        title_suffix
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn tool_part(name: &str, state: Value) -> Value {
        json!({
            "id": "prt_1",
            "sessionID": "ses_1",
            "messageID": "msg_1",
            "callID": "call_1",
            "tool": name,
            "state": state,
        })
    }

    #[test]
    fn count_pluralizes_with_es() {
        assert_eq!(count(1, "match"), "1 match");
        assert_eq!(count(3, "match"), "3 matches");
    }

    #[test]
    fn titlecase_uppercases_word_boundaries() {
        assert_eq!(titlecase("general agent"), "General Agent");
        assert_eq!(titlecase("x"), "X");
    }

    #[test]
    fn web_search_label_matches_providers() {
        assert_eq!(
            web_search_provider_label(Some(&json!("parallel"))),
            "Parallel Web Search"
        );
        assert_eq!(web_search_provider_label(Some(&json!("x"))), "Web Search");
        assert_eq!(web_search_provider_label(None), "Web Search");
    }

    #[test]
    fn bash_completed_renders_trimmed_output() {
        let part = tool_part(
            "bash",
            json!({
                "status": "completed",
                "input": {"command": "ls"},
                "output": "  hi  ",
                "metadata": {},
                "time": {"start": 1, "end": 2},
            }),
        );
        let inline = tool_inline_info(&part);
        assert_eq!(inline.icon, "$");
        assert_eq!(inline.title, "ls");
        assert!(inline.block);
        assert_eq!(inline.body.as_deref(), Some("hi"));
    }

    #[test]
    fn edit_uses_metadata_diff() {
        let part = tool_part(
            "edit",
            json!({
                "status": "completed",
                "input": {"filePath": "/x"},
                "metadata": {"diff": "+a"},
                "time": {"start": 1, "end": 2},
            }),
        );
        let inline = tool_inline_info(&part);
        assert_eq!(inline.title, "Edit /x");
        assert_eq!(inline.body.as_deref(), Some("+a"));
    }

    #[test]
    fn read_uses_description_info() {
        let part = tool_part(
            "read",
            json!({
                "status": "completed",
                "input": {"filePath": "/a", "line": 1},
                "metadata": {},
                "time": {"start": 1, "end": 2},
            }),
        );
        let inline = tool_inline_info(&part);
        assert_eq!(inline.icon, "→");
        assert_eq!(inline.description.as_deref(), Some("[line=1]"));
    }

    #[test]
    fn glob_uses_match_count() {
        let part = tool_part(
            "glob",
            json!({
                "status": "completed",
                "input": {"pattern": "*.rs", "path": "/a"},
                "metadata": {"count": 2},
                "time": {"start": 1, "end": 2},
            }),
        );
        let inline = tool_inline_info(&part);
        assert_eq!(inline.icon, "✱");
        assert!(inline.title.contains("\"*.rs\""));
        assert_eq!(inline.description.as_deref(), Some("in /a · 2 matches"));
    }

    #[test]
    fn task_running_uses_bullet() {
        let part = tool_part(
            "task",
            json!({
                "status": "running",
                "input": {"subagent_type": "general", "description": "Do it"},
                "metadata": {},
                "time": {"start": 1},
            }),
        );
        let inline = tool_inline_info(&part);
        assert_eq!(inline.icon, "•");
        assert_eq!(inline.title, "Do it");
        assert_eq!(inline.description.as_deref(), Some("General Agent"));
    }

    #[test]
    fn task_without_description_uses_kind() {
        let part = tool_part(
            "task",
            json!({
                "status": "completed",
                "input": {"subagent_type": "general"},
                "metadata": {},
                "time": {"start": 1, "end": 2},
            }),
        );
        let inline = tool_inline_info(&part);
        assert_eq!(inline.icon, "✓");
        assert_eq!(inline.title, "General Task");
        assert_eq!(inline.description, None);
    }

    #[test]
    fn unknown_tool_falls_back() {
        let part = tool_part(
            "mystery",
            json!({
                "status": "completed",
                "input": {"x": 1},
                "metadata": {},
                "time": {"start": 1, "end": 2},
            }),
        );
        let inline = tool_inline_info(&part);
        assert_eq!(inline.icon, "⚙");
        assert_eq!(inline.title, "mystery {\"x\":1}");
    }

    #[test]
    fn inline_line_joins_icon_and_title() {
        let line = inline_line(&Inline::new_inline("→", "Read x".to_string()));
        assert_eq!(line, format!("\x1b[0m→ \x1b[0mRead x"),);
    }

    #[test]
    fn inline_line_appends_dim_description() {
        let inline = Inline {
            description: Some("extra".to_string()),
            ..Inline::new_inline("→", "Read x".to_string())
        };
        let line = inline_line(&inline);
        assert_eq!(line, format!("\x1b[0m→ \x1b[0mRead x\x1b[90m extra\x1b[0m"),);
    }
}
