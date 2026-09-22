//! LSP client — port of `lsp/client.ts`.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use serde_json::{json, Value};
use std::sync::Mutex;

use tokio::sync::broadcast;

use crate::lsp::connection::{Connection, Dispatch, RequestResult};
use crate::lsp::language::language_for_extension;
use crate::session::prompt_input::{file_url_to_path, path_to_file_url};
use crate::tool::def::BoxFuture;
use futures::StreamExt;

const DIAGNOSTICS_DEBOUNCE_MS: u64 = 150;
const DIAGNOSTICS_DOCUMENT_WAIT_TIMEOUT_MS: u64 = 5_000;
const DIAGNOSTICS_FULL_WAIT_TIMEOUT_MS: u64 = 10_000;
const DIAGNOSTICS_REQUEST_TIMEOUT_MS: u64 = 3_000;
const INITIALIZE_TIMEOUT_MS: u64 = 45_000;

// LSP spec constants
const FILE_CHANGE_CREATED: u64 = 1;
const FILE_CHANGE_CHANGED: u64 = 2;
const TEXT_DOCUMENT_SYNC_INCREMENTAL: i64 = 2;

/// `Diagnostic = VSCodeDiagnostic` — opaque JSON here.
pub type Diagnostic = Value;

#[derive(Debug, thiserror::Error)]
#[error("LSPInitializeError: {server_id}")]
pub struct InitializeError {
    pub server_id: String,
}

pub struct CreateInput {
    pub server_id: String,
    pub child: tokio::process::Child,
    pub initialization: Option<Value>,
    pub root: PathBuf,
    pub directory: PathBuf,
}

#[derive(Debug, Clone, Copy, Default)]
struct Published {
    at: u64,
    version: Option<i64>,
}

#[derive(Debug, Clone)]
struct DocFile {
    version: i64,
    text: String,
}

#[derive(Debug, Clone)]
struct CapabilityRegistration {
    identifier: Option<Vec<String>>,
    workspace_diagnostics: Option<bool>,
}

#[derive(Default)]
struct DiagnosticRequestResult {
    handled: bool,
    matched: bool,
    by_file: HashMap<String, Vec<Diagnostic>>,
}

#[derive(Default)]
struct PullState {
    supported: bool,
    identifiers: Vec<String>,
}

/// `waitForDiagnostics` request mode (client.ts:630-639).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WaitMode {
    Document,
    Full,
}

/// `waitForDiagnostics` request (client.ts:630).
#[derive(Debug, Clone)]
pub struct WaitRequest {
    pub path: String,
    pub version: i64,
    pub mode: Option<WaitMode>,
    pub after: Option<u64>,
}

struct ClientState {
    server_id: String,
    sync_kind: Mutex<Option<i64>>,
    has_static_pull_diagnostics: Mutex<bool>,
    initialization: Option<Value>,
    root: PathBuf,
    directory: PathBuf,
    push_diagnostics: Mutex<HashMap<String, Vec<Diagnostic>>>,
    pull_diagnostics: Mutex<HashMap<String, Vec<Diagnostic>>>,
    published: Mutex<HashMap<String, Published>>,
    diagnostic_registrations: Mutex<HashMap<String, CapabilityRegistration>>,
    files: Mutex<HashMap<String, DocFile>>,
    diagnostic_events: broadcast::Sender<(String, String)>,
    registration_events: broadcast::Sender<()>,
}

/// One connected language server (`LSPClient.Info`).
pub struct LspClient {
    root: PathBuf,
    state: Arc<ClientState>,
    pub connection: Arc<Connection>,
    pub child: tokio::sync::Mutex<tokio::process::Child>,
}

fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or_default()
}

fn get_file_path(uri: &str) -> Option<String> {
    if !uri.starts_with("file://") {
        return None;
    }
    Some(file_url_to_path(uri).to_string_lossy().into_owned())
}

