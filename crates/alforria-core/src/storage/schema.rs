//! The SQLite schema and the typed row structs over it.
//!
//! `SCHEMA_SQL` holds the fresh-install statements verbatim from
//! `packages/core/src/database/schema.gen.ts` (spec §7): the 19
//! `CREATE TABLE` statements followed by the 17 `CREATE INDEX` statements, in
//! the exact order TS executes them. Backticks are stripped (they are SQLite
//! quoting, not part of the identifiers) and the trailing `;` is omitted because
//! SQLite does not store it in `sqlite_master.sql` — the byte-comparison test
//! relies on both.

use rusqlite::{Row, ToSql};
use serde_json::Value;

use crate::storage::connection::Storage;
use crate::CoreError;

/// Fresh-install schema (spec §7), verbatim from `schema.gen.ts`.
pub const SCHEMA_SQL: &[&str] = &[
    // ------------------------------------------------------------- tables
    "CREATE TABLE workspace (
  id text PRIMARY KEY,
  type text NOT NULL,
  name text DEFAULT '' NOT NULL,
  branch text,
  directory text,
  extra text,
  project_id text NOT NULL,
  time_used integer NOT NULL,
  CONSTRAINT fk_workspace_project_id_project_id_fk FOREIGN KEY (project_id) REFERENCES project(id) ON DELETE CASCADE
)",
    "CREATE TABLE data_migration (
  name text PRIMARY KEY,
  time_completed integer NOT NULL
)",
    "CREATE TABLE account_state (
  id integer PRIMARY KEY,
  active_account_id text,
  active_org_id text,
  CONSTRAINT fk_account_state_active_account_id_account_id_fk FOREIGN KEY (active_account_id) REFERENCES account(id) ON DELETE SET NULL
)",
    "CREATE TABLE account (
  id text PRIMARY KEY,
  email text NOT NULL,
  url text NOT NULL,
  access_token text NOT NULL,
  refresh_token text NOT NULL,
  token_expiry integer,
  time_created integer NOT NULL,
  time_updated integer NOT NULL
)",
    "CREATE TABLE control_account (
  email text NOT NULL,
  url text NOT NULL,
  access_token text NOT NULL,
  refresh_token text NOT NULL,
  token_expiry integer,
  active integer NOT NULL,
  time_created integer NOT NULL,
  time_updated integer NOT NULL,
  CONSTRAINT control_account_pk PRIMARY KEY(email, url)
)",
    "CREATE TABLE credential (
  id text PRIMARY KEY,
  integration_id text,
  label text NOT NULL,
  value text NOT NULL,
  connector_id text,
  method_id text,
  active integer,
  time_created integer NOT NULL,
  time_updated integer NOT NULL
)",
    "CREATE TABLE event_sequence (
  aggregate_id text PRIMARY KEY,
  seq integer NOT NULL,
  owner_id text
)",
    "CREATE TABLE event (
  id text PRIMARY KEY,
  aggregate_id text NOT NULL,
  seq integer NOT NULL,
  type text NOT NULL,
  data text NOT NULL,
  CONSTRAINT fk_event_aggregate_id_event_sequence_aggregate_id_fk FOREIGN KEY (aggregate_id) REFERENCES event_sequence(aggregate_id) ON DELETE CASCADE
)",
    "CREATE TABLE permission (
  id text PRIMARY KEY,
  project_id text NOT NULL,
  action text NOT NULL,
  resource text NOT NULL,
  time_created integer NOT NULL,
  time_updated integer NOT NULL,
  CONSTRAINT fk_permission_project_id_project_id_fk FOREIGN KEY (project_id) REFERENCES project(id) ON DELETE CASCADE
)",
    "CREATE TABLE project_directory (
  project_id text NOT NULL,
  directory text NOT NULL,
  type text,
  strategy text,
  time_created integer NOT NULL,
  CONSTRAINT project_directory_pk PRIMARY KEY(project_id, directory),
  CONSTRAINT fk_project_directory_project_id_project_id_fk FOREIGN KEY (project_id) REFERENCES project(id) ON DELETE CASCADE
)",
    "CREATE TABLE project (
  id text PRIMARY KEY,
  worktree text NOT NULL,
  vcs text,
  name text,
  icon_url text,
  icon_url_override text,
  icon_color text,
  time_created integer NOT NULL,
  time_updated integer NOT NULL,
  time_initialized integer,
  sandboxes text NOT NULL,
  commands text
)",
    "CREATE TABLE message (
  id text PRIMARY KEY,
  session_id text NOT NULL,
  time_created integer NOT NULL,
  time_updated integer NOT NULL,
  data text NOT NULL,
  CONSTRAINT fk_message_session_id_session_id_fk FOREIGN KEY (session_id) REFERENCES session(id) ON DELETE CASCADE
)",
    "CREATE TABLE part (
  id text PRIMARY KEY,
  message_id text NOT NULL,
  session_id text NOT NULL,
  time_created integer NOT NULL,
  time_updated integer NOT NULL,
  data text NOT NULL,
  CONSTRAINT fk_part_message_id_message_id_fk FOREIGN KEY (message_id) REFERENCES message(id) ON DELETE CASCADE
)",
    "CREATE TABLE session_context_epoch (
  session_id text PRIMARY KEY,
  baseline text NOT NULL,
  snapshot text NOT NULL,
  baseline_seq integer NOT NULL,
  CONSTRAINT fk_session_context_epoch_session_id_session_id_fk FOREIGN KEY (session_id) REFERENCES session(id) ON DELETE CASCADE
)",
    "CREATE TABLE session_input (
  id text PRIMARY KEY,
  session_id text NOT NULL,
  prompt text NOT NULL,
  delivery text NOT NULL,
  admitted_seq integer NOT NULL,
  promoted_seq integer,
  time_created integer NOT NULL,
  CONSTRAINT fk_session_input_session_id_session_id_fk FOREIGN KEY (session_id) REFERENCES session(id) ON DELETE CASCADE
)",
    "CREATE TABLE session_message (
  id text PRIMARY KEY,
  session_id text NOT NULL,
  type text NOT NULL,
  seq integer NOT NULL,
  time_created integer NOT NULL,
  time_updated integer NOT NULL,
  data text NOT NULL,
  CONSTRAINT fk_session_message_session_id_session_id_fk FOREIGN KEY (session_id) REFERENCES session(id) ON DELETE CASCADE
)",
    "CREATE TABLE session (
  id text PRIMARY KEY,
  project_id text NOT NULL,
  workspace_id text,
  parent_id text,
  slug text NOT NULL,
  directory text NOT NULL,
  path text,
  title text NOT NULL,
  version text NOT NULL,
  share_url text,
  summary_additions integer,
  summary_deletions integer,
  summary_files integer,
  summary_diffs text,
  metadata text,
  cost real DEFAULT 0 NOT NULL,
  tokens_input integer DEFAULT 0 NOT NULL,
  tokens_output integer DEFAULT 0 NOT NULL,
  tokens_reasoning integer DEFAULT 0 NOT NULL,
  tokens_cache_read integer DEFAULT 0 NOT NULL,
  tokens_cache_write integer DEFAULT 0 NOT NULL,
  revert text,
  permission text,
  agent text,
  model text,
  time_created integer NOT NULL,
  time_updated integer NOT NULL,
  time_compacting integer,
  time_archived integer,
  CONSTRAINT fk_session_project_id_project_id_fk FOREIGN KEY (project_id) REFERENCES project(id) ON DELETE CASCADE
)",
    "CREATE TABLE todo (
  session_id text NOT NULL,
  content text NOT NULL,
  status text NOT NULL,
  priority text NOT NULL,
  position integer NOT NULL,
  time_created integer NOT NULL,
  time_updated integer NOT NULL,
  CONSTRAINT todo_pk PRIMARY KEY(session_id, position),
  CONSTRAINT fk_todo_session_id_session_id_fk FOREIGN KEY (session_id) REFERENCES session(id) ON DELETE CASCADE
)",
    "CREATE TABLE session_share (
  session_id text PRIMARY KEY,
  id text NOT NULL,
  secret text NOT NULL,
  url text NOT NULL,
  time_created integer NOT NULL,
  time_updated integer NOT NULL,
  CONSTRAINT fk_session_share_session_id_session_id_fk FOREIGN KEY (session_id) REFERENCES session(id) ON DELETE CASCADE
)",
    // -------------------------------------------------------------- indexes
    "CREATE UNIQUE INDEX event_aggregate_seq_idx ON event (aggregate_id, seq)",
    "CREATE INDEX event_aggregate_type_seq_idx ON event (aggregate_id, type, seq)",
    "CREATE UNIQUE INDEX permission_project_action_resource_idx ON permission (project_id, action, resource)",
    "CREATE INDEX message_session_time_created_id_idx ON message (session_id, time_created, id)",
    "CREATE INDEX part_message_id_id_idx ON part (message_id, id)",
    "CREATE INDEX part_session_idx ON part (session_id)",
    "CREATE INDEX session_input_session_pending_delivery_seq_idx ON session_input (session_id, promoted_seq, delivery, admitted_seq)",
    "CREATE UNIQUE INDEX session_input_session_admitted_seq_idx ON session_input (session_id, admitted_seq)",
    "CREATE UNIQUE INDEX session_input_session_promoted_seq_idx ON session_input (session_id, promoted_seq)",
    "CREATE UNIQUE INDEX session_message_session_seq_idx ON session_message (session_id, seq)",
    "CREATE INDEX session_message_session_type_seq_idx ON session_message (session_id, type, seq)",
    "CREATE INDEX session_message_session_time_created_id_idx ON session_message (session_id, time_created, id)",
    "CREATE INDEX session_message_time_created_idx ON session_message (time_created)",
    "CREATE INDEX session_project_idx ON session (project_id)",
    "CREATE INDEX session_workspace_idx ON session (workspace_id)",
    "CREATE INDEX session_parent_idx ON session (parent_id)",
    "CREATE INDEX todo_session_idx ON todo (session_id)",
];

