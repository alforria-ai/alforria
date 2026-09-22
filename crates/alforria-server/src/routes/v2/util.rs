//! Shared v2 (`/api/*`) handler helpers — the `Location.response` envelope
//! (`packages/server/src/location.ts:15-27`), the V2 session projection
//! (`core/src/session/info.ts` `fromRow`) and the sessions/message cursor
//! codec (`protocol/groups/session.ts:49-80`, `handlers/message.ts:14-29`).

use axum::body::{Body, Bytes};
use axum::http::{header, StatusCode};
use axum::response::Response;
use base64::Engine;
use serde_json::Value;

use alforria_schema::location::{LocationInfo, LocationProject, LocationRef};
use alforria_schema::permission::{PermissionEffect, PermissionRule};
use alforria_schema::permission_v1::{PermissionV1Action, PermissionV1Ruleset};
use alforria_schema::revert::RevertState;
use alforria_schema::session::{
    SessionInfo, SessionListAnchor, SessionListDirection, SessionTime, SessionTokens,
    SessionTokensCache,
};
use alforria_schema::session_v1::{V1SessionInfo, V1SessionModel};

use crate::error::{ApiError, ServerError};
use crate::middleware::location::LocationContext;

/// 200 with a JSON body.
pub fn json_ok(value: impl serde::Serialize) -> Response {
    let body = serde_json::to_string(&value).expect("serialization cannot fail");
    Response::builder()
        .status(StatusCode::OK)
        .header(header::CONTENT_TYPE, "application/json")
        .body(Body::from(body))
        .expect("static response parts are valid")
}

/// `HttpApiSchema.NoContent` — an empty 204 body.
pub fn no_content() -> Response {
    Response::builder()
        .status(StatusCode::NO_CONTENT)
        .body(Body::empty())
        .expect("static response parts are valid")
}

/// A defect — unexpected core failure routed through the defect-500
/// envelope (`middleware/error.ts:29-41`).
pub fn defect(err: impl std::fmt::Display) -> ServerError {
    ServerError::Core(alforria_core::CoreError::Storage(err.to_string()))
}

// ---------------------------------------------------------------------------
// schema rejections — v2 rejects with `InvalidRequestError` (tagged), not
// the v1 `{name: BadRequest}` envelope (`middleware/schema-error.ts`)
// ---------------------------------------------------------------------------

fn invalid_request(message: impl Into<String>, kind: &str) -> ServerError {
    ApiError::InvalidRequest {
        message: message.into(),
        kind: Some(kind.to_string()),
        field: None,
    }
    .into()
}

/// v2 query schema rejection.
pub fn query_error(message: impl Into<String>) -> ServerError {
    invalid_request(message, "Query")
}

/// v2 payload schema rejection.
pub fn payload_error(message: impl Into<String>) -> ServerError {
    invalid_request(message, "Payload")
}

/// v2 params schema rejection.
pub fn params_error(message: impl Into<String>) -> ServerError {
    invalid_request(message, "Params")
}

/// Parse a required payload body. Invalid JSON or schema mismatches reject
/// as `InvalidRequestError {kind: "Payload"}`.
pub fn parse_payload<T: serde::de::DeserializeOwned>(body: &Bytes) -> Result<T, ServerError> {
    let value: Value = serde_json::from_slice(body)
        .map_err(|err| payload_error(format!("Invalid JSON payload: {err}")))?;
    serde_json::from_value(value).map_err(|err| payload_error(err.to_string()))
}

// ---------------------------------------------------------------------------
// Location envelope
// ---------------------------------------------------------------------------

/// `Location.Info` for a resolved location (`location.ts:29-47`): the
/// project is resolved from the directory through the project registry
/// (`packages/core/src/location.ts:18-27`).
pub fn location_info(location: &LocationContext) -> Result<LocationInfo, ServerError> {
    let context = location
        .services
        .instance(&location.directory)
        .map_err(defect)?;
    Ok(LocationInfo {
        directory: location.directory.to_string_lossy().into_owned(),
        workspace_id: location.workspace_id.clone(),
        project: LocationProject {
            id: context.project.id,
            directory: context.worktree.to_string_lossy().into_owned(),
        },
    })
}

