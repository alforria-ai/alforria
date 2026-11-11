//! Permission evaluation helpers used by the tool system.
//!
//! Port of `evaluate`/`fromConfig`/`merge` from
//! `packages/opencode/src/permission/index.ts` and `Wildcard.match` from
//! `packages/core/src/util/wildcard.ts`. Only the pure evaluation lives
//! here; the ask/answer flow (M5) is out of scope.

use std::path::Path;

use alforria_schema::permission_v1::{PermissionV1Action, PermissionV1Rule, PermissionV1Ruleset};

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
    match regex::Regex::new(&format!("(?s)^{escaped}$")) {
        Ok(re) => re.is_match(&normalized),
        Err(_) => false,
    }
}

/// `Permission.evaluate` (permission/index.ts:28-38): the last matching rule
/// across the flattened rulesets, or the default `ask` rule when nothing
/// matches.
///
/// alforria extension — the host's ceiling. A ruleset may hold a section
/// built by [`with_ceiling`]: the rules as every config wrote them, and the
/// rules as the *host's* config alone would have them. Upstream's
/// last-match-wins puts a project's `agent.<name>.permission` after the
/// host's rules, so a file the agent can write overruled a host or managed
/// `deny`. With a ceiling the answer is the stricter of the two readings
/// (deny over ask over allow): a lower-trust config can narrow what the
/// host allows and can never widen it. Rules outside the section — a
/// session's, a standing approval — count in both readings, as before.
pub fn evaluate(permission: &str, pattern: &str, rulesets: &[&Ruleset]) -> PermissionV1Rule {
    let has_ceiling = rulesets
        .iter()
        .any(|ruleset| ruleset.iter().any(|rule| ceiling_marker(rule).is_some()));
    if !has_ceiling {
        return last_match(permission, pattern, rulesets.iter().flat_map(|r| r.iter()));
    }
    // Two readings of the same list: one without the host's copy, one
    // without the lower-trust original.
    let reading = |skip: Section| {
        let mut section = Section::Outside;
        let mut kept = Vec::new();
        for rule in rulesets.iter().flat_map(|ruleset| ruleset.iter()) {
            match ceiling_marker(rule) {
                Some(next) => section = next,
                None if section != skip => kept.push(rule),
                None => {}
            }
        }
        last_match(permission, pattern, kept.into_iter())
    };
    let written = reading(Section::Host);
    let host = reading(Section::Written);
    if strictness(host.action) > strictness(written.action) {
        host
    } else {
        written
    }
}

fn last_match<'a>(
    permission: &str,
    pattern: &str,
    rules: impl DoubleEndedIterator<Item = &'a PermissionV1Rule>,
) -> PermissionV1Rule {
    for rule in rules.rev() {
        if wildcard_match(permission, &rule.permission) && wildcard_match(pattern, &rule.pattern) {
            return rule.clone();
        }
    }
    PermissionV1Rule {
        permission: permission.to_string(),
        pattern: "*".to_string(),
        action: PermissionV1Action::Ask,
    }
}

/// Deny over ask over allow.
pub fn strictness(action: PermissionV1Action) -> u8 {
    match action {
        PermissionV1Action::Allow => 0,
        PermissionV1Action::Ask => 1,
        PermissionV1Action::Deny => 2,
    }
}

/// The permission name of the marker rules that delimit a ceiling section.
/// No tool asks under it, so a reader that does not know the markers sees
/// three rules that never match.
pub const CEILING_PERMISSION: &str = "alforria:ceiling";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Section {
    Outside,
    /// The rules as every config wrote them.
    Written,
    /// The rules as the host's config alone has them.
    Host,
}

fn ceiling_marker(rule: &PermissionV1Rule) -> Option<Section> {
    if rule.permission != CEILING_PERMISSION {
        return None;
    }
    Some(match rule.pattern.as_str() {
        "written" => Section::Written,
        "host" => Section::Host,
        _ => Section::Outside,
    })
}

