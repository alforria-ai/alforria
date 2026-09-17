//! Context-overflow classification (port of `provider-error.ts`).
//!
//! NOTE: This is the M2.4 `isContextOverflow` port, implemented here so that
//! the M2.5 executor's classification compiles and tests green while M2.4 was
//! still a stub. The M2.4 owner should review/re-land this file.

use std::sync::LazyLock;

use regex::Regex;

const PATTERN_SOURCES: &[&str] = &[
    r"(?i)prompt is too long",
    r"(?i)request_too_large",
    r"(?i)input is too long for requested model",
    r"(?i)exceeds the context window",
    r"(?i)exceeds (?:the )?(?:model'?s )?maximum context length(?: of [\d,]+ tokens?|\s*\([\d,]+\))",
    r"(?i)input token count.*exceeds the maximum",
    r"(?i)tokens in request more than max tokens allowed",
    r"(?i)maximum prompt length is \d+",
    r"(?i)reduce the length of the messages",
    r"(?i)maximum context length is \d+ tokens",
    r"(?i)exceeds (?:the )?maximum allowed input length of [\d,]+ tokens?",
    r"(?i)input \(\d+ tokens\) is longer than the model'?s context length \(\d+ tokens\)",
    r"(?i)exceeds the limit of \d+",
    r"(?i)exceeds the available context size",
    r"(?i)greater than the context length",
    r"(?i)context window exceeds limit",
    r"(?i)exceeded model token limit",
    r"(?i)context[_ ]length[_ ]exceeded",
    r"(?i)request entity too large",
    r"(?i)context length is only \d+ tokens",
    r"(?i)input length.*exceeds.*context length",
    r"(?i)prompt too long; exceeded (?:max )?context length",
    r"(?i)too large for model with \d+ maximum context length",
    r"(?i)prompt has [\d,]+ tokens?, but the configured context size is [\d,]+ tokens?",
    r"(?i)model_context_window_exceeded",
    r"(?i)too many tokens",
    r"(?i)token limit exceeded",
];

const EXCLUSION_SOURCES: &[&str] = &[
    r"(?i)^(throttling error|service unavailable):",
    r"(?i)rate limit",
    r"(?i)too many requests",
];

fn compile(sources: &[&str]) -> Vec<Regex> {
    sources
        .iter()
        .map(|source| Regex::new(source).expect("invalid regex"))
        .collect()
}

static PATTERNS: LazyLock<Vec<Regex>> = LazyLock::new(|| compile(PATTERN_SOURCES));
static EXCLUSIONS: LazyLock<Vec<Regex>> = LazyLock::new(|| compile(EXCLUSION_SOURCES));
static NO_BODY: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?i)^4(00|13)\s*(status code)?\s*\(no body\)").unwrap());

/// Whether a provider error message indicates the request exceeded the
/// model's context window (from `provider-error.ts`).
pub fn is_context_overflow(message: &str) -> bool {
    if EXCLUSIONS.iter().any(|pattern| pattern.is_match(message)) {
        return false;
    }
    PATTERNS.iter().any(|pattern| pattern.is_match(message)) || NO_BODY.is_match(message)
}

#[cfg(test)]
mod tests {
    use super::is_context_overflow;

    #[test]
    fn detects_positive_samples() {
        assert!(is_context_overflow("Error: prompt is too long"));
        assert!(is_context_overflow(
            "requested 5000 tokens; maximum context length is 4096 tokens"
        ));
        assert!(is_context_overflow("400 (no body)"));
    }

    #[test]
    fn honors_exclusions() {
        assert!(!is_context_overflow("Too many requests, please wait"));
        assert!(!is_context_overflow(
            "rate limit reached: prompt is too long"
        ));
        assert!(!is_context_overflow(
            "Throttling error: the request was rejected"
        ));
    }
}
