//! Workspace control-plane service — port of
//! `packages/opencode/src/control-plane/workspace.ts` (+ `adapters/index.ts`,
//! `adapters/worktree.ts`, `workspace-adapter-runtime.ts`).
//!
//! Seams (spec §7.4): the remote sync runtime (`startSync`, SSE loops,
//! connection tracking) is forked in TS; the Rust port ships a local-only
//! surface — `status` always reports no live connections and
//! `start_workspace_syncing` is a no-op. The builtin adapter registry has
//! exactly the `worktree` adapter; the plugin runtime is out of scope, so
//! `sync_list` sees only worktrees.

use std::path::PathBuf;
use std::sync::Arc;

use serde_json::{json, Value};

use opencode_core::storage::Workspace as WorkspaceRow;
use opencode_core::worktree::{Context, Deps, Worktree};

use crate::error::ServerError;

/// `flags.experimentalWorkspaces` (`runtime-flags.ts:50`).
pub fn experimental_workspaces_enabled() -> bool {
    crate::engine::experimental_env("OPENCODE_EXPERIMENTAL_WORKSPACES")
}

fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or_default()
}

fn defect(err: impl std::fmt::Display) -> ServerError {
    ServerError::Core(opencode_core::CoreError::Storage(err.to_string()))
}

/// `Workspace.Info` (`control-plane/workspace.ts:30-43`) — `timeUsed` on
/// top of `types.ts` `WorkspaceInfo`; `branch`/`directory`/`extra` are
/// null-or-value.
fn info_to_json(row: &WorkspaceRow) -> Value {
    let extra: Value = row
        .extra
        .as_deref()
        .and_then(|extra| serde_json::from_str(extra).ok())
        .unwrap_or(Value::Null);
    json!({
        "id": row.id,
        "type": row.r#type,
        "name": row.name,
        "branch": row.branch,
        "directory": row.directory,
        "extra": extra,
        "projectID": row.project_id,
        "timeUsed": row.time_used,
    })
}

/// `WorkspaceNotFoundError` (`workspace.ts:83-88`).
#[derive(Debug, thiserror::Error)]
#[error("{message}")]
pub struct WorkspaceNotFound {
    pub message: String,
}

/// Errors of `create` — the adapter/runtime failures surfaced through the
/// die-reason extraction (`handlers/workspace.ts:25-42`).
#[derive(Debug, thiserror::Error)]
#[error("{0}")]
pub struct CreateError(pub String);

/// `SessionWarpError` union (`workspace.ts:126-128`).
#[derive(Debug, thiserror::Error)]
pub enum WarpError {
    #[error("{0}")]
    NotFound(WorkspaceNotFound),
    #[error("{0}")]
    Other(String),
}

/// `CreateInput` minus `projectID` (`groups/workspace.ts:15-16`).
#[derive(Debug, Clone, Default)]
pub struct CreateInput {
    pub id: Option<String>,
    pub r#type: String,
    pub branch: Option<String>,
    pub extra: Option<Value>,
}

/// `WarpPayload` (`groups/workspace.ts:17-20`).
#[derive(Debug, Clone)]
pub struct WarpInput {
    pub workspace_id: Option<String>,
    pub session_id: String,
    pub copy_changes: Option<bool>,
}

/// The workspace service (`workspace.ts:148-910`). Stateless functions
/// over the per-instance services plus the worktree adapter deps.
#[derive(Clone)]
pub struct WorkspaceService {
    pub directory: PathBuf,
    pub services: Arc<opencode_core::SessionServices>,
    pub worktree: Worktree,
    pub deps: Arc<dyn Deps>,
}

impl WorkspaceService {
    /// `adapters` — `listAdapters(project.id)` (`adapters/index.ts:23-27`):
    /// the builtin registry (the plugin runtime is out of scope).
    pub fn adapters(&self) -> Vec<Value> {
        vec![json!({
            "type": "worktree",
            "name": "Worktree",
            "description": "Create a git worktree",
        })]
    }