fn end_position(text: &str) -> Value {
    let lines: Vec<&str> = text
        .split("\r\n")
        .flat_map(|l| l.split('\r'))
        .flat_map(|l| l.split('\n'))
        .collect();
    json!({
        "line": lines.len().saturating_sub(1),
        "character": lines.last().map(|l| l.encode_utf16().count()).unwrap_or(0),
    })
}

fn diagnostic_key(item: &Value) -> String {
    let key = json!({
        "code": item.get("code"),
        "severity": item.get("severity"),
        "message": item.get("message"),
        "source": item.get("source"),
        "range": item.get("range"),
    });
    serde_json::to_string(&key).unwrap_or_default()
}

fn dedupe_diagnostics(items: Vec<Diagnostic>) -> Vec<Diagnostic> {
    let mut seen = std::collections::HashSet::new();
    items
        .into_iter()
        .filter(|item| seen.insert(diagnostic_key(item)))
        .collect()
}

/// `configurationValue` (client.ts:107-114).
fn configuration_value(settings: Option<&Value>, section: Option<&str>) -> Value {
    let Some(section) = section else {
        return settings.cloned().unwrap_or(Value::Null);
    };
    let mut acc = settings.cloned();
    for key in section.split('.') {
        let Some(value) = acc.as_ref() else {
            return Value::Null;
        };
        if !value.is_object() {
            return Value::Null;
        }
        match value.get(key) {
            Some(next) => acc = Some(next.clone()),
            None => return Value::Null,
        }
    }
    acc.unwrap_or(Value::Null)
}

fn should_seed_diagnostics_on_first_push(server_id: &str) -> bool {
    server_id == "typescript"
}

fn diagnostics_as_vec(diagnostics: &Value) -> Vec<Diagnostic> {
    diagnostics.as_array().cloned().unwrap_or_default()
}

/// The dispatch table — `connection.onNotification`/`onRequest` handlers
/// (client.ts:160-206).
struct ClientDispatch {
    state: Arc<ClientState>,
}

impl Dispatch for ClientDispatch {
    fn notification(&self, method: &str, params: Value) {
        if method != "textDocument/publishDiagnostics" {
            return;
        }
        let Some(file_path) = params
            .get("uri")
            .and_then(Value::as_str)
            .and_then(get_file_path)
        else {
            return;
        };
        let version = params.get("version").and_then(Value::as_i64);
        self.state.published.lock().unwrap().insert(
            file_path.clone(),
            Published {
                at: now_ms(),
                version,
            },
        );
        let diagnostics = params
            .get("diagnostics")
            .cloned()
            .unwrap_or_else(|| json!([]));
        let seed = should_seed_diagnostics_on_first_push(&self.state.server_id)
            && !self
                .state
                .push_diagnostics
                .lock()
                .unwrap()
                .contains_key(&file_path);
        if seed {
            self.state
                .push_diagnostics
                .lock()
                .unwrap()
                .insert(file_path, diagnostics_as_vec(&diagnostics));
            return;
        }
        self.update_push_diagnostics(file_path, diagnostics_as_vec(&diagnostics));
    }

