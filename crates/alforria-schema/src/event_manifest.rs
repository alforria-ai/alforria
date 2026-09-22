//! The big manifest unions — openapi `Event` (legacy `/event` stream, 89
//! types) and `V2Event` (`/api/event` stream, 88 types).
//!
//! Variant order follows the openapi union order. Wire type strings are
//! dotted lowercase and always written explicitly with `#[serde(rename)]` —
//! never derived.

use serde::{Deserialize, Serialize};

use crate::catalog::CatalogUpdatedData;
use crate::filesystem::FileEditedData;
use crate::filesystem_watcher::FileWatcherUpdatedData;
use crate::installation_event::{InstallationUpdateAvailableData, InstallationUpdatedData};
use crate::integration::{IntegrationConnectionUpdatedData, IntegrationUpdatedData};
use crate::legacy_event::CommandExecutedData;
use crate::lsp_event::LspUpdatedData;
use crate::mcp_event::{McpBrowserOpenFailedData, McpToolsChangedData};
use crate::models_dev::ModelsDevRefreshedData;
use crate::permission::{PermissionV2AskedData, PermissionV2RepliedData};
use crate::permission_v1::{PermissionAskedData, PermissionRepliedData};
use crate::plugin::PluginAddedData;
use crate::project::{ProjectDirectoriesUpdatedData, ProjectUpdatedData};
use crate::pty::{PtyCreatedData, PtyDeletedData, PtyExitedData, PtyUpdatedData};
use crate::question::{QuestionV2AskedData, QuestionV2RejectedData, QuestionV2RepliedData};
use crate::question_v1::{QuestionAskedData, QuestionRejectedData, QuestionRepliedData};
use crate::reference::ReferenceUpdatedData;
use crate::server_event::{GlobalDisposedData, ServerConnectedData, ServerInstanceDisposedData};
use crate::session_compaction_event::SessionCompactedData;
use crate::session_event;
use crate::session_status::{SessionIdleData, SessionStatusData};
use crate::session_todo::TodoUpdatedData;
use crate::session_v1::{
    MessagePartDeltaData, MessagePartRemovedData, MessagePartUpdatedData, MessageRemovedData,
    MessageUpdatedData, SessionCreatedData, SessionDeletedData, SessionDiffData, SessionErrorData,
    SessionUpdatedData,
};
use crate::tui_event::{
    TuiCommandExecuteData, TuiPromptAppendData, TuiSessionSelectData, TuiToastShowData,
};
use crate::vcs_event::VcsBranchUpdatedData;
use crate::workspace::{
    WorkspaceFailedData, WorkspaceReadyData, WorkspaceStatusData, WorktreeFailedData,
    WorktreeReadyData,
};

