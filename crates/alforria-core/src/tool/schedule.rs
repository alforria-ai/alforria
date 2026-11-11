//! `schedule` tool — the agent's handle on its own triggers.
//!
//! The scheduler, its storage and the HTTP surface live in
//! [`crate::trigger`]; this is the model's entry point: create a recurring
//! job or a one-shot reminder, list what exists, change, pause or delete it.
//!
//! A trigger made here runs as the agent that made it, under the rules of
//! the session it was made in, so scheduling something is never a way to do
//! more than the agent could do right now.

use std::sync::Arc;

use alforria_schema::permission_v1::{PermissionV1Action, PermissionV1Rule, PermissionV1Ruleset};
use serde::Deserialize;
use serde_json::{json, Map, Value};

use crate::tool::def::{define, Agents, AskRequest, BoxFuture, ExecuteResult, ToolCtxRef, ToolDef};
use crate::tool::error::ToolError;
use crate::tool::truncate::Truncate;
use crate::trigger::{
    format_instant, parse_duration, TriggerError, TriggerRecord, TriggerService, TriggerSpec,
};

pub const TOOL_ID: &str = "schedule";

/// What the tool needs to know about sessions to keep a trigger within the
/// authority of whoever made it.
pub trait ScheduleSessions: Send + Sync {
    /// Everything that governs `session_id` running as `agent` right now:
    /// the agent's rules, then the session's own.
    fn effective_rules(&self, session_id: &str, agent: &str) -> PermissionV1Ruleset;
    /// The same as bounds for a trigger: every reading of the session's
    /// policy (`trigger::rules::CreatorBounds`).
    fn bounds(&self, session_id: &str, agent: &str) -> crate::trigger::CreatorBounds {
        crate::trigger::CreatorBounds::Flat(self.effective_rules(session_id, agent))
    }
    /// The session's parent, its parent's parent, and so on.
    fn ancestors(&self, session_id: &str) -> Vec<String>;
    /// Whether the session is restricted (`session::caller`). A trigger
    /// made there is a restricted caller's, like the session itself.
    fn restricted(&self, _session_id: &str) -> bool {
        false
    }
    /// Whether the session has seen outside content
    /// (`SessionMark::OutsideContent`). What it schedules carries the mark.
    fn seen_outside_content(&self, _session_id: &str) -> bool {
        false
    }
}

/// [`ScheduleSessions`] over the session store and the agent registry.
pub struct SessionTree {
    pub sessions: crate::session::store::SessionStore,
    pub agents: crate::session::agents::AgentRegistry,
}

impl ScheduleSessions for SessionTree {
    fn effective_rules(&self, session_id: &str, agent: &str) -> PermissionV1Ruleset {
        let mut rules = self
            .agents
            .get(agent)
            // The flat rendering: never more permissive than the policy.
            .map(|agent| agent.permission.render())
            .unwrap_or_default();
        if let Ok(session) = self.sessions.get(session_id) {
            rules.extend(session.permission.unwrap_or_default());
        }
        rules
    }

    fn bounds(&self, session_id: &str, agent: &str) -> crate::trigger::CreatorBounds {
        // The policy a turn of this session is judged by: the agent's,
        // the session's own rules, and the session's ceilings. A session
        // or an agent that cannot be read bounds the trigger to nothing.
        let deny_all = || {
            crate::trigger::CreatorBounds::Flat(vec![PermissionV1Rule {
                permission: "*".to_string(),
                pattern: "*".to_string(),
                action: PermissionV1Action::Deny,
            }])
        };
        let (Some(agent), Ok(session)) = (self.agents.get(agent), self.sessions.get(session_id))
        else {
            return deny_all();
        };
        drop(session);
        crate::trigger::CreatorBounds::of(&self.sessions.policy(session_id, &agent.permission))
    }

    fn restricted(&self, session_id: &str) -> bool {
        // A restriction that cannot be read counts as one.
        self.sessions.is_restricted(session_id).unwrap_or(true)
    }

    fn seen_outside_content(&self, session_id: &str) -> bool {
        self.sessions.seen_outside_content(session_id)
    }

    fn ancestors(&self, session_id: &str) -> Vec<String> {
        let mut out = Vec::new();
        let mut current = self.sessions.get(session_id).ok().and_then(|s| s.parent_id);
        while let Some(id) = current {
            if out.len() >= 64 || out.contains(&id) {
                break;
            }
            current = self.sessions.get(&id).ok().and_then(|s| s.parent_id);
            out.push(id);
        }
        out
    }
}

/// How the calling session stands to the session that created a trigger.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Relation {
    /// The caller created it, or is an ancestor of the session that did.
    Owner,
    /// The caller descends from the session that created it.
    Descendant,
    /// Someone else's: the config's, a client's, or another conversation's.
    Stranger,
}

fn relation(record: &TriggerRecord, caller: &str, sessions: &dyn ScheduleSessions) -> Relation {
    if record.source != crate::trigger::TriggerSource::Runtime {
        return Relation::Stranger;
    }
    let Some(origin) = record.origin_session_id.as_deref() else {
        return Relation::Stranger;
    };
    if origin == caller || sessions.ancestors(origin).iter().any(|id| id == caller) {
        Relation::Owner
    } else if sessions.ancestors(caller).iter().any(|id| id == origin) {
        Relation::Descendant
    } else {
        Relation::Stranger
    }
}

