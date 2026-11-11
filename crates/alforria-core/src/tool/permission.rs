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
/// This is the reading of *one* plain rule list — upstream's primitive, and
/// what a wire ruleset means. It knows nothing of ceilings: a decision
/// about what an agent may do is [`Policy::evaluate`], and nothing outside
/// this module reads an agent's rules any other way.
pub fn evaluate(permission: &str, pattern: &str, rulesets: &[&Ruleset]) -> PermissionV1Rule {
    last_match(permission, pattern, rulesets.iter().flat_map(|r| r.iter()))
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

/// One reading of the rules that the written ones may narrow and never
/// widen: the host's config alone, a trigger creator's bounds, …
#[derive(Debug, Clone, PartialEq)]
pub struct Ceiling {
    /// What it is, for messages (`"host"`).
    pub name: String,
    pub rules: Ruleset,
}

/// What an agent may do: the rules as every config wrote them, and the
/// ceilings over them. **The source of truth for a permission decision.**
///
/// Upstream has one last-match-wins list, and a project's
/// `agent.<name>.permission` is appended after the host's rules: a file the
/// agent can write overruled a host or managed `deny`. An intersection
/// cannot be written as one such list, and keeping it *inside* a list (as
/// marker rules) gave every reader of the list that did not know the
/// markers a different, more permissive answer. So the policy is a typed
/// value with one evaluation, [`Policy::evaluate`]: the strictest action —
/// deny over ask over allow — across the written rules and every ceiling.
/// A lower-trust config can narrow a permission; it can never widen one.
///
/// Where a wire format needs a flat ruleset, [`Policy::render`] produces
/// one that is never more permissive when read last-match-wins. It is a
/// rendering for clients, not what the server decides by.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct Policy {
    written: Ruleset,
    ceilings: Vec<Ceiling>,
}

impl From<Ruleset> for Policy {
    fn from(written: Ruleset) -> Policy {
        Policy::plain(written)
    }
}

impl Policy {
    /// A policy with no ceiling: upstream's reading of `written`.
    pub fn plain(written: Ruleset) -> Policy {
        Policy {
            written,
            ceilings: Vec::new(),
        }
    }

    /// Add a ceiling. One that is the written list itself caps nothing and
    /// is not kept — which is every install whose projects set no rules.
    pub fn with_ceiling(mut self, name: &str, rules: Ruleset) -> Policy {
        if rules != self.written && !self.ceilings.iter().any(|c| c.rules == rules) {
            self.ceilings.push(Ceiling {
                name: name.to_string(),
                rules,
            });
        }
        self
    }

    /// The rules as written (upstream's list).
    pub fn written(&self) -> &Ruleset {
        &self.written
    }

    pub fn ceilings(&self) -> &[Ceiling] {
        &self.ceilings
    }

    /// Append to the written rules only (the registry's own additions while
    /// it builds an agent). Ceilings are untouched.
    pub fn push_written(&mut self, rules: impl IntoIterator<Item = PermissionV1Rule>) {
        self.written.extend(rules);
    }

    /// This policy followed by `later` rules that count in every reading:
    /// a session's own rules, a standing approval. They are the operator's
    /// (or an answer to an ask), so they override a ceiling's rule the way
    /// they override a written one.
    pub fn extended(&self, later: &Ruleset) -> Policy {
        let mut policy = self.clone();
        policy.written.extend(later.iter().cloned());
        for ceiling in &mut policy.ceilings {
            ceiling.rules.extend(later.iter().cloned());
        }
        policy
    }

    /// This policy under further ceilings `over` it — another policy's
    /// every reading. The result allows only what both allow.
    pub fn capped_by(mut self, name: &str, over: &Policy) -> Policy {
        self = self.with_ceiling(name, over.written.clone());
        for ceiling in &over.ceilings {
            self = self.with_ceiling(&ceiling.name, ceiling.rules.clone());
        }
        self
    }

    /// **The** evaluation: the strictest of the written reading and every
    /// ceiling's. On a tie the written rule is returned.
    pub fn evaluate(&self, permission: &str, pattern: &str) -> PermissionV1Rule {
        let mut answer = last_match(permission, pattern, self.written.iter());
        for ceiling in &self.ceilings {
            // A ceiling that says nothing about the request does not
            // constrain it (the default `ask` is the written reading's).
            let capped = ceiling.rules.iter().rev().find(|rule| {
                wildcard_match(permission, &rule.permission)
                    && wildcard_match(pattern, &rule.pattern)
            });
            if let Some(capped) = capped {
                if strictness(capped.action) > strictness(answer.action) {
                    answer = capped.clone();
                }
            }
        }
        answer
    }

    /// `disabled` (permission/index.ts:204-211) over every reading: a tool
    /// one of them denies outright is not offered.
    pub fn disabled(&self, tools: &[&str]) -> std::collections::HashSet<String> {
        let mut out = disabled(tools, &self.written);
        for ceiling in &self.ceilings {
            out.extend(disabled(tools, &ceiling.rules));
        }
        out
    }

