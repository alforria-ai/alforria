//! Bedrock SigV4 request signing (TS `protocols/utils/bedrock-auth.ts`).
//!
//! TS delegates to `aws4fetch`'s `AwsV4Signer`. Rust implements SigV4
//! directly on top of the workspace `sha2` dependency plus a hand-rolled
//! HMAC-SHA256 — golden replay never needs signing (recordings carry the
//! signed headers), but the construction is standard and testable. The
//! route-`Auth` integration lands in M2.6; here `sig_v4` is a plain
//! function over the headers it is given.

#![allow(clippy::result_large_err)]

use std::collections::BTreeMap;
use std::time::{SystemTime, UNIX_EPOCH};

use sha2::{Digest, Sha256};

use crate::protocols::shared::invalid_request;
use crate::schema::errors::LlmError;

/// The AWS service name every Bedrock request signs against.
const SERVICE: &str = "bedrock";

/// AWS credentials for SigV4 signing. Bedrock also supports Bearer API key
/// auth, which provider facades configure as route auth instead of SigV4.
/// STS-vended credentials should be refreshed by the consumer (rebuild the
/// model) before they expire; the route does not refresh.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Credentials {
    pub region: String,
    pub access_key_id: String,
    pub secret_access_key: String,
    pub session_token: Option<String>,
}

/// The signed-request input (TS `AuthInput`): the exact JSON bytes and the
/// pre-auth header set.
pub struct AuthInput<'a> {
    pub url: &'a str,
    pub body: &'a str,
    pub headers: &'a BTreeMap<String, String>,
}

/// Bedrock route auth defaults to SigV4 and expects credentials from route
/// configuration (TS: `export const auth = sigV4(undefined)`).
pub fn auth(input: AuthInput<'_>) -> Result<BTreeMap<String, String>, LlmError> {
    sig_v4(None, input)
}

/// Sign the exact JSON bytes with SigV4 using credentials configured on the
/// route. Returns the full signed header set (lowercased names, matching
/// `aws4fetch`): the caller's headers, `content-type: application/json`, and
/// the SigV4 additions (`host`, `x-amz-date`, `x-amz-content-sha256`,
/// `x-amz-security-token`, `authorization`).
pub fn sig_v4(
    credentials: Option<&Credentials>,
    input: AuthInput<'_>,
) -> Result<BTreeMap<String, String>, LlmError> {
    sig_v4_at(credentials, input, SystemTime::now())
}

/// Time-parameterized variant of [`sig_v4`] for deterministic tests.
pub fn sig_v4_at(
    credentials: Option<&Credentials>,
    input: AuthInput<'_>,
    now: SystemTime,
) -> Result<BTreeMap<String, String>, LlmError> {
    let Some(credentials) = credentials else {
        return Err(invalid_request(
            "Bedrock Converse requires either route bearer auth or AWS credentials configured on the route",
        ));
    };
    sign_request(credentials, input, now)
        .map_err(|error| invalid_request(format!("Bedrock Converse SigV4 signing failed: {error}")))
}

