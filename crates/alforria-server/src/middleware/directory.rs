//! The request's directory: one resolver, used by the directory allowlist
//! gate, by the location layer and by alforria's own routes.
//!
//! There used to be three: the gate decoded `?directory=`,
//! `?location[directory]=` and the `x-opencode-directory` header its own
//! way and treated any of them as "the directory this request names"; the
//! location layer decoded the ones its surface reads; and each alforria-own
//! route read `?directory=` by itself. A credential confined to
//! `server.directories` named an allowed directory under a spelling the
//! handler ignored, passed the gate, and the handler acted in the server's
//! working directory. A second decoding of the same value (`%252e%252e`)
//! split the two in the same way.
//!
//! So: [`resolve`] is the only place a directory is read off a request. The
//! gate validates exactly the value it returns; the location layer builds
//! its context from the same value; an own route cannot read the request
//! itself — the auth layer puts the resolved directory in the request
//! extensions ([`RequestDirectory`]) and the handler takes it from there. A
//! route mounted outside the layer has no extension and fails closed.

use std::path::{Path, PathBuf};

use axum::http::HeaderMap;

use super::auth::query_param;

/// The `?directory=` spelling (v1 `workspace-routing.ts:86-88`).
pub const QUERY: &str = "directory";
/// The `?location[directory]=` spelling (v2 `location.ts:29-47`).
pub const LOCATION_QUERY: &str = "location[directory]";
/// The header spelling, read by both surfaces.
pub const HEADER: &str = "x-opencode-directory";

/// Which spellings a route's contract reads, in order.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Family {
    /// The frozen v1 surface: `?directory=`, else the header.
    V1,
    /// The frozen v2 surface (`/api/…`): `?location[directory]=`, else the
    /// header.
    V2,
    /// alforria's own routes outside the frozen tables: any of the three —
    /// `?directory=`, else `?location[directory]=`, else the header — so a
    /// client of either surface addresses them the way it addresses the
    /// rest.
    Own,
}

impl Family {
    /// The family of a request path.
    pub fn of(path: &str) -> Family {
        if super::scope::defaults_to_cwd(path) {
            Family::Own
        } else if path == "/api" || path.starts_with("/api/") {
            Family::V2
        } else {
            Family::V1
        }
    }
}

/// What [`resolve`] found.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Resolved {
    /// The directory as the request gave it, decoded; the working
    /// directory when it named none.
    pub raw: String,
    /// [`Resolved::raw`] made absolute and real
    /// ([`crate::state::resolve_directory`]): what a handler acts in and
    /// what the allowlist is asked about.
    pub directory: PathBuf,
    /// Whether the request named it (false: the working directory).
    pub named: bool,
}

/// The resolved directory of a request to one of alforria's own routes,
/// placed in the request extensions by the auth layer after the allowlist
/// admitted it. The only way such a handler learns its directory.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RequestDirectory(pub PathBuf);

fn header_value(headers: &HeaderMap, name: &str) -> Option<String> {
    headers
        .get(name)
        .and_then(|value| value.to_str().ok())
        .map(str::to_string)
}

fn non_empty(value: Option<String>) -> Option<String> {
    value.filter(|value| !value.is_empty())
}

/// The directory `family` reads off the request, decoded, if it names one.
///
/// Decoding follows the surface (`instance-context.ts:15-21`,
/// `location.ts:41-47`): v1 decodes whatever it resolved once more, v2
/// decodes the header only. An own route decodes like v1.
fn named(family: Family, query: Option<&str>, headers: &HeaderMap) -> Option<String> {
    let query_directory = || non_empty(query_param(query, QUERY));
    let location_directory = || non_empty(query_param(query, LOCATION_QUERY));
    match family {
        // An empty header is not a miss on v1 (`??`): it resolves to the
        // working directory through `path.resolve("")`, like no value.
        Family::V1 => query_directory()
            .or_else(|| header_value(headers, HEADER))
            .map(|value| decode(&value)),
        Family::V2 => location_directory()
            .or_else(|| non_empty(header_value(headers, HEADER)).map(|value| decode(&value))),
        Family::Own => query_directory()
            .or_else(location_directory)
            .or_else(|| non_empty(header_value(headers, HEADER)))
            .map(|value| decode(&value)),
    }
}

/// The directory a request addresses. The single resolver: see the module
/// docs.
pub fn resolve(family: Family, query: Option<&str>, headers: &HeaderMap) -> Resolved {
    match named(family, query, headers).filter(|value| !value.is_empty()) {
        Some(raw) => Resolved {
            directory: crate::state::resolve_directory(Path::new(&raw)),
            raw,
            named: true,
        },
        None => {
            let cwd = crate::state::cwd();
            Resolved {
                raw: cwd.display().to_string(),
                directory: crate::state::resolve_directory(&cwd),
                named: false,
            }
        }
    }
}

