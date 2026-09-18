//! `provider/transform.ts` — the reasoning-variant machinery
//! (`ProviderTransform.variants` / `reasoningVariants`, transform.ts:777-1909).
use crate::catalog;
use serde_json::{Map, Value};
use std::collections::BTreeMap;

/// The `Provider.Model` slice the variant machinery reads
/// (transform.ts touches `model.id`, `model.providerID`, `model.api`,
/// `model.family`, `model.limit.output`, `model.release_date` and
/// `model.capabilities.reasoning`).
#[derive(Debug, Clone, PartialEq)]
pub struct RuntimeModel {
    pub id: String,
    pub provider_id: String,
    pub api_id: String,
    pub api_npm: String,
    pub api_url: String,
    pub release_date: String,
    pub family: Option<String>,
    pub limit_output: f64,
    pub reasoning: bool,
}

/// `Record<string, Record<string, any>>` — variant id → request payload.
pub type Variants = BTreeMap<String, Value>;

/// JS emits integral numbers without a fraction part.
fn num(value: f64) -> Value {
    if value.fract() == 0.0 && value.abs() < 9.007_199_254_740_992e15 {
        Value::from(value as i64)
    } else {
        Value::from(value)
    }
}

fn object(entries: Vec<(&str, Value)>) -> Value {
    Value::Object(Map::from_iter(
        entries.into_iter().map(|(k, v)| (k.to_string(), v)),
    ))
}

const WIDELY_SUPPORTED_EFFORTS: [&str; 3] = ["low", "medium", "high"];
const OPENAI_EFFORTS: [&str; 6] = ["none", "minimal", "low", "medium", "high", "xhigh"];
const OPENAI_GPT5_1_EFFORTS: [&str; 4] = ["none", "low", "medium", "high"];
const OPENAI_GPT5_2_PLUS_EFFORTS: [&str; 5] = ["none", "low", "medium", "high", "xhigh"];
const OPENAI_GPT5_PRO_EFFORTS: [&str; 1] = ["high"];
const OPENAI_GPT5_PRO_2_PLUS_EFFORTS: [&str; 3] = ["medium", "high", "xhigh"];
const OPENAI_GPT5_CHAT_EFFORTS: [&str; 1] = ["medium"];
const OPENAI_GPT5_CODEX_XHIGH_EFFORTS: [&str; 4] = ["low", "medium", "high", "xhigh"];
const OPENAI_GPT5_CODEX_3_PLUS_EFFORTS: [&str; 5] = ["none", "low", "medium", "high", "xhigh"];

const OPENAI_NONE_EFFORT_RELEASE_DATE: &str = "2025-11-13";
const OPENAI_XHIGH_EFFORT_RELEASE_DATE: &str = "2025-12-04";

const GPT5_FAMILY_RE: &str = r"(?:^|/)gpt-5(?:[.-]|$)";
const GPT5_VERSION_RE: &str = r"(?:^|/)gpt-5[.-](\d+)(?:[.-]|$)";
const GPT5_PRO_RE: &str = r"(?:^|/)gpt-5[.-]?pro(?:[.-]|$)";
const GPT5_VERSIONED_PRO_RE: &str = r"(?:^|/)gpt-5[.-]\d+[.-]pro(?:[.-]|$)";

fn regex(pattern: &str) -> regex::Regex {
    regex::Regex::new(pattern).expect("valid regex")
}

/// `isKimiFamily` (transform.ts:29-39).
fn is_kimi_family(model: &RuntimeModel) -> bool {
    if [model.provider_id.as_str(), model.api_id.as_str()]
        .iter()
        .any(|id| {
            let value = id.to_lowercase();
            value.contains("kimi") || value.contains("moonshot")
        })
    {
        return true;
    }
    let url = model.api_url.to_lowercase();
    [
        "api.kimi.com",
        "api.moonshot.ai",
        "api.moonshot.cn",
        "api.moonshotai.cn",
    ]
    .iter()
    .any(|host| url.contains(host))
}

fn gpt5_version(api_id: &str) -> Option<u32> {
    regex(GPT5_VERSION_RE)
        .captures(api_id)
        .and_then(|c| c[1].parse().ok())
}

fn versioned_gpt5_reasoning_efforts(api_id: &str) -> Option<Vec<&'static str>> {
    if regex(GPT5_VERSIONED_PRO_RE).is_match(api_id) {
        return Some(OPENAI_GPT5_PRO_2_PLUS_EFFORTS.to_vec());
    }
    let version = gpt5_version(api_id)?;
    if version == 1 {
        Some(OPENAI_GPT5_1_EFFORTS.to_vec())
    } else {
        Some(OPENAI_GPT5_2_PLUS_EFFORTS.to_vec())
    }
}

fn gpt5_codex_reasoning_efforts(api_id: &str) -> Option<Vec<&'static str>> {
    if !regex(GPT5_FAMILY_RE).is_match(api_id) || !api_id.contains("codex") {
        return None;
    }
    let version = gpt5_version(api_id);
    if version.is_some_and(|version| version >= 3) {
        return Some(OPENAI_GPT5_CODEX_3_PLUS_EFFORTS.to_vec());
    }
    if api_id.contains("codex-max") || version.is_some_and(|v| v >= 2) {
        return Some(OPENAI_GPT5_CODEX_XHIGH_EFFORTS.to_vec());
    }
    Some(WIDELY_SUPPORTED_EFFORTS.to_vec())
}

fn gpt5_chat_reasoning_efforts(api_id: &str) -> Option<Vec<&'static str>> {
    if !regex(GPT5_FAMILY_RE).is_match(api_id) || !api_id.contains("-chat") {
        return None;
    }
    if gpt5_version(api_id).is_none() {
        Some(Vec::new())
    } else {
        Some(OPENAI_GPT5_CHAT_EFFORTS.to_vec())
    }
}