    fn request<'a>(&'a self, method: &'a str, params: Value) -> BoxFuture<'a, RequestResult> {
        Box::pin(async move {
            match method {
                "window/workDoneProgress/create" => Ok(Value::Null),
                "workspace/configuration" => {
                    let items = params
                        .get("items")
                        .and_then(Value::as_array)
                        .cloned()
                        .unwrap_or_default();
                    let settings = self.state.initialization.as_ref();
                    let values: Vec<Value> = items
                        .iter()
                        .map(|item| {
                            let section = item.get("section").and_then(Value::as_str);
                            configuration_value(settings, section)
                        })
                        .collect();
                    Ok(Value::Array(values))
                }
                "client/registerCapability" => {
                    let registrations = params
                        .get("registrations")
                        .and_then(Value::as_array)
                        .cloned()
                        .unwrap_or_default();
                    let mut changed = false;
                    for registration in registrations {
                        if registration.get("method").and_then(Value::as_str)
                            != Some("textDocument/diagnostic")
                        {
                            continue;
                        }
                        let Some(id) = registration.get("id").and_then(Value::as_str) else {
                            continue;
                        };
                        let register_options = registration.get("registerOptions");
                        let identifier = register_options
                            .and_then(|options| options.get("identifier"))
                            .and_then(Value::as_str)
                            .map(|identifier| vec![identifier.to_string()]);
                        self.state.diagnostic_registrations.lock().unwrap().insert(
                            id.to_string(),
                            CapabilityRegistration {
                                identifier,
                                workspace_diagnostics: register_options
                                    .and_then(|options| options.get("workspaceDiagnostics"))
                                    .and_then(Value::as_bool),
                            },
                        );
                        changed = true;
                    }
                    if changed {
                        let _ = self.state.registration_events.send(());
                    }
                    Ok(Value::Null)
                }
                "client/unregisterCapability" => {
                    let registrations = params
                        .get("unregisterations")
                        .and_then(Value::as_array)
                        .cloned()
                        .unwrap_or_default();
                    let mut changed = false;
                    for registration in registrations {
                        if registration.get("method").and_then(Value::as_str)
                            != Some("textDocument/diagnostic")
                        {
                            continue;
                        }
                        let Some(id) = registration.get("id").and_then(Value::as_str) else {
                            continue;
                        };
                        self.state
                            .diagnostic_registrations
                            .lock()
                            .unwrap()
                            .remove(id);
                        changed = true;
                    }
                    if changed {
                        let _ = self.state.registration_events.send(());
                    }
                    Ok(Value::Null)
                }
                "workspace/workspaceFolders" => {
                    let uri = path_to_file_url(&self.state.root);
                    Ok(json!([{ "name": "workspace", "uri": uri }]))
                }
                "workspace/diagnostic/refresh" => Ok(Value::Null),
                _ => Ok(Value::Null),
            }
        })
    }
}

impl ClientDispatch {
    fn update_push_diagnostics(&self, file_path: String, next: Vec<Diagnostic>) {
        self.state
            .push_diagnostics
            .lock()
            .unwrap()
            .insert(file_path.clone(), next);
        let _ = self
            .state
            .diagnostic_events
            .send((file_path, self.state.server_id.clone()));
    }
}

impl LspClient {
    pub fn server_id(&self) -> &str {
        &self.state.server_id
    }

    pub fn root(&self) -> &std::path::Path {
        &self.root
    }

    async fn send_request(&self, method: &str, params: Value) -> Result<Value, Value> {
        self.connection.send_request(method, params).await
    }