/// Refuse a change to a trigger the caller has no say over.
///
/// Changing a trigger changes what an unattended run does, under the
/// trigger's rules rather than the caller's. So the tool only touches
/// triggers from its own conversation tree: the session that created one and
/// its ancestors may change it; a descendant, whose rules may be narrower,
/// may only stop it (`pause`, `delete`). Triggers from the config or made by
/// a client over HTTP are the user's, and the tool leaves them alone.
fn authorize(
    record: &TriggerRecord,
    action: &str,
    caller: &str,
    sessions: &dyn ScheduleSessions,
) -> Result<(), ToolError> {
    let allowed = match relation(record, caller, sessions) {
        Relation::Owner => true,
        Relation::Descendant => matches!(action, "pause" | "delete"),
        Relation::Stranger => false,
    };
    if allowed {
        return Ok(());
    }
    Err(ToolError::Failed(format!(
        "trigger {} was not scheduled by this conversation, so it cannot {action} it. \
         Only the user can change it (in the configuration, or with the /trigger API).",
        record.id
    )))
}

#[derive(Debug, Default, Deserialize)]
pub struct ScheduleParameters {
    action: String,
    #[serde(default)]
    id: Option<String>,
    #[serde(default)]
    prompt: Option<String>,
    #[serde(default)]
    title: Option<String>,
    #[serde(default)]
    cron: Option<String>,
    #[serde(default)]
    timezone: Option<String>,
    #[serde(default)]
    interval: Option<String>,
    #[serde(default)]
    at: Option<String>,
    #[serde(default, rename = "in")]
    after: Option<String>,
    #[serde(default)]
    watch: Option<String>,
    #[serde(default)]
    debounce: Option<String>,
    #[serde(default)]
    webhook: Option<bool>,
    #[serde(default)]
    session: Option<String>,
    #[serde(default)]
    report: Option<String>,
    #[serde(default)]
    quiet_token: Option<String>,
}

const DESCRIPTION: &str = r#"Schedule work for later: recurring jobs, one-shot reminders, and prompts that run when files change or a webhook is called.

Each scheduled item is a trigger: something to wait for plus a prompt. When it fires, the prompt starts a turn as you, with your current permissions, and nobody is there to answer questions, so write the prompt as a complete instruction that can be carried out unattended.

Actions:
- `create`: needs `prompt` and exactly one of:
  - `cron`: a 5-field cron expression (names like MON-FRI and @hourly, @daily, @weekly work), with an optional IANA `timezone` (default UTC);
  - `interval`: a repeat period such as "30m" or "2h" (minimum 10s);
  - `at`: one time, as RFC 3339 (for example 2027-01-31T09:00:00+01:00);
  - `in`: one time, relative to now, such as "20m";
  - `watch`: a path inside the project; fires when it changes and then stays quiet for `debounce` (default 5s);
  - `webhook: true`: fires when POST /trigger/{id}/hook is called.
  Optional: `id` (a short name), `title`, `session`, `report`, `quiet_token`.
- `list`: show every trigger, with its state and next run.
- `update`: change the fields you pass on trigger `id`.
- `pause` / `resume`: stop or restart trigger `id` without deleting it.
- `delete`: remove trigger `id`.

`session` says where the turn runs: "new" (default) starts a fresh session per run, "reuse" keeps one long-lived session for the trigger, and "current" runs in this conversation, which is what a reminder for the user usually wants.

`quiet_token`: if a run's final answer is exactly this text, it counts as "nothing to report" and is not delivered. Use it for checks that usually find nothing, and tell the prompt to answer with the token in that case. `report: "none"` never delivers the answer.

You can only change, pause, resume or delete triggers that this conversation (or one of its subagents) scheduled. Triggers from the configuration file or created by the user are listed but are the user's to change. Give times in the user's timezone when you know it."#;

fn parameters() -> Value {
    json!({
        "type": "object",
        "properties": {
            "action": {
                "type": "string",
                "enum": ["create", "list", "update", "pause", "resume", "delete"],
                "description": "The action to perform"
            },
            "id": {
                "type": "string",
                "description": "The trigger id (required for update, pause, resume and delete; optional short name for create)"
            },
            "prompt": {
                "type": "string",
                "description": "What to do when the trigger fires, written as a complete instruction"
            },
            "title": { "type": "string", "description": "A short human-readable name" },
            "cron": { "type": "string", "description": "5-field cron expression, e.g. \"0 9 * * MON-FRI\"" },
            "timezone": { "type": "string", "description": "IANA timezone for cron, e.g. \"Europe/Paris\"" },
            "interval": { "type": "string", "description": "Repeat period, e.g. \"30m\"" },
            "at": { "type": "string", "description": "One-shot time as RFC 3339" },
            "in": { "type": "string", "description": "One-shot delay from now, e.g. \"20m\"" },
            "watch": { "type": "string", "description": "Path inside the project to watch for changes" },
            "debounce": { "type": "string", "description": "Quiet period before a watch fires, e.g. \"60s\"" },
            "webhook": { "type": "boolean", "description": "Fire on POST /trigger/{id}/hook" },
            "session": {
                "type": "string",
                "enum": ["new", "reuse", "current"],
                "description": "Where the turn runs (default new)"
            },
            "report": {
                "type": "string",
                "enum": ["final", "none"],
                "description": "Whether the final answer is delivered (default final)"
            },
            "quiet_token": {
                "type": "string",
                "description": "A final answer equal to this means nothing to report"
            }
        },
        "required": ["action"]
    })
}

