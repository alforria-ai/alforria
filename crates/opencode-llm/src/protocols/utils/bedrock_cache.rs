//! Bedrock Converse `cachePoint` helpers (from `protocols/utils/bedrock-cache.ts`).

use serde::{Deserialize, Serialize};

use crate::protocols::utils::cache::{new_breakpoints, ttl_bucket, Breakpoints};
use crate::schema::options::{CacheHint, CacheHintType};

// Bedrock cache markers are positional: emit a `cachePoint` block immediately
// after the content the caller wants treated as a cacheable prefix. Bedrock
// accepts optional `ttl: "5m" | "1h"` on cachePoint, mirroring Anthropic.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CachePointBlock {
    pub cache_point: CachePoint,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CachePoint {
    pub r#type: CachePointType,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ttl: Option<CachePointTtl>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum CachePointType {
    #[serde(rename = "default")]
    Default,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum CachePointTtl {
    #[serde(rename = "5m")]
    FiveMinutes,
    #[serde(rename = "1h")]
    OneHour,
}

// Bedrock-Claude enforces the same 4-breakpoint cap as the Anthropic Messages
// API. Callers pass a shared counter through every `block()` call site so the
// budget is respected across `system`, `messages`, and `tools`.
pub const BEDROCK_BREAKPOINT_CAP: i64 = 4;

pub use crate::protocols::utils::cache::Breakpoints as BedrockBreakpoints;

pub fn breakpoints() -> Breakpoints {
    new_breakpoints(BEDROCK_BREAKPOINT_CAP)
}

const DEFAULT_5M: CachePointBlock = CachePointBlock {
    cache_point: CachePoint {
        r#type: CachePointType::Default,
        ttl: None,
    },
};

const DEFAULT_1H: CachePointBlock = CachePointBlock {
    cache_point: CachePoint {
        r#type: CachePointType::Default,
        ttl: Some(CachePointTtl::OneHour),
    },
};

pub fn block(breakpoints: &mut Breakpoints, cache: Option<&CacheHint>) -> Option<CachePointBlock> {
    let cache = cache?;
    if !matches!(
        cache.r#type,
        CacheHintType::Ephemeral | CacheHintType::Persistent
    ) {
        return None;
    }
    if breakpoints.remaining <= 0 {
        breakpoints.dropped += 1;
        return None;
    }
    breakpoints.remaining -= 1;
    Some(match ttl_bucket(cache.ttl_seconds) {
        Some("1h") => DEFAULT_1H.clone(),
        _ => DEFAULT_5M.clone(),
    })
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    fn hint(ttl_seconds: Option<f64>) -> CacheHint {
        CacheHint {
            r#type: CacheHintType::Ephemeral,
            ttl_seconds,
        }
    }

    #[test]
    fn block_serializes_the_default_cache_point() {
        let block = block(&mut breakpoints(), Some(&hint(None))).unwrap();
        let value = serde_json::to_value(&block).unwrap();
        assert_eq!(value, json!({"cachePoint": {"type": "default"}}));
    }

    #[test]
    fn block_serializes_the_1h_ttl_bucket() {
        let block = block(&mut breakpoints(), Some(&hint(Some(3600.0)))).unwrap();
        let value = serde_json::to_value(&block).unwrap();
        assert_eq!(
            value,
            json!({"cachePoint": {"type": "default", "ttl": "1h"}})
        );
    }

    #[test]
    fn block_without_a_cache_hint_is_a_no_op() {
        assert!(block(&mut breakpoints(), None).is_none());
    }

    #[test]
    fn block_respects_the_breakpoint_cap() {
        let mut counter = breakpoints();
        for _ in 0..BEDROCK_BREAKPOINT_CAP {
            assert!(block(&mut counter, Some(&hint(None))).is_some());
        }
        assert_eq!(counter.remaining, 0);
        assert!(block(&mut counter, Some(&hint(None))).is_none());
        assert_eq!(counter.dropped, 1);
    }

    #[test]
    fn five_minute_hints_do_not_carry_a_ttl() {
        let block = block(&mut breakpoints(), Some(&hint(Some(3599.0)))).unwrap();
        assert_eq!(block.cache_point.ttl, None);
    }
}
