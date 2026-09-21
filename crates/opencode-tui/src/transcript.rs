//! `util/transcript.ts` — `formatTranscript`/`formatMessage`/
//! `formatPart`: the `session.copy` / `session.export` transcript
//! formatter.

use serde_json::Value;

use opencode_schema::session_v1::{V1Message, V1Part, V1SessionInfo, V1ToolState};

use crate::ui::locale;

/// `formatTranscript`'s `options` (`util/transcript.ts:5-13`).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct TranscriptOptions {
    pub thinking: bool,
    pub tool_details: bool,
    pub assistant_metadata: bool,
}

/// One `{ info, parts }` pair of the message list.
pub struct MessageParts {
    pub info: V1Message,
    pub parts: Vec<V1Part>,
}

/// `Model.name(providers, providerID, modelID)` (`util/model.ts:22-27`)
/// — the model display name, falling back to the id.
fn model_name(providers: &[Value], provider_id: &str, model_id: &str) -> String {
    let model = providers
        .iter()
        .find(|provider| provider.get("id").and_then(Value::as_str) == Some(provider_id))
        .and_then(|provider| provider.get("models"))
        .and_then(Value::as_object)
        .and_then(|models| models.get(model_id));
    model
        .and_then(|model| model.get("name"))
        .and_then(Value::as_str)
        .map(str::to_string)
        .unwrap_or_else(|| model_id.to_string())
}

/// `formatTranscript` (`util/transcript.ts:26-58`).
pub fn format_transcript(
    session: &V1SessionInfo,
    messages: &[MessageParts],
    options: &TranscriptOptions,
    providers: &[Value],
) -> String {
    let mut transcript = format!("# {}\n\n", session.title);
    transcript += &format!("**Session ID:** {}\n", session.id);
    transcript += &format!(
        "**Created:** {}\n",
        locale::to_locale_string(session.time.created)
    );
    transcript += &format!(
        "**Updated:** {}\n\n",
        locale::to_locale_string(session.time.updated)
    );
    transcript += "---\n\n";

    let mut sorted: Vec<&MessageParts> = messages.iter().collect();
    sorted.sort_by(|x, y| {
        let (a, b) = (message_created(&x.info), message_created(&y.info));
        a.partial_cmp(&b)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then_with(|| message_id(&x.info).cmp(message_id(&y.info)))
    });
    for message in sorted {
        transcript += &format_message(message, options, providers);
        transcript += "---\n\n";
    }

    transcript
}

fn message_id(message: &V1Message) -> &str {
    match message {
        V1Message::User { id, .. } | V1Message::Assistant { id, .. } => id,
    }
}

/// `a.info.time.created` — the user time is fractional (`f64`), the
/// assistant time integral (`u64`).
fn message_created(message: &V1Message) -> f64 {
    match message {
        V1Message::User { time, .. } => time.created,
        V1Message::Assistant { time, .. } => time.created as f64,
    }
}

/// `formatMessage` (`util/transcript.ts:60-75`).
pub fn format_message(
    message: &MessageParts,
    options: &TranscriptOptions,
    providers: &[Value],
) -> String {
    let mut result = String::new();
    match &message.info {
        V1Message::User { .. } => {
            result += "## User\n\n";
        }
        assistant @ V1Message::Assistant { .. } => {
            result += &format_assistant_header(assistant, options.assistant_metadata, providers);
        }
    }
    for part in &message.parts {
        result += &format_part(part, options);
    }
    result
}

/// `formatAssistantHeader` (`util/transcript.ts:77-90`).
fn format_assistant_header(
    message: &V1Message,
    include_metadata: bool,
    providers: &[Value],
) -> String {
    let V1Message::Assistant {
        time,
        agent,
        model_id,
        provider_id,
        ..
    } = message
    else {
        return String::new();
    };
    if !include_metadata {
        return "## Assistant\n\n".to_string();
    }
    let duration = match (time.completed, time.created) {
        (Some(completed), created) if completed >= created => {
            format!("{:.1}s", (completed - created) as f64 / 1000.0)
        }
        _ => String::new(),
    };
    let name = model_name(providers, provider_id, model_id);
    let mut line = format!(
        "## Assistant ({} · {name}",
        crate::ui::locale::titlecase(agent)
    );
    if !duration.is_empty() {
        line += &format!(" · {duration}");
    }
    line += ")\n\n";
    line
}

