//! JSON-schema → protocol tool-schema projections
//! (from `protocols/utils/tool-schema.ts`).

use serde_json::Value;

use crate::protocols::utils::gemini_tool_schema;
use crate::schema::ids::JsonMap;
use crate::schema::options::ModelToolSchemaCompatibility;

/// `removeNullSchemas` — recursively strip `anyOf` null variants, merging a
/// single surviving variant into the parent fields.
fn remove_null_schemas(value: &Value) -> Value {
    if let Some(array) = value.as_array() {
        return Value::Array(array.iter().map(remove_null_schemas).collect());
    }
    let Some(record) = value.as_object() else {
        return value.clone();
    };
    let mut fields = JsonMap::new();
    for (key, field) in record {
        if key == "anyOf" {
            continue;
        }
        fields.insert(key.clone(), remove_null_schemas(field));
    }
    let Some(any_of) = record.get("anyOf").filter(|any_of| any_of.is_array()) else {
        return Value::Object(fields);
    };
    let variants: Vec<Value> = any_of
        .as_array()
        .unwrap()
        .iter()
        .filter(|variant| variant.get("type").and_then(Value::as_str) != Some("null"))
        .map(remove_null_schemas)
        .collect();
    if variants.len() == 1 && variants[0].is_object() {
        // `{ ...fields, ...variants[0] }` — variant fields override.
        let merged = variants[0].as_object().unwrap();
        let mut result = fields;
        for (key, value) in merged {
            result.insert(key.clone(), value.clone());
        }
        return Value::Object(result);
    }
    fields.insert("anyOf".into(), Value::Array(variants));
    Value::Object(fields)
}

/// `tupleItemsSchema` — project a tuple `items`/`prefixItems` array.
fn tuple_items_schema(items: &[Value]) -> Value {
    let projected = items.iter().map(moonshot_node).collect::<Vec<_>>();
    match projected.len() {
        0 => Value::Object(JsonMap::new()),
        1 => projected[0].clone(),
        _ => Value::Object({
            let mut map = JsonMap::new();
            map.insert("anyOf".into(), Value::Array(projected));
            map
        }),
    }
}