fn openai_reasoning_efforts(api_id: &str, release_date: &str) -> Vec<&'static str> {
    if api_id.contains("deep-research") {
        return vec!["medium"];
    }
    if let Some(chat) = gpt5_chat_reasoning_efforts(api_id) {
        return chat;
    }
    if regex(GPT5_PRO_RE).is_match(api_id) {
        return OPENAI_GPT5_PRO_EFFORTS.to_vec();
    }
    if let Some(codex) = gpt5_codex_reasoning_efforts(api_id) {
        return codex;
    }
    if let Some(versioned) = versioned_gpt5_reasoning_efforts(api_id) {
        return versioned;
    }
    let mut efforts = WIDELY_SUPPORTED_EFFORTS.to_vec();
    if regex(GPT5_FAMILY_RE).is_match(api_id) {
        efforts.insert(0, "minimal");
    }
    if release_date >= OPENAI_NONE_EFFORT_RELEASE_DATE {
        efforts.insert(0, "none");
    }
    if release_date >= OPENAI_XHIGH_EFFORT_RELEASE_DATE {
        efforts.push("xhigh");
    }
    efforts
}

fn openai_compatible_reasoning_efforts(id: &str) -> Vec<&'static str> {
    if let Some(chat) = gpt5_chat_reasoning_efforts(id) {
        return chat;
    }
    if regex(GPT5_PRO_RE).is_match(id) {
        return OPENAI_GPT5_PRO_EFFORTS.to_vec();
    }
    gpt5_codex_reasoning_efforts(id)
        .or_else(|| versioned_gpt5_reasoning_efforts(id))
        .unwrap_or_else(|| OPENAI_EFFORTS.to_vec())
}

/// `anthropicUsesModernAdaptiveThinking` (transform.ts:654-663).
fn anthropic_uses_modern_adaptive_thinking(api_id: &str) -> bool {
    if !api_id.to_lowercase().contains("claude-") {
        return false;
    }
    static VERSION: &str = r"(?i)claude-(?:[a-z]+-)?(\d+)(?:[.-](\d{1,2}))?(?:[.@-]|$)";
    match regex(VERSION).captures(api_id) {
        None => true,
        Some(version) => {
            let major = version[1].parse::<u32>().unwrap_or(0);
            let minor = version
                .get(2)
                .and_then(|m| m.as_str().parse::<u32>().ok())
                .unwrap_or(0);
            major > 4 || (major == 4 && minor >= 7)
        }
    }
}

fn anthropic_opus_45(api_id: &str) -> bool {
    ["opus-4-5", "opus-4.5"].iter().any(|v| api_id.contains(v))
}

/// `anthropicAdaptiveEfforts` (transform.ts:669-681).
fn anthropic_adaptive_efforts(api_id: &str) -> Option<Vec<&'static str>> {
    if anthropic_uses_modern_adaptive_thinking(api_id) {
        return Some(vec!["low", "medium", "high", "xhigh", "max"]);
    }
    if [
        "opus-4-6",
        "opus-4.6",
        "4-6-opus",
        "4.6-opus",
        "sonnet-4-6",
        "sonnet-4.6",
        "4-6-sonnet",
        "4.6-sonnet",
    ]
    .iter()
    .any(|v| api_id.contains(v))
    {
        return Some(vec!["low", "medium", "high", "max"]);
    }
    None
}

fn anthropic_omits_thinking(api_id: &str) -> bool {
    anthropic_uses_modern_adaptive_thinking(api_id)
}

/// `googleThinkingLevelEfforts` (transform.ts:738-745).
fn google_thinking_level_efforts(api_id: &str) -> Vec<&'static str> {
    if !api_id.contains("gemini-3") {
        return vec!["low", "high"];
    }
    if api_id.contains("flash-image") {
        return vec!["minimal", "high"];
    }
    if api_id.contains("pro-image") {
        return vec!["high"];
    }
    if api_id.contains("flash") {
        return vec!["minimal", "low", "medium", "high"];
    }
    vec!["low", "medium", "high"]
}

/// `googleThinkingBudgetMax` (transform.ts:747-751).
fn google_thinking_budget_max(api_id: &str) -> f64 {
    if api_id.contains("2.5") && api_id.contains("pro") && !api_id.contains("flash") {
        32_768.0
    } else {
        24_576.0
    }
}

fn thinking_config(entries: Vec<(&str, Value)>) -> Value {
    object(entries)
}

/// `googleThinkingVariants` (transform.ts:759-775).
fn google_thinking_variants(model: &RuntimeModel) -> Variants {
    let id = model.api_id.to_lowercase();
    if id.contains("2.5") {
        return BTreeMap::from([
            (
                "high".to_string(),
                object(vec![(
                    "thinkingConfig",
                    thinking_config(vec![
                        ("includeThoughts", Value::Bool(true)),
                        ("thinkingBudget", num(16_000.0)),
                    ]),
                )]),
            ),
            (
                "max".to_string(),
                object(vec![(
                    "thinkingConfig",
                    thinking_config(vec![
                        ("includeThoughts", Value::Bool(true)),
                        ("thinkingBudget", num(google_thinking_budget_max(&id))),
                    ]),
                )]),
            ),
        ]);
    }
    google_thinking_level_efforts(&id)
        .into_iter()
        .map(|effort| {
            (
                effort.to_string(),
                thinking_config(vec![
                    ("includeThoughts", Value::Bool(true)),
                    ("thinkingLevel", Value::from(effort)),
                ]),
            )
        })
        .collect()
}

/// `wrapInSapModelParams` (transform.ts:755-757).
fn wrap_in_sap_model_params(variants: Variants) -> Variants {
    variants
        .into_iter()
        .map(|(id, value)| (id, object(vec![("modelParams", value)])))
        .collect()
}