/// Every directory the request spells, under any of the three spellings,
/// decoded the way the family that reads that spelling decodes it. The gate
/// refuses a confined credential that names a directory outside the
/// allowlist anywhere — also under a spelling this route ignores.
pub fn spelled(query: Option<&str>, headers: &HeaderMap) -> Vec<String> {
    [
        non_empty(query_param(query, QUERY)).map(|value| decode(&value)),
        non_empty(query_param(query, LOCATION_QUERY)),
        non_empty(header_value(headers, HEADER)).map(|value| decode(&value)),
    ]
    .into_iter()
    .flatten()
    .collect()
}

/// `decodeURIComponent` — `Err` on malformed percent escapes or invalid
/// UTF-8.
pub(crate) fn decode_uri_component(input: &str) -> Result<String, ()> {
    let bytes = input.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' {
            if i + 2 >= bytes.len() {
                return Err(());
            }
            let (Some(hi), Some(lo)) = (hex(bytes[i + 1]), hex(bytes[i + 2])) else {
                return Err(());
            };
            out.push(hi * 16 + lo);
            i += 3;
        } else {
            out.push(bytes[i]);
            i += 1;
        }
    }
    String::from_utf8(out).map_err(|_| ())
}

fn hex(byte: u8) -> Option<u8> {
    match byte {
        b'0'..=b'9' => Some(byte - b'0'),
        b'a'..=b'f' => Some(byte - b'a' + 10),
        b'A'..=b'F' => Some(byte - b'A' + 10),
        _ => None,
    }
}

/// Best-effort `decode()` (`instance-context.ts:15-21`, `location.ts:41-47`):
/// malformed input is kept as it is. The one decoder of a directory value.
pub(crate) fn decode(input: &str) -> String {
    decode_uri_component(input).unwrap_or_else(|_| input.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn headers(directory: Option<&str>) -> HeaderMap {
        let mut headers = HeaderMap::new();
        if let Some(directory) = directory {
            headers.insert(HEADER, directory.parse().unwrap());
        }
        headers
    }

    #[test]
    fn each_family_reads_its_own_spellings() {
        let query = Some("directory=/q&location%5Bdirectory%5D=/l");
        let h = headers(Some("/h"));
        assert_eq!(resolve(Family::V1, query, &h).raw, "/q");
        assert_eq!(resolve(Family::V2, query, &h).raw, "/l");
        assert_eq!(resolve(Family::Own, query, &h).raw, "/q");
        // An own route falls through all three; the frozen surfaces skip
        // the spelling that is not theirs.
        let query = Some("location%5Bdirectory%5D=/l");
        assert_eq!(resolve(Family::Own, query, &h).raw, "/l");
        assert_eq!(resolve(Family::V1, query, &h).raw, "/h");
        assert_eq!(resolve(Family::Own, None, &h).raw, "/h");
        assert_eq!(resolve(Family::V2, Some("directory=/q"), &h).raw, "/h");
        // Nothing named: the working directory, and it says so.
        let none = resolve(Family::Own, None, &headers(None));
        assert!(!none.named);
        assert_eq!(
            none.directory,
            crate::state::resolve_directory(&crate::state::cwd())
        );
        assert_eq!(spelled(query, &h), vec!["/l".to_string(), "/h".to_string()]);
    }

    #[test]
    fn the_resolved_value_is_decoded_exactly_once_more_like_the_handler() {
        // `%252e%252e` is `%2e%2e` after the query decoding and `..` after
        // the surface's own: the gate must be asked about the latter.
        let query = Some("directory=/srv/allowed/%252e%252e/etc");
        let resolved = resolve(Family::V1, query, &headers(None));
        assert_eq!(resolved.raw, "/srv/allowed/../etc");
        assert_eq!(resolved.directory, PathBuf::from("/srv/etc"));
        let resolved = resolve(Family::Own, query, &headers(None));
        assert_eq!(resolved.directory, PathBuf::from("/srv/etc"));
    }

    #[test]
    fn families_follow_the_path() {
        assert_eq!(Family::of("/trigger/trg_1/hook"), Family::Own);
        assert_eq!(Family::of("/file/raw"), Family::Own);
        assert_eq!(Family::of("/session/ses_1/goal"), Family::Own);
        assert_eq!(Family::of("/session/ses_1/message"), Family::V1);
        assert_eq!(Family::of("/api/session"), Family::V2);
    }

    #[test]
    fn decode_uri_component_matrix() {
        assert_eq!(decode_uri_component("/a%20b").as_deref(), Ok("/a b"));
        assert_eq!(
            decode_uri_component("%2Frepo%2Fx").as_deref(),
            Ok("/repo/x")
        );
        assert!(decode_uri_component("%").is_err());
        assert!(decode_uri_component("%zz").is_err());
        assert!(decode_uri_component("%2").is_err());
        // Invalid UTF-8.
        assert!(decode_uri_component("%ff%fe").is_err());
        // `+` is *not* decoded to space (decodeURIComponent, not form).
        assert_eq!(decode_uri_component("a+b").as_deref(), Ok("a+b"));
    }

    #[test]
    fn decode_keeps_malformed_input() {
        assert_eq!(decode("/a%20b"), "/a b");
        assert_eq!(decode("/a%"), "/a%");
    }
}
