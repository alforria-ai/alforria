//! Config-option select options (`acp/config-option.ts`).

use serde_json::{json, Value};

/// `DEFAULT_VARIANT_VALUE` (config-option.ts:3).
pub const DEFAULT_VARIANT_VALUE: &str = "default";

/// `buildModelSelectOption` (config-option.ts:31-50).
pub fn build_model_select_option(
    providers: &[Value],
    current_model: &Value,
    current_variant: Option<&str>,
) -> Value {
    json!({
        "id": "model",
        "name": "Model",
        "category": "model",
        "type": "select",
        "currentValue": format_current_model_id(
            current_model,
            current_variant,
            Some(&variants_for_model(providers, current_model)),
            false,
        ),
        "options": build_model_select_options(providers, false),
    })
}

/// `buildEffortSelectOption` (config-option.ts:52-73).
pub fn build_effort_select_option(
    variants: &[String],
    current_variant: Option<&str>,
) -> Option<Value> {
    if variants.is_empty() {
        return None;
    }
    // `[...new Set([...input.variants, DEFAULT_VARIANT_VALUE])]` —
    // order-preserving dedup with "default" appended when missing.
    let mut all: Vec<String> = variants.to_vec();
    if !all.iter().any(|variant| variant == DEFAULT_VARIANT_VALUE) {
        all.push(DEFAULT_VARIANT_VALUE.to_string());
    } else {
        let mut seen: Vec<String> = Vec::new();
        for variant in all.iter() {
            if !seen.contains(variant) {
                seen.push(variant.clone());
            }
        }
        all = seen;
    }
    let current = if current_variant == Some(DEFAULT_VARIANT_VALUE) {
        DEFAULT_VARIANT_VALUE.to_string()
    } else {
        select_variant(current_variant, variants)
    };
    Some(json!({
        "id": "effort",
        "name": "Effort",
        "description": "Available effort levels for this model",
        "category": "thought_level",
        "type": "select",
        "currentValue": current,
        "options": all
            .iter()
            .map(|variant| json!({ "value": variant, "name": format_variant_name(variant) }))
            .collect::<Vec<_>>(),
    }))
}

/// `buildModeSelectOption` (config-option.ts:75-91).
pub fn build_mode_select_option(modes: &[Value], current_mode_id: &str) -> Value {
    json!({
        "id": "mode",
        "name": "Session Mode",
        "category": "mode",
        "type": "select",
        "currentValue": current_mode_id,
        "options": modes
            .iter()
            .map(|mode| {
                let mut option = json!({ "value": mode["id"], "name": mode["name"] });
                if let Some(description) = mode.get("description") {
                    option["description"] = description.clone();
                }
                option
            })
            .collect::<Vec<_>>(),
    })
}

/// `buildConfigOptions` (config-option.ts:93-116).
pub fn build_config_options(
    providers: &[Value],
    current_model: &Value,
    current_variant: Option<&str>,
    modes: Option<&[Value]>,
    current_mode_id: Option<&str>,
) -> Vec<Value> {
    let variants = variants_for_model(providers, current_model);
    let mut options = vec![build_model_select_option(
        providers,
        current_model,
        current_variant,
    )];
    if let Some(effort) = build_effort_select_option(&variants, current_variant) {
        options.push(effort);
    }
    if let (Some(modes), Some(current_mode_id)) = (modes, current_mode_id) {
        options.push(build_mode_select_option(modes, current_mode_id));
    }
    options
}

/// `parseModelSelection` (config-option.ts:118-149).
pub fn parse_model_selection(model_id: &str, providers: &[Value]) -> Value {
    let matching = providers.iter().find(|provider| {
        model_id.starts_with(&format!("{}/", provider["id"].as_str().unwrap_or_default()))
    });

    if let Some(provider) = matching {
        let provider_id = provider["id"].as_str().unwrap_or_default();
        let inner = &model_id[provider_id.len() + 1..];
        if model_of(provider, inner).is_some() {
            return json!({ "model": { "providerID": provider_id, "modelID": inner } });
        }
        if let Some(separator) = inner.rfind('/') {
            let base_model_id = &inner[..separator];
            let variant = &inner[separator + 1..];
            if model_of(provider, base_model_id)
                .and_then(|model| model.get("variants"))
                .and_then(|variants| variants.get(variant))
                .is_some()
            {
                return json!({
                    "model": { "providerID": provider_id, "modelID": base_model_id },
                    "variant": variant,
                });
            }
        }
        return json!({ "model": { "providerID": provider_id, "modelID": inner } });
    }

    match model_id.find('/') {
        None => json!({ "model": { "providerID": model_id, "modelID": "" } }),
        Some(separator) => json!({
            "model": {
                "providerID": &model_id[..separator],
                "modelID": &model_id[separator + 1..],
            }
        }),
    }
}

/// `formatCurrentModelId` (config-option.ts:151-160).
pub fn format_current_model_id(
    model: &Value,
    variant: Option<&str>,
    variants: Option<&[String]>,
    include_variant: bool,
) -> String {
    let base = format!(
        "{}/{}",
        model["providerID"].as_str().unwrap_or_default(),
        model["modelID"].as_str().unwrap_or_default()
    );
    let Some(variants) = variants else {
        return base;
    };
    if !include_variant || variants.is_empty() {
        return base;
    }
    format!("{base}/{}", select_variant(variant, variants))
}

