//! Session cost accounting — port of `getUsage` (session.ts:338-405).
//!
//! `getUsage` normalizes a provider [`Usage`] into the wire
//! [`V1StepTokens`] shape and prices it against the model's models.dev cost
//! table. All cost arithmetic runs through [`rust_decimal::Decimal`],
//! mirroring the TS `new Decimal(...).mul(...).div(1_000_000)` chain; the
//! final `toNumber()` becomes `Decimal::to_f64`.
//!
//! The model view is the normalized `Provider.Model.cost` (provider.ts
//! `cost()` mapping, models-dev → provider cost): plain `input`/`output`,
//! a non-optional `cache: {read, write}`, `tiers`, and
//! `experimentalOver200K` (models-dev `context_over_200k`).

use std::str::FromStr;

use alforria_llm::schema::Usage;
use alforria_schema::llm::ProviderMetadata;
use alforria_schema::session_v1::{V1StepTokens, V1TokenCache};
use rust_decimal::prelude::ToPrimitive;
use rust_decimal::Decimal;
use serde_json::Value;

use crate::catalog::types::ContextTierType;

/// `models.dev` costs are per million tokens.
const PER_MILLION: f64 = 1_000_000.0;

/// Copilot meter divisor — `totalNanoAiu` → fractional AIU (session.ts:387-392).
const COPILOT_NANO_AIU_DIVISOR: f64 = 100_000_000_000.0;

/// The `> 200_000` tier threshold for `experimentalOver200K` (session.ts:381).
const OVER_200K_THRESHOLD: f64 = 200_000.0;

/// TS `Provider.Model.cost.cache` — `{ read, write }`.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct CacheCost {
    pub read: f64,
    pub write: f64,
}

/// TS `Provider.Model.cost.tiers[*]` — a context-tier price point.
#[derive(Debug, Clone, PartialEq)]
pub struct CostTier {
    pub input: f64,
    pub output: f64,
    pub cache: CacheCost,
    /// `tier.type` — only `"context"` exists in the models.dev schema.
    pub kind: ContextTierType,
    /// `tier.size` — the context-token threshold the tier applies *over*.
    pub size: f64,
}

/// TS `Provider.Model.cost.experimentalOver200K`.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Over200k {
    pub input: f64,
    pub output: f64,
    pub cache: CacheCost,
}

/// TS `Provider.Model.cost` — the normalized models.dev cost entry
/// (provider.ts:1230-1252): every optional side defaults to `0`.
#[derive(Debug, Clone, PartialEq)]
pub struct ModelCost {
    pub input: f64,
    pub output: f64,
    pub cache: CacheCost,
    pub tiers: Vec<CostTier>,
    pub experimental_over_200k: Option<Over200k>,
}

impl ModelCost {
    /// The provider.ts `cost()` mapping (models-dev `Cost` → `Provider.Model.cost`).
    pub fn from_catalog(cost: Option<&crate::catalog::types::Cost>) -> ModelCost {
        let Some(cost) = cost else {
            return ModelCost::free();
        };
        let base = cost;
        ModelCost {
            input: base.input,
            output: base.output,
            cache: CacheCost {
                read: base.cache_read.unwrap_or(0.0),
                write: base.cache_write.unwrap_or(0.0),
            },
            tiers: base
                .tiers
                .as_deref()
                .map(|tiers| {
                    tiers
                        .iter()
                        .map(|tier| CostTier {
                            input: tier.input,
                            output: tier.output,
                            cache: CacheCost {
                                read: tier.cache_read.unwrap_or(0.0),
                                write: tier.cache_write.unwrap_or(0.0),
                            },
                            kind: tier.tier.kind,
                            size: tier.tier.size,
                        })
                        .collect()
                })
                .unwrap_or_default(),
            experimental_over_200k: cost.context_over_200k.as_ref().map(|over| Over200k {
                input: over.input,
                output: over.output,
                cache: CacheCost {
                    read: over.cache_read.unwrap_or(0.0),
                    write: over.cache_write.unwrap_or(0.0),
                },
            }),
        }
    }

    /// A cost entry with all prices zero — models without models.dev cost.
    pub fn free() -> ModelCost {
        ModelCost {
            input: 0.0,
            output: 0.0,
            cache: CacheCost {
                read: 0.0,
                write: 0.0,
            },
            tiers: Vec::new(),
            experimental_over_200k: None,
        }
    }
}

/// [`get_usage`] input — TS `getUsage({ model, usage, metadata })`.
pub struct GetUsage<'a> {
    /// `input.model.cost` — the only model field `getUsage` reads.
    pub model: &'a ModelCost,
    pub usage: &'a Usage,
    pub metadata: Option<&'a ProviderMetadata>,
}