/// Serialize a JSON column the way drizzle does: compact, no spaces.
fn json_to_string(value: &Value) -> Result<String, CoreError> {
    serde_json::to_string(value).map_err(|err| CoreError::Storage(err.to_string()))
}

/// Serialize an optional JSON column (`NULL` stays `NULL`).
pub(crate) fn json_opt_to_string(value: &Option<Value>) -> Result<Option<String>, CoreError> {
    value.as_ref().map(json_to_string).transpose()
}

/// Deserialize a JSON column. Never fails for values written by
/// [`json_to_string`]; corrupt rows surface as a storage error.
fn json_from_text(text: &str) -> Result<Value, CoreError> {
    serde_json::from_str(text).map_err(|err| CoreError::Storage(err.to_string()))
}

/// `json_column` by column name — the TS row mappers (Drizzle) bind by
/// name, so column order can differ across schema versions of an existing
/// database (e.g. `icon_url_override` appended late in `opencode.db`).
fn json_named_column(row: &Row<'_>, name: &str) -> Result<Value, CoreError> {
    json_from_text(&row.get::<_, String>(name)?)
}

fn json_opt_named_column(row: &Row<'_>, name: &str) -> Result<Option<Value>, CoreError> {
    row.get::<_, Option<String>>(name)?
        .as_deref()
        .map(json_from_text)
        .transpose()
}

// --------------------------------------------------------------------- rows

