//! The project registry — port of `packages/opencode/src/project/project.ts`
//! (`Project.Service`).

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use opencode_schema::project::{
    ProjectCommands, ProjectDirectory, ProjectIcon, ProjectInfo, ProjectTime, ProjectVcs,
};
use rusqlite::Connection;

use crate::event::bus::EventBus;
use crate::git::{which_git, GitRunner};
use crate::project::directories::{self, DirectoryBehavior, ProjectDirectoryCreate};
use crate::project::{commit, resolve, GLOBAL_ID};
use crate::storage::schema::project_from_row;
use crate::storage::{schema::Project as ProjectRow, Storage};
use crate::{Clock, CoreError};

/// `GlobalBus.emit("event", …)` seam — `emitUpdated` (`project.ts:133-140`)
/// is a process-wide singleton in TS; Rust injects the sink.
pub type ProjectEventSink = Arc<dyn Fn(&ProjectInfo) + Send + Sync>;

/// `Project.NotFoundError` (`project.ts:75-77`).
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("Project.NotFoundError: {project_id}")]
pub struct NotFoundError {
    pub project_id: String,
}

/// Registry error surface — storage defects, `NotFoundError`, and plain
/// `Error(message)` throws (`Error("Git is not installed")`,
/// `Error("Project not found: …")`).
#[derive(Debug, thiserror::Error)]
pub enum RegistryError {
    #[error(transparent)]
    NotFound(#[from] NotFoundError),
    #[error("{0}")]
    Message(String),
    #[error(transparent)]
    Core(#[from] CoreError),
}

/// A drizzle-style update field: `Unset` (absent) is skipped, `Set(None)`
/// writes NULL.
#[derive(Debug, Clone, PartialEq, Default)]
pub enum Field<T> {
    #[default]
    Unset,
    Set(Option<T>),
}

impl<'de, T: serde::Deserialize<'de>> serde::Deserialize<'de> for Field<T> {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        match Option::<Option<T>>::deserialize(deserializer)? {
            Some(value) => Ok(Field::Set(value)),
            None => Ok(Field::Unset),
        }
    }
}

/// `Project.UpdateInput` (`project.ts:60-65`).
pub struct UpdateInput {
    pub project_id: String,
    pub name: Field<String>,
    pub icon: Field<ProjectIcon>,
    pub commands: Field<ProjectCommands>,
}

/// `Project.Service` (`project.ts:83-100`).
pub struct ProjectRegistry {
    storage: Arc<Storage>,
    git: Arc<dyn GitRunner>,
    clock: Arc<dyn Clock>,
    emit: ProjectEventSink,
    /// `Flag.OPENCODE_FAKE_VCS` decoded through `Schema.optional(Vcs)`.
    fake_vcs: bool,
    /// `flags.experimentalIconDiscovery`.
    icon_discovery: bool,
    /// Retained `/init` command-hook subscriptions (`project.ts:386-396`).
    hooks: Mutex<Vec<Arc<crate::event::Subscription>>>,
}

impl ProjectRegistry {
    /// Production registry — flags from the environment.
    pub fn new(
        storage: Arc<Storage>,
        git: Arc<dyn GitRunner>,
        clock: Arc<dyn Clock>,
        emit: ProjectEventSink,
    ) -> ProjectRegistry {
        ProjectRegistry::with_flags(
            storage,
            git,
            clock,
            emit,
            std::env::var("OPENCODE_FAKE_VCS")
                .ok()
                .map(|value| value == "git")
                .unwrap_or(false),
            truthy_env("OPENCODE_EXPERIMENTAL_ICON_DISCOVERY"),
        )
    }

    /// Test/seam constructor with explicit flag values.
    pub fn with_flags(
        storage: Arc<Storage>,
        git: Arc<dyn GitRunner>,
        clock: Arc<dyn Clock>,
        emit: ProjectEventSink,
        fake_vcs: bool,
        icon_discovery: bool,
    ) -> ProjectRegistry {
        ProjectRegistry {
            storage,
            git,
            clock,
            emit,
            fake_vcs,
            icon_discovery,
            hooks: Mutex::new(Vec::new()),
        }
    }

    fn now(&self) -> u64 {
        self.clock.now_ms()
    }

    /// `emitUpdated` (`project.ts:133-140`): a global
    /// `{directory: "global", project: <id>}` `project.updated` frame.
    fn emit_updated(&self, data: &ProjectInfo) {
        self.emit.as_ref()(data);
    }

    // ------------------------------------------------------------------ rows

