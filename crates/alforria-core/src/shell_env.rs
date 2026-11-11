//! The environment of the processes the agent drives (alforria extension).
//!
//! Upstream hands its children the server's whole environment. On a server
//! that holds provider keys and its own password in environment variables,
//! that makes `env` in the bash tool a way to read them. [`ShellEnvPolicy`]
//! decides what a child sees instead:
//!
//! * shells — the bash tool, the session `shell` command, `` !`cmd` ``
//!   in command templates, terminals and worktree start scripts — get
//!   [`ShellEnvPolicy::shell_env`]: the `inherit` selection, minus the denied
//!   names, plus `set`;
//! * helpers the server starts — MCP stdio servers, language servers and
//!   the commands that probe for them, git — get
//!   [`ShellEnvPolicy::scrubbed_env`]: everything minus the denied names, then
//!   their own configured environment on top. A project config can name a
//!   command, so it must not be a way around the deny list either.
//!
//! What is denied, matched without regard to case: the host's `deny`; the
//! built-in names and patterns ([`BUILTIN_DENY`], [`CREDENTIAL_PATTERNS`]);
//! and the names the server learns at startup that it reads a secret from
//! ([`ShellEnvPolicy::deny_names`]: every `provider.*.env[]` of the model
//! catalog and of the host config, every `{env:NAME}` the host config
//! refers to). `allow` releases a name from all but the host's own `deny`.
//!
//! What a shell inherits by default depends on the server:
//!
//! * standalone — everything that is not denied;
//! * with bearer auth ([`ShellEnvPolicy::restricted`]) — only
//!   [`RESTRICTED_INHERIT`] and what `allow` names. Nobody is at the
//!   terminal of such a server, and no list of secret-looking names is
//!   ever complete.
//!
//! The policy comes from `shell_env` in the host config
//! (`config::host::load_host_config`) and is process-wide: [`install`] it
//! once at startup; until then [`current`] is the standalone default.
//!
//! This is not a sandbox. A child running as the server's user can still
//! read `/proc/<server pid>/environ` and the server's files; keeping those
//! from it takes a different user (`shell_run_as`) or keeping secrets out of
//! the server's environment in the first place.

use std::collections::{BTreeMap, BTreeSet};
use std::ffi::OsString;
use std::sync::{Arc, RwLock};

use crate::config::host::{ShellEnvInfo, ShellEnvInherit};

/// Names denied to every child unless explicitly allowed: the server's own
/// secrets and the variables model providers read their keys from.
pub const BUILTIN_DENY: &[&str] = &[
    // The server's own.
    "OPENCODE_SERVER_PASSWORD",
    "OPENCODE_SERVER_USERNAME",
    "OPENCODE_AUTH_CONTENT",
    "OPENCODE_CONFIG_CONTENT",
    "OPENCODE_API_KEY",
    "ALFORRIA_*_SECRET",
    "ALFORRIA_*_TOKEN",
    "ALFORRIA_*_KEY",
    "ALFORRIA_*_PASSWORD",
    // Model providers.
    "ANTHROPIC_API_KEY",
    "ANTHROPIC_AUTH_TOKEN",
    "OPENAI_API_KEY",
    "AZURE_API_KEY",
    "AZURE_OPENAI_API_KEY",
    "GOOGLE_GENERATIVE_AI_API_KEY",
    "GEMINI_API_KEY",
    "OPENROUTER_API_KEY",
    "GROQ_API_KEY",
    "MISTRAL_API_KEY",
    "XAI_API_KEY",
    "DEEPSEEK_API_KEY",
    "TOGETHER_API_KEY",
    "TOGETHER_AI_API_KEY",
    "FIREWORKS_API_KEY",
    "CEREBRAS_API_KEY",
    "PERPLEXITY_API_KEY",
    "COHERE_API_KEY",
    "DEEPINFRA_API_KEY",
    "MOONSHOT_API_KEY",
    "ZHIPU_API_KEY",
    "ZAI_API_KEY",
    "NOVITA_API_KEY",
    "NEBIUS_API_KEY",
    "VENICE_API_KEY",
    "LIBERTAI_API_KEY",
    "AWS_BEARER_TOKEN_BEDROCK",
    // Search backends the server calls itself.
    "EXA_API_KEY",
    "PARALLEL_API_KEY",
];

