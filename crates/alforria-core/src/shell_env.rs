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
//!   the commands that probe for them, git, the desktop's URL opener — get
//!   [`ShellEnvPolicy::scrubbed_env`], then their own configured
//!   environment on top. A project config can name a command, so it must
//!   not be a way around the policy either.
//!
//! There are two postures, and the host config decides which:
//!
//! * **standalone** (the default: a developer's own loopback server) — a
//!   child inherits everything except
//!   what is alforria's own to protect: the host's `deny`; the server's
//!   secrets and the provider key variables of [`BUILTIN_DENY`]; names
//!   shaped like an API key ([`API_KEY_PATTERNS`]); and the names the
//!   server learns it reads a key or a secret from
//!   ([`ShellEnvPolicy::learn`]: every `provider.*.env[]` of the model
//!   catalog and of the loaded configs, every `{env:NAME}` the host config
//!   refers to). `GH_TOKEN`, `AWS_*`, `NPM_TOKEN`, `PGPASSWORD` and the
//!   like are the user's, for the user's tools, and pass as they do
//!   upstream;
//! * **restricted** ([`ShellEnvPolicy::restricted`]: the server is
//!   unattended or reachable from another machine — bearer auth, a
//!   non-loopback bind, an unattended answer, a webhook, a `webhook` or
//!   `watch` trigger, or `server.trust.project: "restricted"`; the server
//!   crate's `ServerPolicy::unattended_or_remote`) — nobody is at the
//!   terminal of such a server. Names that look like a credential
//!   ([`CREDENTIAL_PATTERNS`]) are denied too, and a shell or a helper
//!   starts from [`RESTRICTED_INHERIT`] plus what `allow` names instead of
//!   from everything: no list of secret-looking names is ever complete.
//!
//! Deny patterns match without regard to case. `allow` releases an exact
//! name from all but the host's own `deny`; `deny` and `inherit` let a
//! host choose either posture for itself, whatever the mode.
//!
//! The policy comes from `shell_env` in the host config
//! (`config::host::load_host_config`) and is process-wide: [`install`] it
//! once at startup; until then [`current`] is the standalone default. The
//! learned names are kept per source and replaced through [`learn_at`]
//! each time that source is read again (a catalog read, an instance
//! boot), and in no other way: a disposed instance's names stay until its
//! directory boots again, because its processes can outlive it.
//!
//! This is not a sandbox. A child running as the server's user can still
//! read `/proc/<server pid>/environ` and the server's files; keeping those
//! from it takes running the server (or the child) as a different user,
//! which alforria does not do for you, or keeping secrets out of the
//! server's environment in the first place.

use std::collections::{BTreeMap, BTreeSet};
use std::ffi::OsString;
use std::sync::{Arc, RwLock};

use crate::config::host::{ShellEnvInfo, ShellEnvInherit};

/// Names denied to every child in either posture unless explicitly
/// allowed: the server's own secrets and the variables model providers
/// read their keys from.
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

/// Names shaped like an API key, denied in either posture like
/// [`BUILTIN_DENY`]: no list of model providers is complete, and keeping a
/// provider's key from the agent is alforria's own concern on a
/// developer's machine too. Narrow on purpose — `GITHUB_TOKEN`, `AWS_*`,
/// `NPM_TOKEN` and `PGPASSWORD` are not of this shape and pass on a
/// standalone install. `shell_env.allow` hands a named one over.
pub const API_KEY_PATTERNS: &[&str] = &["*_API_KEY", "*_API_TOKEN"];

/// Names that look like a credential, denied like [`BUILTIN_DENY`] on a
/// restricted server only ([`ShellEnvPolicy::restricted`]): the usual
/// suffixes, and the variables the cloud SDKs and database clients read
/// theirs from. The agent's own tools (`aws`, `gh`, `gcloud`) read these
/// too, which is why a standalone install passes them; a restricted host
/// that wants the agent to have one names it in `allow`, and a standalone
/// host that does not writes the pattern in `deny`.
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