    /// The `InstanceState.context` bits `Worktree.Service` reads.
    fn worktree_context(&self, workspace_id: Option<&str>) -> Result<Context, ServerError> {
        let instance = self
            .services
            .instance(&self.directory)
            .map_err(|err| defect(format!("{err:?}")))?;
        Ok(Context {
            project_id: instance.project.id.clone(),
            project_worktree: PathBuf::from(&instance.project.worktree),
            worktree: instance.worktree.clone(),
            workspace_id: workspace_id.map(str::to_string),
            is_git: instance.project.vcs == Some(opencode_schema::project::ProjectVcs::Git),
        })
    }

    /// `list` (`workspace.ts:716-725`) — gated by the runtime flag, sorted
    /// by `id.localeCompare`.
    pub fn list(&self) -> Result<Vec<Value>, ServerError> {
        if !experimental_workspaces_enabled() {
            return Ok(Vec::new());
        }
        let instance = self
            .services
            .instance(&self.directory)
            .map_err(|err| defect(err.to_string()))?;
        let mut rows = self
            .services
            .storage
            .list_workspaces()
            .map_err(defect)?
            .into_iter()
            .filter(|row| row.project_id == instance.project.id)
            .collect::<Vec<_>>();
        rows.sort_by(|a, b| a.id.cmp(&b.id));
        Ok(rows.iter().map(info_to_json).collect())
    }

    /// `get` (`workspace.ts:793-796`).
    fn get(&self, id: &str) -> Result<Option<WorkspaceRow>, ServerError> {
        self.services.storage.get_workspace(id).map_err(defect)
    }