fn sign_request(
    credentials: &Credentials,
    input: AuthInput<'_>,
    now: SystemTime,
) -> Result<BTreeMap<String, String>, String> {
    let url = parse_url(input.url)?;
    let (date_stamp, amz_date) = amz_stamp(now);

    // TS sets `content-type: application/json` before signing so routes
    // cannot accidentally send JSON with a stale content type.
    let mut headers: BTreeMap<String, String> = input
        .headers
        .iter()
        .map(|(name, value)| (name.to_ascii_lowercase(), value.trim().to_string()))
        .collect();
    headers.insert("content-type".to_string(), "application/json".to_string());
    headers.insert("x-amz-date".to_string(), amz_date.clone());
    headers.insert(
        "x-amz-content-sha256".to_string(),
        sha256_hex(input.body.as_bytes()),
    );
    if let Some(session_token) = &credentials.session_token {
        headers.insert("x-amz-security-token".to_string(), session_token.clone());
    }

    // Canonical headers: everything to send plus the host from the URL.
    let mut canonical: Vec<(String, String)> = headers.clone().into_iter().collect();
    canonical.push(("host".to_string(), url.host.clone()));
    canonical.sort();
    let canonical_headers = canonical
        .iter()
        .map(|(name, value)| format!("{name}:{value}\n"))
        .collect::<String>();
    let signed_headers = canonical
        .iter()
        .map(|(name, _)| name.as_str())
        .collect::<Vec<_>>()
        .join(";");

    let canonical_request = format!(
        "POST\n{}\n{}\n{}\n{}\n{}",
        url.path,
        canonical_query(&url.query),
        canonical_headers,
        signed_headers,
        sha256_hex(input.body.as_bytes()),
    );
    let scope = format!("{date_stamp}/{}/{SERVICE}/aws4_request", credentials.region);
    let string_to_sign = format!(
        "AWS4-HMAC-SHA256\n{amz_date}\n{scope}\n{}",
        sha256_hex(canonical_request.as_bytes()),
    );
    let signature = hex(&hmac_sha256(
        &signing_key(
            &credentials.secret_access_key,
            &date_stamp,
            &credentials.region,
        ),
        string_to_sign.as_bytes(),
    ));
    headers.insert(
        "authorization".to_string(),
        format!(
            "AWS4-HMAC-SHA256 Credential={}/{scope}, SignedHeaders={signed_headers}, Signature={signature}",
            credentials.access_key_id,
        ),
    );
    Ok(headers)
}

/// SigV4 signing key: HMAC chain from the date through the region, service,
/// and terminal `aws4_request` step.
fn signing_key(secret_access_key: &str, date_stamp: &str, region: &str) -> [u8; 32] {
    let secret = format!("AWS4{secret_access_key}");
    let key = hmac_sha256(secret.as_bytes(), date_stamp.as_bytes());
    let key = hmac_sha256(&key, region.as_bytes());
    let key = hmac_sha256(&key, SERVICE.as_bytes());
    hmac_sha256(&key, b"aws4_request")
}

struct ParsedUrl {
    host: String,
    path: String,
    query: String,
}

fn parse_url(url: &str) -> Result<ParsedUrl, String> {
    let (scheme, rest) = url
        .split_once("://")
        .ok_or_else(|| format!("missing URL scheme: {url}"))?;
    if scheme != "http" && scheme != "https" {
        return Err(format!("unsupported URL scheme: {url}"));
    }
    let (authority, path_query) = match rest.find('/') {
        Some(index) => (&rest[..index], &rest[index..]),
        None => (rest, "/"),
    };
    let mut host = authority;
    if let Some(at) = host.find('@') {
        host = &host[at + 1..];
    }
    if host.starts_with('[') {
        // IPv6 literal: `[::1]:443` → `::1`.
        host = host
            .trim_start_matches('[')
            .split_once(']')
            .map(|(h, _)| h)
            .unwrap_or(host);
    } else if let Some((name, _port)) = host.rsplit_once(':') {
        host = name;
    }
    if host.is_empty() {
        return Err(format!("missing URL host: {url}"));
    }
    let (path, query) = path_query.split_once('?').unwrap_or((path_query, ""));
    Ok(ParsedUrl {
        host: host.to_string(),
        path: path.to_string(),
        query: query.to_string(),
    })
}

/// Canonical query string: percent-encoded, sorted key/value pairs.
fn canonical_query(query: &str) -> String {
    let mut pairs: Vec<(String, String)> = query
        .split('&')
        .filter(|pair| !pair.is_empty())
        .map(|pair| match pair.split_once('=') {
            Some((key, value)) => (key.to_string(), value.to_string()),
            None => (pair.to_string(), String::new()),
        })
        .collect();
    pairs.sort();
    pairs
        .into_iter()
        .map(|(key, value)| format!("{}={}", uri_encode(&key), uri_encode(&value)))
        .collect::<Vec<_>>()
        .join("&")
}

/// AWS-style percent-encoding: everything but unreserved characters is
/// `%XX`-encoded.
fn uri_encode(value: &str) -> String {
    let mut out = String::new();
    for byte in value.as_bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(*byte as char);
            }
            _ => out.push_str(&format!("%{byte:02X}")),
        }
    }
    out
}

