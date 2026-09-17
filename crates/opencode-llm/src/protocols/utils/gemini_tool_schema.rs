//! Gemini-specific JSON-Schema projection for tool parameters
//! (from `protocols/utils/gemini-tool-schema.ts`).
//!
//! Gemini accepts a JSON Schema-like dialect for tool parameters, but rejects a
//! handful of common JSON Schema shapes. Keep this projection isolated so the
//! Gemini protocol file still reads like the other protocol modules.

use serde_json::Value;

use crate::schema::ids::JsonMap;

const SCHEMA_INTENT_KEYS: [&str; 14] = [
    "type",
    "properties",
    "items",
    "prefixItems",
    "enum",
    "const",
    "$ref",
    "additionalProperties",
    "patternProperties",
    "required",
    "not",
    "if",
    "then",
    "else",
];

fn has_combiner_record(schema: &JsonMap) -> bool {
    schema.get("anyOf").is_some_and(Value::is_array)
        || schema.get("oneOf").is_some_and(Value::is_array)
        || schema.get("allOf").is_some_and(Value::is_array)
}

fn has_schema_intent(schema: &Value) -> bool {
    schema.as_object().is_some_and(|record| {
        has_combiner_record(record)
            || SCHEMA_INTENT_KEYS
                .iter()
                .any(|key| record.contains_key(*key))
    })
}

/// JS `String(value)` for the JSON values that can appear in an enum.
fn js_string(value: &Value) -> String {
    match value {
        Value::Null => "null".to_string(),
        Value::Bool(flag) => flag.to_string(),
        Value::Number(number) => match (number.as_i64(), number.as_u64(), number.as_f64()) {
            (Some(int), _, _) => int.to_string(),
            (None, Some(uint), _) => uint.to_string(),
            (None, None, Some(float)) => format!("{}", float),
            (None, None, None) => number.to_string(),
        },
        Value::String(text) => text.clone(),
        Value::Array(items) => items
            .iter()
            .map(|item| match item {
                // Array.prototype.join treats null/undefined as empty strings.
                Value::Null => String::new(),
                other => js_string(other),
            })
            .collect::<Vec<_>>()
            .join(","),
        Value::Object(_) => "[object Object]".to_string(),
    }
}

fn type_is(result: &JsonMap, expected: &str) -> bool {
    result.get("type").and_then(Value::as_str) == Some(expected)
}

fn sanitize_node(schema: &Value) -> Value {
    let Some(record) = schema.as_object() else {
        return match schema {
            Value::Array(items) => Value::Array(items.iter().map(sanitize_node).collect()),
            other => other.clone(),
        };
    };

    let mut result = JsonMap::new();
    for (key, value) in record {
        let sanitized = if key == "enum" && value.is_array() {
            Value::Array(
                value
                    .as_array()
                    .unwrap()
                    .iter()
                    .map(js_string_value)
                    .collect(),
            )
        } else {
            sanitize_node(value)
        };
        result.insert(key.clone(), sanitized);
    }

    if result.get("enum").is_some_and(Value::is_array)
        && (type_is(&result, "integer") || type_is(&result, "number"))
    {
        result.insert("type".into(), Value::String("string".into()));
    }

    if type_is(&result, "object")
        && result.get("properties").is_some_and(Value::is_object)
        && result.get("required").is_some_and(Value::is_array)
    {
        let properties = result.get("properties").unwrap().as_object().unwrap();
        let filtered: Vec<Value> = result
            .get("required")
            .unwrap()
            .as_array()
            .unwrap()
            .iter()
            .filter(|field| {
                field
                    .as_str()
                    .is_some_and(|name| properties.contains_key(name))
            })
            .cloned()
            .collect();
        result.insert("required".into(), Value::Array(filtered));
    }

    if type_is(&result, "array") && !has_combiner_record(&result) {
        let mut items = match result.get("items") {
            // `result.items ?? {}` — absent and null both become {}.
            None | Some(Value::Null) => Value::Object(JsonMap::new()),
            Some(other) => other.clone(),
        };
        if items.as_object().is_some() && !has_schema_intent(&items) {
            let mut with_type = items.as_object().unwrap().clone();
            with_type.insert("type".into(), Value::String("string".into()));
            items = Value::Object(with_type);
        }
        result.insert("items".into(), items);
    }

    if result
        .get("type")
        .and_then(Value::as_str)
        .is_some_and(|type_| type_ != "object")
        && !has_combiner_record(&result)
    {
        result.remove("properties");
        result.remove("required");
    }

    Value::Object(result)
}

fn js_string_value(value: &Value) -> Value {
    Value::String(js_string(value))
}

fn empty_object_schema(schema: &JsonMap) -> bool {
    schema.get("type").and_then(Value::as_str) == Some("object")
        && match schema.get("properties") {
            Some(properties) if properties.is_object() => {
                properties.as_object().unwrap().is_empty()
            }
            _ => true,
        }
        && !js_truthy(schema.get("additionalProperties"))
}

/// JS truthiness for values that can appear on a schema node.
fn js_truthy(value: Option<&Value>) -> bool {
    match value {
        None | Some(Value::Null) => false,
        Some(Value::Bool(flag)) => *flag,
        Some(Value::Number(number)) => number.as_f64() != Some(0.0),
        Some(Value::String(text)) => !text.is_empty(),
        Some(Value::Array(_)) | Some(Value::Object(_)) => true,
    }
}

