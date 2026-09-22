//! Messages, content parts, tool definitions, and the protocol-neutral
//! request model (from `schema/messages.ts`).

#![allow(clippy::arc_with_non_send_sync)]
use std::sync::Arc;

use serde::{Deserialize, Serialize};

use alforria_schema::llm::{ProviderMetadata, ToolContent};

use crate::route::client::RouteHandle;

use super::ids::{JsonMap, MessageRole, ModelId, ProviderId};
use super::options::{
    CacheHint, CachePolicy, GenerationOptions, HttpOptions, Model, ModelCompatibility,
    ModelDefaults, ProviderOptions, SystemPart,
};

/// Base64-encoded media payload (`Schema.Union([Schema.String, Schema.Uint8Array])`
/// in TS). The `Uint8Array` construction input is not ported: it needs a base64
/// dependency this crate does not carry.
pub type MediaData = String;

/// `LLM.ToolResultValue["type"]` literals.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ToolResultType {
    Json,
    Text,
    Error,
    Content,
}

/// `LLM.ToolResult` — a tagged tool result value.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "kebab-case")]
pub enum ToolResultValue {
    Json { value: serde_json::Value },
    Text { value: serde_json::Value },
    Error { value: serde_json::Value },
    Content { value: Vec<ToolContent> },
}

/// TS `ToolResultValue.is` — structural check for a tagged result value.
pub fn is_tool_result_value(value: &serde_json::Value) -> bool {
    let serde_json::Value::Object(record) = value else {
        return false;
    };
    let tagged = matches!(
        record.get("type").and_then(serde_json::Value::as_str),
        Some("json" | "text" | "error" | "content")
    );
    tagged && record.contains_key("value")
}

impl ToolResultValue {
    /// TS `ToolResultValue.make` — coerce a raw value into a result value.
    ///
    /// Values that already structurally look like a tool result are passed
    /// through unchanged (the `result_type` parameter is ignored, exactly as
    /// in TS); everything else is wrapped in the requested type.
    pub fn make(value: serde_json::Value, result_type: ToolResultType) -> ToolResultValue {
        if is_tool_result_value(&value) {
            if let Ok(parsed) = serde_json::from_value::<ToolResultValue>(value.clone()) {
                return parsed;
            }
        }
        match result_type {
            ToolResultType::Json => ToolResultValue::Json { value },
            ToolResultType::Text => ToolResultValue::Text { value },
            ToolResultType::Error => ToolResultValue::Error { value },
            ToolResultType::Content => ToolResultValue::Content {
                value: serde_json::from_value(value).unwrap_or_default(),
            },
        }
    }
}

/// `LLM.ToolOutput` — structured + content view of a tool result.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ToolOutput {
    pub structured: serde_json::Value,
    pub content: Vec<ToolContent>,
}

impl ToolOutput {
    /// TS `ToolOutput.make` — `content` defaults to `[]` in TS.
    pub fn make(structured: serde_json::Value, content: Vec<ToolContent>) -> ToolOutput {
        ToolOutput {
            structured,
            content,
        }
    }

    /// TS `ToolOutput.fromResultValue` — `error` results have no output.
    pub fn from_result_value(result: &ToolResultValue) -> Option<ToolOutput> {
        match result {
            ToolResultValue::Json { value } => Some(Self::make(value.clone(), Vec::new())),
            ToolResultValue::Text { value } => Some(Self::make(
                serde_json::json!({}),
                vec![ToolContent::Text {
                    text: tool_result_text(value),
                }],
            )),
            ToolResultValue::Content { value } => {
                Some(Self::make(serde_json::json!({}), value.clone()))
            }
            ToolResultValue::Error { .. } => None,
        }
    }

    /// TS `ToolOutput.toResultValue`.
    pub fn to_result_value(&self) -> ToolResultValue {
        if self.content.is_empty() {
            return ToolResultValue::Json {
                value: self.structured.clone(),
            };
        }
        if self.content.len() == 1 {
            match &self.content[0] {
                ToolContent::Text { text } => {
                    return ToolResultValue::Text {
                        value: serde_json::Value::String(text.clone()),
                    }
                }
                ToolContent::File { .. } => {
                    return ToolResultValue::Content {
                        value: self.content.clone(),
                    }
                }
            }
        }
        ToolResultValue::Content {
            value: self.content.clone(),
        }
    }
}