/// `(`YYYYMMDD`, `YYYYMMDDThhmmssZ`) for the given instant (UTC).
fn amz_stamp(now: SystemTime) -> (String, String) {
    let seconds_total = now
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_secs() as i64)
        .unwrap_or(0);
    let days = seconds_total.div_euclid(86_400);
    let seconds = seconds_total.rem_euclid(86_400);
    let (year, month, day) = civil_from_days(days);
    let date = format!("{year:04}{month:02}{day:02}");
    let time = format!(
        "{:02}{:02}{:02}",
        seconds / 3_600,
        seconds / 60 % 60,
        seconds % 60
    );
    (date.clone(), format!("{date}T{time}Z"))
}

/// Days since the Unix epoch to a proleptic-Gregorian (year, month, day).
/// Howard Hinnant's `civil_from_days` algorithm.
fn civil_from_days(days: i64) -> (i64, u32, u32) {
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let day_of_era = z.rem_euclid(146_097);
    let year_of_era =
        (day_of_era - day_of_era / 1_460 + day_of_era / 36_524 - day_of_era / 146_096) / 365;
    let year = year_of_era + era * 400;
    let day_of_year = day_of_era - (365 * year_of_era + year_of_era / 4 - year_of_era / 100);
    let month_prime = (5 * day_of_year + 2) / 153;
    let day = (day_of_year - (153 * month_prime + 2) / 5 + 1) as u32;
    let month = if month_prime < 10 {
        (month_prime + 3) as u32
    } else {
        (month_prime - 9) as u32
    };
    (year + i64::from(month <= 2), month, day)
}

fn sha256_hex(data: &[u8]) -> String {
    hex(&Sha256::digest(data))
}

/// HMAC-SHA256 (RFC 2104) over the workspace `sha2` dependency.
fn hmac_sha256(key: &[u8], data: &[u8]) -> [u8; 32] {
    const BLOCK: usize = 64;
    let key = if key.len() > BLOCK {
        Sha256::digest(key).to_vec()
    } else {
        key.to_vec()
    };
    let mut ipad = [0x36u8; BLOCK];
    let mut opad = [0x5cu8; BLOCK];
    for (index, byte) in key.iter().enumerate() {
        ipad[index] ^= *byte;
        opad[index] ^= *byte;
    }
    let mut inner = Sha256::new();
    Digest::update(&mut inner, ipad);
    Digest::update(&mut inner, data);
    let mut outer = Sha256::new();
    Digest::update(&mut outer, opad);
    Digest::update(&mut outer, inner.finalize());
    outer.finalize().into()
}

