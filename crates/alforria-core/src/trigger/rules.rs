//! The permission rules of a trigger's run.
//!
//! A run is judged by one evaluation (`Policy::evaluate`) over the
//! strictest of four things:
//!
//! (a) the policy in force **when the run happens** for its agent and its
//!     session — the agent's rules as the configs write them now, then the
//!     trigger's own `permission` block as the session's rules;
//! (b) the **creator's bounds**, recorded when a session's `schedule` tool
//!     created the trigger: a named, sealed ceiling ([`CREATOR_CEILING`]);
//! (c) the **outside-input ceiling** ([`OUTSIDE_CEILING`]) when outside
//!     input started the run: read-only, plus what the trigger's own block
//!     grants;
//! (d) the host's ceiling, which the agent's policy already carries.
//!
//! (b) and (c) are stored for the run's session (`session_ceiling`) and
//! read back wherever a turn's policy is assembled
//! (`SessionStore::bounded`). Nothing here is replayed into a flat list
//! after the agent's rules: an operator who tightens the policy after a
//! trigger was scheduled binds its next run through (a), and one who
//! loosens it does not lift the run past (b).

use alforria_schema::permission_v1::{PermissionV1Action, PermissionV1Rule, PermissionV1Ruleset};
use serde::{Deserialize, Serialize};

use crate::session::store::SessionCeiling;

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

/// The ceiling that holds a trigger creator's bounds.
pub const CREATOR_CEILING: &str = "trigger-creator";
/// The read-only ceiling of a run outside input started.
pub const OUTSIDE_CEILING: &str = "outside-input";

/// What the session that scheduled a trigger could do when it did: every
/// reading of its policy (the written rules with the session's own, and
/// each ceiling over them), so that the bound is that policy's own
/// strictest-of and not a flattening of it. Triggers stored before the
/// readings were kept hold one flat list, which is one reading.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum CreatorBounds {
    Readings(Vec<PermissionV1Ruleset>),
    Flat(PermissionV1Ruleset),
}

impl Default for CreatorBounds {
    fn default() -> CreatorBounds {
        CreatorBounds::Readings(Vec::new())
    }
}

impl CreatorBounds {
    /// Every reading of `policy`. The written reading starts with the
    /// default every evaluation falls back to (`ask`), so that what the
    /// creator would have been asked about is not something a later, wider
    /// policy allows the run outright.
    pub fn of(policy: &crate::tool::permission::Policy) -> CreatorBounds {
        let mut written = vec![rule("*", "*", PermissionV1Action::Ask)];
        written.extend(policy.written().iter().cloned());
        let mut readings = vec![written];
        readings.extend(
            policy
                .ceilings()
                .iter()
                .map(|ceiling| ceiling.rules.clone()),
        );
        CreatorBounds::Readings(readings)
    }

    pub fn readings(&self) -> Vec<PermissionV1Ruleset> {
        match self {
            CreatorBounds::Readings(readings) => readings.clone(),
            CreatorBounds::Flat(rules) if rules.is_empty() => Vec::new(),
            CreatorBounds::Flat(rules) => vec![rules.clone()],
        }
    }

    pub fn is_empty(&self) -> bool {
        self.readings().iter().all(|reading| reading.is_empty())
    }
}

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

/// What a trigger run is governed by, beyond the policy in force when it
/// runs.
pub struct RunRules<'a> {
    /// The creator's bounds (empty for a trigger from the config or the
    /// API).
    pub inherited: &'a CreatorBounds,
    /// The trigger's own `permission` block.
    pub own: &'a PermissionV1Ruleset,
    /// Outside input started the run (a webhook body, the names of changed
    /// files).
    pub untrusted: bool,
}

/// The rules and ceilings to put on the run's session.
pub struct RunPolicy {
    /// The session's own rules: the trigger's `permission` block, read
    /// after the agent's rules as any session's are.
    pub session: PermissionV1Ruleset,
    /// Sealed ceilings over whatever policy is then in force.
    pub ceilings: Vec<SessionCeiling>,
}

