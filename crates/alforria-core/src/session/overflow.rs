//! Context-overflow detection — port of `session/overflow.ts` (10-34) plus
//! the `ProviderTransform.maxOutputTokens` helper it depends on
//! (transform.ts:18, 1468).

use alforria_llm::schema::messages::{ContentPart, Message, ToolResultValue};
use alforria_schema::llm::ToolContent;
use alforria_schema::session_v1::V1StepTokens;

use crate::config::schema::Config;

/// models.dev `limit.context` reserve held back for compaction
/// (`COMPACTION_BUFFER`, overflow.ts:5).
pub const COMPACTION_BUFFER: f64 = 20_000.0;

/// `OUTPUT_TOKEN_MAX` (transform.ts:18) — the default cap on generated
/// output tokens.
pub const OUTPUT_TOKEN_MAX: f64 = 32_000.0;

/// TS `Provider.Model.limit` — `{ context, input?, output }`.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ModelLimits {
    pub context: f64,
    pub input: Option<f64>,
    pub output: f64,
}

/// `ProviderTransform.maxOutputTokens` (transform.ts:1468):
/// `Math.min(model.limit.output, outputTokenMax) || outputTokenMax` — the
/// `||` restores the cap when the model limit is 0/NaN.
pub fn max_output_tokens(model: &ModelLimits, output_token_max: Option<f64>) -> f64 {
    let cap = output_token_max.unwrap_or(OUTPUT_TOKEN_MAX);
    let min = js_min(model.output, cap);
    if min == 0.0 || min.is_nan() {
        cap
    } else {
        min
    }
}

/// `usable()` input (overflow.ts:10).
pub struct UsableInput<'a> {
    pub cfg: &'a Config,
    pub model: &'a ModelLimits,
    pub output_token_max: Option<f64>,
}

/// `usable()` (overflow.ts:12-22): the context budget a conversation may
/// occupy before compaction kicks in.
pub fn usable(input: &UsableInput<'_>) -> f64 {
    let context = input.model.context;
    if context == 0.0 {
        return 0.0;
    }

    let reserved = match input.cfg.compaction.as_ref().and_then(|c| c.reserved) {
        Some(reserved) => reserved as f64,
        None => js_min(
            COMPACTION_BUFFER,
            max_output_tokens(input.model, input.output_token_max),
        ),
    };

    // Truthy input limit? `Math.max(0, input - reserved)` — NaN-propagating.
    match input.model.input.filter(|v| js_truthy(*v)) {
        Some(limit_input) => js_max(0.0, limit_input - reserved),
        None => js_max(
            0.0,
            context - max_output_tokens(input.model, input.output_token_max),
        ),
    }
}

/// `isOverflow()` input (overflow.ts:24-34).
pub struct IsOverflowInput<'a> {
    pub cfg: &'a Config,
    pub tokens: &'a V1StepTokens,
    pub model: &'a ModelLimits,
    pub output_token_max: Option<f64>,
}

/// `isOverflow()` (overflow.ts:24-34): whether the accumulated tokens have
/// reached the usable budget. Disabled by `compaction.auto == false` or a
/// zero context limit.
pub fn is_overflow(input: &IsOverflowInput<'_>) -> bool {
    if input.cfg.compaction.as_ref().and_then(|c| c.auto) == Some(false) {
        return false;
    }
    if input.model.context == 0.0 {
        return false;
    }

    let count = match input.tokens.total.filter(|total| js_truthy(*total)) {
        Some(total) => total,
        None => {
            let cache = &input.tokens.cache;
            input.tokens.input + input.tokens.output + cache.read + cache.write
        }
    };
    count
        >= usable(&UsableInput {
            cfg: input.cfg,
            model: input.model,
            output_token_max: input.output_token_max,
        })
}

// ---------------------------------------------------------------------------
// Request sizing (alforria extension)
// ---------------------------------------------------------------------------

/// The context window assumed for a model that declares none (`limit.context`
/// of 0), unless `compaction.default_context` says otherwise. Such a model
/// stays "unknown" to [`is_overflow`]; this only bounds what is sent to it.
pub const DEFAULT_CONTEXT_CEILING: f64 = 128_000.0;

/// What one image or PDF is counted as: providers bill media by size on
/// screen, not by the length of its base64.
pub const MEDIA_TOKEN_ESTIMATE: f64 = 2_000.0;

/// A token count for text no provider has counted yet. Three bytes per token
/// overshoots prose and stays close for code, JSON and non-Latin text, where
/// the usual four characters undershoots.
pub fn conservative_tokens(bytes: usize) -> f64 {
    (bytes as f64 / 3.0).ceil()
}

