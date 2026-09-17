//! Permission evaluation helpers used by the tool system.
//!
//! Port of `evaluate` from `packages/opencode/src/permission/index.ts` and
//! `Wildcard.match` from `packages/core/src/util/wildcard.ts`. Only the pure
//! evaluation lives here; the ask/answer flow (M5) is out of scope.

use opencode_schema::permission_v1::{PermissionV1Action, PermissionV1Rule, PermissionV1Ruleset};

/// `PermissionV1.Ruleset` — owned by the schema crate (spec §9 S2).
pub type Ruleset = PermissionV1Ruleset;

/// `Wildcard.match` (`packages/core/src/util/wildcard.ts`): glob-style match
/// where `*` matches anything (including `/`) and `?` one char. A pattern
/// ending in `" *"` optionally matches a suffix starting with `" "`.
pub fn wildcard_match(input: &str, pattern: &str) -> bool {
    let normalized = input.replace('\\', "/");
    let mut escaped = String::with_capacity(pattern.len());
    for c in pattern.replace('\\', "/").chars() {
        if matches!(
            c,
            '.' | '+' | '^' | '$' | '{' | '}' | '(' | ')' | '[' | ']' | '|' | '\\'
        ) {
            escaped.push('\\');
        }
        escaped.push(c);
    }
    let mut escaped = escaped.replace('*', ".*").replace('?', ".");
    if let Some(prefix) = escaped.strip_suffix(" .*") {
        escaped = format!("{prefix}( .*)?");
    }
    match regex::Regex::new(&format!("^{escaped}$")) {
        Ok(re) => re.is_match(&normalized),
        Err(_) => false,
    }
}

/// `Permission.evaluate` (permission/index.ts:28-38): the last matching rule
/// across the flattened rulesets, or the default `ask` rule when nothing
/// matches.
pub fn evaluate(permission: &str, pattern: &str, rulesets: &[&Ruleset]) -> PermissionV1Rule {
    for ruleset in rulesets.iter().rev() {
        for rule in ruleset.iter().rev() {
            if wildcard_match(permission, &rule.permission)
                && wildcard_match(pattern, &rule.pattern)
            {
                return rule.clone();
            }
        }
    }
    PermissionV1Rule {
        permission: permission.to_string(),
        pattern: "*".to_string(),
        action: PermissionV1Action::Ask,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rule(permission: &str, pattern: &str, action: PermissionV1Action) -> PermissionV1Rule {
        PermissionV1Rule {
            permission: permission.to_string(),
            pattern: pattern.to_string(),
            action,
        }
    }

    #[test]
    fn wildcard_star_and_question() {
        assert!(wildcard_match("bash", "bash"));
        assert!(wildcard_match("edit", "*"));
        assert!(wildcard_match("read", "r?a?"));
        assert!(!wildcard_match("read", "r?x?"));
        // glob chars are literal
        assert!(wildcard_match("a.b", "a.b"));
        assert!(!wildcard_match("axb", "a.b"));
        // `dir *` matches both `dir` and `dir x`
        assert!(wildcard_match("dir x", "dir *"));
        assert!(wildcard_match("dir", "dir *"));
        // `*` spans separators (gitignore-free semantics)
        assert!(wildcard_match("rm -rf /tmp/x", "rm *"));
    }

    #[test]
    fn evaluate_find_last_and_default() {
        let rs = vec![
            rule("task", "*", PermissionV1Action::Allow),
            rule("task", "explore", PermissionV1Action::Deny),
        ];
        // findLast: the later deny wins over the earlier allow.
        assert_eq!(
            evaluate("task", "explore", &[&rs]).action,
            PermissionV1Action::Deny
        );
        assert_eq!(
            evaluate("task", "other", &[&rs]).action,
            PermissionV1Action::Allow
        );
        // later rulesets are scanned later by findLast, so they win.
        let rs1 = vec![rule("task", "*", PermissionV1Action::Deny)];
        let rs2 = vec![rule("task", "*", PermissionV1Action::Allow)];
        assert_eq!(
            evaluate("task", "explore", &[&rs1, &rs2]).action,
            PermissionV1Action::Allow
        );
        // no match -> ask
        assert_eq!(
            evaluate("bash", "rm *", &[]).action,
            PermissionV1Action::Ask
        );
        assert_eq!(evaluate("bash", "rm *", &[]).pattern, "*");
    }
}