/// `ProviderTransform.variants` (transform.ts:777-1205).
pub fn variants(model: &RuntimeModel) -> Variants {
    if !model.reasoning {
        return Variants::new();
    }

    let id = model.id.to_lowercase();
    let glm52 = ["glm-5.2", "glm-5-2", "glm-5p2"]
        .iter()
        .any(|name| id.contains(name) || model.api_id.to_lowercase().contains(name));
    if model.api_id.to_lowercase().contains("minimax-m3")
        && ["@ai-sdk/anthropic", "@ai-sdk/openai-compatible"].contains(&model.api_npm.as_str())
    {
        if ["nvidia", "lilac"].contains(&model.provider_id.as_str()) {
            return BTreeMap::from([
                (
                    "none".to_string(),
                    object(vec![(
                        "chat_template_kwargs",
                        object(vec![("thinking_mode", Value::from("disabled"))]),
                    )]),
                ),
                (
                    "thinking".to_string(),
                    object(vec![(
                        "chat_template_kwargs",
                        object(vec![("thinking_mode", Value::from("enabled"))]),
                    )]),
                ),
            ]);
        }
        return BTreeMap::from([
            (
                "none".to_string(),
                object(vec![(
                    "thinking",
                    object(vec![("type", Value::from("disabled"))]),
                )]),
            ),
            (
                "thinking".to_string(),
                object(vec![(
                    "thinking",
                    object(vec![("type", Value::from("adaptive"))]),
                )]),
            ),
        ]);
    }
    let adaptive_thinking_omitted = anthropic_omits_thinking(&model.api_id);
    let adaptive_efforts = anthropic_adaptive_efforts(&model.api_id);
    if glm52 && model.api_npm == "@openrouter/ai-sdk-provider" {
        return BTreeMap::from([
            (
                "high".to_string(),
                object(vec![(
                    "reasoning",
                    object(vec![("effort", Value::from("high"))]),
                )]),
            ),
            (
                "xhigh".to_string(),
                object(vec![(
                    "reasoning",
                    object(vec![("effort", Value::from("xhigh"))]),
                )]),
            ),
        ]);
    }
    if glm52 && model.api_npm == "@ai-sdk/openai-compatible" {
        return BTreeMap::from([
            (
                "high".to_string(),
                object(vec![("reasoningEffort", Value::from("high"))]),
            ),
            (
                "max".to_string(),
                object(vec![("reasoningEffort", Value::from("max"))]),
            ),
        ]);
    }
    if glm52 && model.api_npm == "@ai-sdk/anthropic" {
        return BTreeMap::from([
            (
                "high".to_string(),
                object(vec![("effort", Value::from("high"))]),
            ),
            (
                "max".to_string(),
                object(vec![("effort", Value::from("max"))]),
            ),
        ]);
    }
    if is_kimi_family(model)
        && ["@ai-sdk/anthropic", "@ai-sdk/google-vertex/anthropic"]
            .contains(&model.api_npm.as_str())
    {
        return ["low", "medium", "high", "xhigh", "max"]
            .into_iter()
            .map(|effort| {
                (
                    effort.to_string(),
                    object(vec![
                        (
                            "thinking",
                            object(vec![
                                ("type", Value::from("adaptive")),
                                ("display", Value::from("summarized")),
                            ]),
                        ),
                        ("effort", Value::from(effort)),
                    ]),
                )
            })
            .collect();
    }
    if id.contains("deepseek-chat")
        || id.contains("deepseek-reasoner")
        || id.contains("deepseek-r1")
        || id.contains("deepseek-v3")
        || id.contains("minimax")
        || (id.contains("glm") && !glm52)
        || id.contains("kimi")
        || id.contains("k2p")
        || id.contains("qwen")
        || id.contains("big-pickle")
    {
        return Variants::new();
    }

    if id.contains("grok") && id.contains("grok-3-mini") {
        if model.api_npm == "@openrouter/ai-sdk-provider" {
            return BTreeMap::from([
                (
                    "low".to_string(),
                    object(vec![(
                        "reasoning",
                        object(vec![("effort", Value::from("low"))]),
                    )]),
                ),
                (
                    "high".to_string(),
                    object(vec![(
                        "reasoning",
                        object(vec![("effort", Value::from("high"))]),
                    )]),
                ),
            ]);
        }
        return BTreeMap::from([
            (
                "low".to_string(),
                object(vec![("reasoningEffort", Value::from("low"))]),
            ),
            (
                "high".to_string(),
                object(vec![("reasoningEffort", Value::from("high"))]),
            ),
        ]);
    }

    match model.api_npm.as_str() {
        "@openrouter/ai-sdk-provider" => {
            let efforts = if model.api_id.starts_with("openai/") || id.contains("gpt") {
                openai_compatible_reasoning_efforts(&model.api_id)
            } else {
                WIDELY_SUPPORTED_EFFORTS.to_vec()
            };
            efforts
                .into_iter()
                .map(|effort| {
                    (
                        effort.to_string(),
                        object(vec![(
                            "reasoning",
                            object(vec![("effort", Value::from(effort))]),
                        )]),
                    )
                })
                .collect()
        }
        "ai-gateway-provider" => {
            let efforts = if model.api_id.starts_with("openai/") {
                openai_reasoning_efforts(&model.api_id, &model.release_date)
            } else {
                WIDELY_SUPPORTED_EFFORTS.to_vec()
            };
            efforts
                .into_iter()
                .map(|effort| {
                    (
                        effort.to_string(),
                        object(vec![("reasoningEffort", Value::from(effort))]),
                    )
                })
                .collect()
        }
        "@ai-sdk/gateway" => {
            if model.api_id.contains("anthropic") {
                if let Some(efforts) = adaptive_efforts {
                    return efforts
                        .into_iter()
                        .map(|effort| {
                            let mut thinking = vec![("type".to_string(), Value::from("adaptive"))];
                            if adaptive_thinking_omitted {
                                thinking.push(("display".to_string(), Value::from("summarized")));
                            }
                            (
                                effort.to_string(),
                                object(vec![
                                    ("thinking", Value::Object(Map::from_iter(thinking))),
                                    ("effort", Value::from(effort)),
                                ]),
                            )
                        })
                        .collect();
                }
                return BTreeMap::from([
                    (
                        "high".to_string(),
                        object(vec![(
                            "thinking",
                            object(vec![
                                ("type", Value::from("enabled")),
                                ("budgetTokens", num(16_000.0)),
                            ]),
                        )]),
                    ),
                    (
                        "max".to_string(),
                        object(vec![(
                            "thinking",
                            object(vec![
                                ("type", Value::from("enabled")),
                                ("budgetTokens", num(31_999.0)),
                            ]),
                        )]),
                    ),
                ]);
            }
            if model.api_id.contains("google") {
                if model.api_id.contains("2.5") {
                    return BTreeMap::from([
                        (
                            "high".to_string(),
                            object(vec![(
                                "thinkingConfig",
                                object(vec![
                                    ("includeThoughts", Value::Bool(true)),
                                    ("thinkingBudget", num(16_000.0)),
                                ]),
                            )]),
                        ),
                        (
                            "max".to_string(),
                            object(vec![(
                                "thinkingConfig",
                                object(vec![
                                    ("includeThoughts", Value::Bool(true)),
                                    (
                                        "thinkingBudget",
                                        num(google_thinking_budget_max(
                                            &model.api_id.to_lowercase(),
                                        )),
                                    ),
                                ]),
                            )]),
                        ),
                    ]);
                }
                return ["low", "high"]
                    .into_iter()
                    .map(|effort| {
                        (
                            effort.to_string(),
                            object(vec![
                                ("includeThoughts", Value::Bool(true)),
                                ("thinkingLevel", Value::from(effort)),
                            ]),
                        )
                    })
                    .collect();
            }
            openai_compatible_reasoning_efforts(&model.api_id)
                .into_iter()
                .map(|effort| {
                    (
                        effort.to_string(),
                        object(vec![("reasoningEffort", Value::from(effort))]),
                    )
                })
                .collect()
        }
        "@ai-sdk/github-copilot" => {
            if model.id.contains("gemini") {
                return Variants::new();
            }
            if model.id.contains("claude") {
                return WIDELY_SUPPORTED_EFFORTS
                    .into_iter()
                    .map(|effort| {
                        (
                            effort.to_string(),
                            object(vec![("reasoningEffort", Value::from(effort))]),
                        )
                    })
                    .collect();
            }
            let efforts = {
                if id.contains("5.1-codex-max") || id.contains("5.2") || id.contains("5.3") {
                    Vec::from(WIDELY_SUPPORTED_EFFORTS)
                        .into_iter()
                        .chain(std::iter::once("xhigh"))
                        .collect::<Vec<_>>()
                } else {
                    let mut arr = WIDELY_SUPPORTED_EFFORTS.to_vec();
                    if id.contains("gpt-5")
                        && model.release_date.as_str() >= OPENAI_XHIGH_EFFORT_RELEASE_DATE
                    {
                        arr.push("xhigh");
                    }
                    arr
                }
            };
            efforts
                .into_iter()
                .map(|effort| {
                    (
                        effort.to_string(),
                        object(vec![
                            ("reasoningEffort", Value::from(effort)),
                            ("reasoningSummary", Value::from("auto")),
                            (
                                "include",
                                Value::Array(vec![Value::from("reasoning.encrypted_content")]),
                            ),
                        ]),
                    )
                })
                .collect()
        }
        "@ai-sdk/cerebras"
        | "@ai-sdk/togetherai"
        | "@ai-sdk/xai"
        | "@ai-sdk/deepinfra"
        | "venice-ai-sdk-provider"
        | "@ai-sdk/openai-compatible" => {
            if model.api_id.to_lowercase().contains("north-mini-code") {
                return ["none", "high"]
                    .into_iter()
                    .map(|effort| {
                        (
                            effort.to_string(),
                            object(vec![("reasoningEffort", Value::from(effort))]),
                        )
                    })
                    .collect();
            }
            let mut efforts = WIDELY_SUPPORTED_EFFORTS.to_vec();
            if model.api_id.to_lowercase().contains("deepseek-v4") {
                efforts.push("max");
            }
            efforts
                .into_iter()
                .map(|effort| {
                    (
                        effort.to_string(),
                        object(vec![("reasoningEffort", Value::from(effort))]),
                    )
                })
                .collect()
        }
        "@ai-sdk/azure" => {
            if id == "o1-mini" {
                return Variants::new();
            }
            openai_reasoning_efforts(&id, &model.release_date)
                .into_iter()
                .map(|effort| {
                    (
                        effort.to_string(),
                        object(vec![
                            ("reasoningEffort", Value::from(effort)),
                            ("reasoningSummary", Value::from("auto")),
                            (
                                "include",
                                Value::Array(vec![Value::from("reasoning.encrypted_content")]),
                            ),
                        ]),
                    )
                })
                .collect()
        }
        "@ai-sdk/amazon-bedrock/mantle" | "@ai-sdk/openai" => {
            let efforts = if model.provider_id == "meta" {
                OPENAI_EFFORTS.to_vec()
            } else {
                openai_reasoning_efforts(&model.api_id, &model.release_date)
            };
            efforts
                .into_iter()
                .map(|effort| {
                    (
                        effort.to_string(),
                        object(vec![
                            ("reasoningEffort", Value::from(effort)),
                            ("reasoningSummary", Value::from("auto")),
                            (
                                "include",
                                Value::Array(vec![Value::from("reasoning.encrypted_content")]),
                            ),
                        ]),
                    )
                })
                .collect()
        }
        "@ai-sdk/anthropic" | "@ai-sdk/google-vertex/anthropic" => {
            if let Some(efforts) = adaptive_efforts {
                let mut efforts = efforts;
                if model.provider_id == "github-copilot" {
                    if model.api_id.contains("opus-4.7") {
                        efforts = vec!["medium"];
                    }
                    efforts.retain(|v| *v != "max" && *v != "xhigh");
                }
                return efforts
                    .into_iter()
                    .map(|effort| {
                        let mut thinking = vec![("type".to_string(), Value::from("adaptive"))];
                        if adaptive_thinking_omitted {
                            thinking.push(("display".to_string(), Value::from("summarized")));
                        }
                        (
                            effort.to_string(),
                            object(vec![
                                ("thinking", Value::Object(Map::from_iter(thinking))),
                                ("effort", Value::from(effort)),
                            ]),
                        )
                    })
                    .collect();
            }

            if anthropic_opus_45(&model.api_id) {
                return WIDELY_SUPPORTED_EFFORTS
                    .into_iter()
                    .map(|effort| (effort.to_string(), anthropic_opus_45_effort(model, effort)))
                    .collect();
            }

            BTreeMap::from([
                (
                    "high".to_string(),
                    object(vec![(
                        "thinking",
                        object(vec![
                            ("type", Value::from("enabled")),
                            (
                                "budgetTokens",
                                num(f64::min(16_000.0, (model.limit_output / 2.0 - 1.0).floor())),
                            ),
                        ]),
                    )]),
                ),
                (
                    "max".to_string(),
                    object(vec![(
                        "thinking",
                        object(vec![
                            ("type", Value::from("enabled")),
                            (
                                "budgetTokens",
                                num(f64::min(31_999.0, model.limit_output - 1.0)),
                            ),
                        ]),
                    )]),
                ),
            ])
        }
        "@ai-sdk/amazon-bedrock" => {
            if let Some(efforts) = adaptive_efforts {
                return efforts
                    .into_iter()
                    .map(|effort| {
                        let mut config = vec![
                            ("type".to_string(), Value::from("adaptive")),
                            ("maxReasoningEffort".to_string(), Value::from(effort)),
                        ];
                        if adaptive_thinking_omitted {
                            config.push(("display".to_string(), Value::from("summarized")));
                        }
                        (
                            effort.to_string(),
                            object(vec![(
                                "reasoningConfig",
                                Value::Object(Map::from_iter(config)),
                            )]),
                        )
                    })
                    .collect();
            }
            if model.api_id.contains("anthropic") {
                return BTreeMap::from([
                    (
                        "high".to_string(),
                        object(vec![(
                            "reasoningConfig",
                            object(vec![
                                ("type", Value::from("enabled")),
                                ("budgetTokens", num(16_000.0)),
                            ]),
                        )]),
                    ),
                    (
                        "max".to_string(),
                        object(vec![(
                            "reasoningConfig",
                            object(vec![
                                ("type", Value::from("enabled")),
                                ("budgetTokens", num(31_999.0)),
                            ]),
                        )]),
                    ),
                ]);
            }
            WIDELY_SUPPORTED_EFFORTS
                .into_iter()
                .map(|effort| {
                    (
                        effort.to_string(),
                        object(vec![(
                            "reasoningConfig",
                            object(vec![
                                ("type", Value::from("enabled")),
                                ("maxReasoningEffort", Value::from(effort)),
                            ]),
                        )]),
                    )
                })
                .collect()
        }
        "@ai-sdk/google-vertex" | "@ai-sdk/google" => google_thinking_variants(model),
        "@ai-sdk/mistral" => {
            const MISTRAL_REASONING_IDS: [&str; 4] = [
                "mistral-small-2603",
                "mistral-small-latest",
                "mistral-medium-3.5",
                "mistral-medium-2604",
            ];
            let mistral_id = model.api_id.to_lowercase();
            if !MISTRAL_REASONING_IDS
                .iter()
                .any(|id| mistral_id.contains(id))
            {
                return Variants::new();
            }
            BTreeMap::from([(
                "high".to_string(),
                object(vec![("reasoningEffort", Value::from("high"))]),
            )])
        }
        "@ai-sdk/cohere" | "@ai-sdk/perplexity" => Variants::new(),
        "@ai-sdk/groq" => {
            let efforts = Vec::from(WIDELY_SUPPORTED_EFFORTS);
            std::iter::once("none")
                .chain(efforts)
                .map(|effort| {
                    (
                        effort.to_string(),
                        object(vec![("reasoningEffort", Value::from(effort))]),
                    )
                })
                .collect()
        }
        "@jerome-benoit/sap-ai-provider-v2" => {
            if id.contains("anthropic") {
                if let Some(efforts) = adaptive_efforts {
                    return wrap_in_sap_model_params(
                        efforts
                            .into_iter()
                            .map(|effort| {
                                let mut thinking =
                                    vec![("type".to_string(), Value::from("adaptive"))];
                                if adaptive_thinking_omitted {
                                    thinking
                                        .push(("display".to_string(), Value::from("summarized")));
                                }
                                (
                                    effort.to_string(),
                                    object(vec![
                                        ("thinking", Value::Object(Map::from_iter(thinking))),
                                        (
                                            "output_config",
                                            object(vec![("effort", Value::from(effort))]),
                                        ),
                                    ]),
                                )
                            })
                            .collect(),
                    );
                }
                return wrap_in_sap_model_params(BTreeMap::from([
                    (
                        "high".to_string(),
                        object(vec![(
                            "thinking",
                            object(vec![
                                ("type", Value::from("enabled")),
                                ("budget_tokens", num(16_000.0)),
                            ]),
                        )]),
                    ),
                    (
                        "max".to_string(),
                        object(vec![(
                            "thinking",
                            object(vec![
                                ("type", Value::from("enabled")),
                                ("budget_tokens", num(31_999.0)),
                            ]),
                        )]),
                    ),
                ]));
            }
            if id.contains("gemini") && id.contains("2.5") {
                return wrap_in_sap_model_params(google_thinking_variants(model));
            }
            if id.contains("gpt") || regex(r"\bo[1-9]").is_match(&id) {
                let efforts = openai_reasoning_efforts(&id, &model.release_date);
                return wrap_in_sap_model_params(
                    efforts
                        .into_iter()
                        .map(|effort| {
                            (
                                effort.to_string(),
                                object(vec![("reasoning_effort", Value::from(effort))]),
                            )
                        })
                        .collect(),
                );
            }
            wrap_in_sap_model_params(
                ["low", "medium", "high"]
                    .into_iter()
                    .map(|effort| {
                        (
                            effort.to_string(),
                            object(vec![("reasoning_effort", Value::from(effort))]),
                        )
                    })
                    .collect(),
            )
        }
        _ => Variants::new(),
    }
}

