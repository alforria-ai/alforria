//! LSP service — port of `lsp/lsp.ts` (the M4 `Lsp`/`LspServer`/`ReadLsp`
//! seam fill).

pub mod client;
pub mod connection;
pub mod language;
pub mod launch;
pub mod server;

use std::collections::{BTreeMap, HashSet};
use std::path::PathBuf;
use std::sync::Arc;

use serde_json::{json, Value};

use crate::config::schema::{LspEntry, LspInfo};
use crate::event::bus::PublishOptions;
use crate::event::Definition;
use crate::lsp::client::{CreateInput, LspClient, WaitMode, WaitRequest};
use crate::lsp::server::{builtin_servers, Flags, ServerContext, ServerInfo};
use crate::paths::GlobalPaths;
use crate::tool::def::BoxFuture;
use crate::tool::lsp::{LspServer, Position};

pub use crate::lsp::server::Handle;

/// `Event.Updated` — the `lsp.updated` event (`@opencode-ai/schema/lsp-event`).
pub const LSP_UPDATED: Definition = Definition::ephemeral("lsp.updated");

pub struct LspInput {
    pub lsp: Option<LspInfo>,
    pub directory: PathBuf,
    pub worktree: PathBuf,
    pub paths: GlobalPaths,
    pub events: Option<Arc<crate::event::bus::EventBus>>,
    pub flags: Flags,
}

struct ServiceState {
    clients: Vec<Arc<LspClient>>,
    broken: HashSet<String>,
}

/// The `LSP.Interface` service (`lsp.ts:119-133`) behind one instance.
pub struct LspService {
    input: LspInput,
    servers: Vec<ServerInfo>,
    server_ctx: Arc<ServerContext>,
    state: std::sync::Mutex<ServiceState>,
    /// The in-flight spawn dedup (`spawning`, lsp.ts:116) — concurrent
    /// callers share one spawn section.
    spawn_lock: tokio::sync::Mutex<()>,
}

fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or_default()
}

fn file_extension(file: &str) -> String {
    let path = PathBuf::from(file);
    path.extension()
        .map(|ext| format!(".{}", ext.to_string_lossy()))
        .unwrap_or_else(|| file.to_string())
}

/// Node `path.relative(from, to)` (POSIX).
fn path_relative(from: &std::path::Path, to: &std::path::Path) -> String {
    let from: Vec<_> = from.components().collect();
    let to: Vec<_> = to.components().collect();
    let mut index = 0;
    while index < from.len() && index < to.len() && from[index] == to[index] {
        index += 1;
    }
    let mut parts: Vec<String> = Vec::new();
    for _ in &from[index..] {
        parts.push("..".to_string());
    }
    for part in &to[index..] {
        parts.push(part.as_os_str().to_string_lossy().into_owned());
    }
    if parts.is_empty() {
        return String::new();
    }
    parts.join("/")
}

fn contains_path(file: &str, directory: &std::path::Path, worktree: &std::path::Path) -> bool {
    let file = std::path::Path::new(file);
    if file.strip_prefix(directory).is_ok() {
        return true;
    }
    if worktree == std::path::Path::new("/") {
        return false;
    }
    file.strip_prefix(worktree).is_ok()
}

/// `.flat().filter(Boolean)` — flatten one level, drop nulls.
fn flatten_results(results: Vec<Value>) -> Vec<Value> {
    results
        .into_iter()
        .flat_map(|result| match result {
            Value::Array(items) => items,
            Value::Null => Vec::new(),
            other => vec![other],
        })
        .filter(|result| !result.is_null())
        .collect()
}

/// The `kinds` whitelist (`lsp.ts:87-96`): Class, Function, Method,
/// Interface, Variable, Constant, Struct, Enum.
const SYMBOL_KINDS: [i64; 8] = [5, 12, 6, 11, 13, 14, 23, 10];

