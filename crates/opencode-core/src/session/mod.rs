//! Session engine (M5) — `SessionServices` is the per-instance bundle
//! (spec §2.1) replacing TS's Effect layers.

pub mod agents;
pub mod background;
pub mod compaction;
pub mod error;
pub mod event_definitions;
pub mod from_error;
pub mod ids;
pub mod instruction;
pub mod llm;
pub mod r#loop;
pub mod message;
pub mod overflow;
pub mod permission;
pub mod processor;
pub mod prompt;
pub mod prompt_input;
pub mod question;
pub mod reminders;
pub mod render;
pub mod retry;
pub mod revert;
pub mod run_state;
pub mod snapshot;
pub mod status;
pub mod store;
pub mod subtask;
pub mod summary;
pub mod system;
pub mod task_ops;
pub mod tools;
pub mod usage;

#[cfg(test)]
pub(crate) mod e2e;
#[cfg(test)]
pub(crate) mod test_support;

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use opencode_schema::location::{LocationProject, LocationRef};
use opencode_schema::project::ProjectInfo as WireProjectInfo;

use crate::event::bus::EventBus;
use crate::git::SubprocessGit;
use crate::project::registry::{ProjectRegistry, RegistryError};
use crate::storage::Storage;
use crate::Clock;

pub use agents::{AgentInfo, AgentRegistry, AgentRegistryInput, DefaultAgentError};
pub use background::BackgroundJobService;
pub use compaction::{
    build_prompt, completed_compactions, preserve_recent_budget, select, serialize, summary_text,
    token_estimate, turns, BuildPrompt, CompactionDeps, CompactionOutcome, CompletedCompaction,
    SelectInput, Selected, SessionCompaction, Tail, Turn, MAX_PRESERVE_RECENT_TOKENS,
    MIN_PRESERVE_RECENT_TOKENS, PRUNE_MINIMUM, PRUNE_PROTECT, PRUNE_PROTECTED_TOOLS,
    SUMMARY_TEMPLATE, SUMMARY_UPDATE_INSTRUCTIONS, TOOL_OUTPUT_MAX_CHARS,
};
pub use error::{AuthError, BusyError, NotFoundError, OutputLengthError, SessionError};
pub use message::{filter_compacted, latest, Cursor, Latest, MessagePage, MessageStore, WithParts};
pub use overflow::{is_overflow, max_output_tokens, usable, ModelLimits};
pub use permission::{
    PermissionError, PermissionService, SessionAsk, PERMISSION_ASKED, PERMISSION_REPLIED,
};
pub use prompt::{
    parse_model, shell_impl, CommandFilePart, CommandInput, SessionPrompt, SessionPromptDeps,
    ShellInput, COMMAND_EXECUTED,
};
pub use question::{
    QuestionError, QuestionService, QUESTION_ASKED, QUESTION_REJECTED, QUESTION_REPLIED,
};
pub use retry::{
    delay as retry_delay, retryable, Policy as RetryPolicy, PolicyStep as RetryPolicyStep,
    RetryAction, RetrySet, Retryable as RetryableError,
};
pub use revert::{write_session_diff, RevertDeps, RevertInput, SessionRevert};
pub use run_state::{
    BackgroundJobInfo, BackgroundJobStatus, BackgroundJobs, Latch, Runner, RunnerError,
    SessionRunState, ShellError,
};
pub use status::SessionStatusService;
pub use store::GlobalInfo;
pub use store::{
    get_forked_title, is_default_title, register_projectors, session_path, CreateInput,
    GlobalListInput, ListInput, ProjectInfo, SessionContext, SessionStore, SetClear,
    INSTALLATION_VERSION,
};
pub use subtask::{SessionSubtask, SubtaskDeps};
pub use summary::{unquote_git_path, SessionSummary, SummaryDeps};
pub use task_ops::{ProductionTaskOps, PromptFacade};
pub use usage::{get_usage, CacheCost, CostTier, GetUsage, ModelCost, Over200k, UsageCost};

/// The per-instance context — TS `InstanceContext`
/// (`project/instance-context.ts:5-9`): `{ directory, worktree, project }`
/// captured at instance construction, plus the ambient workspace id.
#[derive(Debug, Clone)]
pub struct InstanceLocation {
    pub directory: PathBuf,
    pub worktree: PathBuf,
    pub project: WireProjectInfo,
    pub workspace_id: Option<String>,
}

