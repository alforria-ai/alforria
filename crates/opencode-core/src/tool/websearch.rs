//! `websearch` tool — port of `tool/websearch.ts` (spec M4.5).

use std::sync::Arc;

use serde::Deserialize;
use serde_json::{json, Value};

use crate::tool::def::{define, Agents, AskRequest, ExecuteResult, ToolCtxRef, ToolDef};
use crate::tool::error::ToolError;
use crate::tool::mcp_websearch::{self, McpHttpClient};
use crate::tool::truncate::Truncate;

/// `checksum` (`core/src/util/encode.ts`): FNV-1a 32-bit over UTF-16 code
/// units, base-36. `None` for empty input.
pub fn checksum(content: &str) -> Option<String> {
    if content.is_empty() {
        return None;
    }
    let mut hash: u32 = 0x811c_9dc5;
    for ch in content.encode_utf16() {
        hash ^= ch as u32;
        hash = hash.wrapping_mul(0x0100_0193);
    }
    Some(format_radix36(hash))
}

fn format_radix36(mut value: u32) -> String {
    const DIGITS: &[u8; 36] = b"0123456789abcdefghijklmnopqrstuvwxyz";
    let mut out = Vec::new();
    loop {
        out.push(DIGITS[(value % 36) as usize]);
        value /= 36;
        if value == 0 {
            break;
        }
    }
    out.reverse();
    String::from_utf8(out).unwrap()
}

/// Runtime feature flags that steer provider selection.
#[derive(Debug, Clone, Copy, Default)]
pub struct WebSearchFlags {
    pub exa: bool,
    pub parallel: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WebSearchProvider {
    Exa,
    Parallel,
}

/// `selectWebSearchProvider` (websearch.ts:30-37).
pub fn select_websearch_provider(
    session_id: &str,
    flags: WebSearchFlags,
    env_provider: Option<&str>,
) -> WebSearchProvider {
    if env_provider == Some("exa") {
        return WebSearchProvider::Exa;
    }
    if env_provider == Some("parallel") {
        return WebSearchProvider::Parallel;
    }
    if flags.parallel {
        return WebSearchProvider::Parallel;
    }
    if flags.exa {
        return WebSearchProvider::Exa;
    }
    // Number.parseInt(checksum(sessionID) ?? "0", 36) % 2
    let base36 = checksum(session_id).unwrap_or_else(|| "0".to_string());
    let value = u32::from_str_radix(&base36, 36).unwrap_or(0);
    if value.is_multiple_of(2) {
        WebSearchProvider::Exa
    } else {
        WebSearchProvider::Parallel
    }
}

/// `webSearchProviderLabel` (websearch.ts:39-43).
pub fn websearch_provider_label(provider: WebSearchProvider) -> &'static str {
    match provider {
        WebSearchProvider::Parallel => "Parallel Web Search",
        WebSearchProvider::Exa => "Exa Web Search",
    }
}

/// `webSearchModelName` (websearch.ts:45-52) — `extra.model` with
/// `(api.id ?? id).slice(0, 100)`.
pub fn websearch_model_name(extra: &Value) -> Option<String> {
    let model = extra.as_object()?;
    let api = model.get("api").and_then(Value::as_object);
    let api_id = api.and_then(|api| api.get("id")).and_then(Value::as_str);
    let id = model.get("id").and_then(Value::as_str);
    // TS `.slice(0, 100)` counts UTF-16 code units.
    let full = api_id.or(id)?;
    Some(String::from_utf16_lossy(
        &full.encode_utf16().take(100).collect::<Vec<u16>>(),
    ))
}

#[derive(Debug, Deserialize)]
pub struct WebSearchParameters {
    pub query: String,
    #[serde(rename = "numResults")]
    pub num_results: Option<f64>,
    pub livecrawl: Option<String>,
    #[serde(rename = "type")]
    pub kind: Option<String>,
    #[serde(rename = "contextMaxCharacters")]
    pub context_max_characters: Option<f64>,
}

/// JSON numbers preserve integer-ness on the wire: integral f64 emits an
/// integer `Number` (3, not 3.0), matching JS `JSON.stringify`.
fn wire_number(value: f64) -> Value {
    if value.fract() == 0.0 && value.abs() < 9.007199254740992e15 {
        Value::from(value as i64)
    } else {
        Value::from(value)
    }
}