/// Names that look like a credential, denied like [`BUILTIN_DENY`]: the
/// usual suffixes, and the variables the cloud SDKs and database clients
/// read theirs from. The agent's own tools (`aws`, `gh`, `gcloud`) read
/// these too; a host that wants the agent to have one names it in `allow`.
///
/// A guess by name, and so incomplete by nature: a secret called
/// `DATABASE_URL` or `SSH_AUTH_SOCK` is not on it.
pub const CREDENTIAL_PATTERNS: &[&str] = &[
    "*_TOKEN",
    "*_KEY",
    "*_SECRET",
    "*_PASSWORD",
    "*_PASSWD",
    "*_PASSPHRASE",
    "*_CREDENTIALS",
    "*_CREDENTIAL",
    "*_PAT",
    "*_ACCESS_KEY_ID",
    "*_CONNECTION_STRING",
    // Cloud SDKs.
    "AWS_*",
    "AZURE_*",
    "GOOGLE_APPLICATION_CREDENTIALS",
    "GOOGLE_CLOUD_KEYFILE_JSON",
    "CLOUDSDK_AUTH_*",
    // Database clients.
    "PGPASSWORD",
    "MYSQL_PWD",
];

/// What a shell inherits by default on a server with bearer auth
/// ([`ShellEnvPolicy::restricted`]), `allow` aside. Never denied on
/// account of a `{env:}` reference or a provider's `env` either: a shell
/// without `PATH` is no shell.
pub const RESTRICTED_INHERIT: &[&str] = &[
    "PATH", "HOME", "LANG", "LC_*", "TERM", "TZ", "USER", "LOGNAME", "SHELL", "TMPDIR",
];

#[derive(Debug, Clone, PartialEq, Eq)]
enum Inherit {
    All,
    None,
    /// Names or `*` / `?` patterns.
    List(Vec<String>),
}

/// What a child process may see of the server's environment.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct ShellEnvPolicy {
    /// `shell_env.inherit`; `None` when the host did not say, and the
    /// default depends on `restricted`.
    inherit: Option<Inherit>,
    /// Bearer auth is configured: a shell inherits [`RESTRICTED_INHERIT`]
    /// and `allow` unless the host set `inherit`.
    restricted: bool,
    /// Exact names exempt from every deny but the host's own `deny`.
    allow: Vec<String>,
    set: BTreeMap<String, String>,
    /// The host's `deny` patterns, upper-cased.
    deny: Vec<String>,
    /// Names learned at startup ([`ShellEnvPolicy::deny_names`]),
    /// upper-cased.
    named: BTreeSet<String>,
}

fn is_literal(name: &str) -> bool {
    !name.contains(['*', '?'])
}

/// Whether `name` matches a `*` / `?` pattern, case-sensitively. Written
/// out rather than compiled: this runs for every variable against every
/// pattern each time a child is spawned.
pub fn glob_match(name: &str, pattern: &str) -> bool {
    let (name, pattern) = (name.as_bytes(), pattern.as_bytes());
    let (mut n, mut p) = (0, 0);
    let mut star: Option<(usize, usize)> = None;
    while n < name.len() {
        if p < pattern.len() && (pattern[p] == b'?' || pattern[p] == name[n]) {
            n += 1;
            p += 1;
        } else if p < pattern.len() && pattern[p] == b'*' {
            star = Some((p, n));
            p += 1;
        } else if let Some((star_p, star_n)) = star {
            p = star_p + 1;
            n = star_n + 1;
            star = Some((star_p, star_n + 1));
        } else {
            return false;
        }
    }
    pattern[p..].iter().all(|byte| *byte == b'*')
}

fn matches_any(name: &str, patterns: &[impl AsRef<str>]) -> bool {
    patterns
        .iter()
        .any(|pattern| glob_match(name, pattern.as_ref()))
}

