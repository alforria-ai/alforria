//! `todowrite` tool — port of `tool/todo.ts` + `session/todo.ts` (spec M4.6).
//!
//! [`Todo`] is the `Todo.Service` seam. The M4 default ([`TodoService`]) is
//! db-backed: `update` is a transactional delete-all + insert with `position`
//! on the M3 storage `todo` table (session/todo.ts:29-51), then publishes the
//! non-durable `todo.updated` event through the EventV2 bridge.

use std::sync::Arc;

use serde::Deserialize;
use serde_json::{json, Value};

use opencode_schema::session_todo::{TodoInfo, TodoUpdatedData};

use crate::event::bus::{EventBus, PublishOptions};
use crate::event::definition::Definition;
use crate::storage::Storage;
use crate::tool::def::{define, Agents, AskRequest, BoxFuture, ExecuteResult, ToolCtxRef, ToolDef};
use crate::tool::error::ToolError;
use crate::tool::truncate::Truncate;

/// `SessionTodo.Event.Updated` — `define({ type: "todo.updated", … })`
/// without a `durable` block (session-todo.ts:15-18): non-durable.
pub const TODO_UPDATED: Definition = Definition {
    r#type: "todo.updated",
    durable: None,
};

/// `Todo.Service` (session/todo.ts:16-19).
pub trait Todo: Send + Sync {
    /// Transactional delete-all + insert with `position` (todo.ts:29-51),
    /// then publish `todo.updated`.
    fn update<'a>(
        &'a self,
        session_id: &'a str,
        todos: &'a [TodoInfo],
    ) -> BoxFuture<'a, Result<(), ToolError>>;

    /// Todos of a session ordered by `position` (todo.ts:53-66).
    fn get<'a>(&'a self, session_id: &'a str) -> BoxFuture<'a, Result<Vec<TodoInfo>, ToolError>>;
}

#[derive(Debug, Deserialize)]
pub struct TodoWriteParameters {
    pub todos: Vec<TodoInfo>,
}

/// Hand-authored JSON Schema (byte-matches the Effect-generated schema; see
/// `tests/golden/toolschema/todowrite.json`).
pub fn parameters() -> Value {
    json!({
        "$schema": "https://json-schema.org/draft/2020-12/schema",
        "type": "object",
        "properties": {
            "todos": {
                "type": "array",
                "items": {
                    "type": "object",
                    "properties": {
                        "content": {
                            "type": "string",
                            "description": "Brief description of the task"
                        },
                        "status": {
                            "type": "string",
                            "description": "Current status of the task: pending, in_progress, completed, cancelled"
                        },
                        "priority": {
                            "type": "string",
                            "description": "Priority level of the task: high, medium, low"
                        }
                    },
                    "required": [
                        "content",
                        "status",
                        "priority"
                    ]
                },
                "description": "The updated todo list"
            }
        },
        "required": [
            "todos"
        ]
    })
}

/// Build the `todowrite` tool.
pub fn todo_tool(
    truncate: Arc<dyn Truncate>,
    agents: Arc<dyn Agents>,
    todo: Arc<dyn Todo>,
) -> ToolDef {
    define(
        "todowrite",
        include_str!("txt/todowrite.txt"),
        parameters(),
        None,
        truncate,
        agents,
        move |params: TodoWriteParameters, ctx: ToolCtxRef<'_>| run(params, ctx, Arc::clone(&todo)),
    )
}

fn run(
    params: TodoWriteParameters,
    ctx: ToolCtxRef<'_>,
    todo: Arc<dyn Todo>,
) -> BoxFuture<'_, Result<ExecuteResult, ToolError>> {
    Box::pin(async move {
        ctx.ask
            .ask(AskRequest {
                permission: "todowrite".to_string(),
                patterns: vec!["*".to_string()],
                always: vec!["*".to_string()],
                metadata: json!({}),
            })
            .await?;

        todo.update(ctx.session_id, &params.todos).await?;

        let open = params
            .todos
            .iter()
            .filter(|todo| todo.status != "completed")
            .count();
        Ok(ExecuteResult {
            title: format!("{open} todos"),
            // JSON.stringify(todos, null, 2)
            output: serde_json::to_string_pretty(&params.todos)
                .map_err(|err| ToolError::Failed(err.to_string()))?,
            metadata: json!({ "todos": params.todos }),
            attachments: None,
        })
    })
}

/// The db-backed `Todo.Service` default (session/todo.ts:23-70): M3 storage
/// `todo` table + EventV2 bridge.
pub struct TodoService {
    storage: std::sync::Arc<Storage>,
    events: Option<Arc<EventBus>>,
}

