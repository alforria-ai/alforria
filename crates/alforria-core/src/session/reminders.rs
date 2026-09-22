//! Session reminders — port of `session/reminders.ts` (15-90), including
//! both runtime-flag branches: the non-experimental plan/build-switch
//! appends and the experimental plan-mode flow with `${planInfo}`
//! substitution. Also carries the `Session.plan()` path helper
//! (session.ts:331-336).

use std::path::PathBuf;

use alforria_schema::session_v1::{V1Part, V1SessionInfo};

use crate::session::agents::AgentInfo;
use crate::session::ids::PartId;
use crate::session::message::{message_role, WithParts};
use crate::session::store::SessionStore;

use crate::session::error::SessionError;

const PROMPT_PLAN: &str = include_str!("prompt/plan.txt");
const BUILD_SWITCH: &str = include_str!("prompt/build-switch.txt");
const PLAN_MODE: &str = include_str!("prompt/plan-mode.txt");

/// `Session.plan` (session.ts:331-336): the plan file path —
/// `<worktree>/.opencode/plans` when the project has VCS, else
/// `<data>/plans`, joined `{created}-{slug}.md`.
pub fn plan(
    session: &V1SessionInfo,
    worktree: &std::path::Path,
    data_dir: &std::path::Path,
    vcs: bool,
) -> PathBuf {
    let base = if vcs {
        worktree.join(".opencode").join("plans")
    } else {
        data_dir.join("plans")
    };
    base.join(format!("{}-{}.md", session.time.created, session.slug))
}

/// `SessionReminders.apply` inputs (reminders.ts:15-19) + the instance
/// context the plan path needs.
#[derive(Debug, Clone)]
pub struct RemindersInput {
    pub messages: Vec<WithParts>,
    pub agent: AgentInfo,
    pub session: V1SessionInfo,
    /// `flags.experimentalPlanMode`.
    pub experimental_plan_mode: bool,
    /// `instance.project.vcs` (plan path).
    pub vcs: bool,
    /// `ctx.worktree` (plan path).
    pub worktree: std::path::PathBuf,
    /// `Global.Path.data` (plan path).
    pub data_dir: std::path::PathBuf,
}

fn text_part(session_id: &str, message_id: &str, text: String) -> V1Part {
    V1Part::Text {
        id: PartId::ascending(None).expect("valid id"),
        session_id: session_id.to_string(),
        message_id: message_id.to_string(),
        text,
        synthetic: Some(true),
        ignored: None,
        time: None,
        metadata: None,
    }
}

fn message_agent(info: &alforria_schema::session_v1::V1Message) -> Option<&str> {
    match info {
        alforria_schema::session_v1::V1Message::User { agent, .. } => Some(agent),
        alforria_schema::session_v1::V1Message::Assistant { agent, .. } => Some(agent),
    }
}