impl ShellEnvPolicy {
    /// The policy for a `shell_env` config value (`None`: the default) on
    /// a standalone server.
    pub fn from_config(info: Option<&ShellEnvInfo>) -> ShellEnvPolicy {
        let Some(info) = info else {
            return ShellEnvPolicy::default();
        };
        let inherit = info.inherit.as_ref().map(|inherit| match inherit {
            ShellEnvInherit::All => Inherit::All,
            ShellEnvInherit::None => Inherit::None,
            ShellEnvInherit::Names(names) => Inherit::List(names.clone()),
        });
        // A name spelled out in `inherit` was asked for on purpose: it is
        // exempt from the built-in deny list, like one in `allow`.
        let mut allow = info.allow.clone().unwrap_or_default();
        if let Some(Inherit::List(names)) = &inherit {
            allow.extend(names.iter().filter(|name| is_literal(name)).cloned());
        }
        ShellEnvPolicy {
            inherit,
            restricted: false,
            allow,
            set: info.set.clone().unwrap_or_default(),
            deny: info
                .deny
                .iter()
                .flatten()
                .map(|pattern| pattern.to_ascii_uppercase())
                .collect(),
            named: BTreeSet::new(),
        }
    }

    /// The same policy on a server with bearer auth (`restricted`): a
    /// shell inherits [`RESTRICTED_INHERIT`] and `allow` instead of
    /// everything, unless the host set `inherit` itself.
    pub fn restricted(mut self, restricted: bool) -> ShellEnvPolicy {
        self.restricted = restricted;
        self
    }

    /// Deny `names` too, whatever their case: the variables the server
    /// reads a secret from (a provider's `env`, a `{env:NAME}` in the host
    /// config). [`RESTRICTED_INHERIT`] is left alone.
    pub fn deny_names<S: AsRef<str>>(&mut self, names: impl IntoIterator<Item = S>) {
        for name in names {
            let name = name.as_ref();
            if !name.is_empty() && !matches_any(name, RESTRICTED_INHERIT) {
                self.named.insert(name.to_ascii_uppercase());
            }
        }
    }

    fn allowed(&self, name: &str) -> bool {
        self.allow.iter().any(|allowed| allowed == name)
    }

    /// Whether `name` is withheld from every child. Deny patterns match
    /// without regard to case (`github_token` is `GITHUB_TOKEN`); `allow`
    /// names a variable exactly.
    pub fn denied(&self, name: &str) -> bool {
        let upper = name.to_ascii_uppercase();
        if matches_any(&upper, &self.deny) {
            return true;
        }
        !self.allowed(name)
            && (matches_any(&upper, BUILTIN_DENY)
                || matches_any(&upper, CREDENTIAL_PATTERNS)
                || self.named.contains(&upper))
    }

    /// Whether `${name}` in a base URL the host wrote stays unexpanded:
    /// the host's `deny` and [`BUILTIN_DENY`] only. A catalog URL is built
    /// from the very variables [`ShellEnvPolicy::denied`] keeps from a
    /// shell (`${AZURE_RESOURCE_NAME}`, `${AWS_REGION}`), so this is the
    /// narrower question.
    pub fn withheld_from_urls(&self, name: &str) -> bool {
        let upper = name.to_ascii_uppercase();
        matches_any(&upper, &self.deny)
            || (matches_any(&upper, BUILTIN_DENY) && !self.allowed(name))
    }

    fn inherited(&self, name: &str) -> bool {
        match &self.inherit {
            Some(Inherit::All) => true,
            Some(Inherit::None) => false,
            Some(Inherit::List(names)) => matches_any(name, names),
            None if self.restricted => matches_any(name, RESTRICTED_INHERIT) || self.allowed(name),
            None => true,
        }
    }

    /// The environment of a shell, computed from `base`: the `inherit`
    /// selection minus the denied names, then `set`.
    pub fn shell_env_from(
        &self,
        base: impl IntoIterator<Item = (OsString, OsString)>,
    ) -> Vec<(OsString, OsString)> {
        let mut env: Vec<(OsString, OsString)> = base
            .into_iter()
            .filter(|(name, _)| {
                let name = name.to_string_lossy();
                self.inherited(&name) && !self.denied(&name) && !self.set.contains_key(&*name)
            })
            .collect();
        env.extend(
            self.set
                .iter()
                .map(|(name, value)| (OsString::from(name), OsString::from(value))),
        );
        env
    }