/// The per-instance session services (spec §2.1): TS resolves these from
/// Effect layers; Rust passes this bundle explicitly.
#[derive(Clone)]
pub struct SessionServices {
    pub storage: Arc<Storage>,
    pub events: Arc<EventBus>,
    pub sessions: SessionStore,
    pub messages: MessageStore,
    pub status: Arc<SessionStatusService>,
    pub run_state: Arc<SessionRunState>,
    pub agents: AgentRegistry,
    pub permission: Arc<PermissionService>,
    pub question: Arc<QuestionService>,
    clock: Arc<dyn Clock>,
    location: Arc<Mutex<Option<InstanceLocation>>>,
}

impl SessionServices {
    /// Wire the full M5.1 service graph: shared storage + event bus with
    /// the session projectors registered, the stores, the status map, the
    /// run state and the agent registry.
    pub fn new(
        storage: Arc<Storage>,
        background: Arc<dyn BackgroundJobs>,
        clock: Arc<dyn Clock>,
        agent_input: &AgentRegistryInput,
    ) -> SessionServices {
        let events = Arc::new(EventBus::new_shared(
            storage.clone(),
            Some(Arc::new(
                crate::session::event_definitions::SessionManifest::new(),
            )),
        ));
        register_projectors(&events);
        let sessions = SessionStore::new(
            events.clone(),
            storage.clone(),
            background.clone(),
            clock.clone(),
        );
        let messages = MessageStore::new(storage.clone());
        let status = Arc::new(SessionStatusService::new(events.clone()));
        let run_state = SessionRunState::new(background, status.clone());
        let permission = Arc::new(PermissionService::new(events.clone()));
        let question = Arc::new(QuestionService::new(events.clone()));
        SessionServices {
            storage,
            events,
            sessions,
            messages,
            status,
            run_state,
            agents: AgentRegistry::new(agent_input),
            permission,
            question,
            clock,
            location: Arc::new(Mutex::new(None)),
        }
    }

    /// Stamp the instance context (`InstanceStore.boot`,
    /// `project/instance-store.ts:45-61`) and inject it as the bus's
    /// ambient publish location (`event-v2-bridge.ts:19-33`).
    pub fn set_instance_location(&self, location: InstanceLocation) {
        self.events.set_ambient_location(LocationRef {
            directory: location.directory.to_string_lossy().into_owned(),
            workspace_id: location.workspace_id.clone(),
            project: Some(LocationProject {
                id: location.project.id.clone(),
                directory: location.worktree.to_string_lossy().into_owned(),
            }),
        });
        *self
            .location
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = Some(location);
    }

    /// The instance context, resolving (and caching) it on first access for
    /// services constructed without one (`InstanceStore.boot`).
    pub fn instance(&self, directory: &Path) -> Result<InstanceLocation, SessionError> {
        {
            let cached = self
                .location
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            if let Some(location) = cached.as_ref() {
                // One instance per directory — a directory change means a
                // different instance booted on these services; re-resolve.
                if location.directory == directory {
                    return Ok(location.clone());
                }
            }
        }
        let registry = ProjectRegistry::new(
            self.storage.clone(),
            Arc::new(SubprocessGit),
            self.clock.clone(),
            Arc::new(|_| {}),
        );
        let (project, worktree) = registry.from_directory(directory).map_err(instance_error)?;
        let location = InstanceLocation {
            directory: directory.to_path_buf(),
            worktree,
            project,
            workspace_id: None,
        };
        self.set_instance_location(location.clone());
        Ok(location)
    }

    /// The instance location once resolved — `None` before the first
    /// [`SessionServices::instance`] call.
    pub fn instance_location(&self) -> Option<InstanceLocation> {
        self.location
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clone()
    }

    /// `InstanceState.context` — the `SessionContext` for per-instance
    /// handlers.
    pub fn instance_context(
        &self,
        directory: &Path,
        workspace_id: Option<String>,
    ) -> Result<SessionContext, SessionError> {
        let location = self.instance(directory)?;
        Ok(SessionContext {
            project_id: location.project.id,
            directory: location.directory,
            worktree: location.worktree,
            workspace_id,
        })
    }
}

fn instance_error(err: RegistryError) -> SessionError {
    match err {
        RegistryError::Core(err) => err.into(),
        other => crate::CoreError::Storage(other.to_string()).into(),
    }
}