/// `written` under the ceiling `host`: the ruleset [`evaluate`] reads both
/// ways. When the two are the same list there is nothing to cap and
/// `written` is returned as it is — which is every install whose projects
/// set no permission rules.
pub fn with_ceiling(written: Ruleset, host: Ruleset) -> Ruleset {
    if written == host {
        return written;
    }
    let marker = |pattern: &str| PermissionV1Rule {
        permission: CEILING_PERMISSION.to_string(),
        pattern: pattern.to_string(),
        action: PermissionV1Action::Deny,
    };
    let mut ruleset = vec![marker("written")];
    ruleset.extend(written);
    ruleset.push(marker("host"));
    ruleset.extend(host);
    ruleset.push(marker("end"));
    ruleset
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

/// `disabled` (permission/index.ts:204-211): the tools whose `deny` rule is
/// a bare `*` pattern under the (remapped) permission key. `edits` tools
/// share the `edit` permission, MCP resource tools the `read` permission.
pub fn disabled(
    tools: &[&str],
    ruleset: &PermissionV1Ruleset,
) -> std::collections::HashSet<String> {
    const EDITS: [&str; 3] = ["edit", "write", "apply_patch"];
    const READS: [&str; 3] = [
        "list_mcp_resources",
        "list_mcp_resource_templates",
        "read_mcp_resource",
    ];
    tools
        .iter()
        .filter(|tool| {
            let tool = **tool;
            let permission = if EDITS.contains(&tool) {
                "edit"
            } else if READS.contains(&tool) {
                "read"
            } else {
                tool
            };
            // With a ceiling, either reading that ends in a bare deny
            // disables the tool.
            let denies = |skip: Section| {
                let mut section = Section::Outside;
                let mut found = None;
                for rule in ruleset {
                    match ceiling_marker(rule) {
                        Some(next) => section = next,
                        None if section != skip
                            && wildcard_match(permission, &rule.permission) =>
                        {
                            found = Some(rule)
                        }
                        None => {}
                    }
                }
                matches!(found, Some(rule) if rule.pattern == "*" && rule.action == PermissionV1Action::Deny)
            };
            denies(Section::Host) || denies(Section::Written)
        })
        .map(|tool| tool.to_string())
        .collect()
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

    /// The host's reading is a ceiling: a lower-trust allow never beats a
    /// host deny or ask, a lower-trust deny still narrows, and rules
    /// outside the section count in both readings.
    #[test]
    fn a_ceiling_is_the_stricter_of_the_two_readings() {
        use PermissionV1Action::{Allow, Ask, Deny};
        let host = vec![
            rule("*", "*", Allow),
            rule("bash", "*", Deny),
            rule("bash", "git status*", Allow),
            rule("edit", "*", Ask),
            rule("read", "*", Allow),
            rule("task", "*", Deny),
        ];
        // A project appended its own rules after the host's.
        let mut written = host.clone();
        written.extend([
            rule("bash", "*", Allow),
            rule("edit", "*", Allow),
            rule("read", "*.secret", Deny),
            rule("task", "*", Allow),
        ]);
        let capped = with_ceiling(written.clone(), host.clone());
        let action = |permission: &str, pattern: &str, rulesets: &[&Ruleset]| {
            evaluate(permission, pattern, rulesets).action
        };
        // Upstream's reading, for contrast: the project wins.
        assert_eq!(action("bash", "rm -rf /", &[&written]), Allow);
        // Capped: the host's deny and ask stand, its own allow hole too.
        assert_eq!(action("bash", "rm -rf /", &[&capped]), Deny);
        assert_eq!(action("bash", "git status", &[&capped]), Allow);
        assert_eq!(action("edit", "a.rs", &[&capped]), Ask);
        // The project's narrowing holds.
        assert_eq!(action("read", "x.secret", &[&capped]), Deny);
        assert_eq!(action("read", "x.txt", &[&capped]), Allow);
        // A session rule or a standing approval after the agent's rules
        // counts in both readings, flattened or not.
        let session = vec![rule("edit", "docs/*", Allow), rule("webfetch", "*", Deny)];
        assert_eq!(action("edit", "docs/a.md", &[&capped, &session]), Allow);
        assert_eq!(action("webfetch", "x", &[&capped, &session]), Deny);
        let flat = merge(&[&capped, &session]);
        assert_eq!(action("edit", "docs/a.md", &[&flat]), Allow);
        assert_eq!(action("bash", "rm -rf /", &[&flat]), Deny);
        // Tool disabling sees a bare deny in either reading.
        assert!(disabled(&["task"], &capped).contains("task"));
        assert!(!disabled(&["task"], &written).contains("task"));
        assert!(!disabled(&["edit"], &capped).contains("edit"));
        // Nothing to cap: the list is untouched.
        assert_eq!(with_ceiling(host.clone(), host.clone()), host);
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

    #[test]
    fn disabled_remaps_edits_and_reads() {
        let ruleset = vec![
            rule("edit", "*", PermissionV1Action::Deny),
            rule("read", "*", PermissionV1Action::Deny),
            rule("skill", "specific", PermissionV1Action::Deny),
        ];
        // write maps onto the edit permission; the bare `*` deny applies.
        assert_eq!(
            disabled(&["write"], &ruleset),
            ["write".to_string()].into_iter().collect()
        );
        // MCP resource tools map onto read.
        assert_eq!(
            disabled(&["read_mcp_resource"], &ruleset),
            ["read_mcp_resource".to_string()].into_iter().collect()
        );
        // A deny that is not `*` does not disable.
        assert!(disabled(&["skill"], &ruleset).is_empty());
        // Non-deny rules never disable.
        let allow_all = vec![rule("skill", "*", PermissionV1Action::Allow)];
        assert!(disabled(&["skill"], &allow_all).is_empty());
    }
}