    /// `fromRow` (`project.ts:35-58`).
    fn convert_row(&self, row: &ProjectRow) -> Result<ProjectInfo, RegistryError> {
        let vcs = match row.vcs.as_deref() {
            Some("git") => Some(ProjectVcs::Git),
            None => None,
            Some(other) => {
                return Err(RegistryError::Core(CoreError::Storage(format!(
                    "invalid project vcs {other:?}"
                ))))
            }
        };
        let commands = row
            .commands
            .as_deref()
            .map(serde_json::from_str::<ProjectCommands>)
            .transpose()
            .map_err(|err| RegistryError::Core(CoreError::Storage(err.to_string())))?;
        Ok(ProjectInfo {
            id: row.id.clone(),
            worktree: row.worktree.clone(),
            vcs,
            name: row.name.clone(),
            icon: icon_from_columns(&row.icon_url, &row.icon_url_override, &row.icon_color),
            commands,
            time: ProjectTime {
                created: u64::try_from(row.time_created).unwrap_or_default(),
                updated: u64::try_from(row.time_updated).unwrap_or_default(),
                initialized: row.time_initialized.map(|t| u64::try_from(t).unwrap_or(0)),
            },
            sandboxes: sandboxes_from(&row.sandboxes)?,
        })
    }

    /// `list` (`project.ts:336-338`).
    pub fn list(&self) -> Result<Vec<ProjectInfo>, RegistryError> {
        let rows = self
            .storage
            .with_connection(|conn| {
                let mut stmt = conn.prepare("SELECT * FROM project ORDER BY rowid")?;
                let mut rows = stmt.query([])?;
                let mut out = Vec::new();
                while let Some(row) = rows.next()? {
                    out.push(project_from_row(row)?);
                }
                Ok::<_, CoreError>(out)
            })
            .map_err(RegistryError::from)?;
        rows.iter().map(|row| self.convert_row(row)).collect()
    }

    /// `get` (`project.ts:340-343`).
    pub fn get(&self, id: &str) -> Result<Option<ProjectInfo>, RegistryError> {
        match self.row(id)? {
            Some(row) => self.convert_row(&row).map(Some),
            None => Ok(None),
        }
    }

    fn row(&self, id: &str) -> Result<Option<ProjectRow>, RegistryError> {
        self.storage
            .with_connection(|conn| query_project(conn, id))
            .map_err(RegistryError::from)
    }

    // -------------------------------------------------------------- migration

    /// `migrateProjectId` (`project.ts:146-193`).
    fn migrate_project_id(&self, old_id: Option<&str>, new_id: &str) -> Result<(), RegistryError> {
        let Some(old_id) = old_id else {
            return Ok(());
        };
        if old_id == GLOBAL_ID || old_id == new_id {
            return Ok(());
        }
        let now = self.now() as i64;
        self.storage
            .with_connection_mut(|conn| {
                let tx = conn.transaction()?;
                let old_project = query_project(&tx, old_id)?;
                let new_project = query_project(&tx, new_id)?;

                if old_project.is_some() && new_project.is_none() {
                    tx.execute(
                        "INSERT INTO project (id, worktree, vcs, name, icon_url, icon_url_override, icon_color, time_created, time_updated, time_initialized, sandboxes, commands)
                         SELECT ?2, worktree, vcs, name, icon_url, icon_url_override, icon_color, time_created, ?3, time_initialized, sandboxes, commands
                         FROM project WHERE id = ?1",
                        rusqlite::params![old_id, new_id, now],
                    )?;
                }

                // Project directories may be shared across distinct
                // checkouts which have diverged. Clear the directory
                // list and rely on it being re-populated to ensure
                // accuracy (project.ts:170-175).
                tx.execute(
                    "DELETE FROM project_directory WHERE project_id = ?1",
                    rusqlite::params![old_id],
                )?;
                tx.execute(
                    "UPDATE session SET project_id = ?2, time_updated = time_updated
                     WHERE project_id = ?1",
                    rusqlite::params![old_id, new_id],
                )?;
                tx.execute(
                    "UPDATE workspace SET project_id = ?2 WHERE project_id = ?1",
                    rusqlite::params![old_id, new_id],
                )?;

                if old_project.is_some() {
                    tx.execute("DELETE FROM project WHERE id = ?1", rusqlite::params![old_id])?;
                }
                tx.commit()?;
                Ok::<(), CoreError>(())
            })
            .map_err(RegistryError::from)
    }

    // -------------------------------------------------------------- fromDirectory