    /// The environment of a config-named helper, computed from `base`:
    /// everything except the denied names.
    pub fn scrubbed_env_from(
        &self,
        base: impl IntoIterator<Item = (OsString, OsString)>,
    ) -> Vec<(OsString, OsString)> {
        base.into_iter()
            .filter(|(name, _)| !self.denied(&name.to_string_lossy()))
            .collect()
    }

    /// [`ShellEnvPolicy::shell_env_from`] the server's environment.
    pub fn shell_env(&self) -> Vec<(OsString, OsString)> {
        self.shell_env_from(std::env::vars_os())
    }

    /// [`ShellEnvPolicy::scrubbed_env_from`] the server's environment.
    pub fn scrubbed_env(&self) -> Vec<(OsString, OsString)> {
        self.scrubbed_env_from(std::env::vars_os())
    }
}

static CURRENT: RwLock<Option<Arc<ShellEnvPolicy>>> = RwLock::new(None);

/// Make `policy` the process-wide policy.
pub fn install(policy: ShellEnvPolicy) {
    *CURRENT.write().unwrap_or_else(|p| p.into_inner()) = Some(Arc::new(policy));
}

/// The process-wide policy: the installed one, else the default.
pub fn current() -> Arc<ShellEnvPolicy> {
    CURRENT
        .read()
        .unwrap_or_else(|p| p.into_inner())
        .clone()
        .unwrap_or_default()
}

/// Give `command` the shell environment of the current policy.
pub fn apply_shell(command: &mut std::process::Command) {
    command.env_clear().envs(current().shell_env());
}

/// Give `command` the scrubbed environment of the current policy.
pub fn apply_scrubbed(command: &mut std::process::Command) {
    command.env_clear().envs(current().scrubbed_env());
}

#[cfg(test)]
mod tests {
    use super::*;

    fn base() -> Vec<(OsString, OsString)> {
        [
            ("PATH", "/usr/bin"),
            ("HOME", "/home/agent"),
            ("LC_ALL", "C"),
            ("OPENCODE_SERVER_PASSWORD", "hunter2"),
            ("ANTHROPIC_API_KEY", "sk-ant"),
            ("ALFORRIA_WEBHOOK_SECRET", "whsec"),
            ("AWS_ACCESS_KEY_ID", "AKIA"),
            ("MY_APP_TOKEN", "t0k"),
            ("EDITOR", "vi"),
        ]
        .into_iter()
        .map(|(name, value)| (OsString::from(name), OsString::from(value)))
        .collect()
    }

    fn policy(value: serde_json::Value) -> ShellEnvPolicy {
        policy_of(value, false)
    }

    fn policy_of(value: serde_json::Value, restricted: bool) -> ShellEnvPolicy {
        let info: ShellEnvInfo = serde_json::from_value(value).unwrap();
        ShellEnvPolicy::from_config(Some(&info)).restricted(restricted)
    }

    fn names(env: &[(OsString, OsString)]) -> Vec<String> {
        let mut names: Vec<String> = env
            .iter()
            .map(|(name, _)| name.to_string_lossy().into_owned())
            .collect();
        names.sort();
        names
    }

    fn value(env: &[(OsString, OsString)], name: &str) -> Option<String> {
        env.iter()
            .find(|(key, _)| key == name)
            .map(|(_, value)| value.to_string_lossy().into_owned())
    }

    #[test]
    fn globs_match_names() {
        for (name, pattern, expected) in [
            ("PATH", "PATH", true),
            ("PATH", "PATHS", false),
            ("PATHS", "PATH", false),
            ("LC_ALL", "LC_*", true),
            ("LC_", "LC_*", true),
            ("LC", "LC_*", false),
            ("ALFORRIA_WEBHOOK_SECRET", "ALFORRIA_*_SECRET", true),
            ("ALFORRIA_SECRET", "ALFORRIA_*_SECRET", false),
            ("ALFORRIA_A_B_SECRET", "ALFORRIA_*_SECRET", true),
            ("ALFORRIA_X_SECRETS", "ALFORRIA_*_SECRET", false),
            ("ABC", "A?C", true),
            ("AC", "A?C", false),
            ("anything", "*", true),
            ("", "*", true),
            ("path", "PATH", false),
            ("A.B", "A?B", true),
            ("AXB", "A.B", false),
        ] {
            assert_eq!(glob_match(name, pattern), expected, "{name} ~ {pattern}");
        }
    }