pub fn schedule_tool(
    truncate: Arc<dyn Truncate>,
    agents: Arc<dyn Agents>,
    triggers: Arc<TriggerService>,
    sessions: Arc<dyn ScheduleSessions>,
) -> ToolDef {
    define(
        TOOL_ID,
        DESCRIPTION,
        parameters(),
        None,
        truncate,
        agents,
        move |params: ScheduleParameters, ctx: ToolCtxRef<'_>| {
            run(params, ctx, Arc::clone(&triggers), Arc::clone(&sessions))
        },
    )
}

fn invalid(detail: impl Into<String>) -> ToolError {
    ToolError::InvalidArguments {
        tool: TOOL_ID.to_string(),
        detail: detail.into(),
    }
}

fn failed(error: TriggerError) -> ToolError {
    match error {
        TriggerError::Invalid(message) => invalid(message),
        other => ToolError::Failed(other.to_string()),
    }
}

/// The `on` object the schedule parameters describe, if any were given.
fn on_of(params: &ScheduleParameters, now_ms: u64) -> Result<Option<Value>, ToolError> {
    let mut on = Map::new();
    if let Some(cron) = &params.cron {
        on.insert("cron".to_string(), json!(cron));
        if let Some(timezone) = &params.timezone {
            on.insert("timezone".to_string(), json!(timezone));
        }
    } else if params.timezone.is_some() {
        return Err(invalid("`timezone` goes with `cron`"));
    }
    if let Some(interval) = &params.interval {
        on.insert("interval".to_string(), json!(interval));
    }
    match (&params.at, &params.after) {
        (Some(_), Some(_)) => return Err(invalid("pass `at` or `in`, not both")),
        (Some(at), None) => {
            on.insert("at".to_string(), json!(at));
        }
        (None, Some(after)) => {
            let delay = parse_duration(after).map_err(invalid)?;
            on.insert(
                "at".to_string(),
                json!(format_instant(now_ms.saturating_add(delay))),
            );
        }
        (None, None) => {}
    }
    if let Some(path) = &params.watch {
        let mut watch = Map::new();
        watch.insert("path".to_string(), json!(path));
        if let Some(debounce) = &params.debounce {
            watch.insert("debounce".to_string(), json!(debounce));
        }
        on.insert("watch".to_string(), Value::Object(watch));
    } else if params.debounce.is_some() {
        return Err(invalid("`debounce` goes with `watch`"));
    }
    if params.webhook == Some(true) {
        on.insert("webhook".to_string(), json!(true));
    }
    Ok((!on.is_empty()).then_some(Value::Object(on)))
}

/// The spec fields the parameters set, as a merge patch.
fn patch_of(
    params: &ScheduleParameters,
    session_id: &str,
    now_ms: u64,
) -> Result<Map<String, Value>, ToolError> {
    let mut patch = Map::new();
    if let Some(on) = on_of(params, now_ms)? {
        patch.insert("on".to_string(), on);
    }
    if let Some(prompt) = &params.prompt {
        patch.insert("prompt".to_string(), json!(prompt));
    }
    if let Some(title) = &params.title {
        patch.insert("title".to_string(), json!(title));
    }
    if let Some(session) = &params.session {
        let value = match session.as_str() {
            "current" => session_id.to_string(),
            "new" | "reuse" => session.clone(),
            other => {
                return Err(invalid(format!(
                    "session must be \"new\", \"reuse\" or \"current\", not \"{other}\""
                )))
            }
        };
        patch.insert("session".to_string(), json!(value));
    }
    if let Some(report) = &params.report {
        if !matches!(report.as_str(), "final" | "none") {
            return Err(invalid("report must be \"final\" or \"none\""));
        }
        patch.insert("report".to_string(), json!(report));
    }
    if let Some(token) = &params.quiet_token {
        patch.insert("quiet_token".to_string(), json!(token));
    }
    Ok(patch)
}

fn required_id(params: &ScheduleParameters) -> Result<&str, ToolError> {
    params
        .id
        .as_deref()
        .filter(|id| !id.trim().is_empty())
        .ok_or_else(|| invalid(format!("{} needs the trigger `id`", params.action)))
}

async fn approve(ctx: &ToolCtxRef<'_>, action: &str, metadata: Value) -> Result<(), ToolError> {
    ctx.ask
        .ask(AskRequest {
            permission: TOOL_ID.to_string(),
            patterns: vec![action.to_string()],
            always: vec![action.to_string()],
            metadata,
        })
        .await
}

