//! M7.9 acceptance: the LSP client (`lsp/client.ts`) and service
//! (`lsp/lsp.ts`) against a scripted JSON-RPC server double
//! (`tests/fixtures/lsp_double.py`).

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use alforria_core::config::schema::{LspEntry, LspInfo, TrueLiteral};
use alforria_core::lsp::client::{CreateInput, LspClient, WaitMode};
use alforria_core::lsp::server::Flags;
use alforria_core::lsp::{LspInput, LspService};
use alforria_core::paths::GlobalPaths;
use alforria_core::tool::lsp::{LspServer, Position};
use serde_json::{json, Value};

fn double_path() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/lsp_double.py")
}

/// Command for the double: `python3 <script> <log> [args...]`.
fn double_command(log: &Path, args: &[&str]) -> Vec<String> {
    let mut command = vec![
        "python3".to_string(),
        double_path().to_string_lossy().into_owned(),
        log.to_string_lossy().into_owned(),
    ];
    command.extend(args.iter().map(|arg| arg.to_string()));
    command
}

async fn spawn_double(log: &Path, args: &[&str]) -> tokio::process::Child {
    let command = double_command(log, args);
    let program = &command[0];
    let rest = &command[1..];
    alforria_core::lsp::launch::spawn(program, rest, &std::env::temp_dir(), None)
        .expect("spawn double")
        .child
}

fn read_log(log: &Path) -> Vec<Value> {
    let text = std::fs::read_to_string(log).unwrap_or_default();
    text.lines()
        .filter(|line| !line.is_empty())
        .map(|line| serde_json::from_str(line).expect("log line json"))
        .collect()
}

fn temp() -> (tempfile::TempDir, PathBuf) {
    let dir = tempfile::tempdir_in("/tmp/opencode").expect("tempdir");
    std::fs::write(dir.path().join("a.rs"), "fn main() {}\nlet x = 1;\n").unwrap();
    let file = dir.path().join("a.rs");
    (dir, file)
}

/// A service whose only matching server for `.rs` is the double (the
/// built-in `rust` server is disabled by config, `lsp.ts:145-189`).
fn service(dir: &Path, log: &Path, args: &[&str]) -> Arc<LspService> {
    service_with_command(dir, double_command(log, args))
}

fn service_with_command(dir: &Path, command: Vec<String>) -> Arc<LspService> {
    let mut entries = BTreeMap::new();
    entries.insert(
        "rust".to_string(),
        LspEntry::Disabled {
            disabled: TrueLiteral,
        },
    );
    entries.insert(
        "double".to_string(),
        LspEntry::Server {
            command,
            extensions: Some(vec![".rs".to_string()]),
            disabled: None,
            env: None,
            initialization: Some(
                json!({ "a": { "b": 5 } })
                    .as_object()
                    .unwrap()
                    .clone()
                    .into_iter()
                    .collect(),
            ),
        },
    );
    LspService::new(LspInput {
        lsp: Some(LspInfo::Entries(entries)),
        directory: dir.to_path_buf(),
        worktree: dir.to_path_buf(),
        paths: GlobalPaths::resolve(dir.to_path_buf()),
        events: None,
        flags: Flags::default(),
    })
}

fn timeout() -> std::time::Duration {
    std::time::Duration::from_secs(30)
}

// -----------------------------------------------------------------------
// client (lsp/client.ts)
// -----------------------------------------------------------------------