    /// Building a child's environment must stay cheap: it happens on every
    /// tool call.
    #[test]
    fn resolving_an_environment_is_fast() {
        let base: Vec<(OsString, OsString)> = (0..500)
            .map(|i| (OsString::from(format!("VAR_{i}")), OsString::from("v")))
            .collect();
        let started = std::time::Instant::now();
        for _ in 0..20 {
            let env = ShellEnvPolicy::default().shell_env_from(base.clone());
            assert_eq!(env.len(), 500);
        }
        assert!(
            started.elapsed() < std::time::Duration::from_secs(2),
            "took {:?}",
            started.elapsed()
        );
    }

    #[test]
    fn the_default_inherits_everything_but_the_builtin_deny_list() {
        let env = ShellEnvPolicy::default().shell_env_from(base());
        assert_eq!(names(&env), ["EDITOR", "HOME", "LC_ALL", "PATH"]);
        assert_eq!(ShellEnvPolicy::from_config(None), ShellEnvPolicy::default());
    }

    #[test]
    fn none_passes_only_what_is_set() {
        let env = policy(serde_json::json!({ "inherit": "none", "set": { "TERM": "dumb" } }))
            .shell_env_from(base());
        assert_eq!(names(&env), ["TERM"]);
    }

    #[test]
    fn a_list_inherits_names_and_patterns() {
        let env = policy(serde_json::json!({ "inherit": ["PATH", "LC_*"] })).shell_env_from(base());
        assert_eq!(names(&env), ["LC_ALL", "PATH"]);
    }

    #[test]
    fn a_builtin_denied_name_needs_naming_exactly() {
        // A pattern that happens to cover a secret does not release it.
        let env = policy(serde_json::json!({ "inherit": ["*"] })).shell_env_from(base());
        assert!(!names(&env).contains(&"ANTHROPIC_API_KEY".to_string()));
        assert!(!names(&env).contains(&"OPENCODE_SERVER_PASSWORD".to_string()));

        let env = policy(serde_json::json!({ "inherit": ["PATH", "ANTHROPIC_API_KEY"] }))
            .shell_env_from(base());
        assert_eq!(names(&env), ["ANTHROPIC_API_KEY", "PATH"]);

        let env =
            policy(serde_json::json!({ "allow": ["ANTHROPIC_API_KEY"] })).shell_env_from(base());
        assert_eq!(value(&env, "ANTHROPIC_API_KEY").as_deref(), Some("sk-ant"));
        assert_eq!(value(&env, "OPENCODE_SERVER_PASSWORD"), None);
    }

    #[test]
    fn deny_beats_inherit_and_allow_and_set_beats_everything() {
        let env = policy(serde_json::json!({
            "inherit": ["PATH", "MY_APP_TOKEN", "ANTHROPIC_API_KEY"],
            "allow": ["ANTHROPIC_API_KEY"],
            "deny": ["MY_*", "ANTHROPIC_*"],
            "set": { "PATH": "/opt/bin", "OPENCODE_SERVER_PASSWORD": "decoy" }
        }))
        .shell_env_from(base());
        assert_eq!(names(&env), ["OPENCODE_SERVER_PASSWORD", "PATH"]);
        assert_eq!(value(&env, "PATH").as_deref(), Some("/opt/bin"));
        assert_eq!(
            value(&env, "OPENCODE_SERVER_PASSWORD").as_deref(),
            Some("decoy")
        );
    }

    #[test]
    fn helpers_keep_everything_but_the_denied_names() {
        let policy = policy(serde_json::json!({ "inherit": "none", "deny": ["MY_*"] }));
        let env = policy.scrubbed_env_from(base());
        assert_eq!(names(&env), ["EDITOR", "HOME", "LC_ALL", "PATH"]);
    }