impl LspService {
    /// Build the service: the server registry from `cfg.lsp`
    /// (`lsp.ts:145-189`).
    pub fn new(input: LspInput) -> Arc<LspService> {
        let mut servers: Vec<ServerInfo> = Vec::new();
        if input.lsp.is_some() {
            servers = builtin_servers();
            // `filterExperimentalServers` (lsp.ts:98-108).
            if input.flags.experimental_lsp_ty {
                servers.retain(|server| server.id != "pyright");
            } else {
                servers.retain(|server| server.id != "ty");
            }
            if let Some(LspInfo::Entries(entries)) = input.lsp.as_ref() {
                for (name, entry) in entries {
                    let existing = servers
                        .iter()
                        .position(|server| server.id == *name)
                        .map(|index| servers[index].clone());
                    match entry {
                        LspEntry::Disabled { .. } => {
                            servers.retain(|server| server.id != *name);
                            continue;
                        }
                        LspEntry::Server {
                            command,
                            extensions,
                            disabled,
                            env,
                            initialization,
                        } => {
                            if *disabled == Some(true) {
                                servers.retain(|server| server.id != *name);
                                continue;
                            }
                            let extensions = extensions
                                .clone()
                                .or_else(|| {
                                    existing.as_ref().map(|server| server.extensions.clone())
                                })
                                .unwrap_or_default();
                            let root = existing
                                .as_ref()
                                .map(|server| server.root.clone())
                                .unwrap_or_else(|| {
                                    Arc::new(|_file: &str, ctx: Arc<ServerContext>| {
                                        Box::pin(async move { Some(ctx.directory.clone()) })
                                            as BoxFuture<'static, Option<PathBuf>>
                                    })
                                        as Arc<
                                            dyn Fn(
                                                    &str,
                                                    Arc<ServerContext>,
                                                )
                                                    -> BoxFuture<'static, Option<PathBuf>>
                                                + Send
                                                + Sync,
                                        >
                                });
                            let command = command.clone();
                            let env = env.clone();
                            let initialization = initialization.clone();
                            let spawn = Arc::new(move |root: PathBuf, _ctx: Arc<ServerContext>| {
                                let command = command.clone();
                                let env = env.clone();
                                let initialization = initialization.clone();
                                Box::pin(async move {
                                    let mut args = command.clone();
                                    let cmd = args.remove(0);
                                    let mut handle =
                                        launch::spawn(&cmd, &args, &root, env.as_ref()).ok()?;
                                    handle.initialization =
                                        initialization.clone().map(|map| map.into_iter().collect());
                                    Some(handle)
                                })
                                    as BoxFuture<'static, Option<Handle>>
                            });
                            let merged = ServerInfo {
                                id: name.clone(),
                                extensions,
                                root,
                                spawn,
                            };
                            match existing {
                                Some(_) => {
                                    if let Some(index) =
                                        servers.iter().position(|server| server.id == *name)
                                    {
                                        servers[index] = merged;
                                    }
                                }
                                None => servers.push(merged),
                            }
                        }
                    }
                }
            }
        }
        let server_ctx = Arc::new(ServerContext {
            directory: input.directory.clone(),
            worktree: input.worktree.clone(),
            bin: input.paths.cache.join("bin"),
            flags: input.flags,
        });
        Arc::new(LspService {
            input,
            servers,
            server_ctx,
            state: std::sync::Mutex::new(ServiceState {
                clients: Vec::new(),
                broken: HashSet::new(),
            }),
            spawn_lock: tokio::sync::Mutex::new(()),
        })
    }

