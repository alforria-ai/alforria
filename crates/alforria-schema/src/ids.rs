//! Wire-visible ID aliases. Prefixes are documentation; validation lives in alforria-core.

pub type SessionId = String; // "ses…"
pub type EventId = String; // "evt_…"
pub type MessageId = String; // "msg…" (v1) / "msg_…" (v2)
pub type PartId = String; // "prt…"
pub type PermissionId = String; // "per…"
pub type QuestionId = String; // "que…"
pub type PtyId = String; // "pty_…"
pub type WorkspaceId = String; // "wrk…"
pub type ProjectId = String; // no prefix; constant "global" exists
pub type PermissionSavedId = String; // "psv_…"
pub type CredentialId = String; // "cred_…"
pub type IntegrationId = String;
pub type IntegrationMethodId = String;
pub type IntegrationAttemptId = String; // "con_…"
pub type AgentId = String;
pub type ProviderId = String;
pub type ModelId = String;
pub type ModelVariantId = String;
pub type ModelFamily = String;
pub type PluginId = String;
pub type ProjectCopyStrategyId = String; // non-empty trimmed string