// ---------------------------------------------------------------------------
// reasoning_options-driven variants (transform.ts:1704-1909)
// ---------------------------------------------------------------------------

const OUTPUT_TOKEN_MAX: f64 = 32_000.0;

fn non_empty(variants: Variants) -> Option<Variants> {
    if variants.is_empty() {
        None
    } else {
        Some(variants)
    }
}

/// `effortVariants` (transform.ts:1722-1734).
fn effort_variants(model: &RuntimeModel, values: &[Option<String>]) -> Variants {
    values
        .iter()
        .filter_map(|value| {
            let id = match value {
                None => "none",
                Some(value) => value.as_str(),
            };
            reasoning_effort(model, id).map(|settings| (id.to_string(), settings))
        })
        .collect()
}

/// `budgetVariants` (transform.ts:1736-1749).
fn budget_variants(model: &RuntimeModel, min: Option<f64>, max: Option<f64>) -> Variants {
    let maximum = (max.unwrap_or(OUTPUT_TOKEN_MAX - 1.0))
        .min(model.limit_output - 1.0)
        .min(OUTPUT_TOKEN_MAX - 1.0);
    if maximum <= 0.0 {
        return Variants::new();
    }
    let high = min
        .unwrap_or(0.0)
        .max(((maximum + 1.0) / 2.0).floor())
        .min(maximum);
    [("high", high), ("max", maximum)]
        .into_iter()
        .filter_map(|(id, budget)| {
            reasoning_budget(model, budget).map(|settings| (id.to_string(), settings))
        })
        .collect()
}