    /// `fromDirectory` (`project.ts:213-310`) — returns
    /// `{ project, sandbox }`.
    pub fn from_directory(
        &self,
        directory: &Path,
    ) -> Result<(ProjectInfo, PathBuf), RegistryError> {
        let data = resolve(self.git.as_ref(), directory);
        let worktree = if data.id == GLOBAL_ID && data.vcs.is_none() {
            PathBuf::from("/")
        } else {
            data.directory.clone()
        };

        let project_id = data.id.clone();
        self.migrate_project_id(data.previous.as_deref(), &project_id)?;

        let now = self.now();
        let mut result = self.get(&project_id)?.unwrap_or_else(|| ProjectInfo {
            id: project_id.clone(),
            worktree: worktree.to_string_lossy().into_owned(),
            vcs: None,
            name: None,
            icon: None,
            commands: None,
            time: ProjectTime {
                created: now,
                updated: now,
                initialized: None,
            },
            sandboxes: Vec::new(),
        });
        if project_id == GLOBAL_ID {
            result.worktree = worktree.to_string_lossy().into_owned();
        }
        result.vcs = if data.vcs.is_some() || self.fake_vcs {
            Some(ProjectVcs::Git)
        } else {
            None
        };
        result.time.updated = now;

        let directory_str = data.directory.to_string_lossy().into_owned();
        if project_id != GLOBAL_ID
            && directory_str != result.worktree
            && !result.sandboxes.contains(&directory_str)
        {
            result.sandboxes.push(directory_str.clone());
        }
        result
            .sandboxes
            .retain(|sandbox| Path::new(sandbox).exists());

        self.upsert(&result)?;

        if project_id != GLOBAL_ID {
            self.storage
                .with_connection(|conn| {
                    conn.execute(
                        "UPDATE session SET project_id = ?1
                         WHERE project_id = ?2 AND directory = ?3",
                        rusqlite::params![project_id, GLOBAL_ID, directory_str],
                    )?;
                    Ok::<(), CoreError>(())
                })
                .map_err(RegistryError::from)?;
        }

        if project_id != GLOBAL_ID {
            // `saveProjectDirectory` (`project.ts:195-211`) — a failure is
            // a logged warning, never fatal.
            if let Err(err) = directories::create(
                &self.storage,
                &ProjectDirectoryCreate {
                    project_id: &project_id,
                    directory: &directory_str,
                    strategy: None,
                    behavior: DirectoryBehavior::Ignore,
                },
            ) {
                tracing::warn!("project directory persistence failed: {err}");
            }
        }

        self.emit_updated(&result);

        if self.icon_discovery && result.vcs.is_some() {
            // Forked in TS (`project.ts:233`) — discovery never blocks
            // from_directory.
            let registry = self.clone_handle();
            let info = result.clone();
            std::thread::spawn(move || {
                let _ = registry.discover(&info);
            });
        }

        if project_id != GLOBAL_ID && data.vcs.is_some() {
            let store = data.vcs.clone().expect("checked above");
            commit(&store.store, &data.id);
        }

        let sandbox = if data.vcs.is_some() {
            data.directory.clone()
        } else {
            worktree
        };
        Ok((result, sandbox))
    }

    /// A handle sharing the storage/git/clock/emit wiring (the forked icon
    /// discovery runs on its own thread).
    fn clone_handle(&self) -> ProjectRegistry {
        ProjectRegistry {
            storage: Arc::clone(&self.storage),
            git: Arc::clone(&self.git),
            clock: Arc::clone(&self.clock),
            emit: Arc::clone(&self.emit),
            fake_vcs: self.fake_vcs,
            icon_discovery: self.icon_discovery,
            hooks: Mutex::new(Vec::new()),
        }
    }

    /// The `project` upsert (`project.ts:257-289`).
    fn upsert(&self, info: &ProjectInfo) -> Result<(), RegistryError> {
        let sandboxes = serde_json::to_string(&info.sandboxes)
            .map_err(|err| RegistryError::Core(CoreError::Storage(err.to_string())))?;
        let commands = info
            .commands
            .as_ref()
            .map(serde_json::to_string)
            .transpose()
            .map_err(|err| RegistryError::Core(CoreError::Storage(err.to_string())))?;
        self.storage
            .with_connection(|conn| {
                conn.execute(
                    "INSERT INTO project (id, worktree, vcs, name, icon_url, icon_url_override, icon_color, time_created, time_updated, time_initialized, sandboxes, commands)
                     VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12)
                     ON CONFLICT (id) DO UPDATE SET
                       worktree = ?2, vcs = ?3, name = ?4, icon_url = ?5,
                       icon_url_override = ?6, icon_color = ?7, time_updated = ?9,
                       time_initialized = ?10, sandboxes = ?11, commands = ?12",
                    rusqlite::params![
                        info.id,
                        info.worktree,
                        vcs_column(&info.vcs),
                        info.name,
                        info.icon.as_ref().and_then(|icon| icon.url.clone()),
                        info.icon.as_ref().and_then(|icon| icon.r#override.clone()),
                        info.icon.as_ref().and_then(|icon| icon.color.clone()),
                        info.time.created as i64,
                        info.time.updated as i64,
                        info.time.initialized.map(|time| time as i64),
                        sandboxes,
                        commands,
                    ],
                )?;
                Ok::<(), CoreError>(())
            })
            .map_err(RegistryError::from)
    }