/// The outside-input ceiling: everything denied, the read-only built-ins
/// left to the policy in force (an `allow` here never widens it), and the
/// trigger's own block — the only place a grant can come from — with `ask`
/// turned into `deny`.
pub fn outside_ceiling(own: &PermissionV1Ruleset) -> PermissionV1Ruleset {
    let mut out = vec![rule("*", "*", PermissionV1Action::Deny)];
    out.extend(
        READ_ONLY
            .iter()
            .map(|permission| rule(permission, "*", PermissionV1Action::Allow)),
    );
    out.extend(
        own.iter()
            .map(|own| rule(&own.permission, &own.pattern, unattended(own.action))),
    );
    out
}

/// What to store for a run's session.
///
/// The creator's bounds become one sealed ceiling per reading. When the
/// trigger also has a block of its own, everything a reading withheld is
/// withheld again at its end as a deny, so the block's presence never turns
/// a creator's `ask` into something the run may be allowed.
pub fn run_policy(input: &RunRules<'_>) -> RunPolicy {
    let mut ceilings = Vec::new();
    let readings: Vec<PermissionV1Ruleset> = input
        .inherited
        .readings()
        .into_iter()
        .filter(|reading| !reading.is_empty())
        .map(|mut reading| {
            if !input.own.is_empty() {
                let withheld: Vec<PermissionV1Rule> = reading
                    .iter()
                    .filter(|held| held.action != PermissionV1Action::Allow)
                    .map(|held| rule(&held.permission, &held.pattern, PermissionV1Action::Deny))
                    .collect();
                reading.extend(withheld);
            }
            reading
        })
        .collect();
    if !readings.is_empty() {
        ceilings.push(SessionCeiling {
            name: CREATOR_CEILING.to_string(),
            readings,
        });
    }
    if input.untrusted {
        ceilings.push(SessionCeiling {
            name: OUTSIDE_CEILING.to_string(),
            readings: vec![outside_ceiling(input.own)],
        });
    }
    RunPolicy {
        session: input.own.clone(),
        ceilings,
    }
}

