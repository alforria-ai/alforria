//! `webfetch` tool — port of `tool/webfetch.ts` (spec M4.5).
//!
//! Fetches a URL and converts the response to markdown/text/html. HTTP runs
//! through the [`HttpClient`] seam so tests never touch the network.

use std::sync::Arc;

use serde::Deserialize;
use serde_json::{json, Value};

use crate::tool::def::{define, Agents, AskRequest, BoxFuture, ExecuteResult, ToolCtxRef, ToolDef};
use crate::tool::error::ToolError;
use crate::tool::truncate::Truncate;

const MAX_RESPONSE_SIZE: usize = 5 * 1024 * 1024; // 5MB
const DEFAULT_TIMEOUT_MS: u64 = 30 * 1000; // 30 seconds
const MAX_TIMEOUT_MS: u64 = 120 * 1000; // 2 minutes

const USER_AGENT: &str = "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/143.0.0.0 Safari/537.36";

/// `isImageAttachment` (`util/media.ts`).
const IMAGE_MIMES: [&str; 6] = [
    "image/png",
    "image/jpeg",
    "image/gif",
    "image/webp",
    "image/avif",
    "image/svg+xml",
];

/// Minimal HTTP response used by the [`HttpClient`] seam.
pub struct HttpResponse {
    pub status: u16,
    pub headers: Vec<(String, String)>,
    pub body: Vec<u8>,
}

impl HttpResponse {
    fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(key, _)| key.eq_ignore_ascii_case(name))
            .map(|(_, value)| value.as_str())
    }

    fn content_type(&self) -> &str {
        self.header("content-type").unwrap_or("")
    }
}

/// HTTP seam (`HttpClient.HttpClient` in webfetch.ts). Production wires
/// reqwest; tests mock it.
pub trait HttpClient: Send + Sync {
    fn get<'a>(
        &'a self,
        url: &'a str,
        headers: Vec<(&'a str, &'a str)>,
    ) -> BoxFuture<'a, Result<HttpResponse, ToolError>>;
}

struct ReqwestClient {
    client: reqwest::Client,
}

impl HttpClient for ReqwestClient {
    fn get<'a>(
        &'a self,
        url: &'a str,
        headers: Vec<(&'a str, &'a str)>,
    ) -> BoxFuture<'a, Result<HttpResponse, ToolError>> {
        Box::pin(async move {
            let mut request = self.client.get(url);
            for (key, value) in headers {
                request = request.header(key, value);
            }
            let response = request
                .send()
                .await
                .map_err(|err| ToolError::Failed(err.to_string()))?;
            let status = response.status().as_u16();
            let mut header_map = Vec::new();
            for (key, value) in response.headers().iter() {
                if let Ok(value) = value.to_str() {
                    header_map.push((key.as_str().to_string(), value.to_string()));
                }
            }
            let body = response
                .bytes()
                .await
                .map_err(|err| ToolError::Failed(err.to_string()))?;
            Ok(HttpResponse {
                status,
                headers: header_map,
                body: body.to_vec(),
            })
        })
    }
}

/// Production [`HttpClient`] over reqwest.
pub fn reqwest_client() -> Arc<dyn HttpClient> {
    Arc::new(ReqwestClient {
        client: reqwest::Client::new(),
    })
}

#[derive(Debug, Deserialize)]
pub struct WebFetchParameters {
    pub url: String,
    /// `text` | `markdown` | `html` (decoding default: markdown).
    pub format: Option<String>,
    /// Optional timeout in seconds (max 120).
    pub timeout: Option<f64>,
}

/// Hand-authored JSON Schema (byte-matches the Effect-generated schema; see
/// `tests/golden/toolschema/webfetch.json`).
pub fn parameters() -> Value {
    json!({
        "$schema": "https://json-schema.org/draft/2020-12/schema",
        "type": "object",
        "properties": {
            "url": {
                "type": "string",
                "description": "The URL to fetch content from"
            },
            "format": {
                "type": "string",
                "enum": ["text", "markdown", "html"],
                "description": "The format to return the content in (text, markdown, or html). Defaults to markdown.",
                "default": "markdown"
            },
            "timeout": {
                "type": "number",
                "description": "Optional timeout in seconds (max 120)"
            }
        },
        "required": ["url"]
    })
}

