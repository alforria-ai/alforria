//! The permission rules of a trigger's session.
//!
//! The session engine evaluates the agent's rules and then the session's,
//! and the last rule that matches wins. So a layer that is meant to narrow
//! cannot be a blanket rule appended at the end: `allow read *` after the
//! agent's `read *.env ask` would hand out exactly what the agent held back.
//!
//! Everything here is therefore built to narrow only. For a run that carries
//! text from outside, the result is the intersection of what the trigger
//! would otherwise be allowed and a read-only ceiling: for every permission
//! and pattern, the stricter of the two, and an `ask` nobody can answer is a
//! deny.

use alforria_schema::permission_v1::{PermissionV1Action, PermissionV1Rule, PermissionV1Ruleset};

use crate::tool::permission::wildcard_match;

/// The permissions a run carrying outside text keeps, to the extent its
/// agent and its creator's rules already allow them: the built-ins that only
/// read. Nothing else is reachable without an explicit grant in the
/// trigger's own `permission` block.
pub const READ_ONLY: [&str; 7] = [
    "read",
    "grep",
    "glob",
    "list",
    "lsp",
    "todowrite",
    // The stand-in the runtime answers a malformed tool call with.
    "invalid",
];

fn rule(permission: &str, pattern: &str, action: PermissionV1Action) -> PermissionV1Rule {
    PermissionV1Rule {
        permission: permission.to_string(),
        pattern: pattern.to_string(),
        action,
    }
}

/// `ask` on a run with nobody to ask is a deny.
fn unattended(action: PermissionV1Action) -> PermissionV1Action {
    match action {
        PermissionV1Action::Allow => PermissionV1Action::Allow,
        PermissionV1Action::Ask | PermissionV1Action::Deny => PermissionV1Action::Deny,
    }
}

/// What a trigger run is governed by.
pub struct RunRules<'a> {
    /// The rules of the agent the run uses, as they are now. The engine
    /// applies them before the session's.
    pub agent: &'a PermissionV1Ruleset,
    /// The effective rules of the session that scheduled the trigger, copied
    /// when it did (empty for a trigger from the config or the API).
    pub inherited: &'a PermissionV1Ruleset,
    /// The trigger's own `permission` block.
    pub own: &'a PermissionV1Ruleset,
    /// The run's prompt carries text from outside (a webhook body, the names
    /// of changed files).
    pub untrusted: bool,
}