/// The limits to size a request against: the model's own, or the default
/// ceiling when the model declares no context window.
pub fn effective_limits(cfg: &Config, model: &ModelLimits) -> ModelLimits {
    if js_truthy(model.context) {
        return *model;
    }
    let ceiling = cfg
        .compaction
        .as_ref()
        .and_then(|compaction| compaction.default_context)
        .map(|ceiling| ceiling.get() as f64)
        .unwrap_or(DEFAULT_CONTEXT_CEILING);
    ModelLimits {
        context: ceiling,
        input: None,
        output: model.output,
    }
}

/// The tokens a request may hold: [`usable`] over [`effective_limits`].
pub fn request_budget(cfg: &Config, model: &ModelLimits, output_token_max: Option<f64>) -> f64 {
    usable(&UsableInput {
        cfg,
        model: &effective_limits(cfg, model),
        output_token_max,
    })
}

fn estimate_part(part: &ContentPart) -> f64 {
    match part {
        ContentPart::Text { text, .. } => conservative_tokens(text.len()),
        ContentPart::Media { .. } => MEDIA_TOKEN_ESTIMATE,
        ContentPart::ToolCall { name, input, .. } => {
            conservative_tokens(name.len() + json_len(input))
        }
        ContentPart::ToolResult { name, result, .. } => {
            conservative_tokens(name.len())
                + match result {
                    ToolResultValue::Json { value }
                    | ToolResultValue::Text { value }
                    | ToolResultValue::Error { value } => conservative_tokens(json_len(value)),
                    ToolResultValue::Content { value } => value
                        .iter()
                        .map(|item| match item {
                            ToolContent::Text { text } => conservative_tokens(text.len()),
                            ToolContent::File { .. } => MEDIA_TOKEN_ESTIMATE,
                        })
                        .sum(),
                }
        }
        ContentPart::Reasoning {
            text, encrypted, ..
        } => conservative_tokens(text.len() + encrypted.as_ref().map_or(0, String::len)),
    }
}

fn json_len(value: &serde_json::Value) -> usize {
    match value {
        serde_json::Value::String(text) => text.len(),
        other => serde_json::to_string(other).map_or(0, |text| text.len()),
    }
}

/// A conservative token count for rendered messages.
pub fn estimate_messages(messages: &[Message]) -> f64 {
    messages
        .iter()
        .map(|message| 4.0 + message.content.iter().map(estimate_part).sum::<f64>())
        .sum()
}

/// A conservative token count for the fixed part of a request: the system
/// prompt and the tool definitions.
pub fn estimate_overhead<'a>(
    system: &[String],
    tools: impl Iterator<Item = (&'a str, &'a str, &'a serde_json::Value)>,
) -> f64 {
    let system: usize = system.iter().map(String::len).sum();
    let tools: usize = tools
        .map(|(name, description, schema)| name.len() + description.len() + json_len(schema))
        .sum();
    conservative_tokens(system + tools)
}

/// The tokens a provider recorded for a finished response: everything it
/// was sent plus what it wrote.
pub fn recorded_tokens(tokens: &V1StepTokens) -> f64 {
    match tokens.total.filter(|total| js_truthy(*total)) {
        Some(total) => total,
        None => tokens.input + tokens.output + tokens.cache.read + tokens.cache.write,
    }
}

/// The size of a request about to be sent.
pub struct RequestSize<'a> {
    /// The newest finished response's recorded tokens, when the provider
    /// counted them and no compaction has replaced the history since. They
    /// cover the system prompt, the tools and every message before `since`.
    pub recorded: Option<f64>,
    /// The messages nothing has counted: all of them without `recorded`,
    /// else that response and what follows it.
    pub since: &'a [Message],
    /// [`estimate_overhead`]; counted only without `recorded`.
    pub overhead: f64,
}

/// A conservative token count for a request.
pub fn estimate_request(input: &RequestSize<'_>) -> f64 {
    match input.recorded.filter(|recorded| js_truthy(*recorded)) {
        Some(recorded) => recorded + estimate_messages(input.since),
        None => input.overhead + estimate_messages(input.since),
    }
}

/// JS number truthiness — `0`, `-0` and `NaN` are falsy.
fn js_truthy(value: f64) -> bool {
    value != 0.0 && !value.is_nan()
}

/// `Math.max` — NaN-propagating (unlike `f64::max`).
fn js_max(a: f64, b: f64) -> f64 {
    if a.is_nan() || b.is_nan() {
        f64::NAN
    } else if a > b {
        a
    } else {
        b
    }
}