    // ------------------------------------------------------------- remaining

    /// `update` (`project.ts:345-364`) — only the provided columns move.
    pub fn update(&self, input: &UpdateInput) -> Result<ProjectInfo, RegistryError> {
        let mut sets: Vec<String> = Vec::new();
        let mut params: Vec<Box<dyn rusqlite::ToSql>> = Vec::new();
        if let Field::Set(value) = &input.name {
            sets.push(format!("name = ?{}", params.len() + 1));
            params.push(Box::new(value.clone()));
        }
        if let Field::Set(icon) = &input.icon {
            sets.push(format!("icon_url = ?{}", params.len() + 1));
            params.push(Box::new(icon.as_ref().and_then(|i| i.url.clone())));
            sets.push(format!("icon_url_override = ?{}", params.len() + 1));
            params.push(Box::new(icon.as_ref().and_then(|i| i.r#override.clone())));
            sets.push(format!("icon_color = ?{}", params.len() + 1));
            params.push(Box::new(icon.as_ref().and_then(|i| i.color.clone())));
        }
        if let Field::Set(commands) = &input.commands {
            sets.push(format!("commands = ?{}", params.len() + 1));
            let commands = commands
                .as_ref()
                .map(serde_json::to_string)
                .transpose()
                .map_err(|err| RegistryError::Core(CoreError::Storage(err.to_string())))?;
            params.push(Box::new(commands));
        }
        sets.push(format!("time_updated = ?{}", params.len() + 1));
        params.push(Box::new(self.now() as i64));

        let sql = format!(
            "UPDATE project SET {} WHERE id = ?{} RETURNING *",
            sets.join(", "),
            params.len() + 1,
        );
        params.push(Box::new(input.project_id.clone()));

        let row = self
            .storage
            .with_connection(|conn| {
                let mut stmt = conn.prepare(&sql)?;
                let mut rows = stmt.query(rusqlite::params_from_iter(
                    params.iter().map(|p| p.as_ref()),
                ))?;
                match rows.next()? {
                    Some(row) => Ok::<_, CoreError>(Some(project_from_row(row)?)),
                    None => Ok(None),
                }
            })
            .map_err(RegistryError::from)?;
        let Some(row) = row else {
            return Err(NotFoundError {
                project_id: input.project_id.clone(),
            }
            .into());
        };
        let data = self.convert_row(&row)?;
        self.emit_updated(&data);
        Ok(data)
    }

    /// `initGit` (`project.ts:366-375`).
    pub fn init_git(
        &self,
        directory: &Path,
        project: &ProjectInfo,
    ) -> Result<ProjectInfo, RegistryError> {
        if project.vcs == Some(ProjectVcs::Git) {
            return Ok(project.clone());
        }
        if !which_git() {
            return Err(RegistryError::Message("Git is not installed".to_string()));
        }
        let result = self.git.run(Some(directory), &["init", "--quiet"]);
        if result.exit_code != 0 {
            let message = if !result.stderr.trim().is_empty() {
                result.stderr.trim()
            } else if !result.text.trim().is_empty() {
                result.text.trim()
            } else {
                "Failed to initialize git repository"
            };
            return Err(RegistryError::Message(message.to_string()));
        }
        Ok(self.from_directory(directory)?.0)
    }

    /// `setInitialized` (`project.ts:377-384`).
    pub fn set_initialized(&self, id: &str) -> Result<(), RegistryError> {
        self.storage
            .with_connection(|conn| {
                conn.execute(
                    "UPDATE project SET time_initialized = ?1 WHERE id = ?2",
                    rusqlite::params![self.now() as i64, id],
                )?;
                Ok::<(), CoreError>(())
            })
            .map_err(RegistryError::from)
    }

    /// `sandboxes` (`project.ts:402-415`) — missing project → empty list;
    /// sandboxes filtered by existence on disk.
    pub fn sandboxes(&self, id: &str) -> Result<Vec<String>, RegistryError> {
        match self.row(id)? {
            Some(row) => {
                let mut sandboxes = sandboxes_from(&row.sandboxes)?;
                sandboxes.retain(|sandbox| Path::new(sandbox).is_dir());
                Ok(sandboxes)
            }
            None => Ok(Vec::new()),
        }
    }

    /// `addSandbox` (`project.ts:417-432`).
    pub fn add_sandbox(&self, id: &str, directory: &str) -> Result<(), RegistryError> {
        self.sandbox_update(id, |sandboxes| {
            if !sandboxes.iter().any(|s| s == directory) {
                sandboxes.push(directory.to_string());
            }
        })
    }

    /// `removeSandbox` (`project.ts:434-448`).
    pub fn remove_sandbox(&self, id: &str, directory: &str) -> Result<(), RegistryError> {
        self.sandbox_update(id, |sandboxes| {
            sandboxes.retain(|sandbox| sandbox != directory);
        })
    }

    fn sandbox_update(
        &self,
        id: &str,
        update: impl FnOnce(&mut Vec<String>),
    ) -> Result<(), RegistryError> {
        let row = self
            .row(id)?
            .ok_or(RegistryError::Message(format!("Project not found: {id}")))?;
        let mut sandboxes = sandboxes_from(&row.sandboxes)?;
        update(&mut sandboxes);
        let serialized = serde_json::to_string(&sandboxes)
            .map_err(|err| RegistryError::Core(CoreError::Storage(err.to_string())))?;
        let now = self.now() as i64;
        self.storage
            .with_connection(|conn| {
                conn.execute(
                    "UPDATE project SET sandboxes = ?1, time_updated = ?2 WHERE id = ?3",
                    rusqlite::params![serialized, now, id],
                )?;
                Ok::<u64, CoreError>(conn.changes())
            })
            .map_err(RegistryError::from)?;
        if let Some(row) = self.row(id)? {
            self.emit_updated(&self.convert_row(&row)?);
        }
        Ok(())
    }

    /// `ProjectV2.directories` — the `project_directory` list
    /// (`directories.ts:100-109`).
    pub fn directories(&self, project_id: &str) -> Result<Vec<ProjectDirectory>, RegistryError> {
        directories::list(&self.storage, project_id).map_err(RegistryError::from)
    }

    // ------------------------------------------------------------------ icon

    /// `discover` (`project.ts:312-334`) — favicon glob behind
    /// `experimentalIconDiscovery`; the shortest favicon wins.
    fn discover(&self, info: &ProjectInfo) -> Result<(), RegistryError> {
        if info.vcs != Some(ProjectVcs::Git) {
            return Ok(());
        }
        if let Some(icon) = &info.icon {
            if icon.r#override.is_some() || icon.url.is_some() {
                return Ok(());
            }
        }
        let Some(favicon) = find_favicon(Path::new(&info.worktree)) else {
            return Ok(());
        };
        let bytes = std::fs::read(&favicon).map_err(|err| {
            RegistryError::Core(CoreError::Storage(format!("favicon read: {err}")))
        })?;
        let mime = favicon
            .extension()
            .and_then(|ext| ext.to_str())
            .map(mime_type)
            .unwrap_or("application/octet-stream");
        let url = format!(
            "data:{mime};base64,{}",
            base64::Engine::encode(&base64::engine::general_purpose::STANDARD, bytes)
        );
        let result = self.update(&UpdateInput {
            project_id: info.id.clone(),
            name: Field::Unset,
            icon: Field::Set(Some(ProjectIcon {
                url: Some(url),
                r#override: None,
                color: None,
            })),
            commands: Field::Unset,
        });
        match result {
            Ok(_) | Err(RegistryError::NotFound(_)) => Ok(()),
            Err(other) => Err(other),
        }
    }

    // ------------------------------------------------------------------ hook

    /// `init` (`project.ts:386-396`) — the per-instance `/init` command
    /// subscription: a `command.executed` event for the instance directory
    /// with `name == "init"` stamps the initialized timestamp. The
    /// registry retains the subscription for the process lifetime.
    pub fn init_hook(self: &Arc<Self>, events: &Arc<EventBus>, directory: &str, project_id: &str) {
        let registry = Arc::clone(self);
        let directory = directory.to_string();
        let project_id = project_id.to_string();
        let subscription = events.listen(Arc::new(move |event| {
            if event.r#type != "command.executed" {
                return;
            }
            let Some(location) = &event.location else {
                return;
            };
            if location.directory != directory {
                return;
            }
            if event.data.get("name").and_then(|name| name.as_str()) == Some("init") {
                if let Err(err) = registry.set_initialized(&project_id) {
                    tracing::error!("setInitialized failed: {err}");
                }
            }
        }));
        self.hooks
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .push(Arc::new(subscription));
    }
}

fn query_project(conn: &Connection, id: &str) -> Result<Option<ProjectRow>, CoreError> {
    let mut stmt = conn.prepare("SELECT * FROM project WHERE id = ?1")?;
    let mut rows = stmt.query([id])?;
    match rows.next()? {
        Some(row) => Ok(Some(project_from_row(row)?)),
        None => Ok(None),
    }
}

/// The icon columns → `Project.Icon` (`project.ts:36-43`).
fn icon_from_columns(
    url: &Option<String>,
    r#override: &Option<String>,
    color: &Option<String>,
) -> Option<ProjectIcon> {
    if url.is_none() && r#override.is_none() && color.is_none() {
        return None;
    }
    Some(ProjectIcon {
        url: url.clone(),
        r#override: r#override.clone(),
        color: color.clone(),
    })
}

