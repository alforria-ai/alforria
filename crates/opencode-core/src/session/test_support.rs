//! Shared M5.6 test harness — a scripted [`LlmStream`], a static
//! [`ModelSource`], fixed clock/permission doubles and a tempdir
//! service bundle, so compaction/summary/revert tests can drive the real
//! stores against SQLite (spec §6).

use std::collections::VecDeque;
use std::sync::{Arc, Mutex};

use futures::StreamExt;
use opencode_llm::schema::errors::LlmError;
use opencode_llm::schema::events::LlmEvent;
use opencode_schema::session_v1::{
    AssistantTime, UserTime, V1Message, V1Path, V1StepTokens, V1TokenCache, V1UserModel,
};
use serde_json::Value;

use crate::session::agents::AgentRegistryInput;
use crate::session::llm::{LlmEventStream, LlmModel, LlmStream, StreamInput};
use crate::session::message::WithParts;
use crate::session::overflow::ModelLimits;
use crate::session::processor::{AskPermission, PermissionAsk, PermissionAskError};
use crate::session::r#loop::{ModelSource, ResolvedModel};
use crate::session::run_state::{BackgroundJobInfo, BackgroundJobs};
use crate::session::snapshot::InMemorySnapshot;
use crate::session::store::{CreateInput, SessionContext, SessionStore};
use crate::session::usage::{CacheCost, ModelCost};
use crate::session::SessionServices;
use crate::Clock;

// ---------------------------------------------------------------------------
// Clock / jobs / permission doubles
// ---------------------------------------------------------------------------

pub(crate) struct FixedClock;

impl Clock for FixedClock {
    fn now_ms(&self) -> u64 {
        1_761_000_000_000
    }
}

pub(crate) struct NoJobs;

impl BackgroundJobs for NoJobs {
    fn list(&self) -> Result<Vec<BackgroundJobInfo>, crate::CoreError> {
        Ok(Vec::new())
    }
    fn cancel(&self, _: &str) -> Result<(), crate::CoreError> {
        Ok(())
    }
}

/// Auto-approving permission seam.
pub(crate) struct AllowAll;

impl AskPermission for AllowAll {
    fn ask<'a>(
        &'a self,
        _request: PermissionAsk,
    ) -> futures::future::BoxFuture<'a, Result<(), PermissionAskError>> {
        Box::pin(async { Ok(()) })
    }
}

// ---------------------------------------------------------------------------
// Mock LLM
// ---------------------------------------------------------------------------

/// A scripted [`LlmStream`]: each `stream()` call pops the next script.
pub(crate) struct MockLlm {
    script: Mutex<VecDeque<Vec<Result<LlmEvent, LlmError>>>>,
    inputs: Mutex<Vec<StreamInput>>,
}

pub(crate) type MockScript = Vec<Vec<Result<LlmEvent, LlmError>>>;

impl MockLlm {
    pub(crate) fn new(script: MockScript) -> Arc<Self> {
        Arc::new(MockLlm {
            script: Mutex::new(script.into_iter().collect()),
            inputs: Mutex::new(Vec::new()),
        })
    }

    pub(crate) fn calls(&self) -> usize {
        self.inputs.lock().unwrap().len()
    }
}

impl LlmStream for MockLlm {
    fn stream(&self, input: StreamInput) -> LlmEventStream {
        self.inputs.lock().unwrap().push(input.clone());
        let events = self.script.lock().unwrap().pop_front().unwrap_or_default();
        futures::stream::iter(events).boxed()
    }
}

// ---------------------------------------------------------------------------
// Model source
// ---------------------------------------------------------------------------

pub(crate) fn test_model() -> ResolvedModel {
    ResolvedModel {
        llm: LlmModel {
            id: "claude".to_string(),
            provider_id: "anthropic".to_string(),
            api_id: "claude".to_string(),
            api_npm: "@ai-sdk/anthropic".to_string(),
            temperature_capable: false,
            headers: Default::default(),
            options: Default::default(),
            context_limit: 200_000.0,
            output_limit: 100.0,
            output_token_max: None,
        },
        cost: ModelCost {
            input: 0.0,
            output: 0.0,
            cache: CacheCost {
                read: 0.0,
                write: 0.0,
            },
            tiers: Vec::new(),
            experimental_over_200k: None,
        },
        limits: ModelLimits {
            context: 200_000.0,
            input: None,
            output: 100.0,
        },
        output_token_max: None,
    }
}

/// `getModel` / `getSmallModel` stub returning one fixed model.
pub(crate) struct StaticModels {
    pub(crate) model: ResolvedModel,
}

impl ModelSource for StaticModels {
    fn get_model<'a>(
        &'a self,
        _provider_id: &'a str,
        _model_id: &'a str,
        _session_id: &'a str,
    ) -> futures::future::BoxFuture<'a, Result<ResolvedModel, crate::session::r#loop::LoopError>>
    {
        Box::pin(async { Ok(self.model.clone()) })
    }

    fn get_small_model<'a>(
        &'a self,
        _provider_id: &'a str,
    ) -> futures::future::BoxFuture<'a, Option<ResolvedModel>> {
        Box::pin(async { None })
    }
}