/// What a shell or a helper inherits by default on a restricted server
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
    /// The restricted posture (an unattended or remotely reachable
    /// server): [`CREDENTIAL_PATTERNS`] are denied, and a child
    /// inherits [`RESTRICTED_INHERIT`] and `allow` unless the host set
    /// `inherit`.
    restricted: bool,
    /// Exact names exempt from every deny but the host's own `deny`.
    allow: Vec<String>,
    set: BTreeMap<String, String>,
    /// The host's `deny` patterns, upper-cased.
    deny: Vec<String>,
    /// Names the server reads a key or a secret from, upper-cased, by the
    /// source they were learned from ([`ShellEnvPolicy::learn_at`]).
    named: BTreeMap<String, Learned>,
}

/// What one source last had the policy withhold.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
struct Learned {
    /// The reading it comes from ([`next_generation`]): an older reading
    /// never replaces a newer one.
    generation: u64,
    names: BTreeSet<String>,
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
            named: BTreeMap::new(),
        }
    }

    /// The same policy in the restricted posture (`restricted`: the
    /// server is unattended or reachable from another machine):
    /// credential-like names are denied, and a child inherits
    /// [`RESTRICTED_INHERIT`] and `allow` instead of everything, unless the
    /// host set `inherit` itself.
    pub fn restricted(mut self, restricted: bool) -> ShellEnvPolicy {
        self.restricted = restricted;
        self
    }

    /// Whether this is the restricted posture.
    pub fn is_restricted(&self) -> bool {
        self.restricted
    }

    /// Whether the host set `shell_env.inherit` itself, so that the
    /// posture's default selection does not apply.
    pub fn inherit_configured(&self) -> bool {
        self.inherit.is_some()
    }

    fn learnable<S: AsRef<str>>(names: impl IntoIterator<Item = S>) -> BTreeSet<String> {
        names
            .into_iter()
            .filter(|name| {
                let name = name.as_ref();
                !name.is_empty() && !matches_any(name, RESTRICTED_INHERIT)
            })
            .map(|name| name.as_ref().to_ascii_uppercase())
            .collect()
    }

    /// Deny `names` too, whatever their case: the variables the server
    /// reads a secret from (a provider's `env`, a `{env:NAME}` in the host
    /// config). [`RESTRICTED_INHERIT`] is left alone. Added to what is
    /// already denied; a source that is read more than once goes through
    /// [`ShellEnvPolicy::learn`] instead.
    pub fn deny_names<S: AsRef<str>>(&mut self, names: impl IntoIterator<Item = S>) {
        self.named
            .entry(String::new())
            .or_default()
            .names
            .extend(Self::learnable(names));
    }

    /// Make `names` the whole of what `source` (a `SOURCE_*` name) has the
    /// server deny, replacing what it said before: a name a reloaded config
    /// or catalog no longer mentions is released, a new one is withheld.
    /// Returns whether the denied names changed. Otherwise as
    /// [`ShellEnvPolicy::deny_names`].
    ///
    /// `generation` orders the readings of one source
    /// ([`next_generation`], taken before the source is read): a reading
    /// older than the one already held is ignored, so a boot that finishes
    /// late cannot put back what a later boot replaced. Replacing is the
    /// only way a source's names go — nothing is forgotten because an
    /// instance was disposed, whose processes can outlive it.
    pub fn learn_at<S: AsRef<str>>(
        &mut self,
        source: &str,
        generation: u64,
        names: impl IntoIterator<Item = S>,
    ) -> bool {
        let names = Self::learnable(names);
        let known = self.named.entry(source.to_string()).or_default();
        if generation < known.generation {
            return false;
        }
        known.generation = generation;
        if known.names == names {
            return false;
        }
        known.names = names;
        true
    }

    /// [`ShellEnvPolicy::learn_at`] as a reading no older than the one
    /// held.
    pub fn learn<S: AsRef<str>>(
        &mut self,
        source: &str,
        names: impl IntoIterator<Item = S>,
    ) -> bool {
        let generation = self.named.get(source).map_or(0, |known| known.generation);
        self.learn_at(source, generation, names)
    }

    fn learned(&self, upper: &str) -> bool {
        self.named.values().any(|known| known.names.contains(upper))
    }

    fn allowed(&self, name: &str) -> bool {
        self.allow.iter().any(|allowed| allowed == name)
    }

    /// Whether `name` is withheld from every child: the host's `deny`,
    /// alforria's own secrets and the provider key variables
    /// ([`BUILTIN_DENY`], [`API_KEY_PATTERNS`], the learned names), and on a
    /// restricted server
    /// whatever looks like a credential ([`CREDENTIAL_PATTERNS`]). Deny
    /// patterns match without regard to case (`github_token` is
    /// `GITHUB_TOKEN`); `allow` names a variable exactly.
    pub fn denied(&self, name: &str) -> bool {
        let upper = name.to_ascii_uppercase();
        if matches_any(&upper, &self.deny) {
            return true;
        }
        !self.allowed(name)
            && (matches_any(&upper, BUILTIN_DENY)
                || matches_any(&upper, API_KEY_PATTERNS)
                || self.learned(&upper)
                || (self.restricted && matches_any(&upper, CREDENTIAL_PATTERNS)))
    }

    /// Whether `${name}` in a base URL the host wrote stays unexpanded:
    /// the host's `deny` and [`BUILTIN_DENY`] only. A catalog URL is built
    /// from the very variables [`ShellEnvPolicy::denied`] keeps from a
    /// shell (`${AZURE_RESOURCE_NAME}` is a provider `env` name,
    /// `${AWS_REGION}` a credential pattern), so this is the narrower
    /// question.
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

    /// What a helper starts from before the deny list. Standalone:
    /// everything, whatever `inherit` says of shells (git needs its
    /// `HOME`). Restricted: [`RESTRICTED_INHERIT`], `allow`, and what the
    /// host's own `inherit` adds — never less than the basics, without
    /// which no helper starts.
    fn helper_inherited(&self, name: &str) -> bool {
        if !self.restricted {
            return true;
        }
        matches_any(name, RESTRICTED_INHERIT)
            || self.allowed(name)
            || match &self.inherit {
                Some(Inherit::All) => true,
                Some(Inherit::List(names)) => matches_any(name, names),
                Some(Inherit::None) | None => false,
            }
    }

    /// The environment of a helper the server starts (an MCP stdio server,
    /// a language server or its probe, git), computed from `base`:
    /// everything except the denied names on a standalone server, the
    /// allowlist on a restricted one. The caller adds the helper's own
    /// configured environment on top.
    pub fn scrubbed_env_from(
        &self,
        base: impl IntoIterator<Item = (OsString, OsString)>,
    ) -> Vec<(OsString, OsString)> {
        base.into_iter()
            .filter(|(name, _)| {
                let name = name.to_string_lossy();
                self.helper_inherited(&name) && !self.denied(&name)
            })
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

/// [`learn`] source: the `provider.*.env[]` names of the model catalog.
pub const SOURCE_CATALOG: &str = "catalog";
/// [`learn`] source: the `provider.*.env[]` names of the host's own config.
pub const SOURCE_HOST_PROVIDERS: &str = "host-providers";
/// [`learn`] source: the `{env:NAME}` references of the host's own config
/// files.
pub const SOURCE_HOST_REFERENCES: &str = "host-references";

/// [`learn`] source: the `provider.*.env[]` names of the merged config of
/// the instance at `directory`.
pub fn source_instance(directory: &std::path::Path) -> String {
    format!("instance:{}", directory.display())
}

static CURRENT: RwLock<Option<Arc<ShellEnvPolicy>>> = RwLock::new(None);

/// Make `policy` the process-wide policy, with the names it carries and
/// no others.
pub fn install(policy: ShellEnvPolicy) {
    *CURRENT.write().unwrap_or_else(|p| p.into_inner()) = Some(Arc::new(policy));
}

static GENERATION: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);