/// `response(data)` (`packages/server/src/location.ts:15-27`) —
/// `{location: {directory, workspaceID, project}, data}`.
pub fn envelope<T: serde::Serialize>(
    location: &LocationContext,
    data: T,
) -> Result<Response, ServerError> {
    let body = serde_json::json!({
        "location": location_info(location)?,
        "data": serde_json::to_value(data).map_err(defect)?,
    });
    let body = serde_json::to_string(&body).map_err(defect)?;
    Ok(Response::builder()
        .status(StatusCode::OK)
        .header(header::CONTENT_TYPE, "application/json")
        .body(Body::from(body))
        .expect("static response parts are valid"))
}

// ---------------------------------------------------------------------------
// V1 → V2 session projection (core/src/session/info.ts `fromRow`)
// ---------------------------------------------------------------------------

/// Map a stored V1 session row onto the V2 `Session.Info` wire shape
/// (`core/src/session/info.ts`). The M5 engine only writes V1 rows; the V2
/// projection is derived per request (M6.7 adapter, spec §9 S1).
pub fn session_from_v1(info: &V1SessionInfo) -> SessionInfo {
    let model = info.model.as_ref().map(v1_model_to_ref);
    let tokens = info.tokens.clone().unwrap_or_else(empty_tokens);
    let revert = info.revert.as_ref().map(|revert| RevertState {
        message_id: revert.message_id.clone(),
        part_id: revert.part_id.clone(),
        snapshot: revert.snapshot.clone(),
        diff: revert.diff.clone(),
        files: None,
    });
    SessionInfo {
        id: info.id.clone(),
        parent_id: info.parent_id.clone(),
        project_id: info.project_id.clone(),
        agent: info.agent.clone(),
        model,
        cost: info.cost.unwrap_or(0.0),
        tokens: SessionTokens {
            input: tokens.input,
            output: tokens.output,
            reasoning: tokens.reasoning,
            cache: SessionTokensCache {
                read: tokens.cache.read,
                write: tokens.cache.write,
            },
        },
        time: SessionTime {
            created: info.time.created as i64,
            updated: info.time.updated as i64,
            archived: info.time.archived.map(|time| time as i64),
        },
        title: info.title.clone(),
        location: LocationRef {
            directory: info.directory.clone(),
            workspace_id: info.workspace_id.clone(),
            project: None,
        },
        subpath: info.path.clone().filter(|path| !path.is_empty()),
        revert,
    }
}

/// The V1 session model keeps `variant` optional on the wire; the V2
/// `fromRow` projection always materializes it (`info.ts:29-33`).
pub fn v1_model_to_ref(model: &V1SessionModel) -> alforria_schema::model::ModelRef {
    alforria_schema::model::ModelRef {
        id: model.id.clone(),
        provider_id: model.provider_id.clone(),
        variant: Some(
            model
                .variant
                .clone()
                .unwrap_or_else(|| "default".to_string()),
        ),
    }
}

fn empty_tokens() -> SessionTokens {
    SessionTokens {
        input: 0.0,
        output: 0.0,
        reasoning: 0.0,
        cache: SessionTokensCache {
            read: 0.0,
            write: 0.0,
        },
    }
}

// ---------------------------------------------------------------------------
// V1 → V2 permission-rule relabel (`{permission, pattern, action}` ↔
// `{action, resource, effect}`)
// ---------------------------------------------------------------------------

/// Relabel a V1 agent ruleset onto the V2 wire shape. The M5 registry
/// stores agent permissions in the V1 layout; V2 evaluates the same rules
/// with renamed fields (M6.7 adapter).
pub fn relabel_ruleset(ruleset: &PermissionV1Ruleset) -> Vec<PermissionRule> {
    ruleset
        .iter()
        .map(|rule| PermissionRule {
            action: rule.permission.clone(),
            resource: rule.pattern.clone(),
            effect: match rule.action {
                PermissionV1Action::Allow => PermissionEffect::Allow,
                PermissionV1Action::Deny => PermissionEffect::Deny,
                PermissionV1Action::Ask => PermissionEffect::Ask,
            },
        })
        .collect()
}