/// Build the `webfetch` tool.
pub fn webfetch_tool(
    truncate: Arc<dyn Truncate>,
    agents: Arc<dyn Agents>,
    http: Arc<dyn HttpClient>,
) -> ToolDef {
    define(
        "webfetch",
        include_str!("txt/webfetch.txt"),
        parameters(),
        None,
        truncate,
        agents,
        move |params: WebFetchParameters, ctx: ToolCtxRef<'_>| {
            let http = http.clone();
            Box::pin(async move { run(params, ctx, http).await })
        },
    )
}

fn accept_header(format: &str) -> &'static str {
    match format {
        "markdown" => {
            "text/markdown;q=1.0, text/x-markdown;q=0.9, text/plain;q=0.8, text/html;q=0.7, */*;q=0.1"
        }
        "text" => "text/plain;q=1.0, text/markdown;q=0.9, text/html;q=0.8, */*;q=0.1",
        "html" => {
            "text/html;q=1.0, application/xhtml+xml;q=0.9, text/plain;q=0.8, text/markdown;q=0.7, */*;q=0.1"
        }
        _ => "text/html,application/xhtml+xml,application/xml;q=0.9,image/avif,image/webp,image/apng,*/*;q=0.8",
    }
}

async fn run(
    params: WebFetchParameters,
    ctx: ToolCtxRef<'_>,
    http: Arc<dyn HttpClient>,
) -> Result<ExecuteResult, ToolError> {
    {
        if !params.url.starts_with("http://") && !params.url.starts_with("https://") {
            return Err(ToolError::Failed(
                "URL must start with http:// or https://".to_string(),
            ));
        }

        let format = params.format.as_deref().unwrap_or("markdown");
        let timeout_secs = params.timeout.unwrap_or((DEFAULT_TIMEOUT_MS / 1000) as f64);
        let _timeout_ms = ((timeout_secs * 1000.0) as u64).min(MAX_TIMEOUT_MS);

        ctx.ask
            .ask(AskRequest {
                permission: "webfetch".to_string(),
                patterns: vec![params.url.clone()],
                always: vec!["*".to_string()],
                metadata: json!({
                    "url": params.url,
                    "format": format,
                    "timeout": params.timeout,
                }),
            })
            .await?;

        let headers = vec![
            ("User-Agent", USER_AGENT),
            ("Accept", accept_header(format)),
            ("Accept-Language", "en-US,en;q=0.9"),
        ];
        let response = http.get(&params.url, headers).await?;

        // Check content length (webfetch.ts:96-104).
        if let Some(content_length) = response.header("content-length") {
            if let Ok(length) = content_length.trim().parse::<usize>() {
                if length > MAX_RESPONSE_SIZE {
                    return Err(ToolError::Failed(
                        "Response too large (exceeds 5MB limit)".to_string(),
                    ));
                }
            }
        }
        if response.body.len() > MAX_RESPONSE_SIZE {
            return Err(ToolError::Failed(
                "Response too large (exceeds 5MB limit)".to_string(),
            ));
        }

        let content_type = response.content_type().to_string();
        let mime = content_type
            .split(';')
            .next()
            .unwrap_or_default()
            .trim()
            .to_lowercase();
        let title = format!("{} ({})", params.url, content_type);

        if IMAGE_MIMES.contains(&mime.as_str()) {
            use base64::Engine as _;
            let base64_content = base64::engine::general_purpose::STANDARD.encode(&response.body);
            return Ok(ExecuteResult {
                title,
                output: "Image fetched successfully".to_string(),
                metadata: json!({}),
                attachments: Some(vec![crate::tool::def::Attachment {
                    kind: "file",
                    mime: mime.clone(),
                    url: format!("data:{mime};base64,{base64_content}"),
                    filename: None,
                }]),
            });
        }

        let content = String::from_utf8_lossy(&response.body).into_owned();

        let output = match format {
            "markdown" => {
                if content_type.contains("text/html") {
                    convert_html_to_markdown(&content)
                } else {
                    content
                }
            }
            "text" => {
                if content_type.contains("text/html") {
                    extract_text_from_html(&content)
                } else {
                    content
                }
            }
            _ => content,
        };

        Ok(ExecuteResult {
            title,
            output,
            metadata: json!({}),
            attachments: None,
        })
    }
}

// ---------------------------------------------------------------------------
// HTML processing (webfetch.ts:158-192)
// ---------------------------------------------------------------------------

