//! `lsp` tool — port of `tool/lsp.ts` (spec M4.8).
//!
//! The LSP client itself is M7; the tool runs against the [`LspServer`]
//! seam.

use std::sync::Arc;

use serde::Deserialize;
use serde_json::{json, Value};

use crate::tool::def::{define, Agents, AskRequest, BoxFuture, ExecuteResult, ToolCtxRef, ToolDef};
use crate::tool::error::ToolError;
use crate::tool::external_directory::{assert_external_directory, ExternalOptions, Kind};
use crate::tool::truncate::Truncate;

pub const OPERATIONS: [&str; 9] = [
    "goToDefinition",
    "findReferences",
    "hover",
    "documentSymbol",
    "workspaceSymbol",
    "goToImplementation",
    "prepareCallHierarchy",
    "incomingCalls",
    "outgoingCalls",
];

/// `LSP.Position` (1-based inputs converted to 0-based for the service).
#[derive(Debug, Clone)]
pub struct Position {
    pub file: String,
    pub line: u64,
    pub character: u64,
}

/// The `LSP.Service` surface the lsp tool consumes.
pub trait LspServer: Send + Sync {
    fn has_clients<'a>(&'a self, file: &'a str) -> BoxFuture<'a, bool>;
    fn touch_file<'a>(&'a self, file: &'a str) -> BoxFuture<'a, ()>;
    fn definition<'a>(&'a self, position: Position) -> BoxFuture<'a, Vec<Value>>;
    fn references<'a>(&'a self, position: Position) -> BoxFuture<'a, Vec<Value>>;
    fn hover<'a>(&'a self, position: Position) -> BoxFuture<'a, Vec<Value>>;
    fn document_symbol<'a>(&'a self, uri: &'a str) -> BoxFuture<'a, Vec<Value>>;
    fn workspace_symbol<'a>(&'a self, query: &'a str) -> BoxFuture<'a, Vec<Value>>;
    fn implementation<'a>(&'a self, position: Position) -> BoxFuture<'a, Vec<Value>>;
    fn prepare_call_hierarchy<'a>(&'a self, position: Position) -> BoxFuture<'a, Vec<Value>>;
    fn incoming_calls<'a>(&'a self, position: Position) -> BoxFuture<'a, Vec<Value>>;
    fn outgoing_calls<'a>(&'a self, position: Position) -> BoxFuture<'a, Vec<Value>>;
}

#[derive(Debug, Deserialize)]
pub struct LspParameters {
    pub operation: String,
    #[serde(rename = "filePath")]
    pub file_path: String,
    pub line: Option<u64>,
    pub character: Option<u64>,
    pub query: Option<String>,
}

/// Hand-authored JSON Schema (byte-matches the Effect-generated schema; see
/// `tests/golden/toolschema/lsp.json`).
pub fn parameters() -> Value {
    let mut properties = json!({
        "operation": {
            "type": "string",
            "enum": OPERATIONS,
            "description": "The LSP operation to perform"
        },
        "filePath": {
            "type": "string",
            "description": "The absolute or relative path to the file"
        },
        "line": {
            "type": "integer",
            "minimum": 1,
            "description": "The line number (1-based, as shown in editors)"
        },
        "character": {
            "type": "integer",
            "minimum": 1,
            "description": "The character offset (1-based, as shown in editors)"
        }
    });
    if let Some(object) = properties.as_object_mut() {
        object.insert(
            "query".to_string(),
            json!({
                "type": "string",
                "description": "Search query for workspaceSymbol. Empty string requests all symbols."
            }),
        );
        // Match TS Schema.Struct field order: operation, filePath, line,
        // character, query.
        let desired = ["operation", "filePath", "line", "character", "query"];
        let mut sorted = serde_json::Map::new();
        for key in desired {
            if let Some(value) = object.remove(key) {
                sorted.insert(key.to_string(), value);
            }
        }
        properties = Value::Object(sorted);
    }
    let mut schema = json!({
        "$schema": "https://json-schema.org/draft/2020-12/schema",
        "type": "object",
        "properties": properties,
    });
    if let Some(object) = schema.as_object_mut() {
        object.insert(
            "required".to_string(),
            json!(["operation", "filePath", "line", "character"]),
        );
    }
    schema
}

/// Build the `lsp` tool.
pub fn lsp_tool(
    truncate: Arc<dyn Truncate>,
    agents: Arc<dyn Agents>,
    lsp: Arc<dyn LspServer>,
) -> ToolDef {
    define(
        "lsp",
        include_str!("txt/lsp.txt"),
        parameters(),
        None,
        truncate,
        agents,
        move |params: LspParameters, ctx: ToolCtxRef<'_>| {
            let lsp = lsp.clone();
            Box::pin(async move { run(params, ctx, lsp).await })
        },
    )
}