/// [`get_usage`] output — `{ cost, tokens }` (session.ts:405-407).
#[derive(Debug, Clone, PartialEq)]
pub struct UsageCost {
    pub cost: f64,
    pub tokens: V1StepTokens,
}

/// `getUsage` (session.ts:338-405) — normalize a provider usage report into
/// wire tokens plus the step's USD cost.
pub fn get_usage(input: GetUsage<'_>) -> UsageCost {
    let usage = input.usage;
    let metadata = input.metadata;

    let input_tokens = safe(usage.input_tokens.unwrap_or(0.0));
    let output_tokens = safe(usage.output_tokens.unwrap_or(0.0));
    let reasoning_tokens = safe(usage.reasoning_tokens.unwrap_or(0.0));

    let cache_read_input_tokens = safe(usage.cache_read_input_tokens.unwrap_or(0.0));
    let cache_write_input_tokens = safe(cache_write_input_tokens(usage, metadata));

    // AI SDK v6 normalized inputTokens to include cached tokens across all
    // providers (including Anthropic/Bedrock which previously excluded
    // them). Always subtract cache tokens to get the non-cached input count
    // for separate cost calculation (session.ts:355-357).
    let adjusted_input_tokens =
        safe(input_tokens - cache_read_input_tokens - cache_write_input_tokens);

    let tokens = V1StepTokens {
        total: usage.total_tokens,
        input: adjusted_input_tokens,
        output: safe(output_tokens - reasoning_tokens),
        reasoning: reasoning_tokens,
        cache: V1TokenCache {
            read: cache_read_input_tokens,
            write: cache_write_input_tokens,
        },
    };

    // Cost-info selection (session.ts:379-386): the largest context tier the
    // request is strictly over, else experimentalOver200K past 200K, else the
    // base cost table.
    let context_tokens = input_tokens;
    let cost = select_cost_tier(input.model, context_tokens)
        .map(CostView::Tier)
        .or_else(|| {
            input
                .model
                .experimental_over_200k
                .as_ref()
                .filter(|_| context_tokens > OVER_200K_THRESHOLD)
                .map(CostView::Over200k)
        })
        .unwrap_or(CostView::Cost(input.model));

    let total_nano_aiu = copilot_total_nano_aiu(metadata);
    let cost = match total_nano_aiu {
        Some(nano) => decimal_from_f64(nano)
            .and_then(|v| v.checked_div(copilot_divisor()))
            .and_then(|v| v.to_f64())
            .unwrap_or_else(|| nano / COPILOT_NANO_AIU_DIVISOR),
        None => safe(cost_sum(cost, &tokens)),
    };

    UsageCost { cost, tokens }
}

/// The cost table variant `getUsage` prices against.
#[derive(Debug, Clone, Copy)]
enum CostView<'a> {
    Tier(&'a CostTier),
    Over200k(&'a Over200k),
    Cost(&'a ModelCost),
}

impl CostView<'_> {
    fn prices(&self) -> (f64, f64, CacheCost) {
        match self {
            CostView::Tier(t) => (t.input, t.output, t.cache),
            CostView::Over200k(o) => (o.input, o.output, o.cache),
            CostView::Cost(c) => (c.input, c.output, c.cache),
        }
    }
}

/// TS `filter(...).sort((a, b) => b.size - a.size)[0]` — the first of the
/// largest tier the context is strictly over (stable sort keeps the first
/// of equal sizes).
fn select_cost_tier(model: &ModelCost, context_tokens: f64) -> Option<&CostTier> {
    let mut best: Option<&CostTier> = None;
    for tier in &model.tiers {
        if tier.kind == ContextTierType::Context
            && context_tokens > tier.size
            && best.is_none_or(|b| tier.size > b.size)
        {
            best = Some(tier);
        }
    }
    best
}

/// `finite()` (session.ts:340) — non-finite values count as 0.
fn finite(value: f64) -> f64 {
    if value.is_finite() {
        value
    } else {
        0.0
    }
}

/// `safe()` (session.ts:341) — clamp to `>= 0`.
fn safe(value: f64) -> f64 {
    finite(value).max(0.0)
}

/// `input.usage.cacheWriteInputTokens ?? anthropic.cacheCreationInputTokens ??
/// vertex.cacheCreationInputTokens ?? bedrock.usage.cacheWriteInputTokens ??
/// venice.usage.cacheCreationInputTokens ?? 0` (session.ts:348-356).
fn cache_write_input_tokens(usage: &Usage, metadata: Option<&ProviderMetadata>) -> f64 {
    if let Some(value) = usage.cache_write_input_tokens {
        return value;
    }
    let value = metadata
        .and_then(|meta| metadata_value(meta, "anthropic", &["cacheCreationInputTokens"]))
        .or_else(|| {
            metadata.and_then(|meta| metadata_value(meta, "vertex", &["cacheCreationInputTokens"]))
        })
        .or_else(|| {
            metadata.and_then(|meta| {
                metadata_value(meta, "bedrock", &["usage", "cacheWriteInputTokens"])
            })
        })
        .or_else(|| {
            metadata.and_then(|meta| {
                metadata_value(meta, "venice", &["usage", "cacheCreationInputTokens"])
            })
        });
    value.map(js_number).unwrap_or(0.0)
}

/// `metadata?.[provider]?.[key…]` — walks a dotted path; `null`/missing
/// (the JS `??` operands) both continue the fallback chain.
fn metadata_value<'a>(
    metadata: &'a ProviderMetadata,
    provider: &str,
    path: &[&str],
) -> Option<&'a Value> {
    let (first, rest) = path.split_first()?;
    let mut value: &Value = metadata.get(provider)?.get(*first)?;
    for key in rest {
        value = value.as_object()?.get(*key)?;
    }
    Some(value).filter(|value| !value.is_null())
}

