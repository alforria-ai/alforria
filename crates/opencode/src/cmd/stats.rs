//! cli/cmd/stats.ts port — the `stats` command: aggregation over session
//! data (usage/cost via core) plus the box-drawing renderers.

use clap::ArgMatches;
use opencode_schema::session_v1::{V1Message, V1Part, V1SessionInfo};

use crate::error::{CliError, TypedError};
use crate::ui::Ui;

const MS_IN_DAY: f64 = 24.0 * 60.0 * 60.0 * 1000.0;

/// The `--models` flag: `undefined` hidden, `true` (all) or top-N.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum ModelsLimit {
    All,
    Top(f64),
}

/// Token counters of the aggregated `totalTokens` record.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct TotalTokens {
    pub input: f64,
    pub output: f64,
    pub reasoning: f64,
    pub cache_read: f64,
    pub cache_write: f64,
}

impl TotalTokens {
    pub fn sum(&self) -> f64 {
        self.input + self.output + self.reasoning + self.cache_read + self.cache_write
    }
}

/// Per-model usage record (stats.ts:24-39).
#[derive(Debug, Clone, Default, PartialEq)]
pub struct ModelUsage {
    pub messages: f64,
    pub input: f64,
    pub output: f64,
    pub cache_read: f64,
    pub cache_write: f64,
    pub cost: f64,
}

/// Ordered map — TS object insertion order preserved for stable sorts.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct SessionStats {
    pub total_sessions: usize,
    pub total_messages: f64,
    pub total_cost: f64,
    pub total_tokens: TotalTokens,
    pub tool_usage: Vec<(String, f64)>,
    pub model_usage: Vec<(String, ModelUsage)>,
    pub date_range_earliest: f64,
    pub date_range_latest: f64,
    pub days: f64,
    pub cost_per_day: f64,
    pub tokens_per_session: f64,
    pub median_tokens_per_session: f64,
}

/// `--days N` cutoff (stats.ts:97-111): undefined → 0 (no filter),
/// `0` → local midnight, else `now - N*MS_IN_DAY`.
pub fn cutoff_time(days: Option<f64>, now_ms: i64) -> f64 {
    let Some(days) = days else {
        return 0.0;
    };
    if days == 0.0 {
        use chrono::TimeZone;
        let local = chrono::Local
            .timestamp_millis_opt(now_ms)
            .single()
            .unwrap_or_default();
        let midnight = local
            .date_naive()
            .and_hms_opt(0, 0, 0)
            .expect("valid midnight");
        return chrono::Local
            .from_local_datetime(&midnight)
            .single()
            .map(|time| time.timestamp_millis() as f64)
            .unwrap_or(0.0);
    }
    now_ms as f64 - days * MS_IN_DAY
}

/// The per-session assistant-token totals (stats.ts:216-221).
fn session_total_tokens(info: &V1SessionInfo) -> TotalTokens {
    let Some(tokens) = info.tokens.as_ref() else {
        return TotalTokens::default();
    };
    TotalTokens {
        input: tokens.input,
        output: tokens.output,
        reasoning: tokens.reasoning,
        cache_read: tokens.cache.read,
        cache_write: tokens.cache.write,
    }
}

/// Assistant-message model-key usage accumulation (stats.ts:184-203).
fn message_model_usage(usage: &mut Vec<(String, ModelUsage)>, message: &V1Message) {
    let V1Message::Assistant {
        provider_id,
        model_id,
        cost,
        tokens,
        ..
    } = message
    else {
        return;
    };
    let key = format!("{}/{}", provider_id.as_str(), model_id.as_str());
    let entry = if let Some((_, entry)) = usage.iter_mut().find(|(id, _)| id == &key) {
        entry
    } else {
        usage.push((key.clone(), ModelUsage::default()));
        &mut usage.last_mut().expect("just pushed").1
    };
    entry.messages += 1.0;
    entry.cost += *cost;
    entry.input += tokens.input;
    entry.output += tokens.output + tokens.reasoning;
    entry.cache_read += tokens.cache.read;
    entry.cache_write += tokens.cache.write;
}