/// `reasoningToggle` (transform.ts:1755-1767).
fn reasoning_toggle(model: &RuntimeModel) -> Variants {
    match model.api_npm.as_str() {
        "@ai-sdk/alibaba" => BTreeMap::from([
            (
                "none".to_string(),
                object(vec![("enableThinking", Value::Bool(false))]),
            ),
            (
                "high".to_string(),
                object(vec![("enableThinking", Value::Bool(true))]),
            ),
        ]),
        "@ai-sdk/cohere" => BTreeMap::from([
            (
                "none".to_string(),
                object(vec![(
                    "thinking",
                    object(vec![("type", Value::from("disabled"))]),
                )]),
            ),
            (
                "high".to_string(),
                object(vec![(
                    "thinking",
                    object(vec![("type", Value::from("enabled"))]),
                )]),
            ),
        ]),
        _ => Variants::new(),
    }
}

/// `anthropicOpus45Effort` (transform.ts:1853-1861).
fn anthropic_opus_45_effort(_model: &RuntimeModel, effort: &str) -> Value {
    object(vec![
        (
            "thinking",
            object(vec![
                ("type", Value::from("enabled")),
                (
                    "budgetTokens",
                    num(f64::min(
                        16_000.0,
                        (_model.limit_output / 2.0 - 1.0).floor(),
                    )),
                ),
            ]),
        ),
        ("effort", Value::from(effort)),
    ])
}

