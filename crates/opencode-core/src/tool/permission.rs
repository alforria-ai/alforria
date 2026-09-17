//! Permission evaluation helpers used by the tool system.
//!
//! Port of `evaluate`/`fromConfig`/`merge` from
//! `packages/opencode/src/permission/index.ts` and `Wildcard.match` from
//! `packages/core/src/util/wildcard.ts`. Only the pure evaluation lives
//! here; the ask/answer flow (M5) is out of scope.

use std::path::Path;

use opencode_schema::permission_v1::{PermissionV1Action, PermissionV1Rule, PermissionV1Ruleset};

use crate::config::schema::{PermissionInfo, PermissionRule};

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

/// The config action enum is a decode-only twin of the wire action —
/// map it onto the wire type.
fn wire_action(action: crate::config::schema::PermissionAction) -> PermissionV1Action {
    match action {
        crate::config::schema::PermissionAction::Ask => PermissionV1Action::Ask,
        crate::config::schema::PermissionAction::Allow => PermissionV1Action::Allow,
        crate::config::schema::PermissionAction::Deny => PermissionV1Action::Deny,
    }
}

/// `expand` (permission/index.ts:178-184): `~/` and `$HOME` expansion
/// against the given home directory.
pub fn expand(pattern: &str, home: &Path) -> String {
    if let Some(rest) = pattern.strip_prefix("~/") {
        return format!("{}/{}", home.display(), rest);
    }
    if pattern == "~" {
        return home.display().to_string();
    }
    if let Some(rest) = pattern.strip_prefix("$HOME") {
        return match rest.strip_prefix('/') {
            Some(rest) => format!("{}/{}", home.display(), rest),
            None => format!("{}{}", home.display(), rest),
        };
    }
    pattern.to_string()
}

/// `fromConfig` (permission/index.ts:186-198): turn a config permission
/// object into a flat ruleset. A bare action string is
/// `{ permission: key, pattern: "*", action }`.
pub fn from_config(permission: &PermissionInfo, home: &Path) -> PermissionV1Ruleset {
    let mut ruleset = PermissionV1Ruleset::new();
    for (key, rule) in &permission.rules {
        match rule {
            PermissionRule::Action(action) => {
                ruleset.push(PermissionV1Rule {
                    permission: key.clone(),
                    pattern: "*".to_string(),
                    action: wire_action(*action),
                });
            }
            PermissionRule::Object(patterns) => {
                for (pattern, action) in patterns {
                    ruleset.push(PermissionV1Rule {
                        permission: key.clone(),
                        pattern: expand(pattern, home),
                        action: wire_action(*action),
                    });
                }
            }
        }
    }
    ruleset
}

/// `merge` (permission/index.ts:200-202): last-match-wins rulesets are
/// flattened in order — `evaluate` finds the last matching rule.
pub fn merge(rulesets: &[&PermissionV1Ruleset]) -> PermissionV1Ruleset {
    rulesets.iter().flat_map(|r| r.iter().cloned()).collect()
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

    #[test]
    fn expand_tilde_and_home() {
        let home = Path::new("/home/user");
        assert_eq!(expand("~/a/b", home), "/home/user/a/b");
        assert_eq!(expand("~", home), "/home/user");
        assert_eq!(expand("$HOME/x", home), "/home/user/x");
        assert_eq!(expand("$HOME", home), "/home/user");
        assert_eq!(expand("$HOMEy", home), "/home/usery");
        assert_eq!(expand("/elsewhere", home), "/elsewhere");
    }

    #[test]
    fn from_config_bare_action_and_patterns() {
        let value = serde_json::json!({
            "bash": "allow",
            "read": {"*.env": "ask", "~/secret/*": "deny"},
        });
        let info: PermissionInfo = serde_json::from_value(value).unwrap();
        let home = Path::new("/home/user");
        let ruleset = from_config(&info, home);
        // A bare action is `pattern: "*"`; pattern objects expand in order.
        assert_eq!(
            ruleset,
            vec![
                rule("bash", "*", PermissionV1Action::Allow),
                rule("read", "*.env", PermissionV1Action::Ask),
                rule("read", "/home/user/secret/*", PermissionV1Action::Deny),
            ]
        );
    }

    #[test]
    fn merge_flattens_in_order() {
        let first = vec![rule("bash", "*", PermissionV1Action::Deny)];
        let second = vec![rule("bash", "*", PermissionV1Action::Allow)];
        let merged = merge(&[&first, &second]);
        assert_eq!(merged.len(), 2);
        // evaluate resolves last-match-wins, so the later allow wins.
        assert_eq!(
            evaluate("bash", "ls", &[&merged]).action,
            PermissionV1Action::Allow
        );
    }
}