/// `toolResultText` — string values pass through, everything else is
/// stringified as JSON.
fn tool_result_text(value: &serde_json::Value) -> String {
    if let serde_json::Value::String(text) = value {
        return text.clone();
    }
    serde_json::to_string(value).unwrap_or_else(|_| value.to_string())
}

/// `LLM.Content` — protocol-neutral content part union.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "kebab-case")]
#[serde(rename_all_fields = "camelCase")]
pub enum ContentPart {
    Text {
        text: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        cache: Option<CacheHint>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        metadata: Option<JsonMap>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        provider_metadata: Option<ProviderMetadata>,
    },
    Media {
        media_type: String,
        data: MediaData,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        filename: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        metadata: Option<JsonMap>,
    },
    ToolCall {
        id: String,
        name: String,
        input: serde_json::Value,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        provider_executed: Option<bool>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        metadata: Option<JsonMap>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        provider_metadata: Option<ProviderMetadata>,
    },
    ToolResult {
        id: String,
        name: String,
        result: ToolResultValue,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        provider_executed: Option<bool>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        cache: Option<CacheHint>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        metadata: Option<JsonMap>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        provider_metadata: Option<ProviderMetadata>,
    },
    Reasoning {
        text: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        encrypted: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        metadata: Option<JsonMap>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        provider_metadata: Option<ProviderMetadata>,
    },
}

/// TS `ToolResultPart.make` input.
#[derive(Debug, Clone, Default)]
pub struct ToolResultInput {
    pub id: String,
    pub name: String,
    pub result: serde_json::Value,
    pub result_type: Option<ToolResultType>,
    pub provider_executed: Option<bool>,
    pub cache: Option<CacheHint>,
    pub metadata: Option<JsonMap>,
    pub provider_metadata: Option<ProviderMetadata>,
}

impl From<ToolResultInput> for ContentPart {
    fn from(input: ToolResultInput) -> Self {
        ContentPart::ToolResult {
            id: input.id,
            name: input.name,
            result: ToolResultValue::make(
                input.result,
                input.result_type.unwrap_or(ToolResultType::Json),
            ),
            provider_executed: input.provider_executed,
            cache: input.cache,
            metadata: input.metadata,
            provider_metadata: input.provider_metadata,
        }
    }
}

impl ContentPart {
    pub fn text(text: impl Into<String>) -> ContentPart {
        ContentPart::Text {
            text: text.into(),
            cache: None,
            metadata: None,
            provider_metadata: None,
        }
    }

    pub fn media(media_type: impl Into<String>, data: impl Into<MediaData>) -> ContentPart {
        ContentPart::Media {
            media_type: media_type.into(),
            data: data.into(),
            filename: None,
            metadata: None,
        }
    }

    pub fn tool_call(
        id: impl Into<String>,
        name: impl Into<String>,
        input: serde_json::Value,
    ) -> ContentPart {
        ContentPart::ToolCall {
            id: id.into(),
            name: name.into(),
            input,
            provider_executed: None,
            metadata: None,
            provider_metadata: None,
        }
    }

    pub fn tool_result(input: ToolResultInput) -> ContentPart {
        input.into()
    }

    pub fn reasoning(text: impl Into<String>) -> ContentPart {
        ContentPart::Reasoning {
            text: text.into(),
            encrypted: None,
            metadata: None,
            provider_metadata: None,
        }
    }
}

/// `LLM.Message.ContentInput` — ergonomic content constructor input.
pub enum ContentInput {
    Text(String),
    Part(ContentPart),
    Parts(Vec<ContentPart>),
}

impl From<&str> for ContentInput {
    fn from(value: &str) -> Self {
        ContentInput::Text(value.to_string())
    }
}

impl From<String> for ContentInput {
    fn from(value: String) -> Self {
        ContentInput::Text(value)
    }
}

impl From<ContentPart> for ContentInput {
    fn from(value: ContentPart) -> Self {
        ContentInput::Part(value)
    }
}

impl From<Vec<ContentPart>> for ContentInput {
    fn from(value: Vec<ContentPart>) -> Self {
        ContentInput::Parts(value)
    }
}

/// `LLM.Message` — one conversation turn.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Message {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub id: Option<String>,
    pub role: MessageRole,
    pub content: Vec<ContentPart>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub metadata: Option<JsonMap>,
    /// Provider-native passthrough (e.g. openaiCompatible `reasoning_content`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub native: Option<JsonMap>,
}

