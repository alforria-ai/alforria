//! Built-in LibertAI provider — LTAI_PRICING aggregate → catalog `Provider`.
//!
//! The provider id (`libertai`), api base and model list are known without
//! models.dev: the models come from the LTAI_PRICING Aleph aggregate with a
//! 24h disk cache (`<cache>/ltai-pricing.json`) and an embedded snapshot
//! fallback, so the provider is never empty. Every leg degrades — IO errors
//! and fetch failures fall through to the next source, never fail the call.

use std::collections::BTreeMap;
use std::env;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use crate::catalog::{Cost, Modalities, Modality, Model, ModelLimit, Provider};
use crate::GlobalPaths;
use serde::{Deserialize, Serialize};

/// Built-in provider id (wire identity: `libertai/<model>`).
pub const LIBERTAI_PROVIDER_ID: &str = "libertai";

/// Default OpenAI-compatible inference base URL.
pub const LIBERTAI_API_BASE: &str = "https://api.libertai.io/v1";

/// Default LTAI_PRICING aggregate URL (`LIBERTAI_MODEL_CATALOG_URL` override;
/// empty string disables the fetch leg).
pub const DEFAULT_MODEL_CATALOG_URL: &str = "https://api2.aleph.im/api/v0/aggregates/0xe1F7220D201C64871Cefb25320a8a588393eE508.json?keys=LTAI_PRICING";

/// Base ids that also get a `{id}-thinking` entry — served by the API but
/// not listed in the aggregate.
const THINKING_VARIANTS: [&str; 1] = ["glm-5.3"];

/// Disk cache file name inside `Global.Path.cache`.
const CACHE_FILE: &str = "ltai-pricing.json";

/// Freshness window of the disk cache.
const CACHE_TTL_SECS: u64 = 24 * 60 * 60;

/// Field required by the catalog `Model` type; no upstream data carries it.
const RELEASE_DATE: &str = "2026-09-22";