/// `Permission.evaluate` (`core/src/permission.ts`) — the last matching
/// rule wins, an unmatched action/resource pair defaults to `ask`.
pub fn evaluate_permission(
    action: &str,
    resource: &str,
    rules: &[PermissionRule],
) -> PermissionRule {
    rules
        .iter()
        .rev()
        .find(|rule| {
            alforria_core::tool::permission::wildcard_match(action, &rule.action)
                && alforria_core::tool::permission::wildcard_match(resource, &rule.resource)
        })
        .cloned()
        .unwrap_or(PermissionRule {
            action: action.to_string(),
            resource: "*".to_string(),
            effect: PermissionEffect::Ask,
        })
}

// ---------------------------------------------------------------------------
// cursor codec — base64url of the JSON cursor (`Encoding.encodeBase64Url`)
// ---------------------------------------------------------------------------

/// Serialize a JSON object literal from pre-serialized fragments in the
/// given order (TS `JSON.stringify` key order).
fn object(pairs: &[(&str, String)]) -> String {
    let mut out = String::from("{");
    for (i, (key, value)) in pairs.iter().enumerate() {
        if i > 0 {
            out.push(',');
        }
        out.push_str(&serde_json::to_string(key).expect("key serialization cannot fail"));
        out.push(':');
        out.push_str(value);
    }
    out.push('}');
    out
}

fn field<T: serde::Serialize>(value: &T) -> String {
    serde_json::to_string(value).expect("value serialization cannot fail")
}

/// Encode the `{...query, anchor}` sessions cursor
/// (`protocol/groups/session.ts:49-80`). `query` keys are emitted in the
/// `SessionsQuery` declaration order with the anchor last, absent keys
/// omitted (JSON.stringify drops undefined).
pub fn encode_sessions_cursor(query: &SessionsQuery, anchor: &SessionListAnchor) -> String {
    let mut pairs: Vec<(&str, String)> = Vec::new();
    if let Some(workspace) = &query.workspace {
        pairs.push(("workspace", field(workspace)));
    }
    if let Some(limit) = &query.limit {
        pairs.push(("limit", field(limit)));
    }
    if let Some(order) = &query.order {
        pairs.push(("order", field(order)));
    }
    if let Some(search) = &query.search {
        pairs.push(("search", field(search)));
    }
    if let Some(directory) = &query.directory {
        pairs.push(("directory", field(directory)));
    }
    if let Some(project) = &query.project {
        pairs.push(("project", field(project)));
    }
    if let Some(subpath) = &query.subpath {
        pairs.push(("subpath", field(subpath)));
    }
    let anchor = object(&[
        ("id", field(&anchor.id)),
        ("time", field(&anchor.time)),
        ("direction", field(&anchor.direction)),
    ]);
    pairs.push(("anchor", anchor));
    let json = object(&pairs);
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(json)
}

/// One decoded sessions cursor.
#[derive(Debug, Clone)]
pub struct DecodedCursor {
    /// The decoded cursor fields (without the anchor).
    pub query: SessionsQuery,
    pub anchor: SessionListAnchor,
}

/// Decode a sessions cursor; `Err(())` is the TS `InvalidCursorError`
/// (`SessionsCursor.parse`, `protocol/groups/session.ts:60-80`).
pub fn decode_sessions_cursor(input: &str) -> Result<DecodedCursor, ()> {
    let bytes = base64::engine::general_purpose::URL_SAFE_NO_PAD
        .decode(input)
        .map_err(|_| ())?;
    let json: Value = serde_json::from_slice(&bytes).map_err(|_| ())?;
    let Value::Object(map) = json else {
        return Err(());
    };
    let str_field = |key: &str| -> Result<Option<String>, ()> {
        match map.get(key) {
            None | Some(Value::Null) => Ok(None),
            Some(Value::String(value)) => Ok(Some(value.clone())),
            _ => Err(()),
        }
    };

    // `withCursor` omits limit; `AbsolutePath`/`Project.ID`/`WorkspaceV2.ID`
    // are branded strings, only the enum-typed fields reject.
    let directory = str_field("directory")?;
    let order = str_field("order")?;
    if let Some(order) = &order {
        if order != "asc" && order != "desc" {
            return Err(());
        }
    }
    let query = SessionsQuery {
        workspace: str_field("workspace")?,
        limit: match map.get("limit") {
            None | Some(Value::Null) => None,
            Some(value @ Value::Number(_)) => {
                let limit = value.as_u64().ok_or(())?;
                if limit == 0 {
                    return Err(());
                }
                Some(limit)
            }
            _ => return Err(()),
        },
        order,
        search: str_field("search")?,
        directory,
        project: str_field("project")?,
        subpath: str_field("subpath")?,
        cursor: None,
    };

    let Value::Object(anchor) = map.get("anchor").ok_or(())? else {
        return Err(());
    };
    let id = match anchor.get("id") {
        Some(Value::String(id)) if id.starts_with("ses_") => id.clone(),
        _ => return Err(()),
    };
    let time = match anchor.get("time") {
        Some(Value::Number(time)) if time.is_f64() || time.is_u64() || time.is_i64() => {
            time.as_f64().ok_or(())?
        }
        _ => return Err(()),
    };
    let direction = match anchor.get("direction") {
        Some(Value::String(direction)) if direction == "previous" => SessionListDirection::Previous,
        Some(Value::String(direction)) if direction == "next" => SessionListDirection::Next,
        _ => return Err(()),
    };

    Ok(DecodedCursor {
        query,
        anchor: SessionListAnchor {
            id,
            time,
            direction,
        },
    })
}

