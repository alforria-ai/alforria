//! Auth value combinators.
//!
//! Port of `route/auth.ts` (M2.6). An [`Auth`] value authenticates one
//! request: it receives the prepared request parts (URL, body, and the
//! headers computed so far) and returns the final header set. Combinators
//! mirror the TS value API — [`Auth::none`], [`Auth::headers`],
//! [`Auth::bearer`], [`Auth::header`] — plus composition via
//! [`Auth::and_then`] / [`Auth::or_else`].
//!
//! Differences from the TS reference: a [`Credential`] is a plain
//! `Box<dyn Fn() -> Result<String, LlmError>>` loader — TS's Effect
//! `Config` machinery does not port, so `Auth.config` becomes
//! [`from_env`] and any config failure surfaces as
//! `Authentication { kind: "missing" }` (spec M2.6). `Redacted` does not
//! port; secrets are plain strings.

#![allow(clippy::result_large_err)]
// `Credential` is `Box<dyn Fn...>` per the spec sketch (no `Send`/`Sync`
// bound), so the boxed `apply` closures are neither; `Arc` is still the
// correct sharing choice for cloning auth values.
#![allow(clippy::arc_with_non_send_sync)]

use std::sync::Arc;

use crate::schema::errors::{AuthKind, LlmError, LlmErrorReason};
use crate::schema::messages::LlmRequest;

/// Ordered header pairs (`(name, value)`). Name matching is case-insensitive,
/// mirroring HTTP header semantics.
pub type Headers = Vec<(String, String)>;

/// A credential loader: resolves the secret at request time.
pub type Credential = Box<dyn Fn() -> Result<String, LlmError>>;

const VALUE_SOURCE: &str = "value";

/// `Auth.value` — a literal secret. Missing/empty credentials fail with
/// `Authentication { kind: "missing" }`.
pub fn value(secret: impl Into<String>) -> Credential {
    let secret = secret.into();
    Box::new(move || load_secret(&secret, VALUE_SOURCE))
}

/// `Auth.optional` — an optional secret; `None` is a missing credential.
pub fn optional(secret: Option<String>, source: &str) -> Credential {
    let source = source.to_string();
    Box::new(move || load_secret(secret.as_deref().unwrap_or(""), &source))
}

/// `Auth.config` — a secret resolved from an environment variable. A missing
/// or empty variable is a missing credential (TS config errors do not port).
pub fn from_env(name: &str) -> Credential {
    let name = name.to_string();
    Box::new(move || load_secret(&std::env::var(&name).unwrap_or_default(), &name))
}

/// `Auth.custom` for credentials — try `first`, fall back to `second` on any
/// load failure (TS `Credential.orElse`).
pub fn or_else(first: Credential, second: Credential) -> Credential {
    Box::new(move || first().or_else(|_| second()))
}

fn missing_credential(source: &str) -> LlmError {
    LlmError {
        module: "Auth".to_string(),
        method: "apply".to_string(),
        reason: LlmErrorReason::Authentication {
            message: format!("Missing auth credential: {source}"),
            kind: AuthKind::Missing,
            provider_metadata: None,
            http: None,
        },
    }
}

fn load_secret(secret: &str, source: &str) -> Result<String, LlmError> {
    if secret.is_empty() {
        Err(missing_credential(source))
    } else {
        Ok(secret.to_string())
    }
}

/// The request parts an [`Auth`] value authenticates (TS `AuthInput`).
pub struct AuthInput<'a> {
    pub request: &'a LlmRequest,
    pub method: &'a str,
    pub url: &'a str,
    pub body: &'a str,
    pub headers: &'a Headers,
}

/// Set each name/value pair, replacing any existing same-named headers
/// (TS `Headers.setAll`; names compare case-insensitively).
fn set_all(headers: &mut Headers, additions: &[(String, String)]) {
    for (name, _) in additions {
        headers.retain(|(existing, _)| !existing.eq_ignore_ascii_case(name));
    }
    headers.extend(additions.iter().cloned());
}

