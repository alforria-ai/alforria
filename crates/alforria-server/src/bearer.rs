//! Bearer authentication (alforria extension): static tokens and Ed25519
//! JWTs, each carrying scopes.
//!
//! HTTP Basic (the upstream scheme) authenticates the operator and may do
//! anything. A bearer credential is for everything else that talks to an
//! unattended server — a browser or a phone holding a short-lived token a
//! control plane minted, a sidecar on the same machine — and says what its
//! holder may do:
//!
//! * `chat` — talk to the agent: sessions, messages, events, permission and
//!   question replies, files inside the project;
//! * `connector` — `chat`, plus what local automation needs (the global
//!   event stream, workflows, status reads, firing webhook triggers);
//! * `admin` — everything;
//! * `hook` — one thing only: `POST /trigger/{id}/hook`, the webhook entry
//!   of a trigger. It is the credential to hand to whatever calls a webhook
//!   from outside, which should hold neither a `chat` token (it could read
//!   and prompt sessions) nor an `admin` one.
//!
//! The server holds no signing secret: static tokens are stored as SHA-256
//! digests and JWTs are verified against configured public keys. Only
//! `EdDSA` is accepted; a token naming any other algorithm (`none`, `HS256`
//! keyed with the public key, …) is refused before any signature check.

use std::sync::Arc;
use std::time::Duration;

use alforria_core::config::host::{BearerAuthInfo, BearerTokenInfo};
use alforria_core::Clock;
use base64::Engine;
use sha2::{Digest, Sha256};

/// What a credential lets its holder do. `chat`, `connector` and `admin`
/// are ordered: each includes the ones before it. `hook` stands apart, below
/// all three: it opens the webhook entry and nothing else, and `chat` does
/// not include it ([`Principal::allows`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Scope {
    Hook,
    Chat,
    Connector,
    Admin,
}

impl Scope {
    pub fn as_str(self) -> &'static str {
        match self {
            Scope::Hook => "hook",
            Scope::Chat => "chat",
            Scope::Connector => "connector",
            Scope::Admin => "admin",
        }
    }

    pub fn parse(name: &str) -> Option<Scope> {
        match name {
            "hook" => Some(Scope::Hook),
            "chat" => Some(Scope::Chat),
            "connector" => Some(Scope::Connector),
            "admin" => Some(Scope::Admin),
            _ => None,
        }
    }

    /// The widest scope named in a space-separated list; unknown names are
    /// ignored. A credential has one role: `hook` beside `chat` is `chat`,
    /// which does not open the webhook entry — a webhook caller gets a
    /// token of its own.
    pub fn widest(names: &str) -> Option<Scope> {
        names.split_whitespace().filter_map(Scope::parse).max()
    }
}

/// Who a request was authenticated as.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Principal {
    /// The widest scope held.
    pub scope: Scope,
    /// The JWT `sub`, or a static token's `name`.
    pub subject: Option<String>,
    pub via: Via,
    /// The one trigger this credential delivers to: the JWT `trg` claim or
    /// a static token's `trigger`. A `hook` credential must have one — it
    /// is handed to a third party to call one webhook, and without the
    /// binding it fired every webhook trigger of the directory. On a wider
    /// credential it narrows the webhook route the same way.
    pub trigger: Option<String>,
}

/// How a request authenticated.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Via {
    /// No authentication is configured.
    Open,
    Basic,
    StaticToken,
    Jwt,
    /// A single-purpose ticket in the URL (a terminal or a file).
    Ticket,
    /// A route the auth layer serves without a credential on a server
    /// that asks for one: the web UI's static files and the OpenAPI
    /// document.
    Public,
}

impl Principal {
    /// The operator: Basic auth, a server with no authentication, or one
    /// of the auth layer's own bypasses (a ticket, a public route).
    pub fn operator(via: Via) -> Principal {
        Principal {
            scope: Scope::Admin,
            subject: None,
            via,
            trigger: None,
        }
    }

