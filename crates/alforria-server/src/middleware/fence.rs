//! Fence middleware — port of `shared/fence.ts` + `httpapi/middleware/fence.ts`.
//!
//! Only active with `OPENCODE_WORKSPACE_ID`; `GET`/`HEAD`/`OPTIONS` are
//! ignored. Active mutating requests get a `x-opencode-sync` header carrying
//! the aggregate sequences that changed while the handler ran.

use std::collections::BTreeMap;
use std::sync::Arc;
use std::task::{Context, Poll};

use alforria_core::Storage;
use axum::body::Body;
use axum::http::{Method, Response};
use futures::future::BoxFuture;
use serde_json::Value;
use tower::Service;

/// `shared/fence.ts:9` — aggregate → sequence state.
pub type State = BTreeMap<String, i64>;

pub const HEADER: &str = "x-opencode-sync";

/// `Fence.load` (`shared/fence.ts:11-20`) — the whole `event_sequence` table
/// when no aggregate ids are given.
pub fn load(storage: &Storage) -> State {
    storage.with_connection(|conn| {
        let mut stmt = conn
            .prepare("SELECT aggregate_id, seq FROM event_sequence")
            .expect("event_sequence table exists after migration");
        let rows = stmt
            .query_map([], |row| {
                Ok((row.get::<_, String>(0)?, row.get::<_, i64>(1)?))
            })
            .expect("static column indices are valid");
        rows.collect::<Result<State, _>>()
            .expect("static column indices are valid")
    })
}

/// `Fence.diff` (`shared/fence.ts:23-32`): ids in either state; the new
/// sequence, or `-1` when the aggregate vanished.
pub fn diff(prev: &State, next: &State) -> State {
    let mut ids: Vec<&String> = prev.keys().collect();
    for id in next.keys() {
        if !prev.contains_key(id) {
            ids.push(id);
        }
    }
    ids.into_iter()
        .map(|id| {
            let seq = next.get(id).copied().unwrap_or(-1);
            (id.clone(), seq)
        })
        .filter(|(id, seq)| prev.get(id).copied().unwrap_or(-1) != *seq)
        .collect()
}

/// `Fence.parse` (`shared/fence.ts:34-52`): parse an `x-opencode-sync` request
/// header; invalid JSON and non-objects are rejected, non-integer entries are
/// dropped (arrays are objects in JS and pass through with index keys).
pub fn parse(raw: &str) -> Option<State> {
    let Ok(data) = serde_json::from_str::<Value>(raw) else {
        return None;
    };
    let entries: Vec<(String, Value)> = match &data {
        Value::Object(map) => map.iter().map(|(k, v)| (k.clone(), v.clone())).collect(),
        Value::Array(items) => items
            .iter()
            .enumerate()
            .map(|(i, v)| (i.to_string(), v.clone()))
            .collect(),
        _ => return None,
    };
    Some(
        entries
            .into_iter()
            .filter_map(|(id, value)| Some((id, value.as_i64()?)))
            .collect(),
    )
}

/// The `fenceLayer` middleware (`httpapi/middleware/fence.ts:9-24`).
#[derive(Clone)]
pub struct FenceLayer {
    storage: Arc<Storage>,
    active: bool,
}

impl FenceLayer {
    /// `active` mirrors `Flag.OPENCODE_WORKSPACE_ID` (TS reads the flag per
    /// request; the env var is process-stable).
    pub fn new(storage: Arc<Storage>, active: bool) -> FenceLayer {
        FenceLayer { storage, active }
    }

    pub fn from_env(storage: Arc<Storage>) -> FenceLayer {
        let active = std::env::var("OPENCODE_WORKSPACE_ID")
            .map(|v| !v.is_empty())
            .unwrap_or(false);
        FenceLayer { storage, active }
    }
}

impl<S> tower::Layer<S> for FenceLayer {
    type Service = FenceService<S>;