/// The tool-name histogram entry helper (stats.ts:205-209).
fn bump_tool(usage: &mut Vec<(String, f64)>, tool: &str) {
    match usage.iter_mut().find(|(name, _)| name == tool) {
        Some(entry) => entry.1 += 1.0,
        None => usage.push((tool.to_string(), 1.0)),
    }
}

/// `aggregateSessionStats` (stats.ts:88-290). `messages_of` returns the
/// messages of a session (None → the session vanished, counts as empty).
#[allow(clippy::too_many_arguments)]
pub fn aggregate<F>(
    sessions: &[V1SessionInfo],
    messages_of: F,
    days: Option<f64>,
    project_filter: Option<&str>,
    current_project: Option<&str>,
    now_ms: i64,
) -> SessionStats
where
    F: Fn(&V1SessionInfo) -> Option<Vec<opencode_core::WithParts>>,
{
    let cutoff = if days.is_some() {
        cutoff_time(days, now_ms)
    } else {
        0.0
    };
    let window_days = days.map(|days| if days == 0.0 { 1.0 } else { days });
    let mut filtered: Vec<&V1SessionInfo> = if cutoff > 0.0 {
        sessions
            .iter()
            .filter(|session| session.time.updated as f64 >= cutoff)
            .collect()
    } else {
        sessions.iter().collect()
    };
    if let Some(filter) = project_filter {
        if filter.is_empty() {
            let current = current_project.unwrap_or_default();
            filtered.retain(|session| session.project_id.as_str() == current);
        } else {
            filtered.retain(|session| session.project_id.as_str() == filter);
        }
    }

    let mut stats = SessionStats {
        total_sessions: filtered.len(),
        date_range_earliest: now_ms as f64,
        date_range_latest: now_ms as f64,
        ..Default::default()
    };

    if filtered.is_empty() {
        stats.days = window_days.unwrap_or(0.0);
        return stats;
    }

    let mut earliest_time = now_ms as f64;
    let mut latest_time = 0.0_f64;
    let mut session_totals: Vec<f64> = Vec::new();

    for session in &filtered {
        let messages = messages_of(session).unwrap_or_default();
        let session_cost = session.cost.unwrap_or(0.0);
        let tokens = session_total_tokens(session);
        let mut tool_usage: Vec<(String, f64)> = Vec::new();
        let mut model_usage: Vec<(String, ModelUsage)> = Vec::new();

        let mut message_count = 0.0;
        for message in &messages {
            message_count += 1.0;
            message_model_usage(&mut model_usage, &message.info);
            for part in &message.parts {
                if let V1Part::Tool { tool, .. } = part {
                    bump_tool(&mut tool_usage, tool);
                }
            }
        }

        earliest_time = earliest_time.min(if cutoff > 0.0 {
            session.time.updated as f64
        } else {
            session.time.created as f64
        });
        latest_time = latest_time.max(session.time.updated as f64);
        session_totals.push(tokens.sum());

        stats.total_messages += message_count;
        stats.total_cost += session_cost;
        stats.total_tokens.input += tokens.input;
        stats.total_tokens.output += tokens.output;
        stats.total_tokens.reasoning += tokens.reasoning;
        stats.total_tokens.cache_read += tokens.cache_read;
        stats.total_tokens.cache_write += tokens.cache_write;
        for (tool, count) in tool_usage {
            match stats.tool_usage.iter_mut().find(|(name, _)| name == &tool) {
                Some(entry) => entry.1 += count,
                None => stats.tool_usage.push((tool, count)),
            }
        }
        for (model, usage) in model_usage {
            match stats.model_usage.iter_mut().find(|(id, _)| id == &model) {
                Some(entry) => {
                    entry.1.messages += usage.messages;
                    entry.1.input += usage.input;
                    entry.1.output += usage.output;
                    entry.1.cache_read += usage.cache_read;
                    entry.1.cache_write += usage.cache_write;
                    entry.1.cost += usage.cost;
                }
                None => stats.model_usage.push((model, usage)),
            }
        }
    }

    let range_days = ((latest_time - earliest_time) / MS_IN_DAY).ceil().max(1.0);
    let effective_days = window_days.unwrap_or(range_days);
    stats.date_range_earliest = earliest_time;
    stats.date_range_latest = latest_time;
    stats.days = effective_days;
    stats.cost_per_day = stats.total_cost / effective_days;
    stats.tokens_per_session = stats.total_tokens.sum() / filtered.len() as f64;
    session_totals.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
    let mid = session_totals.len() / 2;
    stats.median_tokens_per_session = if session_totals.is_empty() {
        0.0
    } else if session_totals.len().is_multiple_of(2) {
        (session_totals[mid - 1] + session_totals[mid]) / 2.0
    } else {
        session_totals[mid]
    };
    stats
}