/// Row of the `project` table.
#[derive(Debug, Clone, PartialEq)]
pub struct Project {
    pub id: String,
    pub worktree: String,
    pub vcs: Option<String>,
    pub name: Option<String>,
    pub icon_url: Option<String>,
    pub icon_url_override: Option<String>,
    pub icon_color: Option<String>,
    pub time_created: i64,
    pub time_updated: i64,
    pub time_initialized: Option<i64>,
    /// JSON column (`text NOT NULL`).
    pub sandboxes: Value,
    pub commands: Option<String>,
}

/// Row of the `workspace` table.
#[derive(Debug, Clone, PartialEq)]
pub struct Workspace {
    pub id: String,
    pub r#type: String,
    pub name: String,
    pub branch: Option<String>,
    pub directory: Option<String>,
    pub extra: Option<String>,
    pub project_id: String,
    pub time_used: i64,
}

/// Row of the `session` table.
#[derive(Debug, Clone, PartialEq)]
pub struct Session {
    pub id: String,
    pub project_id: String,
    pub workspace_id: Option<String>,
    pub parent_id: Option<String>,
    pub slug: String,
    pub directory: String,
    pub path: Option<String>,
    pub title: String,
    pub version: String,
    pub share_url: Option<String>,
    pub summary_additions: Option<i64>,
    pub summary_deletions: Option<i64>,
    pub summary_files: Option<i64>,
    /// JSON column.
    pub summary_diffs: Option<Value>,
    /// JSON column.
    pub metadata: Option<Value>,
    pub cost: f64,
    pub tokens_input: i64,
    pub tokens_output: i64,
    pub tokens_reasoning: i64,
    pub tokens_cache_read: i64,
    pub tokens_cache_write: i64,
    /// JSON column.
    pub revert: Option<Value>,
    /// JSON column.
    pub permission: Option<Value>,
    pub agent: Option<String>,
    pub model: Option<String>,
    pub time_created: i64,
    pub time_updated: i64,
    pub time_compacting: Option<i64>,
    pub time_archived: Option<i64>,
}

/// Row of the `message` table.
#[derive(Debug, Clone, PartialEq)]
pub struct Message {
    pub id: String,
    pub session_id: String,
    pub time_created: i64,
    pub time_updated: i64,
    /// JSON column (`text NOT NULL`).
    pub data: Value,
}

/// Row of the `part` table.
#[derive(Debug, Clone, PartialEq)]
pub struct Part {
    pub id: String,
    pub message_id: String,
    pub session_id: String,
    pub time_created: i64,
    pub time_updated: i64,
    /// JSON column (`text NOT NULL`).
    pub data: Value,
}

/// Row of the `todo` table (composite PK: `session_id`, `position`).
#[derive(Debug, Clone, PartialEq)]
pub struct Todo {
    pub session_id: String,
    pub content: String,
    pub status: String,
    pub priority: String,
    pub position: i64,
    pub time_created: i64,
    pub time_updated: i64,
}

/// Row of the `session_share` table.
#[derive(Debug, Clone, PartialEq)]
pub struct SessionShare {
    pub session_id: String,
    pub id: String,
    pub secret: String,
    pub url: String,
    pub time_created: i64,
    pub time_updated: i64,
}

/// Row of the `account` table.
#[derive(Debug, Clone, PartialEq)]
pub struct Account {
    pub id: String,
    pub email: String,
    pub url: String,
    pub access_token: String,
    pub refresh_token: String,
    pub token_expiry: Option<i64>,
    pub time_created: i64,
    pub time_updated: i64,
}

/// Row of the `account_state` table. `id` is `None` for insert so the
/// `integer PRIMARY KEY` rowid is auto-assigned.
#[derive(Debug, Clone, PartialEq)]
pub struct AccountState {
    pub id: Option<i64>,
    pub active_account_id: Option<String>,
    pub active_org_id: Option<String>,
}

/// Row of the `control_account` table (composite PK: `email`, `url`).
#[derive(Debug, Clone, PartialEq)]
pub struct ControlAccount {
    pub email: String,
    pub url: String,
    pub access_token: String,
    pub refresh_token: String,
    pub token_expiry: Option<i64>,
    pub active: i64,
    pub time_created: i64,
    pub time_updated: i64,
}

// ------------------------------------------------------------------- CRUD