async fn run(
    params: LspParameters,
    ctx: ToolCtxRef<'_>,
    lsp: Arc<dyn LspServer>,
) -> Result<ExecuteResult, ToolError> {
    let instance = ctx.instance;
    let file = if std::path::Path::new(&params.file_path).is_absolute() {
        params.file_path.clone()
    } else {
        instance
            .directory
            .join(&params.file_path)
            .to_string_lossy()
            .to_string()
    };
    assert_external_directory(
        &ctx,
        Some(&file),
        ExternalOptions {
            bypass: ctx.extra.bypass_cwd_check,
            kind: Kind::File,
        },
    )
    .await?;

    let meta = match params.operation.as_str() {
        "workspaceSymbol" => json!({ "operation": params.operation }),
        "documentSymbol" => json!({ "operation": params.operation, "filePath": file }),
        _ => json!({
            "operation": params.operation,
            "filePath": file,
            "line": params.line,
            "character": params.character,
        }),
    };
    ctx.ask
        .ask(AskRequest {
            permission: "lsp".to_string(),
            patterns: vec!["*".to_string()],
            always: vec!["*".to_string()],
            metadata: meta,
        })
        .await?;

    let uri = format!("file://{file}");
    let line = params.line.unwrap_or(1);
    let character = params.character.unwrap_or(1);
    let rel_path =
        crate::tool::ripgrep::ts_relative(&instance.worktree, std::path::Path::new(&file));
    let detail = match params.operation.as_str() {
        "workspaceSymbol" => String::new(),
        "documentSymbol" => rel_path,
        _ => format!("{rel_path}:{line}:{character}"),
    };
    let title = if detail.is_empty() {
        params.operation.clone()
    } else {
        format!("{} {detail}", params.operation)
    };

    if !std::path::Path::new(&file).exists() {
        return Err(ToolError::Failed(format!("File not found: {file}")));
    }

    if !lsp.has_clients(&file).await {
        return Err(ToolError::Failed(
            "No LSP server available for this file type.".to_string(),
        ));
    }

    lsp.touch_file(&file).await;

    let position = Position {
        file: file.clone(),
        line: line - 1,
        character: character - 1,
    };
    let result: Vec<Value> = match params.operation.as_str() {
        "goToDefinition" => lsp.definition(position).await,
        "findReferences" => lsp.references(position).await,
        "hover" => lsp.hover(position).await,
        "documentSymbol" => lsp.document_symbol(&uri).await,
        "workspaceSymbol" => {
            lsp.workspace_symbol(params.query.as_deref().unwrap_or(""))
                .await
        }
        "goToImplementation" => lsp.implementation(position).await,
        "prepareCallHierarchy" => lsp.prepare_call_hierarchy(position).await,
        "incomingCalls" => lsp.incoming_calls(position).await,
        "outgoingCalls" => lsp.outgoing_calls(position).await,
        _ => Vec::new(),
    };

    Ok(ExecuteResult {
        title,
        metadata: json!({ "result": result }),
        output: if result.is_empty() {
            format!("No results found for {}", params.operation)
        } else {
            serde_json::to_string_pretty(&result).unwrap_or_default()
        },
        attachments: None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tool::def::Extra;
    use crate::tool::ripgrep::test_support::{ctx, fixed_agents, instance, RecordingAsk};
    use crate::tool::truncate::TruncateService;
    use serde_json::json;
    use std::sync::Mutex;

    struct FakeLsp {
        has_clients: bool,
        files: Mutex<Vec<String>>,
    }

    impl LspServer for FakeLsp {
        fn has_clients<'a>(&'a self, _file: &'a str) -> BoxFuture<'a, bool> {
            Box::pin(async { self.has_clients })
        }
        fn touch_file<'a>(&'a self, file: &'a str) -> BoxFuture<'a, ()> {
            Box::pin(async move {
                self.files.lock().unwrap().push(file.to_string());
            })
        }
        fn definition<'a>(&'a self, _position: Position) -> BoxFuture<'a, Vec<Value>> {
            Box::pin(async { vec![json!({ "range": { "line": 0 } })] })
        }
        fn references<'a>(&'a self, _position: Position) -> BoxFuture<'a, Vec<Value>> {
            Box::pin(async { Vec::new() })
        }
        fn hover<'a>(&'a self, _position: Position) -> BoxFuture<'a, Vec<Value>> {
            Box::pin(async { Vec::new() })
        }
        fn document_symbol<'a>(&'a self, _uri: &'a str) -> BoxFuture<'a, Vec<Value>> {
            Box::pin(async { vec![json!({ "name": "main" })] })
        }
        fn workspace_symbol<'a>(&'a self, query: &'a str) -> BoxFuture<'a, Vec<Value>> {
            Box::pin(async move { vec![json!({ "name": query })] })
        }
        fn implementation<'a>(&'a self, _position: Position) -> BoxFuture<'a, Vec<Value>> {
            Box::pin(async { Vec::new() })
        }
        fn prepare_call_hierarchy<'a>(&'a self, _position: Position) -> BoxFuture<'a, Vec<Value>> {
            Box::pin(async { Vec::new() })
        }
        fn incoming_calls<'a>(&'a self, _position: Position) -> BoxFuture<'a, Vec<Value>> {
            Box::pin(async { Vec::new() })
        }
        fn outgoing_calls<'a>(&'a self, _position: Position) -> BoxFuture<'a, Vec<Value>> {
            Box::pin(async { Vec::new() })
        }
    }

    fn tool() -> ToolDef {
        lsp_tool(
            Arc::new(TruncateService::default_limits(std::path::PathBuf::from(
                "/tmp/opencode",
            ))),
            fixed_agents(),
            Arc::new(FakeLsp {
                has_clients: true,
                files: Mutex::new(Vec::new()),
            }),
        )
    }

    async fn call(
        args: Value,
        temp: &std::path::Path,
    ) -> (
        Result<ExecuteResult, ToolError>,
        Vec<crate::tool::def::AskRequest>,
    ) {
        let def = tool();
        let ask = RecordingAsk::new();
        let inst = instance(temp);
        let extra = Extra::default();
        let ctx = ctx(&ask, &inst, &extra);
        let result = (def.execute)(args, ctx).await;
        (result, ask.requests())
    }

    fn temp_with_file(name: &str, content: &str) -> crate::storage::test_support::TempDir {
        let temp = crate::storage::test_support::TempDir::new(name);
        std::fs::write(temp.path().join("a.rs"), content).unwrap();
        temp
    }

    #[tokio::test]
    async fn definition_operation() {
        let temp = temp_with_file("lsp-def", "fn main() {}\n");
        let (result, asks) = call(
            json!({ "operation": "goToDefinition", "filePath": "a.rs", "line": 1, "character": 5 }),
            temp.path(),
        )
        .await;
        let result = result.unwrap();
        assert_eq!(result.title, "goToDefinition a.rs:1:5");
        assert!(result.output.contains("range"), "{}", result.output);
        assert_eq!(asks.len(), 1);
        assert_eq!(asks[0].permission, "lsp");
        assert_eq!(asks[0].metadata["line"], json!(1));
        drop(temp);
    }

    #[tokio::test]
    async fn document_symbol_title_has_no_position() {
        let temp = temp_with_file("lsp-doc", "fn main() {}\n");
        let (result, _) = call(
            json!({ "operation": "documentSymbol", "filePath": "a.rs", "line": 1, "character": 1 }),
            temp.path(),
        )
        .await;
        let result = result.unwrap();
        assert_eq!(result.title, "documentSymbol a.rs");
        drop(temp);
    }

    #[tokio::test]
    async fn workspace_symbol_query() {
        let temp = temp_with_file("lsp-ws", "fn main() {}\n");
        let (result, asks) = call(
            json!({ "operation": "workspaceSymbol", "filePath": "a.rs", "line": 1, "character": 1, "query": "main" }),
            temp.path(),
        )
        .await;
        let result = result.unwrap();
        assert_eq!(result.title, "workspaceSymbol");
        assert!(result.output.contains("main"), "{}", result.output);
        // workspaceSymbol metadata: operation only.
        assert_eq!(asks[0].metadata, json!({ "operation": "workspaceSymbol" }));
        drop(temp);
    }

    #[tokio::test]
    async fn missing_file_is_an_error() {
        let temp = crate::storage::test_support::TempDir::new("lsp-missing");
        let (result, _) = call(
            json!({ "operation": "hover", "filePath": "gone.rs", "line": 1, "character": 1 }),
            temp.path(),
        )
        .await;
        let error = result.unwrap_err().to_string();
        assert!(error.starts_with("File not found:"), "{error}");
        drop(temp);
    }

    #[tokio::test]
    async fn no_results_message() {
        let temp = temp_with_file("lsp-none", "fn main() {}\n");
        let (result, _) = call(
            json!({ "operation": "findReferences", "filePath": "a.rs", "line": 1, "character": 1 }),
            temp.path(),
        )
        .await;
        assert_eq!(
            result.unwrap().output,
            "No results found for findReferences"
        );
        drop(temp);
    }
}