impl Message {
    /// TS `Message.text` — a text content part.
    pub fn text(value: impl Into<String>) -> ContentPart {
        ContentPart::Text {
            text: value.into(),
            cache: None,
            metadata: None,
            provider_metadata: None,
        }
    }

    /// TS `Message.content` — normalize content input into parts.
    pub fn content(input: impl Into<ContentInput>) -> Vec<ContentPart> {
        match input.into() {
            ContentInput::Text(text) => vec![Self::text(text)],
            ContentInput::Part(part) => vec![part],
            ContentInput::Parts(parts) => parts,
        }
    }

    /// TS `Message.make` — normalize input into the canonical `Message`.
    pub fn make(role: MessageRole, content: impl Into<ContentInput>) -> Message {
        Message {
            id: None,
            role,
            content: Self::content(content),
            metadata: None,
            native: None,
        }
    }

    pub fn user(content: impl Into<ContentInput>) -> Message {
        Self::make(MessageRole::User, content)
    }

    pub fn assistant(content: impl Into<ContentInput>) -> Message {
        Self::make(MessageRole::Assistant, content)
    }

    /// Add an operator-authored instruction at this chronological point in the
    /// conversation. This is distinct from the initial `LLMRequest.system`
    /// prompt. Keep raw retrieved, tool, and web content out of privileged
    /// system updates; pass that untrusted content through ordinary
    /// user/tool channels.
    pub fn system(content: impl Into<ContentInput>) -> Message {
        Self::make(MessageRole::System, content)
    }

    pub fn tool(result: impl Into<ContentPart>) -> Message {
        Self::make(MessageRole::Tool, ContentInput::Part(result.into()))
    }
}

/// `LLM.ToolDefinition`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ToolDefinition {
    pub name: String,
    pub description: String,
    pub input_schema: JsonMap,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub output_schema: Option<JsonMap>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cache: Option<CacheHint>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub metadata: Option<JsonMap>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub native: Option<JsonMap>,
}

/// `LLM.ToolChoice["type"]` literals.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ToolChoiceType {
    Auto,
    None,
    Required,
    Tool,
}

/// `LLM.ToolChoice`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ToolChoice {
    pub r#type: ToolChoiceType,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
}

impl ToolChoice {
    /// TS `ToolChoice.named` — select a specific named tool.
    pub fn named(name: impl Into<String>) -> ToolChoice {
        ToolChoice {
            r#type: ToolChoiceType::Tool,
            name: Some(name.into()),
        }
    }

    /// TS `ToolChoice.make` — normalize ergonomic tool-choice inputs
    /// (`"auto"`/`"none"`/`"required"` modes, other strings and tool
    /// definitions select a named tool).
    pub fn make(input: impl Into<ToolChoice>) -> ToolChoice {
        input.into()
    }
}

impl From<&str> for ToolChoice {
    fn from(value: &str) -> Self {
        match value {
            "auto" => Self {
                r#type: ToolChoiceType::Auto,
                name: None,
            },
            "none" => Self {
                r#type: ToolChoiceType::None,
                name: None,
            },
            "required" => Self {
                r#type: ToolChoiceType::Required,
                name: None,
            },
            _ => Self::named(value),
        }
    }
}

impl From<String> for ToolChoice {
    fn from(value: String) -> Self {
        Self::from(value.as_str())
    }
}

impl From<ToolDefinition> for ToolChoice {
    fn from(value: ToolDefinition) -> Self {
        Self::named(value.name)
    }
}

/// `LLM.ResponseFormat` — tagged union on `type`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "kebab-case")]
pub enum ResponseFormat {
    Text,
    Json { schema: JsonMap },
    Tool { tool: ToolDefinition },
}

/// Serializable view of `Model` (TS `LLMRequest.model`): `{ id, provider,
/// defaults?, compatibility? }` on the wire, plus the in-process
/// `Arc<RouteHandle>` hidden behind `#[serde(skip)]`.
#[derive(Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ModelRef {
    pub id: ModelId,
    pub provider: ProviderId,
    #[serde(skip, default = "empty_route")]
    pub route: Arc<RouteHandle>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub defaults: Option<ModelDefaults>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub compatibility: Option<ModelCompatibility>,
}