#[tokio::test]
async fn initialize_handshake_and_dispatch() {
    let (dir, a_rs) = temp();
    let log = dir.path().join("double.log");
    let child = spawn_double(&log, &[]).await;
    let client = tokio::time::timeout(
        timeout(),
        LspClient::create(CreateInput {
            server_id: "double".to_string(),
            child,
            initialization: Some(json!({ "a": { "b": 5 } })),
            root: dir.path().to_path_buf(),
            directory: dir.path().to_path_buf(),
        }),
    )
    .await
    .expect("timeout")
    .expect("create");
    assert_eq!(client.server_id(), "double");
    assert_eq!(client.root(), dir.path());

    let hover = tokio::time::timeout(
        timeout(),
        client.connection.send_request(
            "textDocument/hover",
            json!({
                "textDocument": { "uri": format!("file://{}", a_rs.to_string_lossy()) },
                "position": { "line": 0, "character": 0 },
            }),
        ),
    )
    .await
    .expect("timeout")
    .expect("hover");
    assert_eq!(hover["contents"], "hover text");

    let messages = read_log(&log);
    let initialize = messages
        .iter()
        .find(|message| message["method"] == "initialize")
        .expect("initialize logged");
    let params = &initialize["params"];
    assert_eq!(
        params["rootUri"],
        format!("file://{}", dir.path().to_string_lossy())
    );
    assert!(params["processId"].is_number());
    assert_eq!(params["initializationOptions"]["a"]["b"], 5);
    assert_eq!(params["capabilities"]["window"]["workDoneProgress"], true);
    assert_eq!(params["capabilities"]["workspace"]["configuration"], true);
    // `workspace/configuration` answered from initialization (client.ts:107-114).
    let configuration = messages
        .iter()
        .find(|message| message["id"] == "double" && message["result"].is_array())
        .expect("configuration response");
    assert_eq!(configuration["result"], json!([5]));
    // `workspace/workspaceFolders` answers with the root (client.ts:198-206).
    let folders: Vec<&Value> = messages
        .iter()
        .filter(|message| message["result"][0]["name"] == "workspace")
        .collect();
    assert_eq!(folders.len(), 1, "{messages:?}");
    assert_eq!(
        folders[0]["result"][0]["uri"],
        format!("file://{}", dir.path().to_string_lossy())
    );
    // initialization is pushed as didChangeConfiguration (client.ts:607-613).
    assert!(messages.iter().any(
        |message| message["method"] == "workspace/didChangeConfiguration"
            && message["params"]["settings"]["a"]["b"] == 5
    ));
    drop(dir);
}

#[tokio::test]
async fn create_fails_when_server_dies() {
    let (dir, _a_rs) = temp();
    let log = dir.path().join("double.log");
    let child = spawn_double(&log, &["--die"]).await;
    let error = match LspClient::create(CreateInput {
        server_id: "double".to_string(),
        child,
        initialization: None,
        root: dir.path().to_path_buf(),
        directory: dir.path().to_path_buf(),
    })
    .await
    {
        Ok(_) => panic!("expected create failure"),
        Err(error) => error,
    };
    assert_eq!(error.server_id, "double");
    drop(dir);
}

#[tokio::test]
async fn notify_open_returns_ascending_versions() {
    let (dir, a_rs) = temp();
    let log = dir.path().join("double.log");
    let child = spawn_double(&log, &[]).await;
    let client = LspClient::create(CreateInput {
        server_id: "double".to_string(),
        child,
        initialization: None,
        root: dir.path().to_path_buf(),
        directory: dir.path().to_path_buf(),
    })
    .await
    .expect("create");
    let a_rs = a_rs.to_string_lossy().into_owned();
    assert_eq!(client.notify_open(&a_rs).await, 0);
    assert_eq!(client.notify_open(&a_rs).await, 1);
    let methods = |log: &Path| -> Vec<String> {
        read_log(log)
            .iter()
            .filter(|message| message["method"].is_string())
            .map(|message| message["method"].as_str().unwrap().to_string())
            .collect()
    };
    // The double consumes its stdin asynchronously — poll for the second
    // didChange to hit the log before asserting on the trace.
    let deadline = std::time::Instant::now() + timeout();
    let methods = loop {
        let methods = methods(&log);
        if methods
            .iter()
            .filter(|method| *method == "textDocument/didChange")
            .count()
            == 1
            || std::time::Instant::now() > deadline
        {
            break methods;
        }
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    };
    assert_eq!(
        methods
            .iter()
            .filter(|method| **method == "textDocument/didOpen")
            .count(),
        1,
        "{methods:?}"
    );
    assert_eq!(
        methods
            .iter()
            .filter(|method| **method == "textDocument/didChange")
            .count(),
        1,
        "{methods:?}"
    );
    client.shutdown().await;
    drop(dir);
}

