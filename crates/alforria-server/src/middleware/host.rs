//! `Host` allowlist — no TS counterpart (deliberate deviation, see
//! `docs/plans/review-findings.md`, "Server boundary").
//!
//! Without a password the server trusts any request that reaches it. A page
//! on `evil.example` whose DNS record is rebound to `127.0.0.1` reaches the
//! loopback listener as a *same-origin* request (`Origin` and `Host` both
//! name `evil.example`), so neither CORS nor the cross-site write guard
//! stops it. Browsers always send the name they resolved in `Host`, so
//! refusing names the server does not answer to closes that door.
//!
//! Allowed: the loopback names, the bound `--hostname`, extra names the
//! listener announces (mDNS), and the host of every `--cors` origin. When the
//! listener binds every interface (`0.0.0.0` / `::`), any IP literal passes
//! too: an address can't be rebound, and that is how LAN clients reach it.
//! Requests without a `Host` (in-process callers, HTTP/1.0) pass. With a
//! password set the check is off: auth already stops a rebound page, which
//! has no credentials for the server.

use std::net::IpAddr;
use std::task::{Context, Poll};

use axum::body::Body;
use axum::http::{Request, Response};
use axum::response::IntoResponse;
use futures::future::BoxFuture;
use tower::Service;

use crate::state::AuthConfig;

/// The names a password-less server answers to.
#[derive(Debug, Clone, Default)]
pub struct HostAllowlist {
    names: Vec<String>,
    any_ip: bool,
}

impl HostAllowlist {
    /// Loopback names only — the default for contexts built without a
    /// listener (tests, in-process callers).
    pub fn loopback() -> HostAllowlist {
        HostAllowlist::default()
    }

    /// The allowlist for a listener bound to `hostname`, announcing `extra`
    /// names, with `cors` origins allowed.
    pub fn new(hostname: &str, extra: &[String], cors: &[String]) -> HostAllowlist {
        let bound = normalize(hostname);
        let any_ip = matches!(bound.as_str(), "0.0.0.0" | "[::]");
        let mut names = vec![bound];
        names.extend(extra.iter().map(|name| normalize(name)));
        names.extend(cors.iter().filter_map(|origin| origin_host(origin)));
        names.retain(|name| !name.is_empty());
        HostAllowlist { names, any_ip }
    }

    /// Whether a request carrying `Host: host` may proceed.
    pub fn allows(&self, host: &str) -> bool {
        let name = normalize(strip_port(host));
        if matches!(name.as_str(), "localhost" | "127.0.0.1" | "[::1]") {
            return true;
        }
        if self.any_ip && is_ip_literal(&name) {
            return true;
        }
        self.names.contains(&name)
    }
}

/// Lowercase, no trailing dot, IPv6 literals in brackets.
fn normalize(name: &str) -> String {
    let name = name.trim().trim_end_matches('.').to_ascii_lowercase();
    if !name.starts_with('[') && name.parse::<std::net::Ipv6Addr>().is_ok() {
        return format!("[{name}]");
    }
    name
}

/// `host[:port]` → `host`; bracketed IPv6 keeps its brackets.
fn strip_port(host: &str) -> &str {
    if host.starts_with('[') {
        return host.find(']').map_or(host, |end| &host[..=end]);
    }
    host.rsplit_once(':').map_or(host, |(name, _)| name)
}

fn is_ip_literal(name: &str) -> bool {
    let bare = name
        .strip_prefix('[')
        .and_then(|rest| rest.strip_suffix(']'))
        .unwrap_or(name);
    bare.parse::<IpAddr>().is_ok()
}

/// The host of a `scheme://host[:port]` origin.
fn origin_host(origin: &str) -> Option<String> {
    let (_, rest) = origin.split_once("://")?;
    let authority = rest.split(['/', '?', '#']).next()?;
    let authority = authority.rsplit_once('@').map_or(authority, |(_, h)| h);
    Some(normalize(strip_port(authority)))
}

#[derive(Clone)]
pub struct HostLayer {
    auth: AuthConfig,
    hosts: HostAllowlist,
}

impl HostLayer {
    pub fn new(auth: AuthConfig, hosts: HostAllowlist) -> HostLayer {
        HostLayer { auth, hosts }
    }
}

impl<S> tower::Layer<S> for HostLayer {
    type Service = HostService<S>;

    fn layer(&self, inner: S) -> Self::Service {
        HostService {
            inner,
            enabled: !self.auth.required(),
            hosts: self.hosts.clone(),
        }
    }
}

#[derive(Clone)]
pub struct HostService<S> {
    inner: S,
    enabled: bool,
    hosts: HostAllowlist,
}

impl<S> Service<Request<Body>> for HostService<S>
where
    S: Service<Request<Body>, Response = Response<Body>> + Clone + Send + 'static,
    S::Future: Send,
{
    type Response = S::Response;
    type Error = S::Error;
    type Future = BoxFuture<'static, Result<Self::Response, Self::Error>>;

    fn poll_ready(&mut self, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        self.inner.poll_ready(cx)
    }

    fn call(&mut self, req: Request<Body>) -> Self::Future {
        let host = req
            .headers()
            .get(axum::http::header::HOST)
            .and_then(|value| value.to_str().ok())
            .map(str::to_string)
            .or_else(|| req.uri().authority().map(|a| a.as_str().to_string()));
        let refused = self.enabled && host.is_some_and(|host| !self.hosts.allows(&host));
        if refused {
            let response = crate::error::ServerError::from(crate::error::ApiError::Forbidden {
                message: "Host not allowed".to_string(),
            })
            .into_response();
            return Box::pin(async move { Ok(response) });
        }
        let mut inner = self.inner.clone();
        Box::pin(async move { inner.call(req).await })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn loopback_names_always_pass() {
        let hosts = HostAllowlist::loopback();
        for host in [
            "localhost",
            "localhost:4096",
            "LOCALHOST.:4096",
            "127.0.0.1:4096",
            "[::1]:4096",
            "[::1]",
        ] {
            assert!(hosts.allows(host), "{host}");
        }
        for host in [
            "evil.example",
            "evil.example:4096",
            "localhost.evil.example:4096",
            "10.0.0.5:4096",
            "[fe80::1]:4096",
        ] {
            assert!(!hosts.allows(host), "{host}");
        }
    }

    #[test]
    fn bound_hostname_extra_names_and_cors_hosts_pass() {
        let hosts = HostAllowlist::new(
            "box.lan",
            &["opencode.local".to_string()],
            &["https://partner.example:8443".to_string()],
        );
        assert!(hosts.allows("box.lan:4096"));
        assert!(hosts.allows("opencode.local:4096"));
        assert!(hosts.allows("partner.example"));
        assert!(!hosts.allows("10.0.0.5:4096"));
        assert!(!hosts.allows("evil.example"));

        let v6 = HostAllowlist::new("fe80::1", &[], &[]);
        assert!(v6.allows("[fe80::1]:4096"));
    }

    #[test]
    fn unspecified_bind_accepts_ip_literals_only() {
        for bound in ["0.0.0.0", "::"] {
            let hosts = HostAllowlist::new(bound, &[], &[]);
            assert!(hosts.allows("192.168.1.20:4096"));
            assert!(hosts.allows("[fe80::1]:4096"));
            assert!(!hosts.allows("evil.example:4096"));
        }
    }
}