fn sandboxes_from(value: &serde_json::Value) -> Result<Vec<String>, RegistryError> {
    serde_json::from_value(value.clone())
        .map_err(|err| RegistryError::Core(CoreError::Storage(err.to_string())))
}

fn vcs_column(vcs: &Option<ProjectVcs>) -> Option<&'static str> {
    match vcs {
        Some(ProjectVcs::Git) => Some("git"),
        None => None,
    }
}

fn truthy_env(name: &str) -> bool {
    std::env::var(name)
        .map(|value| matches!(value.as_str(), "1" | "true" | "yes"))
        .unwrap_or(false)
}

/// `fs.glob("**/favicon.{ico,png,svg,jpg,jpeg,webp}")` — regular files,
/// dot-directories skipped (`dot: false`), shortest absolute path wins.
fn find_favicon(worktree: &Path) -> Option<PathBuf> {
    const EXTENSIONS: &[&str] = &["ico", "png", "svg", "jpg", "jpeg", "webp"];
    let mut matches: Vec<PathBuf> = Vec::new();
    for entry in walkdir::WalkDir::new(worktree)
        .follow_links(false)
        .into_iter()
        .filter_entry(|entry| {
            entry.depth() == 0
                || entry
                    .file_name()
                    .to_str()
                    .map(|name| name.starts_with('.'))
                    .unwrap_or(true)
        })
        .filter_map(|entry| entry.ok())
    {
        if !entry.file_type().is_file() {
            continue;
        }
        let path = entry.path();
        if path
            .file_stem()
            .map(|stem| stem == "favicon")
            .unwrap_or(false)
            && path
                .extension()
                .and_then(|ext| ext.to_str())
                .map(|ext| EXTENSIONS.contains(&ext.to_ascii_lowercase().as_str()))
                .unwrap_or(false)
        {
            matches.push(path.to_path_buf());
        }
    }
    matches.sort_by_key(|path| path.as_os_str().len());
    matches.into_iter().next()
}