/// `formatVariantName` (config-option.ts:162-167).
pub fn format_variant_name(variant: &str) -> String {
    variant
        .split(['_', '-'])
        .map(|part| {
            let mut chars = part.chars();
            match chars.next() {
                Some(first) => first.to_uppercase().collect::<String>() + chars.as_str(),
                None => String::new(),
            }
        })
        .collect::<Vec<_>>()
        .join(" ")
}

fn build_model_select_options(providers: &[Value], _include_variants: bool) -> Vec<Value> {
    let mut options: Vec<Value> = Vec::new();
    for provider in providers {
        let Some(models) = provider.get("models").and_then(Value::as_object) else {
            continue;
        };
        // `a.name.localeCompare(b.name)` (config-option.ts:175) —
        // case-sensitive, no lowercasing.
        let mut sorted: Vec<(&String, &Value)> = models.iter().collect();
        sorted.sort_by(|a, b| {
            a.1.get("name")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .cmp(b.1.get("name").and_then(Value::as_str).unwrap_or_default())
        });
        for (model_id, model) in sorted {
            options.push(json!({
                "value": format!("{}/{}", provider["id"].as_str().unwrap_or_default(), model_id),
                "name": format!("{}/{}", provider["name"].as_str().unwrap_or_default(), model["name"].as_str().unwrap_or_default()),
            }));
        }
    }
    options
}

/// `variantsForModel` (config-option.ts:196-200).
pub fn variants_for_model(providers: &[Value], model: &Value) -> Vec<String> {
    providers
        .iter()
        .find(|provider| provider["id"] == model["providerID"])
        .and_then(|provider| {
            provider
                .get("models")
                .and_then(|models| models.get(model["modelID"].as_str().unwrap_or_default()))
        })
        .and_then(|model| model.get("variants"))
        .and_then(Value::as_object)
        .map(|variants| variants.keys().cloned().collect())
        .unwrap_or_default()
}

/// `selectVariant` (config-option.ts:202-206).
pub fn select_variant(variant: Option<&str>, variants: &[String]) -> String {
    if let Some(variant) = variant {
        if variants.iter().any(|item| item == variant) {
            return variant.to_string();
        }
    }
    if let Some(first) = variants
        .iter()
        .find(|item| item.as_str() == DEFAULT_VARIANT_VALUE)
    {
        return first.clone();
    }
    variants.first().cloned().unwrap_or_default()
}

fn model_of<'a>(provider: &'a Value, model_id: &str) -> Option<&'a Value> {
    provider
        .get("models")
        .and_then(|models| models.get(model_id))
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    const PROVIDERS: &str = r#"
    [
        { "id": "anthropic", "name": "Anthropic", "models": {
            "claude-3": { "id": "claude-3", "name": "Claude 3", "variants": { "high": {}, "default": {} } }
        }},
        { "id": "mock", "name": "Mock", "models": {
            "mock-model": { "id": "mock-model", "name": "Mock Model" }
        }}
    ]
    "#;

    #[test]
    fn parse_plain_provider_model() {
        let providers: Vec<Value> = serde_json::from_str(PROVIDERS).unwrap();
        let selection = parse_model_selection("mock/mock-model", &providers);
        assert_eq!(
            selection,
            json!({
                "model": { "providerID": "mock", "modelID": "mock-model" }
            })
        );
    }

    #[test]
    fn parse_variant_selection() {
        let providers: Vec<Value> = serde_json::from_str(PROVIDERS).unwrap();
        let selection = parse_model_selection("anthropic/claude-3/high", &providers);
        assert_eq!(
            selection,
            json!({
                "model": { "providerID": "anthropic", "modelID": "claude-3" },
                "variant": "high",
            })
        );
    }

    #[test]
    fn parse_unknown_provider_splits_on_first_slash() {
        let providers: Vec<Value> = serde_json::from_str(PROVIDERS).unwrap();
        let selection = parse_model_selection("other/some/model", &providers);
        assert_eq!(
            selection,
            json!({
                "model": { "providerID": "other", "modelID": "some/model" }
            })
        );
    }

    #[test]
    fn variant_names_are_formatted() {
        assert_eq!(format_variant_name("high_effort"), "High Effort");
        assert_eq!(format_variant_name("default"), "Default");
    }

    #[test]
    fn effort_option_includes_default_variant() {
        let options = build_effort_select_option(&["high".to_string()], Some("default")).unwrap();
        assert_eq!(options["currentValue"], json!("default"));
        let values: Vec<&str> = options["options"]
            .as_array()
            .unwrap()
            .iter()
            .map(|option| option["value"].as_str().unwrap())
            .collect();
        assert_eq!(values, vec!["high", "default"]);
    }

    #[test]
    fn config_options_shape() {
        let providers: Vec<Value> = serde_json::from_str(PROVIDERS).unwrap();
        let modes = vec![json!({ "id": "build", "name": "build" })];
        let options = build_config_options(
            &providers,
            &json!({ "providerID": "mock", "modelID": "mock-model" }),
            None,
            Some(&modes),
            Some("build"),
        );
        assert_eq!(options.len(), 2);
        assert_eq!(options[0]["id"], json!("model"));
        assert_eq!(options[1]["id"], json!("mode"));
    }
}