impl Storage {
    fn query_all<T>(
        &self,
        sql: &str,
        params: &[&dyn ToSql],
        map: fn(&Row<'_>) -> Result<T, CoreError>,
    ) -> Result<Vec<T>, CoreError> {
        self.with_connection(|conn| {
            let mut stmt = conn.prepare(sql)?;
            let mut rows = stmt.query(params)?;
            let mut out = Vec::new();
            while let Some(row) = rows.next()? {
                out.push(map(row)?);
            }
            Ok(out)
        })
    }

    fn query_one<T>(
        &self,
        sql: &str,
        params: &[&dyn ToSql],
        map: fn(&Row<'_>) -> Result<T, CoreError>,
    ) -> Result<Option<T>, CoreError> {
        self.with_connection(|conn| {
            let mut stmt = conn.prepare(sql)?;
            let mut rows = stmt.query(params)?;
            match rows.next()? {
                Some(row) => Ok(Some(map(row)?)),
                None => Ok(None),
            }
        })
    }

    // ---------------------------------------------------------------- project

    pub fn put_project(&self, row: &Project) -> Result<(), CoreError> {
        self.with_connection(|conn| {
            conn.execute(
                "INSERT OR REPLACE INTO project (id, worktree, vcs, name, icon_url, icon_url_override, icon_color, time_created, time_updated, time_initialized, sandboxes, commands)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12)",
                rusqlite::params![
                    row.id,
                    row.worktree,
                    row.vcs,
                    row.name,
                    row.icon_url,
                    row.icon_url_override,
                    row.icon_color,
                    row.time_created,
                    row.time_updated,
                    row.time_initialized,
                    json_to_string(&row.sandboxes)?,
                    row.commands,
                ],
            )?;
            Ok(())
        })
    }

    pub fn get_project(&self, id: &str) -> Result<Option<Project>, CoreError> {
        self.query_one(
            "SELECT * FROM project WHERE id = ?1",
            &[&id],
            project_from_row,
        )
    }

    pub fn list_projects(&self) -> Result<Vec<Project>, CoreError> {
        self.query_all("SELECT * FROM project ORDER BY id", &[], project_from_row)
    }

    // -------------------------------------------------------------- workspace

    pub fn put_workspace(&self, row: &Workspace) -> Result<(), CoreError> {
        self.with_connection(|conn| {
            conn.execute(
                "INSERT OR REPLACE INTO workspace (id, type, name, branch, directory, extra, project_id, time_used)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
                rusqlite::params![
                    row.id,
                    row.r#type,
                    row.name,
                    row.branch,
                    row.directory,
                    row.extra,
                    row.project_id,
                    row.time_used,
                ],
            )?;
            Ok(())
        })
    }

    pub fn get_workspace(&self, id: &str) -> Result<Option<Workspace>, CoreError> {
        self.query_one(
            "SELECT * FROM workspace WHERE id = ?1",
            &[&id],
            workspace_from_row,
        )
    }

    pub fn list_workspaces(&self) -> Result<Vec<Workspace>, CoreError> {
        self.query_all(
            "SELECT * FROM workspace ORDER BY id",
            &[],
            workspace_from_row,
        )
    }

    // ---------------------------------------------------------------- session

    pub fn put_session(&self, row: &Session) -> Result<(), CoreError> {
        self.with_connection(|conn| {
            conn.execute(
                "INSERT OR REPLACE INTO session (id, project_id, workspace_id, parent_id, slug, directory, path, title, version, share_url,
                    summary_additions, summary_deletions, summary_files, summary_diffs, metadata, cost, tokens_input, tokens_output,
                    tokens_reasoning, tokens_cache_read, tokens_cache_write, revert, permission, agent, model,
                    time_created, time_updated, time_compacting, time_archived)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16, ?17, ?18, ?19, ?20, ?21, ?22, ?23, ?24, ?25, ?26, ?27, ?28, ?29)",
                rusqlite::params![
                    row.id,
                    row.project_id,
                    row.workspace_id,
                    row.parent_id,
                    row.slug,
                    row.directory,
                    row.path,
                    row.title,
                    row.version,
                    row.share_url,
                    row.summary_additions,
                    row.summary_deletions,
                    row.summary_files,
                    json_opt_to_string(&row.summary_diffs)?,
                    json_opt_to_string(&row.metadata)?,
                    row.cost,
                    row.tokens_input,
                    row.tokens_output,
                    row.tokens_reasoning,
                    row.tokens_cache_read,
                    row.tokens_cache_write,
                    json_opt_to_string(&row.revert)?,
                    json_opt_to_string(&row.permission)?,
                    row.agent,
                    row.model,
                    row.time_created,
                    row.time_updated,
                    row.time_compacting,
                    row.time_archived,
                ],
            )?;
            Ok(())
        })
    }

    pub fn get_session(&self, id: &str) -> Result<Option<Session>, CoreError> {
        self.query_one(
            "SELECT * FROM session WHERE id = ?1",
            &[&id],
            session_from_row,
        )
    }

    pub fn list_sessions(&self) -> Result<Vec<Session>, CoreError> {
        self.query_all("SELECT * FROM session ORDER BY id", &[], session_from_row)
    }

    pub fn delete_session(&self, id: &str) -> Result<(), CoreError> {
        self.with_connection(|conn| {
            conn.execute("DELETE FROM session WHERE id = ?1", [id])?;
            Ok(())
        })
    }

    // ---------------------------------------------------------------- message

    pub fn put_message(&self, row: &Message) -> Result<(), CoreError> {
        self.with_connection(|conn| {
            conn.execute(
                "INSERT OR REPLACE INTO message (id, session_id, time_created, time_updated, data)
                 VALUES (?1, ?2, ?3, ?4, ?5)",
                rusqlite::params![
                    row.id,
                    row.session_id,
                    row.time_created,
                    row.time_updated,
                    json_to_string(&row.data)?,
                ],
            )?;
            Ok(())
        })
    }

    pub fn get_message(&self, id: &str) -> Result<Option<Message>, CoreError> {
        self.query_one(
            "SELECT * FROM message WHERE id = ?1",
            &[&id],
            message_from_row,
        )
    }

    pub fn list_messages(&self, session_id: &str) -> Result<Vec<Message>, CoreError> {
        self.query_all(
            "SELECT * FROM message WHERE session_id = ?1 ORDER BY id",
            &[&session_id],
            message_from_row,
        )
    }

    // ------------------------------------------------------------------- part

    pub fn put_part(&self, row: &Part) -> Result<(), CoreError> {
        self.with_connection(|conn| {
            conn.execute(
                "INSERT OR REPLACE INTO part (id, message_id, session_id, time_created, time_updated, data)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
                rusqlite::params![
                    row.id,
                    row.message_id,
                    row.session_id,
                    row.time_created,
                    row.time_updated,
                    json_to_string(&row.data)?,
                ],
            )?;
            Ok(())
        })
    }

    pub fn get_part(&self, id: &str) -> Result<Option<Part>, CoreError> {
        self.query_one("SELECT * FROM part WHERE id = ?1", &[&id], part_from_row)
    }

    pub fn list_parts(&self, message_id: &str) -> Result<Vec<Part>, CoreError> {
        self.query_all(
            "SELECT * FROM part WHERE message_id = ?1 ORDER BY id",
            &[&message_id],
            part_from_row,
        )
    }

