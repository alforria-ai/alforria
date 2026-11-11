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
//! * helpers the config names — MCP stdio servers, language servers — get
//!   [`ShellEnvPolicy::scrubbed_env`]: everything minus the denied names, then
//!   their own configured environment on top. A project config can name a
//!   command, so it must not be a way around the deny list either.
//!
//! The policy comes from `shell_env` in the host config
//! (`config::host::load_host_config`) and is process-wide: [`install`] it
//! once at startup; until then [`current`] is the default, which inherits
//! everything except [`BUILTIN_DENY`].
//!
//! This is not a sandbox. A child running as the server's user can still
//! read `/proc/<server pid>/environ` and the server's files; keeping those
//! from it takes a different user (`shell_run_as`) or keeping secrets out of
//! the server's environment in the first place.

use std::collections::BTreeMap;
use std::ffi::OsString;
use std::sync::{Arc, RwLock};

use crate::config::host::{ShellEnvInfo, ShellEnvInherit};

/// Names denied to every child unless explicitly allowed: the server's own
/// secrets and the variables model providers read their keys from.
///
/// General-purpose credentials (`AWS_*`, `GITHUB_TOKEN`, `GOOGLE_*`, …) are
/// deliberately absent: the agent's own tools (`aws`, `gh`, `gcloud`) need
/// them, and whether the agent should have them is the operator's call.
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

#[derive(Debug, Clone, PartialEq, Eq)]
enum Inherit {
    All,
    None,
    /// Names or `*` / `?` patterns.
    List(Vec<String>),
}

/// What a child process may see of the server's environment.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ShellEnvPolicy {
    inherit: Inherit,
    /// Names exempt from [`BUILTIN_DENY`] (never from `deny`).
    allow: Vec<String>,
    set: BTreeMap<String, String>,
    deny: Vec<String>,
}

impl Default for ShellEnvPolicy {
    fn default() -> Self {
        ShellEnvPolicy {
            inherit: Inherit::All,
            allow: Vec::new(),
            set: BTreeMap::new(),
            deny: Vec::new(),
        }
    }
}

fn is_literal(name: &str) -> bool {
    !name.contains(['*', '?'])
}

/// Whether `name` matches a `*` / `?` pattern, case-sensitively. Written
/// out rather than compiled: this runs for every variable against every
/// pattern each time a child is spawned.
fn glob_match(name: &str, pattern: &str) -> bool {
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
    /// The policy for a `shell_env` config value (`None`: the default).
    pub fn from_config(info: Option<&ShellEnvInfo>) -> ShellEnvPolicy {
        let Some(info) = info else {
            return ShellEnvPolicy::default();
        };
        let inherit = match &info.inherit {
            None | Some(ShellEnvInherit::All) => Inherit::All,
            Some(ShellEnvInherit::None) => Inherit::None,
            Some(ShellEnvInherit::Names(names)) => Inherit::List(names.clone()),
        };
        // A name spelled out in `inherit` was asked for on purpose: it is
        // exempt from the built-in deny list, like one in `allow`.
        let mut allow = info.allow.clone().unwrap_or_default();
        if let Inherit::List(names) = &inherit {
            allow.extend(names.iter().filter(|name| is_literal(name)).cloned());
        }
        ShellEnvPolicy {
            inherit,
            allow,
            set: info.set.clone().unwrap_or_default(),
            deny: info.deny.clone().unwrap_or_default(),
        }
    }

    /// Whether `name` is withheld from every child.
    pub fn denied(&self, name: &str) -> bool {
        if matches_any(name, &self.deny) {
            return true;
        }
        matches_any(name, BUILTIN_DENY) && !self.allow.iter().any(|allowed| allowed == name)
    }

    fn inherited(&self, name: &str) -> bool {
        match &self.inherit {
            Inherit::All => true,
            Inherit::None => false,
            Inherit::List(names) => matches_any(name, names),
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
        ]
        .into_iter()
        .map(|(name, value)| (OsString::from(name), OsString::from(value)))
        .collect()
    }

    fn policy(value: serde_json::Value) -> ShellEnvPolicy {
        let info: ShellEnvInfo = serde_json::from_value(value).unwrap();
        ShellEnvPolicy::from_config(Some(&info))
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
        assert_eq!(
            names(&env),
            [
                "AWS_ACCESS_KEY_ID",
                "HOME",
                "LC_ALL",
                "MY_APP_TOKEN",
                "PATH"
            ]
        );
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
        assert_eq!(names(&env), ["AWS_ACCESS_KEY_ID", "HOME", "LC_ALL", "PATH"]);
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
        for name in ["PATH", "HOME", "GITHUB_TOKEN", "AWS_SECRET_ACCESS_KEY"] {
            assert!(!policy.denied(name), "{name} must pass");
        }
    }
}