    /// `create` (client.ts:123-648) — wire the connection, run the
    /// initialize handshake and capture the server capabilities.
    pub async fn create(input: CreateInput) -> Result<Arc<LspClient>, InitializeError> {
        let initialize_error = || InitializeError {
            server_id: input.server_id.clone(),
        };
        let mut child = input.child;
        let state = Arc::new(ClientState {
            server_id: input.server_id.clone(),
            sync_kind: Mutex::new(None),
            has_static_pull_diagnostics: Mutex::new(false),
            initialization: input.initialization.clone(),
            root: input.root.clone(),
            directory: input.directory.clone(),
            push_diagnostics: Mutex::new(HashMap::new()),
            pull_diagnostics: Mutex::new(HashMap::new()),
            published: Mutex::new(HashMap::new()),
            diagnostic_registrations: Mutex::new(HashMap::new()),
            files: Mutex::new(HashMap::new()),
            diagnostic_events: broadcast::channel(1024).0,
            registration_events: broadcast::channel(1024).0,
        });
        let connection = Connection::spawn(
            &mut child,
            Arc::new(ClientDispatch {
                state: Arc::clone(&state),
            }),
        )
        .map_err(|_| initialize_error())?;
        // stderr "resume" (client.ts:136) — drain it so the pipe never
        // fills.
        if let Some(stderr) = child.stderr.take() {
            tokio::spawn(async move {
                use tokio::io::AsyncReadExt;
                let mut stderr = stderr;
                let mut buffer = [0u8; 4096];
                loop {
                    match stderr.read(&mut buffer).await {
                        Ok(0) | Err(_) => break,
                        Ok(_) => continue,
                    }
                }
            });
        }
        let root_uri = path_to_file_url(&input.root);
        let process_id = child.id();
        let initialized =
            tokio::time::timeout(Duration::from_millis(INITIALIZE_TIMEOUT_MS), async {
                let request = json!({
                    "rootUri": root_uri,
                    "processId": process_id,
                    "workspaceFolders": [
                        { "name": "workspace", "uri": root_uri }
                    ],
                    "initializationOptions": input
                        .initialization
                        .clone()
                        .unwrap_or_else(|| json!({})),
                    "capabilities": {
                        "window": { "workDoneProgress": true },
                        "workspace": {
                            "configuration": true,
                            "didChangeWatchedFiles": { "dynamicRegistration": true },
                            "diagnostics": { "refreshSupport": false },
                        },
                        "textDocument": {
                            "synchronization": { "didOpen": true, "didChange": true },
                            "diagnostic": {
                                "dynamicRegistration": true,
                                "relatedDocumentSupport": true,
                            },
                            "publishDiagnostics": { "versionSupport": false },
                        },
                    },
                });
                connection
                    .send_request("initialize", request)
                    .await
                    .map(|result| result.get("capabilities").cloned().unwrap_or(Value::Null))
            })
            .await
            .map_err(|_| initialize_error())?
            .map_err(|_| initialize_error())?;
        let sync = initialized.get("textDocumentSync");
        *state.sync_kind.lock().unwrap() = match sync {
            Some(Value::Object(_)) => sync
                .and_then(|sync| sync.get("change"))
                .and_then(Value::as_i64),
            _ => sync.and_then(Value::as_i64),
        };
        *state.has_static_pull_diagnostics.lock().unwrap() = initialized
            .get("diagnosticProvider")
            .is_some_and(|provider| !provider.is_null());
        let this = Arc::new(LspClient {
            root: input.root.clone(),
            state,
            connection,
            child: tokio::sync::Mutex::new(child),
        });
        this.connection
            .send_notification("initialized", json!({}))
            .await;
        if let Some(initialization) = input.initialization.clone() {
            this.connection
                .send_notification(
                    "workspace/didChangeConfiguration",
                    json!({ "settings": initialization }),
                )
                .await;
        }
        Ok(this)
    }

    fn normalize(&self, path: &str) -> String {
        let path = PathBuf::from(path);
        let path = if path.is_absolute() {
            path
        } else {
            self.state.directory.join(path)
        };
        path.to_string_lossy().into_owned()
    }

    /// `notify.open` (client.ts:553-621) — didOpen/didChange + the
    /// didChangeWatchedFiles bookkeeping. Returns the new document
    /// version.
    pub async fn notify_open(&self, path: &str) -> i64 {
        let path = self.normalize(path);
        let path_buf = PathBuf::from(&path);
        let text = tokio::fs::read_to_string(&path_buf)
            .await
            .unwrap_or_default();
        let extension = path_buf
            .extension()
            .map(|ext| format!(".{}", ext.to_string_lossy()))
            .unwrap_or_else(|| path.clone());
        let language_id = language_for_extension(&extension)
            .map(String::from)
            .unwrap_or_else(|| "plaintext".to_string());
        let uri = path_to_file_url(&path_buf);
        let existing = self
            .state
            .files
            .lock()
            .unwrap()
            .get(&path)
            .map(|doc| (doc.version, doc.text.clone()));
        if let Some((version, document_text)) = existing {
            self.connection
                .send_notification(
                    "workspace/didChangeWatchedFiles",
                    json!({
                        "changes": [
                            { "uri": uri, "type": FILE_CHANGE_CHANGED }
                        ]
                    }),
                )
                .await;
            let next = version + 1;
            self.state.files.lock().unwrap().insert(
                path.clone(),
                DocFile {
                    version: next,
                    text: text.clone(),
                },
            );
            let sync_kind = *self.state.sync_kind.lock().unwrap();
            let content_changes = if sync_kind == Some(TEXT_DOCUMENT_SYNC_INCREMENTAL) {
                json!([{
                    "range": {
                        "start": { "line": 0, "character": 0 },
                        "end": end_position(&document_text),
                    },
                    "text": text,
                }])
            } else {
                json!([{ "text": text }])
            };
            self.connection
                .send_notification(
                    "textDocument/didChange",
                    json!({
                        "textDocument": { "uri": uri, "version": next },
                        "contentChanges": content_changes,
                    }),
                )
                .await;
            return next;
        }

        self.connection
            .send_notification(
                "workspace/didChangeWatchedFiles",
                json!({
                    "changes": [
                        { "uri": uri, "type": FILE_CHANGE_CREATED }
                    ]
                }),
            )
            .await;
        {
            let mut push = self.state.push_diagnostics.lock().unwrap();
            let mut pull = self.state.pull_diagnostics.lock().unwrap();
            push.remove(&path);
            pull.remove(&path);
        }
        self.connection
            .send_notification(
                "textDocument/didOpen",
                json!({
                    "textDocument": {
                        "uri": uri,
                        "languageId": language_id,
                        "version": 0,
                        "text": text,
                    }
                }),
            )
            .await;
        self.state
            .files
            .lock()
            .unwrap()
            .insert(path, DocFile { version: 0, text });
        0
    }