    /// Whether this credential may deliver to webhook trigger `id`: a
    /// `hook` credential only to the trigger it is bound to, any other
    /// only to its binding when it has one.
    pub fn may_deliver_to(&self, id: &str) -> bool {
        match &self.trigger {
            Some(bound) => bound == id,
            None => self.scope != Scope::Hook,
        }
    }

    pub fn allows(&self, required: Scope) -> bool {
        match required {
            // Delivering an outside event is not part of talking to the
            // agent: a `hook` token does it, and so does the sidecar's
            // `connector` (and `admin`); `chat` does not.
            Scope::Hook => self.scope != Scope::Chat,
            _ => self.scope >= required,
        }
    }

    /// Whether a request is to be held to what a credential below `admin`
    /// may do. The auth layer gives every request it lets through a
    /// principal — the operator's on a server with no authentication
    /// ([`Via::Open`]) — so a request without one did not pass its
    /// judgement: a route mounted outside it, a layer left out. That is
    /// not the operator. It is confined like the narrowest caller, and
    /// the mistake is logged.
    pub fn confines(principal: Option<&Principal>) -> bool {
        match principal {
            Some(principal) => principal.restricted(),
            None => {
                tracing::error!(
                    "a request reached a handler without a principal: the auth layer did not \
                     judge it; treating it as a caller below admin"
                );
                true
            }
        }
    }

    /// Whether the directory allowlist (`server.directories`) applies.
    pub fn restricted(&self) -> bool {
        self.scope < Scope::Admin
    }
}

/// Why a bearer credential was refused. The text is safe to return: it
/// never echoes the token.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BearerError {
    /// Not a known static token and not shaped like a JWT.
    Unknown,
    Malformed(&'static str),
    /// `alg` is not `EdDSA`.
    Algorithm(String),
    /// No configured key verifies the signature.
    Signature,
    Expired,
    NotYetValid,
    Audience,
    Issuer,
    /// No `hook`, `chat`, `connector` or `admin` in `scope`.
    Scope,
}

impl std::fmt::Display for BearerError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            BearerError::Unknown => f.write_str("the token is not recognised"),
            BearerError::Malformed(what) => write!(f, "the token is malformed: {what}"),
            BearerError::Algorithm(alg) => {
                write!(
                    f,
                    "unsupported token algorithm {alg:?}: only EdDSA is accepted"
                )
            }
            BearerError::Signature => f.write_str("the token signature matches no configured key"),
            BearerError::Expired => f.write_str("the token has expired"),
            BearerError::NotYetValid => f.write_str("the token is not valid yet"),
            BearerError::Audience => f.write_str("the token is for a different audience"),
            BearerError::Issuer => f.write_str("the token is from a different issuer"),
            BearerError::Scope => f.write_str(
                "the token carries no scope this server knows (chat, connector, admin, hook)",
            ),
        }
    }
}

/// `leeway` when unset.
pub const DEFAULT_LEEWAY: Duration = Duration::from_secs(30);

/// The DER prefix of an Ed25519 `SubjectPublicKeyInfo` (RFC 8410): the 32
/// key bytes follow.
const SPKI_PREFIX: [u8; 12] = [
    0x30, 0x2a, 0x30, 0x05, 0x06, 0x03, 0x2b, 0x65, 0x70, 0x03, 0x21, 0x00,
];

struct StaticToken {
    digest: [u8; 32],
    scope: Scope,
    name: Option<String>,
    trigger: Option<String>,
}

/// The resolved `server.auth.bearer`.
pub struct BearerAuth {
    keys: Vec<[u8; 32]>,
    audiences: Vec<String>,
    issuer: Option<String>,
    leeway: Duration,
    tokens: Vec<StaticToken>,
    clock: Arc<dyn Clock>,
}

impl std::fmt::Debug for BearerAuth {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("BearerAuth")
            .field("keys", &self.keys.len())
            .field("audiences", &self.audiences)
            .field("issuer", &self.issuer)
            .field("leeway", &self.leeway)
            .field("tokens", &self.tokens.len())
            .finish()
    }
}