    fn layer(&self, inner: S) -> Self::Service {
        FenceService {
            inner,
            storage: self.storage.clone(),
            active: self.active,
        }
    }
}

#[derive(Clone)]
pub struct FenceService<S> {
    inner: S,
    storage: Arc<Storage>,
    active: bool,
}

impl<S> Service<axum::http::Request<Body>> for FenceService<S>
where
    S: Service<axum::http::Request<Body>, Response = Response<Body>> + Clone + Send + 'static,
    S::Future: Send,
{
    type Response = S::Response;
    type Error = S::Error;
    type Future = BoxFuture<'static, Result<Self::Response, Self::Error>>;

    fn poll_ready(&mut self, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        self.inner.poll_ready(cx)
    }

    fn call(&mut self, req: axum::http::Request<Body>) -> Self::Future {
        let ignore =
            !self.active || matches!(*req.method(), Method::GET | Method::HEAD | Method::OPTIONS);
        let storage = self.storage.clone();
        let mut inner = self.inner.clone();
        Box::pin(async move {
            if ignore {
                return inner.call(req).await;
            }
            let previous = load(&storage);
            let mut response = inner.call(req).await?;
            let current = diff(&previous, &load(&storage));
            if !current.is_empty() {
                let payload = state_json(&current);
                if let Ok(value) = axum::http::HeaderValue::from_str(&payload) {
                    response.headers_mut().insert(HEADER, value);
                }
            }
            Ok(response)
        })
    }
}

/// `JSON.stringify(state)` — object keys in insertion order; `BTreeMap` gives
/// the deterministic sorted order TS's numeric-looking ids would produce
/// anyway (all aggregate ids are `ses_…`/`evt_…` prefixed, so insertion never
/// happens at runtime).
fn state_json(state: &State) -> String {
    let pairs: Vec<String> = state
        .iter()
        .map(|(id, seq)| format!("{}:{}", serde_json::to_string(id).unwrap(), seq))
        .collect();
    format!("{{{}}}", pairs.join(","))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn state(pairs: &[(&str, i64)]) -> State {
        pairs
            .iter()
            .map(|(id, seq)| ((*id).to_string(), *seq))
            .collect()
    }

    #[test]
    fn diff_detects_adds_updates_and_removals() {
        let prev = state(&[("ses_a", 1), ("ses_b", 2)]);
        let next = state(&[("ses_a", 3), ("ses_b", 2), ("ses_c", 0)]);
        assert_eq!(diff(&prev, &next), state(&[("ses_a", 3), ("ses_c", 0)]));

        // A removed aggregate reports -1 (fence.ts:27).
        let prev = state(&[("ses_a", 1), ("ses_b", 2)]);
        let next = state(&[("ses_a", 1)]);
        assert_eq!(diff(&prev, &next), state(&[("ses_b", -1)]));
    }

    #[test]
    fn diff_empty_when_unchanged() {
        let prev = state(&[("ses_a", 1)]);
        let next = state(&[("ses_a", 1)]);
        assert!(diff(&prev, &next).is_empty());
        assert!(diff(&state(&[]), &state(&[])).is_empty());
    }

    #[test]
    fn parse_accepts_integer_maps() {
        assert_eq!(
            parse(r#"{"ses_a":1,"ses_b":2}"#),
            Some(state(&[("ses_a", 1), ("ses_b", 2)]))
        );
    }

    #[test]
    fn parse_rejects_garbage() {
        assert_eq!(parse("not json"), None);
        assert_eq!(parse("42"), None);
        assert_eq!(parse("null"), None);
        assert_eq!(parse(r#"[1,2]"#), Some(state(&[("0", 1), ("1", 2)])));
        // Non-integer entries are dropped (fence.ts:49).
        assert_eq!(
            parse(r#"{"ses_a":1,"ses_b":"x"}"#),
            Some(state(&[("ses_a", 1)]))
        );
    }
}