/// Hand-authored JSON Schema (byte-matches the Effect-generated schema; see
/// `tests/golden/toolschema/websearch.json`).
pub fn parameters() -> Value {
    json!({
        "$schema": "https://json-schema.org/draft/2020-12/schema",
        "type": "object",
        "properties": {
            "query": {
                "type": "string",
                "description": "Websearch query"
            },
            "numResults": {
                "type": "number",
                "description": "Number of search results to return (default: 8)"
            },
            "livecrawl": {
                "type": "string",
                "enum": ["fallback", "preferred"],
                "description": "Live crawl mode - 'fallback': use live crawling as backup if cached content unavailable, 'preferred': prioritize live crawling (default: 'fallback')"
            },
            "type": {
                "type": "string",
                "enum": ["auto", "fast", "deep"],
                "description": "Search type - 'auto': balanced search (default), 'fast': quick results, 'deep': comprehensive search"
            },
            "contextMaxCharacters": {
                "type": "number",
                "description": "Maximum characters for context string optimized for LLMs (default: 10000)"
            }
        },
        "required": ["query"]
    })
}

/// How the description's `{{year}}` is resolved — injectable for tests.
pub trait YearProvider: Send + Sync {
    fn year(&self) -> u32;
}

pub struct SystemYear;

impl YearProvider for SystemYear {
    fn year(&self) -> u32 {
        use std::time::{SystemTime, UNIX_EPOCH};
        // 2026-01-01T00:00:00Z
        const START_OF_2026: u64 = 1_767_225_600;
        let elapsed = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);
        (2026 + elapsed.saturating_sub(START_OF_2026) / (365 * 24 * 3600)) as u32
    }
}

/// Environment inputs the tool needs (env vars + version string) — all
/// injectable for tests.
#[derive(Clone)]
pub struct WebSearchEnv {
    pub exa_api_key: Option<String>,
    pub parallel_api_key: Option<String>,
    pub websearch_provider: Option<String>,
    pub version: String,
}

impl Default for WebSearchEnv {
    fn default() -> Self {
        WebSearchEnv {
            exa_api_key: std::env::var("EXA_API_KEY").ok(),
            parallel_api_key: std::env::var("PARALLEL_API_KEY").ok(),
            websearch_provider: std::env::var("OPENCODE_WEBSEARCH_PROVIDER").ok(),
            version: "0.1.0".to_string(),
        }
    }
}

/// Build the `websearch` tool.
pub fn websearch_tool(
    truncate: Arc<dyn Truncate>,
    agents: Arc<dyn Agents>,
    http: Arc<dyn McpHttpClient>,
    flags: WebSearchFlags,
    year: Arc<dyn YearProvider>,
    env: WebSearchEnv,
) -> ToolDef {
    let description =
        include_str!("txt/websearch.txt").replace("{{year}}", &year.year().to_string());
    define(
        "websearch",
        description,
        parameters(),
        None,
        truncate,
        agents,
        move |params: WebSearchParameters, ctx: ToolCtxRef<'_>| {
            let http = http.clone();
            let env = env.clone();
            Box::pin(async move { run(params, ctx, http, flags, env).await })
        },
    )
}

async fn run(
    params: WebSearchParameters,
    ctx: ToolCtxRef<'_>,
    http: Arc<dyn McpHttpClient>,
    flags: WebSearchFlags,
    env: WebSearchEnv,
) -> Result<ExecuteResult, ToolError> {
    let provider =
        select_websearch_provider(ctx.session_id, flags, env.websearch_provider.as_deref());
    let label = websearch_provider_label(provider);

    ctx.metadata
        .metadata(crate::tool::def::MetadataInput {
            title: Some(format!("{label} \"{}\"", params.query)),
            metadata: Some(json!({ "provider": provider_value(provider) })),
        })
        .await?;

    ctx.ask
        .ask(AskRequest {
            permission: "websearch".to_string(),
            patterns: vec![params.query.clone()],
            always: vec!["*".to_string()],
            metadata: json!({
                "query": params.query,
                "numResults": params.num_results.map(wire_number),
                "livecrawl": params.livecrawl,
                "type": params.kind,
                "contextMaxCharacters": params.context_max_characters.map(wire_number),
                "provider": provider_value(provider),
            }),
        })
        .await?;

    let result = call_provider(&*http, provider, &params, ctx.session_id, ctx.extra, &env).await;

    Ok(ExecuteResult {
        output: result.ok().flatten().unwrap_or_else(|| {
            "No search results found. Please try a different query.".to_string()
        }),
        title: format!("{label}: {}", params.query),
        metadata: json!({ "provider": provider_value(provider) }),
        attachments: None,
    })
}