    /// `diagnostics` getter (client.ts:623-629) — the merged push+pull
    /// map.
    pub fn diagnostics(&self) -> HashMap<String, Vec<Diagnostic>> {
        let mut keys: std::collections::HashSet<String> = std::collections::HashSet::new();
        keys.extend(self.state.push_diagnostics.lock().unwrap().keys().cloned());
        keys.extend(self.state.pull_diagnostics.lock().unwrap().keys().cloned());
        keys.into_iter()
            .map(|key| {
                let merged = self.merged_diagnostics(&key);
                (key, merged)
            })
            .collect()
    }

    fn merged_diagnostics(&self, file_path: &str) -> Vec<Diagnostic> {
        let mut items = self
            .state
            .push_diagnostics
            .lock()
            .unwrap()
            .get(file_path)
            .cloned()
            .unwrap_or_default();
        items.extend(
            self.state
                .pull_diagnostics
                .lock()
                .unwrap()
                .get(file_path)
                .cloned()
                .unwrap_or_default(),
        );
        dedupe_diagnostics(items)
    }

    fn update_pull_diagnostics(&self, file_path: &str, next: Vec<Diagnostic>) {
        self.state
            .pull_diagnostics
            .lock()
            .unwrap()
            .insert(file_path.to_string(), next);
    }

    /// `mergeResults` (client.ts:272-291).
    fn merge_results(&self, file_path: &str, results: &[DiagnosticRequestResult]) -> (bool, bool) {
        let handled = results.iter().any(|result| result.handled);
        let matched = results.iter().any(|result| result.matched);
        if !handled {
            return (false, false);
        }
        let mut merged: HashMap<String, Vec<Diagnostic>> = HashMap::new();
        for result in results {
            for (target, items) in &result.by_file {
                merged
                    .entry(target.clone())
                    .or_default()
                    .extend(items.iter().cloned());
            }
        }
        if matched && !merged.contains_key(file_path) {
            merged.insert(file_path.to_string(), Vec::new());
        }
        for (target, items) in merged {
            self.update_pull_diagnostics(&target, dedupe_diagnostics(items));
        }
        (handled, matched)
    }