/// The ruleset to put on the run's session.
///
/// Ordinarily that is the creator's rules, then the trigger's own block.
/// When both exist, everything the creator's rules withheld is withheld
/// again at the end, so the block cannot take an agent-scheduled trigger
/// past its creator.
///
/// For an untrusted run the ruleset then starts over from deny-everything
/// and rebuilds, in this order:
/// 1. for each read-only permission, the rules that would have decided it
///    anyway (the agent's, the creator's, the block's), in their order, with
///    `ask` turned into `deny`: the same answer as before, never a wider one;
/// 2. the trigger's own block, the only place a grant can come from;
/// 3. the creator's withholdings again, as above.
pub fn session_rules(input: &RunRules<'_>) -> PermissionV1Ruleset {
    let withheld = || {
        input
            .inherited
            .iter()
            .filter(|rule| rule.action != PermissionV1Action::Allow)
            .map(|held| rule(&held.permission, &held.pattern, PermissionV1Action::Deny))
    };
    let bounded = !input.inherited.is_empty() && !input.own.is_empty();
    let mut out = input.inherited.clone();
    out.extend(input.own.iter().cloned());
    if bounded {
        out.extend(withheld());
    }
    if !input.untrusted {
        return out;
    }
    out.push(rule("*", "*", PermissionV1Action::Deny));
    for permission in READ_ONLY {
        for decided in input
            .agent
            .iter()
            .chain(input.inherited.iter())
            .chain(input.own.iter())
            .filter(|decided| wildcard_match(permission, &decided.permission))
        {
            out.push(rule(
                permission,
                &decided.pattern,
                unattended(decided.action),
            ));
        }
    }
    out.extend(
        input
            .own
            .iter()
            .map(|own| rule(&own.permission, &own.pattern, unattended(own.action))),
    );
    if bounded {
        out.extend(withheld());
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tool::permission::{disabled, evaluate};
    use PermissionV1Action::{Allow, Ask, Deny};

    /// Rules shaped like the default agent's: allow, with care around
    /// secrets and around leaving the project.
    fn agent() -> PermissionV1Ruleset {
        vec![
            rule("*", "*", Allow),
            rule("external_directory", "*", Ask),
            rule("external_directory", "/data/tool-output/*", Allow),
            rule("read", "*", Allow),
            rule("read", "*.env", Ask),
            rule("read", "*.env.*", Ask),
            rule("read", "*.env.example", Allow),
            rule("question", "*", Allow),
        ]
    }

    /// What the engine decides: the agent's rules, then the session's.
    fn decide(
        agent: &PermissionV1Ruleset,
        session: &PermissionV1Ruleset,
        permission: &str,
        pattern: &str,
    ) -> PermissionV1Action {
        evaluate(permission, pattern, &[agent, session]).action
    }

    fn rules(
        agent: &PermissionV1Ruleset,
        inherited: &PermissionV1Ruleset,
        own: &PermissionV1Ruleset,
        untrusted: bool,
    ) -> PermissionV1Ruleset {
        session_rules(&RunRules {
            agent,
            inherited,
            own,
            untrusted,
        })
    }

    fn strictest(a: PermissionV1Action, b: PermissionV1Action) -> PermissionV1Action {
        match (a, b) {
            (Deny, _) | (_, Deny) => Deny,
            (Ask, _) | (_, Ask) => Ask,
            _ => Allow,
        }
    }

    const PROBES: [(&str, &str); 22] = [
        ("read", "src/lib.rs"),
        ("read", ".env"),
        ("read", "deploy/.env.production"),
        ("read", ".env.example"),
        ("read", "secrets/key.pem"),
        ("grep", "TODO"),
        ("glob", "**/*.rs"),
        ("list", "src"),
        ("todowrite", "*"),
        ("external_directory", "/etc/*"),
        ("external_directory", "/data/tool-output/x"),
        ("bash", "make deploy"),
        ("bash", "rm -rf /"),
        ("edit", "src/lib.rs"),
        ("webfetch", "https://example.com"),
        ("task", "general"),
        ("skill", "deploy"),
        ("schedule", "create"),
        ("memory", "remember"),
        ("question", "*"),
        ("github_create_issue", "*"),
        ("a_tool_added_next_year", "*"),
    ];

    #[test]
    fn an_untrusted_run_keeps_what_was_withheld_and_gains_nothing() {
        let agent = agent();
        let none = Vec::new();
        let session = rules(&agent, &none, &none, true);
        for (permission, pattern) in PROBES {
            let before = decide(&agent, &none, permission, pattern);
            let after = decide(&agent, &session, permission, pattern);
            // Never wider than without the ceiling, and never left to ask.
            assert_eq!(strictest(before, after), after, "{permission} {pattern}");
            assert_ne!(after, Ask, "{permission} {pattern}");
            let reads = READ_ONLY.contains(&permission);
            let expected = if reads && before == Allow {
                Allow
            } else {
                Deny
            };
            assert_eq!(after, expected, "{permission} {pattern}");
        }
        // The agent's `.env` ask did not become an allow.
        assert_eq!(decide(&agent, &session, "read", ".env"), Deny);
        assert_eq!(decide(&agent, &session, "read", ".env.example"), Allow);
        // Leaving the project is still refused, for read, grep and glob alike:
        // they all ask `external_directory`.
        assert_eq!(
            decide(&agent, &session, "external_directory", "/etc/*"),
            Deny
        );
        // Tools that are denied outright leave the request.
        let merged: PermissionV1Ruleset = agent.iter().chain(session.iter()).cloned().collect();
        let gone = disabled(
            &[
                "bash",
                "write",
                "apply_patch",
                "task",
                "mcp_tool",
                "question",
                "read",
                "grep",
            ],
            &merged,
        );
        assert_eq!(gone.len(), 6, "{gone:?}");
        assert!(!gone.contains("read") && !gone.contains("grep"));
    }

    #[test]
    fn a_read_the_creator_was_denied_stays_denied_on_webhook_and_watch_runs() {
        let agent = agent();
        // The session that scheduled the trigger could not read `secrets/`.
        let mut inherited = agent.clone();
        inherited.push(rule("read", "secrets/*", Deny));
        inherited.push(rule("grep", "*", Deny));
        let none = Vec::new();
        let session = rules(&agent, &inherited, &none, true);
        assert_eq!(decide(&agent, &session, "read", "secrets/key.pem"), Deny);
        assert_eq!(decide(&agent, &session, "grep", "TODO"), Deny);
        assert_eq!(decide(&agent, &session, "read", "src/lib.rs"), Allow);
        assert_eq!(decide(&agent, &session, "read", ".env"), Deny);
        // An agent-level deny holds the same way with nothing inherited.
        let mut strict_agent = agent.clone();
        strict_agent.push(rule("read", "private/*", Deny));
        strict_agent.push(rule("glob", "*", Ask));
        let session = rules(&strict_agent, &none, &none, true);
        assert_eq!(
            decide(&strict_agent, &session, "read", "private/notes"),
            Deny
        );
        assert_eq!(decide(&strict_agent, &session, "glob", "**"), Deny);
        // An agent that allows nothing gets nothing: there is no rule to copy.
        let session = rules(&none, &none, &none, true);
        for (permission, pattern) in PROBES {
            assert_eq!(decide(&none, &session, permission, pattern), Deny);
        }
    }

    #[test]
    fn a_grant_in_the_triggers_own_block_gives_exactly_that() {
        let agent = agent();
        let none = Vec::new();
        let own = vec![
            rule("bash", "make deploy", Allow),
            rule("notes_add", "*", Allow),
            rule("read", "src/generated/*", Deny),
            rule("webfetch", "*", Ask),
        ];
        let session = rules(&agent, &none, &own, true);
        assert_eq!(decide(&agent, &session, "bash", "make deploy"), Allow);
        assert_eq!(
            decide(&agent, &session, "bash", "make deploy --force"),
            Deny
        );
        assert_eq!(decide(&agent, &session, "bash", "rm -rf /"), Deny);
        assert_eq!(decide(&agent, &session, "notes_add", "x"), Allow);
        assert_eq!(decide(&agent, &session, "edit", "x"), Deny);
        // The block narrows too, and its `ask` cannot be answered.
        assert_eq!(decide(&agent, &session, "read", "src/generated/a.rs"), Deny);
        assert_eq!(decide(&agent, &session, "webfetch", "https://x"), Deny);
        assert_eq!(decide(&agent, &session, "read", ".env"), Deny);
        assert_eq!(
            decide(&agent, &session, "external_directory", "/etc/*"),
            Deny
        );
    }

    #[test]
    fn a_block_cannot_take_an_agent_scheduled_trigger_past_its_creator() {
        let agent = agent();
        let mut inherited = agent.clone();
        inherited.push(rule("bash", "*", Deny));
        inherited.push(rule("edit", "*", Ask));
        let own = vec![
            rule("bash", "*", Allow),
            rule("edit", "*", Allow),
            rule("webfetch", "*", Allow),
        ];
        for untrusted in [false, true] {
            let session = rules(&agent, &inherited, &own, untrusted);
            assert_eq!(decide(&agent, &session, "bash", "ls"), Deny, "{untrusted}");
            assert_eq!(decide(&agent, &session, "edit", "x"), Deny, "{untrusted}");
            // What the creator could do, the block may grant.
            assert_eq!(
                decide(&agent, &session, "webfetch", "https://x"),
                Allow,
                "{untrusted}"
            );
        }
    }

    /// One definition of "carries untrusted input": the mark on the session,
    /// inherited at creation however deep, and never cleared.
    #[tokio::test]
    async fn the_untrusted_mark_is_inherited_at_creation_and_is_sticky() {
        use crate::session::store::{CreateInput, SessionMark};
        use crate::session::test_support::{create_engine_session, engine};
        let e = engine("untrusted-mark", Vec::new());
        let sessions = &e.services.sessions;
        let context = e.services.instance_context(&e.worktree, None).unwrap();
        let child_of = |parent: &str, metadata: Option<alforria_schema::schema::JsonMap>| {
            sessions
                .create(
                    &context,
                    &CreateInput {
                        parent_id: Some(parent.to_string()),
                        metadata,
                        ..Default::default()
                    },
                )
                .unwrap()
        };
        // A trigger run, its subagent, and that subagent's subagent.
        let run = create_engine_session(&e);
        sessions.mark(&run.id, SessionMark::OutsideStart).unwrap();
        let sub = child_of(&run.id, None);
        let mut own = alforria_schema::schema::JsonMap::new();
        own.insert("workflowAgent".to_string(), serde_json::json!("general"));
        let subsub = child_of(&sub.id, Some(own));
        for id in [&run.id, &sub.id, &subsub.id] {
            assert!(sessions.started_by_outside_input(id), "{id}");
            assert!(sessions.seen_outside_content(id), "{id}");
        }
        assert_eq!(
            subsub.metadata.as_ref().unwrap()["workflowAgent"],
            "general"
        );
        // A fork copies the conversation, and the mark with it.
        let fork = sessions.fork(&context, &subsub.id, None).unwrap();
        assert!(sessions.started_by_outside_input(&fork.id));
        // Replacing the metadata cannot clear it.
        sessions
            .set_metadata(&subsub.id, alforria_schema::schema::JsonMap::new())
            .unwrap();
        assert!(sessions.started_by_outside_input(&subsub.id));
        // An ordinary session and its children are not marked.
        let plain = create_engine_session(&e);
        let plain_child = child_of(&plain.id, None);
        assert!(!sessions.started_by_outside_input(&plain.id));
        assert!(!sessions.started_by_outside_input(&plain_child.id));
        // What cannot be read is not vouched for.
        assert!(sessions.started_by_outside_input("ses_missing"));
        let lost = child_of("ses_missing", None);
        assert!(sessions.started_by_outside_input(&lost.id));
    }

    #[test]
    fn a_trusted_run_is_the_creators_rules_then_the_block() {
        let agent = agent();
        let none = Vec::new();
        assert!(rules(&agent, &none, &none, false).is_empty());
        let own = vec![rule("bash", "*", Deny)];
        assert_eq!(rules(&agent, &none, &own, false), own);
        let inherited = vec![rule("edit", "*", Deny)];
        assert_eq!(rules(&agent, &inherited, &none, false), inherited);
    }
}