/// The copilot `totalNanoAiu` short-circuit (session.ts:387, 389-391):
/// `typeof number && Number.isFinite && >= 0`.
fn copilot_total_nano_aiu(metadata: Option<&ProviderMetadata>) -> Option<f64> {
    let value = metadata.and_then(|meta| metadata_value(meta, "copilot", &["totalNanoAiu"]))?;
    let number = value.as_f64()?;
    (number.is_finite() && number >= 0.0).then_some(number)
}

/// The per-term cost sum (session.ts:388-404): every term
/// `Decimal(tokens).mul(finite(price)).div(1_000_000)` — input, output,
/// cache.read, cache.write, and reasoning charged at the *output* rate.
fn cost_sum(cost: CostView<'_>, tokens: &V1StepTokens) -> f64 {
    let (input_price, output_price, cache) = cost.prices();
    let terms = [
        (tokens.input, input_price),
        (tokens.output, output_price),
        (tokens.cache.read, cache.read),
        (tokens.cache.write, cache.write),
        // charge reasoning tokens at the same rate as output tokens
        (tokens.reasoning, output_price),
    ];
    match cost_sum_decimal(&terms) {
        Some(total) => total,
        None => cost_sum_f64(&terms),
    }
}

/// Decimal sum — mirrors the TS chain exactly. Returns `None` when a value
/// leaves the 28-digit `Decimal` range (Decimal.js would keep going; the
/// f64 fallback keeps the result finite instead of panicking).
fn cost_sum_decimal(terms: &[(f64, f64)]) -> Option<f64> {
    let mut total = Decimal::ZERO;
    for (tokens, price) in terms {
        let term = decimal_from_f64(*tokens)?;
        let price = decimal_from_f64(finite(*price))?;
        let term = term.checked_mul(price)?;
        let term = term.checked_div(decimal_divisor())?;
        total = total.checked_add(term)?;
    }
    total.to_f64()
}

fn cost_sum_f64(terms: &[(f64, f64)]) -> f64 {
    terms
        .iter()
        .map(|(tokens, price)| tokens * finite(*price) / PER_MILLION)
        .sum()
}

/// `new Decimal(number)` — Decimal.js converts via the number's shortest
/// round-trip string; do the same so `0.1` stays `0.1`.
fn decimal_from_f64(value: f64) -> Option<Decimal> {
    Decimal::from_str(&format!("{value}")).ok()
}

fn decimal_divisor() -> Decimal {
    Decimal::from_str(&format!("{PER_MILLION}")).unwrap()
}

fn copilot_divisor() -> Decimal {
    Decimal::from_str(&format!("{COPILOT_NANO_AIU_DIVISOR}")).unwrap()
}

/// JS `Number(value)` for a JSON value (NaN-able). `null` → 0 per JS;
/// objects/arrays coerce per ToNumber (subset).
fn js_number(value: &Value) -> f64 {
    match value {
        Value::Null => 0.0,
        Value::Bool(b) => {
            if *b {
                1.0
            } else {
                0.0
            }
        }
        Value::Number(n) => n.as_f64().unwrap_or(f64::NAN),
        Value::String(s) => js_to_number(s),
        Value::Array(items) => match items.as_slice() {
            [] => 0.0,
            [single] => js_number(single),
            _ => f64::NAN,
        },
        _ => f64::NAN,
    }
}