    /// `getClients` (lsp.ts:208-297).
    async fn get_clients(&self, file: &str) -> Vec<Arc<LspClient>> {
        if !contains_path(file, &self.input.directory, &self.input.worktree) {
            return Vec::new();
        }
        let extension = file_extension(file);
        let _guard = self.spawn_lock.lock().await;
        let mut updated = 0usize;
        let mut result: Vec<Arc<LspClient>> = Vec::new();
        for server in &self.servers {
            if !server.extensions.is_empty() && !server.extensions.contains(&extension) {
                continue;
            }
            let Some(root) = (server.root)(file, Arc::clone(&self.server_ctx)).await else {
                continue;
            };
            let key = format!("{}{}", root.to_string_lossy(), server.id);
            if self.lock_state().broken.contains(&key) {
                continue;
            }
            let existing = self
                .lock_state()
                .clients
                .iter()
                .find(|client| client.root() == root && client.server_id() == server.id)
                .cloned();
            if let Some(client) = existing {
                result.push(client);
                continue;
            }
            let handle = (server.spawn)(root.clone(), Arc::clone(&self.server_ctx)).await;
            let Some(handle) = handle else {
                self.lock_state().broken.insert(key);
                continue;
            };
            let client = LspClient::create(CreateInput {
                server_id: server.id.clone(),
                child: handle.child,
                initialization: handle.initialization.map(Value::Object),
                root: root.clone(),
                directory: self.input.directory.clone(),
            })
            .await;
            match client {
                Err(_) => {
                    self.lock_state().broken.insert(key);
                    continue;
                }
                Ok(client) => {
                    let existing = self
                        .lock_state()
                        .clients
                        .iter()
                        .find(|other| other.root() == root && other.server_id() == server.id)
                        .cloned();
                    if let Some(existing) = existing {
                        client.shutdown().await;
                        result.push(existing);
                        continue;
                    }
                    self.lock_state().clients.push(client.clone());
                    result.push(client);
                    updated += 1;
                }
            }
        }
        if let Some(events) = &self.input.events {
            for _ in 0..updated {
                let _ = events.publish(&LSP_UPDATED, json!({}), PublishOptions::default());
            }
        }
        result
    }

    async fn run_all<T>(&self, f: impl Fn(Arc<LspClient>) -> BoxFuture<'static, T>) -> Vec<T> {
        let clients = self.lock_state().clients.clone();
        let futures: Vec<BoxFuture<'static, T>> = clients.into_iter().map(f).collect();
        futures::future::join_all(futures).await
    }

    async fn run<T>(
        &self,
        file: &str,
        f: impl Fn(Arc<LspClient>) -> BoxFuture<'static, T>,
    ) -> Vec<T> {
        let clients = self.get_clients(file).await;
        let futures: Vec<BoxFuture<'static, T>> = clients.into_iter().map(f).collect();
        futures::future::join_all(futures).await
    }

    /// `status` (lsp.ts:313-326) — the `GET /lsp` payload.
    pub fn status(&self) -> Vec<Value> {
        let directory = &self.input.directory;
        self.lock_state()
            .clients
            .iter()
            .map(|client| {
                let root = path_relative(directory, client.root());
                json!({
                    "id": client.server_id(),
                    "name": client.server_id(),
                    "root": root,
                    "status": "connected",
                })
            })
            .collect()
    }

    /// `hasClients` (lsp.ts:328-342).
    pub async fn has_clients(&self, file: &str) -> bool {
        let extension = file_extension(file);
        for server in &self.servers {
            if !server.extensions.is_empty() && !server.extensions.contains(&extension) {
                continue;
            }
            let Some(root) = (server.root)(file, Arc::clone(&self.server_ctx)).await else {
                continue;
            };
            let key = format!("{}{}", root.to_string_lossy(), server.id);
            if self.lock_state().broken.contains(&key) {
                continue;
            }
            return true;
        }
        false
    }

    /// `touchFile` (lsp.ts:344-362).
    pub async fn touch_file(&self, file: &str, diagnostics: Option<WaitMode>) {
        let clients = self.get_clients(file).await;
        let futures: Vec<BoxFuture<'static, ()>> = clients
            .into_iter()
            .map(|client| {
                let file = file.to_string();
                Box::pin(async move {
                    let after = now_ms();
                    let version = client.notify_open(&file).await;
                    let Some(mode) = diagnostics else {
                        return;
                    };
                    client
                        .wait_for_diagnostics(WaitRequest {
                            path: file.clone(),
                            version,
                            mode: Some(mode),
                            after: Some(after),
                        })
                        .await;
                }) as BoxFuture<'static, ()>
            })
            .collect();
        let _ = futures::future::join_all(futures).await;
    }