/// `#[serde(skip)]` uses this when deserializing: requests built in memory
/// always carry the real handle; deserialized requests get an empty one.
fn empty_route() -> Arc<RouteHandle> {
    Arc::new(RouteHandle::empty())
}

impl PartialEq for ModelRef {
    fn eq(&self, other: &Self) -> bool {
        // The route handle is process-local; equality is on the model identity.
        self.id == other.id
            && self.provider == other.provider
            && self.defaults == other.defaults
            && self.compatibility == other.compatibility
    }
}

impl std::fmt::Debug for ModelRef {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ModelRef")
            .field("id", &self.id)
            .field("provider", &self.provider)
            .field("defaults", &self.defaults)
            .field("compatibility", &self.compatibility)
            .finish_non_exhaustive()
    }
}

impl ModelRef {
    pub fn new(
        id: impl Into<ModelId>,
        provider: impl Into<ProviderId>,
        route: Arc<RouteHandle>,
    ) -> ModelRef {
        ModelRef {
            id: id.into(),
            provider: provider.into(),
            route,
            defaults: None,
            compatibility: None,
        }
    }

    pub fn from_model(model: &Model) -> ModelRef {
        ModelRef {
            id: model.id.clone(),
            provider: model.provider.clone(),
            route: model.route.clone(),
            defaults: model.defaults.clone(),
            compatibility: model.compatibility.clone(),
        }
    }
}

/// `LLM.Request` — the protocol-neutral request model.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LlmRequest {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub id: Option<String>,
    pub model: ModelRef,
    pub system: Vec<SystemPart>,
    pub messages: Vec<Message>,
    pub tools: Vec<ToolDefinition>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool_choice: Option<ToolChoice>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub generation: Option<GenerationOptions>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub provider_options: Option<ProviderOptions>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub http: Option<HttpOptions>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub response_format: Option<ResponseFormat>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cache: Option<CachePolicy>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub metadata: Option<JsonMap>,
}

impl LlmRequest {
    /// Empty request for a model — `system`, `messages`, and `tools` default
    /// to empty, every optional section to `None`.
    pub fn new(model: ModelRef) -> LlmRequest {
        LlmRequest {
            id: None,
            model,
            system: Vec::new(),
            messages: Vec::new(),
            tools: Vec::new(),
            tool_choice: None,
            generation: None,
            provider_options: None,
            http: None,
            response_format: None,
            cache: None,
            metadata: None,
        }
    }

    /// TS `LLMRequest.update` — apply a partial patch; `model` is always
    /// carried over unless the patch replaces it.
    pub fn update(&self, patch: LlmRequestPatch) -> LlmRequest {
        if patch == LlmRequestPatch::default() {
            return self.clone();
        }
        LlmRequest {
            id: patch.id.or_else(|| self.id.clone()),
            model: patch.model.unwrap_or_else(|| self.model.clone()),
            system: patch.system.unwrap_or_else(|| self.system.clone()),
            messages: patch.messages.unwrap_or_else(|| self.messages.clone()),
            tools: patch.tools.unwrap_or_else(|| self.tools.clone()),
            tool_choice: patch.tool_choice.or_else(|| self.tool_choice.clone()),
            generation: patch.generation.or_else(|| self.generation.clone()),
            provider_options: patch
                .provider_options
                .or_else(|| self.provider_options.clone()),
            http: patch.http.or_else(|| self.http.clone()),
            response_format: patch
                .response_format
                .or_else(|| self.response_format.clone()),
            cache: patch.cache.or_else(|| self.cache.clone()),
            metadata: patch.metadata.or_else(|| self.metadata.clone()),
        }
    }
}