fn decode_any_base64(text: &str) -> Option<Vec<u8>> {
    let text = text.trim().trim_end_matches('=');
    base64::engine::general_purpose::STANDARD_NO_PAD
        .decode(text)
        .or_else(|_| base64::engine::general_purpose::URL_SAFE_NO_PAD.decode(text))
        .ok()
}

fn key_from_bytes(bytes: &[u8]) -> Option<[u8; 32]> {
    match bytes.len() {
        32 => bytes.try_into().ok(),
        44 if bytes[..12] == SPKI_PREFIX => bytes[12..].try_into().ok(),
        _ => None,
    }
}

/// Parse an Ed25519 public key: PEM (`BEGIN PUBLIC KEY`), or base64 / hex of
/// the raw 32 bytes or of the SPKI DER.
pub fn parse_public_key(text: &str) -> Result<[u8; 32], String> {
    let text = text.trim();
    let bytes = if text.starts_with("-----BEGIN") {
        if !text.starts_with("-----BEGIN PUBLIC KEY-----") {
            return Err(
                "expected a `PUBLIC KEY` PEM block (a private key must never be configured here)"
                    .to_string(),
            );
        }
        let body: String = text
            .lines()
            .filter(|line| !line.starts_with("-----"))
            .flat_map(|line| line.chars())
            .filter(|c| !c.is_whitespace())
            .collect();
        decode_any_base64(&body)
    } else if text.len() == 64 && text.bytes().all(|b| b.is_ascii_hexdigit()) {
        hex::decode(text).ok()
    } else {
        decode_any_base64(text)
    };
    bytes.as_deref().and_then(key_from_bytes).ok_or_else(|| {
        "not an Ed25519 public key (PEM, or base64 / hex of 32 raw bytes)".to_string()
    })
}

fn parse_digest(text: &str) -> Result<[u8; 32], String> {
    let text = text.trim();
    let text = text.strip_prefix("sha256:").unwrap_or(text);
    hex::decode(text)
        .ok()
        .and_then(|bytes| <[u8; 32]>::try_from(bytes).ok())
        .ok_or_else(|| {
            "a token digest must be 64 hex characters (the SHA-256 of the token)".to_string()
        })
}

/// The SHA-256 of a token, in the hex form `tokens_sha256` takes.
pub fn token_digest(token: &str) -> String {
    hex::encode(Sha256::digest(token.as_bytes()))
}

