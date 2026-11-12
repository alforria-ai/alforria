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
//!   [`ShellEnvPolicy::scrubbed_env`], then their own configured
//!   environment on top. A project config can name a command, so it must
//!   not be a way around the policy either.
//!
//! Outside the policy, on purpose: the user's own tooling at the user's
//! terminal or desktop (the TUI's editor, pager and clipboard tool,
//! `sqlite3`, `gh`, the command of `providers login`) and the fixed desktop
//! opener handed a sign-in URL. Each such spawn says so where it is made.
//!
//! There are two postures, and the server decides which:
//!
//! * **standalone** (the default: a developer's own loopback server) — a
//!   child inherits everything except
//!   what is alforria's own to protect: the host's `deny`; the server's
//!   secrets and the provider key variables of [`BUILTIN_DENY`]; names
//!   shaped like an API key ([`API_KEY_PATTERNS`]); and the names the
//!   server learns it reads a key or a secret from
//!   ([`ShellEnvPolicy::deny_names`]: every `provider.*.env[]` of the
//!   loaded configs, every `{env:NAME}` the host config and each opened
//!   project's own config files refer to). The model catalog adds less
//!   ([`ShellEnvPolicy::deny_catalog_names`]): of the names its providers
//!   list, only those of a provider this install uses, only the ones
//!   shaped like a credential ([`CREDENTIAL_PATTERNS`] — a region, a
//!   project or an endpoint passes), and never a general-purpose one
//!   ([`GENERAL_PURPOSE_CREDENTIALS`]: `GITHUB_TOKEN`, `AWS_*`, …).
//!   `GITHUB_TOKEN`, `GH_TOKEN`, `AWS_*`, `NPM_TOKEN`, `PGPASSWORD` and
//!   the like are the user's, for the user's tools, and pass as they do
//!   upstream whatever the catalog lists; `deny` is how a host that
//!   signs in to Copilot or Bedrock with one keeps it from the agent;
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
//! host choose either posture for itself, whatever the mode. `inherit` and
//! `set` are about shells: a helper on a standalone server starts from
//! everything that is not denied whatever `inherit` says, and on a
//! restricted one from [`RESTRICTED_INHERIT`], `allow` and what `inherit`
//! adds.
//!
//! That predicate governs this module's posture and nothing else: what a
//! project's config may set and read follows `config::HostMode::restricted`
//! (bearer auth or `server.trust.project`), what a request may do follows
//! its credential's scope, and the write guard and the read-only ceiling
//! follow the session (`crate::protect`, `SessionMark`).
//!
//! Two things move the posture after startup, both towards restricted:
//!
//! * the ratchet, [`tighten`]: a `webhook` or `watch` trigger that is
//!   stored or found in a config read at an instance boot, or a listener
//!   bound beyond loopback, puts the process-wide policy in the restricted
//!   posture from the next spawn on. Nothing relaxes it before the process
//!   exits — not deleting the trigger, not [`install`];
//! * the per-run floor, [`apply_shell_for`]: a process started for a
//!   session that is restricted or that outside input started gets the
//!   restricted selection whatever the process-wide posture is. One
//!   predicate, `SessionStore::runs_restricted` (the session is
//!   restricted, or has the outside-start mark), decides it for the bash
//!   tool (which reads it off the session's write guard, set by the same
//!   predicate), the session `shell` command and inline template shell.
//!   Terminals, worktree start scripts and helpers have no session and
//!   follow the process-wide posture.
//!
//! The policy comes from `shell_env` in the host config
//! (`config::host::load_host_config`) and is process-wide: [`install`] it
//! once at startup; until then [`current`] is the standalone default. The
//! learned names are a registry that only grows for the life of the
//! process: [`learn`] adds to it at every instance boot, [`learn_catalog`]
//! at every catalog read and every build of an instance's provider state,
//! and nothing takes a name out — not disposing an instance, whose
//! processes can outlive it, and not a config that stopped mentioning it.
//! Withholding a variable until the next restart is harmless; releasing
//! one early is a leak. `allow` names the variables the host hands over
//! all the same; like the rest of the host config it is read at startup.
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