    /// `requestDiagnosticReport` (client.ts:293-327).
    async fn request_diagnostic_report(
        &self,
        file_path: &str,
        identifier: Option<&str>,
    ) -> DiagnosticRequestResult {
        let params = match identifier {
            Some(identifier) => json!({
                "identifier": identifier,
                "textDocument": { "uri": path_to_file_url(&PathBuf::from(file_path)) },
            }),
            None => json!({
                "textDocument": { "uri": path_to_file_url(&PathBuf::from(file_path)) },
            }),
        };
        let report = match tokio::time::timeout(
            Duration::from_millis(DIAGNOSTICS_REQUEST_TIMEOUT_MS),
            self.send_request("textDocument/diagnostic", params),
        )
        .await
        {
            Ok(Ok(report)) => report,
            _ => return DiagnosticRequestResult::default(),
        };
        let mut result = DiagnosticRequestResult::default();
        if let Some(items) = report.get("items").filter(|items| items.is_array()) {
            result
                .by_file
                .entry(file_path.to_string())
                .or_default()
                .extend(items.as_array().cloned().unwrap_or_default());
            result.handled = true;
            result.matched = true;
        }
        if let Some(related) = report.get("relatedDocuments").and_then(Value::as_object) {
            for (uri, related) in related {
                let Some(related_path) = get_file_path(uri) else {
                    continue;
                };
                let Some(items) = related.get("items").filter(|items| items.is_array()) else {
                    continue;
                };
                result
                    .by_file
                    .entry(related_path.clone())
                    .or_default()
                    .extend(items.as_array().cloned().unwrap_or_default());
                result.handled = true;
                result.matched = result.matched || related_path == file_path;
            }
        }
        result
    }

    /// `requestWorkspaceDiagnosticReport` (client.ts:329-353).
    async fn request_workspace_diagnostic_report(
        &self,
        file_path: &str,
        identifier: Option<&str>,
    ) -> DiagnosticRequestResult {
        let params = match identifier {
            Some(identifier) => json!({
                "identifier": identifier,
                "previousResultIds": [],
            }),
            None => json!({ "previousResultIds": [] }),
        };
        let report = match tokio::time::timeout(
            Duration::from_millis(DIAGNOSTICS_REQUEST_TIMEOUT_MS),
            self.send_request("workspace/diagnostic", params),
        )
        .await
        {
            Ok(Ok(report)) => report,
            _ => return DiagnosticRequestResult::default(),
        };
        let mut result = DiagnosticRequestResult {
            handled: true,
            ..Default::default()
        };
        for item in report
            .get("items")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default()
        {
            let Some(related_path) = item
                .get("uri")
                .and_then(Value::as_str)
                .and_then(get_file_path)
            else {
                continue;
            };
            let Some(items) = item.get("items").filter(|items| items.is_array()) else {
                continue;
            };
            result
                .by_file
                .entry(related_path.clone())
                .or_default()
                .extend(items.as_array().cloned().unwrap_or_default());
            result.matched = result.matched || related_path == file_path;
        }
        result
    }

    /// `documentPullState` (client.ts:355-365).
    fn document_pull_state(&self) -> PullState {
        let registrations = self.state.diagnostic_registrations.lock().unwrap();
        let document: Vec<&CapabilityRegistration> = registrations
            .values()
            .filter(|registration| registration.workspace_diagnostics != Some(true))
            .collect();
        let mut identifiers: Vec<String> = Vec::new();
        for registration in &document {
            if let Some(identifier) = &registration.identifier {
                for id in identifier {
                    if !identifiers.contains(id) {
                        identifiers.push(id.clone());
                    }
                }
            }
        }
        PullState {
            supported: *self.state.has_static_pull_diagnostics.lock().unwrap()
                || !document.is_empty(),
            identifiers,
        }
    }

    /// `workspacePullState` (client.ts:367-377).
    fn workspace_pull_state(&self) -> PullState {
        let registrations = self.state.diagnostic_registrations.lock().unwrap();
        let workspace: Vec<&CapabilityRegistration> = registrations
            .values()
            .filter(|registration| registration.workspace_diagnostics == Some(true))
            .collect();
        let mut identifiers: Vec<String> = Vec::new();
        for registration in &workspace {
            if let Some(identifier) = &registration.identifier {
                for id in identifier {
                    if !identifiers.contains(id) {
                        identifiers.push(id.clone());
                    }
                }
            }
        }
        PullState {
            supported: !workspace.is_empty(),
            identifiers,
        }
    }