/// Project one schema node, or `None` when the node collapses away
/// (non-records and empty object schemas). Object entries whose projection is
/// `undefined` are dropped, matching the JSON encoding of the TS result.
fn project_node(schema: &Value) -> Option<Value> {
    let record = schema.as_object()?;
    if empty_object_schema(record) {
        return None;
    }

    let mut result = JsonMap::new();

    if let Some(value) = record.get("description") {
        result.insert("description".into(), value.clone());
    }
    if let Some(value) = record.get("required") {
        result.insert("required".into(), value.clone());
    }
    if let Some(value) = record.get("format") {
        result.insert("format".into(), value.clone());
    }

    let type_value = match record.get("type") {
        Some(Value::Array(variants)) => variants
            .iter()
            .find(|variant| variant.as_str() != Some("null"))
            .cloned(),
        other => other.cloned(),
    };
    if let Some(value) = type_value {
        result.insert("type".into(), value);
    }

    let nullable = match record.get("type") {
        Some(Value::Array(variants)) => variants
            .iter()
            .any(|variant| variant.as_str() == Some("null")),
        _ => false,
    };
    if nullable {
        result.insert("nullable".into(), Value::Bool(true));
    }

    match record.get("const") {
        Some(const_value) => {
            result.insert("enum".into(), Value::Array(vec![const_value.clone()]));
        }
        None => {
            if let Some(value) = record.get("enum") {
                result.insert("enum".into(), value.clone());
            }
        }
    }

    if let Some(properties) = record.get("properties").filter(|value| value.is_object()) {
        let mut projected = JsonMap::new();
        for (key, value) in properties.as_object().unwrap() {
            if let Some(value) = project_node(value) {
                projected.insert(key.clone(), value);
            }
        }
        result.insert("properties".into(), Value::Object(projected));
    }

    match record.get("items") {
        Some(Value::Array(variants)) => {
            let projected = variants
                .iter()
                .map(|variant| project_node(variant).unwrap_or(Value::Null))
                .collect::<Vec<_>>();
            result.insert("items".into(), Value::Array(projected));
        }
        Some(other) => {
            if let Some(value) = project_node(other) {
                result.insert("items".into(), value);
            }
        }
        None => {}
    }

    for key in ["allOf", "anyOf", "oneOf"] {
        if let Some(Value::Array(variants)) = record.get(key) {
            let projected = variants
                .iter()
                .map(|variant| project_node(variant).unwrap_or(Value::Null))
                .collect::<Vec<_>>();
            result.insert(key.into(), Value::Array(projected));
        }
    }

    if let Some(value) = record.get("minLength") {
        result.insert("minLength".into(), value.clone());
    }

    Some(Value::Object(result))
}

pub fn convert(schema: &Value) -> Option<Value> {
    project_node(&sanitize_node(schema))
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    #[test]
    fn convert_coerces_integer_enums_to_strings() {
        let value = convert(&json!({
            "type": "object",
            "properties": {"kind": {"type": "integer", "enum": [1, 2]}},
            "required": ["kind"],
        }))
        .unwrap();
        assert_eq!(
            value,
            json!({
                "type": "object",
                "required": ["kind"],
                "properties": {"kind": {"type": "string", "enum": ["1", "2"]}},
            }),
        );
    }

    #[test]
    fn convert_drops_required_without_a_property() {
        let value = convert(&json!({
            "type": "object",
            "properties": {"a": {"type": "string"}},
            "required": ["a", "b"],
        }))
        .unwrap();
        assert_eq!(value["required"], json!(["a"]));
    }

    #[test]
    fn convert_repairs_untyped_arrays() {
        let value = convert(&json!({"type": "array"})).unwrap();
        assert_eq!(value["items"], json!({"type": "string"}));

        let value = convert(&json!({"type": "array", "items": {"enum": ["a"]}})).unwrap();
        assert_eq!(value["items"], json!({"enum": ["a"]}));
    }

    #[test]
    fn convert_unwraps_nullable_type_arrays() {
        let value = convert(&json!({
            "type": "object",
            "properties": {"a": {"type": ["string", "null"]}},
        }))
        .unwrap();
        assert_eq!(
            value["properties"]["a"],
            json!({"type": "string", "nullable": true}),
        );
    }

    #[test]
    fn convert_all_null_type_arrays_drop_the_type() {
        let value = convert(&json!({"type": ["null"]})).unwrap();
        assert!(value.get("type").is_none());
        assert_eq!(value.get("nullable"), Some(&json!(true)));
    }

    #[test]
    fn convert_maps_const_to_enum() {
        let value = convert(&json!({"const": "fixed"})).unwrap();
        assert_eq!(value["enum"], json!(["fixed"]));
    }

    #[test]
    fn convert_drops_empty_object_properties() {
        let value = convert(&json!({
            "type": "object",
            "properties": {"a": {"type": "object", "properties": {}}},
        }))
        .unwrap();
        assert_eq!(value["properties"], json!({}));
    }

    #[test]
    fn convert_returns_none_for_empty_object_schemas() {
        assert_eq!(convert(&json!({"type": "object", "properties": {}})), None);
        assert_eq!(convert(&json!(42)), None);
    }

    #[test]
    fn convert_keeps_additional_properties_objects() {
        // `additionalProperties` is truthy, so this is not an empty-object
        // schema; the key itself is not in the projection allowlist.
        let value = convert(&json!({
            "type": "object",
            "additionalProperties": {"type": "string"},
        }))
        .unwrap();
        assert_eq!(value, json!({"type": "object"}));
    }

    #[test]
    fn convert_strips_properties_on_non_object_types() {
        let value = convert(&json!({
            "type": "string",
            "properties": {"a": {"type": "string"}},
            "required": ["a"],
        }))
        .unwrap();
        assert_eq!(value.get("properties"), None);
        assert_eq!(value.get("required"), None);
        assert_eq!(value["type"], json!("string"));
    }
}