/// Legacy `/event` stream event (openapi `Event` union — 89 types).
///
/// Wire shape: `{"id": "evt_…", "type": "…", "properties": {…payload…}}`
/// (see [`crate::event::LegacyEnvelope`]).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", content = "properties")]
pub enum Event {
    #[serde(rename = "models-dev.refreshed")]
    ModelsDevRefreshed(ModelsDevRefreshedData),
    #[serde(rename = "integration.updated")]
    IntegrationUpdated(IntegrationUpdatedData),
    #[serde(rename = "integration.connection.updated")]
    IntegrationConnectionUpdated(IntegrationConnectionUpdatedData),
    #[serde(rename = "catalog.updated")]
    CatalogUpdated(CatalogUpdatedData),
    #[serde(rename = "session.created")]
    SessionCreated(SessionCreatedData),
    #[serde(rename = "session.updated")]
    SessionUpdated(SessionUpdatedData),
    #[serde(rename = "session.deleted")]
    SessionDeleted(SessionDeletedData),
    #[serde(rename = "message.updated")]
    MessageUpdated(MessageUpdatedData),
    #[serde(rename = "message.removed")]
    MessageRemoved(MessageRemovedData),
    #[serde(rename = "message.part.updated")]
    MessagePartUpdated(MessagePartUpdatedData),
    #[serde(rename = "message.part.removed")]
    MessagePartRemoved(MessagePartRemovedData),
    #[serde(rename = "session.next.agent.switched")]
    SessionNextAgentSwitched(session_event::AgentSwitched),
    #[serde(rename = "session.next.model.switched")]
    SessionNextModelSwitched(session_event::ModelSwitched),
    #[serde(rename = "session.next.moved")]
    SessionNextMoved(session_event::Moved),
    #[serde(rename = "session.next.prompted")]
    SessionNextPrompted(session_event::Prompted),
    #[serde(rename = "session.next.prompt.admitted")]
    SessionNextPromptAdmitted(session_event::PromptAdmitted),
    #[serde(rename = "session.next.context.updated")]
    SessionNextContextUpdated(session_event::ContextUpdated),
    #[serde(rename = "session.next.synthetic")]
    SessionNextSynthetic(session_event::Synthetic),
    #[serde(rename = "session.next.shell.started")]
    SessionNextShellStarted(session_event::ShellStarted),
    #[serde(rename = "session.next.shell.ended")]
    SessionNextShellEnded(session_event::ShellEnded),
    #[serde(rename = "session.next.step.started")]
    SessionNextStepStarted(session_event::StepStarted),
    #[serde(rename = "session.next.step.ended")]
    SessionNextStepEnded(session_event::StepEnded),
    #[serde(rename = "session.next.step.failed")]
    SessionNextStepFailed(session_event::StepFailed),
    #[serde(rename = "session.next.text.started")]
    SessionNextTextStarted(session_event::TextStarted),
    #[serde(rename = "session.next.text.delta")]
    SessionNextTextDelta(session_event::TextDelta),
    #[serde(rename = "session.next.text.ended")]
    SessionNextTextEnded(session_event::TextEnded),
    #[serde(rename = "session.next.reasoning.started")]
    SessionNextReasoningStarted(session_event::ReasoningStarted),
    #[serde(rename = "session.next.reasoning.delta")]
    SessionNextReasoningDelta(session_event::ReasoningDelta),
    #[serde(rename = "session.next.reasoning.ended")]
    SessionNextReasoningEnded(session_event::ReasoningEnded),
    #[serde(rename = "session.next.tool.input.started")]
    SessionNextToolInputStarted(session_event::ToolInputStarted),
    #[serde(rename = "session.next.tool.input.delta")]
    SessionNextToolInputDelta(session_event::ToolInputDelta),
    #[serde(rename = "session.next.tool.input.ended")]
    SessionNextToolInputEnded(session_event::ToolInputEnded),
    #[serde(rename = "session.next.tool.called")]
    SessionNextToolCalled(session_event::ToolCalled),
    #[serde(rename = "session.next.tool.progress")]
    SessionNextToolProgress(session_event::ToolProgress),
    #[serde(rename = "session.next.tool.success")]
    SessionNextToolSuccess(session_event::ToolSuccess),
    #[serde(rename = "session.next.tool.failed")]
    SessionNextToolFailed(session_event::ToolFailed),
    #[serde(rename = "session.next.retried")]
    SessionNextRetried(session_event::Retried),
    #[serde(rename = "session.next.compaction.started")]
    SessionNextCompactionStarted(session_event::CompactionStarted),
    #[serde(rename = "session.next.compaction.delta")]
    SessionNextCompactionDelta(session_event::CompactionDelta),
    #[serde(rename = "session.next.compaction.ended")]
    SessionNextCompactionEnded(session_event::CompactionEnded),
    #[serde(rename = "session.next.revert.staged")]
    SessionNextRevertStaged(session_event::RevertStaged),
    #[serde(rename = "session.next.revert.cleared")]
    SessionNextRevertCleared(session_event::RevertCleared),
    #[serde(rename = "session.next.revert.committed")]
    SessionNextRevertCommitted(session_event::RevertCommitted),
    #[serde(rename = "message.part.delta")]
    MessagePartDelta(MessagePartDeltaData),
    #[serde(rename = "session.diff")]
    SessionDiff(SessionDiffData),
    #[serde(rename = "session.error")]
    SessionError(SessionErrorData),
    #[serde(rename = "installation.updated")]
    InstallationUpdated(InstallationUpdatedData),
    #[serde(rename = "installation.update-available")]
    InstallationUpdateAvailable(InstallationUpdateAvailableData),
    #[serde(rename = "file.edited")]
    FileEdited(FileEditedData),
    #[serde(rename = "reference.updated")]
    ReferenceUpdated(ReferenceUpdatedData),
    #[serde(rename = "permission.v2.asked")]
    PermissionV2Asked(PermissionV2AskedData),
    #[serde(rename = "permission.v2.replied")]
    PermissionV2Replied(PermissionV2RepliedData),
    #[serde(rename = "plugin.added")]
    PluginAdded(PluginAddedData),
    #[serde(rename = "project.directories.updated")]
    ProjectDirectoriesUpdated(ProjectDirectoriesUpdatedData),
    #[serde(rename = "file.watcher.updated")]
    FileWatcherUpdated(FileWatcherUpdatedData),
    #[serde(rename = "pty.created")]
    PtyCreated(PtyCreatedData),
    #[serde(rename = "pty.updated")]
    PtyUpdated(PtyUpdatedData),
    #[serde(rename = "pty.exited")]
    PtyExited(PtyExitedData),
    #[serde(rename = "pty.deleted")]
    PtyDeleted(PtyDeletedData),
    #[serde(rename = "question.v2.asked")]
    QuestionV2Asked(QuestionV2AskedData),
    #[serde(rename = "question.v2.replied")]
    QuestionV2Replied(QuestionV2RepliedData),
    #[serde(rename = "question.v2.rejected")]
    QuestionV2Rejected(QuestionV2RejectedData),
    #[serde(rename = "todo.updated")]
    TodoUpdated(TodoUpdatedData),
    #[serde(rename = "lsp.updated")]
    LspUpdated(LspUpdatedData),
    #[serde(rename = "permission.asked")]
    PermissionAsked(PermissionAskedData),
    #[serde(rename = "permission.replied")]
    PermissionReplied(PermissionRepliedData),
    #[serde(rename = "tui.prompt.append")]
    TuiPromptAppend(TuiPromptAppendData),
    #[serde(rename = "tui.command.execute")]
    TuiCommandExecute(TuiCommandExecuteData),
    #[serde(rename = "tui.toast.show")]
    TuiToastShow(TuiToastShowData),
    #[serde(rename = "tui.session.select")]
    TuiSessionSelect(TuiSessionSelectData),
    #[serde(rename = "mcp.tools.changed")]
    McpToolsChanged(McpToolsChangedData),
    #[serde(rename = "mcp.browser.open.failed")]
    McpBrowserOpenFailed(McpBrowserOpenFailedData),
    #[serde(rename = "command.executed")]
    CommandExecuted(CommandExecutedData),
    #[serde(rename = "project.updated")]
    ProjectUpdated(ProjectUpdatedData),
    #[serde(rename = "session.status")]
    SessionStatus(SessionStatusData),
    #[serde(rename = "session.idle")]
    SessionIdle(SessionIdleData),
    #[serde(rename = "question.asked")]
    QuestionAsked(QuestionAskedData),
    #[serde(rename = "question.replied")]
    QuestionReplied(QuestionRepliedData),
    #[serde(rename = "question.rejected")]
    QuestionRejected(QuestionRejectedData),
    #[serde(rename = "session.compacted")]
    SessionCompacted(SessionCompactedData),
    #[serde(rename = "vcs.branch.updated")]
    VcsBranchUpdated(VcsBranchUpdatedData),
    #[serde(rename = "workspace.ready")]
    WorkspaceReady(WorkspaceReadyData),
    #[serde(rename = "workspace.failed")]
    WorkspaceFailed(WorkspaceFailedData),
    #[serde(rename = "workspace.status")]
    WorkspaceStatus(WorkspaceStatusData),
    #[serde(rename = "worktree.ready")]
    WorktreeReady(WorktreeReadyData),
    #[serde(rename = "worktree.failed")]
    WorktreeFailed(WorktreeFailedData),
    #[serde(rename = "server.connected")]
    ServerConnected(ServerConnectedData),
    #[serde(rename = "global.disposed")]
    GlobalDisposed(GlobalDisposedData),
    /// Server-injected; legacy union only. openapi
    /// `EventServerInstanceDisposed.properties` requires `directory`.
    #[serde(rename = "server.instance.disposed")]
    ServerInstanceDisposed(ServerInstanceDisposedData),
}

