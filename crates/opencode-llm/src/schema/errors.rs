//! Error values from `schema/errors.ts`.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use crate::schema::ids::{JsonMap, ModelId, ProviderId, ProviderMetadata, RouteId};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum AuthKind {
    Missing,
    Invalid,
    Expired,
    InsufficientPermissions,
    Unknown,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ProviderFailureClassification {
    #[serde(rename = "context-overflow")]
    ContextOverflow,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct HttpRequestDetails {
    pub method: String,
    pub url: String,
    pub headers: BTreeMap<String, String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct HttpResponseDetails {
    pub status: f64,
    pub headers: BTreeMap<String, String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct HttpRateLimitDetails {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub retry_after_ms: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub limit: Option<BTreeMap<String, String>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub remaining: Option<BTreeMap<String, String>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reset: Option<BTreeMap<String, String>>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct HttpContext {
    pub request: HttpRequestDetails,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub response: Option<HttpResponseDetails>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub body: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub body_truncated: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub request_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rate_limit: Option<HttpRateLimitDetails>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, thiserror::Error)]
#[serde(tag = "_tag")]
#[serde(rename_all_fields = "camelCase")]
pub enum LlmErrorReason {
    #[error("{message}")]
    InvalidRequest {
        message: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        parameter: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        classification: Option<ProviderFailureClassification>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        provider_metadata: Option<ProviderMetadata>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        http: Option<HttpContext>,
    },
    #[error("No LLM route")]
    NoRoute {
        route: RouteId,
        provider: ProviderId,
        model: ModelId,
    },
    #[error("{message}")]
    Authentication {
        message: String,
        kind: AuthKind,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        provider_metadata: Option<ProviderMetadata>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        http: Option<HttpContext>,
    },
    #[error("{message}")]
    RateLimit {
        message: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        retry_after_ms: Option<f64>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        rate_limit: Option<HttpRateLimitDetails>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        provider_metadata: Option<ProviderMetadata>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        http: Option<HttpContext>,
    },
    #[error("{message}")]
    QuotaExceeded {
        message: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        provider_metadata: Option<ProviderMetadata>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        http: Option<HttpContext>,
    },
    #[error("{message}")]
    ContentPolicy {
        message: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        provider_metadata: Option<ProviderMetadata>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        http: Option<HttpContext>,
    },
    #[error("{message}")]
    ProviderInternal {
        message: String,
        status: f64,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        retry_after_ms: Option<f64>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        provider_metadata: Option<ProviderMetadata>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        http: Option<HttpContext>,
    },
    #[error("{message}")]
    Transport {
        message: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        kind: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        url: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        http: Option<HttpContext>,
    },
    #[error("{message}")]
    InvalidProviderOutput {
        message: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        route: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        raw: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        provider_metadata: Option<ProviderMetadata>,
    },
    #[error("{message}")]
    UnknownProvider {
        message: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        status: Option<f64>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        provider_metadata: Option<ProviderMetadata>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        http: Option<HttpContext>,
    },
}

impl LlmErrorReason {
    pub fn retryable(&self) -> bool {
        matches!(
            self,
            LlmErrorReason::RateLimit { .. } | LlmErrorReason::ProviderInternal { .. }
        )
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, thiserror::Error)]
#[serde(rename_all = "camelCase")]
#[error("{module}.{method}")]
pub struct LlmError {
    pub module: String,
    pub method: String,
    pub reason: LlmErrorReason,
}

impl LlmError {
    pub fn retryable(&self) -> bool {
        self.reason.retryable()
    }

    pub fn retry_after_ms(&self) -> Option<f64> {
        match &self.reason {
            LlmErrorReason::RateLimit { retry_after_ms, .. } => *retry_after_ms,
            LlmErrorReason::ProviderInternal { retry_after_ms, .. } => *retry_after_ms,
            _ => None,
        }
    }

    pub fn invalid(message: impl Into<String>) -> Self {
        Self {
            module: "ProviderShared".to_string(),
            method: "request".to_string(),
            reason: LlmErrorReason::InvalidRequest {
                message: message.into(),
                parameter: None,
                classification: None,
                provider_metadata: None,
                http: None,
            },
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, thiserror::Error)]
#[serde(rename_all = "camelCase")]
#[error("{message}")]
pub struct ToolFailure {
    pub message: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<serde_json::Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub metadata: Option<JsonMap>,
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    #[test]
    fn rate_limit_reason_round_trips_with_retry_after_ms() {
        let reason = LlmErrorReason::RateLimit {
            message: "Too many requests".to_string(),
            retry_after_ms: Some(1200.0),
            rate_limit: None,
            provider_metadata: None,
            http: None,
        };
        let value = serde_json::to_value(&reason).unwrap();
        assert_eq!(
            value,
            json!({
                "_tag": "RateLimit",
                "message": "Too many requests",
                "retryAfterMs": 1200.0,
            }),
        );
        let roundtripped: LlmErrorReason = serde_json::from_value(value).unwrap();
        assert_eq!(roundtripped, reason);
    }

    #[test]
    fn retryable_is_true_exactly_for_rate_limit_and_provider_internal() {
        let cases = [
            (
                LlmErrorReason::InvalidRequest {
                    message: "m".to_string(),
                    parameter: None,
                    classification: None,
                    provider_metadata: None,
                    http: None,
                },
                false,
            ),
            (
                LlmErrorReason::NoRoute {
                    route: "route".to_string(),
                    provider: "provider".to_string(),
                    model: "model".to_string(),
                },
                false,
            ),
            (
                LlmErrorReason::Authentication {
                    message: "m".to_string(),
                    kind: AuthKind::Missing,
                    provider_metadata: None,
                    http: None,
                },
                false,
            ),
            (
                LlmErrorReason::RateLimit {
                    message: "m".to_string(),
                    retry_after_ms: None,
                    rate_limit: None,
                    provider_metadata: None,
                    http: None,
                },
                true,
            ),
            (
                LlmErrorReason::QuotaExceeded {
                    message: "m".to_string(),
                    provider_metadata: None,
                    http: None,
                },
                false,
            ),
            (
                LlmErrorReason::ContentPolicy {
                    message: "m".to_string(),
                    provider_metadata: None,
                    http: None,
                },
                false,
            ),
            (
                LlmErrorReason::ProviderInternal {
                    message: "m".to_string(),
                    status: 503.0,
                    retry_after_ms: None,
                    provider_metadata: None,
                    http: None,
                },
                true,
            ),
            (
                LlmErrorReason::Transport {
                    message: "m".to_string(),
                    kind: None,
                    url: None,
                    http: None,
                },
                false,
            ),
            (
                LlmErrorReason::InvalidProviderOutput {
                    message: "m".to_string(),
                    route: None,
                    raw: None,
                    provider_metadata: None,
                },
                false,
            ),
            (
                LlmErrorReason::UnknownProvider {
                    message: "m".to_string(),
                    status: None,
                    provider_metadata: None,
                    http: None,
                },
                false,
            ),
        ];
        for (reason, retryable) in cases {
            assert_eq!(reason.retryable(), retryable);
        }
    }

    #[test]
    fn llm_error_exposes_retry_after_ms_and_invalid_helper() {
        let error = LlmError {
            module: "Route".to_string(),
            method: "generate".to_string(),
            reason: LlmErrorReason::RateLimit {
                message: "m".to_string(),
                retry_after_ms: Some(500.0),
                rate_limit: None,
                provider_metadata: None,
                http: None,
            },
        };
        assert!(error.retryable());
        assert_eq!(error.retry_after_ms(), Some(500.0));

        let invalid = LlmError::invalid("bad request");
        assert_eq!(invalid.module, "ProviderShared");
        assert_eq!(invalid.method, "request");
        assert!(!invalid.retryable());
        assert_eq!(invalid.retry_after_ms(), None);
        assert_eq!(invalid.to_string(), "ProviderShared.request");
    }
}
