//! Deep-merge semantics for config values (`merge.rs`).
//!
//! Port of the config merge used by
//! `packages/opencode/src/config/config.ts:40-52`: remeda `mergeDeep` plus the
//! `instructions` concat special case. Merging happens over
//! `serde_json::Value` *before* typed decode so that a JSON `null` (a present
//! value) overrides while an absent key never touches the target.

use serde_json::{Map, Value};

/// remeda `mergeDeep` semantics:
///
/// * both sides objects → merge recursively per key;
/// * anything else (scalars, arrays, mixed) → the source replaces the target;
/// * JSON `null` is a present value and replaces (remeda assigns it);
/// * keys absent from `source` never touch the target.
///
/// Unlike TS there is no `undefined` special case: JSON round-trips cannot
/// produce it.
pub fn merge_deep(target: &mut Value, source: &Value) {
    if let (Value::Object(_), Value::Object(s)) = (&*target, source) {
        let t: &mut Map<String, Value> = target.as_object_mut().unwrap();
        for (key, value) in s {
            match t.get_mut(key) {
                Some(existing) => merge_deep(existing, value),
                None => {
                    t.insert(key.clone(), value.clone());
                }
            }
        }
    } else {
        *target = source.clone();
    }
}

/// The config `PATCH` merge: [`merge_deep`], except that a JSON `null` in
/// `source` deletes the key from `target`, at any depth (RFC 7386 style).
/// Deliberate deviation (open decision 1 of the improvement plan,
/// `review-findings.md`): TS `mergeDeep` writes the `null`, so a client can
/// never clear a setting such as `model`. Nulls never reach the result.
pub fn merge_patch(target: &mut Value, source: &Value) {
    let Value::Object(s) = source else {
        *target = strip_nulls(source);
        return;
    };
    if !target.is_object() {
        *target = Value::Object(Map::new());
    }
    let t = target.as_object_mut().expect("made an object above");
    for (key, value) in s {
        if value.is_null() {
            t.remove(key);
            continue;
        }
        match t.get_mut(key) {
            Some(existing) if value.is_object() => merge_patch(existing, value),
            _ => {
                t.insert(key.clone(), strip_nulls(value));
            }
        }
    }
}

/// `value` without the object keys whose value is `null`, at any depth —
/// what a [`merge_patch`] payload contributes to an empty target.
pub fn strip_nulls(value: &Value) -> Value {
    match value {
        Value::Object(map) => Value::Object(
            map.iter()
                .filter(|(_, v)| !v.is_null())
                .map(|(k, v)| (k.clone(), strip_nulls(v)))
                .collect(),
        ),
        other => other.clone(),
    }
}

/// `mergeConfigConcatArrays` from `config.ts:46-52`: `merge_deep`, then if
/// BOTH sides carry an `instructions` array, replace the result with a
/// concatenation of the two, deduplicated with first-occurrence-wins order
/// (TS `new Set([...target.instructions, ...source.instructions])`).
pub fn merge_config_concat_arrays(target: &mut Value, source: &Value) {
    let target_instructions = target.get("instructions").cloned();
    let source_instructions = source.get("instructions").cloned();
    merge_deep(target, source);

    if let (Some(Value::Array(t)), Some(Value::Array(s))) =
        (target_instructions, source_instructions)
    {
        let mut merged: Vec<Value> = Vec::new();
        for item in t.iter().chain(s.iter()) {
            if !merged.contains(item) {
                merged.push(item.clone());
            }
        }
        if !target.is_object() {
            *target = Value::Object(Map::new());
        }
        target
            .as_object_mut()
            .unwrap()
            .insert("instructions".into(), Value::Array(merged));
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    #[test]
    fn merges_nested_objects() {
        let mut target = json!({ "a": 1, "nested": { "x": 1, "y": 2 }, "only_target": true });
        let source = json!({ "nested": { "y": 3, "z": 4 }, "only_source": true });
        merge_deep(&mut target, &source);
        assert_eq!(
            target,
            json!({ "a": 1, "nested": { "x": 1, "y": 3, "z": 4 }, "only_target": true, "only_source": true }),
        );
    }

    #[test]
    fn scalars_and_arrays_replace() {
        let mut target = json!({ "scalar": 1, "arr": [1, 2, 3] });
        let source = json!({ "scalar": "now a string", "arr": [4] });
        merge_deep(&mut target, &source);
        assert_eq!(target, json!({ "scalar": "now a string", "arr": [4] }));
    }

    #[test]
    fn null_overrides_absent_does_not() {
        let mut target = json!({ "nullme": "x", "keep": "y" });
        let source = json!({ "nullme": null });
        merge_deep(&mut target, &source);
        assert_eq!(target, json!({ "nullme": null, "keep": "y" }));
    }

    #[test]
    fn type_changes_replace() {
        let mut target = json!({ "v": { "deep": 1 } });
        let source = json!({ "v": [1, 2] });
        merge_deep(&mut target, &source);
        assert_eq!(target, json!({ "v": [1, 2] }));

        let mut target = json!({ "v": [1] });
        let source = json!({ "v": { "deep": 1 } });
        merge_deep(&mut target, &source);
        assert_eq!(target, json!({ "v": { "deep": 1 } }));
    }

    #[test]
    fn patch_null_deletes_at_any_depth() {
        let mut target = json!({
            "model": "a/b",
            "keep": 1,
            "provider": { "x": { "options": { "baseURL": "u", "apiKey": "k" } } },
        });
        let source = json!({
            "model": null,
            "absent": null,
            "provider": { "x": { "options": { "baseURL": null } } },
        });
        merge_patch(&mut target, &source);
        assert_eq!(
            target,
            json!({ "keep": 1, "provider": { "x": { "options": { "apiKey": "k" } } } })
        );
    }

    #[test]
    fn patch_merges_like_merge_deep_otherwise() {
        let mut target = json!({ "nested": { "x": 1, "y": 2 }, "arr": [1], "s": { "o": 1 } });
        let source = json!({
            "nested": { "y": 3 },
            "arr": [2],
            "s": "scalar",
            "new": { "a": null, "b": 1 },
        });
        merge_patch(&mut target, &source);
        assert_eq!(
            target,
            json!({ "nested": { "x": 1, "y": 3 }, "arr": [2], "s": "scalar", "new": { "b": 1 } })
        );

        // A scalar target replaced by an object keeps no nulls either.
        let mut target = json!({ "v": 1 });
        merge_patch(
            &mut target,
            &json!({ "v": { "a": null, "b": { "c": null } } }),
        );
        assert_eq!(target, json!({ "v": { "b": {} } }));
    }

    #[test]
    fn instructions_concat_dedupes_target_first() {
        let mut target = json!({ "instructions": ["a", "b"], "other": 1 });
        let source = json!({ "instructions": ["b", "c"], "other": 2 });
        merge_config_concat_arrays(&mut target, &source);
        assert_eq!(target["instructions"], json!(["a", "b", "c"]));
        assert_eq!(target["other"], json!(2));
    }

    #[test]
    fn instructions_left_alone_when_one_side_missing() {
        // Only target has instructions: merge_deep leaves them.
        let mut target = json!({ "instructions": ["a"] });
        let source = json!({ "other": 1 });
        merge_config_concat_arrays(&mut target, &source);
        assert_eq!(target["instructions"], json!(["a"]));

        // Only source has instructions: merge_deep sets them.
        let mut target = json!({});
        let source = json!({ "instructions": ["a", "b"] });
        merge_config_concat_arrays(&mut target, &source);
        assert_eq!(target["instructions"], json!(["a", "b"]));
    }
}