    /// The rules of every reading that concern `permission` — what a
    /// denial reports.
    pub fn rules_for(&self, permission: &str) -> Ruleset {
        self.written
            .iter()
            .chain(self.ceilings.iter().flat_map(|c| c.rules.iter()))
            .filter(|rule| wildcard_match(permission, &rule.permission))
            .cloned()
            .collect()
    }

    /// A flat ruleset for a wire format. Without ceilings it is the written
    /// list, byte for byte. With them it is the written list followed by
    /// every ceiling's `ask` rules, then every reading's `deny` rules: read
    /// last-match-wins it never allows what [`Policy::evaluate`] asks about
    /// or denies, and never asks about what it denies. It can be stricter
    /// (an allow that re-opens part of a deny is lost) — it is a rendering
    /// for display, never an input to a decision.
    pub fn render(&self) -> Ruleset {
        if self.ceilings.is_empty() {
            return self.written.clone();
        }
        let of = |rules: &Ruleset, action: PermissionV1Action| -> Vec<PermissionV1Rule> {
            rules
                .iter()
                .filter(|r| r.action == action)
                .cloned()
                .collect()
        };
        let mut flat = self.written.clone();
        // An `ask` anywhere must not be hidden by a written allow…
        flat.extend(of(&self.written, PermissionV1Action::Ask));
        for ceiling in &self.ceilings {
            flat.extend(of(&ceiling.rules, PermissionV1Action::Ask));
        }
        // …and a `deny` anywhere not by an ask or an allow.
        flat.extend(of(&self.written, PermissionV1Action::Deny));
        for ceiling in &self.ceilings {
            flat.extend(of(&ceiling.rules, PermissionV1Action::Deny));
        }
        flat
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
            let rule = ruleset
                .iter()
                .rev()
                .find(|rule| wildcard_match(permission, &rule.permission));
            matches!(rule, Some(rule) if rule.pattern == "*" && rule.action == PermissionV1Action::Deny)
        })
        .map(|tool| tool.to_string())
        .collect()
}

/// Generated policies and queries for the differential tests: every public
/// evaluation path must give [`Policy::evaluate`]'s answer for them, and
/// none may be more permissive than a ceiling. Deterministic (a small
/// linear congruential generator), so a failure reproduces.
pub mod differential {
    use super::*;

    const PERMISSIONS: [&str; 5] = ["bash", "edit", "read", "memory", "*"];
    const PATTERNS: [&str; 5] = ["*", "git *", "docs/*", "*.env", "remember"];
    const QUERIES: [(&str, &str); 8] = [
        ("bash", "git status"),
        ("bash", "rm -rf /"),
        ("edit", "docs/a.md"),
        ("edit", "src/a.rs"),
        ("read", "a.env"),
        ("read", "a.txt"),
        ("memory", "remember"),
        ("webfetch", "https://x.example"),
    ];

    fn rules(seed: &mut u64, max: u64) -> Ruleset {
        let mut next = || {
            *seed = seed
                .wrapping_mul(6364136223846793005)
                .wrapping_add(1442695040888963407);
            (*seed >> 33) as usize
        };
        let count = next() as u64 % (max + 1);
        (0..count)
            .map(|_| PermissionV1Rule {
                permission: PERMISSIONS[next() % PERMISSIONS.len()].to_string(),
                pattern: PATTERNS[next() % PATTERNS.len()].to_string(),
                action: [
                    PermissionV1Action::Allow,
                    PermissionV1Action::Ask,
                    PermissionV1Action::Deny,
                ][next() % 3],
            })
            .collect()
    }

    /// `(policy, queries)`: plain policies, one ceiling, two ceilings.
    pub fn cases() -> Vec<(Policy, Vec<(String, String)>)> {
        let queries: Vec<(String, String)> = QUERIES
            .iter()
            .map(|(permission, pattern)| (permission.to_string(), pattern.to_string()))
            .collect();
        let mut seed = 0x5eed_u64;
        (0..400)
            .map(|index| {
                let mut policy = Policy::plain(rules(&mut seed, 6));
                for ceiling in 0..(index % 3) {
                    policy = policy.with_ceiling(&format!("c{ceiling}"), rules(&mut seed, 6));
                }
                (policy, queries.clone())
            })
            .collect()
    }