/// A number greater than every one handed out before, in this process:
/// taken *before* a source is read, it says which of two readings is the
/// later one ([`learn_at`]).
pub fn next_generation() -> u64 {
    GENERATION.fetch_add(1, std::sync::atomic::Ordering::SeqCst)
}

/// [`ShellEnvPolicy::learn_at`] on the process-wide policy: called each
/// time `source` is read again, so that the names follow a config or
/// catalog reload instead of staying what they were at startup. The
/// comparison of generations and the replacement happen under one lock.
pub fn learn_at<S: AsRef<str>>(source: &str, generation: u64, names: impl IntoIterator<Item = S>) {
    let mut current = CURRENT.write().unwrap_or_else(|p| p.into_inner());
    let mut policy = current.as_deref().cloned().unwrap_or_default();
    // The generation is recorded even when the names are the same.
    policy.learn_at(source, generation, names);
    *current = Some(Arc::new(policy));
}

/// [`learn_at`] as a reading made now.
pub fn learn<S: AsRef<str>>(source: &str, names: impl IntoIterator<Item = S>) {
    learn_at(source, next_generation(), names);
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

/// Give `command` the helper environment of the current policy
/// ([`ShellEnvPolicy::scrubbed_env`]).
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
        assert_eq!(
            names(&env),
            [
                "AWS_ACCESS_KEY_ID",
                "EDITOR",
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
        assert_eq!(
            names(&env),
            ["AWS_ACCESS_KEY_ID", "EDITOR", "HOME", "LC_ALL", "PATH"]
        );
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

    /// What a server holds besides its model keys. `ACME_GATEWAY_AUTH`
    /// stands for a provider's `env` name and `ACME_REFERENCED` for a
    /// `{env:}` in the host config — names no pattern would catch.
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

    /// A standalone install — no bearer auth, no host switch — is the
    /// user's own machine: `gh`, `aws`, `npm publish`, `psql` and a
    /// token-reading MCP server find their credentials as they do
    /// upstream. Only what is alforria's own to protect is withheld: the
    /// server password, the bearer and webhook secrets, the config and
    /// auth it was handed through the environment, and the variables a
    /// provider's key is read from — built in, learned from a config or
    /// the catalog, or referred to by the host config — whatever the case
    /// they are spelled in.
    #[test]
    fn a_standalone_child_keeps_the_users_credentials_and_loses_alforrias_own() {
        let base = env(&[
            ("PATH", "/usr/bin"),
            ("HOME", "/home/agent"),
            ("EDITOR", "vi"),
            // The user's, for the user's tools.
            ("GH_TOKEN", "gh"),
            ("GITHUB_TOKEN", "github"),
            ("AWS_ACCESS_KEY_ID", "AKIA"),
            ("AWS_SECRET_ACCESS_KEY", "aws"),
            ("AWS_PROFILE", "prod"),
            ("AZURE_CLIENT_ID", "azure"),
            ("GOOGLE_APPLICATION_CREDENTIALS", "/secrets/gcp.json"),
            ("NPM_TOKEN", "npm"),
            ("PGPASSWORD", "pg"),
            ("DB_PASSWORD", "db"),
            ("SSH_AUTH_SOCK", "/run/agent.sock"),
            // alforria's own.
            ("OPENCODE_SERVER_PASSWORD", "password-leak"),
            ("opencode_server_password", "password-lower-leak"),
            ("ALFORRIA_SERVER_TOKEN", "bearer-leak"),
            ("ALFORRIA_WEBHOOK_SECRET", "webhook-leak"),
            ("OPENCODE_AUTH_CONTENT", "auth-leak"),
            ("OPENCODE_CONFIG_CONTENT", "config-leak"),
            // Provider keys: built in, learned, referred to.
            ("ANTHROPIC_API_KEY", "builtin-leak"),
            ("openai_api_key", "builtin-lower-leak"),
            // Shaped like an API key, of a provider no list knows.
            ("MY_CORP_API_KEY", "shape-leak"),
            ("my_corp_api_key", "shape-lower-leak"),
            ("SERVICE_API_TOKEN", "shape-token-leak"),
            ("ACME_GATEWAY_AUTH", "provider-leak"),
            ("acme_gateway_auth", "provider-lower-leak"),
            ("ACME_REFERENCED", "referenced-leak"),
        ]);
        let policy = with_names(ShellEnvPolicy::default());
        assert!(!policy.is_restricted());
        let passed = [
            "AWS_ACCESS_KEY_ID",
            "AWS_PROFILE",
            "AWS_SECRET_ACCESS_KEY",
            "AZURE_CLIENT_ID",
            "DB_PASSWORD",
            "EDITOR",
            "GH_TOKEN",
            "GITHUB_TOKEN",
            "GOOGLE_APPLICATION_CREDENTIALS",
            "HOME",
            "NPM_TOKEN",
            "PATH",
            "PGPASSWORD",
            "SSH_AUTH_SOCK",
        ];
        assert_eq!(names(&policy.shell_env_from(base.clone())), passed);
        // A helper (an MCP server, a language server, git) gets the same.
        assert_eq!(names(&policy.scrubbed_env_from(base.clone())), passed);

        // `allow` is how a host passes a key through: the name written,
        // not its other spelling.
        let info =
            serde_json::from_value(serde_json::json!({ "allow": ["MY_CORP_API_KEY"] })).unwrap();
        let policy = with_names(ShellEnvPolicy::from_config(Some(&info)));
        let mut expected = passed.to_vec();
        expected.push("MY_CORP_API_KEY");
        expected.sort();
        assert_eq!(names(&policy.shell_env_from(base)), expected);
    }

    /// A host chooses its posture whatever the mode: a standalone one can
    /// deny the credential patterns or write its own allowlist, a
    /// restricted one can hand a credential over or inherit everything
    /// that does not look like one.
    #[test]
    fn deny_allow_and_inherit_override_either_posture() {
        // Standalone, tightened by `deny` — matched whatever the case.
        let policy = with_names(policy_of(
            serde_json::json!({
                "deny": ["*_token", "AWS_*", "PGPASSWORD"]
            }),
            false,
        ));
        let kept = names(&policy.shell_env_from(credentials()));
        assert_eq!(
            kept,
            [
                "AZURE_CLIENT_ID",
                "CARGO_HOME",
                "DB_PASSWORD",
                "EDITOR",
                "GOOGLE_APPLICATION_CREDENTIALS",
                "HOME",
                "PATH"
            ]
        );
        assert_eq!(names(&policy.scrubbed_env_from(credentials())), kept);

        // Standalone, as an allowlist of its own.
        let policy = with_names(policy_of(
            serde_json::json!({
                "inherit": ["PATH", "HOME", "GITHUB_TOKEN"]
            }),
            false,
        ));
        assert_eq!(
            names(&policy.shell_env_from(credentials())),
            ["GITHUB_TOKEN", "HOME", "PATH"]
        );

        // Standalone, releasing a provider's key variable and a referenced
        // one: exactly the names written, not their other spellings.
        let policy = with_names(policy_of(
            serde_json::json!({
                "allow": ["ACME_GATEWAY_AUTH", "ACME_REFERENCED"],
                "inherit": ["ACME_*", "acme_*"]
            }),
            false,
        ));
        assert_eq!(
            names(&policy.shell_env_from(credentials())),
            ["ACME_GATEWAY_AUTH", "ACME_REFERENCED"]
        );

        // Restricted, loosened by `allow`: exactly the names written, not
        // the rest of a pattern's family.
        let policy = with_names(policy_of(
            serde_json::json!({ "allow": ["GITHUB_TOKEN", "AWS_PROFILE", "ACME_GATEWAY_AUTH"] }),
            true,
        ));
        let kept = names(&policy.shell_env_from(credentials()));
        assert_eq!(
            kept,
            [
                "ACME_GATEWAY_AUTH",
                "AWS_PROFILE",
                "GITHUB_TOKEN",
                "HOME",
                "PATH"
            ]
        );
        assert_eq!(names(&policy.scrubbed_env_from(credentials())), kept);

        // Restricted, inheriting everything: the credential patterns and
        // alforria's own still hold.
        let policy = with_names(policy_of(serde_json::json!({ "inherit": "all" }), true));
        let kept = names(&policy.shell_env_from(credentials()));
        assert_eq!(kept, ["CARGO_HOME", "EDITOR", "HOME", "PATH"]);
        assert_eq!(names(&policy.scrubbed_env_from(credentials())), kept);

        // The host's own `deny` is the one thing `allow` does not lift, in
        // either posture.
        for restricted in [false, true] {
            let policy = with_names(policy_of(
                serde_json::json!({
                    "allow": ["GITHUB_TOKEN", "EDITOR"],
                    "deny": ["github_*", "editor"]
                }),
                restricted,
            ));
            let kept = names(&policy.shell_env_from(credentials()));
            assert!(!kept.contains(&"GITHUB_TOKEN".to_string()), "{kept:?}");
            assert!(!kept.contains(&"github_token".to_string()), "{kept:?}");
            assert!(!kept.contains(&"EDITOR".to_string()), "{kept:?}");
        }
    }

    /// The names learned from a config or the catalog follow a reload:
    /// each source's names replace what it said before, and one source
    /// never forgets for another.
    #[test]
    fn a_source_read_again_replaces_its_names() {
        let mut policy = ShellEnvPolicy::default();
        assert!(policy.learn(SOURCE_CATALOG, ["CATALOG_ONE", "shared_auth"]));
        assert!(policy.learn(SOURCE_HOST_PROVIDERS, ["HOST_ONE", "SHARED_AUTH"]));
        for name in ["CATALOG_ONE", "catalog_one", "HOST_ONE", "SHARED_AUTH"] {
            assert!(policy.denied(name), "{name} must be denied");
        }
        // Unchanged: nothing to swap.
        assert!(!policy.learn(SOURCE_CATALOG, ["SHARED_AUTH", "catalog_one"]));

        // The catalog now names another variable.
        assert!(policy.learn(SOURCE_CATALOG, ["CATALOG_TWO"]));
        assert!(!policy.denied("CATALOG_ONE"));
        assert!(policy.denied("CATALOG_TWO"));
        // Still the host's.
        assert!(policy.denied("SHARED_AUTH"));
        assert!(policy.denied("HOST_ONE"));

        // A source with nothing left releases its names; the basics are
        // never learned.
        assert!(policy.learn(SOURCE_HOST_PROVIDERS, [] as [&str; 0]));
        assert!(!policy.learn(SOURCE_HOST_PROVIDERS, ["PATH", "HOME", ""]));
        assert!(!policy.denied("HOST_ONE"));
        assert!(!policy.denied("SHARED_AUTH"));
        assert!(!policy.denied("PATH"));
        assert!(policy.learn(SOURCE_CATALOG, [] as [&str; 0]));
        assert!(!policy.denied("CATALOG_TWO"));
    }

    /// Two readings of one source can finish in either order (two boots
    /// of one directory): the one that started later stands, whichever
    /// arrives last — an older one never empties or overwrites it.
    #[test]
    fn an_older_reading_arriving_late_changes_nothing() {
        let source = source_instance(std::path::Path::new("/repo"));
        let mut policy = ShellEnvPolicy::default();
        assert!(policy.learn_at(&source, 7, ["NEWER_KEY"]));
        // The boot that started earlier finishes now: with other names,
        // and with none at all.
        assert!(!policy.learn_at(&source, 5, ["OLDER_KEY"]));
        assert!(!policy.learn_at(&source, 6, [] as [&str; 0]));
        assert!(policy.denied("NEWER_KEY"));
        assert!(!policy.denied("OLDER_KEY"));
        // The same reading again, and a later one, do replace.
        assert!(policy.learn_at(&source, 7, ["NEWER_KEY", "SECOND_KEY"]));
        assert!(policy.learn_at(&source, 8, ["LATEST_KEY"]));
        assert!(!policy.denied("NEWER_KEY"));
        assert!(policy.denied("LATEST_KEY"));
        // A later reading with nothing is remembered as later too.
        assert!(policy.learn_at(&source, 9, [] as [&str; 0]));
        assert!(!policy.learn_at(&source, 8, ["LATEST_KEY"]));
        assert!(!policy.denied("LATEST_KEY"));
        // Another source is not ordered against this one.
        assert!(policy.learn_at(SOURCE_CATALOG, 1, ["CATALOG_KEY"]));
        assert!(policy.denied("CATALOG_KEY"));
    }

    /// On a restricted server the default is an allowlist: what is not on
    /// it does not pass, whatever it is called — to a shell or to a helper
    /// the server starts (an MCP stdio server, a language server, git).
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
        assert_eq!(names(&policy.scrubbed_env_from(base.clone())), basics);
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
        // A helper gets the same names; `set` is for shells.
        let env = policy.scrubbed_env_from(base.clone());
        assert_eq!(names(&env), expected);
        assert_eq!(value(&env, "TERM").as_deref(), Some("xterm"));

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
        assert_eq!(
            names(&policy.shell_env_from(base.clone())),
            ["INNOCUOUS", "PATH"]
        );
        // A helper keeps the basics under any `inherit`: without them it
        // does not start.
        let mut expected = basics.to_vec();
        expected.push("INNOCUOUS");
        expected.sort();
        assert_eq!(names(&policy.scrubbed_env_from(base.clone())), expected);
        let policy = policy_of(serde_json::json!({ "inherit": "none" }), true);
        assert_eq!(names(&policy.shell_env_from(base.clone())), [""; 0]);
        assert_eq!(names(&policy.scrubbed_env_from(base)), basics);
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
        let mut policy = policy_of(serde_json::json!({ "deny": ["MY_*"] }), true);
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