/// `anthropicEffort` (transform.ts:1839-1851).
fn anthropic_effort(model: &RuntimeModel, effort: &str) -> Option<Value> {
    if anthropic_opus_45(&model.api_id) {
        return Some(anthropic_opus_45_effort(model, effort));
    }
    if is_kimi_family(model) {
        return Some(object(vec![
            (
                "thinking",
                object(vec![
                    ("type", Value::from("adaptive")),
                    ("display", Value::from("summarized")),
                ]),
            ),
            ("effort", Value::from(effort)),
        ]));
    }
    anthropic_adaptive_efforts(&model.api_id)?;
    let mut thinking = vec![("type".to_string(), Value::from("adaptive"))];
    if anthropic_omits_thinking(&model.api_id) {
        thinking.push(("display".to_string(), Value::from("summarized")));
    }
    Some(object(vec![
        ("thinking", Value::Object(Map::from_iter(thinking))),
        ("effort", Value::from(effort)),
    ]))
}

/// `reasoningEffort` (transform.ts:1769-1837).
fn reasoning_effort(model: &RuntimeModel, effort: &str) -> Option<Value> {
    match model.api_npm.as_str() {
        "@openrouter/ai-sdk-provider" => Some(object(vec![(
            "reasoning",
            object(vec![("effort", Value::from(effort))]),
        )])),
        "@ai-sdk/anthropic" | "@ai-sdk/google-vertex/anthropic" => anthropic_effort(model, effort)
            .or_else(|| Some(object(vec![("effort", Value::from(effort))]))),
        "@ai-sdk/google" | "@ai-sdk/google-vertex" => Some(object(vec![(
            "thinkingConfig",
            object(vec![
                ("includeThoughts", Value::Bool(true)),
                ("thinkingLevel", Value::from(effort)),
            ]),
        )])),
        "@ai-sdk/amazon-bedrock" => {
            if anthropic_adaptive_efforts(&model.api_id).is_some() {
                let mut config = vec![
                    ("type".to_string(), Value::from("adaptive")),
                    ("maxReasoningEffort".to_string(), Value::from(effort)),
                ];
                if anthropic_omits_thinking(&model.api_id) {
                    config.push(("display".to_string(), Value::from("summarized")));
                }
                return Some(object(vec![(
                    "reasoningConfig",
                    Value::Object(Map::from_iter(config)),
                )]));
            }
            if anthropic_opus_45(&model.api_id) {
                return Some(object(vec![(
                    "reasoningConfig",
                    object(vec![
                        ("type", Value::from("enabled")),
                        (
                            "budgetTokens",
                            num(f64::min(16_000.0, (model.limit_output / 2.0 - 1.0).floor())),
                        ),
                        ("maxReasoningEffort", Value::from(effort)),
                    ]),
                )]));
            }
            if model.api_id.contains("anthropic") {
                return None;
            }
            Some(object(vec![(
                "reasoningConfig",
                object(vec![
                    ("type", Value::from("enabled")),
                    ("maxReasoningEffort", Value::from(effort)),
                ]),
            )]))
        }
        "@ai-sdk/gateway" => {
            if model.id.contains("anthropic") {
                Some(object(vec![
                    (
                        "thinking",
                        object(vec![
                            ("type", Value::from("adaptive")),
                            ("display", Value::from("summarized")),
                        ]),
                    ),
                    ("effort", Value::from(effort)),
                ]))
            } else if model.id.contains("google") {
                Some(object(vec![(
                    "thinkingConfig",
                    object(vec![
                        ("includeThoughts", Value::Bool(true)),
                        ("thinkingLevel", Value::from(effort)),
                    ]),
                )]))
            } else {
                Some(object(vec![("reasoningEffort", Value::from(effort))]))
            }
        }
        "@ai-sdk/github-copilot" => {
            if model.id.contains("gemini") {
                None
            } else if model.id.contains("claude") {
                Some(object(vec![("reasoningEffort", Value::from(effort))]))
            } else {
                Some(object(vec![
                    ("reasoningEffort", Value::from(effort)),
                    ("reasoningSummary", Value::from("auto")),
                    (
                        "include",
                        Value::Array(vec![Value::from("reasoning.encrypted_content")]),
                    ),
                ]))
            }
        }
        "@ai-sdk/openai" | "@ai-sdk/amazon-bedrock/mantle" | "@ai-sdk/azure" => Some(object(vec![
            ("reasoningEffort", Value::from(effort)),
            ("reasoningSummary", Value::from("auto")),
            (
                "include",
                Value::Array(vec![Value::from("reasoning.encrypted_content")]),
            ),
        ])),
        "@jerome-benoit/sap-ai-provider-v2" => {
            if model.id.contains("anthropic") {
                Some(object(vec![(
                    "modelParams",
                    object(vec![
                        (
                            "thinking",
                            object(vec![
                                ("type", Value::from("adaptive")),
                                ("display", Value::from("summarized")),
                            ]),
                        ),
                        (
                            "output_config",
                            object(vec![("effort", Value::from(effort))]),
                        ),
                    ]),
                )]))
            } else {
                Some(object(vec![(
                    "modelParams",
                    object(vec![("reasoning_effort", Value::from(effort))]),
                )]))
            }
        }
        "@ai-sdk/openai-compatible"
        | "@ai-sdk/xai"
        | "@ai-sdk/mistral"
        | "@ai-sdk/groq"
        | "@ai-sdk/cerebras"
        | "@ai-sdk/deepinfra"
        | "@ai-sdk/togetherai"
        | "venice-ai-sdk-provider"
        | "ai-gateway-provider"
        | "merge-gateway-ai-sdk-provider" => {
            Some(object(vec![("reasoningEffort", Value::from(effort))]))
        }
        "gitlab-ai-provider" => {
            if model
                .family
                .as_deref()
                .is_some_and(|f| f.starts_with("gpt"))
            {
                Some(object(vec![("reasoningEffort", Value::from(effort))]))
            } else if model
                .family
                .as_deref()
                .is_some_and(|f| f.starts_with("claude"))
            {
                Some(object(vec![(
                    "thinking",
                    object(vec![
                        ("type", Value::from("adaptive")),
                        ("effort", Value::from(effort)),
                    ]),
                )]))
            } else {
                None
            }
        }
        "@ai-sdk/cohere" | "@ai-sdk/perplexity" | "@ai-sdk/vercel" | "@ai-sdk/alibaba" => None,
        _ => None,
    }
}