fn moonshot_node(schema: &Value) -> Value {
    if let Some(array) = schema.as_array() {
        return Value::Array(array.iter().map(moonshot_node).collect());
    }
    let Some(record) = schema.as_object() else {
        return schema.clone();
    };
    if let Some(r#ref) = record.get("$ref").and_then(Value::as_str) {
        return Value::Object({
            let mut map = JsonMap::new();
            map.insert("$ref".into(), Value::String(r#ref.to_string()));
            map
        });
    }
    let mut result = JsonMap::new();
    for (key, value) in record {
        match key.as_str() {
            "items" if value.is_array() => {
                result.insert(key.clone(), tuple_items_schema(value.as_array().unwrap()));
            }
            "prefixItems" => {
                if !record.contains_key("items") {
                    let items = value
                        .as_array()
                        .map(|items| items.as_slice())
                        .unwrap_or(&[]);
                    result.insert("items".into(), tuple_items_schema(items));
                }
            }
            "unevaluatedItems" => {}
            _ => {
                result.insert(key.clone(), moonshot_node(value));
            }
        }
    }
    Value::Object(result)
}

/// Projections available on the protocol-neutral model. Each entry matches a
/// provider dialect that rejects pieces of standard JSON Schema.
pub struct ToolSchemaProjection;

impl ToolSchemaProjection {
    /// Moonshot's tool dialect: no `prefixItems`/`unevaluatedItems`, tuple
    /// `items` collapsed into `items`/`anyOf`, and `$ref` passed through alone.
    pub fn moonshot(schema: &JsonMap) -> JsonMap {
        let projected = moonshot_node(&Value::Object(schema.clone()));
        match projected {
            Value::Object(projected) => projected,
            _ => JsonMap::new(),
        }
    }

    /// OpenAI's tool dialect: merge a top-level `anyOf` union into a single
    /// object schema with `type: "object"` and `additionalProperties: false`.
    pub fn open_ai(schema: &JsonMap) -> JsonMap {
        let variants: Vec<Value> = schema
            .get("anyOf")
            .and_then(Value::as_array)
            .map(|any_of| {
                any_of
                    .iter()
                    .filter(|variant| variant.is_object())
                    .cloned()
                    .collect()
            })
            .unwrap_or_default();
        let flattened = if variants.is_empty() {
            let mut flattened = schema.clone();
            flattened.insert("type".into(), Value::String("object".into()));
            Value::Object(flattened)
        } else {
            // Earlier variants win when merging `properties`.
            let mut properties = JsonMap::new();
            for variant in &variants {
                if let Some(variant_properties) =
                    variant.get("properties").and_then(Value::as_object)
                {
                    for (key, value) in variant_properties {
                        properties
                            .entry(key.clone())
                            .or_insert_with(|| value.clone());
                    }
                }
            }
            let mut flattened = schema.clone();
            flattened.remove("anyOf");
            flattened.insert("type".into(), Value::String("object".into()));
            flattened.insert("properties".into(), Value::Object(properties));
            flattened.insert("additionalProperties".into(), Value::Bool(false));
            Value::Object(flattened)
        };
        let normalized = remove_null_schemas(&flattened);
        match normalized {
            Value::Object(projected) => projected,
            _ => {
                let mut map = JsonMap::new();
                map.insert("type".into(), Value::String("object".into()));
                map
            }
        }
    }

    /// Gemini's tool dialect — see `gemini_tool_schema`.
    pub fn gemini(schema: &JsonMap) -> JsonMap {
        gemini_tool_schema::convert(&Value::Object(schema.clone()))
            .and_then(|projected| projected.as_object().cloned())
            .unwrap_or_default()
    }

    /// Dispatch on the model's tool-schema compatibility, or pass through when
    /// the model has none.
    pub fn model_compatibility(
        schema: &JsonMap,
        compatibility: Option<ModelToolSchemaCompatibility>,
    ) -> JsonMap {
        match compatibility {
            None => schema.clone(),
            Some(ModelToolSchemaCompatibility::Gemini) => Self::gemini(schema),
            Some(ModelToolSchemaCompatibility::Moonshot) => Self::moonshot(schema),
        }
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    fn map(value: serde_json::Value) -> JsonMap {
        value.as_object().cloned().unwrap()
    }

    #[test]
    fn open_ai_without_any_of_forces_object_type() {
        let schema = map(json!({"properties": {"a": {"type": "string"}}}));
        let projected = ToolSchemaProjection::open_ai(&schema);
        assert_eq!(
            Value::Object(projected),
            json!({"type": "object", "properties": {"a": {"type": "string"}}}),
        );
    }

    #[test]
    fn open_ai_merges_any_of_variants() {
        let schema = map(json!({
            "description": "union",
            "anyOf": [
                {"type": "object", "properties": {"a": {"type": "string"}}},
                {"type": "object", "properties": {"b": {"type": "number"}}},
            ],
        }));
        let projected = ToolSchemaProjection::open_ai(&schema);
        assert_eq!(
            Value::Object(projected),
            json!({
                "description": "union",
                "type": "object",
                "properties": {
                    "a": {"type": "string"},
                    "b": {"type": "number"},
                },
                "additionalProperties": false,
            }),
        );
    }

    #[test]
    fn open_ai_strips_null_variants_recursively() {
        let schema = map(json!({
            "anyOf": [
                {
                    "type": "object",
                    "properties": {"a": {"anyOf": [{"type": "string"}, {"type": "null"}]}},
                },
                {"type": "object", "properties": {"b": {"type": "string"}}},
            ],
        }));
        let projected = ToolSchemaProjection::open_ai(&schema);
        assert_eq!(projected.get("additionalProperties"), Some(&json!(false)));
        // The inner null variant collapses; `a` becomes a plain string schema.
        assert_eq!(
            projected.get("properties"),
            Some(&json!({"a": {"type": "string"}, "b": {"type": "string"}})),
        );
    }

    #[test]
    fn open_ai_drops_a_single_null_variant_union() {
        let schema = map(json!({
            "anyOf": [
                {"type": "object", "properties": {"a": {"anyOf": [{"type": "null"}]}}},
            ],
        }));
        let projected = ToolSchemaProjection::open_ai(&schema);
        // The nested single-null union collapses to `{"anyOf": []}`.
        assert_eq!(
            projected.get("properties"),
            Some(&json!({"a": {"anyOf": []}})),
        );
    }

    #[test]
    fn moonshot_converts_prefix_items_to_items() {
        let schema = map(json!({
            "type": "object",
            "properties": {
                "tuple": {
                    "type": "array",
                    "prefixItems": [{"type": "string"}, {"type": "number"}],
                },
            },
        }));
        let projected = ToolSchemaProjection::moonshot(&schema);
        assert_eq!(
            projected["properties"]["tuple"]["items"],
            json!({"anyOf": [{"type": "string"}, {"type": "number"}]}),
        );
    }

    #[test]
    fn moonshot_prefers_existing_items_over_prefix_items() {
        let schema = map(json!({
            "type": "array",
            "items": [{"type": "string"}],
            "prefixItems": [{"type": "number"}],
            "unevaluatedItems": false,
        }));
        let projected = ToolSchemaProjection::moonshot(&schema);
        // A single-element tuple collapses to the element itself.
        assert_eq!(projected["items"], json!({"type": "string"}));
        assert_eq!(projected.get("prefixItems"), None);
        assert_eq!(projected.get("unevaluatedItems"), None);
    }

    #[test]
    fn moonshot_passes_refs_through_alone() {
        let schema = map(json!({
            "type": "array",
            "items": [{"$ref": "#/definitions/Foo", "description": "ignored"},
                       {"$ref": "#/definitions/Bar"}],
        }));
        let projected = ToolSchemaProjection::moonshot(&schema);
        assert_eq!(
            projected["items"],
            json!({"anyOf": [{"$ref": "#/definitions/Foo"}, {"$ref": "#/definitions/Bar"}]}),
        );
    }

    #[test]
    fn model_compatibility_dispatches_by_dialect() {
        let schema = map(json!({"type": "integer", "enum": [1, 2]}));
        let projected = ToolSchemaProjection::model_compatibility(
            &schema,
            Some(ModelToolSchemaCompatibility::Gemini),
        );
        assert_eq!(projected.get("type"), Some(&json!("string")));

        let schema = map(json!({"anyOf": [{"type": "null"}]}));
        let projected = ToolSchemaProjection::model_compatibility(&schema, None);
        assert_eq!(Value::Object(projected), Value::Object(schema));

        let schema = map(json!({"type": "array", "prefixItems": [{"type": "string"}]}));
        let projected = ToolSchemaProjection::model_compatibility(
            &schema,
            Some(ModelToolSchemaCompatibility::Moonshot),
        );
        assert_eq!(
            Value::Object(projected),
            json!({"type": "array", "items": {"type": "string"}}),
        );
    }
}