fn provider_value(provider: WebSearchProvider) -> &'static str {
    match provider {
        WebSearchProvider::Exa => "exa",
        WebSearchProvider::Parallel => "parallel",
    }
}

async fn call_provider(
    http: &dyn McpHttpClient,
    provider: WebSearchProvider,
    params: &WebSearchParameters,
    session_id: &str,
    extra: &crate::tool::def::Extra,
    env: &WebSearchEnv,
) -> Result<Option<String>, ToolError> {
    match provider {
        WebSearchProvider::Parallel => {
            let headers = parallel_auth_headers(env);
            let mut args = json!({
                "objective": params.query,
                "search_queries": [params.query],
                "session_id": session_id,
            });
            if let Some(model) =
                websearch_model_name(extra.model.as_ref().unwrap_or(&serde_json::Value::Null))
            {
                if let Some(object) = args.as_object_mut() {
                    object.insert("model_name".to_string(), json!(model));
                }
            }
            mcp_websearch::call(
                http,
                mcp_websearch::PARALLEL_URL,
                "web_search",
                args,
                headers,
            )
            .await
        }
        WebSearchProvider::Exa => {
            let url = mcp_websearch::exa_url(env.exa_api_key.as_deref());
            let mut args = json!({
                "query": params.query,
                "type": params.kind.clone().unwrap_or_else(|| "auto".to_string()),
                "numResults": wire_number(params.num_results.unwrap_or(8.0)),
                "livecrawl": params.livecrawl.clone().unwrap_or_else(|| "fallback".to_string()),
            });
            if let Some(max) = params.context_max_characters {
                if let Some(object) = args.as_object_mut() {
                    object.insert("contextMaxCharacters".to_string(), json!(wire_number(max)));
                }
            }
            mcp_websearch::call(http, &url, "web_search_exa", args, vec![]).await
        }
    }
}