// ---------------------------------------------------------------------------
// Harness
// ---------------------------------------------------------------------------

pub(crate) struct Harness {
    pub(crate) temp: crate::storage::test_support::TempDir,
    pub(crate) services: SessionServices,
    pub(crate) config: crate::config::schema::Config,
    pub(crate) llm: Arc<MockLlm>,
    pub(crate) snapshot: Arc<InMemorySnapshot>,
    pub(crate) worktree: std::path::PathBuf,
}

pub(crate) fn harness(name: &str, script: MockScript) -> Harness {
    harness_with_config(name, script, serde_json::json!({}))
}

pub(crate) fn harness_with_config(name: &str, script: MockScript, config: Value) -> Harness {
    let temp = crate::storage::test_support::TempDir::new(name);
    let worktree = temp.path().join("repo");
    std::fs::create_dir_all(&worktree).unwrap();
    let config: crate::config::schema::Config =
        serde_json::from_value(config).expect("valid config");
    let agent_input = AgentRegistryInput {
        config: config.clone(),
        skill_dirs: Vec::new(),
        reference_dirs: Vec::new(),
        worktree: worktree.clone(),
        data_dir: temp.path().to_path_buf(),
        tmp_dir: temp.path().to_path_buf(),
        home: temp.path().to_path_buf(),
    };
    let services = SessionServices::new(
        Arc::new(crate::storage::Storage::open(temp.path().join("db.sqlite")).unwrap()),
        Arc::new(NoJobs),
        Arc::new(FixedClock),
        &agent_input,
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
    Harness {
        snapshot: InMemorySnapshot::new(worktree.clone()),
        temp,
        services,
        config,
        llm: MockLlm::new(script),
        worktree,
    }
}

pub(crate) fn static_models() -> StaticModels {
    StaticModels {
        model: test_model(),
    }
}

pub(crate) fn create_session(
    store: &SessionStore,
    worktree: &std::path::Path,
) -> opencode_schema::session_v1::V1SessionInfo {
    store
        .create(
            &SessionContext {
                project_id: "global".to_string(),
                directory: worktree.to_path_buf(),
                worktree: worktree.to_path_buf(),
                workspace_id: None,
            },
            &CreateInput::default(),
        )
        .unwrap()
}

// ---------------------------------------------------------------------------
// Message / part builders
// ---------------------------------------------------------------------------

pub(crate) fn user_message(session: &str, id: &str, created: f64) -> V1Message {
    V1Message::User {
        id: id.to_string(),
        session_id: session.to_string(),
        time: UserTime { created },
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
    }
}

pub(crate) fn assistant_message(
    session: &str,
    id: &str,
    parent: &str,
    created: u64,
    summary: Option<bool>,
    finish: Option<String>,
) -> V1Message {
    V1Message::Assistant {
        id: id.to_string(),
        session_id: session.to_string(),
        time: AssistantTime {
            created,
            completed: None,
        },
        error: None,
        parent_id: parent.to_string(),
        model_id: "claude".to_string(),
        provider_id: "anthropic".to_string(),
        mode: "primary".to_string(),
        agent: "build".to_string(),
        path: V1Path {
            cwd: "/repo".to_string(),
            root: "/repo".to_string(),
        },
        summary,
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
        finish,
    }
}

/// A text part carrying the given `text`.
pub(crate) fn text_part(
    session: &str,
    message: &str,
    id: &str,
    text: &str,
) -> opencode_schema::session_v1::V1Part {
    opencode_schema::session_v1::V1Part::Text {
        id: id.to_string(),
        session_id: session.to_string(),
        message_id: message.to_string(),
        text: text.to_string(),
        synthetic: None,
        ignored: None,
        time: None,
        metadata: None,
    }
}

/// `WithParts` helper.
pub(crate) fn with_parts(
    info: V1Message,
    parts: Vec<opencode_schema::session_v1::V1Part>,
) -> WithParts {
    WithParts { info, parts }
}

/// A compaction part.
pub(crate) fn compaction_part(
    session: &str,
    message: &str,
    id: &str,
    auto: bool,
    overflow: Option<bool>,
    tail: Option<&str>,
) -> opencode_schema::session_v1::V1Part {
    opencode_schema::session_v1::V1Part::Compaction {
        id: id.to_string(),
        session_id: session.to_string(),
        message_id: message.to_string(),
        auto,
        overflow,
        tail_start_id: tail.map(|tail| tail.to_string()),
    }
}

/// A minimal text stream: text + finish.
pub(crate) fn text_stream(text: &str) -> Vec<Result<LlmEvent, LlmError>> {
    use opencode_llm::schema::ids::FinishReason;
    vec![
        Ok(LlmEvent::TextStart {
            id: "t1".to_string(),
            provider_metadata: None,
        }),
        Ok(LlmEvent::TextDelta {
            id: "t1".to_string(),
            text: text.to_string(),
            provider_metadata: None,
        }),
        Ok(LlmEvent::TextEnd {
            id: "t1".to_string(),
            provider_metadata: None,
        }),
        Ok(LlmEvent::Finish {
            reason: FinishReason::Stop,
            usage: None,
            provider_metadata: None,
        }),
    ]
}