/// `formatPart` (`util/transcript.ts:92-120`).
pub fn format_part(part: &V1Part, options: &TranscriptOptions) -> String {
    match part {
        V1Part::Text {
            text, synthetic, ..
        } if !synthetic.unwrap_or(false) => {
            format!("{text}\n\n")
        }
        V1Part::Reasoning { text, .. } => {
            if options.thinking {
                format!("_Thinking:_\n\n{text}\n\n")
            } else {
                String::new()
            }
        }
        V1Part::Tool { tool, state, .. } => {
            // `part.state.input` gates on truthiness for every status
            // (transcript.ts:100-107), and completed/error render only
            // when their payload is non-empty.
            let mut result = format!("**Tool: {tool}**\n");
            if options.tool_details {
                let input = match &state {
                    V1ToolState::Pending { input, .. }
                    | V1ToolState::Running { input, .. }
                    | V1ToolState::Completed { input, .. }
                    | V1ToolState::Error { input, .. } => input,
                };
                if !input.is_empty() {
                    result += &format!("\n**Input:**\n```json\n{}\n```\n", pretty_json(input));
                }
                if let V1ToolState::Completed { output, .. } = state {
                    if !output.is_empty() {
                        result += &format!("\n**Output:**\n```\n{output}\n```\n");
                    }
                }
                if let V1ToolState::Error { error, .. } = state {
                    if !error.is_empty() {
                        result += &format!("\n**Error:**\n```\n{error}\n```\n");
                    }
                }
            }
            result += "\n";
            result
        }
        _ => String::new(),
    }
}