fn hex(data: &[u8]) -> String {
    let mut out = String::with_capacity(data.len() * 2);
    for byte in data {
        out.push_str(&format!("{byte:02x}"));
    }
    out
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::*;

    fn credentials() -> Credentials {
        Credentials {
            region: "us-east-1".to_string(),
            access_key_id: "AKIDEXAMPLE".to_string(),
            secret_access_key: "wJalrXUtnFEMi/K7MDENG/bPxRfiCYEXAMPLEKEY".to_string(),
            session_token: None,
        }
    }

    fn fixed_time() -> SystemTime {
        UNIX_EPOCH + Duration::from_secs(1_000_000_000)
    }

    #[test]
    fn hmac_sha256_matches_rfc_4231() {
        // RFC 4231 test case 2.
        let mac = hmac_sha256(b"Jefe", b"what do ya want for nothing?");
        assert_eq!(
            hex(&mac),
            "5bdcc146bf60754e6a042426089575c75a003f089d2739839dec58b964ec3843",
        );
        // RFC 4231 test case 1.
        let mac = hmac_sha256(&[0x0b; 20], b"Hi There");
        assert_eq!(
            hex(&mac),
            "b0344c61d8db38535ca8afceaf0bf12b881dc200c9833da726e9376c2e32cff7",
        );
    }

    #[test]
    fn sha256_hex_matches_known_vectors() {
        assert_eq!(
            sha256_hex(b""),
            "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855",
        );
        assert_eq!(
            sha256_hex(b"abc"),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad",
        );
    }

    #[test]
    fn missing_credentials_is_a_request_error() {
        let headers = BTreeMap::new();
        let error = auth(AuthInput {
            url: "https://bedrock-runtime.us-east-1.amazonaws.com/model/x/invoke",
            body: "{}",
            headers: &headers,
        })
        .unwrap_err();
        match error.reason {
            crate::schema::errors::LlmErrorReason::InvalidRequest { message, .. } => {
                assert_eq!(
                    message,
                    "Bedrock Converse requires either route bearer auth or AWS credentials configured on the route",
                );
            }
            reason => panic!("unexpected reason: {reason:?}"),
        }
    }

    #[test]
    fn signing_produces_the_expected_header_shape() {
        let headers = BTreeMap::new();
        let signed = sig_v4_at(
            Some(&credentials()),
            AuthInput {
                url: "https://bedrock-runtime.us-east-1.amazonaws.com/model/us.amazon.nova-micro/invoke",
                body: "{\"messages\":[]}",
                headers: &headers,
            },
            fixed_time(),
        )
        .unwrap();
        assert_eq!(signed["content-type"], "application/json");
        assert_eq!(signed["x-amz-date"], "20010909T014640Z");
        let content_hash = signed["x-amz-content-sha256"].clone();
        assert_eq!(content_hash.len(), 64);
        assert!(
            content_hash.bytes().all(|byte| byte.is_ascii_hexdigit()),
            "content hash is not hex: {content_hash}"
        );
        let authorization = signed["authorization"].clone();
        assert_eq!(
            authorization.split(", ").next().unwrap(),
            "AWS4-HMAC-SHA256 Credential=AKIDEXAMPLE/20010909/us-east-1/bedrock/aws4_request",
        );
        assert_eq!(
            authorization.split(", ").nth(1).unwrap(),
            "SignedHeaders=content-type;host;x-amz-content-sha256;x-amz-date",
        );
        let signature = authorization.rsplit(", Signature=").next().unwrap();
        assert_eq!(signature.len(), 64);
        assert!(
            signature.bytes().all(|byte| byte.is_ascii_hexdigit()),
            "signature is not hex: {signature}"
        );
    }

    #[test]
    fn signing_is_deterministic_and_includes_the_session_token() {
        let credentials = Credentials {
            session_token: Some("session-token".to_string()),
            ..credentials()
        };
        let headers = BTreeMap::new();
        let make_input = || AuthInput {
            url: "https://bedrock-runtime.us-east-1.amazonaws.com/model/x/invoke",
            body: "{}",
            headers: &headers,
        };
        let first = sig_v4_at(Some(&credentials), make_input(), fixed_time()).unwrap();
        let second = sig_v4_at(Some(&credentials), make_input(), fixed_time()).unwrap();
        assert_eq!(first, second);
        assert_eq!(first["x-amz-security-token"], "session-token");
        assert!(first["authorization"].contains(
            "SignedHeaders=content-type;host;x-amz-content-sha256;x-amz-date;x-amz-security-token"
        ));
    }

    #[test]
    fn amz_stamp_formats_utc_dates() {
        assert_eq!(
            amz_stamp(UNIX_EPOCH),
            ("19700101".to_string(), "19700101T000000Z".to_string()),
        );
        assert_eq!(
            amz_stamp(fixed_time()),
            ("20010909".to_string(), "20010909T014640Z".to_string()),
        );
        // 2026-02-01T03:04:05Z (leap year, month/day boundary).
        let leap = UNIX_EPOCH + Duration::from_secs(1_769_915_045);
        assert_eq!(
            amz_stamp(leap),
            ("20260201".to_string(), "20260201T030405Z".to_string()),
        );
    }

    #[test]
    fn parse_url_splits_host_path_and_query() {
        let parsed =
            parse_url("https://bedrock-runtime.eu-west-1.amazonaws.com/a%20b/c?x=1&y=2").unwrap();
        assert_eq!(parsed.host, "bedrock-runtime.eu-west-1.amazonaws.com");
        assert_eq!(parsed.path, "/a%20b/c");
        assert_eq!(parsed.query, "x=1&y=2");
        assert!(parse_url("not-a-url").is_err());
        assert!(parse_url("ftp://example.com").is_err());
    }

    #[test]
    fn canonical_query_encodes_and_sorts() {
        assert_eq!(canonical_query("b=2&a=1&a=0"), "a=0&a=1&b=2",);
        assert_eq!(canonical_query(""), "");
        // Reserved characters are percent-encoded per the SigV4 canonical form.
        assert_eq!(canonical_query("key=a b"), "key=a%20b");
    }
}