// ---------------------------------------------------------------------------
// LTAI_PRICING wire types
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize)]
struct AggregateModel {
    id: String,
    name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    hf_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pricing: Option<AggregatePricing>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    capabilities: Option<AggregateCapabilities>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct AggregatePricing {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    text: Option<AggregatePricingText>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct AggregatePricingText {
    price_per_million_input_tokens: f64,
    price_per_million_output_tokens: f64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    price_per_million_cached_input_tokens: Option<f64>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct AggregateCapabilities {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    text: Option<AggregateCapabilitiesText>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct AggregateCapabilitiesText {
    #[serde(default)]
    vision: bool,
    #[serde(default)]
    reasoning: bool,
    #[serde(default)]
    context_window: Option<u32>,
    #[serde(default)]
    function_calling: bool,
}

/// The aggregate `LTAI_PRICING` value.
#[derive(Debug, Clone, Serialize, Deserialize)]
struct LtaiPricing {
    #[serde(default)]
    models: Vec<AggregateModel>,
}

// ---------------------------------------------------------------------------
// conversion
// ---------------------------------------------------------------------------

fn catalog_model(entry: &AggregateModel) -> Option<Model> {
    let pricing = entry.pricing.as_ref().and_then(|p| p.text.as_ref());
    let capabilities = entry.capabilities.as_ref().and_then(|c| c.text.as_ref());
    if pricing.is_none() && capabilities.is_none() {
        return None;
    }
    let vision = capabilities.map(|c| c.vision).unwrap_or(false);
    let mut input = vec![Modality::Text];
    if vision {
        input.push(Modality::Image);
    }
    let family = if entry.id == "glm-5.3-flash" {
        Some("glm-flash".to_string())
    } else {
        None
    };
    Some(Model {
        id: entry.id.clone(),
        name: entry.name.clone(),
        family,
        release_date: RELEASE_DATE.to_string(),
        attachment: vision,
        reasoning: capabilities.map(|c| c.reasoning).unwrap_or(false),
        temperature: true,
        tool_call: capabilities.map(|c| c.function_calling).unwrap_or(false),
        reasoning_options: None,
        interleaved: None,
        cost: pricing.map(|p| Cost {
            input: p.price_per_million_input_tokens,
            output: p.price_per_million_output_tokens,
            cache_read: p.price_per_million_cached_input_tokens,
            cache_write: None,
            tiers: None,
            context_over_200k: None,
        }),
        limit: ModelLimit {
            context: capabilities
                .and_then(|c| c.context_window)
                .map(f64::from)
                .unwrap_or(0.0),
            input: None,
            output: 32768.0,
        },
        modalities: Some(Modalities {
            input,
            output: vec![Modality::Text],
        }),
        experimental: None,
        status: None,
        provider: None,
    })
}

/// The aggregate → catalog conversion: text models only, plus the thinking
/// variants served by the API but not listed in the aggregate.
fn models_from_pricing(pricing: &LtaiPricing) -> BTreeMap<String, Model> {
    let mut models = BTreeMap::new();
    for entry in &pricing.models {
        if let Some(model) = catalog_model(entry) {
            models.insert(model.id.clone(), model);
        }
    }
    for base in THINKING_VARIANTS {
        let Some(model) = models.get(base) else {
            continue;
        };
        let mut thinking = model.clone();
        thinking.id = format!("{base}-thinking");
        thinking.name = format!("{} (thinking)", model.name);
        models.insert(thinking.id.clone(), thinking);
    }
    models
}

fn fallback_model(id: &str, name: &str, input: f64, output: f64) -> Model {
    Model {
        id: id.to_string(),
        name: name.to_string(),
        family: None,
        release_date: RELEASE_DATE.to_string(),
        attachment: false,
        reasoning: true,
        temperature: true,
        tool_call: true,
        reasoning_options: None,
        interleaved: None,
        cost: Some(Cost {
            input,
            output,
            cache_read: None,
            cache_write: None,
            tiers: None,
            context_over_200k: None,
        }),
        limit: ModelLimit {
            context: 262144.0,
            input: None,
            output: 32768.0,
        },
        modalities: Some(Modalities {
            input: vec![Modality::Text],
            output: vec![Modality::Text],
        }),
        experimental: None,
        status: None,
        provider: None,
    }
}

/// The last-resort pair when every other source is unavailable — the
/// provider is never empty.
fn fallback_models() -> BTreeMap<String, Model> {
    BTreeMap::from([
        (
            "glm-5.3".to_string(),
            fallback_model("glm-5.3", "GLM-5.3", 1.4, 4.4),
        ),
        (
            "glm-5.3-flash".to_string(),
            fallback_model("glm-5.3-flash", "GLM-5.3-Flash", 0.15, 0.5),
        ),
    ])
}

// ---------------------------------------------------------------------------
// cache / fetch
// ---------------------------------------------------------------------------

#[derive(Serialize, Deserialize)]
struct CacheFile {
    fetched_at_unix: u64,
    catalog: LtaiPricing,
}

fn api_base() -> String {
    env::var("LIBERTAI_API_BASE")
        .ok()
        .filter(|value| !value.is_empty())
        .unwrap_or_else(|| LIBERTAI_API_BASE.to_string())
}

fn catalog_url() -> Option<String> {
    match env::var("LIBERTAI_MODEL_CATALOG_URL") {
        Ok(url) if url.is_empty() => None,
        Ok(url) => Some(url),
        Err(_) => Some(DEFAULT_MODEL_CATALOG_URL.to_string()),
    }
}

fn now_unix() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

fn parse_pricing(text: &str) -> Option<LtaiPricing> {
    let value = serde_json::from_str::<serde_json::Value>(text).ok()?;
    if let Some(pricing) = value
        .get("data")
        .and_then(|data| data.get("LTAI_PRICING"))
        .cloned()
    {
        return serde_json::from_value(pricing).ok();
    }
    serde_json::from_value(value).ok()
}

/// Cache write is best-effort (`0o600` under unix) — IO errors never fail
/// the caller.
fn write_cache(path: &std::path::Path, pricing: &LtaiPricing) {
    #[cfg(unix)]
    use std::os::unix::fs::OpenOptionsExt;
    let Ok(text) = serde_json::to_string(&CacheFile {
        fetched_at_unix: now_unix(),
        catalog: pricing.clone(),
    }) else {
        return;
    };
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    #[cfg(unix)]
    let result = std::fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o600)
        .open(path)
        .and_then(|mut file| std::io::Write::write_all(&mut file, text.as_bytes()));
    #[cfg(not(unix))]
    let result = std::fs::write(path, &text);
    let _ = result;
}

/// The blocking client owns a tokio runtime; building or dropping it inside
/// an async worker panics, so the whole fetch runs on a plain thread.
/// `builtin_provider` is called from async request handlers.
fn fetch_pricing() -> Option<LtaiPricing> {
    std::thread::spawn(|| {
        let client = reqwest::blocking::Client::builder()
            .connect_timeout(Duration::from_secs(2))
            .timeout(Duration::from_secs(5))
            .build()
            .ok()?;
        let text = client
            .get(catalog_url()?)
            .send()
            .ok()?
            .error_for_status()
            .ok()?
            .text()
            .ok()?;
        parse_pricing(&text)
    })
    .join()
    .ok()
    .flatten()
}

/// Fresh 24h cache → fetch (rewrites the cache) → stale cache → embedded
/// snapshot → hardcoded fallback pair. Result memoized for the process so
/// at most one fetch attempt happens per TTL window.
fn resolve_models() -> BTreeMap<String, Model> {
    static MEMO: std::sync::Mutex<Option<(u64, BTreeMap<String, Model>)>> =
        std::sync::Mutex::new(None);
    let mut memo = MEMO.lock().unwrap();
    if let Some((fetched_at, models)) = memo.as_ref() {
        if now_unix().saturating_sub(*fetched_at) < CACHE_TTL_SECS {
            return models.clone();
        }
    }
    let models = compute_models();
    *memo = Some((now_unix(), models.clone()));
    models
}

fn compute_models() -> BTreeMap<String, Model> {
    let path = GlobalPaths::from_env().cache.join(CACHE_FILE);
    let cached = std::fs::read_to_string(&path)
        .ok()
        .and_then(|text| serde_json::from_str::<CacheFile>(&text).ok());
    if let Some(cached) = &cached {
        if now_unix().saturating_sub(cached.fetched_at_unix) < CACHE_TTL_SECS {
            let models = models_from_pricing(&cached.catalog);
            if !models.is_empty() {
                return models;
            }
        }
    }
    if let Some(pricing) = fetch_pricing() {
        let models = models_from_pricing(&pricing);
        if !models.is_empty() {
            write_cache(&path, &pricing);
            return models;
        }
    }
    if let Some(cached) = cached {
        let models = models_from_pricing(&cached.catalog);
        if !models.is_empty() {
            return models;
        }
    }
    let snapshot: LtaiPricing = serde_json::from_str(include_str!("libertai/snapshot.json"))
        .expect("embedded snapshot parses");
    let models = models_from_pricing(&snapshot);
    if models.is_empty() {
        fallback_models()
    } else {
        models
    }
}

/// The built-in `libertai` catalog provider.
pub fn builtin_provider() -> Provider {
    Provider {
        api: Some(api_base()),
        name: "LibertAI".to_string(),
        env: vec!["LIBERTAI_API_KEY".to_string()],
        id: LIBERTAI_PROVIDER_ID.to_string(),
        npm: Some("@ai-sdk/openai-compatible".to_string()),
        models: resolve_models(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn snapshot_pricing() -> LtaiPricing {
        serde_json::from_str(include_str!("libertai/snapshot.json")).unwrap()
    }

    #[test]
    fn converts_snapshot_models() {
        let models = models_from_pricing(&snapshot_pricing());
        let glm = &models["glm-5.3"];
        assert_eq!(glm.name, "GLM-5.3");
        assert_eq!(glm.family, None);
        let cost = glm.cost.as_ref().unwrap();
        assert_eq!(cost.input, 1.4);
        assert_eq!(cost.output, 4.4);
        assert_eq!(cost.cache_read, Some(0.26));
        assert_eq!(glm.limit.context, 262144.0);
        assert_eq!(glm.limit.output, 32768.0);
        assert!(!glm.attachment);
        assert!(glm.reasoning);
        assert!(glm.tool_call);
        assert!(glm.temperature);

        let flash = &models["glm-5.3-flash"];
        assert_eq!(flash.family.as_deref(), Some("glm-flash"));
        assert!(flash.attachment);
        assert_eq!(
            flash.modalities.as_ref().unwrap().input,
            vec![Modality::Text, Modality::Image]
        );
    }

    #[test]
    fn excludes_non_text_models() {
        let models = models_from_pricing(&snapshot_pricing());
        assert!(!models.contains_key("z-image-turbo"));
        assert!(!models.contains_key("search/duckduckgo"));
        assert!(!models.contains_key("bge-m3"));
        assert!(!models.contains_key("kokoro-82m"));
    }

    #[test]
    fn synthesizes_thinking_variants() {
        let models = models_from_pricing(&snapshot_pricing());
        let thinking = &models["glm-5.3-thinking"];
        assert_eq!(thinking.name, "GLM-5.3 (thinking)");
        assert_eq!(thinking.cost, models["glm-5.3"].cost);
        assert_eq!(thinking.limit, models["glm-5.3"].limit);
        assert!(!models.contains_key("glm-5.3-flash-thinking"));
    }
}
