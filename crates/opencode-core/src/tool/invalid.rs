//! `invalid` tool — port of `tool/invalid.ts` (spec M4.6).
//!
//! Trivial passthrough: the session loop routes malformed tool calls here so
//! the model sees *why* its arguments were rejected. It always succeeds.

use std::sync::Arc;

use serde::Deserialize;
use serde_json::{json, Value};

use crate::tool::def::{define, Agents, ExecuteResult, ToolCtxRef, ToolDef};
use crate::tool::truncate::Truncate;

#[derive(Debug, Deserialize)]
pub struct InvalidParameters {
    pub tool: String,
    pub error: String,
}

/// Hand-authored JSON Schema (byte-matches the Effect-generated schema; see
/// `tests/golden/toolschema/invalid.json`).
pub fn parameters() -> Value {
    json!({
        "$schema": "https://json-schema.org/draft/2020-12/schema",
        "type": "object",
        "properties": {
            "tool": {
                "type": "string"
            },
            "error": {
                "type": "string"
            }
        },
        "required": [
            "tool",
            "error"
        ]
    })
}

/// Build the `invalid` tool.
pub fn invalid_tool(truncate: Arc<dyn Truncate>, agents: Arc<dyn Agents>) -> ToolDef {
    define(
        "invalid",
        "Do not use",
        parameters(),
        None,
        truncate,
        agents,
        |params: InvalidParameters, _ctx: ToolCtxRef<'_>| {
            Box::pin(async move {
                Ok(ExecuteResult {
                    title: "Invalid Tool".to_string(),
                    output: format!(
                        "The arguments provided to the tool are invalid: {}",
                        params.error
                    ),
                    metadata: json!({}),
                    attachments: None,
                })
            })
        },
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tool::def::Extra;
    use crate::tool::error::ToolError;
    use crate::tool::ripgrep::test_support::{ctx, fixed_agents, instance, RecordingAsk};
    use crate::tool::truncate::TruncateService;
    use serde_json::json;

    fn tool(dir: &std::path::Path) -> ToolDef {
        invalid_tool(
            Arc::new(TruncateService::default_limits(dir.join("tool-output"))),
            fixed_agents(),
        )
    }

    #[test]
    fn golden_parameters_schema() {
        let golden = std::fs::read_to_string(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/tests/golden/toolschema/invalid.json"
        ))
        .unwrap();
        let golden: Value = serde_json::from_str(&golden).unwrap();
        assert_eq!(parameters(), golden);
    }

    #[tokio::test]
    async fn passthrough_output_title_and_metadata() {
        let temp = crate::storage::test_support::TempDir::new("invalid-basic");
        let def = tool(temp.path());
        let ask = RecordingAsk::new();
        let inst = instance(temp.path());
        let extra = Extra::default();
        let ctx = ctx(&ask, &inst, &extra);

        let result = (def.execute)(json!({ "tool": "read", "error": "boom" }), ctx)
            .await
            .unwrap();

        assert_eq!(result.title, "Invalid Tool");
        assert_eq!(
            result.output,
            "The arguments provided to the tool are invalid: boom"
        );
        assert_eq!(
            result.metadata,
            json!({ "truncated": false }),
            "wrap() injects truncated=false into the {{}} metadata"
        );
        assert!(result.attachments.is_none());
        // The invalid tool never asks for permission.
        assert!(ask.requests().is_empty());
        std::fs::remove_dir_all(temp.path()).ok();
    }

    #[tokio::test]
    async fn invalid_arguments_are_byte_exact() {
        let temp = crate::storage::test_support::TempDir::new("invalid-args");
        let def = tool(temp.path());
        let ask = RecordingAsk::new();
        let inst = instance(temp.path());
        let extra = Extra::default();
        let ctx = ctx(&ask, &inst, &extra);

        let err = (def.execute)(json!({ "tool": "read" }), ctx)
            .await
            .unwrap_err();
        assert!(
            matches!(err, ToolError::InvalidArguments { ref tool, .. } if tool == "invalid"),
            "{err:?}"
        );
        std::fs::remove_dir_all(temp.path()).ok();
    }
}