type ApplyFn = Arc<dyn Fn(&AuthInput<'_>) -> Result<Headers, LlmError>>;

fn auth(apply: ApplyFn) -> Auth {
    Auth { apply }
}

/// A value that authenticates one request by producing the final headers.
#[derive(Clone)]
pub struct Auth {
    apply: ApplyFn,
}

impl Auth {
    /// No-op auth: passes the input headers through unchanged.
    pub fn none() -> Auth {
        auth(Arc::new(|input| Ok(input.headers.clone())))
    }

    /// Static headers: sets the given pairs, replacing existing same-named ones.
    pub fn headers(input: impl Into<Headers>) -> Auth {
        let input = input.into();
        auth(Arc::new(move |headers| {
            let mut headers = headers.headers.clone();
            set_all(&mut headers, &input);
            Ok(headers)
        }))
    }

    /// `authorization: Bearer <secret>`.
    pub fn bearer(source: Credential) -> Auth {
        auth(Arc::new(move |input| {
            let secret = source()?;
            let mut headers = input.headers.clone();
            set_all(
                &mut headers,
                &[("authorization".to_string(), format!("Bearer {secret}"))],
            );
            Ok(headers)
        }))
    }

    /// A single header carrying the credential verbatim.
    pub fn header(name: impl Into<String>, source: Credential) -> Auth {
        let name = name.into();
        auth(Arc::new(move |input| {
            let secret = source()?;
            let mut headers = input.headers.clone();
            set_all(&mut headers, &[(name.clone(), secret)]);
            Ok(headers)
        }))
    }

    /// A single header carrying `Bearer <secret>` (TS `bearerHeader`).
    pub fn bearer_header(name: impl Into<String>, source: Credential) -> Auth {
        let name = name.into();
        auth(Arc::new(move |input| {
            let secret = source()?;
            let mut headers = input.headers.clone();
            set_all(&mut headers, &[(name.clone(), format!("Bearer {secret}"))]);
            Ok(headers)
        }))
    }

    /// Remove a header by name (TS `Auth.remove`).
    pub fn remove(name: impl Into<String>) -> Auth {
        let name = name.into();
        auth(Arc::new(move |input| {
            Ok(input
                .headers
                .iter()
                .filter(|(existing, _)| !existing.eq_ignore_ascii_case(&name))
                .cloned()
                .collect())
        }))
    }

    /// Custom auth over the full [`AuthInput`] (e.g. SigV4 signing).
    pub fn custom(apply: ApplyFn) -> Auth {
        Auth { apply }
    }

    /// Run `self`, then `that`, threading the resulting headers (TS `andThen`).
    pub fn and_then(&self, that: Auth) -> Auth {
        let this = self.clone();
        auth(Arc::new(move |input| {
            let headers = (this.apply)(input)?;
            let input = AuthInput {
                request: input.request,
                method: input.method,
                url: input.url,
                body: input.body,
                headers: &headers,
            };
            (that.apply)(&input)
        }))
    }

    /// Run `self`, falling back to `that` on any failure (TS `orElse`).
    pub fn or_else(&self, that: Auth) -> Auth {
        let this = self.clone();
        auth(Arc::new(move |input| {
            (this.apply)(input).or_else(|_| (that.apply)(input))
        }))
    }

    /// Authenticate one request: returns the final header set (TS `toEffect`).
    pub fn apply(&self, input: &AuthInput<'_>) -> Result<Headers, LlmError> {
        (self.apply)(input)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_request() -> LlmRequest {
        LlmRequest {
            id: None,
            model: crate::schema::messages::ModelRef {
                id: "test-model".to_string(),
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

    fn input<'a>(request: &'a LlmRequest, headers: &'a Headers) -> AuthInput<'a> {
        AuthInput {
            request,
            method: "POST",
            url: "https://api.example.com/v1/messages",
            body: "{}",
            headers,
        }
    }

    #[test]
    fn none_passes_headers_through() {
        let auth = Auth::none();
        let headers = vec![(String::from("accept"), String::from("application/json"))];
        let request = test_request();
        let result = auth.apply(&input(&request, &headers)).unwrap();
        assert_eq!(result, headers);
    }

    #[test]
    fn headers_replaces_same_named_entries() {
        let auth = Auth::headers(vec![(String::from("x-api-key"), String::from("secret"))]);
        let headers = vec![(String::from("X-API-Key"), String::from("stale"))];
        let request = test_request();
        let result = auth.apply(&input(&request, &headers)).unwrap();
        assert_eq!(
            result,
            vec![(String::from("x-api-key"), String::from("secret"))]
        );
    }

    #[test]
    fn bearer_sets_authorization_header() {
        let auth = Auth::bearer(value("sk-test"));
        let request = test_request();
        let result = auth.apply(&input(&request, &Vec::new())).unwrap();
        assert_eq!(
            result,
            vec![(
                String::from("authorization"),
                String::from("Bearer sk-test")
            )]
        );
    }

    #[test]
    fn header_sets_named_header() {
        let auth = Auth::header("x-api-key", value("sk-test"));
        let request = test_request();
        let result = auth.apply(&input(&request, &Vec::new())).unwrap();
        assert_eq!(
            result,
            vec![(String::from("x-api-key"), String::from("sk-test"))]
        );
    }

    #[test]
    fn empty_credential_is_authentication_missing() {
        let request = test_request();
        let error = Auth::bearer(value(""))
            .apply(&input(&request, &Vec::new()))
            .unwrap_err();
        assert_eq!(error.module, "Auth");
        assert_eq!(error.method, "apply");
        assert_eq!(
            error.reason,
            LlmErrorReason::Authentication {
                message: "Missing auth credential: value".to_string(),
                kind: AuthKind::Missing,
                provider_metadata: None,
                http: None,
            },
        );
    }

    #[test]
    fn credential_or_else_falls_back_on_missing() {
        let auth = Auth::bearer(or_else(value(""), from_env("DEFINITELY_NOT_SET_ENV_VAR")));
        let request = test_request();
        let result = auth.apply(&input(&request, &Vec::new()));
        assert!(matches!(
            result.unwrap_err().reason,
            LlmErrorReason::Authentication {
                kind: AuthKind::Missing,
                ..
            }
        ));
    }

    #[test]
    fn optional_empty_credential_is_missing() {
        let auth = Auth::bearer(optional(None, "apiKey"));
        let request = test_request();
        let error = auth.apply(&input(&request, &Vec::new())).unwrap_err();
        assert!(matches!(
            error.reason,
            LlmErrorReason::Authentication {
                message,
                kind: AuthKind::Missing,
                ..
            } if message == "Missing auth credential: apiKey"
        ));
    }

    #[test]
    fn and_then_threads_headers() {
        let auth = Auth::none()
            .and_then(Auth::headers(vec![(
                String::from("anthropic-version"),
                String::from("2023-06-01"),
            )]))
            .and_then(Auth::bearer(value("sk-test")));
        let request = test_request();
        let result = auth.apply(&input(&request, &Vec::new())).unwrap();
        assert_eq!(
            result,
            vec![
                (
                    String::from("anthropic-version"),
                    String::from("2023-06-01")
                ),
                (
                    String::from("authorization"),
                    String::from("Bearer sk-test")
                ),
            ]
        );
    }

    #[test]
    fn or_else_falls_back_to_second_auth() {
        let auth = Auth::bearer(value("")).or_else(Auth::bearer(value("sk-second")));
        let request = test_request();
        let result = auth.apply(&input(&request, &Vec::new())).unwrap();
        assert_eq!(
            result,
            vec![(
                String::from("authorization"),
                String::from("Bearer sk-second")
            )]
        );
    }

    #[test]
    fn remove_drops_named_header() {
        let auth = Auth::remove("Authorization");
        let headers = vec![
            (String::from("authorization"), String::from("Bearer stale")),
            (String::from("x-api-key"), String::from("sk-test")),
        ];
        let request = test_request();
        let result = auth.apply(&input(&request, &headers)).unwrap();
        assert_eq!(
            result,
            vec![(String::from("x-api-key"), String::from("sk-test"))]
        );
    }

    #[test]
    fn bearer_header_sets_bearer_prefixed_header() {
        let auth = Auth::bearer_header("cf-aig-authorization", value("sk-gw"));
        let request = test_request();
        let result = auth.apply(&input(&request, &Vec::new())).unwrap();
        assert_eq!(
            result,
            vec![(
                String::from("cf-aig-authorization"),
                String::from("Bearer sk-gw")
            )]
        );
    }
}