    #[test]
    fn the_builtin_list_covers_the_server_and_provider_secrets() {
        let policy = ShellEnvPolicy::default();
        for name in [
            "OPENCODE_SERVER_PASSWORD",
            "OPENCODE_AUTH_CONTENT",
            "OPENCODE_CONFIG_CONTENT",
            "ALFORRIA_WEBHOOK_SECRET",
            "ALFORRIA_SERVER_TOKEN",
            "ANTHROPIC_API_KEY",
            "OPENAI_API_KEY",
            "GEMINI_API_KEY",
            "LIBERTAI_API_KEY",
        ] {
            assert!(policy.denied(name), "{name} must be denied");
        }
        for name in [
            "PATH",
            "HOME",
            "EDITOR",
            "SSH_AUTH_SOCK",
            "KEYBOARD",
            "MONKEY",
        ] {
            assert!(!policy.denied(name), "{name} must pass");
        }
    }

    fn env(pairs: &[(&str, &str)]) -> Vec<(OsString, OsString)> {
        pairs
            .iter()
            .map(|(name, value)| (OsString::from(name), OsString::from(value)))
            .collect()
    }

    /// What a server holds besides its model keys: none of it is the
    /// agent's by default. `ACME_GATEWAY_AUTH` stands for a provider's
    /// `env` name and `ACME_REFERENCED` for a `{env:}` in the host config —
    /// names no pattern would catch.
    fn credentials() -> Vec<(OsString, OsString)> {
        env(&[
            ("PATH", "/usr/bin"),
            ("HOME", "/home/agent"),
            ("EDITOR", "vi"),
            ("CARGO_HOME", "/home/agent/.cargo"),
            ("GITHUB_TOKEN", "ghp-leak"),
            ("github_token", "ghp-lower-leak"),
            ("AWS_SECRET_ACCESS_KEY", "aws-leak"),
            ("aws_secret_access_key", "aws-lower-leak"),
            ("AWS_PROFILE", "prod"),
            ("HF_TOKEN", "hf-leak"),
            ("Hf_Token", "hf-mixed-leak"),
            ("GOOGLE_APPLICATION_CREDENTIALS", "/secrets/gcp.json"),
            ("AZURE_CLIENT_ID", "azure-leak"),
            ("DB_PASSWORD", "db-leak"),
            ("PGPASSWORD", "pg-leak"),
            ("ACME_GATEWAY_AUTH", "provider-leak"),
            ("acme_gateway_auth", "provider-lower-leak"),
            ("ACME_REFERENCED", "referenced-leak"),
            ("acme_referenced", "referenced-lower-leak"),
        ])
    }

    fn with_names(mut policy: ShellEnvPolicy) -> ShellEnvPolicy {
        policy.deny_names(["ACME_GATEWAY_AUTH", "acme_referenced"]);
        policy
    }

    /// M3: `GITHUB_TOKEN`, `AWS_SECRET_ACCESS_KEY`, `HF_TOKEN`, a custom
    /// provider's key variable, a variable the host config reads through
    /// `{env:}` and their lower-case spellings all reached the agent's
    /// shell.
    #[test]
    fn credentials_do_not_reach_a_standalone_shell() {
        let policy = with_names(ShellEnvPolicy::default());
        let passed = ["CARGO_HOME", "EDITOR", "HOME", "PATH"];
        assert_eq!(names(&policy.shell_env_from(credentials())), passed);
        // Nor a helper a config names (an MCP server, a language server).
        assert_eq!(names(&policy.scrubbed_env_from(credentials())), passed);
    }

    #[test]
    fn allow_lets_a_named_credential_through() {
        let policy = with_names(policy(serde_json::json!({
            "allow": ["GITHUB_TOKEN", "ACME_GATEWAY_AUTH", "ACME_REFERENCED", "AWS_PROFILE"]
        })));
        // Exactly the names written: not their other spellings, and not
        // the rest of a pattern's family.
        assert_eq!(
            names(&policy.shell_env_from(credentials())),
            [
                "ACME_GATEWAY_AUTH",
                "ACME_REFERENCED",
                "AWS_PROFILE",
                "CARGO_HOME",
                "EDITOR",
                "GITHUB_TOKEN",
                "HOME",
                "PATH"
            ]
        );
        // The host's own `deny` is the one thing `allow` does not lift,
        // and it too matches whatever the case.
        let policy = with_names(policy_of(
            serde_json::json!({ "allow": ["GITHUB_TOKEN"], "deny": ["github_*", "editor"] }),
            false,
        ));
        assert_eq!(
            names(&policy.shell_env_from(credentials())),
            ["CARGO_HOME", "HOME", "PATH"]
        );
    }