/// `parallelAuthHeaders` (websearch.ts:54-58).
fn parallel_auth_headers(env: &WebSearchEnv) -> Vec<(String, String)> {
    let mut headers = vec![(
        "User-Agent".to_string(),
        format!("opencode/{}", env.version),
    )];
    if let Some(key) = &env.parallel_api_key {
        headers.push(("Authorization".to_string(), format!("Bearer {key}")));
    }
    headers
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tool::def::Extra;
    use crate::tool::ripgrep::test_support::{ctx, fixed_agents, instance, RecordingAsk};
    use crate::tool::truncate::TruncateService;
    use serde_json::json;
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
    }

    impl McpHttpClient for FakeHttp {
        fn post<'a>(
            &'a self,
            url: &'a str,
            headers: Vec<(String, String)>,
            body: &'a str,
        ) -> crate::tool::def::BoxFuture<'a, Result<mcp_websearch::McpHttpResponse, ToolError>>
        {
            Box::pin(async move {
                self.calls
                    .lock()
                    .unwrap()
                    .push((url.to_string(), headers, body.to_string()));
                let mut bodies = self.bodies.lock().unwrap();
                if bodies.is_empty() {
                    return Err(ToolError::Failed("no body".to_string()));
                }
                Ok(mcp_websearch::McpHttpResponse {
                    body: bodies.remove(0),
                })
            })
        }
    }

    struct FixedYear;

    impl YearProvider for FixedYear {
        fn year(&self) -> u32 {
            2026
        }
    }

    fn env() -> WebSearchEnv {
        WebSearchEnv {
            exa_api_key: None,
            parallel_api_key: None,
            websearch_provider: None,
            version: "1.18.31".to_string(),
        }
    }

    fn tool(http: Arc<dyn McpHttpClient>, flags: WebSearchFlags, e: WebSearchEnv) -> ToolDef {
        websearch_tool(
            Arc::new(TruncateService::default_limits(std::path::PathBuf::from(
                "/tmp/opencode",
            ))),
            fixed_agents(),
            http,
            flags,
            Arc::new(FixedYear),
            e,
        )
    }

    async fn call(
        http: Arc<dyn McpHttpClient>,
        flags: WebSearchFlags,
        e: WebSearchEnv,
        args: Value,
    ) -> (
        Result<ExecuteResult, ToolError>,
        Vec<crate::tool::def::AskRequest>,
        Vec<crate::tool::def::MetadataInput>,
    ) {
        let def = tool(http, flags, e);
        let ask = RecordingAsk::new();
        let inst = instance(std::path::Path::new("/tmp/opencode"));
        let extra = Extra::default();
        let ctx = ctx(&ask, &inst, &extra);
        let result = (def.execute)(args, ctx).await;
        let metadata = ask.metadata_calls.lock().unwrap().clone();
        (result, ask.requests(), metadata)
    }

    fn exa_result(text: &str) -> String {
        format!(r#"{{"result":{{"content":[{{"type":"text","text":"{text}"}}]}}}}"#)
    }

    #[test]
    fn golden_parameters_schema() {
        let golden = std::fs::read_to_string(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/tests/golden/toolschema/websearch.json"
        ))
        .unwrap();
        let golden: Value = serde_json::from_str(&golden).unwrap();
        assert_eq!(parameters(), golden);
    }

    #[test]
    fn checksum_fnv1a_base36() {
        // Deterministic and stable per input.
        assert_eq!(checksum(""), None);
        assert!(checksum("ses_1").is_some());
        // Same input → same hash.
        assert_eq!(checksum("ses_1"), checksum("ses_1"));
        // Different inputs differ.
        assert_ne!(checksum("ses_1"), checksum("ses_2"));
    }

    #[test]
    fn provider_selection_determinism() {
        let none = WebSearchFlags::default();
        // Env override wins.
        assert_eq!(
            select_websearch_provider("x", none, Some("exa")),
            WebSearchProvider::Exa
        );
        assert_eq!(
            select_websearch_provider("x", none, Some("parallel")),
            WebSearchProvider::Parallel
        );
        // Flags before checksum.
        assert_eq!(
            select_websearch_provider(
                "x",
                WebSearchFlags {
                    exa: true,
                    parallel: false
                },
                None
            ),
            WebSearchProvider::Exa
        );
        assert_eq!(
            select_websearch_provider(
                "x",
                WebSearchFlags {
                    exa: false,
                    parallel: true
                },
                None
            ),
            WebSearchProvider::Parallel
        );
        // Checksum is stable per session id.
        for id in ["ses_a", "ses_b", "ses_c", "ses_d"] {
            let expected = select_websearch_provider(id, none, None);
            for _ in 0..10 {
                assert_eq!(select_websearch_provider(id, none, None), expected);
            }
        }
        // Both parities are reachable.
        let mut seen_exa = false;
        let mut seen_parallel = false;
        for i in 0..50 {
            match select_websearch_provider(&format!("ses_{i}"), none, None) {
                WebSearchProvider::Exa => seen_exa = true,
                WebSearchProvider::Parallel => seen_parallel = true,
            }
        }
        assert!(seen_exa && seen_parallel);
    }

    #[test]
    fn year_substituted_in_description() {
        let def = tool(
            Arc::new(FakeHttp::new(vec![])),
            WebSearchFlags::default(),
            env(),
        );
        assert!(def.description.contains("The current year is 2026"));
        assert!(!def.description.contains("{{year}}"));
    }

    #[tokio::test]
    async fn exa_call_shape_and_output() {
        let http = Arc::new(FakeHttp::new(vec![exa_result("rust results")]));
        let http_dyn: Arc<dyn McpHttpClient> = http.clone();
        let (result, asks, metadata) = call(
            http_dyn.clone(),
            WebSearchFlags::default(),
            env(),
            json!({ "query": "rust lang", "numResults": 3, "type": "deep", "livecrawl": "preferred", "contextMaxCharacters": 500 }),
        )
        .await;
        let result = result.unwrap();
        assert_eq!(result.output, "rust results");
        assert_eq!(result.title, "Exa Web Search: rust lang");
        assert_eq!(result.metadata["provider"], json!("exa"));
        assert_eq!(asks.len(), 1);
        assert_eq!(asks[0].permission, "websearch");
        assert_eq!(asks[0].metadata["numResults"], json!(3));
        assert_eq!(
            metadata[0].title,
            Some("Exa Web Search \"rust lang\"".to_string())
        );

        let calls = http.calls.lock().unwrap().clone();
        assert_eq!(calls[0].0, "https://mcp.exa.ai/mcp");
        let request: Value = serde_json::from_str(&calls[0].2).unwrap();
        assert_eq!(request["params"]["name"], "web_search_exa");
        assert_eq!(request["params"]["arguments"]["query"], "rust lang");
        assert_eq!(request["params"]["arguments"]["numResults"], 3);
        assert_eq!(request["params"]["arguments"]["type"], "deep");
        assert_eq!(request["params"]["arguments"]["livecrawl"], "preferred");
    }

    #[tokio::test]
    async fn exa_default_args() {
        let http = Arc::new(FakeHttp::new(vec![exa_result("x")]));
        let http_dyn: Arc<dyn McpHttpClient> = http.clone();
        let _ = call(
            http_dyn,
            WebSearchFlags::default(),
            env(),
            json!({ "query": "q" }),
        )
        .await;
        let calls = http.calls.lock().unwrap().clone();
        let request: Value = serde_json::from_str(&calls[0].2).unwrap();
        assert_eq!(request["params"]["arguments"]["numResults"], 8);
        assert_eq!(request["params"]["arguments"]["type"], "auto");
        assert_eq!(request["params"]["arguments"]["livecrawl"], "fallback");
    }

    #[tokio::test]
    async fn parallel_call_shape() {
        let mut e = env();
        e.parallel_api_key = Some("pk_123".to_string());
        e.websearch_provider = Some("parallel".to_string());
        let http = Arc::new(FakeHttp::new(vec![exa_result("via parallel")]));
        let http_dyn: Arc<dyn McpHttpClient> = http.clone();
        let (result, _, _) = call(
            http_dyn,
            WebSearchFlags::default(),
            e,
            json!({ "query": "test q" }),
        )
        .await;
        let result = result.unwrap();
        assert_eq!(result.output, "via parallel");
        assert_eq!(result.title, "Parallel Web Search: test q");
        let calls = http.calls.lock().unwrap().clone();
        assert_eq!(calls[0].0, "https://search.parallel.ai/mcp");
        let headers: Vec<(String, String)> = calls[0].1.clone();
        assert_eq!(
            headers.iter().find(|(k, _)| k == "User-Agent").unwrap().1,
            "opencode/1.18.31"
        );
        assert_eq!(
            headers
                .iter()
                .find(|(k, _)| k == "Authorization")
                .unwrap()
                .1,
            "Bearer pk_123"
        );
        let request: Value = serde_json::from_str(&calls[0].2).unwrap();
        assert_eq!(request["params"]["name"], "web_search");
        assert_eq!(request["params"]["arguments"]["objective"], "test q");
        assert_eq!(
            request["params"]["arguments"]["search_queries"],
            json!(["test q"])
        );
        assert_eq!(request["params"]["arguments"]["session_id"], "ses_1");
    }

    #[tokio::test]
    async fn empty_result_fallback_text() {
        let http: Arc<dyn McpHttpClient> =
            Arc::new(FakeHttp::new(vec!["no json here".to_string()]));
        let (result, _, _) = call(
            http,
            WebSearchFlags::default(),
            env(),
            json!({ "query": "nothing" }),
        )
        .await;
        assert_eq!(
            result.unwrap().output,
            "No search results found. Please try a different query."
        );
    }

    #[test]
    fn model_name_slice() {
        let long_id = "x".repeat(150);
        let model = json!({ "id": long_id, "api": { "id": "api-id" } });
        assert_eq!(websearch_model_name(&model), Some("api-id".to_string()));
        let model = json!({ "id": long_id });
        assert_eq!(websearch_model_name(&model), Some("x".repeat(100)));
        assert_eq!(websearch_model_name(&json!(null)), None);
    }
}