const SKIP_TEXT_TAGS: [&str; 6] = ["script", "style", "noscript", "iframe", "object", "embed"];
const REMOVE_MARKDOWN_TAGS: [&str; 4] = ["script", "style", "meta", "link"];

/// `extractTextFromHTML` — text nodes with script/style/noscript/iframe/
/// object/embed subtrees skipped; any close tag decrements the skip depth
/// (htmlparser2 skip counting, webfetch.ts:158-180).
pub fn extract_text_from_html(html: &str) -> String {
    let mut text = String::new();
    let mut skip_depth = 0usize;
    for token in tokenize(html) {
        match token {
            Token::Text(raw) => {
                if skip_depth == 0 {
                    text.push_str(&decode_entities(&raw));
                }
            }
            Token::Open(tag, _) => {
                if skip_depth > 0 || SKIP_TEXT_TAGS.contains(&tag.as_str()) {
                    skip_depth += 1;
                }
            }
            Token::Close(_) => {
                skip_depth = skip_depth.saturating_sub(1);
            }
        }
    }
    text.trim().to_string()
}

/// `convertHTMLToMarkdown` — turndown with headingStyle atx, hr `---`,
/// bullet `-`, fenced code blocks, `*` em, script/style/meta/link removed
/// (webfetch.ts:182-192). A pragmatic port covering the common HTML surface.
pub fn convert_html_to_markdown(html: &str) -> String {
    let converter = HtmlToMarkdown::new();
    converter.convert(html)
}

struct HtmlToMarkdown {
    out: String,
    skip_depth: usize,
    pre_depth: usize,
    /// (start index in `out`, href) for the currently open `<a>`.
    link: Option<(usize, String)>,
}

impl HtmlToMarkdown {
    fn new() -> Self {
        Self {
            out: String::new(),
            skip_depth: 0,
            pre_depth: 0,
            link: None,
        }
    }

    fn convert(mut self, html: &str) -> String {
        for token in tokenize(html) {
            match token {
                Token::Text(raw) => {
                    if self.skip_depth == 0 {
                        let decoded = decode_entities(&raw);
                        if self.pre_depth > 0 {
                            self.out.push_str(&decoded);
                        } else {
                            self.out.push_str(&decoded.replace('\n', " "));
                        }
                    }
                }
                Token::Open(tag, raw) => self.open_tag(&tag, &raw),
                Token::Close(tag) => self.close_tag(&tag),
            }
        }
        self.out.trim().to_string()
    }

    fn open_tag(&mut self, tag: &str, raw: &str) {
        if REMOVE_MARKDOWN_TAGS.contains(&tag) || SKIP_TEXT_TAGS.contains(&tag) {
            self.skip_depth += 1;
            return;
        }
        if self.skip_depth > 0 {
            return;
        }
        match tag {
            "pre" => {
                self.pre_depth += 1;
                self.out.push_str("\n```\n");
            }
            "code" => self.out.push('`'),
            "em" | "i" => self.out.push('*'),
            "strong" | "b" => self.out.push_str("**"),
            "h1" | "h2" | "h3" | "h4" | "h5" | "h6" => {
                let level = tag.chars().nth(1).unwrap_or('1').to_digit(10).unwrap_or(1);
                self.out
                    .push_str(&format!("\n\n{} ", "#".repeat(level as usize)));
            }
            "li" => self.out.push_str("\n- "),
            "blockquote" => self.out.push_str("\n\n> "),
            "br" => self.out.push('\n'),
            "hr" => self.out.push_str("\n\n---\n\n"),
            "img" => {
                if let (Some(alt), Some(src)) = (attr_value(raw, "alt"), attr_value(raw, "src")) {
                    self.out.push_str(&format!("![{alt}]({src})"));
                }
            }
            "a" => {
                self.link = Some((self.out.len(), attr_value(raw, "href").unwrap_or_default()));
            }
            "p" | "div" | "section" | "article" | "header" | "footer" | "main" => {
                self.out.push_str("\n\n");
            }
            "td" | "th" => self.out.push_str(" | "),
            "tr" => self.out.push('\n'),
            _ => {}
        }
    }