/// JS `(1234).toLocaleString()` — en-US comma grouping.
fn to_locale_string(value: f64) -> String {
    let rounded = value.round();
    if rounded.abs() >= 9_007_199_254_740_992.0 {
        return format!("{value}");
    }
    let digits = rounded.abs() as u128;
    let text = digits.to_string();
    let grouped: Vec<String> = text
        .as_bytes()
        .rchunks(3)
        .rev()
        .map(|chunk| String::from_utf8_lossy(chunk).into_owned())
        .collect();
    let sign = if rounded < 0.0 { "-" } else { "" };
    format!("{sign}{}", grouped.join(","))
}

/// JS `Number.prototype.toFixed(2)`-shaped f64 formatting.
fn fixed(value: f64, digits: usize) -> String {
    format!("{:.digits$}", value)
}

/// JS number rendering: integral values without a decimal point.
fn js_number(value: f64) -> String {
    if value == value.trunc() && value.is_finite() && value.abs() < 1e15 {
        format!("{}", value as i64)
    } else {
        format!("{value}")
    }
}

/// `formatNumber` (stats.ts:386-393).
fn format_number(num: f64) -> String {
    if num >= 1_000_000.0 {
        format!("{}M", fixed(num / 1_000_000.0, 1))
    } else if num >= 1_000.0 {
        format!("{}K", fixed(num / 1_000.0, 1))
    } else {
        js_number(num)
    }
}

const WIDTH: usize = 56;

/// `renderRow` (stats.ts:295-300).
fn render_row(label: &str, value: &str) -> String {
    let padding = WIDTH
        .saturating_sub(1)
        .saturating_sub(label.chars().count())
        .saturating_sub(value.chars().count());
    format!("│{label}{}{value} │", " ".repeat(padding))
}

fn box_line() -> String {
    format!("┌{}┐", "─".repeat(WIDTH))
}

fn box_mid() -> String {
    format!("├{}┤", "─".repeat(WIDTH))
}

fn box_end() -> String {
    format!("└{}┘", "─".repeat(WIDTH))
}

fn js_max(values: &[f64]) -> f64 {
    values.iter().copied().fold(f64::NEG_INFINITY, f64::max)
}

