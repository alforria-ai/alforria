//! Shared helpers for provider cache-marker lowering
//! (from `protocols/utils/cache.ts`).
//!
//! Anthropic and Bedrock both enforce a 4-breakpoint cap per request and
//! accept the same `5m`/`1h` TTL buckets, so the counter and TTL mapping live
//! here.

/// Counter for provider cache-marker breakpoints.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Breakpoints {
    pub remaining: i64,
    pub dropped: i64,
}

pub fn new_breakpoints(cap: i64) -> Breakpoints {
    Breakpoints {
        remaining: cap,
        dropped: 0,
    }
}

/// Returns `"1h"` for any `ttl_seconds >= 3600`, otherwise `None` (the
/// provider default 5m). Anthropic & Bedrock both treat anything shorter than
/// an hour as 5m.
pub fn ttl_bucket(ttl_seconds: Option<f64>) -> Option<&'static str> {
    match ttl_seconds {
        Some(ttl) if ttl >= 3600.0 => Some("1h"),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn new_breakpoints_initializes_the_counter() {
        let breakpoints = new_breakpoints(4);
        assert_eq!(breakpoints.remaining, 4);
        assert_eq!(breakpoints.dropped, 0);
    }

    #[test]
    fn ttl_bucket_returns_1h_at_or_above_an_hour() {
        assert_eq!(ttl_bucket(None), None);
        assert_eq!(ttl_bucket(Some(3599.0)), None);
        assert_eq!(ttl_bucket(Some(3600.0)), Some("1h"));
        assert_eq!(ttl_bucket(Some(7200.0)), Some("1h"));
    }
}