/// `Math.min` — NaN-propagating (unlike `f64::min`).
fn js_min(a: f64, b: f64) -> f64 {
    if a.is_nan() || b.is_nan() {
        f64::NAN
    } else if a < b {
        a
    } else {
        b
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg() -> Config {
        serde_json::from_str("{}").unwrap()
    }

    fn cfg_compaction(json: &str) -> Config {
        serde_json::from_str(json).unwrap()
    }

    fn model(context: f64, input: Option<f64>, output: f64) -> ModelLimits {
        ModelLimits {
            context,
            input,
            output,
        }
    }

    fn tokens(total: Option<f64>, input: f64, output: f64, read: f64, write: f64) -> V1StepTokens {
        V1StepTokens {
            total,
            input,
            output,
            reasoning: 0.0,
            cache: alforria_schema::session_v1::V1TokenCache { read, write },
        }
    }

    #[test]
    fn max_output_tokens_table() {
        let big = model(200_000.0, None, 64_000.0);
        let small = model(200_000.0, None, 8_192.0);
        let zero = model(200_000.0, None, 0.0);
        assert_eq!(max_output_tokens(&big, None), 32_000.0);
        assert_eq!(max_output_tokens(&small, None), 8_192.0);
        assert_eq!(max_output_tokens(&zero, None), 32_000.0); // || fallback
        assert_eq!(max_output_tokens(&big, Some(4_096.0)), 4_096.0);
        assert_eq!(max_output_tokens(&small, Some(64_000.0)), 8_192.0);
        // Math.min propagates NaN → NaN || NaN is still NaN
        assert!(max_output_tokens(&big, Some(f64::NAN)).is_nan());
        let nan_output = model(200_000.0, None, f64::NAN);
        assert_eq!(max_output_tokens(&nan_output, None), OUTPUT_TOKEN_MAX);
    }

    #[test]
    fn usable_table() {
        let cfg = cfg();

        // context == 0 → 0
        assert_eq!(
            usable(&UsableInput {
                cfg: &cfg,
                model: &model(0.0, None, 8_192.0),
                output_token_max: None,
            }),
            0.0
        );

        // input limit present: input - min(20000, maxOutput);
        // maxOutputTokens = min(8192, 32000) = 8192 → reserved 8192
        assert_eq!(
            usable(&UsableInput {
                cfg: &cfg,
                model: &model(200_000.0, Some(100_000.0), 8_192.0),
                output_token_max: None,
            }),
            91_808.0
        );
        // 32000 > 20000 → reserved 20000
        assert_eq!(
            usable(&UsableInput {
                cfg: &cfg,
                model: &model(200_000.0, Some(100_000.0), 100_000.0),
                output_token_max: None,
            }),
            80_000.0
        );
        // outputTokenMax overrides the buffer
        assert_eq!(
            usable(&UsableInput {
                cfg: &cfg,
                model: &model(200_000.0, Some(100_000.0), 100_000.0),
                output_token_max: Some(5_000.0),
            }),
            95_000.0
        );

        // no input limit: context - maxOutputTokens
        assert_eq!(
            usable(&UsableInput {
                cfg: &cfg,
                model: &model(200_000.0, None, 8_192.0),
                output_token_max: None,
            }),
            191_808.0
        );
        // falsy input limits (0 / None) take the context branch
        assert_eq!(
            usable(&UsableInput {
                cfg: &cfg,
                model: &model(200_000.0, Some(0.0), 8_192.0),
                output_token_max: None,
            }),
            191_808.0
        );
        // clamp at 0
        assert_eq!(
            usable(&UsableInput {
                cfg: &cfg,
                model: &model(10_000.0, None, 100_000.0),
                output_token_max: None,
            }),
            0.0
        );

        // cfg.compaction.reserved override
        let cfg = cfg_compaction(r#"{"compaction": {"reserved": 5000}}"#);
        assert_eq!(
            usable(&UsableInput {
                cfg: &cfg,
                model: &model(200_000.0, Some(100_000.0), 8_192.0),
                output_token_max: None,
            }),
            95_000.0
        );
        // reserved 0 is honored (?? keeps 0)
        let cfg = cfg_compaction(r#"{"compaction": {"reserved": 0}}"#);
        assert_eq!(
            usable(&UsableInput {
                cfg: &cfg,
                model: &model(200_000.0, Some(100_000.0), 8_192.0),
                output_token_max: None,
            }),
            100_000.0
        );
    }

    #[test]
    fn is_overflow_table() {
        // disabled via compaction.auto == false
        let disabled = cfg_compaction(r#"{"compaction": {"auto": false}}"#);
        assert!(!is_overflow(&IsOverflowInput {
            cfg: &disabled,
            tokens: &tokens(Some(1_000_000.0), 0.0, 0.0, 0.0, 0.0),
            model: &model(200_000.0, Some(100_000.0), 8_192.0),
            output_token_max: None,
        }));

        // context == 0 → never
        let cfg = cfg();
        assert!(!is_overflow(&IsOverflowInput {
            cfg: &cfg,
            tokens: &tokens(Some(1_000_000.0), 0.0, 0.0, 0.0, 0.0),
            model: &model(0.0, Some(100_000.0), 8_192.0),
            output_token_max: None,
        }));

        // under budget (usable = 100_000 - 8_192 = 91_808)
        assert!(!is_overflow(&IsOverflowInput {
            cfg: &cfg,
            tokens: &tokens(Some(91_807.0), 0.0, 0.0, 0.0, 0.0),
            model: &model(200_000.0, Some(100_000.0), 8_192.0),
            output_token_max: None,
        }));

        // at budget — >= is overflow
        assert!(is_overflow(&IsOverflowInput {
            cfg: &cfg,
            tokens: &tokens(Some(91_808.0), 0.0, 0.0, 0.0, 0.0),
            model: &model(200_000.0, Some(100_000.0), 8_192.0),
            output_token_max: None,
        }));

        // total falsy (0) → falls back to the component sum
        assert!(is_overflow(&IsOverflowInput {
            cfg: &cfg,
            tokens: &tokens(Some(0.0), 50_000.0, 41_808.0, 0.0, 0.0),
            model: &model(200_000.0, Some(100_000.0), 8_192.0),
            output_token_max: None,
        }));

        // missing total → component sum
        assert!(is_overflow(&IsOverflowInput {
            cfg: &cfg,
            tokens: &tokens(None, 50_000.0, 0.0, 41_808.0, 0.0),
            model: &model(200_000.0, Some(100_000.0), 8_192.0),
            output_token_max: None,
        }));
    }

    #[test]
    fn a_model_without_a_limit_is_sized_against_the_default_ceiling() {
        let unknown = model(0.0, None, 8_192.0);
        assert_eq!(
            request_budget(&cfg(), &unknown, None),
            DEFAULT_CONTEXT_CEILING - 8_192.0
        );
        let configured = cfg_compaction(r#"{"compaction": {"default_context": 32000}}"#);
        assert_eq!(
            request_budget(&configured, &unknown, None),
            32_000.0 - 8_192.0
        );
        // A declared limit is never replaced.
        let known = model(200_000.0, None, 8_192.0);
        assert_eq!(request_budget(&configured, &known, None), 191_808.0);
        // ...and the recorded-token check still treats 0 as unknown.
        assert!(!is_overflow(&IsOverflowInput {
            cfg: &cfg(),
            tokens: &tokens(Some(1_000_000.0), 0.0, 0.0, 0.0, 0.0),
            model: &unknown,
            output_token_max: None,
        }));
    }

    #[test]
    fn a_request_is_sized_from_recorded_tokens_plus_what_nothing_counted() {
        let text = Message::user("x".repeat(3_000));
        let image = Message {
            id: None,
            role: alforria_llm::schema::ids::MessageRole::User,
            content: vec![ContentPart::media("image/png", "A".repeat(4_000_000))],
            metadata: None,
            native: None,
        };
        // 3,000 bytes at three a token, plus the per-message allowance.
        assert_eq!(estimate_messages(std::slice::from_ref(&text)), 1_004.0);
        // An image counts as an image, not as megabytes of base64.
        assert_eq!(
            estimate_messages(std::slice::from_ref(&image)),
            4.0 + MEDIA_TOKEN_ESTIMATE
        );
        let schema = serde_json::json!({ "type": "object" });
        let overhead = estimate_overhead(
            &["s".repeat(300)],
            [("read", "Reads a file", &schema)].into_iter(),
        );
        assert_eq!(overhead, ((300 + 4 + 12 + 17) as f64 / 3.0).ceil());
        // Nothing recorded: everything is estimated.
        assert_eq!(
            estimate_request(&RequestSize {
                recorded: None,
                since: std::slice::from_ref(&text),
                overhead,
            }),
            overhead + 1_004.0
        );
        // Recorded tokens already cover the system prompt and the tools.
        assert_eq!(
            estimate_request(&RequestSize {
                recorded: Some(50_000.0),
                since: std::slice::from_ref(&text),
                overhead,
            }),
            51_004.0
        );
    }
}