    /// `diagnostics` (lsp.ts:364-375) — `Record<file, Diagnostic[]>`
    /// merged across every connected client.
    pub async fn diagnostics_record(&self) -> BTreeMap<String, Vec<Value>> {
        let clients = self.lock_state().clients.clone();
        let futures: Vec<BoxFuture<'static, std::collections::HashMap<String, Vec<Value>>>> =
            clients
                .into_iter()
                .map(|client| {
                    Box::pin(async move {
                        client
                            .diagnostics()
                            .into_iter()
                            .collect::<std::collections::HashMap<_, _>>()
                    }) as _
                })
                .collect();
        let all = futures::future::join_all(futures).await;
        let mut results: BTreeMap<String, Vec<Value>> = BTreeMap::new();
        for result in all {
            for (path, diags) in result {
                results.entry(path).or_default().extend(diags);
            }
        }
        results
    }

    fn lock_state(&self) -> std::sync::MutexGuard<'_, ServiceState> {
        self.state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    fn position_params(position: &Position) -> Value {
        json!({
            "textDocument": {
                "uri": crate::session::prompt_input::path_to_file_url(&PathBuf::from(&position.file)),
            },
            "position": { "line": position.line, "character": position.character },
        })
    }
}

// The `LspServer` seam — the `lsp` tool + the prompt-part documentSymbol
// expansion (lsp.ts Interface surface).
impl LspServer for LspService {
    fn has_clients<'a>(&'a self, file: &'a str) -> BoxFuture<'a, bool> {
        Box::pin(async move { self.has_clients(file).await })
    }

    fn touch_file<'a>(&'a self, file: &'a str) -> BoxFuture<'a, ()> {
        Box::pin(async move {
            self.touch_file(file, None).await;
        })
    }

    fn definition<'a>(&'a self, position: Position) -> BoxFuture<'a, Vec<Value>> {
        Box::pin(async move {
            let results = self
                .run(&position.file, |client| {
                    let params = Self::position_params(&position);
                    Box::pin(async move {
                        client
                            .connection
                            .send_request("textDocument/definition", params)
                            .await
                            .unwrap_or(Value::Null)
                    }) as BoxFuture<'static, Value>
                })
                .await;
            flatten_results(results)
        })
    }

    fn references<'a>(&'a self, position: Position) -> BoxFuture<'a, Vec<Value>> {
        Box::pin(async move {
            let results = self
                .run(&position.file, |client| {
                    let params = json!({
                        "textDocument": {
                            "uri": crate::session::prompt_input::path_to_file_url(&PathBuf::from(&position.file)),
                        },
                        "position": { "line": position.line, "character": position.character },
                        "context": { "includeDeclaration": true },
                    });
                    Box::pin(async move {
                        client
                            .connection
                            .send_request("textDocument/references", params)
                            .await
                            .unwrap_or(json!([]))
                    }) as BoxFuture<'static, Value>
                })
                .await;
            flatten_results(results)
        })
    }

    fn hover<'a>(&'a self, position: Position) -> BoxFuture<'a, Vec<Value>> {
        Box::pin(async move {
            self.run(&position.file, |client| {
                let params = Self::position_params(&position);
                Box::pin(async move {
                    client
                        .connection
                        .send_request("textDocument/hover", params)
                        .await
                        .unwrap_or(Value::Null)
                }) as BoxFuture<'static, Value>
            })
            .await
        })
    }

    fn document_symbol<'a>(&'a self, uri: &'a str) -> BoxFuture<'a, Vec<Value>> {
        Box::pin(async move {
            let file = crate::session::prompt_input::file_url_to_path(uri);
            let results = self
                .run(&file.to_string_lossy(), |client| {
                    let params = json!({ "textDocument": { "uri": uri } });
                    Box::pin(async move {
                        client
                            .connection
                            .send_request("textDocument/documentSymbol", params)
                            .await
                            .unwrap_or(json!([]))
                    }) as BoxFuture<'static, Value>
                })
                .await;
            flatten_results(results)
        })
    }

    fn workspace_symbol<'a>(&'a self, query: &'a str) -> BoxFuture<'a, Vec<Value>> {
        Box::pin(async move {
            let query = query.to_string();
            let results = self
                .run_all(|client| {
                    let params = json!({ "query": query });
                    Box::pin(async move {
                        client
                            .connection
                            .send_request("workspace/symbol", params)
                            .await
                            .map(|result| {
                                result
                                    .as_array()
                                    .cloned()
                                    .unwrap_or_default()
                                    .into_iter()
                                    .filter(|symbol| {
                                        symbol
                                            .get("kind")
                                            .and_then(Value::as_i64)
                                            .is_some_and(|kind| SYMBOL_KINDS.contains(&kind))
                                    })
                                    .take(10)
                                    .collect::<Vec<Value>>()
                            })
                            .unwrap_or_default()
                    }) as BoxFuture<'static, Vec<Value>>
                })
                .await;
            results.into_iter().flatten().collect()
        })
    }

    fn implementation<'a>(&'a self, position: Position) -> BoxFuture<'a, Vec<Value>> {
        Box::pin(async move {
            let results = self
                .run(&position.file, |client| {
                    let params = Self::position_params(&position);
                    Box::pin(async move {
                        client
                            .connection
                            .send_request("textDocument/implementation", params)
                            .await
                            .unwrap_or(Value::Null)
                    }) as BoxFuture<'static, Value>
                })
                .await;
            flatten_results(results)
        })
    }

    fn prepare_call_hierarchy<'a>(&'a self, position: Position) -> BoxFuture<'a, Vec<Value>> {
        Box::pin(async move {
            let results = self
                .run(&position.file, |client| {
                    let params = Self::position_params(&position);
                    Box::pin(async move {
                        client
                            .connection
                            .send_request("textDocument/prepareCallHierarchy", params)
                            .await
                            .unwrap_or(json!([]))
                    }) as BoxFuture<'static, Value>
                })
                .await;
            flatten_results(results)
        })
    }

    fn incoming_calls<'a>(&'a self, position: Position) -> BoxFuture<'a, Vec<Value>> {
        Box::pin(async move { call_hierarchy(self, position, "callHierarchy/incomingCalls").await })
    }

    fn outgoing_calls<'a>(&'a self, position: Position) -> BoxFuture<'a, Vec<Value>> {
        Box::pin(async move { call_hierarchy(self, position, "callHierarchy/outgoingCalls").await })
    }
}