/// `reasoningBudget` (transform.ts:1863-1907).
fn reasoning_budget(model: &RuntimeModel, budget: f64) -> Option<Value> {
    match model.api_npm.as_str() {
        "@openrouter/ai-sdk-provider" => Some(object(vec![(
            "reasoning",
            object(vec![("max_tokens", num(budget))]),
        )])),
        "@ai-sdk/anthropic" | "@ai-sdk/google-vertex/anthropic" => Some(object(vec![(
            "thinking",
            object(vec![
                ("type", Value::from("enabled")),
                ("budgetTokens", num(budget)),
            ]),
        )])),
        "@ai-sdk/google" | "@ai-sdk/google-vertex" => Some(object(vec![(
            "thinkingConfig",
            object(vec![
                ("includeThoughts", Value::Bool(true)),
                ("thinkingBudget", num(budget)),
            ]),
        )])),
        "@ai-sdk/amazon-bedrock" => Some(object(vec![(
            "reasoningConfig",
            object(vec![
                ("type", Value::from("enabled")),
                ("budgetTokens", num(budget)),
            ]),
        )])),
        "@ai-sdk/gateway" => {
            if model.id.contains("anthropic") {
                Some(object(vec![(
                    "thinking",
                    object(vec![
                        ("type", Value::from("enabled")),
                        ("budgetTokens", num(budget)),
                    ]),
                )]))
            } else if model.id.contains("google") {
                Some(object(vec![(
                    "thinkingConfig",
                    object(vec![
                        ("includeThoughts", Value::Bool(true)),
                        ("thinkingBudget", num(budget)),
                    ]),
                )]))
            } else {
                None
            }
        }
        "@ai-sdk/cohere" => Some(object(vec![(
            "thinking",
            object(vec![
                ("type", Value::from("enabled")),
                ("tokenBudget", num(budget)),
            ]),
        )])),
        "@ai-sdk/alibaba" => Some(object(vec![
            ("enableThinking", Value::Bool(true)),
            ("thinkingBudget", num(budget)),
        ])),
        "@jerome-benoit/sap-ai-provider-v2" => {
            if model.id.contains("anthropic") {
                Some(object(vec![(
                    "modelParams",
                    object(vec![(
                        "thinking",
                        object(vec![
                            ("type", Value::from("enabled")),
                            ("budget_tokens", num(budget)),
                        ]),
                    )]),
                )]))
            } else if model.id.contains("gemini") {
                Some(object(vec![(
                    "modelParams",
                    object(vec![(
                        "thinkingConfig",
                        object(vec![
                            ("includeThoughts", Value::Bool(true)),
                            ("thinkingBudget", num(budget)),
                        ]),
                    )]),
                )]))
            } else {
                None
            }
        }
        _ => None,
    }
}