/// V2 `/api/event` stream event (openapi `V2Event` union — 88 types): the
/// same set as [`Event`] minus `server.instance.disposed`.
///
/// Wire shape: `{"id": "evt_…", "metadata"?: …, "type": "…", "durable"?: …,
/// "location"?: …, "data": {…payload…}}` (see [`crate::event::V2Envelope`]).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", content = "data")]
pub enum V2Event {
    #[serde(rename = "models-dev.refreshed")]
    ModelsDevRefreshed(ModelsDevRefreshedData),
    #[serde(rename = "integration.updated")]
    IntegrationUpdated(IntegrationUpdatedData),
    #[serde(rename = "integration.connection.updated")]
    IntegrationConnectionUpdated(IntegrationConnectionUpdatedData),
    #[serde(rename = "catalog.updated")]
    CatalogUpdated(CatalogUpdatedData),
    #[serde(rename = "session.created")]
    SessionCreated(SessionCreatedData),
    #[serde(rename = "session.updated")]
    SessionUpdated(SessionUpdatedData),
    #[serde(rename = "session.deleted")]
    SessionDeleted(SessionDeletedData),
    #[serde(rename = "message.updated")]
    MessageUpdated(MessageUpdatedData),
    #[serde(rename = "message.removed")]
    MessageRemoved(MessageRemovedData),
    #[serde(rename = "message.part.updated")]
    MessagePartUpdated(MessagePartUpdatedData),
    #[serde(rename = "message.part.removed")]
    MessagePartRemoved(MessagePartRemovedData),
    #[serde(rename = "session.next.agent.switched")]
    SessionNextAgentSwitched(session_event::AgentSwitched),
    #[serde(rename = "session.next.model.switched")]
    SessionNextModelSwitched(session_event::ModelSwitched),
    #[serde(rename = "session.next.moved")]
    SessionNextMoved(session_event::Moved),
    #[serde(rename = "session.next.prompted")]
    SessionNextPrompted(session_event::Prompted),
    #[serde(rename = "session.next.prompt.admitted")]
    SessionNextPromptAdmitted(session_event::PromptAdmitted),
    #[serde(rename = "session.next.context.updated")]
    SessionNextContextUpdated(session_event::ContextUpdated),
    #[serde(rename = "session.next.synthetic")]
    SessionNextSynthetic(session_event::Synthetic),
    #[serde(rename = "session.next.shell.started")]
    SessionNextShellStarted(session_event::ShellStarted),
    #[serde(rename = "session.next.shell.ended")]
    SessionNextShellEnded(session_event::ShellEnded),
    #[serde(rename = "session.next.step.started")]
    SessionNextStepStarted(session_event::StepStarted),
    #[serde(rename = "session.next.step.ended")]
    SessionNextStepEnded(session_event::StepEnded),
    #[serde(rename = "session.next.step.failed")]
    SessionNextStepFailed(session_event::StepFailed),
    #[serde(rename = "session.next.text.started")]
    SessionNextTextStarted(session_event::TextStarted),
    #[serde(rename = "session.next.text.delta")]
    SessionNextTextDelta(session_event::TextDelta),
    #[serde(rename = "session.next.text.ended")]
    SessionNextTextEnded(session_event::TextEnded),
    #[serde(rename = "session.next.reasoning.started")]
    SessionNextReasoningStarted(session_event::ReasoningStarted),
    #[serde(rename = "session.next.reasoning.delta")]
    SessionNextReasoningDelta(session_event::ReasoningDelta),
    #[serde(rename = "session.next.reasoning.ended")]
    SessionNextReasoningEnded(session_event::ReasoningEnded),
    #[serde(rename = "session.next.tool.input.started")]
    SessionNextToolInputStarted(session_event::ToolInputStarted),
    #[serde(rename = "session.next.tool.input.delta")]
    SessionNextToolInputDelta(session_event::ToolInputDelta),
    #[serde(rename = "session.next.tool.input.ended")]
    SessionNextToolInputEnded(session_event::ToolInputEnded),
    #[serde(rename = "session.next.tool.called")]
    SessionNextToolCalled(session_event::ToolCalled),
    #[serde(rename = "session.next.tool.progress")]
    SessionNextToolProgress(session_event::ToolProgress),
    #[serde(rename = "session.next.tool.success")]
    SessionNextToolSuccess(session_event::ToolSuccess),
    #[serde(rename = "session.next.tool.failed")]
    SessionNextToolFailed(session_event::ToolFailed),
    #[serde(rename = "session.next.retried")]
    SessionNextRetried(session_event::Retried),
    #[serde(rename = "session.next.compaction.started")]
    SessionNextCompactionStarted(session_event::CompactionStarted),
    #[serde(rename = "session.next.compaction.delta")]
    SessionNextCompactionDelta(session_event::CompactionDelta),
    #[serde(rename = "session.next.compaction.ended")]
    SessionNextCompactionEnded(session_event::CompactionEnded),
    #[serde(rename = "session.next.revert.staged")]
    SessionNextRevertStaged(session_event::RevertStaged),
    #[serde(rename = "session.next.revert.cleared")]
    SessionNextRevertCleared(session_event::RevertCleared),
    #[serde(rename = "session.next.revert.committed")]
    SessionNextRevertCommitted(session_event::RevertCommitted),
    #[serde(rename = "message.part.delta")]
    MessagePartDelta(MessagePartDeltaData),
    #[serde(rename = "session.diff")]
    SessionDiff(SessionDiffData),
    #[serde(rename = "session.error")]
    SessionError(SessionErrorData),
    #[serde(rename = "installation.updated")]
    InstallationUpdated(InstallationUpdatedData),
    #[serde(rename = "installation.update-available")]
    InstallationUpdateAvailable(InstallationUpdateAvailableData),
    #[serde(rename = "file.edited")]
    FileEdited(FileEditedData),
    #[serde(rename = "reference.updated")]
    ReferenceUpdated(ReferenceUpdatedData),
    #[serde(rename = "permission.v2.asked")]
    PermissionV2Asked(PermissionV2AskedData),
    #[serde(rename = "permission.v2.replied")]
    PermissionV2Replied(PermissionV2RepliedData),
    #[serde(rename = "plugin.added")]
    PluginAdded(PluginAddedData),
    #[serde(rename = "project.directories.updated")]
    ProjectDirectoriesUpdated(ProjectDirectoriesUpdatedData),
    #[serde(rename = "file.watcher.updated")]
    FileWatcherUpdated(FileWatcherUpdatedData),
    #[serde(rename = "pty.created")]
    PtyCreated(PtyCreatedData),
    #[serde(rename = "pty.updated")]
    PtyUpdated(PtyUpdatedData),
    #[serde(rename = "pty.exited")]
    PtyExited(PtyExitedData),
    #[serde(rename = "pty.deleted")]
    PtyDeleted(PtyDeletedData),
    #[serde(rename = "question.v2.asked")]
    QuestionV2Asked(QuestionV2AskedData),
    #[serde(rename = "question.v2.replied")]
    QuestionV2Replied(QuestionV2RepliedData),
    #[serde(rename = "question.v2.rejected")]
    QuestionV2Rejected(QuestionV2RejectedData),
    #[serde(rename = "todo.updated")]
    TodoUpdated(TodoUpdatedData),
    #[serde(rename = "lsp.updated")]
    LspUpdated(LspUpdatedData),
    #[serde(rename = "permission.asked")]
    PermissionAsked(PermissionAskedData),
    #[serde(rename = "permission.replied")]
    PermissionReplied(PermissionRepliedData),
    #[serde(rename = "tui.prompt.append")]
    TuiPromptAppend(TuiPromptAppendData),
    #[serde(rename = "tui.command.execute")]
    TuiCommandExecute(TuiCommandExecuteData),
    #[serde(rename = "tui.toast.show")]
    TuiToastShow(TuiToastShowData),
    #[serde(rename = "tui.session.select")]
    TuiSessionSelect(TuiSessionSelectData),
    #[serde(rename = "mcp.tools.changed")]
    McpToolsChanged(McpToolsChangedData),
    #[serde(rename = "mcp.browser.open.failed")]
    McpBrowserOpenFailed(McpBrowserOpenFailedData),
    #[serde(rename = "command.executed")]
    CommandExecuted(CommandExecutedData),
    #[serde(rename = "project.updated")]
    ProjectUpdated(ProjectUpdatedData),
    #[serde(rename = "session.status")]
    SessionStatus(SessionStatusData),
    #[serde(rename = "session.idle")]
    SessionIdle(SessionIdleData),
    #[serde(rename = "question.asked")]
    QuestionAsked(QuestionAskedData),
    #[serde(rename = "question.replied")]
    QuestionReplied(QuestionRepliedData),
    #[serde(rename = "question.rejected")]
    QuestionRejected(QuestionRejectedData),
    #[serde(rename = "session.compacted")]
    SessionCompacted(SessionCompactedData),
    #[serde(rename = "vcs.branch.updated")]
    VcsBranchUpdated(VcsBranchUpdatedData),
    #[serde(rename = "workspace.ready")]
    WorkspaceReady(WorkspaceReadyData),
    #[serde(rename = "workspace.failed")]
    WorkspaceFailed(WorkspaceFailedData),
    #[serde(rename = "workspace.status")]
    WorkspaceStatus(WorkspaceStatusData),
    #[serde(rename = "worktree.ready")]
    WorktreeReady(WorktreeReadyData),
    #[serde(rename = "worktree.failed")]
    WorktreeFailed(WorktreeFailedData),
    #[serde(rename = "server.connected")]
    ServerConnected(ServerConnectedData),
    #[serde(rename = "global.disposed")]
    GlobalDisposed(GlobalDisposedData),
}

