//! MCP websearch JSON-RPC client — port of `tool/mcp-websearch.ts`
//! (spec M4.5).

use serde_json::{json, Value};

use crate::tool::error::ToolError;

pub const EXA_URL: &str = "https://mcp.exa.ai/mcp";
pub const PARALLEL_URL: &str = "https://search.parallel.ai/mcp";

/// `EXA_URL` including the api key when `EXA_API_KEY` is set
/// (mcp-websearch.ts:3-6).
pub fn exa_url(exa_api_key: Option<&str>) -> String {
    match exa_api_key {
        Some(key) => format!("https://mcp.exa.ai/mcp?exaApiKey={}", urlencode(key)),
        None => EXA_URL.to_string(),
    }
}

fn urlencode(value: &str) -> String {
    let mut out = String::new();
    for byte in value.bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(byte as char);
            }
            _ => out.push_str(&format!("%{byte:02X}")),
        }
    }
    out
}

/// Minimal HTTP response used by the [`McpHttpClient`] seam.
pub struct McpHttpResponse {
    pub body: String,
}

/// HTTP seam for the MCP client; tests mock it.
pub trait McpHttpClient: Send + Sync {
    fn post<'a>(
        &'a self,
        url: &'a str,
        headers: Vec<(String, String)>,
        body: &'a str,
    ) -> crate::tool::def::BoxFuture<'a, Result<McpHttpResponse, ToolError>>;
}

/// `parsePayload` — a direct JSON MCP result payload → first non-empty
/// `result.content[].text` (mcp-websearch.ts:16-21).
fn parse_payload(payload: &str) -> Option<String> {
    let trimmed = payload.trim();
    if !trimmed.starts_with('{') {
        return None;
    }
    let data: Value = serde_json::from_str(trimmed).ok()?;
    let content = data.get("result")?.get("content")?.as_array()?;
    for item in content {
        if let Some(text) = item.get("text").and_then(Value::as_str) {
            if !text.is_empty() {
                return Some(text.to_string());
            }
        }
    }
    None
}

/// `parseResponse` — direct JSON payload first, then SSE `data: ` lines
/// (mcp-websearch.ts:23-32).
pub fn parse_response(body: &str) -> Option<String> {
    let trimmed = body.trim();
    if !trimmed.is_empty() {
        if let Some(direct) = parse_payload(trimmed) {
            return Some(direct);
        }
    }

    for line in body.split('\n') {
        if !line.starts_with("data: ") {
            continue;
        }
        if let Some(data) = parse_payload(&line[6..]) {
            return Some(data);
        }
    }
    None
}

/// `call` (mcp-websearch.ts:74-102): POST a `tools/call` JSON-RPC request
/// and extract the first content text from the response.
pub async fn call(
    http: &dyn McpHttpClient,
    url: &str,
    tool: &str,
    arguments: Value,
    headers: Vec<(String, String)>,
) -> Result<Option<String>, ToolError> {
    let request = json!({
        "jsonrpc": "2.0",
        "id": 1,
        "method": "tools/call",
        "params": {
            "name": tool,
            "arguments": arguments,
        },
    });
    let mut all_headers = vec![(
        "Accept".to_string(),
        "application/json, text/event-stream".to_string(),
    )];
    all_headers.extend(headers);
    // `Effect.timeoutOrElse` — the caller-supplied budget bounds the whole
    // request so a stalled provider can't hold the agent turn hostage.
    let response = match tokio::time::timeout(
        std::time::Duration::from_secs(25),
        http.post(url, all_headers, &request.to_string()),
    )
    .await
    {
        Ok(response) => response?,
        Err(_) => return Err(ToolError::Failed(format!("{tool} request timed out"))),
    };
    Ok(parse_response(&response.body))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    /// (url, headers, body)
    type RecordedCall = (String, Vec<(String, String)>, String);

    struct FakeHttp {
        bodies: Mutex<Vec<String>>,
        calls: Mutex<Vec<RecordedCall>>,
    }

    impl FakeHttp {
        fn new(bodies: Vec<String>) -> Self {
            Self {
                bodies: Mutex::new(bodies),
                calls: Mutex::new(Vec::new()),
            }
        }

        fn calls(&self) -> Vec<RecordedCall> {
            self.calls.lock().unwrap().clone()
        }
    }

    impl McpHttpClient for FakeHttp {
        fn post<'a>(
            &'a self,
            url: &'a str,
            headers: Vec<(String, String)>,
            body: &'a str,
        ) -> crate::tool::def::BoxFuture<'a, Result<McpHttpResponse, ToolError>> {
            Box::pin(async move {
                self.calls
                    .lock()
                    .unwrap()
                    .push((url.to_string(), headers, body.to_string()));
                let mut bodies = self.bodies.lock().unwrap();
                if bodies.is_empty() {
                    return Err(ToolError::Failed("no body".to_string()));
                }
                Ok(McpHttpResponse {
                    body: bodies.remove(0),
                })
            })
        }
    }

    #[test]
    fn parses_direct_json_payload() {
        let body = r#"{"result":{"content":[{"type":"text","text":"search results here"}]}}"#;
        assert_eq!(
            parse_response(body),
            Some("search results here".to_string())
        );
    }

    #[test]
    fn parses_sse_data_lines() {
        let body = concat!(
            "event: message\n",
            "data: {\"result\":{\"content\":[{\"type\":\"text\",\"text\":\"via sse\"}]}}\n",
            "\n",
        );
        assert_eq!(parse_response(body), Some("via sse".to_string()));
    }

    #[test]
    fn skips_empty_text_entries() {
        let body =
            r#"{"result":{"content":[{"type":"text","text":""},{"type":"text","text":"second"}]}}"#;
        assert_eq!(parse_response(body), Some("second".to_string()));
    }

    #[test]
    fn none_when_no_content() {
        assert_eq!(parse_response("not json"), None);
        assert_eq!(parse_response("{\"other\": 1}"), None);
        assert_eq!(parse_response(""), None);
    }

    #[tokio::test]
    async fn call_sends_jsonrpc_tools_call() {
        let fake = FakeHttp::new(vec![
            r#"{"result":{"content":[{"type":"text","text":"hi"}]}}"#.to_string(),
        ]);
        let result = call(
            &fake,
            "https://mcp.example/mcp",
            "web_search_exa",
            json!({"query": "rust"}),
            vec![("User-Agent".to_string(), "opencode/1.0.0".to_string())],
        )
        .await
        .unwrap();
        assert_eq!(result, Some("hi".to_string()));
        let calls = fake.calls();
        assert_eq!(calls[0].0, "https://mcp.example/mcp");
        let request: Value = serde_json::from_str(&calls[0].2).unwrap();
        assert_eq!(request["jsonrpc"], "2.0");
        assert_eq!(request["id"], 1);
        assert_eq!(request["method"], "tools/call");
        assert_eq!(request["params"]["name"], "web_search_exa");
        assert_eq!(request["params"]["arguments"]["query"], "rust");
    }
}