fn run(
    params: ScheduleParameters,
    ctx: ToolCtxRef<'_>,
    triggers: Arc<TriggerService>,
    sessions: Arc<dyn ScheduleSessions>,
) -> BoxFuture<'_, Result<ExecuteResult, ToolError>> {
    Box::pin(async move {
        if !triggers.enabled() {
            return Err(ToolError::Failed(
                "triggers are disabled in the configuration".to_string(),
            ));
        }
        let now = triggers.now_ms();
        match params.action.as_str() {
            "list" => {
                let list = triggers.list();
                Ok(ExecuteResult {
                    title: "Triggers".to_string(),
                    metadata: json!({ "count": list.len() }),
                    output: render_list(&list),
                    attachments: None,
                })
            }
            "create" => {
                let mut patch = patch_of(&params, ctx.session_id, now)?;
                if !patch.contains_key("on") {
                    return Err(invalid(
                        "create needs one of cron, interval, at, in, watch or webhook: true",
                    ));
                }
                if !patch.contains_key("prompt") {
                    return Err(invalid("create needs a prompt"));
                }
                // It runs as the agent that asked, never as another one.
                patch.insert("agent".to_string(), json!(ctx.agent));
                let spec: TriggerSpec = serde_json::from_value(Value::Object(patch))
                    .map_err(|error| invalid(error.to_string()))?;
                spec.schedule().map_err(invalid)?;
                approve(
                    &ctx,
                    "create",
                    json!({ "action": "create", "trigger": spec }),
                )
                .await?;
                // The trigger records what governs this session as this
                // agent, as it stands now: its creator's bounds, a ceiling
                // over every run. A run is judged by the policy in force
                // when it happens and by these, whichever is stricter.
                let inherited = sessions.bounds(ctx.session_id, ctx.agent);
                let record = triggers
                    .create_as(
                        params.id.clone().filter(|id| !id.trim().is_empty()),
                        spec,
                        crate::trigger::Creator {
                            origin_session_id: Some(ctx.session_id.to_string()),
                            inherited: Some(inherited),
                            // The session's restriction goes with the
                            // trigger: its runs are bound as this turn is.
                            restricted: sessions.restricted(ctx.session_id),
                            // And so does what it has read: a prompt
                            // written after outside content was seen may
                            // be an outsider's words.
                            outside_content: sessions.seen_outside_content(ctx.session_id),
                        },
                    )
                    .map_err(failed)?;
                Ok(result("Scheduled", &record))
            }
            "update" => {
                let id = required_id(&params)?;
                let patch = patch_of(&params, ctx.session_id, now)?;
                if patch.is_empty() {
                    return Err(invalid("update needs at least one field to change"));
                }
                let Some(record) = triggers.get(id) else {
                    return Err(ToolError::Failed(format!("trigger not found: {id}")));
                };
                // The patch never carries `agent` or `permission`, so the
                // rules the trigger was created under stay as they are.
                authorize(&record, "update", ctx.session_id, sessions.as_ref())?;
                approve(
                    &ctx,
                    "update",
                    json!({ "action": "update", "id": id, "changes": patch }),
                )
                .await?;
                // A change made after outside content was seen may be an
                // outsider's words: the trigger carries the mark from now
                // on, recorded before the change is.
                if sessions.seen_outside_content(ctx.session_id) {
                    triggers.mark_outside_content(id).map_err(failed)?;
                }
                let record = triggers.update(id, &Value::Object(patch)).map_err(failed)?;
                Ok(result("Updated", &record))
            }
            action @ ("pause" | "resume") => {
                let id = required_id(&params)?;
                let Some(record) = triggers.get(id) else {
                    return Err(ToolError::Failed(format!("trigger not found: {id}")));
                };
                authorize(&record, action, ctx.session_id, sessions.as_ref())?;
                approve(&ctx, action, json!({ "action": action, "id": id })).await?;
                let record = triggers
                    .update(id, &json!({ "enabled": action == "resume" }))
                    .map_err(failed)?;
                Ok(result(
                    if action == "pause" {
                        "Paused"
                    } else {
                        "Resumed"
                    },
                    &record,
                ))
            }
            "delete" => {
                let id = required_id(&params)?;
                if let Some(record) = triggers.get(id) {
                    authorize(&record, "delete", ctx.session_id, sessions.as_ref())?;
                }
                approve(&ctx, "delete", json!({ "action": "delete", "id": id })).await?;
                let deleted = triggers.delete(id).map_err(failed)?;
                Ok(ExecuteResult {
                    title: format!("Deleted {id}"),
                    metadata: json!({ "id": id, "deleted": deleted }),
                    output: if deleted {
                        format!("Trigger {id} is deleted.")
                    } else {
                        format!("There is no trigger named {id}.")
                    },
                    attachments: None,
                })
            }
            other => Err(invalid(format!("unknown action: {other}"))),
        }
    })
}

fn result(verb: &str, record: &TriggerRecord) -> ExecuteResult {
    ExecuteResult {
        title: format!("{verb} {}", record.id),
        metadata: json!({ "id": record.id, "trigger": record }),
        output: format!("{verb} trigger {}.\n{}", record.id, render(record)),
        attachments: None,
    }
}

fn render(record: &TriggerRecord) -> String {
    let mut line = format!("- {} — {}", record.id, TriggerService::describe(record));
    if let Some(title) = &record.spec.title {
        line.push_str(&format!(" — {title}"));
    }
    let state = if let Some(error) = &record.error {
        format!("cannot run: {error}")
    } else if !record.enabled {
        "paused".to_string()
    } else if record.running {
        "running now".to_string()
    } else if let Some(next) = record.next {
        format!("next {}", format_instant(next))
    } else if record.spec.on.at.is_some() && record.last_fired.is_some() {
        "done".to_string()
    } else {
        "waiting".to_string()
    };
    line.push_str(&format!(" [{state}]"));
    if record.source == crate::trigger::TriggerSource::Config {
        line.push_str(" (from the configuration)");
    }
    let mut prompt: String = record.spec.prompt.chars().take(200).collect();
    if prompt.len() < record.spec.prompt.len() {
        prompt.push('…');
    }
    format!("{line}\n  prompt: {}", prompt.replace('\n', " "))
}