    fn has_current_file_diagnostics(
        &self,
        file_path: &str,
        results: &[DiagnosticRequestResult],
    ) -> bool {
        results.iter().any(|result| {
            result
                .by_file
                .get(file_path)
                .is_some_and(|items| !items.is_empty())
        })
    }

    /// `requestDiagnostics` (client.ts:382-410) — dispatch in parallel,
    /// unblock once `done` matches.
    async fn request_diagnostics(
        &self,
        file_path: &str,
        requests: Vec<BoxFuture<'_, DiagnosticRequestResult>>,
        done: impl Fn(&[DiagnosticRequestResult]) -> bool,
    ) -> (bool, bool) {
        if requests.is_empty() {
            return (false, false);
        }
        let mut stream = futures::stream::FuturesUnordered::new();
        for request in requests {
            stream.push(request);
        }
        let mut results = Vec::new();
        while let Some(result) = stream.next().await {
            results.push(result);
            let merged = self.merge_results(file_path, &results);
            if done(&results) {
                return merged;
            }
            if stream.is_empty() {
                return merged;
            }
        }
        (false, false)
    }

    /// `requestDocumentDiagnostics` (client.ts:416-427).
    async fn request_document_diagnostics(&self, file_path: &str) -> (bool, bool) {
        let state = self.document_pull_state();
        if !state.supported {
            return (false, false);
        }
        let mut requests: Vec<BoxFuture<'_, DiagnosticRequestResult>> = Vec::new();
        requests.push(Box::pin(self.request_diagnostic_report(file_path, None)));
        for identifier in &state.identifiers {
            requests.push(Box::pin(
                self.request_diagnostic_report(file_path, Some(identifier)),
            ));
        }
        let current = file_path.to_string();
        self.request_diagnostics(file_path, requests, move |results| {
            self.has_current_file_diagnostics(&current, results)
        })
        .await
    }

    /// `requestFullDiagnostics` (client.ts:429-444).
    async fn request_full_diagnostics(&self, file_path: &str) -> (bool, bool) {
        let document_state = self.document_pull_state();
        let workspace_state = self.workspace_pull_state();
        if !document_state.supported && !workspace_state.supported {
            return (false, false);
        }
        let mut requests: Vec<BoxFuture<'_, DiagnosticRequestResult>> = Vec::new();
        if document_state.supported {
            requests.push(Box::pin(self.request_diagnostic_report(file_path, None)));
        }
        for identifier in &document_state.identifiers {
            requests.push(Box::pin(
                self.request_diagnostic_report(file_path, Some(identifier)),
            ));
        }
        if workspace_state.supported {
            requests.push(Box::pin(
                self.request_workspace_diagnostic_report(file_path, None),
            ));
        }
        for identifier in &workspace_state.identifiers {
            requests.push(Box::pin(
                self.request_workspace_diagnostic_report(file_path, Some(identifier)),
            ));
        }
        let results = futures::future::join_all(requests).await;
        self.merge_results(file_path, &results)
    }

    /// `waitForRegistrationChange` (client.ts:446-462).
    async fn wait_for_registration_change(&self, timeout: u64) -> bool {
        if timeout == 0 {
            return false;
        }
        let mut receiver = self.state.registration_events.subscribe();
        matches!(
            tokio::time::timeout(Duration::from_millis(timeout), receiver.recv()).await,
            Ok(Ok(_))
        )
    }

    /// `schedule` inside `waitForFreshPush` (client.ts:479-486) —
    /// computes the debounced fire instant for a fresh push.
    fn schedule_fresh_push(
        &self,
        path: &str,
        version: i64,
        after: u64,
        current: Option<tokio::time::Instant>,
    ) -> Option<tokio::time::Instant> {
        let hit = self.state.published.lock().unwrap().get(path).copied();
        let Some(hit) = hit else {
            return current;
        };
        if let Some(hit_version) = hit.version {
            if hit_version != version {
                return current;
            }
        }
        if hit.at < after && hit.version != Some(version) {
            return current;
        }
        let elapsed = now_ms().saturating_sub(hit.at);
        let delay = DIAGNOSTICS_DEBOUNCE_MS.saturating_sub(elapsed);
        Some(tokio::time::Instant::now() + Duration::from_millis(delay))
    }