/// `callHierarchyRequest` (lsp.ts:455-470) — prepare, then the direction
/// request against the first item.
async fn call_hierarchy(
    service: &LspService,
    position: Position,
    direction: &'static str,
) -> Vec<Value> {
    let results = service
        .run(&position.file, |client| {
            let params = LspService::position_params(&position);
            Box::pin(async move {
                let items = client
                    .connection
                    .send_request("textDocument/prepareCallHierarchy", params)
                    .await
                    .unwrap_or(json!([]));
                let Some(first) = items.as_array().and_then(|items| items.first().cloned()) else {
                    return json!([]);
                };
                client
                    .connection
                    .send_request(direction, json!({ "item": first }))
                    .await
                    .unwrap_or(json!([]))
            }) as BoxFuture<'static, Value>
        })
        .await;
    flatten_results(results)
}

// The edit/write `Lsp` seam — `touchFile(file, "document")` +
// `diagnostics()` (edit.ts:197-198, write.ts:75-76).
impl crate::tool::edit::Lsp for LspService {
    fn touch_file<'a>(&'a self, file: &'a str) -> BoxFuture<'a, ()> {
        Box::pin(async move {
            self.touch_file(file, Some(WaitMode::Document)).await;
        })
    }

    fn diagnostics<'a>(&'a self) -> BoxFuture<'a, Value> {
        Box::pin(async move {
            let record = self.diagnostics_record().await;
            serde_json::to_value(&record).unwrap_or(Value::Null)
        })
    }
}

// The read warm-up seam (read.ts:119) — `touchFile` without a wait.
impl crate::tool::read::ReadLsp for LspService {
    fn touch_file<'a>(&'a self, filepath: &'a str) -> BoxFuture<'a, ()> {
        Box::pin(async move {
            self.touch_file(filepath, None).await;
        })
    }
}