fn constant_time_eq(a: &[u8; 32], b: &[u8; 32]) -> bool {
    #[cfg(test)]
    COMPARISONS.with(|count| count.set(count.get() + 1));
    a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

#[cfg(test)]
thread_local! {
    /// How many constant-time comparisons this thread has made.
    pub(crate) static COMPARISONS: std::cell::Cell<u64> = const { std::cell::Cell::new(0) };
}

/// Whether two secrets are the same, in time that does not depend on where
/// they differ or on their lengths: their SHA-256 digests are compared in
/// full. For the Basic username and password (`AuthConfig::authorized`).
pub(crate) fn secret_eq(a: &str, b: &str) -> bool {
    let (a, b): ([u8; 32], [u8; 32]) = (
        Sha256::digest(a.as_bytes()).into(),
        Sha256::digest(b.as_bytes()).into(),
    );
    constant_time_eq(&a, &b)
}

fn decode_segment(segment: &str) -> Option<Vec<u8>> {
    base64::engine::general_purpose::URL_SAFE_NO_PAD
        .decode(segment)
        .ok()
}

impl BearerAuth {
    /// Resolve `server.auth.bearer`. `Ok(None)` when it configures nothing.
    pub fn from_config(info: &BearerAuthInfo) -> Result<Option<BearerAuth>, String> {
        BearerAuth::from_config_with(info, Arc::new(alforria_core::catalog::SystemClock))
    }

    /// [`BearerAuth::from_config`] with the clock injected (tests).
    pub fn from_config_with(
        info: &BearerAuthInfo,
        clock: Arc<dyn Clock>,
    ) -> Result<Option<BearerAuth>, String> {
        let mut keys = Vec::new();
        for (index, key) in info.keys.iter().flatten().enumerate() {
            keys.push(
                parse_public_key(key)
                    .map_err(|err| format!("server.auth.bearer.keys[{index}]: {err}"))?,
            );
        }
        let audiences: Vec<String> = info
            .audience
            .as_ref()
            .map(|audience| audience.to_vec())
            .unwrap_or_default()
            .into_iter()
            .filter(|audience| !audience.is_empty())
            .collect();
        if !keys.is_empty() && audiences.is_empty() {
            return Err(
                "server.auth.bearer.audience is required with keys: without it a token minted \
                 for another server would be accepted here"
                    .to_string(),
            );
        }
        let mut tokens = Vec::new();
        for (index, token) in info.tokens_sha256.iter().flatten().enumerate() {
            let at = |err: String| format!("server.auth.bearer.tokens_sha256[{index}]: {err}");
            tokens.push(match token {
                BearerTokenInfo::Digest(digest) => StaticToken {
                    digest: parse_digest(digest).map_err(at)?,
                    scope: Scope::Admin,
                    name: None,
                    trigger: None,
                },
                BearerTokenInfo::Scoped {
                    sha256,
                    scope,
                    name,
                    trigger,
                } => StaticToken {
                    digest: parse_digest(sha256).map_err(at)?,
                    scope: match scope {
                        None => Scope::Admin,
                        Some(names) => Scope::widest(names).ok_or_else(|| {
                            at(format!(
                                "unknown scope {names:?} (expected hook, chat, connector or admin)"
                            ))
                        })?,
                    },
                    name: name.clone(),
                    trigger: trigger.clone().filter(|id| !id.is_empty()),
                },
            });
        }
        if keys.is_empty() && tokens.is_empty() {
            return Ok(None);
        }
        Ok(Some(BearerAuth {
            keys,
            audiences,
            issuer: info.issuer.clone().filter(|issuer| !issuer.is_empty()),
            leeway: info
                .leeway
                .map(|leeway| leeway.get())
                .unwrap_or(DEFAULT_LEEWAY),
            tokens,
            clock,
        }))
    }

    /// Authenticate a bearer credential.
    pub fn verify(&self, token: &str) -> Result<Principal, BearerError> {
        if token.is_empty() {
            return Err(BearerError::Unknown);
        }
        let digest: [u8; 32] = Sha256::digest(token.as_bytes()).into();
        // No early exit: every configured digest is compared.
        let mut matched = None;
        for candidate in &self.tokens {
            if constant_time_eq(&candidate.digest, &digest) {
                matched = Some(candidate);
            }
        }
        if let Some(matched) = matched {
            return Ok(Principal {
                scope: matched.scope,
                subject: matched.name.clone(),
                via: Via::StaticToken,
                trigger: matched.trigger.clone(),
            });
        }
        if self.keys.is_empty() || token.split('.').count() != 3 {
            return Err(BearerError::Unknown);
        }
        self.verify_jwt(token)
    }

    fn verify_jwt(&self, token: &str) -> Result<Principal, BearerError> {
        let mut parts = token.split('.');
        let (Some(header_b64), Some(payload_b64), Some(signature_b64)) =
            (parts.next(), parts.next(), parts.next())
        else {
            return Err(BearerError::Malformed("expected three segments"));
        };
        let header: serde_json::Value = decode_segment(header_b64)
            .and_then(|bytes| serde_json::from_slice(&bytes).ok())
            .ok_or(BearerError::Malformed("unreadable header"))?;
        // The algorithm is decided here, by this server, never by the token:
        // anything but EdDSA is refused before a key is touched.
        let alg = header
            .get("alg")
            .and_then(|alg| alg.as_str())
            .ok_or(BearerError::Malformed("no alg"))?;
        if alg != "EdDSA" {
            return Err(BearerError::Algorithm(alg.to_string()));
        }
        if header.get("crit").is_some() {
            return Err(BearerError::Malformed(
                "critical header extensions are not supported",
            ));
        }
        let signature =
            decode_segment(signature_b64).ok_or(BearerError::Malformed("unreadable signature"))?;
        if signature.len() != 64 {
            return Err(BearerError::Signature);
        }
        let signed = &token[..header_b64.len() + 1 + payload_b64.len()];
        let verified = self.keys.iter().any(|key| {
            ring::signature::UnparsedPublicKey::new(&ring::signature::ED25519, key)
                .verify(signed.as_bytes(), &signature)
                .is_ok()
        });
        if !verified {
            return Err(BearerError::Signature);
        }

        // Only now are the claims trusted enough to read.
        let claims: serde_json::Value = decode_segment(payload_b64)
            .and_then(|bytes| serde_json::from_slice(&bytes).ok())
            .ok_or(BearerError::Malformed("unreadable claims"))?;
        let now = self.clock.now_ms() as f64 / 1000.0;
        let leeway = self.leeway.as_secs_f64();
        let exp = claims
            .get("exp")
            .and_then(|exp| exp.as_f64())
            .ok_or(BearerError::Malformed("no exp"))?;
        if now >= exp + leeway {
            return Err(BearerError::Expired);
        }
        if let Some(nbf) = claims.get("nbf") {
            let nbf = nbf
                .as_f64()
                .ok_or(BearerError::Malformed("nbf is not a number"))?;
            if now + leeway < nbf {
                return Err(BearerError::NotYetValid);
            }
        }
        let audiences: Vec<&str> = match claims.get("aud") {
            Some(serde_json::Value::String(audience)) => vec![audience.as_str()],
            Some(serde_json::Value::Array(list)) => list
                .iter()
                .filter_map(|audience| audience.as_str())
                .collect(),
            _ => Vec::new(),
        };
        if !audiences
            .iter()
            .any(|audience| self.audiences.iter().any(|expected| expected == audience))
        {
            return Err(BearerError::Audience);
        }
        if let Some(expected) = &self.issuer {
            if claims.get("iss").and_then(|iss| iss.as_str()) != Some(expected.as_str()) {
                return Err(BearerError::Issuer);
            }
        }
        let scope = match claims.get("scope") {
            Some(serde_json::Value::String(names)) => Scope::widest(names),
            Some(serde_json::Value::Array(list)) => list
                .iter()
                .filter_map(|name| name.as_str())
                .filter_map(Scope::parse)
                .max(),
            _ => None,
        }
        .ok_or(BearerError::Scope)?;
        Ok(Principal {
            scope,
            subject: claims
                .get("sub")
                .and_then(|sub| sub.as_str())
                .map(str::to_string),
            via: Via::Jwt,
            // A claim that is present and is not a non-empty string is a
            // malformed token, not an unbound one.
            trigger: match claims.get("trg") {
                None | Some(serde_json::Value::Null) => None,
                Some(serde_json::Value::String(id)) if !id.is_empty() => Some(id.clone()),
                Some(_) => return Err(BearerError::Malformed("trg is not a trigger id")),
            },
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use ring::signature::KeyPair;

    struct FixedClock(u64);

    impl Clock for FixedClock {
        fn now_ms(&self) -> u64 {
            self.0
        }
    }

    const NOW: u64 = 1_800_000_000;

    fn keypair() -> ring::signature::Ed25519KeyPair {
        let rng = ring::rand::SystemRandom::new();
        let pkcs8 = ring::signature::Ed25519KeyPair::generate_pkcs8(&rng).unwrap();
        ring::signature::Ed25519KeyPair::from_pkcs8(pkcs8.as_ref()).unwrap()
    }

    fn b64(bytes: &[u8]) -> String {
        base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(bytes)
    }

    fn sign(pair: &ring::signature::Ed25519KeyPair, claims: serde_json::Value) -> String {
        let signed = format!(
            "{}.{}",
            b64(br#"{"alg":"EdDSA","typ":"JWT"}"#),
            b64(claims.to_string().as_bytes())
        );
        let signature = pair.sign(signed.as_bytes());
        format!("{signed}.{}", b64(signature.as_ref()))
    }

    fn auth(pair: &ring::signature::Ed25519KeyPair, extra: serde_json::Value) -> BearerAuth {
        let mut info = serde_json::json!({
            "keys": [base64::engine::general_purpose::STANDARD.encode(pair.public_key().as_ref())],
            "audience": "agent-1",
        });
        for (key, value) in extra.as_object().unwrap() {
            info[key] = value.clone();
        }
        let info: BearerAuthInfo = serde_json::from_value(info).unwrap();
        BearerAuth::from_config_with(&info, Arc::new(FixedClock(NOW * 1000)))
            .unwrap()
            .unwrap()
    }

    fn claims(scope: &str) -> serde_json::Value {
        serde_json::json!({ "aud": "agent-1", "exp": NOW + 300, "scope": scope, "sub": "user-7" })
    }

    #[test]
    fn a_signed_token_authenticates_with_its_widest_scope() {
        let pair = keypair();
        let auth = auth(&pair, serde_json::json!({}));
        let principal = auth.verify(&sign(&pair, claims("chat"))).unwrap();
        assert_eq!(principal.scope, Scope::Chat);
        assert_eq!(principal.subject.as_deref(), Some("user-7"));
        assert_eq!(principal.via, Via::Jwt);
        assert!(principal.restricted());

        let principal = auth
            .verify(&sign(&pair, claims("chat admin other")))
            .unwrap();
        assert_eq!(principal.scope, Scope::Admin);
        assert!(!principal.restricted());

        let mut array = claims("");
        array["scope"] = serde_json::json!(["connector"]);
        array["aud"] = serde_json::json!(["elsewhere", "agent-1"]);
        assert_eq!(
            auth.verify(&sign(&pair, array)).unwrap().scope,
            Scope::Connector
        );
    }

    #[test]
    fn expiry_and_not_before_are_enforced_with_leeway() {
        let pair = keypair();
        let auth = auth(&pair, serde_json::json!({ "leeway": "10s" }));
        let with = |key: &str, at: u64| {
            let mut claims = claims("chat");
            claims[key] = serde_json::json!(at);
            auth.verify(&sign(&pair, claims))
        };
        assert!(with("exp", NOW - 5).is_ok(), "inside the leeway");
        assert_eq!(with("exp", NOW - 10), Err(BearerError::Expired));
        assert_eq!(with("exp", NOW - 3600), Err(BearerError::Expired));
        assert!(with("nbf", NOW + 5).is_ok(), "inside the leeway");
        assert_eq!(with("nbf", NOW + 60), Err(BearerError::NotYetValid));

        let mut no_exp = claims("chat");
        no_exp.as_object_mut().unwrap().remove("exp");
        assert_eq!(
            auth.verify(&sign(&pair, no_exp)),
            Err(BearerError::Malformed("no exp"))
        );
    }

    #[test]
    fn audience_issuer_and_scope_are_required() {
        let pair = keypair();
        let auth = auth(&pair, serde_json::json!({ "issuer": "https://cp.example" }));
        let mut good = claims("chat");
        good["iss"] = serde_json::json!("https://cp.example");
        assert!(auth.verify(&sign(&pair, good.clone())).is_ok());

        let mut wrong_audience = good.clone();
        wrong_audience["aud"] = serde_json::json!("agent-2");
        assert_eq!(
            auth.verify(&sign(&pair, wrong_audience)),
            Err(BearerError::Audience)
        );
        let mut no_audience = good.clone();
        no_audience.as_object_mut().unwrap().remove("aud");
        assert_eq!(
            auth.verify(&sign(&pair, no_audience)),
            Err(BearerError::Audience)
        );

        let mut wrong_issuer = good.clone();
        wrong_issuer["iss"] = serde_json::json!("https://evil.example");
        assert_eq!(
            auth.verify(&sign(&pair, wrong_issuer)),
            Err(BearerError::Issuer)
        );

        let mut no_scope = good.clone();
        no_scope["scope"] = serde_json::json!("read write");
        assert_eq!(auth.verify(&sign(&pair, no_scope)), Err(BearerError::Scope));
    }

    #[test]
    fn a_token_signed_by_another_key_is_refused() {
        let pair = keypair();
        let other = keypair();
        let auth = auth(&pair, serde_json::json!({}));
        assert_eq!(
            auth.verify(&sign(&other, claims("admin"))),
            Err(BearerError::Signature)
        );
        // A tampered payload under a genuine signature fails the same way.
        let token = sign(&pair, claims("chat"));
        let mut parts: Vec<&str> = token.split('.').collect();
        let forged = b64(claims("admin").to_string().as_bytes());
        parts[1] = &forged;
        assert_eq!(auth.verify(&parts.join(".")), Err(BearerError::Signature));
    }

    #[test]
    fn algorithm_confusion_is_refused() {
        let pair = keypair();
        let auth = auth(&pair, serde_json::json!({}));
        let payload = b64(claims("admin").to_string().as_bytes());

        // `alg: none`, with and without a signature segment.
        let none = format!("{}.{payload}.", b64(br#"{"alg":"none","typ":"JWT"}"#));
        assert_eq!(
            auth.verify(&none),
            Err(BearerError::Algorithm("none".to_string()))
        );
        assert_eq!(
            auth.verify(&format!("{}.{payload}", b64(br#"{"alg":"none"}"#))),
            Err(BearerError::Unknown),
            "two segments are not a token at all"
        );

        // HS256 keyed with the public key — the classic confusion.
        let signed = format!("{}.{payload}", b64(br#"{"alg":"HS256","typ":"JWT"}"#));
        let key = ring::hmac::Key::new(ring::hmac::HMAC_SHA256, pair.public_key().as_ref());
        let mac = ring::hmac::sign(&key, signed.as_bytes());
        assert_eq!(
            auth.verify(&format!("{signed}.{}", b64(mac.as_ref()))),
            Err(BearerError::Algorithm("HS256".to_string()))
        );

        // A genuine EdDSA signature under a header claiming another
        // algorithm is still refused on the header.
        let signed = format!("{}.{payload}", b64(br#"{"alg":"ES256"}"#));
        let signature = pair.sign(signed.as_bytes());
        assert_eq!(
            auth.verify(&format!("{signed}.{}", b64(signature.as_ref()))),
            Err(BearerError::Algorithm("ES256".to_string()))
        );
        // Case matters: `eddsa` is not `EdDSA`.
        let signed = format!("{}.{payload}", b64(br#"{"alg":"eddsa"}"#));
        let signature = pair.sign(signed.as_bytes());
        assert_eq!(
            auth.verify(&format!("{signed}.{}", b64(signature.as_ref()))),
            Err(BearerError::Algorithm("eddsa".to_string()))
        );
    }

    #[test]
    fn static_tokens_match_by_digest_and_carry_their_scope() {
        let info: BearerAuthInfo = serde_json::from_value(serde_json::json!({
            "tokens_sha256": [
                token_digest("operator-token"),
                { "sha256": token_digest("sidecar-token"), "scope": "connector", "name": "sidecar" },
                { "sha256": format!("sha256:{}", token_digest("phone-token")), "scope": "chat" },
            ]
        }))
        .unwrap();
        let auth = BearerAuth::from_config(&info).unwrap().unwrap();
        let operator = auth.verify("operator-token").unwrap();
        assert_eq!(
            (operator.scope, operator.via),
            (Scope::Admin, Via::StaticToken)
        );
        let sidecar = auth.verify("sidecar-token").unwrap();
        assert_eq!(sidecar.scope, Scope::Connector);
        assert_eq!(sidecar.subject.as_deref(), Some("sidecar"));
        assert_eq!(auth.verify("phone-token").unwrap().scope, Scope::Chat);
        assert_eq!(auth.verify("operator-tokem"), Err(BearerError::Unknown));
        assert_eq!(auth.verify(""), Err(BearerError::Unknown));
        // The digest is not the token.
        assert_eq!(
            auth.verify(&token_digest("operator-token")),
            Err(BearerError::Unknown)
        );
        // With no keys configured a JWT-shaped string is just an unknown token.
        assert_eq!(auth.verify("a.b.c"), Err(BearerError::Unknown));
    }

    #[test]
    fn public_keys_parse_from_every_documented_form() {
        let pair = keypair();
        let raw = pair.public_key().as_ref().to_vec();
        let mut spki = SPKI_PREFIX.to_vec();
        spki.extend_from_slice(&raw);
        let standard = base64::engine::general_purpose::STANDARD.encode(&raw);
        let spki_b64 = base64::engine::general_purpose::STANDARD.encode(&spki);
        let pem = format!("-----BEGIN PUBLIC KEY-----\n{spki_b64}\n-----END PUBLIC KEY-----\n");
        for form in [
            standard,
            b64(&raw),
            hex::encode(&raw),
            spki_b64.clone(),
            pem,
        ] {
            assert_eq!(parse_public_key(&form).unwrap().to_vec(), raw, "{form}");
        }
        assert!(parse_public_key("not a key").is_err());
        assert!(parse_public_key(&b64(&raw[..31])).is_err());
        let private = "-----BEGIN PRIVATE KEY-----\nAAAA\n-----END PRIVATE KEY-----";
        assert!(parse_public_key(private)
            .unwrap_err()
            .contains("private key"));
    }

    #[test]
    fn config_errors_are_reported_and_an_empty_block_is_no_auth() {
        let parse = |value: serde_json::Value| {
            let info: BearerAuthInfo = serde_json::from_value(value).unwrap();
            BearerAuth::from_config(&info)
        };
        assert!(parse(serde_json::json!({})).unwrap().is_none());
        assert!(parse(serde_json::json!({ "audience": "x" }))
            .unwrap()
            .is_none());

        let pair = keypair();
        let key = b64(pair.public_key().as_ref());
        let err = parse(serde_json::json!({ "keys": [key] })).unwrap_err();
        assert!(err.contains("audience is required"), "{err}");
        let err = parse(serde_json::json!({ "keys": ["nope"], "audience": "x" })).unwrap_err();
        assert!(err.contains("keys[0]"), "{err}");
        let err = parse(serde_json::json!({ "tokens_sha256": ["abc"] })).unwrap_err();
        assert!(err.contains("tokens_sha256[0]"), "{err}");
        let err = parse(serde_json::json!({
            "tokens_sha256": [{ "sha256": token_digest("t"), "scope": "root" }]
        }))
        .unwrap_err();
        assert!(err.contains("unknown scope"), "{err}");
    }

    /// `hook` is not a rung of the ladder: it opens the webhook entry and
    /// nothing else, and `chat` does not include it.
    #[test]
    fn the_hook_scope_stands_apart() {
        let holder = |scope| Principal {
            trigger: None,
            scope,
            subject: None,
            via: Via::Jwt,
        };
        for (scope, hook, chat, connector, admin) in [
            (Scope::Hook, true, false, false, false),
            (Scope::Chat, false, true, false, false),
            (Scope::Connector, true, true, true, false),
            (Scope::Admin, true, true, true, true),
        ] {
            let principal = holder(scope);
            assert_eq!(principal.allows(Scope::Hook), hook, "{scope:?} hook");
            assert_eq!(principal.allows(Scope::Chat), chat, "{scope:?} chat");
            assert_eq!(
                principal.allows(Scope::Connector),
                connector,
                "{scope:?} connector"
            );
            assert_eq!(principal.allows(Scope::Admin), admin, "{scope:?} admin");
            // Everything below `admin` is confined to the directory
            // allowlist and prompts as a restricted caller, `hook` too.
            assert_eq!(principal.restricted(), scope != Scope::Admin);
        }
        assert_eq!(Scope::parse("hook"), Some(Scope::Hook));
        assert_eq!(Scope::Hook.as_str(), "hook");
        // One credential, one role: `hook` beside a wider scope is that
        // scope.
        assert_eq!(Scope::widest("hook"), Some(Scope::Hook));
        assert_eq!(Scope::widest("hook chat"), Some(Scope::Chat));
        assert_eq!(Scope::widest("connector hook"), Some(Scope::Connector));
    }
}