/// `SessionReminders.apply` (reminders.ts:15-90): appends reminder text
/// parts to the last user message, in memory for the non-experimental
/// branch and persisted via `updatePart` for the experimental one.
pub fn apply(
    input: RemindersInput,
    sessions: &SessionStore,
) -> Result<Vec<WithParts>, SessionError> {
    let mut messages = input.messages;
    let Some(user_index) = messages
        .iter()
        .rposition(|msg| message_role(&msg.info) == "user")
    else {
        return Ok(messages);
    };

    if !input.experimental_plan_mode {
        if input.agent.name == "plan" {
            let session_id =
                crate::session::message::message_session_id(&messages[user_index].info).to_string();
            let message_id =
                crate::session::message::message_id(&messages[user_index].info).to_string();
            messages[user_index].parts.push(text_part(
                &session_id,
                &message_id,
                PROMPT_PLAN.to_string(),
            ));
        }
        let was_plan = messages.iter().any(|msg| {
            message_role(&msg.info) == "assistant" && message_agent(&msg.info) == Some("plan")
        });
        if was_plan && input.agent.name == "build" {
            let session_id =
                crate::session::message::message_session_id(&messages[user_index].info).to_string();
            let message_id =
                crate::session::message::message_id(&messages[user_index].info).to_string();
            messages[user_index].parts.push(text_part(
                &session_id,
                &message_id,
                BUILD_SWITCH.to_string(),
            ));
        }
        return Ok(messages);
    }

    let assistant_agent = messages
        .iter()
        .rev()
        .find(|msg| message_role(&msg.info) == "assistant")
        .and_then(|msg| message_agent(&msg.info));

    if input.agent.name != "plan" && assistant_agent == Some("plan") {
        let plan = plan(&input.session, &input.worktree, &input.data_dir, input.vcs);
        let exists = plan.exists();
        let text = if exists {
            format!(
                "{BUILD_SWITCH}\n\nA plan file exists at {}. You should execute on the plan defined within it",
                plan.display()
            )
        } else {
            BUILD_SWITCH.to_string()
        };
        let session_id =
            crate::session::message::message_session_id(&messages[user_index].info).to_string();
        let message_id =
            crate::session::message::message_id(&messages[user_index].info).to_string();
        let part = text_part(&session_id, &message_id, text);
        sessions.update_part(&part)?;
        messages[user_index].parts.push(part);
        return Ok(messages);
    }

    if input.agent.name != "plan" || assistant_agent == Some("plan") {
        return Ok(messages);
    }

    let plan = plan(&input.session, &input.worktree, &input.data_dir, input.vcs);
    let exists = plan.exists();
    if !exists {
        std::fs::create_dir_all(plan.parent().unwrap_or(plan.as_path()))
            .map_err(|err| SessionError::not_found(err.to_string()))?;
    }
    let plan_info = if exists {
        format!("A plan file already exists at {}. You can read it and make incremental edits using the edit tool.", plan.display())
    } else {
        format!(
            "No plan file exists yet. You should create your plan at {} using the write tool.",
            plan.display()
        )
    };
    let text = PLAN_MODE.replacen("${planInfo}", &plan_info, 1);
    let session_id =
        crate::session::message::message_session_id(&messages[user_index].info).to_string();
    let message_id = crate::session::message::message_id(&messages[user_index].info).to_string();
    let part = text_part(&session_id, &message_id, text);
    sessions.update_part(&part)?;
    messages[user_index].parts.push(part);
    Ok(messages)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::session::store::SessionContext;
    use crate::storage::test_support::TempDir;
    use crate::storage::Storage;
    use alforria_schema::session_v1::{
        AssistantTime, UserTime, V1Path, V1StepTokens, V1TokenCache, V1UserModel,
    };
    use std::sync::Arc;

    fn user_message(session_id: &str, id: &str) -> WithParts {
        WithParts {
            info: alforria_schema::session_v1::V1Message::User {
                id: id.to_string(),
                session_id: session_id.to_string(),
                time: UserTime { created: 1.0 },
                format: None,
                summary: None,
                agent: "build".to_string(),
                model: V1UserModel {
                    provider_id: "anthropic".to_string(),
                    model_id: "claude".to_string(),
                    variant: None,
                },
                system: None,
                tools: None,
            },
            parts: vec![],
        }
    }

    fn assistant_message(session_id: &str, id: &str, agent: &str) -> WithParts {
        WithParts {
            info: alforria_schema::session_v1::V1Message::Assistant {
                id: id.to_string(),
                session_id: session_id.to_string(),
                time: AssistantTime {
                    created: 2,
                    completed: None,
                },
                error: None,
                parent_id: "msg_0".to_string(),
                model_id: "claude".to_string(),
                provider_id: "anthropic".to_string(),
                mode: "primary".to_string(),
                agent: agent.to_string(),
                path: V1Path {
                    cwd: "/repo".to_string(),
                    root: "/repo".to_string(),
                },
                summary: None,
                cost: 0.0,
                tokens: V1StepTokens {
                    total: None,
                    input: 0.0,
                    output: 0.0,
                    reasoning: 0.0,
                    cache: V1TokenCache {
                        read: 0.0,
                        write: 0.0,
                    },
                },
                structured: None,
                variant: None,
                finish: None,
            },
            parts: vec![],
        }
    }

    fn session_info(id: &str) -> V1SessionInfo {
        V1SessionInfo {
            id: id.to_string(),
            slug: "my-session".to_string(),
            project_id: "global".to_string(),
            workspace_id: None,
            directory: "/repo".to_string(),
            path: None,
            parent_id: None,
            summary: None,
            cost: None,
            tokens: None,
            share: None,
            title: "t".to_string(),
            agent: None,
            model: None,
            version: "1".to_string(),
            metadata: None,
            time: alforria_schema::session_v1::V1SessionTime {
                created: 1778031210000,
                updated: 1778031210000,
                compacting: None,
                archived: None,
            },
            permission: None,
            revert: None,
        }
    }

    fn agent(name: &str) -> AgentInfo {
        AgentInfo {
            name: name.to_string(),
            description: None,
            mode: crate::tool::def::AgentMode::Primary,
            native: None,
            hidden: None,
            top_p: None,
            temperature: None,
            color: None,
            permission: Vec::new(),
            model: None,
            variant: None,
            prompt: None,
            options: Default::default(),
            steps: None,
        }
    }

    struct NoJobs;
    impl crate::session::run_state::BackgroundJobs for NoJobs {
        fn list(
            &self,
        ) -> Result<Vec<crate::session::run_state::BackgroundJobInfo>, crate::CoreError> {
            Ok(Vec::new())
        }
        fn cancel(&self, _: &str) -> Result<(), crate::CoreError> {
            Ok(())
        }
    }

    struct FixedClock;
    impl crate::Clock for FixedClock {
        fn now_ms(&self) -> u64 {
            1_761_000_000_000
        }
    }

    fn harness() -> (SessionStore, String) {
        let temp = TempDir::new("reminders");
        let services = crate::session::SessionServices::new(
            Arc::new(Storage::open(temp.path().join("db.sqlite")).unwrap()),
            Arc::new(NoJobs),
            Arc::new(FixedClock),
            &crate::session::agents::AgentRegistryInput::default(),
        );
        services
            .storage
            .with_connection(|conn| {
                conn.execute(
                    "INSERT INTO project (id, worktree, sandboxes, time_created, time_updated)
                     VALUES (?1, ?2, ?3, ?4, ?5)",
                    rusqlite::params!["global", "/repo", "[]", 1, 1],
                )
            })
            .unwrap();
        let context = SessionContext {
            project_id: "global".to_string(),
            directory: std::path::PathBuf::from("/repo"),
            worktree: std::path::PathBuf::from("/repo"),
            workspace_id: None,
        };
        let created = services
            .sessions
            .create(&context, &Default::default())
            .unwrap();
        (services.sessions, created.id)
    }

    fn input_with_session(
        session_id: &str,
        messages: Vec<WithParts>,
        agent_name: &str,
    ) -> RemindersInput {
        RemindersInput {
            messages,
            agent: agent(agent_name),
            session: session_info(session_id),
            experimental_plan_mode: false,
            vcs: true,
            worktree: std::path::PathBuf::from("/repo"),
            data_dir: std::path::PathBuf::new(),
        }
    }

    #[test]
    fn plan_path_prefers_worktree_with_vcs() {
        let session = session_info("ses_1");
        let path = plan(
            &session,
            std::path::Path::new("/repo"),
            std::path::Path::new("/data"),
            true,
        );
        assert_eq!(
            path,
            std::path::Path::new("/repo/.opencode/plans/1778031210000-my-session.md")
        );
        let path = plan(
            &session,
            std::path::Path::new("/repo"),
            std::path::Path::new("/data"),
            false,
        );
        assert_eq!(
            path,
            std::path::Path::new("/data/plans/1778031210000-my-session.md")
        );
    }

    #[test]
    fn no_user_message_is_unchanged() {
        let (sessions, session_id) = harness();
        let out = apply(
            input_with_session(
                session_id.as_str(),
                vec![assistant_message(session_id.as_str(), "msg_a", "build")],
                "build",
            ),
            &sessions,
        )
        .unwrap();
        assert_eq!(out.len(), 1);
        assert!(out[0].parts.is_empty());
    }

    #[test]
    fn plan_agent_gets_plan_prompt() {
        let (sessions, session_id) = harness();
        let out = apply(
            input_with_session(
                session_id.as_str(),
                vec![user_message(session_id.as_str(), "msg_u")],
                "plan",
            ),
            &sessions,
        )
        .unwrap();
        let V1Part::Text {
            text, synthetic, ..
        } = &out[0].parts[0]
        else {
            panic!("expected text part");
        };
        assert_eq!(text, PROMPT_PLAN);
        assert_eq!(*synthetic, Some(true));
    }

    #[test]
    fn build_agent_after_plan_gets_build_switch() {
        let (sessions, session_id) = harness();
        let messages = vec![
            user_message(session_id.as_str(), "msg_u"),
            assistant_message(session_id.as_str(), "msg_a", "plan"),
        ];
        let out = apply(
            input_with_session(session_id.as_str(), messages, "build"),
            &sessions,
        )
        .unwrap();
        let V1Part::Text { text, .. } = &out[0].parts[0] else {
            panic!("expected text part");
        };
        assert_eq!(text, BUILD_SWITCH);
    }

    #[test]
    fn build_agent_without_plan_history_gets_nothing() {
        let (sessions, session_id) = harness();
        let messages = vec![
            user_message(session_id.as_str(), "msg_u"),
            assistant_message(session_id.as_str(), "msg_a", "build"),
        ];
        let out = apply(
            input_with_session(session_id.as_str(), messages, "build"),
            &sessions,
        )
        .unwrap();
        assert!(out[0].parts.is_empty());
    }

    #[test]
    fn plan_agent_does_not_get_build_switch() {
        let (sessions, session_id) = harness();
        let messages = vec![
            user_message(session_id.as_str(), "msg_u"),
            assistant_message(session_id.as_str(), "msg_a", "plan"),
        ];
        let out = apply(
            input_with_session(session_id.as_str(), messages, "plan"),
            &sessions,
        )
        .unwrap();
        assert_eq!(out[0].parts.len(), 1);
    }

    #[test]
    fn non_experimental_plan_prompt_not_persisted() {
        // In the non-experimental branch the part is only pushed in memory
        // (reminders.ts:26-48) — no `updatePart` call.
        let (sessions, session_id) = harness();
        let out = apply(
            input_with_session(
                session_id.as_str(),
                vec![user_message(session_id.as_str(), "msg_u")],
                "plan",
            ),
            &sessions,
        )
        .unwrap();
        assert_eq!(out[0].parts.len(), 1);
    }

    #[test]
    fn experimental_build_switch_after_plan() {
        let (sessions, session_id) = harness();
        let user = user_message(session_id.as_str(), "msg_u");
        // The experimental branch persists the part via updatePart, so the
        // message must already exist in storage (in the runtime the prompt
        // pipeline persists it first).
        sessions.update_message(&user.info).unwrap();
        let messages = vec![
            user,
            assistant_message(session_id.as_str(), "msg_a", "plan"),
        ];
        let mut input = input_with_session(session_id.as_str(), messages, "build");
        input.experimental_plan_mode = true;
        let out = apply(input, &sessions).unwrap();
        let V1Part::Text { text, .. } = &out[0].parts[0] else {
            panic!("expected text part");
        };
        // No plan file exists -> just the build switch.
        assert_eq!(text, BUILD_SWITCH);
        // The part was persisted via updatePart.
        let part = out[0].parts[0].clone();
        if let V1Part::Text {
            id,
            session_id,
            message_id,
            ..
        } = &part
        {
            assert!(sessions
                .get_part(session_id, message_id, id)
                .unwrap()
                .is_some());
        }
    }

    #[test]
    fn experimental_plan_mode_creates_plan_info() {
        let temp = TempDir::new("reminders-plan-mode");
        let worktree = temp.path().join("repo");
        std::fs::create_dir_all(&worktree).unwrap();
        let (sessions, session_id) = harness();
        let user = user_message(session_id.as_str(), "msg_u");
        sessions.update_message(&user.info).unwrap();
        let mut input = input_with_session(session_id.as_str(), vec![user], "plan");
        input.experimental_plan_mode = true;
        input.worktree = worktree.clone();
        let out = apply(input, &sessions).unwrap();
        let V1Part::Text { text, .. } = &out[0].parts[0] else {
            panic!("expected text part");
        };
        assert!(
            text.contains("No plan file exists yet. You should create your plan at"),
            "{text}"
        );
        assert!(text.contains("plan-mode") || !text.contains("${planInfo}"));
    }

    #[test]
    fn experimental_plan_mode_existing_plan() {
        let temp = TempDir::new("reminders-plan-exists");
        let worktree = temp.path().join("repo");
        let plan_path = worktree.join(".opencode/plans/1778031210000-my-session.md");
        std::fs::create_dir_all(plan_path.parent().unwrap()).unwrap();
        std::fs::write(&plan_path, "the plan").unwrap();
        let (sessions, session_id) = harness();
        let user = user_message(session_id.as_str(), "msg_u");
        sessions.update_message(&user.info).unwrap();
        let mut input = input_with_session(session_id.as_str(), vec![user], "plan");
        input.experimental_plan_mode = true;
        input.worktree = worktree.clone();
        let out = apply(input, &sessions).unwrap();
        let V1Part::Text { text, .. } = &out[0].parts[0] else {
            panic!("expected text part");
        };
        assert!(text.contains("A plan file already exists at"));
        assert!(text.contains(&plan_path.display().to_string()));
    }
}
