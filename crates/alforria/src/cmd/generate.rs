//! cli/cmd/generate.ts port — print the OpenAPI spec JSON with
//! `x-codeSamples` injected (generate.ts:5-54).

use clap::ArgMatches;
use serde_json::{json, Value};

use crate::error::TypedError;
use crate::ui::Ui;

/// The `x-codeSamples` js source (generate.ts:18-27) — note the unterminated
/// string literal is verbatim from the TS.
const SAMPLE_SOURCE: &str = "import { createOpencodeClient } from \"@opencode-ai/sdk\n\nconst client = createOpencodeClient()\nawait client.OPERATION({\n  ...\n})";

pub fn generate_document(spec: Value) -> Value {
    let Value::Object(mut document) = spec else {
        return spec;
    };
    let Some(Value::Object(paths)) = document.get("paths").cloned() else {
        return Value::Object(document);
    };
    let mut sanitized_paths = serde_json::Map::new();
    for (path, item) in paths {
        let mut item = match item {
            Value::Object(item) => item,
            other => {
                sanitized_paths.insert(path, other);
                continue;
            }
        };
        for method in ["get", "post", "put", "delete", "patch"] {
            let operation = item
                .get_mut(method)
                .and_then(|value| value.as_object_mut())
                .filter(|operation| operation.get("operationId").is_some_and(Value::is_string));
            let Some(operation) = operation else {
                continue;
            };
            operation.insert(
                "x-codeSamples".to_string(),
                json!([{
                    "lang": "js",
                    "source": SAMPLE_SOURCE.replace(
                        "OPERATION",
                        operation["operationId"].as_str().unwrap_or_default(),
                    ),
                }]),
            );
        }
        sanitized_paths.insert(path, Value::Object(item));
    }
    document.insert("paths".to_string(), Value::Object(sanitized_paths));
    Value::Object(document)
}

pub fn run(_matches: &ArgMatches, ui: &mut Ui) -> Result<(), TypedError> {
    let spec: Value = serde_json::from_str(alforria_server::openapi::V1_DOC)
        .map_err(|err| TypedError::Cli(crate::error::CliError::new(err.to_string())))?;
    let spec = generate_document(spec);
    let json = serde_json::to_string_pretty(&spec).unwrap_or_default();
    ui.write_stdout(&format!("{json}\n"));
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn injects_samples_for_operations_with_ids() {
        let spec = json!({
            "openapi": "3.1.0",
            "paths": {
                "/session": {
                    "get": {"operationId": "session.list"},
                    "post": {"operationId": "session.create"},
                    "delete": {},
                },
                "/other": {"put": {}},
            },
        });
        let generated = generate_document(spec);
        let get = &generated["paths"]["/session"]["get"];
        assert_eq!(get["x-codeSamples"][0]["lang"], "js");
        assert!(
            get["x-codeSamples"][0]["source"]
                .as_str()
                .unwrap()
                .contains("await client.session.list({"),
            "{}",
            get["x-codeSamples"][0]["source"]
        );
        assert!(generated["paths"]["/session"]["delete"]
            .get("x-codeSamples")
            .is_none());
        assert!(generated["paths"]["/other"]["put"]
            .get("x-codeSamples")
            .is_none());
    }

    #[test]
    fn v1_doc_has_code_samples_after_generation() {
        let spec: Value = serde_json::from_str(alforria_server::openapi::V1_DOC).unwrap();
        let generated = generate_document(spec);
        let paths = generated["paths"].as_object().unwrap();
        let mut sampled = 0;
        for item in paths.values() {
            for method in ["get", "post", "put", "delete", "patch"] {
                if let Some(operation) = item.get(method) {
                    if operation.get("operationId").is_some() {
                        assert!(
                            operation.get("x-codeSamples").is_some(),
                            "missing samples on {method}"
                        );
                        sampled += 1;
                    }
                }
            }
        }
        assert!(sampled > 10, "sampled {sampled} operations");
    }
}