    // ------------------------------------------------------------------- todo

    pub fn put_todo(&self, row: &Todo) -> Result<(), CoreError> {
        self.with_connection(|conn| {
            conn.execute(
                "INSERT OR REPLACE INTO todo (session_id, content, status, priority, position, time_created, time_updated)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
                rusqlite::params![
                    row.session_id,
                    row.content,
                    row.status,
                    row.priority,
                    row.position,
                    row.time_created,
                    row.time_updated,
                ],
            )?;
            Ok(())
        })
    }

    pub fn get_todo(&self, session_id: &str, position: i64) -> Result<Option<Todo>, CoreError> {
        self.query_one(
            "SELECT * FROM todo WHERE session_id = ?1 AND position = ?2",
            &[&session_id, &position],
            todo_from_row,
        )
    }

    pub fn list_todos(&self, session_id: &str) -> Result<Vec<Todo>, CoreError> {
        self.query_all(
            "SELECT * FROM todo WHERE session_id = ?1 ORDER BY position",
            &[&session_id],
            todo_from_row,
        )
    }

    // ----------------------------------------------------------- session_share

    pub fn put_session_share(&self, row: &SessionShare) -> Result<(), CoreError> {
        self.with_connection(|conn| {
            conn.execute(
                "INSERT OR REPLACE INTO session_share (session_id, id, secret, url, time_created, time_updated)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
                rusqlite::params![
                    row.session_id,
                    row.id,
                    row.secret,
                    row.url,
                    row.time_created,
                    row.time_updated,
                ],
            )?;
            Ok(())
        })
    }

    pub fn get_session_share(&self, session_id: &str) -> Result<Option<SessionShare>, CoreError> {
        self.query_one(
            "SELECT * FROM session_share WHERE session_id = ?1",
            &[&session_id],
            session_share_from_row,
        )
    }

    // ---------------------------------------------------------------- account

    pub fn put_account(&self, row: &Account) -> Result<(), CoreError> {
        self.with_connection(|conn| {
            conn.execute(
                "INSERT OR REPLACE INTO account (id, email, url, access_token, refresh_token, token_expiry, time_created, time_updated)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
                rusqlite::params![
                    row.id,
                    row.email,
                    row.url,
                    row.access_token,
                    row.refresh_token,
                    row.token_expiry,
                    row.time_created,
                    row.time_updated,
                ],
            )?;
            Ok(())
        })
    }

    pub fn get_account(&self, id: &str) -> Result<Option<Account>, CoreError> {
        self.query_one(
            "SELECT * FROM account WHERE id = ?1",
            &[&id],
            account_from_row,
        )
    }

    pub fn list_accounts(&self) -> Result<Vec<Account>, CoreError> {
        self.query_all("SELECT * FROM account ORDER BY id", &[], account_from_row)
    }

    // ------------------------------------------------------------ account_state

    pub fn put_account_state(&self, row: &AccountState) -> Result<i64, CoreError> {
        self.with_connection(|conn| {
            conn.execute(
                "INSERT OR REPLACE INTO account_state (id, active_account_id, active_org_id) VALUES (?1, ?2, ?3)",
                rusqlite::params![row.id, row.active_account_id, row.active_org_id],
            )?;
            Ok(conn.last_insert_rowid())
        })
    }

    pub fn get_account_state(&self, id: i64) -> Result<Option<AccountState>, CoreError> {
        self.query_one(
            "SELECT * FROM account_state WHERE id = ?1",
            &[&id],
            account_state_from_row,
        )
    }

    pub fn list_account_states(&self) -> Result<Vec<AccountState>, CoreError> {
        self.query_all(
            "SELECT * FROM account_state ORDER BY id",
            &[],
            account_state_from_row,
        )
    }

    // --------------------------------------------------------- control_account

    pub fn put_control_account(&self, row: &ControlAccount) -> Result<(), CoreError> {
        self.with_connection(|conn| {
            conn.execute(
                "INSERT OR REPLACE INTO control_account (email, url, access_token, refresh_token, token_expiry, active, time_created, time_updated)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
                rusqlite::params![
                    row.email,
                    row.url,
                    row.access_token,
                    row.refresh_token,
                    row.token_expiry,
                    row.active,
                    row.time_created,
                    row.time_updated,
                ],
            )?;
            Ok(())
        })
    }

    pub fn get_control_account(
        &self,
        email: &str,
        url: &str,
    ) -> Result<Option<ControlAccount>, CoreError> {
        self.query_one(
            "SELECT * FROM control_account WHERE email = ?1 AND url = ?2",
            &[&email, &url],
            control_account_from_row,
        )
    }

    pub fn list_control_accounts(&self) -> Result<Vec<ControlAccount>, CoreError> {
        self.query_all(
            "SELECT * FROM control_account ORDER BY email, url",
            &[],
            control_account_from_row,
        )
    }
}

// ------------------------------------------------------------ row mapping

pub(crate) fn project_from_row(row: &Row<'_>) -> Result<Project, CoreError> {
    Ok(Project {
        id: row.get("id")?,
        worktree: row.get("worktree")?,
        vcs: row.get("vcs")?,
        name: row.get("name")?,
        icon_url: row.get("icon_url")?,
        icon_url_override: row.get("icon_url_override")?,
        icon_color: row.get("icon_color")?,
        time_created: row.get("time_created")?,
        time_updated: row.get("time_updated")?,
        time_initialized: row.get("time_initialized")?,
        sandboxes: json_named_column(row, "sandboxes")?,
        commands: row.get("commands")?,
    })
}