    fn close_tag(&mut self, tag: &str) {
        if SKIP_TEXT_TAGS.contains(&tag) || REMOVE_MARKDOWN_TAGS.contains(&tag) {
            if self.skip_depth > 0 {
                self.skip_depth -= 1;
            }
            return;
        }
        if self.skip_depth > 0 {
            return;
        }
        match tag {
            "pre" => {
                self.pre_depth = self.pre_depth.saturating_sub(1);
                self.out.push_str("\n```\n");
            }
            "code" => self.out.push('`'),
            "em" | "i" => self.out.push('*'),
            "strong" | "b" => self.out.push_str("**"),
            "h1" | "h2" | "h3" | "h4" | "h5" | "h6" => self.out.push_str("\n\n"),
            "blockquote" => self.out.push('\n'),
            "a" => {
                if let Some((start, href)) = self.link.take() {
                    let text = self.out[start..].trim().to_string();
                    if !text.is_empty() && !href.is_empty() {
                        self.out.truncate(start);
                        self.out.push_str(&format!("[{text}]({href})"));
                    }
                }
            }
            "li" | "ul" | "ol" => self.out.push('\n'),
            _ => {}
        }
    }
}

fn attr_value(raw: &str, name: &str) -> Option<String> {
    let lower = raw.to_lowercase();
    let needle = format!("{name}=");
    if let Some(at) = lower.find(&needle) {
        let rest = &raw[at + needle.len()..];
        if let Some(stripped) = rest.strip_prefix('"') {
            return stripped.split('"').next().map(str::to_string);
        }
        if let Some(stripped) = rest.strip_prefix('\'') {
            return stripped.split('\'').next().map(str::to_string);
        }
        return rest.split_whitespace().next().map(str::to_string);
    }
    None
}

enum Token {
    Text(String),
    Open(String, String),
    Close(String),
}

fn tokenize(html: &str) -> Vec<Token> {
    let mut tokens = Vec::new();
    let mut pos = 0usize;
    while pos < html.len() {
        if let Some(at) = html[pos..].find('<') {
            if at > 0 {
                tokens.push(Token::Text(html[pos..pos + at].to_string()));
            }
            let tag_start = pos + at;
            let end = html[tag_start..]
                .find('>')
                .map(|i| tag_start + i + 1)
                .unwrap_or(html.len());
            let raw = html[tag_start..end].to_string();
            let inner = raw
                .strip_prefix('<')
                .unwrap_or(&raw)
                .strip_suffix('>')
                .unwrap_or(&raw)
                .trim();
            if let Some(name) = inner.strip_prefix('/') {
                tokens.push(Token::Close(
                    name.split_whitespace().next().unwrap_or("").to_lowercase(),
                ));
            } else if !inner.starts_with('!') && !inner.starts_with('?') {
                if let Some(name) = inner.split_whitespace().next() {
                    tokens.push(Token::Open(name.to_lowercase(), raw.clone()));
                }
            }
            pos = end;
        } else {
            tokens.push(Token::Text(html[pos..].to_string()));
            pos = html.len();
        }
    }
    tokens
}

fn decode_entity(entity: &str) -> Option<&'static str> {
    match entity {
        "&amp;" => Some("&"),
        "&lt;" => Some("<"),
        "&gt;" => Some(">"),
        "&quot;" => Some("\""),
        "&#39;" => Some("'"),
        "&apos;" => Some("'"),
        "&nbsp;" => Some("\u{a0}"),
        _ => None,
    }
}

