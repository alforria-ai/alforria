//! Context-overflow detection — port of `session/overflow.ts` (10-34) plus
//! the `ProviderTransform.maxOutputTokens` helper it depends on
//! (transform.ts:18, 1468).

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
}