fn workspace_from_row(row: &Row<'_>) -> Result<Workspace, CoreError> {
    Ok(Workspace {
        id: row.get("id")?,
        r#type: row.get("type")?,
        name: row.get("name")?,
        branch: row.get("branch")?,
        directory: row.get("directory")?,
        extra: row.get("extra")?,
        project_id: row.get("project_id")?,
        time_used: row.get("time_used")?,
    })
}

pub(crate) fn session_from_row(row: &Row<'_>) -> Result<Session, CoreError> {
    Ok(Session {
        id: row.get("id")?,
        project_id: row.get("project_id")?,
        workspace_id: row.get("workspace_id")?,
        parent_id: row.get("parent_id")?,
        slug: row.get("slug")?,
        directory: row.get("directory")?,
        path: row.get("path")?,
        title: row.get("title")?,
        version: row.get("version")?,
        share_url: row.get("share_url")?,
        summary_additions: row.get("summary_additions")?,
        summary_deletions: row.get("summary_deletions")?,
        summary_files: row.get("summary_files")?,
        summary_diffs: json_opt_named_column(row, "summary_diffs")?,
        metadata: json_opt_named_column(row, "metadata")?,
        cost: row.get("cost")?,
        tokens_input: row.get("tokens_input")?,
        tokens_output: row.get("tokens_output")?,
        tokens_reasoning: row.get("tokens_reasoning")?,
        tokens_cache_read: row.get("tokens_cache_read")?,
        tokens_cache_write: row.get("tokens_cache_write")?,
        revert: json_opt_named_column(row, "revert")?,
        permission: json_opt_named_column(row, "permission")?,
        agent: row.get("agent")?,
        model: row.get("model")?,
        time_created: row.get("time_created")?,
        time_updated: row.get("time_updated")?,
        time_compacting: row.get("time_compacting")?,
        time_archived: row.get("time_archived")?,
    })
}

pub(crate) fn message_from_row(row: &Row<'_>) -> Result<Message, CoreError> {
    Ok(Message {
        id: row.get("id")?,
        session_id: row.get("session_id")?,
        time_created: row.get("time_created")?,
        time_updated: row.get("time_updated")?,
        data: json_named_column(row, "data")?,
    })
}

pub(crate) fn part_from_row(row: &Row<'_>) -> Result<Part, CoreError> {
    Ok(Part {
        id: row.get("id")?,
        message_id: row.get("message_id")?,
        session_id: row.get("session_id")?,
        time_created: row.get("time_created")?,
        time_updated: row.get("time_updated")?,
        data: json_named_column(row, "data")?,
    })
}

fn todo_from_row(row: &Row<'_>) -> Result<Todo, CoreError> {
    Ok(Todo {
        session_id: row.get("session_id")?,
        content: row.get("content")?,
        status: row.get("status")?,
        priority: row.get("priority")?,
        position: row.get("position")?,
        time_created: row.get("time_created")?,
        time_updated: row.get("time_updated")?,
    })
}

fn session_share_from_row(row: &Row<'_>) -> Result<SessionShare, CoreError> {
    Ok(SessionShare {
        session_id: row.get("session_id")?,
        id: row.get("id")?,
        secret: row.get("secret")?,
        url: row.get("url")?,
        time_created: row.get("time_created")?,
        time_updated: row.get("time_updated")?,
    })
}

fn account_from_row(row: &Row<'_>) -> Result<Account, CoreError> {
    Ok(Account {
        id: row.get("id")?,
        email: row.get("email")?,
        url: row.get("url")?,
        access_token: row.get("access_token")?,
        refresh_token: row.get("refresh_token")?,
        token_expiry: row.get("token_expiry")?,
        time_created: row.get("time_created")?,
        time_updated: row.get("time_updated")?,
    })
}

fn account_state_from_row(row: &Row<'_>) -> Result<AccountState, CoreError> {
    Ok(AccountState {
        id: row.get("id")?,
        active_account_id: row.get("active_account_id")?,
        active_org_id: row.get("active_org_id")?,
    })
}