/// `displayStats` (stats.ts:292-384) — everything on stdout (`console.log`).
pub fn display_stats(
    ui: &mut Ui,
    stats: &SessionStats,
    tool_limit: Option<usize>,
    model_limit: Option<ModelsLimit>,
) {
    let nan0 = |value: f64| if value.is_nan() { 0.0 } else { value };

    ui.write_stdout(&format!("{}\n", box_line()));
    ui.write_stdout("│                       OVERVIEW                         │\n");
    ui.write_stdout(&format!("{}\n", box_mid()));
    ui.write_stdout(&format!(
        "{}\n",
        render_row("Sessions", &to_locale_string(stats.total_sessions as f64))
    ));
    ui.write_stdout(&format!(
        "{}\n",
        render_row("Messages", &to_locale_string(stats.total_messages))
    ));
    ui.write_stdout(&format!("{}\n", render_row("Days", &js_number(stats.days))));
    ui.write_stdout(&format!("{}\n\n", box_end()));

    let cost = nan0(stats.total_cost);
    let cost_per_day = nan0(stats.cost_per_day);
    let tokens_per_session = nan0(stats.tokens_per_session);
    ui.write_stdout(&format!("{}\n", box_line()));
    ui.write_stdout("│                    COST & TOKENS                       │\n");
    ui.write_stdout(&format!("{}\n", box_mid()));
    ui.write_stdout(&format!(
        "{}\n",
        render_row("Total Cost", &format!("${}", fixed(cost, 2)))
    ));
    ui.write_stdout(&format!(
        "{}\n",
        render_row("Avg Cost/Day", &format!("${}", fixed(cost_per_day, 2)))
    ));
    ui.write_stdout(&format!(
        "{}\n",
        render_row(
            "Avg Tokens/Session",
            &format_number(tokens_per_session.round())
        )
    ));
    ui.write_stdout(&format!(
        "{}\n",
        render_row(
            "Median Tokens/Session",
            &format_number(nan0(stats.median_tokens_per_session).round())
        )
    ));
    ui.write_stdout(&format!(
        "{}\n",
        render_row("Input", &format_number(stats.total_tokens.input))
    ));
    ui.write_stdout(&format!(
        "{}\n",
        render_row("Output", &format_number(stats.total_tokens.output))
    ));
    ui.write_stdout(&format!(
        "{}\n",
        render_row("Cache Read", &format_number(stats.total_tokens.cache_read))
    ));
    ui.write_stdout(&format!(
        "{}\n",
        render_row(
            "Cache Write",
            &format_number(stats.total_tokens.cache_write)
        )
    ));
    ui.write_stdout(&format!("{}\n\n", box_end()));

    if let Some(limit) = model_limit {
        if !stats.model_usage.is_empty() {
            let mut models = stats.model_usage.clone();
            models.sort_by(|a, b| {
                b.1.messages
                    .partial_cmp(&a.1.messages)
                    .unwrap_or(std::cmp::Ordering::Equal)
            });
            let displayed = match limit {
                ModelsLimit::All => models,
                ModelsLimit::Top(count) => {
                    models.into_iter().take(count.max(0.0) as usize).collect()
                }
            };
            ui.write_stdout(&format!("{}\n", box_line()));
            ui.write_stdout("│                      MODEL USAGE                       │\n");
            ui.write_stdout(&format!("{}\n", box_mid()));
            for (model, usage) in displayed {
                let name: String = format!("{:<54}", model);
                ui.write_stdout(&format!("│ {name} │\n"));
                ui.write_stdout(&format!(
                    "{}\n",
                    render_row("  Messages", &to_locale_string(usage.messages))
                ));
                ui.write_stdout(&format!(
                    "{}\n",
                    render_row("  Input Tokens", &format_number(usage.input))
                ));
                ui.write_stdout(&format!(
                    "{}\n",
                    render_row("  Output Tokens", &format_number(usage.output))
                ));
                ui.write_stdout(&format!(
                    "{}\n",
                    render_row("  Cache Read", &format_number(usage.cache_read))
                ));
                ui.write_stdout(&format!(
                    "{}\n",
                    render_row("  Cache Write", &format_number(usage.cache_write))
                ));
                ui.write_stdout(&format!(
                    "{}\n",
                    render_row("  Cost", &format!("${}", fixed(usage.cost, 4)))
                ));
                ui.write_stdout(&format!("{}\n", box_mid()));
            }
            // Replace the trailing separator with the bottom border
            // (`process.stdout.write("\x1B[1A")` — stats.ts:350-352).
            ui.write_stdout("\x1b[1A");
            ui.write_stdout(&format!("{}\n", box_end()));
        }
    }
    ui.write_stdout("\n");

    if !stats.tool_usage.is_empty() {
        let mut tools = stats.tool_usage.clone();
        tools.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));
        let displayed = match tool_limit {
            Some(limit) => tools.into_iter().take(limit).collect::<Vec<_>>(),
            None => tools,
        };
        let max_count = js_max(
            &displayed
                .iter()
                .map(|(_, count)| *count)
                .collect::<Vec<_>>(),
        );
        let total_tool_usage: f64 = stats.tool_usage.iter().map(|(_, count)| *count).sum();
        ui.write_stdout(&format!("{}\n", box_line()));
        ui.write_stdout("│                      TOOL USAGE                        │\n");
        ui.write_stdout(&format!("{}\n", box_mid()));
        for (tool, count) in displayed {
            let bar_length = ((count / max_count) * 20.0).floor().max(1.0) as usize;
            let bar = "█".repeat(bar_length);
            let percentage = fixed((count / total_tool_usage) * 100.0, 1);
            let truncated = if tool.chars().count() > 18 {
                let prefix: String = tool.chars().take(16).collect();
                format!("{prefix}..")
            } else {
                tool.clone()
            };
            let tool_name = format!("{:<18}", truncated);
            let count_text = format!("{:>3}", js_number(count));
            let percentage_text = format!("{:>4}", percentage);
            let content = format!(" {tool_name} {:<20} {count_text} ({percentage_text}%)", bar);
            let padding = WIDTH
                .saturating_sub(content.chars().count())
                .saturating_sub(1);
            ui.write_stdout(&format!("│{content}{} │\n", " ".repeat(padding)));
        }
        ui.write_stdout(&format!("{}\n", box_end()));
    }
    ui.write_stdout("\n");
}