fn decode_entities(text: &str) -> String {
    if !text.contains('&') {
        return text.to_string();
    }
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(at) = rest.find('&') {
        out.push_str(&rest[..at]);
        if let Some(semi) = rest[at..].find(';') {
            let entity = &rest[at..=at + semi];
            match decode_entity(entity) {
                Some(decoded) => out.push_str(decoded),
                None => out.push_str(entity),
            }
            rest = &rest[at + semi + 1..];
        } else {
            out.push('&');
            rest = &rest[at + 1..];
        }
    }
    out.push_str(rest);
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tool::def::Extra;
    use crate::tool::ripgrep::test_support::{ctx, fixed_agents, instance, RecordingAsk};
    use crate::tool::truncate::TruncateService;
    use serde_json::json;
    use std::sync::Mutex;

    struct FakeHttp {
        responses: Mutex<Vec<HttpResponse>>,
        calls: Mutex<Vec<RecordedCall>>,
    }

    /// (url, headers)
    type RecordedCall = (String, Vec<(String, String)>);

    impl FakeHttp {
        fn new(responses: Vec<HttpResponse>) -> Self {
            Self {
                responses: Mutex::new(responses),
                calls: Mutex::new(Vec::new()),
            }
        }

        fn calls(&self) -> Vec<RecordedCall> {
            self.calls.lock().unwrap().clone()
        }
    }

    impl HttpClient for FakeHttp {
        fn get<'a>(
            &'a self,
            url: &'a str,
            headers: Vec<(&'a str, &'a str)>,
        ) -> BoxFuture<'a, Result<HttpResponse, ToolError>> {
            Box::pin(async move {
                self.calls.lock().unwrap().push((
                    url.to_string(),
                    headers
                        .into_iter()
                        .map(|(k, v)| (k.to_string(), v.to_string()))
                        .collect(),
                ));
                let mut responses = self.responses.lock().unwrap();
                if responses.is_empty() {
                    return Err(ToolError::Failed("no more responses".to_string()));
                }
                Ok(responses.remove(0))
            })
        }
    }

    fn tool(http: Arc<dyn HttpClient>) -> ToolDef {
        webfetch_tool(
            Arc::new(TruncateService::default_limits(std::path::PathBuf::from(
                "/tmp/opencode",
            ))),
            fixed_agents(),
            http,
        )
    }

    async fn call(
        http: Arc<dyn HttpClient>,
        args: Value,
    ) -> (
        Result<ExecuteResult, ToolError>,
        Vec<crate::tool::def::AskRequest>,
    ) {
        let def = tool(http);
        let ask = RecordingAsk::new();
        let inst = instance(std::path::Path::new("/tmp/opencode"));
        let extra = Extra::default();
        let ctx = ctx(&ask, &inst, &extra);
        let result = (def.execute)(args, ctx).await;
        (result, ask.requests())
    }

    fn response(status: u16, headers: Vec<(&str, &str)>, body: &str) -> HttpResponse {
        HttpResponse {
            status,
            headers: headers
                .into_iter()
                .map(|(k, v)| (k.to_string(), v.to_string()))
                .collect(),
            body: body.as_bytes().to_vec(),
        }
    }

    #[test]
    fn golden_parameters_schema() {
        let golden = std::fs::read_to_string(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/tests/golden/toolschema/webfetch.json"
        ))
        .unwrap();
        let golden: Value = serde_json::from_str(&golden).unwrap();
        assert_eq!(parameters(), golden);
    }

    #[tokio::test]
    async fn rejects_non_http_url() {
        let http: Arc<dyn HttpClient> = Arc::new(FakeHttp::new(vec![]));
        let (result, _) = call(http.clone(), json!({ "url": "ftp://example.com" })).await;
        assert_eq!(
            result.unwrap_err().to_string(),
            "URL must start with http:// or https://"
        );
    }

    #[tokio::test]
    async fn asks_permission_and_sends_headers() {
        let fake = Arc::new(FakeHttp::new(vec![response(
            200,
            vec![("content-type", "text/plain")],
            "hello",
        )]));
        let (result, asks) = call(fake.clone(), json!({ "url": "https://example.com" })).await;
        assert!(result.is_ok(), "{}", result.unwrap_err());
        assert_eq!(asks.len(), 1);
        assert_eq!(asks[0].permission, "webfetch");
        let calls = fake.calls();
        assert_eq!(
            calls[0]
                .1
                .iter()
                .find(|(k, _)| k == "User-Agent")
                .unwrap()
                .1,
            USER_AGENT
        );
        assert_eq!(
            calls[0].1.iter().find(|(k, _)| k == "Accept").unwrap().1,
            "text/markdown;q=1.0, text/x-markdown;q=0.9, text/plain;q=0.8, text/html;q=0.7, */*;q=0.1"
        );
        assert_eq!(
            calls[0]
                .1
                .iter()
                .find(|(k, _)| k == "Accept-Language")
                .unwrap()
                .1,
            "en-US,en;q=0.9"
        );
    }

    #[tokio::test]
    async fn html_to_markdown_conversion() {
        let fake: Arc<dyn HttpClient> = Arc::new(FakeHttp::new(vec![response(
            200,
            vec![("content-type", "text/html; charset=utf-8")],
            "<html><head><style>.x{}</style></head><body><h1>Title</h1><p>Hello <em>world</em></p><script>evil()</script></body></html>",
        )]));
        let (result, _) = call(fake.clone(), json!({ "url": "https://example.com" })).await;
        let result = result.unwrap();
        assert_eq!(
            result.title,
            "https://example.com (text/html; charset=utf-8)"
        );
        assert!(result.output.contains("# Title"), "{}", result.output);
        assert!(result.output.contains("Hello *world*"), "{}", result.output);
        assert!(!result.output.contains("evil"));
    }

    #[tokio::test]
    async fn text_extraction_strips_scripts() {
        let fake: Arc<dyn HttpClient> = Arc::new(FakeHttp::new(vec![response(
            200,
            vec![("content-type", "text/html")],
            "<p>keep</p><script>drop()</script><style>.x{}</style>",
        )]));
        let (result, _) = call(
            fake.clone(),
            json!({ "url": "https://example.com", "format": "text" }),
        )
        .await;
        let result = result.unwrap();
        assert!(result.output.contains("keep"), "{}", result.output);
        assert!(!result.output.contains("drop"));
    }

    #[tokio::test]
    async fn raw_html_passthrough() {
        let fake: Arc<dyn HttpClient> = Arc::new(FakeHttp::new(vec![response(
            200,
            vec![("content-type", "text/html")],
            "<p>raw &amp; content</p>",
        )]));
        let (result, _) = call(
            fake.clone(),
            json!({ "url": "https://example.com", "format": "html" }),
        )
        .await;
        assert_eq!(result.unwrap().output, "<p>raw &amp; content</p>");
    }

    #[tokio::test]
    async fn content_length_cap() {
        let fake: Arc<dyn HttpClient> = Arc::new(FakeHttp::new(vec![response(
            200,
            vec![
                ("content-type", "text/plain"),
                ("content-length", "999999999"),
            ],
            "x",
        )]));
        let (result, _) = call(fake.clone(), json!({ "url": "https://example.com" })).await;
        assert_eq!(
            result.unwrap_err().to_string(),
            "Response too large (exceeds 5MB limit)"
        );
    }

    #[tokio::test]
    async fn body_cap() {
        let fake: Arc<dyn HttpClient> = Arc::new(FakeHttp::new(vec![response(
            200,
            vec![("content-type", "text/plain")],
            &"x".repeat(5 * 1024 * 1024 + 1),
        )]));
        let (result, _) = call(fake.clone(), json!({ "url": "https://example.com" })).await;
        assert_eq!(
            result.unwrap_err().to_string(),
            "Response too large (exceeds 5MB limit)"
        );
    }

    #[tokio::test]
    async fn image_attachment() {
        let fake: Arc<dyn HttpClient> = Arc::new(FakeHttp::new(vec![response(
            200,
            vec![("content-type", "image/png")],
            "PNGDATA",
        )]));
        let (result, _) = call(fake.clone(), json!({ "url": "https://example.com" })).await;
        let result = result.unwrap();
        assert_eq!(result.output, "Image fetched successfully");
        let attachments = result.attachments.expect("attachments");
        assert_eq!(attachments[0].mime, "image/png");
        assert!(attachments[0].url.starts_with("data:image/png;base64,"));
    }

    #[test]
    fn markdown_converts_lists_and_code() {
        let out = convert_html_to_markdown(
            "<ul><li>one</li><li>two</li></ul><pre><code>x=1\ny=2</code></pre>",
        );
        assert!(out.contains("- one"), "{out}");
        assert!(out.contains("- two"), "{out}");
        assert!(out.contains("```"), "{out}");
        assert!(out.contains("x=1"), "{out}");
    }

    #[test]
    fn markdown_headings_and_hr() {
        let out = convert_html_to_markdown("<h1>A</h1><h3>B</h3><hr><p>C</p>");
        assert!(out.contains("# A"), "{out}");
        assert!(out.contains("### B"), "{out}");
        assert!(out.contains("---"), "{out}");
    }

    #[test]
    fn markdown_links() {
        let out = convert_html_to_markdown(r#"<p>see <a href="https://x.example">docs</a></p>"#);
        assert!(out.contains("[docs](https://x.example)"), "{out}");
    }

    #[test]
    fn text_extraction_decodes_entities() {
        let out = extract_text_from_html("<p>a &amp; b</p>");
        assert!(out.contains("a & b"), "{out}");
    }
}