/// The parsed `SessionsQuery` (`protocol/groups/session.ts:17-50`).
#[derive(Debug, Clone, Default)]
pub struct SessionsQuery {
    pub workspace: Option<String>,
    pub limit: Option<u64>,
    pub order: Option<String>,
    pub search: Option<String>,
    pub directory: Option<String>,
    pub project: Option<String>,
    pub subpath: Option<String>,
    pub cursor: Option<String>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sessions_cursor_roundtrip() {
        let query = SessionsQuery {
            workspace: Some("wrk_1".to_string()),
            order: Some("desc".to_string()),
            search: Some("hello".to_string()),
            directory: None,
            project: None,
            subpath: None,
            limit: None,
            cursor: None,
        };
        let cursor = encode_sessions_cursor(
            &query,
            &SessionListAnchor {
                id: "ses_1".to_string(),
                time: 1000.0,
                direction: SessionListDirection::Next,
            },
        );
        let decoded = decode_sessions_cursor(&cursor).unwrap();
        assert_eq!(decoded.query.workspace.as_deref(), Some("wrk_1"));
        assert_eq!(decoded.query.order.as_deref(), Some("desc"));
        assert_eq!(decoded.query.search.as_deref(), Some("hello"));
        assert_eq!(decoded.query.directory, None);
        assert_eq!(decoded.anchor.id, "ses_1");
        assert_eq!(decoded.anchor.time, 1000.0);
        assert_eq!(decoded.anchor.direction, SessionListDirection::Next);
    }

    #[test]
    fn invalid_sessions_cursors_reject() {
        assert!(decode_sessions_cursor("not base64 !!!").is_err());
        assert!(
            decode_sessions_cursor("aGVsbG8").is_err(),
            "valid b64, not JSON"
        );
        let bad = base64::engine::general_purpose::URL_SAFE_NO_PAD
            .encode(r#"{"anchor":{"id":"x","time":1,"direction":"next"}}"#);
        assert!(decode_sessions_cursor(&bad).is_err(), "non-ses id");
        let bad = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(r#"{}"#);
        assert!(decode_sessions_cursor(&bad).is_err(), "missing anchor");
    }

    #[test]
    fn relabel_and_evaluate() {
        let ruleset: PermissionV1Ruleset = serde_json::from_value(serde_json::json!([
            {"permission": "bash", "pattern": "git *", "action": "allow"},
            {"permission": "bash", "pattern": "rm -rf *", "action": "deny"},
        ]))
        .unwrap();
        let relabeled = relabel_ruleset(&ruleset);
        assert_eq!(relabeled[0].action, "bash");
        assert_eq!(relabeled[0].resource, "git *");
        let matched = evaluate_permission("bash", "git push", &relabeled);
        assert_eq!(matched.effect, PermissionEffect::Allow);
        assert_eq!(
            evaluate_permission("bash", "rm -rf /", &relabeled).effect,
            PermissionEffect::Deny
        );
        assert_eq!(
            evaluate_permission("read", "x", &relabeled).effect,
            PermissionEffect::Ask
        );
    }
}
