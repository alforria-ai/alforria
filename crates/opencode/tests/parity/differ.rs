//! Structural + value diff over normalized captures (spec PARITY §3.4).
//!
//! - **Structural** differences (different key sets, array lengths, JSON
//!   types, null-vs-value) are always deviations — they are never
//!   allowlisted.
//! - **Value** differences (same structure, different scalar) go through
//!   triage: fix, or allowlist with justification.
//!
//! JSON numbers compare numerically: TS emits `0` where Rust emits `0.0`
//! for the same zero — identical JSON values, different encodings. All
//! other scalars compare exactly, and `null` stays distinct from an
//! absent key (a missing key is a structural difference).

use serde_json::Value;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    Structural,
    Value,
}

#[derive(Debug, Clone)]
pub struct Deviation {
    pub kind: Kind,
    pub path: String,
    pub ts: Value,
    pub rs: Value,
}

pub fn diff(path: &str, ts: &Value, rs: &Value) -> Vec<Deviation> {
    let mut out = Vec::new();
    diff_value(path, ts, rs, &mut out);
    out
}

fn structural(path: &str, ts: &Value, rs: &Value, out: &mut Vec<Deviation>) {
    out.push(Deviation {
        kind: Kind::Structural,
        path: path.to_string(),
        ts: ts.clone(),
        rs: rs.clone(),
    });
}

fn diff_value(path: &str, ts: &Value, rs: &Value, out: &mut Vec<Deviation>) {
    match (ts, rs) {
        (Value::Object(ts_map), Value::Object(rs_map)) => {
            for (key, ts_value) in ts_map {
                let child = format!("{path}.{key}");
                match rs_map.get(key) {
                    Some(rs_value) => diff_value(&child, ts_value, rs_value, out),
                    None => out.push(Deviation {
                        kind: Kind::Structural,
                        path: child,
                        ts: ts_value.clone(),
                        rs: Value::Null,
                    }),
                }
            }
            for (key, rs_value) in rs_map {
                if !ts_map.contains_key(key) {
                    out.push(Deviation {
                        kind: Kind::Structural,
                        path: format!("{path}.{key}"),
                        ts: Value::Null,
                        rs: rs_value.clone(),
                    });
                }
            }
        }
        (Value::Array(ts_items), Value::Array(rs_items)) => {
            if ts_items.len() != rs_items.len() {
                structural(path, ts, rs, out);
                return;
            }
            for (index, (ts_item, rs_item)) in ts_items.iter().zip(rs_items).enumerate() {
                diff_value(&format!("{path}[{index}]"), ts_item, rs_item, out);
            }
        }
        (Value::Number(ts_number), Value::Number(rs_number)) => {
            if ts_number.as_f64() != rs_number.as_f64() {
                out.push(Deviation {
                    kind: Kind::Value,
                    path: path.to_string(),
                    ts: ts.clone(),
                    rs: rs.clone(),
                });
            }
        }
        _ if kind_of(ts) == kind_of(rs) => {
            if ts != rs {
                out.push(Deviation {
                    kind: Kind::Value,
                    path: path.to_string(),
                    ts: ts.clone(),
                    rs: rs.clone(),
                });
            }
        }
        _ => structural(path, ts, rs, out),
    }
}

/// The JSON kind used for structural comparison; numbers are their own
/// kind so `0` vs `0.0` falls to the numeric branch above.
fn kind_of(value: &Value) -> u8 {
    match value {
        Value::Null => 0,
        Value::Bool(_) => 1,
        Value::Number(_) => 2,
        Value::String(_) => 3,
        Value::Array(_) => 4,
        Value::Object(_) => 5,
    }
}