fn pretty_json(value: &serde_json::Map<String, Value>) -> String {
    serde_json::to_string_pretty(value).unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    fn session() -> V1SessionInfo {
        V1SessionInfo {
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
            title: "Demo session".into(),
            agent: None,
            model: None,
            version: "1".into(),
            metadata: None,
            time: opencode_schema::session_v1::V1SessionTime {
                created: 1_700_000_000_000,
                updated: 1_700_000_000_000,
                compacting: None,
                archived: None,
            },
            permission: None,
            revert: None,
        }
    }

    fn user_message(id: &str, created: f64) -> V1Message {
        V1Message::User {
            id: id.into(),
            session_id: "ses_1".into(),
            time: opencode_schema::session_v1::UserTime { created },
            format: None,
            summary: None,
            agent: "build".into(),
            model: opencode_schema::session_v1::V1UserModel {
                provider_id: "anthropic".into(),
                model_id: "claude".into(),
                variant: None,
            },
            system: None,
            tools: None,
        }
    }

    fn assistant_message(id: &str, created: u64) -> V1Message {
        V1Message::Assistant {
            id: id.into(),
            session_id: "ses_1".into(),
            time: opencode_schema::session_v1::AssistantTime {
                created,
                completed: Some(created + 1_500),
            },
            error: None,
            parent_id: "msg_1".into(),
            model_id: "claude".into(),
            provider_id: "anthropic".into(),
            mode: "primary".into(),
            agent: "build".into(),
            path: opencode_schema::session_v1::V1Path {
                cwd: "/x".into(),
                root: "/x".into(),
            },
            summary: None,
            cost: 0.0,
            tokens: opencode_schema::session_v1::V1StepTokens {
                total: None,
                input: 1.0,
                output: 2.0,
                reasoning: 0.0,
                cache: opencode_schema::session_v1::V1TokenCache {
                    read: 0.0,
                    write: 0.0,
                },
            },
            structured: None,
            variant: None,
            finish: None,
        }
    }

    fn text_part(message_id: &str, text: &str, synthetic: bool) -> V1Part {
        V1Part::Text {
            id: format!("prt_{message_id}"),
            session_id: "ses_1".into(),
            message_id: message_id.into(),
            text: text.into(),
            synthetic: Some(synthetic),
            ignored: None,
            time: None,
            metadata: None,
        }
    }

    fn options() -> TranscriptOptions {
        TranscriptOptions {
            thinking: true,
            tool_details: true,
            assistant_metadata: true,
        }
    }

    #[test]
    fn transcript_header_and_sorting() {
        let messages = vec![
            MessageParts {
                info: assistant_message("msg_2", 2),
                parts: vec![text_part("msg_2", "world", false)],
            },
            MessageParts {
                info: user_message("msg_1", 1.0),
                parts: vec![text_part("msg_1", "hello", false)],
            },
        ];
        let transcript = format_transcript(&session(), &messages, &options(), &[]);
        assert!(transcript.starts_with("# Demo session\n\n"), "{transcript}");
        assert!(
            transcript.contains("**Session ID:** ses_1\n"),
            "{transcript}"
        );
        assert!(
            transcript
                .find("## User")
                .is_some_and(|user| transcript.find("## Assistant").is_some_and(|a| user < a)),
            "messages sort by creation time: {transcript}"
        );
    }

    #[test]
    fn assistant_metadata_header() {
        let providers = vec![json!({
            "id": "anthropic",
            "models": {"claude": {"name": "Claude", "limit": {"context": 1000}}},
        })];
        let message = MessageParts {
            info: assistant_message("msg_2", 1),
            parts: vec![],
        };
        let with = format_message(
            &message,
            &TranscriptOptions {
                assistant_metadata: true,
                ..options()
            },
            &providers,
        );
        assert!(
            with.starts_with("## Assistant (Build · Claude · 1.5s)\n\n"),
            "{with}"
        );

        let without = format_message(
            &message,
            &TranscriptOptions {
                assistant_metadata: false,
                ..options()
            },
            &providers,
        );
        assert!(without.starts_with("## Assistant\n\n"), "{without}");
    }

    #[test]
    fn part_formatting_matrix() {
        let tool = V1Part::Tool {
            id: "prt_tool".into(),
            session_id: "ses_1".into(),
            message_id: "msg_2".into(),
            call_id: "cal_1".into(),
            tool: "read".into(),
            state: opencode_schema::session_v1::V1ToolState::Completed {
                input: serde_json::Map::new(),
                output: "file content".into(),
                title: "Read".into(),
                metadata: serde_json::Map::new(),
                time: opencode_schema::session_v1::ToolStateCompletedTime {
                    start: 0,
                    end: 0,
                    compacted: None,
                },
                attachments: None,
            },
            metadata: None,
        };
        let hidden = format_part(
            &tool,
            &TranscriptOptions {
                tool_details: false,
                ..options()
            },
        );
        assert!(hidden == "**Tool: read**\n\n", "{hidden}");

        let shown = format_part(&tool, &options());
        assert!(shown.contains("**Output:**"), "{shown}");
        assert!(shown.contains("```\nfile content\n```"), "{shown}");

        let reasoning = V1Part::Reasoning {
            id: "prt_r".into(),
            session_id: "ses_1".into(),
            message_id: "msg_2".into(),
            text: "hmm".into(),
            metadata: None,
            time: opencode_schema::session_v1::ReasoningTime {
                start: 0,
                end: None,
            },
        };
        assert_eq!(
            format_part(&reasoning, &options()),
            "_Thinking:_\n\nhmm\n\n"
        );
        assert_eq!(
            format_part(
                &reasoning,
                &TranscriptOptions {
                    thinking: false,
                    ..options()
                }
            ),
            ""
        );

        let synthetic = text_part("msg_2", "invisible", true);
        assert_eq!(format_part(&synthetic, &options()), "");
    }
}