    /// `waitForFreshPush` (client.ts:464-497).
    async fn wait_for_fresh_push(
        &self,
        path: &str,
        version: i64,
        after: u64,
        timeout: u64,
    ) -> bool {
        if timeout == 0 {
            return false;
        }
        let deadline = tokio::time::Instant::now() + Duration::from_millis(timeout);
        let mut receiver = self.state.diagnostic_events.subscribe();
        let mut debounce = self.schedule_fresh_push(path, version, after, None);
        loop {
            let target = debounce.unwrap_or(deadline);
            let fires_debounce = debounce.is_some();
            tokio::select! {
                _ = tokio::time::sleep_until(target) => {
                    return fires_debounce && target <= deadline;
                }
                event = receiver.recv() => {
                    match event {
                        Err(_) => return false,
                        Ok((event_path, server_id)) => {
                            if event_path == path && server_id == self.state.server_id {
                                debounce = self.schedule_fresh_push(
                                    path,
                                    version,
                                    after,
                                    debounce,
                                );
                            }
                        }
                    }
                }
            }
        }
    }

    /// `waitForDocumentDiagnostics` (client.ts:499-519).
    async fn wait_for_document_diagnostics(&self, request: &WaitRequest) {
        let started_at = request.after.unwrap_or_else(now_ms);
        let path = request.path.clone();
        let push_wait = self.wait_for_fresh_push(
            &path,
            request.version,
            started_at,
            DIAGNOSTICS_DOCUMENT_WAIT_TIMEOUT_MS,
        );
        let mut push_wait = Box::pin(push_wait);
        while now_ms() - started_at < DIAGNOSTICS_DOCUMENT_WAIT_TIMEOUT_MS {
            let (_, matched) = self.request_document_diagnostics(&path).await;
            if matched {
                return;
            }
            let remaining =
                DIAGNOSTICS_DOCUMENT_WAIT_TIMEOUT_MS.saturating_sub(now_ms() - started_at);
            if remaining == 0 {
                return;
            }
            tokio::select! {
                _ = &mut push_wait => {
                    return;
                }
                changed = self.wait_for_registration_change(remaining) => {
                    if !changed {
                        return;
                    }
                }
            };
        }
    }

    /// `waitForFullDiagnostics` (client.ts:521-541).
    async fn wait_for_full_diagnostics(&self, request: &WaitRequest) {
        let started_at = request.after.unwrap_or_else(now_ms);
        let path = request.path.clone();
        let push_wait = self.wait_for_fresh_push(
            &path,
            request.version,
            started_at,
            DIAGNOSTICS_FULL_WAIT_TIMEOUT_MS,
        );
        let mut push_wait = Box::pin(push_wait);
        while now_ms() - started_at < DIAGNOSTICS_FULL_WAIT_TIMEOUT_MS {
            let (handled, matched) = self.request_full_diagnostics(&path).await;
            if handled || matched {
                return;
            }
            let remaining = DIAGNOSTICS_FULL_WAIT_TIMEOUT_MS.saturating_sub(now_ms() - started_at);
            if remaining == 0 {
                return;
            }
            tokio::select! {
                _ = &mut push_wait => {
                    return;
                }
                changed = self.wait_for_registration_change(remaining) => {
                    if !changed {
                        return;
                    }
                }
            };
        }
    }

    /// `waitForDiagnostics` (client.ts:630-639).
    pub async fn wait_for_diagnostics(&self, request: WaitRequest) {
        let path = self.normalize(&request.path);
        let request = WaitRequest { path, ..request };
        match request.mode {
            Some(WaitMode::Document) => self.wait_for_document_diagnostics(&request).await,
            _ => self.wait_for_full_diagnostics(&request).await,
        }
    }

    /// `shutdown` (client.ts:640-645).
    pub async fn shutdown(&self) {
        self.connection.end().await;
        let mut child = self.child.lock().await;
        let _ = child.start_kill();
    }
}