#[cfg(test)]
impl RunPolicy {
    /// The policy a turn of the run is judged by, given the agent's policy
    /// as it stands: what `SessionStore::policy` assembles from what is
    /// stored.
    pub(crate) fn over(
        &self,
        agent: &crate::tool::permission::Policy,
    ) -> crate::tool::permission::Policy {
        self.ceilings
            .iter()
            .fold(agent.extended_for_test(&self.session), |policy, ceiling| {
                ceiling.readings.iter().fold(policy, |policy, reading| {
                    policy.with_sealed_ceiling(&ceiling.name, reading.clone())
                })
            })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tool::permission::Policy;
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

    fn run(
        inherited: &PermissionV1Ruleset,
        own: &PermissionV1Ruleset,
        untrusted: bool,
    ) -> RunPolicy {
        run_policy(&RunRules {
            inherited: &CreatorBounds::Flat(inherited.clone()),
            own,
            untrusted,
        })
    }

    /// What the engine decides for the run: the one evaluation, and — for
    /// a run outside input started — the permission service's "an ask is a
    /// deny at once".
    fn decide(
        agent: &PermissionV1Ruleset,
        run: &RunPolicy,
        permission: &str,
        pattern: &str,
    ) -> PermissionV1Action {
        let action = run
            .over(&Policy::plain(agent.clone()))
            .evaluate(permission, pattern)
            .action;
        let outside = run.ceilings.iter().any(|c| c.name == OUTSIDE_CEILING);
        if outside {
            unattended(action)
        } else {
            action
        }
    }

    fn plain(agent: &PermissionV1Ruleset, permission: &str, pattern: &str) -> PermissionV1Action {
        Policy::plain(agent.clone())
            .evaluate(permission, pattern)
            .action
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
        let run = run(&none, &none, true);
        for (permission, pattern) in PROBES {
            let before = plain(&agent, permission, pattern);
            let after = decide(&agent, &run, permission, pattern);
            let reads = READ_ONLY.contains(&permission);
            let expected = if reads && before == Allow {
                Allow
            } else {
                Deny
            };
            assert_eq!(after, expected, "{permission} {pattern}");
        }
        // The agent's `.env` ask did not become an allow.
        assert_eq!(decide(&agent, &run, "read", ".env"), Deny);
        assert_eq!(decide(&agent, &run, "read", ".env.example"), Allow);
        assert_eq!(decide(&agent, &run, "external_directory", "/etc/*"), Deny);
        // Tools that are denied outright leave the request.
        let gone = run.over(&Policy::plain(agent.clone())).disabled(&[
            "bash",
            "write",
            "apply_patch",
            "task",
            "mcp_tool",
            "question",
            "read",
            "grep",
        ]);
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
        let bound = run(&inherited, &none, true);
        assert_eq!(decide(&agent, &bound, "read", "secrets/key.pem"), Deny);
        assert_eq!(decide(&agent, &bound, "grep", "TODO"), Deny);
        assert_eq!(decide(&agent, &bound, "read", "src/lib.rs"), Allow);
        assert_eq!(decide(&agent, &bound, "read", ".env"), Deny);
        // An agent-level deny holds the same way with nothing inherited.
        let mut strict_agent = agent.clone();
        strict_agent.push(rule("read", "private/*", Deny));
        strict_agent.push(rule("glob", "*", Ask));
        let bound = run(&none, &none, true);
        assert_eq!(decide(&strict_agent, &bound, "read", "private/notes"), Deny);
        assert_eq!(decide(&strict_agent, &bound, "glob", "**"), Deny);
        // An agent that allows nothing gets nothing.
        for (permission, pattern) in PROBES {
            assert_eq!(decide(&none, &bound, permission, pattern), Deny);
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
        let bound = run(&none, &own, true);
        assert_eq!(decide(&agent, &bound, "bash", "make deploy"), Allow);
        assert_eq!(decide(&agent, &bound, "bash", "make deploy --force"), Deny);
        assert_eq!(decide(&agent, &bound, "bash", "rm -rf /"), Deny);
        assert_eq!(decide(&agent, &bound, "notes_add", "x"), Allow);
        assert_eq!(decide(&agent, &bound, "edit", "x"), Deny);
        // The block narrows too, and its `ask` cannot be answered.
        assert_eq!(decide(&agent, &bound, "read", "src/generated/a.rs"), Deny);
        assert_eq!(decide(&agent, &bound, "webfetch", "https://x"), Deny);
        assert_eq!(decide(&agent, &bound, "read", ".env"), Deny);
        assert_eq!(decide(&agent, &bound, "external_directory", "/etc/*"), Deny);
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
            let bound = run(&inherited, &own, untrusted);
            assert_eq!(decide(&agent, &bound, "bash", "ls"), Deny, "{untrusted}");
            assert_eq!(decide(&agent, &bound, "edit", "x"), Deny, "{untrusted}");
            // What the creator could do, the block may grant.
            assert_eq!(
                decide(&agent, &bound, "webfetch", "https://x"),
                Allow,
                "{untrusted}"
            );
        }
    }

    /// H5. The creator's bounds are a ceiling, not a replay: the policy in
    /// force when the run happens binds it, and so do the bounds.
    #[test]
    fn a_run_is_bound_by_the_policy_now_and_by_its_creators_bounds() {
        let none = Vec::new();
        // Scheduled while the agent allowed everything.
        let at_creation = agent();
        let bound = run(&at_creation, &none, false);
        assert_eq!(decide(&at_creation, &bound, "bash", "make deploy"), Allow);

        // The operator tightens: bash denied, edits asked about, `.env`
        // reads denied. The next run is bound by that. (The snapshot used
        // to be replayed after the agent's rules, and won.)
        let mut tightened = agent();
        tightened.push(rule("bash", "*", Deny));
        tightened.push(rule("edit", "*", Ask));
        tightened.push(rule("read", "*.env", Deny));
        assert_eq!(decide(&tightened, &bound, "bash", "make deploy"), Deny);
        assert_eq!(decide(&tightened, &bound, "edit", "src/lib.rs"), Ask);
        assert_eq!(decide(&tightened, &bound, "read", ".env"), Deny);
        assert_eq!(decide(&tightened, &bound, "read", "src/lib.rs"), Allow);
        assert!(bound
            .over(&Policy::plain(tightened.clone()))
            .disabled(&["bash"])
            .contains("bash"));

        // Scheduled by a session that could not run bash or read `.env`;
        // the operator loosens afterwards. The run stays within the bounds.
        let mut narrow = agent();
        narrow.push(rule("bash", "*", Deny));
        narrow.push(rule("read", "*.env", Deny));
        let bound = run(&narrow, &none, false);
        let loosened = vec![rule("*", "*", Allow)];
        assert_eq!(decide(&loosened, &bound, "bash", "make deploy"), Deny);
        assert_eq!(decide(&loosened, &bound, "read", ".env"), Deny);
        assert_eq!(decide(&loosened, &bound, "edit", "src/lib.rs"), Allow);
        // A standing approval, or a rule added to the session later, does
        // not reach a sealed ceiling.
        let approved = vec![rule("bash", "*", Allow)];
        assert_eq!(
            bound
                .over(&Policy::plain(loosened.clone()))
                .with_approvals(&approved)
                .evaluate("bash", "make deploy")
                .action,
            Deny
        );
    }

    /// The bounds keep every reading of the creator's policy: a host
    /// ceiling over the creator is a ceiling over the run, not flattened.
    #[test]
    fn the_creators_bounds_are_every_reading_of_its_policy() {
        let creator = Policy::plain(vec![rule("*", "*", Allow), rule("bash", "git *", Allow)])
            .with_ceiling(
                "host",
                vec![
                    rule("*", "*", Allow),
                    rule("bash", "*", Deny),
                    rule("bash", "git status", Allow),
                ],
            );
        let bounds = CreatorBounds::of(&creator);
        assert_eq!(bounds.readings().len(), 2);
        let bound = run_policy(&RunRules {
            inherited: &bounds,
            own: &Vec::new(),
            untrusted: false,
        });
        let now = Policy::plain(vec![rule("*", "*", Allow)]);
        for (pattern, expected) in [("git status", Allow), ("git push", Deny), ("ls", Deny)] {
            assert_eq!(
                bound.over(&now).evaluate("bash", pattern).action,
                expected,
                "{pattern}"
            );
            assert_eq!(creator.evaluate("bash", pattern).action, expected);
        }
        // Stored and read back, in both the old and the new shape.
        let stored = serde_json::to_value(&bounds).unwrap();
        assert_eq!(
            serde_json::from_value::<CreatorBounds>(stored).unwrap(),
            bounds
        );
        let legacy =
            serde_json::json!([{ "permission": "bash", "pattern": "*", "action": "deny" }]);
        assert_eq!(
            serde_json::from_value::<CreatorBounds>(legacy).unwrap(),
            CreatorBounds::Flat(vec![rule("bash", "*", Deny)])
        );
    }

    /// The marks are inherited at creation, however deep, and are sticky.
    #[tokio::test]
    async fn the_marks_are_inherited_at_creation_and_are_sticky() {
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
        let fork = sessions.fork(&context, &subsub.id, None).unwrap();
        assert!(sessions.started_by_outside_input(&fork.id));
        sessions
            .set_metadata(&subsub.id, alforria_schema::schema::JsonMap::new())
            .unwrap();
        assert!(sessions.started_by_outside_input(&subsub.id));
        let plain = create_engine_session(&e);
        let plain_child = child_of(&plain.id, None);
        assert!(!sessions.started_by_outside_input(&plain.id));
        assert!(!sessions.started_by_outside_input(&plain_child.id));
        assert!(sessions.started_by_outside_input("ses_missing"));
        let lost = child_of("ses_missing", None);
        assert!(sessions.started_by_outside_input(&lost.id));
    }

    #[test]
    fn a_trusted_run_of_an_operators_trigger_has_the_block_and_no_ceiling() {
        let none = Vec::new();
        let bound = run(&none, &none, false);
        assert!(bound.session.is_empty() && bound.ceilings.is_empty());
        let own = vec![rule("bash", "*", Deny)];
        let bound = run(&none, &own, false);
        assert_eq!(bound.session, own);
        assert!(bound.ceilings.is_empty());
    }
}