fn mime_type(extension: &str) -> &str {
    match extension.to_ascii_lowercase().as_str() {
        "ico" => "image/vnd.microsoft.icon",
        "png" => "image/png",
        "svg" => "image/svg+xml",
        "jpg" | "jpeg" => "image/jpeg",
        "webp" => "image/webp",
        _ => "application/octet-stream",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::git::SubprocessGit;
    use crate::project::hash_fast;
    use crate::storage::test_support::TempDir;
    use std::sync::atomic::{AtomicU64, Ordering};

    use opencode_schema::project::ProjectVcs;

    struct FixedClock;
    impl Clock for FixedClock {
        fn now_ms(&self) -> u64 {
            0
        }
    }

    struct TickingClock(AtomicU64);
    impl Clock for TickingClock {
        fn now_ms(&self) -> u64 {
            self.0.fetch_add(1, Ordering::SeqCst) + 1
        }
    }

    /// `(tempdir, storage, emitted)` — tests capture the `emitUpdated` sink.
    fn harness() -> (TempDir, Arc<Storage>, Arc<Mutex<Vec<ProjectInfo>>>) {
        let dir = TempDir::new("project-registry");
        let storage = Arc::new(Storage::open(dir.path().join("db.sqlite")).unwrap());
        let emitted: Arc<Mutex<Vec<ProjectInfo>>> = Arc::new(Mutex::new(Vec::new()));
        (dir, storage, emitted)
    }

    fn registry_with(
        clock: Arc<dyn Clock>,
        storage: &Arc<Storage>,
        emitted: &Arc<Mutex<Vec<ProjectInfo>>>,
    ) -> ProjectRegistry {
        let sink = Arc::clone(emitted);
        ProjectRegistry::with_flags(
            storage.clone(),
            Arc::new(SubprocessGit),
            clock,
            Arc::new(move |info| {
                sink.lock()
                    .unwrap_or_else(|p| p.into_inner())
                    .push(info.clone());
            }),
            false,
            false,
        )
    }

    fn registry(storage: &Arc<Storage>, emitted: &Arc<Mutex<Vec<ProjectInfo>>>) -> ProjectRegistry {
        registry_with(Arc::new(FixedClock), storage, emitted)
    }

    fn init_repo(dir: &Path) {
        let git = SubprocessGit;
        let result = git.run(Some(dir), &["init", "--quiet"]);
        assert_eq!(result.exit_code, 0, "git init: {}", result.stderr);
    }

    fn set_remote(dir: &Path, url: &str) {
        let git = SubprocessGit;
        git.run(Some(dir), &["remote", "remove", "origin"]);
        let result = git.run(Some(dir), &["remote", "add", "origin", url]);
        assert_eq!(result.exit_code, 0, "git remote add: {}", result.stderr);
    }

    fn insert_session(storage: &Storage, id: &str, project_id: &str, directory: &str) {
        storage
            .with_connection(|conn| {
                conn.execute(
                    "INSERT INTO session (id, project_id, directory, slug, title, version, time_created, time_updated, cost)
                     VALUES (?1, ?2, ?3, ?4, ?5, 'v', 1, 1, 0)",
                    rusqlite::params![id, project_id, directory, id, id],
                )?;
                Ok::<(), CoreError>(())
            })
            .unwrap();
    }

    #[test]
    fn from_directory_non_git_is_global() {
        let (dir, storage, emitted) = harness();
        let registry = registry(&storage, &emitted);

        let (project, sandbox) = registry
            .from_directory(dir.path())
            .expect("from_directory succeeds");
        assert_eq!(project.id, GLOBAL_ID);
        assert_eq!(project.worktree, "/");
        assert_eq!(project.vcs, None);
        // The sandbox is the worktree ("/") when no VCS is present.
        assert_eq!(sandbox, PathBuf::from("/"));

        let emitted = emitted.lock().unwrap_or_else(|p| p.into_inner()).clone();
        assert_eq!(emitted.len(), 1, "emitUpdated fires once per boot");
        assert_eq!(emitted[0].id, GLOBAL_ID);
        assert!(
            registry
                .list()
                .unwrap()
                .iter()
                .any(|project| project.id == GLOBAL_ID),
            "the global project row is persisted"
        );
    }

    #[test]
    fn from_directory_git_remote_project() {
        let (dir, storage, emitted) = harness();
        let repo = dir.path().join("repo");
        std::fs::create_dir_all(&repo).unwrap();
        init_repo(&repo);
        set_remote(&repo, "https://github.com/acme/widgets.git");
        let registry = registry(&storage, &emitted);

        let (project, sandbox) = registry
            .from_directory(&repo)
            .expect("from_directory succeeds");
        let expected = hash_fast("git-remote:github.com/acme/widgets");
        assert_eq!(project.id, expected);
        assert_eq!(project.vcs, Some(ProjectVcs::Git));
        assert_eq!(project.worktree, repo.to_string_lossy());
        assert_eq!(sandbox, repo);
        // `.git/opencode` caches the id (Project.commit).
        let cached = std::fs::read_to_string(repo.join(".git/opencode")).unwrap();
        assert_eq!(cached, expected);
    }

    #[test]
    fn from_directory_migrates_previous_project() {
        let (dir, storage, _emitted) = harness();
        let repo = dir.path().join("repo");
        std::fs::create_dir_all(&repo).unwrap();
        init_repo(&repo);
        set_remote(&repo, "https://github.com/acme/widgets.git");
        let registry = registry(&storage, &_emitted);

        let (first, _) = registry.from_directory(&repo).unwrap();
        insert_session(&storage, "ses_1", &first.id, &repo.to_string_lossy());

        // The remote moves to a different host — the cached id is the
        // `previous` and the migration transaction runs.
        set_remote(&repo, "https://github.com/acme/renamed.git");
        let (second, _) = registry.from_directory(&repo).unwrap();
        let new_id = hash_fast("git-remote:github.com/acme/renamed");
        assert_eq!(second.id, new_id);
        assert_ne!(first.id, second.id);

        // The session moved and the old project row is gone.
        let count = storage
            .with_connection(|conn| {
                Ok::<_, CoreError>(conn.query_row(
                    "SELECT COUNT(*) FROM session WHERE project_id = ?1 AND id = 'ses_1'",
                    [&new_id],
                    |row| row.get::<_, i64>(0),
                )? as u64)
            })
            .unwrap();
        assert_eq!(count, 1, "session must move to the new project id");
        assert!(
            registry.get(&first.id).unwrap().is_none(),
            "old project row must be deleted"
        );
        assert!(
            registry.get(&new_id).unwrap().is_some(),
            "new project row must exist"
        );
    }

    #[test]
    fn from_directory_pushes_and_filters_sandboxes() {
        let (dir, storage, emitted) = harness();
        let repo = dir.path().join("repo");
        std::fs::create_dir_all(&repo).unwrap();
        init_repo(&repo);
        set_remote(&repo, "https://github.com/acme/widgets.git");
        let registry = registry(&storage, &emitted);

        // First boot: the resolved directory IS the worktree, so the
        // sandbox list stays empty (`data.directory !== result.worktree`,
        // project.ts:245-246).
        let nested = repo.join("sub/deep");
        std::fs::create_dir_all(&nested).unwrap();
        let (project, sandbox) = registry.from_directory(&nested).unwrap();
        assert_eq!(sandbox, repo, "sandbox is the git worktree");
        assert!(project.sandboxes.is_empty());

        // The repository moves on disk — the new location becomes a
        // sandbox of the same project.
        registry
            .add_sandbox(&project.id, "/definitely/not/there")
            .unwrap();
        let moved = dir.path().join("moved");
        std::fs::rename(&repo, &moved).unwrap();
        let (project, sandbox) = registry.from_directory(&moved).unwrap();
        assert_eq!(sandbox, moved);
        assert_eq!(
            project.sandboxes,
            vec![moved.to_string_lossy().into_owned()],
            "missing sandboxes are dropped"
        );
    }

    #[test]
    fn update_moves_only_provided_columns() {
        let (dir, storage, emitted) = harness();
        let registry = registry(&storage, &emitted);
        let repo = dir.path().join("repo");
        std::fs::create_dir_all(&repo).unwrap();
        init_repo(&repo);
        set_remote(&repo, "https://github.com/acme/widgets.git");
        let (project, _) = registry.from_directory(&repo).unwrap();

        let updated = registry
            .update(&UpdateInput {
                project_id: project.id.clone(),
                name: Field::Set(Some("Widgets".to_string())),
                icon: Field::Unset,
                commands: Field::Unset,
            })
            .unwrap();
        assert_eq!(updated.name.as_deref(), Some("Widgets"));

        // Null icon fields clear the columns (`Set(None)`).
        let updated = registry
            .update(&UpdateInput {
                project_id: project.id.clone(),
                name: Field::Unset,
                icon: Field::Set(None),
                commands: Field::Unset,
            })
            .unwrap();
        assert_eq!(updated.icon, None);

        let err = registry
            .update(&UpdateInput {
                project_id: "prj_missing".to_string(),
                name: Field::Set(Some("x".to_string())),
                icon: Field::Unset,
                commands: Field::Unset,
            })
            .unwrap_err();
        assert!(
            matches!(
                &err,
                RegistryError::NotFound(NotFoundError { project_id }) if project_id == "prj_missing"
            ),
            "NotFound error with the project id, got {err:?}"
        );
    }

    #[test]
    fn init_git_bootstraps_a_repository() {
        let (dir, storage, emitted) = harness();
        let clock = Arc::new(TickingClock(AtomicU64::new(0)));
        let registry = registry_with(clock, &storage, &emitted);
        let plain = dir.path().join("plain");
        std::fs::create_dir_all(&plain).unwrap();

        let (project, _) = registry.from_directory(&plain).unwrap();
        assert_eq!(project.id, GLOBAL_ID);
        assert_eq!(project.vcs, None);

        // Once git initializes, the project gains a VCS but keeps the
        // global id until a root commit exists.
        let project = registry.init_git(&plain, &project).unwrap();
        assert!(plain.join(".git").exists(), "git init must run");
        assert_eq!(project.vcs, Some(ProjectVcs::Git));

        let initialized = registry.set_initialized(&project.id);
        assert!(initialized.is_ok());
    }
}