/// Wire-only event type emitted by the server every 10 s
/// (`{"id": …, "type": "server.heartbeat", "properties": {}}`) but never
/// part of any TS schema manifest — hence not a variant of [`Event`] or
/// [`V2Event`].
pub const SERVER_HEARTBEAT_TYPE: &str = "server.heartbeat";

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;
    use crate::event::{GlobalEnvelope, LegacyEnvelope, V2Envelope};

    #[test]
    fn legacy_envelope_round_trips() {
        let wire = json!({
            "id": "evt_1",
            "type": "todo.updated",
            "properties": {
                "sessionID": "ses_1",
                "todos": [
                    { "content": "write tests", "status": "pending", "priority": "high" }
                ]
            },
        });
        let envelope: LegacyEnvelope<Event> = serde_json::from_value(wire.clone()).unwrap();
        match &envelope.event {
            Event::TodoUpdated(data) => {
                assert_eq!(data.session_id, "ses_1");
                assert_eq!(data.todos.len(), 1);
            }
            _ => panic!("expected TodoUpdated"),
        }
        let back = serde_json::to_value(&envelope).unwrap();
        assert_eq!(back, wire);
    }

    #[test]
    fn v2_envelope_round_trips_with_metadata_durable_location() {
        let wire = json!({
            "id": "evt_01JDY",
            "metadata": { "origin": "test" },
            "durable": { "aggregateID": "ses_01JDY", "seq": 12, "version": 1 },
            "location": { "directory": "/repo", "workspaceID": "wrk_1" },
            "type": "session.next.text.started",
            "data": {
                "timestamp": 1778031210000i64,
                "sessionID": "ses_01JDY",
                "assistantMessageID": "msg_01JDY",
                "textID": "txt_1"
            }
        });
        let envelope: V2Envelope<V2Event> = serde_json::from_value(wire.clone()).unwrap();
        assert_eq!(
            envelope.metadata.as_ref().unwrap().get("origin"),
            Some(&json!("test"))
        );
        assert_eq!(envelope.durable.as_ref().unwrap().seq, 12);
        assert_eq!(
            envelope.location.as_ref().unwrap().workspace_id.as_deref(),
            Some("wrk_1")
        );
        match &envelope.event {
            V2Event::SessionNextTextStarted(data) => {
                assert_eq!(data.text_id, "txt_1");
            }
            _ => panic!("expected SessionNextTextStarted"),
        }
        let back = serde_json::to_value(&envelope).unwrap();
        assert_eq!(back, wire);
    }

    #[test]
    fn v2_envelope_omits_optional_keys() {
        let wire = json!({
            "id": "evt_2",
            "type": "session.next.text.started",
            "data": {
                "timestamp": 1778031210000i64,
                "sessionID": "ses_01JDY",
                "assistantMessageID": "msg_01JDY",
                "textID": "txt_1"
            }
        });
        let envelope: V2Envelope<V2Event> = serde_json::from_value(wire).unwrap();
        assert!(envelope.metadata.is_none());
        assert!(envelope.durable.is_none());
        assert!(envelope.location.is_none());
        let back = serde_json::to_value(&envelope).unwrap();
        assert!(back.get("metadata").is_none());
        assert!(back.get("durable").is_none());
        assert!(back.get("location").is_none());
    }

    #[test]
    fn server_instance_disposed_is_legacy_only() {
        let wire = json!({
            "id": "evt_3",
            "type": "server.instance.disposed",
            "properties": { "directory": "/repo" }
        });
        let legacy: LegacyEnvelope<Event> = serde_json::from_value(wire).unwrap();
        match legacy.event {
            Event::ServerInstanceDisposed(data) => {
                assert_eq!(data.directory, "/repo");
            }
            _ => panic!("expected ServerInstanceDisposed"),
        }
        // The v2 union must reject the type string…
        let v2_wire = json!({
            "id": "evt_3",
            "type": "server.instance.disposed",
            "data": { "directory": "/repo" }
        });
        assert!(serde_json::from_value::<V2Event>(v2_wire).is_err());
    }

    #[test]
    fn heartbeat_is_not_in_unions() {
        let legacy_wire = json!({
            "id": "evt_4",
            "type": "server.heartbeat",
            "properties": {}
        });
        assert!(
            serde_json::from_value::<LegacyEnvelope<Event>>(legacy_wire).is_err(),
            "server.heartbeat is wire-only and never part of the Event union"
        );
        let v2_wire = json!({
            "id": "evt_4",
            "type": "server.heartbeat",
            "data": {}
        });
        assert!(serde_json::from_value::<V2Event>(v2_wire).is_err());
        assert_eq!(SERVER_HEARTBEAT_TYPE, "server.heartbeat");
    }

    #[test]
    fn global_envelope_round_trips() {
        let wire = json!({
            "directory": "/repo",
            "payload": {
                "id": "evt_5",
                "type": "workspace.ready",
                "properties": { "name": "wrk-main" }
            }
        });
        let envelope: GlobalEnvelope<Event> = serde_json::from_value(wire.clone()).unwrap();
        assert_eq!(envelope.directory, "/repo");
        assert!(envelope.project.is_none());
        assert!(envelope.workspace.is_none());
        match &envelope.payload.event {
            Event::WorkspaceReady(data) => assert_eq!(data.name, "wrk-main"),
            _ => panic!("expected WorkspaceReady"),
        }
        let back = serde_json::to_value(&envelope).unwrap();
        assert_eq!(back, wire);
    }
}