/// TS `Partial<LLMRequest.Input>` — `None` fields leave the request unchanged.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct LlmRequestPatch {
    pub id: Option<String>,
    pub model: Option<ModelRef>,
    pub system: Option<Vec<SystemPart>>,
    pub messages: Option<Vec<Message>>,
    pub tools: Option<Vec<ToolDefinition>>,
    pub tool_choice: Option<ToolChoice>,
    pub generation: Option<GenerationOptions>,
    pub provider_options: Option<ProviderOptions>,
    pub http: Option<HttpOptions>,
    pub response_format: Option<ResponseFormat>,
    pub cache: Option<CachePolicy>,
    pub metadata: Option<JsonMap>,
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;
    use crate::route::client::RouteHandle;

    fn model_ref() -> ModelRef {
        ModelRef::new(
            "claude-sonnet-4-5",
            "anthropic",
            Arc::new(RouteHandle::empty()),
        )
    }

    #[test]
    fn content_part_uses_kebab_case_wire_tags() {
        let call = ContentPart::tool_call("toolu_1", "get_weather", json!({"city": "Paris"}));
        let value = serde_json::to_value(&call).unwrap();
        assert_eq!(value["type"], "tool-call");
        assert_eq!(value["id"], "toolu_1");
        assert_eq!(value["name"], "get_weather");
        assert_eq!(value["input"], json!({"city": "Paris"}));
        assert!(value.get("providerExecuted").is_none());

        let result = ContentPart::tool_result(ToolResultInput {
            id: "toolu_1".to_string(),
            name: "get_weather".to_string(),
            result: json!({"temp": "18"}),
            ..ToolResultInput::default()
        });
        let value = serde_json::to_value(&result).unwrap();
        assert_eq!(value["type"], "tool-result");
        assert_eq!(value["result"]["type"], "json");

        let back: ContentPart =
            serde_json::from_value(serde_json::to_value(&result).unwrap()).unwrap();
        assert_eq!(back, result);
    }

    #[test]
    fn tool_result_value_content_round_trips() {
        let value = ToolResultValue::Content {
            value: vec![ToolContent::Text {
                text: "18".to_string(),
            }],
        };
        let wire = serde_json::to_value(&value).unwrap();
        assert_eq!(
            wire,
            json!({"type": "content", "value": [{"type": "text", "text": "18"}]}),
        );
        let back: ToolResultValue = serde_json::from_value(wire).unwrap();
        assert_eq!(back, value);
    }

    #[test]
    fn llm_request_serialization_omits_optional_keys() {
        let request = LlmRequest::new(model_ref());
        let value = serde_json::to_value(&request).unwrap();
        assert_eq!(
            value,
            json!({
                "model": {"id": "claude-sonnet-4-5", "provider": "anthropic"},
                "system": [],
                "messages": [],
                "tools": [],
            }),
        );
        for key in [
            "id",
            "toolChoice",
            "generation",
            "providerOptions",
            "http",
            "responseFormat",
            "cache",
            "metadata",
        ] {
            assert!(value.get(key).is_none(), "unexpected key: {key}");
        }
    }

    #[test]
    fn tool_result_value_make_passthrough() {
        assert_eq!(
            ToolResultValue::make(
                json!({"type": "json", "value": {"a": 1}}),
                ToolResultType::Text,
            ),
            ToolResultValue::Json {
                value: json!({"a": 1})
            },
        );
        assert_eq!(
            ToolResultValue::make(json!({"a": 1}), ToolResultType::Json),
            ToolResultValue::Json {
                value: json!({"a": 1})
            },
        );
        // Non-array content coerces to an empty content list.
        assert_eq!(
            ToolResultValue::make(json!({"a": 1}), ToolResultType::Content),
            ToolResultValue::Content { value: Vec::new() },
        );
    }

    #[test]
    fn tool_output_from_and_to_result_value() {
        assert_eq!(
            ToolOutput::from_result_value(&ToolResultValue::Json {
                value: json!({"ok": true}),
            }),
            Some(ToolOutput::make(json!({"ok": true}), Vec::new())),
        );
        assert_eq!(
            ToolOutput::from_result_value(&ToolResultValue::Text {
                value: json!("18 degrees"),
            }),
            Some(ToolOutput::make(
                json!({}),
                vec![ToolContent::Text {
                    text: "18 degrees".to_string(),
                }],
            )),
        );
        // Non-string text values are stringified.
        assert_eq!(
            ToolOutput::from_result_value(&ToolResultValue::Text {
                value: json!({"temp": 18}),
            }),
            Some(ToolOutput::make(
                json!({}),
                vec![ToolContent::Text {
                    text: "{\"temp\":18}".to_string(),
                }],
            )),
        );
        assert_eq!(
            ToolOutput::from_result_value(&ToolResultValue::Error {
                value: json!("boom"),
            }),
            None,
        );

        assert_eq!(
            ToolOutput::make(json!({"ok": true}), Vec::new()).to_result_value(),
            ToolResultValue::Json {
                value: json!({"ok": true}),
            },
        );
        assert_eq!(
            ToolOutput::make(
                json!({}),
                vec![ToolContent::Text {
                    text: "18".to_string(),
                }],
            )
            .to_result_value(),
            ToolResultValue::Text { value: json!("18") },
        );
        let file = ToolContent::File {
            uri: "file:///tmp/a".to_string(),
            mime: "text/plain".to_string(),
            name: None,
        };
        assert_eq!(
            ToolOutput::make(json!({}), vec![file.clone()]).to_result_value(),
            ToolResultValue::Content { value: vec![file] },
        );
    }

    #[test]
    fn tool_choice_make_accepts_ergonomic_inputs() {
        assert_eq!(
            ToolChoice::make("auto"),
            ToolChoice {
                r#type: ToolChoiceType::Auto,
                name: None,
            },
        );
        assert_eq!(
            ToolChoice::make("required"),
            ToolChoice {
                r#type: ToolChoiceType::Required,
                name: None,
            },
        );
        assert_eq!(
            ToolChoice::make("get_weather"),
            ToolChoice::named("get_weather"),
        );
        let tool = ToolDefinition {
            name: "get_weather".to_string(),
            description: "Get the weather".to_string(),
            input_schema: JsonMap::new(),
            output_schema: None,
            cache: None,
            metadata: None,
            native: None,
        };
        assert_eq!(ToolChoice::make(tool), ToolChoice::named("get_weather"));
        let choice = ToolChoice {
            r#type: ToolChoiceType::None,
            name: None,
        };
        assert_eq!(ToolChoice::make(choice.clone()), choice);
    }

    #[test]
    fn message_constructors_normalize_content() {
        let message = Message::user("Hello!");
        assert_eq!(message.role, MessageRole::User);
        assert_eq!(message.content, vec![Message::text("Hello!")]);
        let roundtrip: Message =
            serde_json::from_value(serde_json::to_value(&message).unwrap()).unwrap();
        assert_eq!(roundtrip, message);

        let tool_message = Message::tool(ToolResultInput {
            id: "toolu_1".to_string(),
            name: "get_weather".to_string(),
            result: json!({"temp": "18"}),
            ..ToolResultInput::default()
        });
        assert_eq!(tool_message.role, MessageRole::Tool);
        match &tool_message.content[0] {
            ContentPart::ToolResult { result, .. } => assert_eq!(
                result,
                &ToolResultValue::Json {
                    value: json!({"temp": "18"}),
                },
            ),
            other => panic!("expected tool result part, got {other:?}"),
        }
    }

    #[test]
    fn response_format_wire_tags() {
        let json_format = ResponseFormat::Json {
            schema: JsonMap::new(),
        };
        assert_eq!(
            serde_json::to_value(&json_format).unwrap(),
            json!({"type": "json", "schema": {}}),
        );
        let text: ResponseFormat = serde_json::from_value(json!({"type": "text"})).unwrap();
        assert_eq!(text, ResponseFormat::Text);
    }

    #[test]
    fn llm_request_update_applies_patch_and_preserves_model() {
        let request = LlmRequest::new(model_ref());
        assert_eq!(request.update(LlmRequestPatch::default()), request);

        let patched = request.update(LlmRequestPatch {
            messages: Some(vec![Message::assistant("Hi there")]),
            ..LlmRequestPatch::default()
        });
        assert_eq!(patched.model, request.model);
        assert_eq!(patched.messages, vec![Message::assistant("Hi there")]);
        assert_eq!(patched.tools, Vec::new());
    }

    #[test]
    fn model_ref_is_a_serializable_view_of_model() {
        let model = Model {
            id: "claude-sonnet-4-5".to_string(),
            provider: "anthropic".to_string(),
            route: Arc::new(RouteHandle::empty()),
            defaults: None,
            compatibility: None,
        };
        let model_ref = ModelRef::from_model(&model);
        assert_eq!(model_ref.id, "claude-sonnet-4-5");
        let value = serde_json::to_value(&model_ref).unwrap();
        assert_eq!(
            value,
            json!({"id": "claude-sonnet-4-5", "provider": "anthropic"}),
        );
        let back: ModelRef = serde_json::from_value(value).unwrap();
        assert_eq!(back, model_ref);
    }
}