/// `reasoningVariants` (transform.ts:1704-1720) — `model` is the models-dev
/// catalog entry, `target` the runtime model the payloads are computed for.
pub fn reasoning_variants(model: &catalog::Model, target: &RuntimeModel) -> Option<Variants> {
    let options = model.reasoning_options.as_ref()?;
    if options.is_empty() {
        return Some(Variants::new());
    }

    if let Some(crate::catalog::ReasoningOption::Effort { values }) = options
        .iter()
        .find(|option| matches!(option, crate::catalog::ReasoningOption::Effort { .. }))
    {
        return Some(effort_variants(target, values));
    }

    let toggle = options
        .iter()
        .any(|option| matches!(option, crate::catalog::ReasoningOption::Toggle));
    let budget = options.iter().find_map(|option| match option {
        crate::catalog::ReasoningOption::BudgetTokens { min, max } => Some((*min, *max)),
        _ => None,
    });
    let Some((min, max)) = budget else {
        return toggle
            .then(|| non_empty(reasoning_toggle(target)))
            .flatten();
    };
    let mut merged = Variants::new();
    if toggle {
        merged.extend(reasoning_toggle(target));
    }
    merged.extend(budget_variants(target, min, max));
    non_empty(merged)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn model(api_id: &str, npm: &str) -> RuntimeModel {
        RuntimeModel {
            id: api_id.to_string(),
            provider_id: "test".to_string(),
            api_id: api_id.to_string(),
            api_npm: npm.to_string(),
            api_url: String::new(),
            release_date: "2025-06-01".to_string(),
            family: None,
            limit_output: 64_000.0,
            reasoning: true,
        }
    }

    #[test]
    fn non_reasoning_model_has_no_variants() {
        let mut m = model("any-model", "@ai-sdk/anthropic");
        m.reasoning = false;
        assert!(variants(&m).is_empty());
    }

    #[test]
    fn anthropic_modern_adaptive_efforts() {
        let mut m = model("claude-opus-4-7", "@ai-sdk/anthropic");
        m.limit_output = 64_000.0;
        let v = variants(&m);
        let keys: Vec<&str> = v.keys().map(String::as_str).collect();
        assert_eq!(keys, vec!["high", "low", "max", "medium", "xhigh"]);
        assert_eq!(
            v["low"],
            json!({
                "thinking": {"type": "adaptive", "display": "summarized"},
                "effort": "low"
            })
        );
        // 4.5-era models inline the token budgets instead (opus-4-5 payload).
        let old = model("claude-opus-4-5", "@ai-sdk/anthropic");
        let v = variants(&old);
        let keys: Vec<&str> = v.keys().map(String::as_str).collect();
        assert_eq!(keys, vec!["high", "low", "medium"]);
        assert_eq!(
            v["high"],
            json!({
                "thinking": {"type": "enabled", "budgetTokens": 16000},
                "effort": "high"
            })
        );
        let plain = model("claude-sonnet-4-5", "@ai-sdk/anthropic");
        let v = variants(&plain);
        assert_eq!(
            v["max"],
            json!({"thinking": {"type": "enabled", "budgetTokens": 31999}})
        );
    }

    #[test]
    fn openai_efforts_follow_release_date() {
        let mut m = model("gpt-5", "@ai-sdk/openai");
        m.release_date = "2025-12-10".to_string();
        let v = variants(&m);
        let keys: Vec<&str> = v.keys().map(String::as_str).collect();
        assert_eq!(
            keys,
            vec!["high", "low", "medium", "minimal", "none", "xhigh"]
        );
        assert_eq!(
            variants(&m)["none"],
            json!({
                "reasoningEffort": "none",
                "reasoningSummary": "auto",
                "include": ["reasoning.encrypted_content"]
            })
        );
        m.release_date = "2025-11-01".to_string();
        let v = variants(&m);
        let keys: Vec<&str> = v.keys().map(String::as_str).collect();
        assert_eq!(keys, vec!["high", "low", "medium", "minimal"]);
    }

    #[test]
    fn gemini_2_5_budgets() {
        let mut m = model("gemini-2.5-pro", "@ai-sdk/google");
        m.limit_output = 65_536.0;
        let v = variants(&m);
        assert_eq!(
            v["high"],
            json!({"thinkingConfig": {"includeThoughts": true, "thinkingBudget": 16000}})
        );
        assert_eq!(
            v["max"],
            json!({"thinkingConfig": {"includeThoughts": true, "thinkingBudget": 32768}})
        );
        let flash = model("gemini-3-flash", "@ai-sdk/google");
        let v = variants(&flash);
        let keys: Vec<&str> = v.keys().map(String::as_str).collect();
        assert_eq!(keys, vec!["high", "low", "medium", "minimal"]);
    }

    #[test]
    fn glm52_detection() {
        let m = model("glm-5.2", "@ai-sdk/openai-compatible");
        let v = variants(&m);
        assert_eq!(v["high"], json!({"reasoningEffort": "high"}));
        assert_eq!(v["max"], json!({"reasoningEffort": "max"}));
    }

    #[test]
    fn deepseek_and_qwen_have_no_variants() {
        assert!(variants(&model("deepseek-v3", "@ai-sdk/openai-compatible")).is_empty());
        assert!(variants(&model("qwen3-max", "@ai-sdk/openai-compatible")).is_empty());
    }

    #[test]
    fn reasoning_options_effort() {
        let mut target = model("openai/gpt-5.2", "@ai-sdk/openai-compatible");
        target.id = "openai/gpt-5.2".to_string();
        target.release_date = "2025-12-01".to_string();
        let model = crate::catalog::Model {
            id: "gpt-5.2".to_string(),
            name: "GPT".to_string(),
            family: None,
            release_date: "2025-12-01".to_string(),
            attachment: false,
            reasoning: true,
            temperature: false,
            tool_call: true,
            reasoning_options: Some(vec![crate::catalog::ReasoningOption::Effort {
                values: vec![Some("low".to_string()), None, Some("high".to_string())],
            }]),
            interleaved: None,
            cost: None,
            limit: crate::catalog::ModelLimit {
                context: 1000.0,
                input: None,
                output: 64_000.0,
            },
            modalities: None,
            experimental: None,
            status: None,
            provider: None,
        };
        let v = reasoning_variants(&model, &target).unwrap();
        assert_eq!(v["none"], json!({"reasoningEffort": "none"}));
        assert_eq!(v["low"], json!({"reasoningEffort": "low"}));
        assert_eq!(v["high"], json!({"reasoningEffort": "high"}));
    }

    #[test]
    fn reasoning_options_budget_clamps_to_output_limit() {
        let mut target = model("claude-opus-4-5", "@ai-sdk/anthropic");
        target.limit_output = 8192.0;
        let model = crate::catalog::Model {
            id: "claude-opus-4-5".to_string(),
            name: "Opus".to_string(),
            family: None,
            release_date: "2025-01-01".to_string(),
            attachment: true,
            reasoning: true,
            temperature: true,
            tool_call: true,
            reasoning_options: Some(vec![crate::catalog::ReasoningOption::BudgetTokens {
                min: Some(1024.0),
                max: Some(65536.0),
            }]),
            interleaved: None,
            cost: None,
            limit: crate::catalog::ModelLimit {
                context: 200_000.0,
                input: None,
                output: 8192.0,
            },
            modalities: None,
            experimental: None,
            status: None,
            provider: None,
        };
        let v = reasoning_variants(&model, &target).unwrap();
        // max clamps to output - 1; high is midpoint of the clamped maximum.
        assert_eq!(
            v["high"],
            json!({"thinking": {"type": "enabled", "budgetTokens": 4096}})
        );
        assert_eq!(
            v["max"],
            json!({"thinking": {"type": "enabled", "budgetTokens": 8191}})
        );
    }

    #[test]
    fn sap_wraps_in_model_params() {
        let mut m = model("gpt-5.2", "@jerome-benoit/sap-ai-provider-v2");
        m.release_date = "2025-12-10".to_string();
        let v = variants(&m);
        assert_eq!(
            v["medium"],
            json!({"modelParams": {"reasoning_effort": "medium"}})
        );
    }

    #[test]
    fn minimax_m3_nvidia_uses_chat_template_kwargs() {
        let mut m = model("minimax-m3", "@ai-sdk/openai-compatible");
        m.provider_id = "nvidia".to_string();
        let v = variants(&m);
        assert_eq!(
            v["none"],
            json!({"chat_template_kwargs": {"thinking_mode": "disabled"}})
        );
    }
}