/// Credentials other tools on a developer's machine read (`gh`, `git`,
/// `glab`, `aws`, `az`, `gcloud`, the cloud SDKs): never withheld on a
/// standalone server on the model catalog's say-so, even when the install
/// uses the provider that lists them (GitHub Copilot lists `GITHUB_TOKEN`,
/// Amazon Bedrock the `AWS_*` access keys). They are not alforria's to
/// take: the user's shell had them before a catalog was cached. A host
/// that wants one kept from the agent says so itself — `shell_env.deny`,
/// or the name in its own `provider.*.env` — and a restricted server
/// withholds them all anyway ([`CREDENTIAL_PATTERNS`]).
pub const GENERAL_PURPOSE_CREDENTIALS: &[&str] = &[
    "GITHUB_TOKEN",
    "GH_TOKEN",
    "GITLAB_TOKEN",
    "AWS_*",
    "AZURE_*",
    "GOOGLE_APPLICATION_CREDENTIALS",
    "GOOGLE_CLOUD_*",
    "GCLOUD_*",
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
    /// Names the server learned it reads a key or a secret from
    /// ([`ShellEnvPolicy::deny_names`]), upper-cased. Only ever added to.
    named: BTreeSet<String>,
    /// The `env` names of every provider the model catalog lists
    /// ([`ShellEnvPolicy::deny_catalog_names`]), upper-cased: denied on a
    /// restricted server. Only ever added to.
    catalog: BTreeSet<String>,
    /// Those of `catalog` that belong to a provider the install uses:
    /// what a standalone server withholds of them is decided in
    /// [`ShellEnvPolicy::learned`]. Only ever added to.
    catalog_in_use: BTreeSet<String>,
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
            catalog: BTreeSet::new(),
            catalog_in_use: BTreeSet::new(),
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

    /// Deny `names` too, whatever their case: the variables a config says
    /// the server reads a secret from (a `provider.*.env` name, a
    /// `{env:NAME}`). [`RESTRICTED_INHERIT`] is left alone. Added to what is
    /// already denied: there is no way to take a learned name out of a
    /// policy — `allow` is how a host releases one.
    pub fn deny_names<S: AsRef<str>>(&mut self, names: impl IntoIterator<Item = S>) {
        for name in names {
            let name = name.as_ref();
            if !name.is_empty() && !matches_any(name, RESTRICTED_INHERIT) {
                self.named.insert(name.to_ascii_uppercase());
            }
        }
    }

    /// Deny the `env` names a provider of the model catalog lists.
    /// `in_use`: the install uses that provider (it is declared in a
    /// loaded config, selected as a model, or has stored credentials).
    ///
    /// The catalog lists the variables of every provider there is, and
    /// some are not a model provider's alone: a restricted server denies
    /// them all, as it denies whatever looks like a credential. A
    /// standalone one denies a name only when its provider is in use,
    /// the name is shaped like a credential ([`CREDENTIAL_PATTERNS`]) and
    /// it is not one of the [`GENERAL_PURPOSE_CREDENTIALS`].
    pub fn deny_catalog_names<S: AsRef<str>>(
        &mut self,
        names: impl IntoIterator<Item = S>,
        in_use: bool,
    ) {
        for name in names {
            let name = name.as_ref();
            if name.is_empty() || matches_any(name, RESTRICTED_INHERIT) {
                continue;
            }
            let upper = name.to_ascii_uppercase();
            if in_use {
                self.catalog_in_use.insert(upper.clone());
            }
            self.catalog.insert(upper);
        }
    }

    fn learned(&self, upper: &str) -> bool {
        if self.named.contains(upper) {
            return true;
        }
        if self.restricted {
            return self.catalog.contains(upper);
        }
        self.catalog_in_use.contains(upper)
            && matches_any(upper, CREDENTIAL_PATTERNS)
            && !matches_any(upper, GENERAL_PURPOSE_CREDENTIALS)
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

    /// [`ShellEnvPolicy::shell_env_from`] for a process started on behalf
    /// of a session: with `outside_run` (the session is restricted, or
    /// outside input started a run in it) the restricted posture's
    /// selection, whatever this policy's own posture is.
    pub fn shell_env_for_from(
        &self,
        outside_run: bool,
        base: impl IntoIterator<Item = (OsString, OsString)>,
    ) -> Vec<(OsString, OsString)> {
        if outside_run && !self.restricted {
            self.clone().restricted(true).shell_env_from(base)
        } else {
            self.shell_env_from(base)
        }
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

/// Why the process-wide policy was put in the restricted posture after
/// it was installed ([`tighten`]); empty when it was not.
static TIGHTENED: std::sync::Mutex<Vec<String>> = std::sync::Mutex::new(Vec::new());

/// Make `policy` the process-wide policy. Names the process has already
/// learned stay learned, and a posture that was tightened stays
/// restricted.
pub fn install(mut policy: ShellEnvPolicy) {
    let mut current = CURRENT.write().unwrap_or_else(|p| p.into_inner());
    if let Some(previous) = current.as_deref() {
        policy.named.extend(previous.named.iter().cloned());
        policy.catalog.extend(previous.catalog.iter().cloned());
        policy
            .catalog_in_use
            .extend(previous.catalog_in_use.iter().cloned());
    }
    if !TIGHTENED
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .is_empty()
    {
        policy.restricted = true;
    }
    *current = Some(Arc::new(policy));
}

/// Put the process-wide policy in the restricted posture, for `reason`,
/// from the next spawn on: something that makes the server unattended or
/// reachable appeared after it started (a `webhook` or `watch` trigger
/// was created, a listener was bound beyond loopback). A ratchet — the
/// posture only tightens while the process lives, and deleting the
/// trigger does not relax it. Each reason is logged once.
pub fn tighten(reason: &str) {
    let mut current = CURRENT.write().unwrap_or_else(|p| p.into_inner());
    let mut reasons = TIGHTENED.lock().unwrap_or_else(|p| p.into_inner());
    if reasons.iter().any(|known| known == reason) {
        return;
    }
    reasons.push(reason.to_string());
    let policy = current.as_deref().cloned().unwrap_or_default();
    if policy.restricted {
        return;
    }
    tracing::warn!(
        "shell environment: now restricted ({reason}): from here on the agent's processes \
         start from the allowlist and credential-like variables are withheld, until the \
         server restarts"
    );
    *current = Some(Arc::new(policy.restricted(true)));
}

/// The reasons the posture was tightened for since the process started
/// ([`tighten`]).
pub fn tightened() -> Vec<String> {
    TIGHTENED.lock().unwrap_or_else(|p| p.into_inner()).clone()
}

/// [`ShellEnvPolicy::deny_names`] on the process-wide policy: called at
/// every instance boot and catalog read with the names found there. The
/// registry is the union of everything ever passed, taken under one lock;
/// see the module documentation for why nothing is removed.
pub fn learn<S: AsRef<str>>(names: impl IntoIterator<Item = S>) {
    let mut current = CURRENT.write().unwrap_or_else(|p| p.into_inner());
    let mut policy = current.as_deref().cloned().unwrap_or_default();
    policy.deny_names(names);
    if current.as_deref() != Some(&policy) {
        *current = Some(Arc::new(policy));
    }
}

/// [`ShellEnvPolicy::deny_catalog_names`] on the process-wide policy:
/// called with each provider of the model catalog when the catalog is
/// read (`in_use` false) and when an instance's provider state is built
/// (`in_use` true for the providers it uses). Append-only like [`learn`].
pub fn learn_catalog<S: AsRef<str>>(names: impl IntoIterator<Item = S>, in_use: bool) {
    let mut current = CURRENT.write().unwrap_or_else(|p| p.into_inner());
    let mut policy = current.as_deref().cloned().unwrap_or_default();
    policy.deny_catalog_names(names, in_use);
    if current.as_deref() != Some(&policy) {
        *current = Some(Arc::new(policy));
    }
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

/// [`apply_shell`] for a process started on behalf of a session.
/// `outside_run`: the session is restricted or a run in it was started by
/// outside input (`SessionMark::OutsideStart` — a webhook or watch
/// trigger's run, its subagents and forks). Such a process gets the
/// restricted posture's environment whatever the server's own posture is:
/// the floor does not depend on the process-wide policy being current.
pub fn apply_shell_for(command: &mut std::process::Command, outside_run: bool) {
    command
        .env_clear()
        .envs(current().shell_env_for_from(outside_run, std::env::vars_os()));
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

    /// The model catalog lists the variables of every provider there is,
    /// some of them the user's own tools' (`GITHUB_TOKEN` for GitHub
    /// Copilot, the `AWS_*` keys for Amazon Bedrock). On a standalone
    /// server a catalog name is withheld only for a provider in use, only
    /// when shaped like a credential, and never when general-purpose; a
    /// name a config lists, and the host's `deny`, are withheld whatever
    /// they are. A restricted server withholds every catalog name.
    #[test]
    fn catalog_names_are_withheld_for_providers_in_use_and_credentials_only() {
        let teach = |policy: &mut ShellEnvPolicy| {
            // Unused providers.
            policy.deny_catalog_names(
                [
                    "GITHUB_TOKEN",
                    "AWS_ACCESS_KEY_ID",
                    "OTHER_TOKEN",
                    "HF_TOKEN",
                ],
                false,
            );
            // Providers in use.
            policy.deny_catalog_names(
                [
                    "ACME_TOKEN",
                    "ACME_REGION",
                    "ACME_BASE_URL",
                    "GITHUB_TOKEN",
                    "GH_TOKEN",
                    "GITLAB_TOKEN",
                    "AWS_ACCESS_KEY_ID",
                    "AWS_SECRET_ACCESS_KEY",
                    "AWS_REGION",
                    "AZURE_RESOURCE_NAME",
                    "GOOGLE_APPLICATION_CREDENTIALS",
                    "GOOGLE_CLOUD_PROJECT",
                    "GCLOUD_PROJECT",
                    "PATH",
                ],
                true,
            );
        };
        let mut policy = ShellEnvPolicy::default();
        teach(&mut policy);
        let denied = |policy: &ShellEnvPolicy, names: &[&str]| -> Vec<String> {
            names
                .iter()
                .filter(|name| policy.denied(name))
                .map(|name| name.to_string())
                .collect()
        };
        let all = [
            "ACME_TOKEN",
            "acme_token",
            "ACME_REGION",
            "ACME_BASE_URL",
            "OTHER_TOKEN",
            "HF_TOKEN",
            "GITHUB_TOKEN",
            "GH_TOKEN",
            "GITLAB_TOKEN",
            "AWS_ACCESS_KEY_ID",
            "AWS_SECRET_ACCESS_KEY",
            "AWS_REGION",
            "AZURE_RESOURCE_NAME",
            "GOOGLE_APPLICATION_CREDENTIALS",
            "GOOGLE_CLOUD_PROJECT",
            "GCLOUD_PROJECT",
            "PATH",
        ];
        assert_eq!(denied(&policy, &all), ["ACME_TOKEN", "acme_token"]);

        // Explicit operator intent wins over the exemption: the host's
        // `deny`, and a name its own config lists as a provider's.
        let mut explicit = self::policy(serde_json::json!({ "deny": ["GITHUB_TOKEN"] }));
        teach(&mut explicit);
        explicit.deny_names(["AWS_SECRET_ACCESS_KEY", "ACME_REGION"]);
        assert_eq!(
            denied(&explicit, &all),
            [
                "ACME_TOKEN",
                "acme_token",
                "ACME_REGION",
                "GITHUB_TOKEN",
                "AWS_SECRET_ACCESS_KEY"
            ]
        );

        // `allow` hands a catalog name back.
        let mut allowing = self::policy(serde_json::json!({ "allow": ["ACME_TOKEN"] }));
        teach(&mut allowing);
        assert_eq!(denied(&allowing, &all), ["acme_token"]);

        // Restricted: every catalog name but the basics, as before.
        let restricted = policy.clone().restricted(true);
        assert_eq!(
            denied(&restricted, &all),
            all.iter()
                .filter(|name| **name != "PATH")
                .map(|name| name.to_string())
                .collect::<Vec<_>>()
        );
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

    /// The per-run floor: a process started for a session that outside
    /// input started (a webhook or watch trigger's run, its subagents and
    /// forks) gets the restricted environment on a server whose own
    /// posture is standalone — it does not wait for the server's posture
    /// to be current.
    #[test]
    fn an_outside_started_run_gets_the_restricted_environment_on_a_standalone_server() {
        let policy = with_names(ShellEnvPolicy::default());
        assert!(!policy.is_restricted());
        // Any other session on that server: the user's environment.
        let own = names(&policy.shell_env_for_from(false, credentials()));
        assert!(own.contains(&"GITHUB_TOKEN".to_string()), "{own:?}");
        assert!(own.contains(&"AWS_PROFILE".to_string()), "{own:?}");
        // The outside-started one: the allowlist.
        assert_eq!(
            names(&policy.shell_env_for_from(true, credentials())),
            ["HOME", "PATH"]
        );
        // The host's `allow` and `set` still apply to it.
        let policy = with_names(policy_of(
            serde_json::json!({ "allow": ["CARGO_HOME"], "set": { "CI": "1" } }),
            false,
        ));
        assert_eq!(
            names(&policy.shell_env_for_from(true, credentials())),
            ["CARGO_HOME", "CI", "HOME", "PATH"]
        );
        // On a restricted server it changes nothing.
        let policy = with_names(ShellEnvPolicy::default().restricted(true));
        assert_eq!(
            policy.shell_env_for_from(true, credentials()),
            policy.shell_env_for_from(false, credentials())
        );
    }

    /// Learned names only accumulate: a later reading that no longer
    /// mentions one does not release it, and `allow` does.
    #[test]
    fn learned_names_accumulate_and_allow_releases_one() {
        let mut policy = ShellEnvPolicy::default();
        policy.deny_names(["CATALOG_ONE", "shared_auth"]);
        policy.deny_names(["HOST_ONE"]);
        // The same source read again, now naming something else.
        policy.deny_names(["CATALOG_TWO", "PATH", "HOME", ""]);
        policy.deny_names([] as [&str; 0]);
        for name in [
            "CATALOG_ONE",
            "catalog_one",
            "SHARED_AUTH",
            "HOST_ONE",
            "CATALOG_TWO",
        ] {
            assert!(policy.denied(name), "{name} must stay denied");
        }
        assert!(!policy.denied("PATH"));

        // The way out without a restart: the exact name, in `allow`.
        let info = serde_json::from_value(serde_json::json!({ "allow": ["CATALOG_ONE"] })).unwrap();
        let mut allowing = ShellEnvPolicy::from_config(Some(&info));
        allowing.deny_names(["CATALOG_ONE", "CATALOG_TWO"]);
        assert!(!allowing.denied("CATALOG_ONE"));
        assert!(allowing.denied("catalog_one"));
        assert!(allowing.denied("CATALOG_TWO"));

        // A restart is a fresh policy: it knows none of them.
        assert!(!ShellEnvPolicy::default().denied("CATALOG_ONE"));
    }

    /// The process-wide registry is the union of everything learned,
    /// whatever the order the learners finish in, and installing a policy
    /// takes nothing out of it.
    #[test]
    fn concurrent_learns_end_with_the_union() {
        let name = |thread: usize, round: usize| format!("UNIONTEST_{thread}_{round}_AUTH");
        std::thread::scope(|scope| {
            for thread in 0..8 {
                scope.spawn(move || {
                    for round in 0..50 {
                        learn([name(thread, round)]);
                        if round == 25 {
                            install(current().as_ref().clone());
                        }
                    }
                });
            }
        });
        install(ShellEnvPolicy::default());
        let policy = current();
        for thread in 0..8 {
            for round in 0..50 {
                assert!(policy.denied(&name(thread, round)), "{thread}/{round}");
            }
        }
        assert!(!policy.denied("UNIONTEST_NEVER_LEARNED_AUTH"));
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
