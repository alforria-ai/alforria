//! JS `Number` wire parity: the TS reference serializes every number
//! through `JSON.stringify`, which emits whole values as integers —
//! `110`, never `110.0`. Whole-valued f64 fields therefore serialize
//! as JSON integers, exactly like the TS wire format.

use serde::Serializer;

pub fn js_f64<S: Serializer>(value: &f64, serializer: S) -> Result<S::Ok, S::Error> {
    if value.fract() == 0.0 && value.abs() < 9.007_199_254_740_992e15 {
        serializer.serialize_i64(*value as i64)
    } else {
        serializer.serialize_f64(*value)
    }
}

pub fn js_opt_f64<S: Serializer>(value: &Option<f64>, serializer: S) -> Result<S::Ok, S::Error> {
    match value {
        Some(value) => js_f64(value, serializer),
        None => serializer.serialize_none(),
    }
}