    /// With bearer auth the default is an allowlist: what is not on it
    /// does not pass, whatever it is called.
    #[test]
    fn a_restricted_server_passes_only_the_allowlist() {
        let mut base = credentials();
        base.extend(env(&[
            ("LANG", "C.UTF-8"),
            ("LC_ALL", "C"),
            ("TERM", "xterm"),
            ("TZ", "UTC"),
            ("USER", "agent"),
            ("LOGNAME", "agent"),
            ("SHELL", "/bin/sh"),
            ("TMPDIR", "/tmp"),
            ("SSH_AUTH_SOCK", "/run/agent.sock"),
            ("DATABASE_URL", "postgres://user:url-leak@db/app"),
            ("INNOCUOUS", "but-not-listed"),
        ]));
        let basics = [
            "HOME", "LANG", "LC_ALL", "LOGNAME", "PATH", "SHELL", "TERM", "TMPDIR", "TZ", "USER",
        ];
        let policy = with_names(ShellEnvPolicy::default().restricted(true));
        assert_eq!(names(&policy.shell_env_from(base.clone())), basics);
        assert_eq!(
            with_names(ShellEnvPolicy::from_config(None).restricted(true)),
            policy
        );

        // The host adds to it with `allow` — a credential too — and `set`
        // and `deny` work as they do anywhere.
        let policy = with_names(policy_of(
            serde_json::json!({
                "allow": ["EDITOR", "GITHUB_TOKEN"],
                "deny": ["TZ"],
                "set": { "TERM": "dumb" }
            }),
            true,
        ));
        let env = policy.shell_env_from(base.clone());
        let mut expected: Vec<&str> = basics
            .iter()
            .copied()
            .filter(|name| *name != "TZ")
            .collect();
        expected.extend(["EDITOR", "GITHUB_TOKEN"]);
        expected.sort();
        assert_eq!(names(&env), expected);
        assert_eq!(value(&env, "TERM").as_deref(), Some("dumb"));

        // A host that sets `inherit` has said what it wants; the deny list
        // still holds.
        let policy = with_names(policy_of(serde_json::json!({ "inherit": "all" }), true));
        assert_eq!(
            names(&policy.shell_env_from(credentials())),
            ["CARGO_HOME", "EDITOR", "HOME", "PATH"]
        );
        let policy = policy_of(
            serde_json::json!({ "inherit": ["PATH", "INNOCUOUS"] }),
            true,
        );
        assert_eq!(names(&policy.shell_env_from(base)), ["INNOCUOUS", "PATH"]);
    }

    /// A host config that reads `{env:HOME}` must not cost the shell its
    /// `HOME`.
    #[test]
    fn a_learned_name_never_denies_the_basics() {
        let mut policy = ShellEnvPolicy::default();
        policy.deny_names(["HOME", "PATH", "LC_ALL", "TMPDIR", "", "OTHER"]);
        for name in ["HOME", "PATH", "LC_ALL", "TMPDIR"] {
            assert!(!policy.denied(name), "{name} must pass");
        }
        assert!(policy.denied("OTHER"));
        assert!(policy.denied("other"));
    }

    /// A catalog base URL is built from variables a shell does not get;
    /// only the secrets themselves stay out of a URL.
    #[test]
    fn a_base_url_is_refused_secrets_only() {
        let mut policy = policy(serde_json::json!({ "deny": ["MY_*"] }));
        policy.deny_names(["AZURE_RESOURCE_NAME", "CLOUDFLARE_ACCOUNT_ID"]);
        for name in ["AZURE_RESOURCE_NAME", "CLOUDFLARE_ACCOUNT_ID", "AWS_REGION"] {
            assert!(policy.denied(name), "{name} is kept from a shell");
            assert!(!policy.withheld_from_urls(name), "{name} expands");
        }
        for name in [
            "OPENAI_API_KEY",
            "openai_api_key",
            "MY_HOST",
            "OPENCODE_SERVER_PASSWORD",
        ] {
            assert!(policy.withheld_from_urls(name), "{name} must not expand");
        }
    }
}