    /// The strictest action any single reading of `policy` gives: what no
    /// evaluation path may be more permissive than.
    pub fn floor(policy: &Policy, permission: &str, pattern: &str) -> PermissionV1Action {
        let matched = |rules: &Ruleset| -> Option<PermissionV1Action> {
            rules
                .iter()
                .rev()
                .find(|rule| {
                    wildcard_match(permission, &rule.permission)
                        && wildcard_match(pattern, &rule.pattern)
                })
                .map(|rule| rule.action)
        };
        std::iter::once(evaluate(permission, pattern, &[policy.written()]).action)
            .chain(
                policy
                    .ceilings()
                    .iter()
                    .filter_map(|ceiling| matched(&ceiling.rules)),
            )
            .max_by_key(|action| strictness(*action))
            .expect("the written reading exists")
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

    /// The host's reading is a ceiling: a lower-trust allow never beats a
    /// host deny or ask, a lower-trust deny still narrows, and later rules
    /// count in every reading.
    #[test]
    fn a_policy_answers_with_the_strictest_reading() {
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
        let policy = Policy::plain(written.clone()).with_ceiling("host", host.clone());
        let action = |policy: &Policy, permission: &str, pattern: &str| {
            policy.evaluate(permission, pattern).action
        };
        // Upstream's reading, for contrast: the project wins.
        assert_eq!(
            action(&Policy::plain(written.clone()), "bash", "rm -rf /"),
            Allow
        );
        // Capped: the host's deny and ask stand, its own allow hole too.
        assert_eq!(action(&policy, "bash", "rm -rf /"), Deny);
        assert_eq!(action(&policy, "bash", "git status"), Allow);
        assert_eq!(action(&policy, "edit", "a.rs"), Ask);
        // The project's narrowing holds.
        assert_eq!(action(&policy, "read", "x.secret"), Deny);
        assert_eq!(action(&policy, "read", "x.txt"), Allow);
        // A session rule or a standing approval counts in every reading.
        let session = vec![rule("edit", "docs/*", Allow), rule("webfetch", "*", Deny)];
        let extended = policy.extended(&session);
        assert_eq!(action(&extended, "edit", "docs/a.md"), Allow);
        assert_eq!(action(&extended, "webfetch", "x"), Deny);
        assert_eq!(action(&extended, "bash", "rm -rf /"), Deny);
        // Tool disabling sees a bare deny in any reading.
        assert!(policy.disabled(&["task"]).contains("task"));
        assert!(!Policy::plain(written.clone())
            .disabled(&["task"])
            .contains("task"));
        // Nothing to cap: no ceiling is kept and the rendering is the list.
        let same = Policy::plain(host.clone()).with_ceiling("host", host.clone());
        assert!(same.ceilings().is_empty());
        assert_eq!(same.render(), host);
    }

    /// The differential: every way core reaches a decision about an agent
    /// gives the policy's answer — or, where a flat list is derived from it,
    /// one that is never more permissive — and the policy's answer is the
    /// strictest of its readings. (The V2 surface has the same test beside
    /// it, `routes::v2::util`.)
    #[test]
    fn every_core_evaluation_path_agrees_with_the_policy() {
        use crate::trigger::rules::{session_rules, RunRules};
        for (policy, queries) in differential::cases() {
            let flat = policy.render();
            let none = Ruleset::new();
            for (permission, pattern) in &queries {
                let decided = policy.evaluate(permission, pattern).action;
                // The one evaluation is the strictest reading.
                assert_eq!(decided, differential::floor(&policy, permission, pattern));
                // A session with no rules of its own, and the memory flush's
                // check (`agent.permission.extended(&own)`), change nothing.
                assert_eq!(
                    policy.extended(&none).evaluate(permission, pattern).action,
                    decided
                );
                // Tool-list filtering: a tool that is dropped is denied.
                if policy
                    .disabled(&[permission.as_str()])
                    .contains(permission.as_str())
                {
                    assert_eq!(decided, PermissionV1Action::Deny, "{permission} {pattern}");
                }
                // A trigger run's effective rules, built from the rendering:
                // never more permissive, trusted or carrying outside text.
                for untrusted in [false, true] {
                    let session = session_rules(&RunRules {
                        agent: &flat,
                        inherited: &flat,
                        own: &none,
                        untrusted,
                    });
                    let run = policy
                        .extended(&session)
                        .evaluate(permission, pattern)
                        .action;
                    assert!(
                        strictness(run) >= strictness(decided),
                        "trigger run (untrusted={untrusted}) {permission} {pattern}: \
                         {run:?} < {decided:?}\n{policy:#?}"
                    );
                }
            }
        }
    }

    /// The flat rendering is never more permissive than the policy, for
    /// every combination of small rule lists.
    #[test]
    fn the_rendering_is_at_least_as_strict_as_the_policy() {
        for (policy, queries) in differential::cases() {
            let flat = policy.render();
            for (permission, pattern) in &queries {
                let decided = policy.evaluate(permission, pattern).action;
                let rendered = evaluate(permission, pattern, &[&flat]).action;
                assert!(
                    strictness(rendered) >= strictness(decided),
                    "{permission} {pattern}: rendered {rendered:?} < decided {decided:?}\n{policy:#?}"
                );
            }
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