#[tokio::test]
async fn shutdown_kills_the_server_process() {
    let (dir, _a_rs) = temp();
    let log = dir.path().join("double.log");
    let child = spawn_double(&log, &[]).await;
    let client = LspClient::create(CreateInput {
        server_id: "double".to_string(),
        child,
        initialization: None,
        root: dir.path().to_path_buf(),
        directory: dir.path().to_path_buf(),
    })
    .await
    .expect("create");
    client.shutdown().await;
    let mut child = client.child.lock().await;
    tokio::time::timeout(timeout(), async {
        loop {
            if child.try_wait().expect("wait").is_some() {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("timeout");
    drop(dir);
}

#[tokio::test]
async fn wait_for_diagnostics_push() {
    let (dir, a_rs) = temp();
    let child = spawn_double(&dir.path().join("double.log"), &["--no-pull"]).await;
    let client = LspClient::create(CreateInput {
        server_id: "double".to_string(),
        child,
        initialization: None,
        root: dir.path().to_path_buf(),
        directory: dir.path().to_path_buf(),
    })
    .await
    .expect("create");
    let a_rs = a_rs.to_string_lossy().into_owned();
    let version = client.notify_open(&a_rs).await;
    let wait = client.wait_for_diagnostics(alforria_core::lsp::client::WaitRequest {
        path: a_rs,
        version,
        mode: Some(WaitMode::Document),
        after: None,
    });
    tokio::time::timeout(timeout(), wait)
        .await
        .expect("timeout");
    let diagnostics = client.diagnostics();
    assert!(!diagnostics.is_empty(), "{diagnostics:?}");
    drop(dir);
}

// -----------------------------------------------------------------------
// service (lsp/lsp.ts Interface)
// -----------------------------------------------------------------------

#[tokio::test]
async fn push_diagnostics_publish_and_wait() {
    let (dir, a_rs) = temp();
    let log = dir.path().join("double.log");
    let service = service(dir.path(), &log, &["--no-pull"]);
    let a_rs = a_rs.to_string_lossy().into_owned();
    service.touch_file(&a_rs, Some(WaitMode::Document)).await;
    let record = service.diagnostics_record().await;
    let issues = record.get(&a_rs).expect("diagnostics for file");
    assert!(
        issues
            .iter()
            .any(|issue| issue["message"] == "double error" && issue["severity"] == 1),
        "{issues:?}"
    );
    drop(dir);
}

#[tokio::test]
async fn pull_diagnostics_wait() {
    let (dir, a_rs) = temp();
    let log = dir.path().join("double.log");
    let service = service(dir.path(), &log, &[]);
    let a_rs = a_rs.to_string_lossy().into_owned();
    service.touch_file(&a_rs, Some(WaitMode::Document)).await;
    let record = service.diagnostics_record().await;
    let issues = record.get(&a_rs).expect("diagnostics for file");
    assert!(
        issues
            .iter()
            .any(|issue| issue["message"] == "pull diagnostic"),
        "{issues:?}"
    );
    drop(dir);
}

#[tokio::test]
async fn hover_definition_references_implementation() {
    let (dir, a_rs) = temp();
    let log = dir.path().join("double.log");
    let service = service(dir.path(), &log, &[]);
    let lsp: Arc<dyn LspServer> = service.clone();
    let file = a_rs.to_string_lossy().into_owned();
    let position = |line: u64, character: u64| Position {
        file: file.clone(),
        line,
        character,
    };

    let hovers = tokio::time::timeout(timeout(), lsp.hover(position(0, 3)))
        .await
        .unwrap();
    assert_eq!(hovers.len(), 1);
    assert_eq!(hovers[0]["contents"], "hover text");

    let definitions = lsp.definition(position(1, 2)).await;
    assert_eq!(definitions.len(), 1);
    assert_eq!(definitions[0]["uri"], "file:///double/target.rs");
    assert_eq!(definitions[0]["range"]["start"]["line"], 3);

    let references = lsp.references(position(1, 2)).await;
    assert_eq!(references.len(), 1);
    assert_eq!(references[0]["uri"], "file:///double/ref1.rs");

    let implementations = lsp.implementation(position(1, 2)).await;
    assert_eq!(implementations.len(), 1);
    assert_eq!(implementations[0]["uri"], "file:///double/impl.rs");

    let uri = format!("file://{file}");
    let symbols = lsp.document_symbol(&uri).await;
    assert_eq!(symbols.len(), 1);
    assert_eq!(symbols[0]["name"], "main");
    drop(dir);
}

#[tokio::test]
async fn workspace_symbol_filters_kinds() {
    let (dir, a_rs) = temp();
    let log = dir.path().join("double.log");
    let service = service(dir.path(), &log, &[]);
    // workspaceSymbol runs over connected clients (runAll, lsp.ts:308-311).
    service.touch_file(&a_rs.to_string_lossy(), None).await;
    let lsp: Arc<dyn LspServer> = service.clone();
    let symbols = lsp.workspace_symbol("q").await;
    let names: Vec<&str> = symbols
        .iter()
        .map(|symbol| symbol["name"].as_str().unwrap())
        .collect();
    // kind 1 (Text) is outside the whitelist (lsp.ts:87-96).
    assert_eq!(names, vec!["Good", "Fn"]);
    drop(dir);
}

#[tokio::test]
async fn call_hierarchy_two_step() {
    let (dir, _a_rs) = temp();
    let log = dir.path().join("double.log");
    let service = service(dir.path(), &log, &[]);
    let lsp: Arc<dyn LspServer> = service.clone();
    let position = Position {
        file: dir.path().join("a.rs").to_string_lossy().into_owned(),
        line: 0,
        character: 0,
    };
    let prepared = lsp.prepare_call_hierarchy(position.clone()).await;
    assert_eq!(prepared.len(), 1);
    let incoming = lsp.incoming_calls(position.clone()).await;
    assert_eq!(incoming.len(), 1);
    assert_eq!(incoming[0]["from"]["name"], "caller");
    let outgoing = lsp.outgoing_calls(position).await;
    assert_eq!(outgoing.len(), 1);
    assert_eq!(outgoing[0]["to"]["name"], "callee");
    drop(dir);
}

#[tokio::test]
async fn status_list_shape() {
    let (dir, a_rs) = temp();
    let log = dir.path().join("double.log");
    let service = service(dir.path(), &log, &[]);
    assert_eq!(service.status().len(), 0);
    service.touch_file(&a_rs.to_string_lossy(), None).await;
    // `status` (lsp.ts:313-326) — { id, name, root, status } with the root
    // relative to the instance directory.
    assert_eq!(
        serde_json::to_value(service.status()).unwrap(),
        json!([{
            "id": "double",
            "name": "double",
            "root": "",
            "status": "connected",
        }])
    );
    drop(dir);
}

#[tokio::test]
async fn has_clients_extension_filter() {
    let (dir, a_rs) = temp();
    let log = dir.path().join("double.log");
    let service = service(dir.path(), &log, &[]);
    let a_rs = a_rs.to_string_lossy().into_owned();
    assert!(service.has_clients(&a_rs).await);
    assert!(
        !service
            .has_clients(&dir.path().join("a.txt").to_string_lossy())
            .await
    );
    // Outside the worktree: hasClients has no containsPath check
    // (lsp.ts:328-342) — only getClients filters by location.
    assert!(
        service
            .has_clients("/tmp/opencode-nowhere-outside/a.rs")
            .await
    );
    drop(dir);
}

#[tokio::test]
async fn broken_server_is_excluded() {
    let (dir, a_rs) = temp();
    let command = vec!["/nonexistent/lsp-binary".to_string()];
    let service = service_with_command(dir.path(), command);
    let a_rs = a_rs.to_string_lossy().into_owned();
    assert!(service.has_clients(&a_rs).await);
    service.touch_file(&a_rs, None).await;
    // The failed spawn is remembered as broken (lsp.ts:240-244).
    assert!(!service.has_clients(&a_rs).await);
    assert!(service.status().is_empty());
    drop(dir);
}

// -----------------------------------------------------------------------
// the M4 edit/read seams (edit.ts:197-198, read.ts:119)
// -----------------------------------------------------------------------

#[tokio::test]
async fn edit_seam_diagnostic_gating() {
    let (dir, a_rs) = temp();
    let log = dir.path().join("double.log");
    let service = service(dir.path(), &log, &["--no-pull"]);
    let edit: Arc<dyn alforria_core::tool::edit::Lsp> = service.clone();
    let read: Arc<dyn alforria_core::tool::read::ReadLsp> = service.clone();
    let a_rs = a_rs.to_string_lossy().into_owned();
    read.touch_file(&a_rs).await;
    edit.touch_file(&a_rs).await;
    let diagnostics = tokio::time::timeout(timeout(), edit.diagnostics())
        .await
        .unwrap();
    let issues = diagnostics
        .as_object()
        .expect("record")
        .get(&a_rs)
        .expect("file diagnostics")
        .clone();
    let report = alforria_core::tool::edit::diagnostic_report(&a_rs, &issues);
    assert!(report.contains("double error"), "{report}");
    drop(dir);
}