fn control_account_from_row(row: &Row<'_>) -> Result<ControlAccount, CoreError> {
    Ok(ControlAccount {
        email: row.get("email")?,
        url: row.get("url")?,
        access_token: row.get("access_token")?,
        refresh_token: row.get("refresh_token")?,
        token_expiry: row.get("token_expiry")?,
        active: row.get("active")?,
        time_created: row.get("time_created")?,
        time_updated: row.get("time_updated")?,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    use crate::storage::test_support::TempDir;

    #[test]
    fn schema_sql_statement_shape() {
        // Byte-compare sqlite_master against SCHEMA_SQL (spec §7). The
        // journal table (created afterwards by `migration::apply`) is
        // excluded.
        let temp = TempDir::new("schema");
        let storage = crate::storage::Storage::open(temp.path().join("db.sqlite")).unwrap();
        let actual = storage
            .with_connection(|conn| -> rusqlite::Result<Vec<String>> {
                let mut stmt = conn.prepare(
                    "SELECT sql FROM sqlite_master WHERE sql IS NOT NULL AND name != 'migration'",
                )?;
                let rows = stmt.query_map([], |row| row.get::<_, String>(0))?;
                rows.collect::<rusqlite::Result<Vec<_>>>()
            })
            .unwrap();
        // `sqlite_master` rows appear in creation order, so this also pins
        // the statement order.
        assert_eq!(actual, SCHEMA_SQL.to_vec());
    }

    /// Regression (battle test): the row mappers bind columns by NAME
    /// because existing databases created by the TS app can carry a
    /// different column order than `SCHEMA_SQL` (e.g. `opencode.db`
    /// appends `icon_url_override` last). A positional mapper reads
    /// `time_created` (INTEGER) into `icon_color` (TEXT) and fails with
    /// `Invalid column type Integer`.
    #[test]
    fn reads_rows_with_reordered_columns() {
        let temp = TempDir::new("reordered");
        let path = temp.path().join("db.sqlite");
        let storage = Storage::open(path).unwrap();
        // Swap `project` for a legacy-order copy — `opencode.db` ships
        // `icon_url_override` last.
        storage
            .with_connection(|conn| {
                conn.execute_batch(
                    "DROP TABLE project;
                     CREATE TABLE project (
                        id TEXT PRIMARY KEY NOT NULL,
                        worktree TEXT NOT NULL,
                        vcs TEXT,
                        name TEXT,
                        icon_url TEXT,
                        icon_color TEXT,
                        time_created INTEGER NOT NULL,
                        time_updated INTEGER NOT NULL,
                        time_initialized INTEGER,
                        sandboxes TEXT NOT NULL,
                        commands TEXT,
                        icon_url_override TEXT
                    );
                     INSERT INTO project (id, worktree, time_created, time_updated, sandboxes, icon_url_override)
                     VALUES ('p1', '/wt', 123, 456, '{}', 'over');",
                )?;
                Ok::<(), CoreError>(())
            })
            .unwrap();
        let project = storage
            .get_project("p1")
            .expect("project row reads under legacy column order");
        let Some(project) = project else {
            panic!("project row missing");
        };
        assert_eq!(project.time_created, 123);
        assert_eq!(project.time_updated, 456);
        assert_eq!(project.icon_url_override.as_deref(), Some("over"));
        assert_eq!(project.icon_color, None);
    }

    #[test]
    fn round_trips_each_table() {
        let temp = TempDir::new("roundtrip");
        let storage = Storage::open(temp.path().join("db.sqlite")).unwrap();

        let project = Project {
            id: "p1".into(),
            worktree: "/wt".into(),
            vcs: Some("git".into()),
            name: Some("proj".into()),
            icon_url: None,
            icon_url_override: Some("override".into()),
            icon_color: None,
            time_created: 1,
            time_updated: 2,
            time_initialized: Some(3),
            sandboxes: serde_json::json!({"a": [1, 2]}),
            commands: Some("cmd".into()),
        };
        storage.put_project(&project).unwrap();
        assert_eq!(storage.get_project("p1").unwrap(), Some(project.clone()));
        assert_eq!(storage.list_projects().unwrap(), vec![project.clone()]);

        let workspace = Workspace {
            id: "w1".into(),
            r#type: "local".into(),
            name: String::new(),
            branch: Some("main".into()),
            directory: Some("/wt".into()),
            extra: None,
            project_id: "p1".into(),
            time_used: 4,
        };
        storage.put_workspace(&workspace).unwrap();
        assert_eq!(
            storage.get_workspace("w1").unwrap(),
            Some(workspace.clone())
        );
        assert_eq!(storage.list_workspaces().unwrap(), vec![workspace]);

        let session = Session {
            id: "s1".into(),
            project_id: "p1".into(),
            workspace_id: Some("w1".into()),
            parent_id: None,
            slug: "slug".into(),
            directory: "/wt".into(),
            path: Some("/wt/s1".into()),
            title: "Title".into(),
            version: "1".into(),
            share_url: None,
            summary_additions: Some(10),
            summary_deletions: Some(20),
            summary_files: Some(30),
            summary_diffs: Some(serde_json::json!({"f": "x"})),
            metadata: Some(serde_json::json!({"m": 1})),
            cost: 1.5,
            tokens_input: 100,
            tokens_output: 200,
            tokens_reasoning: 300,
            tokens_cache_read: 400,
            tokens_cache_write: 500,
            revert: None,
            permission: Some(serde_json::json!({"bash": "allow"})),
            agent: Some("build".into()),
            model: Some("anthropic/claude".into()),
            time_created: 5,
            time_updated: 6,
            time_compacting: None,
            time_archived: Some(7),
        };
        storage.put_session(&session).unwrap();
        assert_eq!(storage.get_session("s1").unwrap(), Some(session.clone()));
        assert_eq!(storage.list_sessions().unwrap(), vec![session]);

        let message = Message {
            id: "m1".into(),
            session_id: "s1".into(),
            time_created: 8,
            time_updated: 9,
            data: serde_json::json!({"role": "user"}),
        };
        storage.put_message(&message).unwrap();
        assert_eq!(storage.get_message("m1").unwrap(), Some(message.clone()));
        assert_eq!(storage.list_messages("s1").unwrap(), vec![message]);

        let part = Part {
            id: "part1".into(),
            message_id: "m1".into(),
            session_id: "s1".into(),
            time_created: 10,
            time_updated: 11,
            data: serde_json::json!({"type": "text"}),
        };
        storage.put_part(&part).unwrap();
        assert_eq!(storage.get_part("part1").unwrap(), Some(part.clone()));
        assert_eq!(storage.list_parts("m1").unwrap(), vec![part]);

        let todo = Todo {
            session_id: "s1".into(),
            content: "do it".into(),
            status: "pending".into(),
            priority: "high".into(),
            position: 0,
            time_created: 12,
            time_updated: 13,
        };
        storage.put_todo(&todo).unwrap();
        assert_eq!(storage.get_todo("s1", 0).unwrap(), Some(todo.clone()));
        assert_eq!(storage.list_todos("s1").unwrap(), vec![todo]);

        let share = SessionShare {
            session_id: "s1".into(),
            id: "share1".into(),
            secret: "s3cret".into(),
            url: "https://share".into(),
            time_created: 14,
            time_updated: 15,
        };
        storage.put_session_share(&share).unwrap();
        assert_eq!(
            storage.get_session_share("s1").unwrap(),
            Some(share.clone())
        );

        let account = Account {
            id: "a1".into(),
            email: "a@b.c".into(),
            url: "https://acct".into(),
            access_token: "at".into(),
            refresh_token: "rt".into(),
            token_expiry: Some(16),
            time_created: 17,
            time_updated: 18,
        };
        storage.put_account(&account).unwrap();
        assert_eq!(storage.get_account("a1").unwrap(), Some(account.clone()));
        assert_eq!(storage.list_accounts().unwrap(), vec![account]);

        let state = AccountState {
            id: None,
            active_account_id: Some("a1".into()),
            active_org_id: None,
        };
        let state_id = storage.put_account_state(&state).unwrap();
        assert_eq!(
            storage.get_account_state(state_id).unwrap(),
            Some(AccountState {
                id: Some(state_id),
                active_account_id: Some("a1".into()),
                active_org_id: None,
            })
        );
        assert_eq!(storage.list_account_states().unwrap().len(), 1);

        let control = ControlAccount {
            email: "a@b.c".into(),
            url: "https://acct".into(),
            access_token: "at".into(),
            refresh_token: "rt".into(),
            token_expiry: None,
            active: 1,
            time_created: 19,
            time_updated: 20,
        };
        storage.put_control_account(&control).unwrap();
        assert_eq!(
            storage
                .get_control_account("a@b.c", "https://acct")
                .unwrap(),
            Some(control.clone())
        );
        assert_eq!(storage.list_control_accounts().unwrap(), vec![control]);
    }

    #[test]
    fn json_columns_are_compact() {
        let temp = TempDir::new("json-compact");
        let storage = Storage::open(temp.path().join("db.sqlite")).unwrap();
        storage
            .put_project(&Project {
                id: "p1".into(),
                worktree: "/wt".into(),
                vcs: None,
                name: None,
                icon_url: None,
                icon_url_override: None,
                icon_color: None,
                time_created: 1,
                time_updated: 2,
                time_initialized: None,
                sandboxes: serde_json::json!({"a": 1, "b": [2, 3]}),
                commands: None,
            })
            .unwrap();
        let stored = storage
            .with_connection(|conn| {
                conn.query_row("SELECT sandboxes FROM project WHERE id = 'p1'", [], |row| {
                    row.get::<_, String>(0)
                })
            })
            .unwrap();
        assert_eq!(stored, r#"{"a":1,"b":[2,3]}"#);
    }

    #[test]
    fn deleting_session_cascades() {
        let temp = TempDir::new("cascade");
        let storage = Storage::open(temp.path().join("db.sqlite")).unwrap();
        storage
            .put_project(&Project {
                id: "p1".into(),
                worktree: "/wt".into(),
                vcs: None,
                name: None,
                icon_url: None,
                icon_url_override: None,
                icon_color: None,
                time_created: 1,
                time_updated: 2,
                time_initialized: None,
                sandboxes: Value::Null,
                commands: None,
            })
            .unwrap();
        storage
            .put_session(&Session {
                id: "s1".into(),
                project_id: "p1".into(),
                workspace_id: None,
                parent_id: None,
                slug: "slug".into(),
                directory: "/wt".into(),
                path: None,
                title: "Title".into(),
                version: "1".into(),
                share_url: None,
                summary_additions: None,
                summary_deletions: None,
                summary_files: None,
                summary_diffs: None,
                metadata: None,
                cost: 0.0,
                tokens_input: 0,
                tokens_output: 0,
                tokens_reasoning: 0,
                tokens_cache_read: 0,
                tokens_cache_write: 0,
                revert: None,
                permission: None,
                agent: None,
                model: None,
                time_created: 1,
                time_updated: 2,
                time_compacting: None,
                time_archived: None,
            })
            .unwrap();
        storage
            .put_message(&Message {
                id: "m1".into(),
                session_id: "s1".into(),
                time_created: 1,
                time_updated: 2,
                data: Value::Null,
            })
            .unwrap();
        storage
            .put_part(&Part {
                id: "part1".into(),
                message_id: "m1".into(),
                session_id: "s1".into(),
                time_created: 1,
                time_updated: 2,
                data: Value::Null,
            })
            .unwrap();
        storage
            .put_todo(&Todo {
                session_id: "s1".into(),
                content: "x".into(),
                status: "pending".into(),
                priority: "low".into(),
                position: 0,
                time_created: 1,
                time_updated: 2,
            })
            .unwrap();
        storage
            .put_session_share(&SessionShare {
                session_id: "s1".into(),
                id: "shr".into(),
                secret: "s".into(),
                url: "u".into(),
                time_created: 1,
                time_updated: 2,
            })
            .unwrap();
        storage
            .with_connection(|conn| {
                conn.execute(
                    "INSERT INTO event_sequence (aggregate_id, seq) VALUES ('s1', 0)",
                    [],
                )?;
                conn.execute(
                    "INSERT INTO event (id, aggregate_id, seq, type, data) VALUES ('e1', 's1', 0, 't', '{}')",
                    [],
                )
            })
            .unwrap();

        storage.delete_session("s1").unwrap();
        assert_eq!(storage.get_session("s1").unwrap(), None);
        let count = |table: &str| {
            storage
                .with_connection(|conn| {
                    conn.query_row(&format!("SELECT COUNT(*) FROM {table}"), [], |row| {
                        row.get::<_, i64>(0)
                    })
                })
                .unwrap()
        };
        assert_eq!(count("message"), 0);
        assert_eq!(count("part"), 0);
        assert_eq!(count("todo"), 0);
        assert_eq!(count("session_share"), 0);

        // `event` rows cascade through `event_sequence`, not `session`
        // (that is the M3.6 event bus's `remove` path).
        assert_eq!(count("event"), 1);
        storage
            .with_connection(|conn| {
                conn.execute("DELETE FROM event_sequence WHERE aggregate_id = 's1'", [])
            })
            .unwrap();
        assert_eq!(count("event"), 0);
    }
}