fn render_list(list: &[TriggerRecord]) -> String {
    if list.is_empty() {
        return "Nothing is scheduled.".to_string();
    }
    let mut lines = vec!["Triggers:".to_string()];
    lines.extend(list.iter().map(render));
    lines.join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::storage::test_support::TempDir;
    use crate::tool::def::Extra;
    use crate::tool::ripgrep::test_support::{ctx, fixed_agents, instance, RecordingAsk};
    use crate::tool::truncate::TruncateService;
    use crate::trigger::{
        TriggerOps, TriggerSettings, TriggerSource, TriggerTurn, TurnFailure, TurnOutcome,
    };
    use alforria_schema::permission_v1::{PermissionV1Action, PermissionV1Rule};

    struct NoTurns;

    impl TriggerOps for NoTurns {
        // The test's sessions are the caller's own.
        fn target(&self, _session_id: &str) -> crate::trigger::SessionTarget {
            crate::trigger::SessionTarget::Found { restricted: false }
        }

        fn run_turn<'a>(
            &'a self,
            _turn: &'a TriggerTurn,
        ) -> BoxFuture<'a, Result<TurnOutcome, TurnFailure>> {
            Box::pin(async { Ok(TurnOutcome::default()) })
        }
    }

    /// `ses_1` (the caller) has the parent `ses_0` and the child `ses_2`.
    #[derive(Default)]
    struct FakeSessions {
        /// The calling session has seen outside content.
        seen: std::sync::atomic::AtomicBool,
    }

    impl ScheduleSessions for FakeSessions {
        fn effective_rules(&self, session_id: &str, agent: &str) -> PermissionV1Ruleset {
            [agent, session_id]
                .into_iter()
                .map(|pattern| PermissionV1Rule {
                    permission: "bash".to_string(),
                    pattern: pattern.to_string(),
                    action: PermissionV1Action::Deny,
                })
                .collect()
        }

        fn seen_outside_content(&self, _session_id: &str) -> bool {
            self.seen.load(std::sync::atomic::Ordering::SeqCst)
        }

        fn ancestors(&self, session_id: &str) -> Vec<String> {
            match session_id {
                "ses_2" => vec!["ses_1".to_string(), "ses_0".to_string()],
                "ses_1" => vec!["ses_0".to_string()],
                _ => Vec::new(),
            }
        }
    }

    struct At(u64);

    impl crate::Clock for At {
        fn now_ms(&self) -> u64 {
            self.0
        }
    }

    struct Setup {
        dir: TempDir,
        triggers: Arc<TriggerService>,
        def: ToolDef,
        sessions: Arc<FakeSessions>,
    }

    const NOW: u64 = 1_800_000_000_000; // 2027-01-15T08:00:00Z

    fn setup(name: &str, enabled: bool) -> Setup {
        let dir = TempDir::new(name);
        let storage =
            Arc::new(crate::storage::Storage::open(dir.path().join("db.sqlite")).unwrap());
        let events = Arc::new(crate::event::EventBus::new_shared(storage.clone(), None));
        let directory = dir.path().join("project");
        std::fs::create_dir_all(directory.join("inbox")).unwrap();
        let triggers = TriggerService::new(
            Arc::new(NoTurns),
            events,
            storage,
            Arc::new(At(NOW)),
            TriggerSettings {
                directory: directory.canonicalize().unwrap(),
                project_id: "global".to_string(),
                home: dir.path().to_path_buf(),
                enabled,
                concurrency: 1,
                max_watched: 100,
                ignore: Vec::new(),
                config: serde_json::from_value(json!({
                    "nightly": { "on": { "cron": "@daily" }, "prompt": "Nightly work" }
                }))
                .unwrap(),
                webhook_min_interval_ms: 0,
            },
        );
        let rules = Arc::new(FakeSessions::default());
        let def = schedule_tool(
            Arc::new(TruncateService::default_limits(dir.path().to_path_buf())),
            fixed_agents(),
            triggers.clone(),
            rules.clone(),
        );
        Setup {
            dir,
            triggers,
            def,
            sessions: rules,
        }
    }

    async fn call(
        setup: &Setup,
        ask: &RecordingAsk,
        args: Value,
    ) -> Result<ExecuteResult, ToolError> {
        let inst = instance(setup.dir.path());
        let extra = Extra::default();
        (setup.def.execute)(args, ctx(ask, &inst, &extra)).await
    }

    #[tokio::test]
    async fn the_agent_creates_lists_changes_pauses_and_deletes_a_trigger() {
        let s = setup("schedule-roundtrip", true);
        let ask = RecordingAsk::new();
        let created = call(
            &s,
            &ask,
            json!({
                "action": "create",
                "id": "standup",
                "prompt": "Summarise yesterday's commits",
                "cron": "0 9 * * MON-FRI",
                "timezone": "Europe/Paris",
                "quiet_token": "NOTHING",
            }),
        )
        .await
        .unwrap();
        assert_eq!(created.title, "Scheduled standup");
        // 2027-01-15 is a Friday; 09:00 in Paris is 08:00 UTC, which is now.
        assert!(
            created
                .output
                .contains("cron 0 9 * * MON-FRI (Europe/Paris)")
                && created.output.contains("[next 2027-01-18T08:00:00Z]"),
            "{}",
            created.output
        );
        let record = s.triggers.get("standup").unwrap();
        assert_eq!(record.source, TriggerSource::Runtime);
        // It runs as the calling agent, under the calling session's rules.
        assert_eq!(record.spec.agent.as_deref(), Some("build"));
        assert_eq!(record.origin_session_id.as_deref(), Some("ses_1"));
        assert_eq!(record.spec.quiet_token.as_deref(), Some("NOTHING"));

        let listed = call(&s, &ask, json!({ "action": "list" })).await.unwrap();
        assert!(
            listed.output.contains("- standup — cron"),
            "{}",
            listed.output
        );
        assert!(
            listed.output.contains("- nightly — cron @daily (UTC) [next 2027-01-16T00:00:00Z] (from the configuration)"),
            "{}",
            listed.output
        );
        assert_eq!(listed.metadata["count"], 2);

        // A new schedule replaces the old one; other fields stay.
        let updated = call(
            &s,
            &ask,
            json!({ "action": "update", "id": "standup", "interval": "2h", "title": "Standup" }),
        )
        .await
        .unwrap();
        assert!(
            updated.output.contains("every 2h — Standup"),
            "{}",
            updated.output
        );
        let record = s.triggers.get("standup").unwrap();
        assert_eq!(record.spec.on.cron, None);
        assert_eq!(record.spec.prompt, "Summarise yesterday's commits");

        let paused = call(&s, &ask, json!({ "action": "pause", "id": "standup" }))
            .await
            .unwrap();
        assert!(paused.output.contains("[paused]"), "{}", paused.output);
        assert!(!s.triggers.get("standup").unwrap().enabled);
        let resumed = call(&s, &ask, json!({ "action": "resume", "id": "standup" }))
            .await
            .unwrap();
        assert!(resumed.output.contains("[next "), "{}", resumed.output);

        let deleted = call(&s, &ask, json!({ "action": "delete", "id": "standup" }))
            .await
            .unwrap();
        assert_eq!(deleted.metadata["deleted"], true);
        assert!(s.triggers.get("standup").is_none());
        let again = call(&s, &ask, json!({ "action": "delete", "id": "standup" }))
            .await
            .unwrap();
        assert_eq!(again.output, "There is no trigger named standup.");

        // Every change asked for the `schedule` permission by action; list did not.
        let asked: Vec<(String, Vec<String>)> = ask
            .requests()
            .into_iter()
            .map(|request| (request.permission, request.patterns))
            .collect();
        let actions: Vec<&str> = asked
            .iter()
            .map(|(_, patterns)| patterns[0].as_str())
            .collect();
        assert_eq!(
            actions,
            ["create", "update", "pause", "resume", "delete", "delete"]
        );
        assert!(asked.iter().all(|(permission, _)| permission == "schedule"));
    }

    #[tokio::test]
    async fn a_reminder_is_a_one_shot_that_can_land_in_this_conversation() {
        let s = setup("schedule-reminder", true);
        let ask = RecordingAsk::new();
        let created = call(
            &s,
            &ask,
            json!({
                "action": "create",
                "prompt": "Remind the user to stretch",
                "in": "20m",
                "session": "current",
            }),
        )
        .await
        .unwrap();
        let id = created.metadata["id"].as_str().unwrap().to_string();
        assert!(id.starts_with("trg_"));
        let record = s.triggers.get(&id).unwrap();
        assert_eq!(record.spec.on.at.as_deref(), Some("2027-01-15T08:20:00Z"));
        assert_eq!(record.spec.session.as_deref(), Some("ses_1"));
        assert_eq!(record.next, Some(NOW + 20 * 60_000));
        assert!(
            created.output.contains("once at 2027-01-15T08:20:00Z"),
            "{}",
            created.output
        );

        let absolute = call(
            &s,
            &ask,
            json!({ "action": "create", "prompt": "Call back", "at": "2027-02-01T10:00:00+01:00" }),
        )
        .await
        .unwrap();
        assert!(
            absolute.output.contains("[next 2027-02-01T09:00:00Z]"),
            "{}",
            absolute.output
        );

        let watch = call(
            &s,
            &ask,
            json!({ "action": "create", "prompt": "File the new document", "watch": "inbox", "debounce": "60s" }),
        )
        .await
        .unwrap();
        assert!(
            watch.output.contains("when inbox changes"),
            "{}",
            watch.output
        );
        let hook = call(
            &s,
            &ask,
            json!({ "action": "create", "id": "deploys", "prompt": "Check the deploy", "webhook": true }),
        )
        .await
        .unwrap();
        assert!(
            hook.output.contains("on a webhook call [waiting]"),
            "{}",
            hook.output
        );
    }

    #[tokio::test]
    async fn bad_requests_are_refused_before_anything_is_asked_or_stored() {
        let s = setup("schedule-invalid", true);
        let ask = RecordingAsk::new();
        for (args, needle) in [
            (json!({ "action": "create", "prompt": "x" }), "needs one of"),
            (
                json!({ "action": "create", "interval": "1h" }),
                "needs a prompt",
            ),
            (
                json!({ "action": "create", "prompt": "x", "interval": "1h", "cron": "@daily" }),
                "exactly one",
            ),
            (
                json!({ "action": "create", "prompt": "x", "cron": "whenever" }),
                "invalid cron",
            ),
            (
                json!({ "action": "create", "prompt": "x", "in": "a while" }),
                "not a duration",
            ),
            (
                json!({ "action": "create", "prompt": "x", "at": "2027-01-01T00:00:00Z", "in": "5m" }),
                "not both",
            ),
            (
                json!({ "action": "create", "prompt": "x", "interval": "1h", "timezone": "UTC" }),
                "goes with `cron`",
            ),
            (
                json!({ "action": "create", "prompt": "x", "interval": "1h", "session": "ses_other" }),
                "session must be",
            ),
            (
                json!({ "action": "create", "prompt": "x", "interval": "1h", "report": "loud" }),
                "report must be",
            ),
            (
                json!({ "action": "update", "id": "nightly" }),
                "at least one field",
            ),
            (json!({ "action": "pause" }), "needs the trigger `id`"),
            (json!({ "action": "explode" }), "unknown action"),
        ] {
            let error = call(&s, &ask, args.clone()).await.unwrap_err();
            assert!(error.to_string().contains(needle), "{args}: {error}");
        }
        assert!(ask.requests().is_empty());
        assert_eq!(s.triggers.list().len(), 1);
        // A path outside the project is refused by the service, after the ask.
        let outside = call(
            &s,
            &ask,
            json!({ "action": "create", "prompt": "x", "watch": "/etc" }),
        )
        .await
        .unwrap_err();
        assert!(outside.to_string().contains("outside"), "{outside}");

        let off = setup("schedule-disabled", false);
        let error = call(&off, &ask, json!({ "action": "list" }))
            .await
            .unwrap_err();
        assert!(error.to_string().contains("disabled"), "{error}");
    }

    #[tokio::test]
    async fn a_created_trigger_inherits_the_rules_of_its_session() {
        let s = setup("schedule-inherit", true);
        let ask = RecordingAsk::new();
        call(
            &s,
            &ask,
            json!({ "action": "create", "id": "job", "prompt": "Work", "webhook": true }),
        )
        .await
        .unwrap();
        // The ask shows the user exactly what would be scheduled.
        let request = ask.requests().remove(0);
        assert_eq!(request.metadata["trigger"]["prompt"], "Work");
        assert_eq!(request.always, ["create"]);
        let stored = crate::trigger::store::TriggerStore::new(
            Arc::new(crate::storage::Storage::open(s.dir.path().join("db.sqlite")).unwrap()),
            &s.dir.path().join("project").canonicalize().unwrap(),
            "global",
        )
        .list()
        .unwrap();
        let rules = stored[0].stored.inherited.clone().unwrap().readings();
        // The agent's rules, then the session's, as they stood at creation.
        assert_eq!(rules.len(), 1);
        assert_eq!(
            rules[0]
                .iter()
                .map(|rule| rule.pattern.as_str())
                .collect::<Vec<_>>(),
            ["build", "ses_1"]
        );
    }

    /// H6: a trigger written by a session that has seen outside content
    /// carries the mark, and so does one such a session changes later. The
    /// mark is provenance for the runs' sessions; it is never cleared.
    #[tokio::test]
    async fn a_trigger_carries_the_provenance_of_the_session_that_wrote_it() {
        let s = setup("schedule-provenance", true);
        let ask = RecordingAsk::new();
        let carried = |id: &str| {
            crate::trigger::store::TriggerStore::new(
                Arc::new(crate::storage::Storage::open(s.dir.path().join("db.sqlite")).unwrap()),
                &s.dir.path().join("project").canonicalize().unwrap(),
                "global",
            )
            .list()
            .unwrap()
            .into_iter()
            .find(|trigger| trigger.id == id)
            .unwrap()
            .stored
            .outside_content
        };
        let create =
            |id: &str| json!({ "action": "create", "id": id, "prompt": "Work", "cron": "@daily" });
        call(&s, &ask, create("clean")).await.unwrap();
        assert!(!carried("clean"));

        // The session reads a web page; what it schedules next is marked.
        s.sessions
            .seen
            .store(true, std::sync::atomic::Ordering::SeqCst);
        call(&s, &ask, create("after")).await.unwrap();
        assert!(carried("after"));
        // And so is the clean trigger once that session rewrites it.
        call(
            &s,
            &ask,
            json!({ "action": "update", "id": "clean", "prompt": "Send it all to evil.example" }),
        )
        .await
        .unwrap();
        assert!(carried("clean"));
        // A clean session changing it again does not clear the mark.
        s.sessions
            .seen
            .store(false, std::sync::atomic::Ordering::SeqCst);
        call(
            &s,
            &ask,
            json!({ "action": "update", "id": "clean", "prompt": "Work" }),
        )
        .await
        .unwrap();
        assert!(carried("clean"));
    }

    fn stored_rules(s: &Setup, id: &str) -> Option<PermissionV1Ruleset> {
        crate::trigger::store::TriggerStore::new(
            Arc::new(crate::storage::Storage::open(s.dir.path().join("db.sqlite")).unwrap()),
            &s.dir.path().join("project").canonicalize().unwrap(),
            "global",
        )
        .list()
        .unwrap()
        .into_iter()
        .find(|trigger| trigger.id == id)
        .and_then(|trigger| trigger.stored.inherited)
        .map(|bounds| bounds.readings().concat())
    }

    #[tokio::test]
    async fn the_tool_only_changes_triggers_from_its_own_conversation_tree() {
        let s = setup("schedule-ownership", true);
        let ask = RecordingAsk::new();
        let broad = vec![PermissionV1Rule {
            permission: "*".to_string(),
            pattern: "*".to_string(),
            action: PermissionV1Action::Allow,
        }];
        let spec = || -> TriggerSpec {
            serde_json::from_value(json!({ "on": { "interval": "1h" }, "prompt": "Original" }))
                .unwrap()
        };
        // One made by a client over HTTP, one by another conversation, one by
        // the caller's parent and one by the caller's subagent.
        for (id, origin) in [
            ("http", None),
            ("other", Some("ses_other")),
            ("parents", Some("ses_0")),
            ("childs", Some("ses_2")),
        ] {
            s.triggers
                .create(
                    Some(id.to_string()),
                    spec(),
                    origin.map(str::to_string),
                    Some(broad.clone()),
                )
                .unwrap();
        }

        // The user's triggers (config, HTTP) and a stranger's are off limits,
        // for every changing action, before anything is asked.
        for id in ["nightly", "http", "other"] {
            for args in [
                json!({ "action": "update", "id": id, "prompt": "Do something else" }),
                json!({ "action": "pause", "id": id }),
                json!({ "action": "resume", "id": id }),
                json!({ "action": "delete", "id": id }),
            ] {
                let error = call(&s, &ask, args.clone()).await.unwrap_err();
                assert!(
                    error
                        .to_string()
                        .contains("was not scheduled by this conversation"),
                    "{args}: {error}"
                );
            }
            let record = s.triggers.get(id).expect("still there");
            assert!(record.enabled);
            assert_ne!(record.spec.prompt, "Do something else");
        }
        assert!(ask.requests().is_empty());

        // A trigger of the caller's parent runs under the parent's rules,
        // which may be wider than the caller's: the caller may stop it, but
        // not redirect it or start it again.
        for args in [
            json!({ "action": "update", "id": "parents", "prompt": "Do something else" }),
            json!({ "action": "update", "id": "parents", "session": "current" }),
            json!({ "action": "resume", "id": "parents" }),
        ] {
            assert!(call(&s, &ask, args).await.is_err());
        }
        assert_eq!(s.triggers.get("parents").unwrap().spec.prompt, "Original");
        call(&s, &ask, json!({ "action": "pause", "id": "parents" }))
            .await
            .unwrap();
        assert!(!s.triggers.get("parents").unwrap().enabled);
        call(&s, &ask, json!({ "action": "delete", "id": "parents" }))
            .await
            .unwrap();
        assert!(s.triggers.get("parents").is_none());

        // A trigger of the caller's own subagent is the caller's to change,
        // and the change leaves the rules it was created under alone.
        call(
            &s,
            &ask,
            json!({ "action": "update", "id": "childs", "prompt": "Narrower job" }),
        )
        .await
        .unwrap();
        let record = s.triggers.get("childs").unwrap();
        assert_eq!(record.spec.prompt, "Narrower job");
        assert_eq!(record.spec.agent, None, "an update never sets the agent");
        assert_eq!(record.spec.permission, None);
        assert_eq!(stored_rules(&s, "childs"), Some(broad));
    }

    #[tokio::test]
    async fn agents_cannot_schedule_without_end() {
        let s = setup("schedule-cap", true);
        let ask = RecordingAsk::new();
        for index in 0..crate::trigger::MAX_AGENT_TRIGGERS {
            call(
                &s,
                &ask,
                json!({ "action": "create", "id": format!("job-{index}"), "prompt": "x", "webhook": true }),
            )
            .await
            .unwrap();
        }
        let error = call(
            &s,
            &ask,
            json!({ "action": "create", "prompt": "one more", "webhook": true }),
        )
        .await
        .unwrap_err();
        assert!(
            error.to_string().contains("agents have already scheduled"),
            "{error}"
        );
        // The user is not bound by the agents' cap.
        s.triggers
            .create(
                None,
                serde_json::from_value(json!({ "on": { "webhook": true }, "prompt": "mine" }))
                    .unwrap(),
                None,
                None,
            )
            .unwrap();
    }

    /// A session whose restriction cannot be read schedules as a
    /// restricted one: the trigger it makes is bound, not free.
    #[test]
    fn an_unreadable_restriction_counts_as_one_when_scheduling() {
        let h = crate::session::test_support::harness("schedule-fail-closed", Vec::new());
        let session =
            crate::session::test_support::create_session(&h.services.sessions, &h.worktree);
        let tree = SessionTree {
            sessions: h.services.sessions.clone(),
            agents: h.services.agents.clone(),
        };
        assert!(!tree.restricted(&session.id));
        h.services.sessions.restrict(&session.id).unwrap();
        assert!(tree.restricted(&session.id));
        h.services.storage.with_connection(|conn| {
            conn.execute_batch("ALTER TABLE session_restriction RENAME TO restriction_gone;")
                .unwrap()
        });
        assert!(tree.restricted(&session.id));
        assert!(tree.restricted("ses_unknown"));
    }
}
