#![allow(clippy::arc_with_non_send_sync)]
//! URL construction for one route.
//!
//! Port of `route/endpoint.ts` (M2.6). An [`Endpoint`] declares where a
//! request is sent: an optional `base_url` (routes with a canonical host
//! put it here; provider facades override it by configuring the route
//! before selecting a model), a `path` — static string or a function of the
//! lowered request `{request, body}` for routes whose URL embeds the model
//! id (Gemini, Bedrock) — and an optional `query` overlay.

#![allow(clippy::result_large_err)]

use std::collections::BTreeMap;

use crate::protocols::shared::trim_base_url;
use crate::schema::errors::{LlmError, LlmErrorReason};
use crate::schema::messages::LlmRequest;

/// The parts of a request available to a path function: the resolved common
/// request and its lowered provider-native JSON body.
#[derive(Debug, Clone, PartialEq)]
pub struct EndpointInput<'a> {
    pub request: &'a LlmRequest,
    pub body: &'a serde_json::Value,
}

/// A route path: static, or computed per request (TS `EndpointPart`).
#[derive(Debug, Clone)]
pub enum EndpointPart {
    Path(String),
    Function(fn(&EndpointInput<'_>) -> String),
}

impl From<&str> for EndpointPart {
    fn from(value: &str) -> Self {
        EndpointPart::Path(value.to_string())
    }
}

impl From<String> for EndpointPart {
    fn from(value: String) -> Self {
        EndpointPart::Path(value)
    }
}

/// Declarative URL construction for one route (TS `Endpoint`).
#[derive(Debug, Clone)]
pub struct Endpoint {
    /// Host root; trailing slashes are trimmed at render time.
    pub base_url: Option<String>,
    pub path: EndpointPart,
    pub query: Option<BTreeMap<String, String>>,
}

/// Patch fields for [`Endpoint::merge`]; `None` keeps the base value.
/// `query` merges into the base entries (patch keys win).
#[derive(Debug, Clone, Default)]
pub struct EndpointPatch {
    pub base_url: Option<String>,
    pub path: Option<EndpointPart>,
    pub query: Option<BTreeMap<String, String>>,
}

impl Endpoint {
    /// Construct an endpoint from a static path (TS `Endpoint.path`).
    pub fn path(path: impl Into<EndpointPart>) -> Self {
        Self {
            base_url: None,
            path: path.into(),
            query: None,
        }
    }

    /// Overlay a patch onto a base endpoint (TS `Endpoint.merge`).
    pub fn merge(base: &Endpoint, patch: &EndpointPatch) -> Endpoint {
        let query = match (&base.query, &patch.query) {
            (Some(base_query), Some(patch_query)) => {
                let mut merged = base_query.clone();
                merged.extend(patch_query.clone());
                Some(merged)
            }
            (None, Some(patch_query)) => Some(patch_query.clone()),
            (base_query, None) => base_query.clone(),
        };
        Endpoint {
            base_url: patch.base_url.clone().or_else(|| base.base_url.clone()),
            path: patch.path.clone().unwrap_or_else(|| base.path.clone()),
            query,
        }
    }
}

fn render_part(part: &EndpointPart, input: &EndpointInput<'_>) -> String {
    match part {
        EndpointPart::Path(value) => value.clone(),
        EndpointPart::Function(function) => function(input),
    }
}

/// `searchParams.set` semantics: replace the first pair with a matching key
/// and drop later duplicates, or append when the key is absent.
fn set_query_pair(url: &mut reqwest::Url, key: &str, value: &str) {
    let mut pairs: Vec<(String, String)> = url
        .query_pairs()
        .map(|(k, v)| (k.into_owned(), v.into_owned()))
        .collect();
    let mut seen = false;
    pairs.retain_mut(|(existing, existing_value)| {
        if existing == key {
            if seen {
                return false;
            }
            seen = true;
            *existing_value = value.to_string();
        }
        true
    });
    if !seen {
        pairs.push((key.to_string(), value.to_string()));
    }
    url.set_query(None);
    for (k, v) in &pairs {
        url.query_pairs_mut().append_pair(k, v);
    }
}

/// Render an endpoint to a request URL, trimming trailing slashes off the
/// base URL and overlaying `query` pairs onto any the path embedded.
///
/// The TS reference constructs a relative URL when `base_url` is missing and
/// lets `new URL` throw; panics are not part of the Rust contract, so this
/// surfaces that misconfiguration as an `InvalidRequest` error instead.
pub fn render(endpoint: &Endpoint, input: &EndpointInput<'_>) -> Result<reqwest::Url, LlmError> {
    let base = trim_base_url(endpoint.base_url.as_deref().unwrap_or(""));
    let rendered = render_part(&endpoint.path, input);
    let mut url = reqwest::Url::parse(&format!("{base}{rendered}")).map_err(|error| LlmError {
        module: "Endpoint".to_string(),
        method: "render".to_string(),
        reason: LlmErrorReason::InvalidRequest {
            message: format!("Invalid request URL {base:?}{rendered:?}: {error}"),
            parameter: None,
            classification: None,
            provider_metadata: None,
            http: None,
        },
    })?;
    if let Some(query) = &endpoint.query {
        for (key, value) in query {
            set_query_pair(&mut url, key, value);
        }
    }
    Ok(url)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn request_with_model(id: &str) -> LlmRequest {
        LlmRequest {
            id: None,
            model: crate::schema::messages::ModelRef {
                id: id.to_string(),
                provider: "test-provider".to_string(),
                route: std::sync::Arc::new(crate::route::client::RouteHandle::empty()),
                defaults: None,
                compatibility: None,
            },
            system: Vec::new(),
            messages: Vec::new(),
            tools: Vec::new(),
            tool_choice: None,
            generation: None,
            provider_options: None,
            http: None,
            response_format: None,
            cache: None,
            metadata: None,
        }
    }

    #[test]
    fn renders_static_path() {
        let endpoint = Endpoint {
            base_url: Some("https://api.anthropic.com/v1".to_string()),
            path: EndpointPart::Path("/messages".to_string()),
            query: None,
        };
        let request = request_with_model("claude-sonnet-4");
        let body = serde_json::json!({});
        let url = render(
            &endpoint,
            &EndpointInput {
                request: &request,
                body: &body,
            },
        )
        .unwrap();
        assert_eq!(url.as_str(), "https://api.anthropic.com/v1/messages");
    }

    #[test]
    fn trims_trailing_slashes_from_base_url() {
        let endpoint = Endpoint {
            base_url: Some("https://api.example.com/v1///".to_string()),
            path: EndpointPart::Path("/chat/completions".to_string()),
            query: None,
        };
        let request = request_with_model("gpt");
        let url = render(
            &endpoint,
            &EndpointInput {
                request: &request,
                body: &serde_json::json!({}),
            },
        )
        .unwrap();
        assert_eq!(url.as_str(), "https://api.example.com/v1/chat/completions");
    }

    #[test]
    fn renders_path_function_for_gemini() {
        let endpoint = Endpoint {
            base_url: Some("https://generativelanguage.googleapis.com/v1beta".to_string()),
            path: EndpointPart::Function(|input| {
                format!(
                    "/models/{}:streamGenerateContent?alt=sse",
                    input.request.model.id
                )
            }),
            query: None,
        };
        let request = request_with_model("gemini-2.5-pro");
        let url = render(
            &endpoint,
            &EndpointInput {
                request: &request,
                body: &serde_json::json!({}),
            },
        )
        .unwrap();
        assert_eq!(
            url.as_str(),
            "https://generativelanguage.googleapis.com/v1beta/models/gemini-2.5-pro:streamGenerateContent?alt=sse"
        );
    }

    #[test]
    fn query_overlays_without_replacing_path_params() {
        let endpoint = Endpoint {
            base_url: Some("https://example.test".to_string()),
            path: EndpointPart::Path("/respond".to_string()),
            query: Some(BTreeMap::from([(
                "api-version".to_string(),
                "v1".to_string(),
            )])),
        };
        let request = request_with_model("model");
        let url = render(
            &endpoint,
            &EndpointInput {
                request: &request,
                body: &serde_json::json!({}),
            },
        )
        .unwrap();
        assert_eq!(url.as_str(), "https://example.test/respond?api-version=v1");
    }

    #[test]
    fn query_set_replaces_existing_key() {
        let endpoint = Endpoint {
            base_url: Some("https://example.test".to_string()),
            path: EndpointPart::Path("/generate?alt=sse".to_string()),
            query: Some(BTreeMap::from([("alt".to_string(), "stream".to_string())])),
        };
        let request = request_with_model("model");
        let url = render(
            &endpoint,
            &EndpointInput {
                request: &request,
                body: &serde_json::json!({}),
            },
        )
        .unwrap();
        assert_eq!(url.as_str(), "https://example.test/generate?alt=stream");
    }

    #[test]
    fn missing_base_url_is_invalid_request() {
        let endpoint = Endpoint {
            base_url: None,
            path: EndpointPart::Path("/messages".to_string()),
            query: None,
        };
        let request = request_with_model("model");
        let error = render(
            &endpoint,
            &EndpointInput {
                request: &request,
                body: &serde_json::json!({}),
            },
        )
        .unwrap_err();
        assert_eq!(error.module, "Endpoint");
        assert!(matches!(
            error.reason,
            LlmErrorReason::InvalidRequest { .. }
        ));
    }

    #[test]
    fn merge_prefers_patch_values() {
        let base = Endpoint {
            base_url: Some("https://api.example.com/v1".to_string()),
            path: EndpointPart::Path("/chat/completions".to_string()),
            query: Some(BTreeMap::from([(
                "api-version".to_string(),
                "v1".to_string(),
            )])),
        };
        let patch = EndpointPatch {
            base_url: Some("https://other.example.com".to_string()),
            path: Some(EndpointPart::Path("/responses".to_string())),
            query: Some(BTreeMap::from([(
                "deployment".to_string(),
                "x".to_string(),
            )])),
        };
        let merged = Endpoint::merge(&base, &patch);
        assert_eq!(
            merged.base_url.as_deref(),
            Some("https://other.example.com")
        );
        assert!(matches!(merged.path, EndpointPart::Path(ref path) if path == "/responses"));
        assert_eq!(
            merged
                .query
                .as_ref()
                .unwrap()
                .get("api-version")
                .map(String::as_str),
            Some("v1")
        );
        assert_eq!(
            merged
                .query
                .as_ref()
                .unwrap()
                .get("deployment")
                .map(String::as_str),
            Some("x")
        );
    }

    #[test]
    fn merge_without_patch_keeps_base() {
        let base = Endpoint {
            base_url: Some("https://api.example.com/v1".to_string()),
            path: EndpointPart::Path("/messages".to_string()),
            query: None,
        };
        let merged = Endpoint::merge(&base, &EndpointPatch::default());
        assert_eq!(merged.base_url, base.base_url);
        assert!(matches!(merged.path, EndpointPart::Path(ref path) if path == "/messages"));
        assert!(merged.query.is_none());
    }
}