impl TodoService {
    pub fn new(storage: std::sync::Arc<Storage>) -> TodoService {
        TodoService {
            storage,
            events: None,
        }
    }

    /// Publish `todo.updated` through the EventV2 bridge.
    pub fn with_events(mut self, events: Arc<EventBus>) -> TodoService {
        self.events = Some(events);
        self
    }
}

fn core_error(err: impl Into<crate::CoreError>) -> ToolError {
    ToolError::Failed(err.into().to_string())
}

impl Todo for TodoService {
    fn update<'a>(
        &'a self,
        session_id: &'a str,
        todos: &'a [TodoInfo],
    ) -> BoxFuture<'a, Result<(), ToolError>> {
        Box::pin(async move {
            self.storage
                .with_connection_mut(|conn| {
                    let tx = conn.transaction().map_err(core_error)?;
                    tx.execute("DELETE FROM todo WHERE session_id = ?1", [session_id])
                        .map_err(core_error)?;
                    let now = chrono::Utc::now().timestamp_millis();
                    for (position, todo) in todos.iter().enumerate() {
                        tx.execute(
                            "INSERT INTO todo (session_id, content, status, priority, position, time_created, time_updated)
                             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
                            rusqlite::params![
                                session_id,
                                todo.content,
                                todo.status,
                                todo.priority,
                                position as i64,
                                now,
                                now,
                            ],
                        )
                        .map_err(core_error)?;
                    }
                    tx.commit().map_err(core_error)
                })?;
            if let Some(events) = &self.events {
                let data = serde_json::to_value(TodoUpdatedData {
                    session_id: session_id.to_string(),
                    todos: todos.to_vec(),
                })
                .map_err(|err| ToolError::Failed(err.to_string()))?;
                events
                    .publish(&TODO_UPDATED, data, PublishOptions::default())
                    .map_err(core_error)?;
            }
            Ok(())
        })
    }

    fn get<'a>(&'a self, session_id: &'a str) -> BoxFuture<'a, Result<Vec<TodoInfo>, ToolError>> {
        Box::pin(async move {
            let rows = self.storage.list_todos(session_id).map_err(core_error)?;
            Ok(rows
                .into_iter()
                .map(|row| TodoInfo {
                    content: row.content,
                    status: row.status,
                    priority: row.priority,
                })
                .collect())
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::event::EventBus;
    use crate::storage::schema::{Project, Session};
    use crate::storage::test_support::TempDir;
    use crate::tool::def::Extra;
    use crate::tool::ripgrep::test_support::{ctx, fixed_agents, instance, RecordingAsk};
    use crate::tool::truncate::TruncateService;
    use serde_json::json;

    struct Setup {
        service: TodoService,
        bus: Arc<EventBus>,
        dir: TempDir,
    }

    /// A tempdir DB with the project/session rows the `todo` FK requires,
    /// plus an EventV2 bus subscribed to `todo.updated`.
    fn setup(tag: &str) -> Setup {
        let dir = TempDir::new(tag);
        let storage = Storage::open(dir.path().join("db.sqlite")).unwrap();
        storage
            .put_project(&Project {
                id: "p1".to_string(),
                worktree: "/wt".to_string(),
                vcs: None,
                name: None,
                icon_url: None,
                icon_url_override: None,
                icon_color: None,
                time_created: 1,
                time_updated: 2,
                time_initialized: None,
                sandboxes: json!({}),
                commands: None,
            })
            .unwrap();
        storage
            .put_session(&Session {
                id: "ses_1".to_string(),
                project_id: "p1".to_string(),
                workspace_id: None,
                parent_id: None,
                slug: "slug".to_string(),
                directory: "/wt".to_string(),
                path: None,
                title: "Title".to_string(),
                version: "1".to_string(),
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
        // The bus owns its own connection to the same database file.
        let bus_storage = Storage::open(dir.path().join("db.sqlite")).unwrap();
        let bus = Arc::new(EventBus::new(bus_storage, None));
        let service = TodoService::new(Arc::new(storage)).with_events(Arc::clone(&bus));
        Setup { service, bus, dir }
    }

    fn tool(service: Arc<dyn Todo>, dir: &std::path::Path) -> ToolDef {
        todo_tool(
            Arc::new(TruncateService::default_limits(dir.join("tool-output"))),
            fixed_agents(),
            service,
        )
    }

    #[test]
    fn golden_parameters_schema() {
        let golden = std::fs::read_to_string(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/tests/golden/toolschema/todowrite.json"
        ))
        .unwrap();
        let golden: Value = serde_json::from_str(&golden).unwrap();
        assert_eq!(parameters(), golden);
    }

    #[tokio::test]
    async fn round_trip_on_tempdir_db_and_publishes_event() {
        let setup = setup("todo-roundtrip");
        let mut events = setup.bus.subscribe("todo.updated");
        let service: Arc<dyn Todo> = Arc::new(setup.service);
        let def = tool(Arc::clone(&service), setup.dir.path());
        let ask = RecordingAsk::new();
        let inst = instance(setup.dir.path());
        let extra = Extra::default();
        let ctx = ctx(&ask, &inst, &extra);

        let args = json!({
            "todos": [
                { "content": "first", "status": "pending", "priority": "high" },
                { "content": "second", "status": "completed", "priority": "low" },
                { "content": "third", "status": "in_progress", "priority": "medium" },
            ]
        });
        let result = (def.execute)(args, ctx).await.unwrap();

        // ask shape (todo.ts:24-29)
        let requests = ask.requests();
        assert_eq!(requests.len(), 1);
        assert_eq!(requests[0].permission, "todowrite");
        assert_eq!(requests[0].patterns, vec!["*".to_string()]);
        assert_eq!(requests[0].always, vec!["*".to_string()]);
        assert_eq!(requests[0].metadata, json!({}));

        // title counts the non-completed todos.
        assert_eq!(result.title, "2 todos");
        // output is the exact 2-space pretty JSON of the todos, input order.
        assert_eq!(
            result.output,
            "[\n  {\n    \"content\": \"first\",\n    \"status\": \"pending\",\n    \"priority\": \"high\"\n  },\n  {\n    \"content\": \"second\",\n    \"status\": \"completed\",\n    \"priority\": \"low\"\n  },\n  {\n    \"content\": \"third\",\n    \"status\": \"in_progress\",\n    \"priority\": \"medium\"\n  }\n]"
        );
        assert_eq!(
            result.metadata["todos"],
            json!([
                { "content": "first", "status": "pending", "priority": "high" },
                { "content": "second", "status": "completed", "priority": "low" },
                { "content": "third", "status": "in_progress", "priority": "medium" },
            ])
        );

        // Rows landed in the todo table in input order.
        let todos = service.get("ses_1").await.unwrap();
        assert_eq!(todos.len(), 3);
        assert_eq!(todos[0].content, "first");
        assert_eq!(todos[2].status, "in_progress");

        // todo.updated published with the sessionID + todos payload.
        let event = events.try_recv().unwrap();
        assert_eq!(event.r#type, "todo.updated");
        assert_eq!(
            event.data,
            json!({
                "sessionID": "ses_1",
                "todos": [
                    { "content": "first", "status": "pending", "priority": "high" },
                    { "content": "second", "status": "completed", "priority": "low" },
                    { "content": "third", "status": "in_progress", "priority": "medium" },
                ]
            })
        );
    }

    #[tokio::test]
    async fn update_replaces_all_previous_rows() {
        let setup = setup("todo-replace");
        let service: Arc<dyn Todo> = Arc::new(setup.service);
        let def = tool(Arc::clone(&service), setup.dir.path());
        let ask = RecordingAsk::new();
        let inst = instance(setup.dir.path());
        let extra = Extra::default();

        (def.execute)(
            json!({ "todos": [
                { "content": "a", "status": "pending", "priority": "low" },
                { "content": "b", "status": "pending", "priority": "low" },
            ] }),
            ctx(&ask, &inst, &extra),
        )
        .await
        .unwrap();
        let todos = service.get("ses_1").await.unwrap();
        assert_eq!(todos.len(), 2);

        (def.execute)(
            json!({ "todos": [
                { "content": "c", "status": "pending", "priority": "low" },
            ] }),
            ctx(&ask, &inst, &extra),
        )
        .await
        .unwrap();
        let todos = service.get("ses_1").await.unwrap();
        assert_eq!(
            todos,
            vec![TodoInfo {
                content: "c".to_string(),
                status: "pending".to_string(),
                priority: "low".to_string(),
            }]
        );

        // Empty list: title "0 todos", output "[]", DB emptied.
        let result = (def.execute)(json!({ "todos": [] }), ctx(&ask, &inst, &extra))
            .await
            .unwrap();
        assert_eq!(result.title, "0 todos");
        assert_eq!(result.output, "[]");
        assert_eq!(service.get("ses_1").await.unwrap(), Vec::new());
    }
}