    /// `create` (`workspace.ts:492-557`) — the worktree adapter path. The
    /// sync-runtime race (`waitEvent` + `startSync`) is a forked TS
    /// continuation; the Rust port creates the worktree and inserts the
    /// row.
    pub fn create(&self, input: &CreateInput) -> Result<Value, CreateError> {
        if input.r#type != "worktree" {
            return Err(CreateError(format!(
                "Unknown workspace adapter: {}",
                input.r#type
            )));
        }
        let id = match &input.id {
            Some(id) => {
                // `WorkspaceV2.ID.ascending(id)` — the `wrk` prefix check.
                if !id.starts_with("wrk") {
                    return Err(CreateError(format!("ID {id} does not start with wrk")));
                }
                id.clone()
            }
            None => opencode_core::session::ids::generate_id("wrk_"),
        };
        let context = self
            .worktree_context(None)
            .map_err(|err| CreateError(format!("{err:?}")))?;
        // `WorktreeAdapter.configure` — `makeWorktreeInfo({ detached: true })`.
        let info = self
            .worktree
            .make_worktree_info(self.deps.as_ref() as &dyn Deps, &context, None, true)
            .map_err(|err| CreateError(err.message))?;
        let row = WorkspaceRow {
            id,
            r#type: "worktree".to_string(),
            name: info.name.clone(),
            branch: info.branch.clone(),
            directory: Some(info.directory.clone()),
            extra: None,
            project_id: context.project_id.clone(),
            time_used: now_ms(),
        };
        self.services
            .storage
            .put_workspace(&row)
            .map_err(|err| CreateError(err.to_string()))?;
        // `WorktreeAdapter.create` — `createFromInfo` (the boot
        // continuation runs inline).
        self.worktree
            .create_from_info(self.deps.as_ref() as &dyn Deps, &context, &info, None)
            .map_err(|err| CreateError(err.message))?;
        Ok(info_to_json(&row))
    }

    /// `syncList` (`workspace.ts:728-791`) — discover adapter workspaces not
    /// yet in the workspace table. The route responds 204 either way.
    pub fn sync_list(&self) -> Result<(), ServerError> {
        let names = self
            .list()?
            .iter()
            .filter_map(|workspace| workspace["name"].as_str().map(str::to_string))
            .collect::<Vec<_>>();
        let context = self.worktree_context(None)?;
        let discovered = self
            .worktree
            .list(&context)
            .map_err(|err| defect(err.message))?;
        for info in discovered {
            if names.iter().any(|name| name == &info.name) {
                continue;
            }
            let row = WorkspaceRow {
                id: opencode_core::session::ids::generate_id("wrk_"),
                r#type: "worktree".to_string(),
                name: info.name.clone(),
                branch: info.branch,
                directory: Some(info.directory.clone()),
                extra: None,
                project_id: context.project_id.clone(),
                time_used: now_ms(),
            };
            self.services.storage.put_workspace(&row).map_err(defect)?;
        }
        Ok(())
    }

    /// `status` (`workspace.ts:801-803`) — live connection statuses; the
    /// sync runtime is forked, so no workspace is ever live-connected.
    pub fn status(&self) -> Vec<Value> {
        Vec::new()
    }

    /// `remove` (`workspace.ts:798-815`) — remove the workspace's sessions,
    /// stop syncing (forked), run the adapter remove, delete the row.
    pub fn remove(&self, id: &str) -> Result<Option<Value>, ServerError> {
        let session_ids: Vec<String> = self
            .services
            .storage
            .with_connection(|conn| {
                let mut stmt = conn.prepare("SELECT id FROM session WHERE workspace_id = ?1")?;
                let mut rows = stmt.query(rusqlite::params![id])?;
                let mut out = Vec::new();
                while let Some(row) = rows.next()? {
                    out.push(row.get::<_, String>(0)?);
                }
                Ok::<_, opencode_core::CoreError>(out)
            })
            .map_err(defect)?;
        for session_id in session_ids {
            let _ = self.services.sessions.remove(&session_id);
        }
        let Some(row) = self.get(id)? else {
            return Ok(None);
        };
        // `WorktreeAdapter.remove` — best-effort like the TS catchCause.
        if row.r#type == "worktree" {
            if let Some(directory) = row.directory.as_deref() {
                if let Ok(context) = self.worktree_context(Some(&row.id)) {
                    let _ =
                        self.worktree
                            .remove(self.deps.as_ref() as &dyn Deps, &context, directory);
                }
            }
        }
        self.services
            .storage
            .with_connection(|conn| {
                conn.execute("DELETE FROM workspace WHERE id = ?1", rusqlite::params![id])
            })
            .map_err(defect)?;
        Ok(Some(info_to_json(&row)))
    }

    /// `sessionWarp` (`workspace.ts:559-716`) — the local-adapter path. The
    /// remote-URL branch (history replay batches, `/sync/steal` over HTTP,
    /// `copyChanges` patch flows) needs the sync runtime and is unreachable
    /// with the builtin adapters.
    pub fn session_warp(&self, input: &WarpInput) -> Result<(), WarpError> {
        let current: Option<String> = self
            .services
            .storage
            .with_connection(|conn| {
                let mut stmt = conn.prepare("SELECT workspace_id FROM session WHERE id = ?1")?;
                let mut rows = stmt.query(rusqlite::params![input.session_id])?;
                Ok::<_, opencode_core::CoreError>(match rows.next()? {
                    Some(row) => row.get::<_, Option<String>>(0)?,
                    None => None,
                })
            })
            .map_err(|err| WarpError::Other(err.to_string()))?;
        if current.is_some() && input.workspace_id.is_some() {
            // The builtin adapters are local: the TS branch is
            // `prompt.cancel` plus a sync-runtime claim, both forked.
        }
        let Some(workspace_id) = input.workspace_id.as_deref() else {
            self.services
                .sessions
                .set_workspace(&input.session_id, None)
                .map_err(|err| WarpError::Other(err.to_string()))?;
            return Ok(());
        };
        let Some(_space) = self
            .get(workspace_id)
            .map_err(|err| WarpError::Other(format!("{err:?}")))?
        else {
            return Err(WarpError::NotFound(WorkspaceNotFound {
                message: format!("Workspace not found: {workspace_id}"),
            }));
        };
        self.services
            .sessions
            .set_workspace(&input.session_id, Some(workspace_id.to_string()))
            .map_err(|err| WarpError::Other(err.to_string()))?;
        Ok(())
    }
}
