//! Session engine (M5) — `SessionServices` is the per-instance bundle
//! (spec §2.1) replacing TS's Effect layers.

pub mod agents;
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
pub mod prompt_input;
pub mod question;
pub mod reminders;
pub mod render;
pub mod retry;
pub mod run_state;
pub mod snapshot;
pub mod status;
pub mod store;
pub mod system;
pub mod tools;
pub mod usage;

use std::sync::Arc;

use crate::event::bus::EventBus;
use crate::storage::Storage;
use crate::Clock;

pub use agents::{AgentInfo, AgentRegistry, AgentRegistryInput, DefaultAgentError};
pub use error::{AuthError, BusyError, NotFoundError, OutputLengthError, SessionError};
pub use message::{filter_compacted, latest, Cursor, Latest, MessagePage, MessageStore, WithParts};
pub use overflow::{is_overflow, max_output_tokens, usable, ModelLimits};
pub use permission::{
    PermissionError, PermissionService, SessionAsk, PERMISSION_ASKED, PERMISSION_REPLIED,
};
pub use question::{
    QuestionError, QuestionService, QUESTION_ASKED, QUESTION_REJECTED, QUESTION_REPLIED,
};
pub use retry::{
    delay as retry_delay, retryable, Policy as RetryPolicy, PolicyStep as RetryPolicyStep,
    RetryAction, RetrySet, Retryable as RetryableError,
};
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
pub use usage::{get_usage, CacheCost, CostTier, GetUsage, ModelCost, Over200k, UsageCost};

/// The per-instance session services (spec §2.1): TS resolves these from
/// Effect layers; Rust passes this bundle explicitly.
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
        let sessions =
            SessionStore::new(events.clone(), storage.clone(), background.clone(), clock);
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
        }
    }
}