/// JS `ToNumber(string)`: trimmed; `""` → 0; `Infinity`; hex/octal/binary
/// literals; else a full-string decimal parse — `"12abc"` is NaN.
fn js_to_number(input: &str) -> f64 {
    let trimmed = input.trim();
    if trimmed.is_empty() {
        return 0.0;
    }
    let (sign, trimmed) = match trimmed.strip_prefix('-') {
        Some(rest) => (-1.0, rest),
        None => (1.0, trimmed.strip_prefix('+').unwrap_or(trimmed)),
    };
    if trimmed == "Infinity" {
        return sign * f64::INFINITY;
    }
    let radix_body = trimmed
        .strip_prefix("0x")
        .map(|b| (u128::from_str_radix(b, 16).ok(), 16.0))
        .or_else(|| {
            trimmed
                .strip_prefix("0X")
                .map(|b| (u128::from_str_radix(b, 16).ok(), 16.0))
        })
        .or_else(|| {
            trimmed
                .strip_prefix("0o")
                .map(|b| (u128::from_str_radix(b, 8).ok(), 8.0))
        })
        .or_else(|| {
            trimmed
                .strip_prefix("0b")
                .map(|b| (u128::from_str_radix(b, 2).ok(), 2.0))
        });
    if let Some((digits, _radix)) = radix_body {
        return digits.map(|d| sign * d as f64).unwrap_or(f64::NAN);
    }
    if let Ok(parsed) = trimmed.parse::<f64>() {
        return sign * parsed;
    }
    f64::NAN
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Deterministic LCG so property tests are reproducible.
    struct Lcg(u64);
    impl Lcg {
        fn next(&mut self) -> u64 {
            self.0 = self
                .0
                .wrapping_mul(6364136223846793005)
                .wrapping_add(1442695040888963407);
            self.0 >> 33
        }
        fn unit(&mut self) -> f64 {
            self.next() as f64 / u64::MAX as f64
        }
        fn tokens(&mut self, max: u64) -> f64 {
            let _ = self.unit();
            (self.next() % max) as f64
        }
    }

    fn anthropic_cost() -> ModelCost {
        ModelCost {
            input: 3.0,
            output: 15.0,
            cache: CacheCost {
                read: 0.3,
                write: 3.75,
            },
            tiers: vec![CostTier {
                input: 3.0,
                output: 15.0,
                cache: CacheCost {
                    read: 0.3,
                    write: 3.75,
                },
                kind: ContextTierType::Context,
                size: 200_000.0,
            }],
            experimental_over_200k: Some(Over200k {
                input: 6.0,
                output: 22.5,
                cache: CacheCost {
                    read: 0.6,
                    write: 3.75,
                },
            }),
        }
    }

    fn fractional_cost() -> ModelCost {
        ModelCost {
            input: 0.07,
            output: 0.28,
            cache: CacheCost {
                read: 0.014,
                write: 0.25,
            },
            tiers: Vec::new(),
            experimental_over_200k: None,
        }
    }

    fn usage(
        input: Option<f64>,
        output: Option<f64>,
        reasoning: Option<f64>,
        cache_read: Option<f64>,
        cache_write: Option<f64>,
        total: Option<f64>,
    ) -> Usage {
        Usage {
            input_tokens: input,
            output_tokens: output,
            non_cached_input_tokens: None,
            cache_read_input_tokens: cache_read,
            cache_write_input_tokens: cache_write,
            reasoning_tokens: reasoning,
            total_tokens: total,
            provider_metadata: None,
        }
    }

    /// Insert `value` (an object) into a ProviderMetadata under `key`.
    fn meta_insert(meta: &mut ProviderMetadata, key: &str, value: serde_json::Value) {
        let obj = value.as_object().cloned().unwrap_or_default();
        meta.insert(key.to_string(), obj);
    }

    fn run(model: &ModelCost, usage: &Usage, metadata: Option<&ProviderMetadata>) -> UsageCost {
        get_usage(GetUsage {
            model,
            usage,
            metadata,
        })
    }

    /// 26 vectors captured from the TS `getUsage` with decimal.js@10.5.0
    /// (session.ts:338-405, pinned commit 88c6c7a). Cost equality is
    /// bit-exact — byte-parity is the STOP S1 gate.
    #[test]
    fn ts_cost_vectors() {
        let anthropic = anthropic_cost();
        let fractional = fractional_cost();
        let over_200k = ModelCost {
            input: 3.0,
            output: 15.0,
            cache: CacheCost {
                read: 0.3,
                write: 3.75,
            },
            tiers: Vec::new(),
            experimental_over_200k: Some(Over200k {
                input: 6.0,
                output: 22.5,
                cache: CacheCost {
                    read: 0.6,
                    write: 3.75,
                },
            }),
        };
        type CostVector<'a> = (
            &'a str,
            &'a ModelCost,
            Usage,
            Option<ProviderMetadata>,
            f64,
            V1StepTokens,
        );
        let cases: Vec<CostVector> = vec![
            // v1_plain
            (
                "v1",
                &anthropic,
                usage(
                    Some(1000.0),
                    Some(500.0),
                    Some(0.0),
                    None,
                    None,
                    Some(1500.0),
                ),
                None,
                0.0105,
                V1StepTokens {
                    total: Some(1500.0),
                    input: 1000.0,
                    output: 500.0,
                    reasoning: 0.0,
                    cache: V1TokenCache {
                        read: 0.0,
                        write: 0.0,
                    },
                },
            ),
            // v2_cached_reasoning
            (
                "v2",
                &anthropic,
                usage(
                    Some(50000.0),
                    Some(2000.0),
                    Some(800.0),
                    Some(30000.0),
                    Some(5000.0),
                    Some(57000.0),
                ),
                None,
                0.10275,
                V1StepTokens {
                    total: Some(57000.0),
                    input: 15000.0,
                    output: 1200.0,
                    reasoning: 800.0,
                    cache: V1TokenCache {
                        read: 30000.0,
                        write: 5000.0,
                    },
                },
            ),
            // v3_fractional
            (
                "v3",
                &fractional,
                usage(
                    Some(1234567.0),
                    Some(89012.0),
                    Some(34000.0),
                    None,
                    None,
                    Some(1322579.0),
                ),
                None,
                0.11134305,
                V1StepTokens {
                    total: Some(1322579.0),
                    input: 1234567.0,
                    output: 55012.0,
                    reasoning: 34000.0,
                    cache: V1TokenCache {
                        read: 0.0,
                        write: 0.0,
                    },
                },
            ),
            // v4_tier_at_boundary — contextTokens == tier.size, not strictly >
            (
                "v4",
                &anthropic,
                usage(
                    Some(200000.0),
                    Some(100.0),
                    None,
                    None,
                    None,
                    Some(200100.0),
                ),
                None,
                0.6015,
                V1StepTokens {
                    total: Some(200100.0),
                    input: 200000.0,
                    output: 100.0,
                    reasoning: 0.0,
                    cache: V1TokenCache {
                        read: 0.0,
                        write: 0.0,
                    },
                },
            ),
            // v5_tier_just_over
            (
                "v5",
                &anthropic,
                usage(
                    Some(200001.0),
                    Some(100.0),
                    None,
                    None,
                    None,
                    Some(200101.0),
                ),
                None,
                0.601503,
                V1StepTokens {
                    total: Some(200101.0),
                    input: 200001.0,
                    output: 100.0,
                    reasoning: 0.0,
                    cache: V1TokenCache {
                        read: 0.0,
                        write: 0.0,
                    },
                },
            ),
            // v6_over_200k — the 200K tier matches first (same prices as base
            // here); the experimentalOver200K branch only applies with no
            // matching tier
            (
                "v6",
                &anthropic,
                usage(
                    Some(250000.0),
                    Some(1000.0),
                    None,
                    None,
                    None,
                    Some(251000.0),
                ),
                None,
                0.765,
                V1StepTokens {
                    total: Some(251000.0),
                    input: 250000.0,
                    output: 1000.0,
                    reasoning: 0.0,
                    cache: V1TokenCache {
                        read: 0.0,
                        write: 0.0,
                    },
                },
            ),
            // v6b — experimentalOver200K pricing when no tier matches
            (
                "v6b",
                &over_200k,
                usage(
                    Some(250000.0),
                    Some(1000.0),
                    None,
                    None,
                    None,
                    Some(251000.0),
                ),
                None,
                1.5225,
                V1StepTokens {
                    total: Some(251000.0),
                    input: 250000.0,
                    output: 1000.0,
                    reasoning: 0.0,
                    cache: V1TokenCache {
                        read: 0.0,
                        write: 0.0,
                    },
                },
            ),
        ];

        for (name, model, usage, metadata, cost, tokens) in cases {
            let out = run(model, &usage, metadata.as_ref());
            assert_eq!(out.cost, cost, "cost vector {name}");
            assert_eq!(out.tokens, tokens, "tokens vector {name}");
        }
    }

    #[test]
    fn ts_cost_vectors_copilot() {
        // v7_copilot — short-circuit: Decimal(123456789012) / 1e11
        let mut meta = ProviderMetadata::new();
        let mut copilot = serde_json::Map::new();
        copilot.insert(
            "totalNanoAiu".to_string(),
            serde_json::json!(123456789012_i64),
        );
        meta_insert(&mut meta, "copilot", serde_json::Value::Object(copilot));
        let out = run(
            &anthropic_cost(),
            &usage(Some(100.0), Some(50.0), None, None, None, Some(150.0)),
            Some(&meta),
        );
        assert_eq!(out.cost, 1.23456789012);
        assert_eq!(out.tokens.total, Some(150.0));

        // v17_copilot_zero
        let mut meta = ProviderMetadata::new();
        let mut copilot = serde_json::Map::new();
        copilot.insert("totalNanoAiu".to_string(), serde_json::json!(0));
        meta_insert(&mut meta, "copilot", serde_json::Value::Object(copilot));
        let out = run(
            &anthropic_cost(),
            &usage(Some(10.0), Some(10.0), None, None, None, Some(20.0)),
            Some(&meta),
        );
        assert_eq!(out.cost, 0.0);

        // v18_copilot_negative — negative falls through to cost math
        let mut meta = ProviderMetadata::new();
        let mut copilot = serde_json::Map::new();
        copilot.insert("totalNanoAiu".to_string(), serde_json::json!(-5));
        meta_insert(&mut meta, "copilot", serde_json::Value::Object(copilot));
        let out = run(
            &fractional_cost(),
            &usage(Some(1000.0), Some(2000.0), None, None, None, Some(3000.0)),
            Some(&meta),
        );
        assert_eq!(out.cost, 0.00063);
    }

    #[test]
    fn ts_cost_vectors_cache_fallback() {
        // v8_anthropic_metadata
        let mut meta = ProviderMetadata::new();
        meta_insert(
            &mut meta,
            "anthropic",
            serde_json::json!({"cacheCreationInputTokens": 1500}),
        );
        let out = run(
            &anthropic_cost(),
            &usage(Some(4000.0), Some(100.0), None, None, None, Some(4100.0)),
            Some(&meta),
        );
        assert_eq!(out.cost, 0.014625);
        assert_eq!(out.tokens.cache.write, 1500.0);
        assert_eq!(out.tokens.input, 2500.0);

        // v9_bedrock_metadata
        let mut meta = ProviderMetadata::new();
        meta_insert(
            &mut meta,
            "bedrock",
            serde_json::json!({"usage": {"cacheWriteInputTokens": 777}}),
        );
        let out = run(
            &anthropic_cost(),
            &usage(Some(4000.0), Some(100.0), None, None, None, Some(4100.0)),
            Some(&meta),
        );
        assert_eq!(out.cost, 0.01408275);
        assert_eq!(out.tokens.cache.write, 777.0);

        // v19_vertex_metadata
        let mut meta = ProviderMetadata::new();
        meta_insert(
            &mut meta,
            "vertex",
            serde_json::json!({"cacheCreationInputTokens": 900}),
        );
        let out = run(
            &anthropic_cost(),
            &usage(Some(3000.0), Some(100.0), None, None, None, Some(3100.0)),
            Some(&meta),
        );
        assert_eq!(out.cost, 0.011175);

        // v20_venice_metadata
        let mut meta = ProviderMetadata::new();
        meta_insert(
            &mut meta,
            "venice",
            serde_json::json!({"usage": {"cacheCreationInputTokens": 400}}),
        );
        let out = run(
            &anthropic_cost(),
            &usage(Some(3000.0), Some(100.0), None, None, None, Some(3100.0)),
            Some(&meta),
        );
        assert_eq!(out.cost, 0.0108);

        // v21_venice_missing — no usage object
        let mut meta = ProviderMetadata::new();
        meta_insert(&mut meta, "venice", serde_json::json!({}));
        let out = run(
            &anthropic_cost(),
            &usage(Some(3000.0), Some(100.0), None, None, None, Some(3100.0)),
            Some(&meta),
        );
        assert_eq!(out.cost, 0.0105);

        // v22_metadata_nan — non-numeric string → Number() NaN → 0
        let mut meta = ProviderMetadata::new();
        meta_insert(
            &mut meta,
            "anthropic",
            serde_json::json!({"cacheCreationInputTokens": "garbage"}),
        );
        let out = run(
            &anthropic_cost(),
            &usage(Some(3000.0), Some(100.0), None, None, None, Some(3100.0)),
            Some(&meta),
        );
        assert_eq!(out.cost, 0.0105);

        // v23_metadata_numeric_string — "250.5" coerces
        let mut meta = ProviderMetadata::new();
        meta_insert(
            &mut meta,
            "anthropic",
            serde_json::json!({"cacheCreationInputTokens": "250.5"}),
        );
        let out = run(
            &anthropic_cost(),
            &usage(Some(3000.0), Some(100.0), None, None, None, Some(3100.0)),
            Some(&meta),
        );
        assert_eq!(out.cost, 0.010687875);

        // v24_usage_wins — usage.cacheWriteInputTokens beats metadata
        let mut meta = ProviderMetadata::new();
        meta_insert(
            &mut meta,
            "anthropic",
            serde_json::json!({"cacheCreationInputTokens": 222}),
        );
        let out = run(
            &anthropic_cost(),
            &usage(
                Some(3000.0),
                Some(100.0),
                None,
                None,
                Some(111.0),
                Some(3100.0),
            ),
            Some(&meta),
        );
        assert_eq!(out.cost, 0.01058325);
        assert_eq!(out.tokens.cache.write, 111.0);
    }

    #[test]
    fn ts_cost_vectors_edge_clamps() {
        // v11_no_cost — a model with no cost table
        let out = run(
            &ModelCost::free(),
            &usage(Some(100.0), Some(100.0), None, None, None, Some(200.0)),
            None,
        );
        assert_eq!(out.cost, 0.0);
        assert_eq!(out.tokens.input, 100.0);

        // v12_multi_tier — largest applicable tier wins
        let multi = ModelCost {
            input: 0.15,
            output: 0.6,
            cache: CacheCost {
                read: 0.014,
                write: 0.2,
            },
            tiers: vec![
                CostTier {
                    input: 0.15,
                    output: 0.6,
                    cache: CacheCost {
                        read: 0.014,
                        write: 0.2,
                    },
                    kind: ContextTierType::Context,
                    size: 128_000.0,
                },
                CostTier {
                    input: 0.30,
                    output: 1.2,
                    cache: CacheCost {
                        read: 0.03,
                        write: 0.4,
                    },
                    kind: ContextTierType::Context,
                    size: 200_000.0,
                },
            ],
            experimental_over_200k: None,
        };
        let out = run(
            &multi,
            &usage(
                Some(180000.0),
                Some(5000.0),
                None,
                None,
                None,
                Some(185000.0),
            ),
            None,
        );
        assert_eq!(out.cost, 0.03);

        // v13_reasoning_over — reasoning > output clamps output to 0
        let out = run(
            &anthropic_cost(),
            &usage(Some(100.0), Some(10.0), Some(50.0), None, None, Some(110.0)),
            None,
        );
        assert_eq!(out.cost, 0.00105);
        assert_eq!(out.tokens.output, 0.0);
        assert_eq!(out.tokens.reasoning, 50.0);

        // v14_negative — usage clamps to zero
        let out = run(
            &anthropic_cost(),
            &usage(Some(-5.0), Some(-10.0), Some(-3.0), None, None, Some(-15.0)),
            None,
        );
        assert_eq!(out.cost, 0.0);
        assert_eq!(out.tokens.input, 0.0);
        assert_eq!(out.tokens.total, Some(-15.0)); // passthrough

        // v15_cache_over_input — cache read+write exceed input
        let out = run(
            &anthropic_cost(),
            &usage(
                Some(100.0),
                Some(100.0),
                None,
                Some(80.0),
                Some(40.0),
                Some(200.0),
            ),
            None,
        );
        assert_eq!(out.cost, 0.001674);
        assert_eq!(out.tokens.input, 0.0);

        // v16_big
        let out = run(
            &anthropic_cost(),
            &usage(
                Some(987654321.0),
                Some(123456789.0),
                Some(45678901.0),
                Some(111111111.0),
                Some(22222222.0),
                Some(1111111310.0),
            ),
            None,
        );
        assert_eq!(out.cost, 4531.4814648);

        // v25_infinity
        let out = run(
            &anthropic_cost(),
            &usage(
                Some(f64::INFINITY),
                Some(f64::INFINITY),
                Some(f64::INFINITY),
                None,
                None,
                Some(f64::INFINITY),
            ),
            None,
        );
        assert_eq!(out.cost, 0.0);

        // v26_no_total
        let out = run(
            &fractional_cost(),
            &usage(Some(1000.0), Some(2000.0), None, None, None, None),
            None,
        );
        assert_eq!(out.cost, 0.00063);
        assert_eq!(out.tokens.total, None);
    }

    /// M2 usage invariant through `getUsage`: when the provider breakdown
    /// holds (`nonCached + cacheRead + cacheWrite == input`), the derived
    /// adjusted input round-trips.
    #[test]
    fn property_usage_invariants() {
        let mut rng = Lcg(0x5eed);
        for _ in 0..200 {
            let non_cached = rng.tokens(500_000);
            let read = rng.tokens(100_000);
            let write = rng.tokens(50_000);
            let reasoning = rng.tokens(20_000);
            let visible = rng.tokens(20_000);
            let total = non_cached + read + write + reasoning + visible;
            let inclusive = non_cached + read + write;

            let out = run(
                &anthropic_cost(),
                &usage(
                    Some(inclusive),
                    Some(visible + reasoning),
                    Some(reasoning),
                    Some(read),
                    Some(write),
                    Some(total),
                ),
                None,
            );
            assert_eq!(out.tokens.input, non_cached);
            assert_eq!(out.tokens.cache.read, read);
            assert_eq!(out.tokens.cache.write, write);
            assert_eq!(out.tokens.output, visible);
            assert_eq!(out.tokens.reasoning, reasoning);
            assert!(out.cost.is_finite() && out.cost >= 0.0);

            // Cost is exact-decimal linear: doubling every token count
            // doubles the cost (f64 rounding aside).
            let doubled = run(
                &anthropic_cost(),
                &usage(
                    Some(inclusive * 2.0),
                    Some((visible + reasoning) * 2.0),
                    Some(reasoning * 2.0),
                    Some(read * 2.0),
                    Some(write * 2.0),
                    Some(total * 2.0),
                ),
                None,
            );
            let ratio = doubled.cost / out.cost.max(f64::MIN_POSITIVE);
            assert!(
                (ratio - 2.0).abs() < 1e-9,
                "cost not linear: {ratio} vs {}",
                out.cost
            );
        }
    }

    /// Copilot short-circuit property: cost is exactly `nano / 1e11`.
    #[test]
    fn property_copilot_short_circuit() {
        let mut rng = Lcg(0xfeed);
        for _ in 0..100 {
            let nano = rng.next() % 10_000_000_000_000;
            let mut meta = ProviderMetadata::new();
            meta_insert(
                &mut meta,
                "copilot",
                serde_json::json!({"totalNanoAiu": nano}),
            );
            let out = run(
                &anthropic_cost(),
                &usage(Some(1.0), Some(1.0), None, None, None, Some(2.0)),
                Some(&meta),
            );
            assert_eq!(out.cost, nano as f64 / 1e11);
        }
    }

    /// Tier selection at boundaries (session.ts:379-386): strictly over the
    /// tier size; the largest applicable size wins.
    #[test]
    fn tier_selection_boundaries() {
        let model = ModelCost {
            input: 1.0,
            output: 2.0,
            cache: CacheCost {
                read: 0.1,
                write: 0.2,
            },
            tiers: vec![
                CostTier {
                    input: 10.0,
                    output: 20.0,
                    cache: CacheCost {
                        read: 1.0,
                        write: 2.0,
                    },
                    kind: ContextTierType::Context,
                    size: 100_000.0,
                },
                CostTier {
                    input: 100.0,
                    output: 200.0,
                    cache: CacheCost {
                        read: 10.0,
                        write: 20.0,
                    },
                    kind: ContextTierType::Context,
                    size: 500_000.0,
                },
            ],
            experimental_over_200k: None,
        };

        // below all tiers → base
        let out = run(
            &model,
            &usage(Some(99_999.0), Some(0.0), None, None, None, None),
            None,
        );
        assert_eq!(out.cost, 99_999.0 * 1.0 / 1e6);

        // exactly at 100_000 → not over → base
        let out = run(
            &model,
            &usage(Some(100_000.0), Some(0.0), None, None, None, None),
            None,
        );
        assert_eq!(out.cost, 100_000.0 * 1.0 / 1e6);

        // one over → first tier
        let out = run(
            &model,
            &usage(Some(100_001.0), Some(0.0), None, None, None, None),
            None,
        );
        assert_eq!(out.cost, 100_001.0 * 10.0 / 1e6);

        // over both → largest tier wins
        let out = run(
            &model,
            &usage(Some(500_001.0), Some(0.0), None, None, None, None),
            None,
        );
        assert_eq!(out.cost, 500_001.0 * 100.0 / 1e6);
    }

    #[test]
    fn model_cost_from_catalog() {
        use crate::catalog::types::Cost;

        let cost = Cost {
            input: 3.0,
            output: 15.0,
            cache_read: Some(0.3),
            cache_write: Some(3.75),
            tiers: Some(vec![crate::catalog::types::CostTier {
                input: 3.0,
                output: 15.0,
                cache_read: None,
                cache_write: None,
                tier: crate::catalog::types::CostTierType {
                    kind: ContextTierType::Context,
                    size: 200_000.0,
                },
            }]),
            context_over_200k: Some(crate::catalog::types::ContextOver200k {
                input: 6.0,
                output: 22.5,
                cache_read: None,
                cache_write: None,
            }),
        };
        let normalized = ModelCost::from_catalog(Some(&cost));
        assert_eq!(normalized.cache.read, 0.3);
        assert_eq!(normalized.tiers[0].cache.read, 0.0);
        assert_eq!(
            normalized.experimental_over_200k,
            Some(Over200k {
                input: 6.0,
                output: 22.5,
                cache: CacheCost {
                    read: 0.0,
                    write: 0.0
                }
            })
        );

        let none = ModelCost::from_catalog(None);
        assert_eq!(none, ModelCost::free());
    }

    #[test]
    fn js_number_coercions() {
        assert_eq!(js_to_number(""), 0.0);
        assert_eq!(js_to_number("  12  "), 12.0);
        assert!(js_to_number("12abc").is_nan());
        assert_eq!(js_to_number("0x1A"), 26.0);
        assert_eq!(js_to_number("-Infinity"), f64::NEG_INFINITY);
        assert_eq!(js_to_number("+5.5"), 5.5);
        assert!(js_to_number(".").is_nan());
        assert_eq!(js_to_number(".5"), 0.5);
    }
}