/// `--models` parsing (stats.ts:73-79): `true` → all, number → top N.
pub fn models_limit(raw: Option<&str>) -> Result<Option<ModelsLimit>, TypedError> {
    match raw {
        None => Ok(None),
        Some("true") => Ok(Some(ModelsLimit::All)),
        Some(value) => value
            .parse::<f64>()
            .map(|count| Some(ModelsLimit::Top(count)))
            .map_err(|_| {
                TypedError::Cli(CliError::new(format!(
                    "Invalid value '{value}' for option '--models <models>'"
                )))
            }),
    }
}

pub fn run(matches: &ArgMatches, ui: &mut Ui) -> Result<(), TypedError> {
    let days = matches.get_one::<f64>("days").copied();
    let tool_limit = matches.get_one::<f64>("tools").map(|value| *value as usize);
    let model_limit = models_limit(matches.get_one::<String>("models").map(String::as_str))?;
    let project = matches.get_one::<String>("project").map(String::as_str);

    let instance = crate::instance::boot(None)?;
    let context = instance
        .services
        .instance_context(&instance.directory, None)
        .map_err(|err| TypedError::Cli(CliError::new(err.to_string())))?;
    let sessions = instance
        .services
        .sessions
        .list_by_project(
            &context.project_id,
            &opencode_core::ListInput {
                limit: Some(i64::MAX),
                ..Default::default()
            },
        )
        .map_err(|err| TypedError::Cli(CliError::new(err.to_string())))?;
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| duration.as_millis() as i64)
        .unwrap_or_default();
    let stats = aggregate(
        &sessions,
        |session| instance.services.sessions.messages(&session.id, None).ok(),
        days,
        project,
        Some(&instance.location.project.id),
        now,
    );
    if stats.total_sessions > 1000 {
        ui.write_stdout(&format!(
            "Large dataset detected ({} sessions). This may take a while...\n",
            stats.total_sessions
        ));
    }
    display_stats(ui, &stats, tool_limit, model_limit);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    use opencode_schema::ids::{MessageId, ModelId, PartId, ProviderId, SessionId};
    use opencode_schema::session_v1::{
        AssistantTime, UserTime, V1Message, V1Part, V1Path, V1SessionInfo, V1SessionTime,
        V1StepTokens, V1TokenCache, V1UserModel,
    };

    fn session(id: &str, project: &str, created: u64, updated: u64) -> V1SessionInfo {
        V1SessionInfo {
            id: SessionId::from(id),
            slug: String::new(),
            project_id: project.to_string(),
            workspace_id: None,
            directory: "/repo".to_string(),
            path: None,
            parent_id: None,
            summary: None,
            cost: Some(0.1),
            tokens: None,
            share: None,
            title: "t".to_string(),
            agent: None,
            model: None,
            version: "1".to_string(),
            metadata: None,
            time: V1SessionTime {
                created,
                updated,
                compacting: None,
                archived: None,
            },
            permission: None,
            revert: None,
        }
    }

    fn assistant_message(id: &str, model: &str) -> V1Message {
        V1Message::Assistant {
            id: MessageId::from(id),
            session_id: SessionId::from("ses_1"),
            time: AssistantTime {
                created: 1,
                completed: Some(2),
            },
            error: None,
            parent_id: MessageId::from(id),
            model_id: ModelId::from(model),
            provider_id: ProviderId::from("anthropic"),
            mode: "primary".to_string(),
            agent: "build".to_string(),
            path: V1Path {
                cwd: "/repo".to_string(),
                root: "/repo".to_string(),
            },
            summary: None,
            cost: 0.25,
            tokens: V1StepTokens {
                total: Some(30.0),
                input: 10.0,
                output: 5.0,
                reasoning: 2.0,
                cache: V1TokenCache {
                    read: 1.0,
                    write: 1.0,
                },
            },
            structured: None,
            variant: None,
            finish: None,
        }
    }

    fn user_message(id: &str) -> V1Message {
        V1Message::User {
            id: MessageId::from(id),
            session_id: SessionId::from("ses_1"),
            time: UserTime { created: 1.0 },
            format: None,
            summary: None,
            agent: "build".to_string(),
            model: V1UserModel {
                provider_id: ProviderId::from("x"),
                model_id: ModelId::from("y"),
                variant: None,
            },
            system: None,
            tools: None,
        }
    }

    fn text_part(id: &str) -> V1Part {
        V1Part::Text {
            id: PartId::from(id),
            session_id: SessionId::from("ses_1"),
            message_id: MessageId::from("msg_1"),
            text: "hello".to_string(),
            synthetic: None,
            ignored: None,
            time: None,
            metadata: None,
        }
    }

    fn tool_part(id: &str, tool: &str) -> V1Part {
        use opencode_schema::session_v1::{ToolStateCompletedTime, V1ToolState};
        V1Part::Tool {
            id: PartId::from(id),
            session_id: SessionId::from("ses_1"),
            message_id: MessageId::from("msg_1"),
            call_id: "call".to_string(),
            tool: tool.to_string(),
            state: V1ToolState::Completed {
                input: Default::default(),
                output: "done".to_string(),
                title: "title".to_string(),
                metadata: Default::default(),
                time: ToolStateCompletedTime {
                    start: 1,
                    end: 2,
                    compacted: None,
                },
                attachments: None,
            },
            metadata: None,
        }
    }

    fn with_parts(info: V1Message, parts: Vec<V1Part>) -> opencode_core::WithParts {
        opencode_core::WithParts { info, parts }
    }

    const NOW: i64 = 1_800_000_000_000;

    #[test]
    fn to_locale_string_groups_digits() {
        assert_eq!(to_locale_string(1234.0), "1,234");
        assert_eq!(to_locale_string(0.0), "0");
        assert_eq!(to_locale_string(1234567.0), "1,234,567");
        assert_eq!(to_locale_string(-42.4), "-42");
    }

    #[test]
    fn format_number_matches_js_shapes() {
        assert_eq!(format_number(999.0), "999");
        assert_eq!(format_number(1234.0), "1.2K");
        assert_eq!(format_number(1234567.0), "1.2M");
    }

    #[test]
    fn cutoff_time_uses_midnight_for_zero_days() {
        assert_eq!(cutoff_time(None, NOW), 0.0);
        assert_eq!(cutoff_time(Some(7.0), NOW), NOW as f64 - 7.0 * MS_IN_DAY);
        let midnight = cutoff_time(Some(0.0), NOW);
        assert!(midnight < NOW as f64);
        assert!(midnight > NOW as f64 - MS_IN_DAY);
    }

    #[test]
    fn aggregate_empty_days_window() {
        let stats = aggregate(&[], |_| None, None, None, None, NOW);
        assert_eq!(stats.days, 0.0);
        assert_eq!(stats.total_sessions, 0);
    }

    #[test]
    fn aggregate_days_zero_sets_window_one() {
        let stats = aggregate(
            &[session("ses_1", "prj", 5, 6)],
            |_| None,
            Some(0.0),
            None,
            None,
            NOW,
        );
        assert_eq!(stats.days, 1.0);
    }

    #[test]
    fn aggregate_counts_messages_tools_and_models() {
        let sessions = vec![session("ses_1", "prj", 5, 6)];
        let stats = aggregate(
            &sessions,
            |_| {
                Some(vec![
                    with_parts(user_message("msg_1"), vec![]),
                    with_parts(
                        assistant_message("msg_2", "claude"),
                        vec![text_part("prt_1"), tool_part("prt_2", "read")],
                    ),
                ])
            },
            None,
            None,
            None,
            NOW,
        );
        assert_eq!(stats.total_sessions, 1);
        assert_eq!(stats.total_messages, 2.0);
        assert_eq!(stats.total_cost, 0.1);
        assert_eq!(stats.tool_usage, vec![("read".to_string(), 1.0)]);
        assert_eq!(stats.model_usage.len(), 1);
        assert_eq!(stats.model_usage[0].0, "anthropic/claude");
        assert_eq!(stats.model_usage[0].1.messages, 1.0);
        assert_eq!(stats.model_usage[0].1.cost, 0.25);
        assert_eq!(stats.model_usage[0].1.input, 10.0);
        assert_eq!(stats.model_usage[0].1.output, 7.0);
        assert_eq!(stats.model_usage[0].1.cache_read, 1.0);
        assert_eq!(stats.days, 1.0);
    }

    #[test]
    fn aggregate_project_filter() {
        let sessions = vec![
            session("ses_1", "prj_a", 5, 6),
            session("ses_2", "prj_b", 5, 6),
        ];
        let stats = aggregate(&sessions, |_| None, None, Some("prj_a"), None, NOW);
        assert_eq!(stats.total_sessions, 1);
        let stats = aggregate(&sessions, |_| None, None, Some(""), Some("prj_b"), NOW);
        assert_eq!(stats.total_sessions, 1);
    }

    #[test]
    fn aggregate_days_cutoff_filters_old_sessions() {
        let day = 24.0 * 60.0 * 60.0 * 1000.0;
        let sessions = vec![
            session(
                "ses_old",
                "prj",
                (NOW as f64 - 3.0 * day) as u64,
                (NOW as f64 - 3.0 * day) as u64,
            ),
            session(
                "ses_new",
                "prj",
                (NOW as f64 - day) as u64,
                (NOW as f64 - day) as u64,
            ),
        ];
        let stats = aggregate(&sessions, |_| None, Some(2.0), None, None, NOW);
        assert_eq!(stats.total_sessions, 1);
        assert_eq!(stats.days, 2.0);
    }

    #[test]
    fn aggregate_median_tokens_even_count() {
        let mut ses_a = session("ses_a", "prj", 1, 1);
        let mut ses_b = session("ses_b", "prj", 1, 1);
        ses_a.tokens = Some(opencode_schema::session::SessionTokens {
            input: 10.0,
            output: 0.0,
            reasoning: 0.0,
            cache: opencode_schema::session::SessionTokensCache {
                read: 0.0,
                write: 0.0,
            },
        });
        ses_b.tokens = Some(opencode_schema::session::SessionTokens {
            input: 20.0,
            output: 0.0,
            reasoning: 0.0,
            cache: opencode_schema::session::SessionTokensCache {
                read: 0.0,
                write: 0.0,
            },
        });
        let stats = aggregate(&[ses_a, ses_b], |_| None, None, None, None, NOW);
        assert_eq!(stats.median_tokens_per_session, 15.0);
        assert_eq!(stats.tokens_per_session, 15.0);
    }

    #[test]
    fn render_row_pads_to_width() {
        let row = render_row("Sessions", "1");
        assert!(row.starts_with('│'));
        assert!(row.ends_with(" │"), "{row}");
        assert_eq!(row.chars().count(), 58, "{row}");
    }

    #[test]
    fn display_overview_box_shape() {
        let stats = aggregate(
            &[session("ses_1", "prj", 5, 6)],
            |_| None,
            None,
            None,
            None,
            NOW,
        );
        let (mut ui, captured) = Ui::capture(false);
        display_stats(&mut ui, &stats, None, None);
        let out = captured.stdout();
        assert!(
            out.contains("┌────────────────────────────────────────────────────────┐\n"),
            "{out}"
        );
        assert!(
            out.contains("│                       OVERVIEW                         │\n"),
            "{out}"
        );
        assert!(
            out.contains("│                    COST & TOKENS                       │\n"),
            "{out}"
        );
        assert!(out.contains(&render_row("Sessions", "1")), "{out}");
        assert!(!out.contains("MODEL USAGE"), "{out}");
        assert!(!out.contains("TOOL USAGE"), "{out}");
    }

    #[test]
    fn display_model_and_tool_sections() {
        let stats = aggregate(
            &[session("ses_1", "prj", 5, 6)],
            |_| {
                Some(vec![with_parts(
                    assistant_message("msg_1", "claude"),
                    vec![tool_part("prt_1", "read"), tool_part("prt_2", "edit")],
                )])
            },
            None,
            None,
            None,
            NOW,
        );
        let (mut ui, captured) = Ui::capture(false);
        display_stats(&mut ui, &stats, None, Some(ModelsLimit::All));
        let out = captured.stdout();
        assert!(
            out.contains("│                      MODEL USAGE                       │\n"),
            "{out}"
        );
        assert!(
            out.contains("│ anthropic/claude                                       │\n"),
            "{out}"
        );
        assert!(out.contains(&render_row("  Cost", "$0.2500")), "{out}");
        assert!(
            out.contains("│                      TOOL USAGE                        │\n"),
            "{out}"
        );
        assert!(out.contains("read"), "{out}");
        assert!(out.contains("edit"), "{out}");
    }

    #[test]
    fn display_top_tool_limit() {
        let stats = aggregate(
            &[session("ses_1", "prj", 5, 6)],
            |_| {
                Some(vec![with_parts(
                    assistant_message("msg_1", "claude"),
                    vec![tool_part("prt_1", "read"), tool_part("prt_2", "edit")],
                )])
            },
            None,
            None,
            None,
            NOW,
        );
        let (mut ui, captured) = Ui::capture(false);
        display_stats(&mut ui, &stats, Some(1), Some(ModelsLimit::Top(1.0)));
        let out = captured.stdout();
        assert!(out.contains("TOOL USAGE"), "{out}");
        assert!(out.contains("read"), "{out}");
        assert!(!out.contains("edit"), "{out}");
    }

    #[test]
    fn models_limit_parses_flag_shapes() {
        assert_eq!(models_limit(None).unwrap(), None);
        assert_eq!(models_limit(Some("true")).unwrap(), Some(ModelsLimit::All));
        assert_eq!(
            models_limit(Some("5")).unwrap(),
            Some(ModelsLimit::Top(5.0))
        );
        assert!(models_limit(Some("abc")).is_err());
    }
}
