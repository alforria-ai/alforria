//! cli/cmd/providers.ts port — the `providers`/`auth` command family:
//! `list`, `login` (well-known URL + API-key flows) and `logout`.

use std::path::Path;

use clap::ArgMatches;
use opencode_core::catalog::Provider;
use opencode_core::catalog::Providers;
use opencode_core::{CatalogService, ConfigLoader, GlobalPaths, LoadParams};
use serde_json::{json, Value};

use crate::error::{core_error, CliError, TypedError};
use crate::ui::{style, Ui};

use opencode_server::state::{AuthStore, FileAuthStore};

// ---------------------------------------------------------------------------
// Prompt frame — @clack/prompts intro/outro/log lines. The exact clack frame
// glyphs are not part of the pinned TS source (node_modules aren't vendored);
// `┌`/`│`/`└` mirror the documented clack layout.
// ---------------------------------------------------------------------------

fn intro(ui: &mut Ui, message: &str) {
    ui.println(&format!("┌ {message}"));
}

fn outro(ui: &mut Ui, message: &str) {
    ui.println(&format!("└ {message}"));
}

fn log_info(ui: &mut Ui, message: &str) {
    ui.println(&format!("│ {message}"));
}

fn log_success(ui: &mut Ui, message: &str) {
    ui.println(&format!("│ {message}"));
}

fn log_warn(ui: &mut Ui, message: &str) {
    ui.println(&format!("│ {message}"));
}

fn log_error(ui: &mut Ui, message: &str) {
    ui.println(&format!("│ {message}"));
}

fn server_error(err: opencode_server::ServerError) -> TypedError {
    match err {
        opencode_server::ServerError::Core(core) => core_error(core),
        opencode_server::ServerError::Api(api) => {
            TypedError::Cli(CliError::new(format!("{api:?}")))
        }
    }
}

// ---------------------------------------------------------------------------
// Seams
// ---------------------------------------------------------------------------

/// Interactive prompt seam (`@clack/prompts`): `select` over (label, value)
/// pairs, free-form `text`, and `password`. Cancellation maps to
/// `UICancelledError`.
pub trait Prompter {
    fn select(&mut self, message: &str, options: &[(String, String)])
        -> Result<String, TypedError>;
    fn text(&mut self, message: &str) -> Result<String, TypedError>;
    fn password(&mut self, message: &str) -> Result<String, TypedError>;
}

/// The interactive fallback: prompts go to stderr, answers come from stdin.
struct StdinPrompter;

fn read_line(message: &str) -> Result<String, TypedError> {
    eprintln!("{message}");
    let mut line = String::new();
    if std::io::stdin().read_line(&mut line).unwrap_or(0) == 0 {
        return Err(TypedError::UiCancelled);
    }
    Ok(line.trim_end_matches(['\r', '\n']).to_string())
}

impl Prompter for StdinPrompter {
    fn select(
        &mut self,
        message: &str,
        options: &[(String, String)],
    ) -> Result<String, TypedError> {
        loop {
            let line = read_line(message)?;
            for (label, value) in options {
                if line == *value || line.eq_ignore_ascii_case(label) {
                    return Ok(value.clone());
                }
            }
        }
    }

    fn text(&mut self, message: &str) -> Result<String, TypedError> {
        read_line(message)
    }

    fn password(&mut self, message: &str) -> Result<String, TypedError> {
        read_line(message)
    }
}

/// The `{url}/.well-known/opencode` document (`auth.command` + `auth.env`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WellKnownAuth {
    pub command: Vec<String>,
    pub env: String,
}

/// Seam for the well-known URL login flow: HTTP metadata fetch + the auth
/// command runner (TS `fetch` + `Process.spawn`).
pub trait WellKnown {
    fn fetch(&self, url: &str) -> Result<WellKnownAuth, String>;
    /// Runs the auth command (stdout piped, stderr inherited) and returns its
    /// exit status plus captured stdout (the token).
    fn run(&self, command: &[String]) -> Result<(i32, String), String>;
}

struct HttpWellKnown;

impl WellKnown for HttpWellKnown {
    fn fetch(&self, url: &str) -> Result<WellKnownAuth, String> {
        let url = format!("{url}/.well-known/opencode");
        std::thread::scope(|scope| {
            scope
                .spawn(move || {
                    tokio::runtime::Builder::new_current_thread()
                        .enable_all()
                        .build()
                        .map_err(|err| err.to_string())?
                        .block_on(async {
                            let body = reqwest::get(&url)
                                .await
                                .map_err(|err| err.to_string())?
                                .text()
                                .await
                                .map_err(|err| err.to_string())?;
                            let value: Value =
                                serde_json::from_str(&body).map_err(|err| err.to_string())?;
                            let auth = value
                                .get("auth")
                                .ok_or_else(|| "missing auth".to_string())?;
                            let command = auth
                                .get("command")
                                .and_then(Value::as_array)
                                .ok_or_else(|| "missing auth.command".to_string())?
                                .iter()
                                .filter_map(Value::as_str)
                                .map(str::to_string)
                                .collect();
                            let env = auth
                                .get("env")
                                .and_then(Value::as_str)
                                .ok_or_else(|| "missing auth.env".to_string())?
                                .to_string();
                            Ok(WellKnownAuth { command, env })
                        })
                })
                .join()
                .map_err(|_| "fetch panicked".to_string())?
        })
    }

    fn run(&self, command: &[String]) -> Result<(i32, String), String> {
        let Some((head, args)) = command.split_first() else {
            return Err("empty command".to_string());
        };
        let child = std::process::Command::new(head)
            .args(args)
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::inherit())
            .spawn()
            .map_err(|err| err.to_string())?;
        let output = child.wait_with_output().map_err(|err| err.to_string())?;
        Ok((
            output.status.code().unwrap_or(-1),
            String::from_utf8_lossy(&output.stdout).into_owned(),
        ))
    }
}

// ---------------------------------------------------------------------------
// Arguments
// ---------------------------------------------------------------------------

/// The `providers login` flag surface (providers.ts:304-319). `--method` is
/// plugin-auth only (spec non-goal N5): accepted on the CLI surface, never
/// carried.
#[derive(Debug, Clone, Default)]
pub struct LoginArgs {
    pub url: Option<String>,
    pub provider: Option<String>,
}

impl LoginArgs {
    pub fn from_matches(matches: &ArgMatches) -> Self {
        LoginArgs {
            url: matches.get_one::<String>("url").cloned(),
            provider: matches.get_one::<String>("provider").cloned(),
        }
    }
}

/// Injected collaborators for the `login` handler.
pub struct LoginDeps<'a> {
    pub auth: &'a FileAuthStore,
    pub catalog: &'a CatalogService,
    pub wellknown: &'a mut dyn WellKnown,
    pub prompter: &'a mut dyn Prompter,
    pub disabled_providers: Vec<String>,
    pub enabled_providers: Option<Vec<String>>,
}

/// providers.ts:371-379 — the fixed provider selection priority.
pub fn priority(id: &str) -> usize {
    match id {
        "opencode" => 0,
        "openai" => 1,
        "github-copilot" => 2,
        "google" => 3,
        "anthropic" => 4,
        "openrouter" => 5,
        "vercel" => 6,
        _ => 99,
    }
}

/// providers.ts:261 — `<home>/…` display paths render as `~/…`.
fn display_path(auth_path: &Path, home: &Path) -> String {
    let path = auth_path.to_string_lossy();
    let home = home.to_string_lossy();
    if let Some(rest) = path.strip_prefix(home.as_ref()) {
        return format!("~{rest}");
    }
    path.into_owned()
}

/// The provider entries the `--provider` flag / select prompt match against,
/// in the fixed priority order (providers.ts:361-379): `(name, id)` pairs.
fn ordered_providers(deps: &LoginDeps, catalog: &Providers) -> Vec<(String, String)> {
    let disabled: Vec<&str> = deps.disabled_providers.iter().map(String::as_str).collect();
    let enabled = deps.enabled_providers.as_ref();
    let mut providers: Vec<&Provider> = catalog
        .values()
        .filter(|p| {
            enabled.is_none_or(|enabled| enabled.iter().any(|id| id == &p.id))
                && !disabled.contains(&p.id.as_str())
        })
        .collect();
    providers.sort_by_key(|p| (priority(&p.id), p.name.clone()));
    providers
        .iter()
        .map(|p| (p.name.clone(), p.id.clone()))
        .collect()
}

// ---------------------------------------------------------------------------
// list (providers.ts:248-297)
// ---------------------------------------------------------------------------

pub fn list(
    ui: &mut Ui,
    auth: &FileAuthStore,
    catalog: &Providers,
    auth_file: &Path,
    home: &Path,
) -> Result<(), TypedError> {
    ui.empty();
    intro(
        ui,
        &format!(
            "Credentials {}{}{}",
            style::TEXT_DIM,
            display_path(auth_file, home),
            style::TEXT_NORMAL
        ),
    );
    let credentials = auth.all().map_err(server_error)?;
    for (key, info) in &credentials {
        let name = catalog
            .get(key)
            .map(|p| p.name.clone())
            .filter(|name| !name.is_empty())
            .unwrap_or_else(|| key.clone());
        let ty = info.get("type").and_then(Value::as_str).unwrap_or_default();
        log_info(
            ui,
            &format!("{name} {}{ty}{}", style::TEXT_DIM, style::TEXT_NORMAL),
        );
    }
    outro(ui, &format!("{} credentials", credentials.len()));

    let mut active_env_vars = Vec::new();
    for (provider_id, provider) in catalog {
        for env_var in &provider.env {
            if std::env::var(env_var).is_ok_and(|value| !value.is_empty()) {
                let name = if provider.name.is_empty() {
                    provider_id.clone()
                } else {
                    provider.name.clone()
                };
                active_env_vars.push((name, env_var.clone()));
            }
        }
    }
    if !active_env_vars.is_empty() {
        ui.empty();
        intro(ui, "Environment");
        for (provider, env_var) in &active_env_vars {
            log_info(ui, &format!("{provider} {}{env_var}", style::TEXT_DIM));
        }
        let plural = if active_env_vars.len() == 1 { "" } else { "s" };
        outro(
            ui,
            &format!("{} environment variable{plural}", active_env_vars.len()),
        );
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// login (providers.ts:299-489)
// ---------------------------------------------------------------------------

fn prompt_provider_id(prompter: &mut dyn Prompter) -> Result<String, TypedError> {
    loop {
        let value = prompter.text("Enter provider id")?;
        if value
            .chars()
            .all(|c| c.is_ascii_digit() || c.is_ascii_lowercase() || c == '-')
            && !value.is_empty()
        {
            return Ok(value.strip_prefix("@ai-sdk/").unwrap_or(&value).to_string());
        }
    }
}

/// The "other" entry degrades to the plain API-key store path (spec STOP:
/// plugin providers are non-goal N5).
fn login_api_key_flow(ui: &mut Ui, deps: &mut LoginDeps, provider: &str) -> Result<(), TypedError> {
    if provider == "amazon-bedrock" {
        log_info(
            ui,
            "Amazon Bedrock authentication priority:\n  1. Bearer token (AWS_BEARER_TOKEN_BEDROCK or /connect)\n  2. AWS credential chain (profile, access keys, IAM roles, EKS IRSA)\n\nConfigure via opencode.json options (profile, region, endpoint) or\nAWS environment variables (AWS_PROFILE, AWS_REGION, AWS_ACCESS_KEY_ID, AWS_WEB_IDENTITY_TOKEN_FILE).",
        );
    }
    if provider == "opencode" {
        log_info(ui, "Create an api key at https://opencode.ai/auth");
    }
    if provider == "vercel" {
        log_info(
            ui,
            "You can create an api key at https://vercel.link/ai-gateway-token",
        );
    }
    if provider == "cloudflare" || provider == "cloudflare-ai-gateway" {
        log_info(
            ui,
            "Cloudflare AI Gateway can be configured with CLOUDFLARE_GATEWAY_ID, CLOUDFLARE_ACCOUNT_ID, and CLOUDFLARE_API_TOKEN environment variables. Read more: https://opencode.ai/docs/providers/#cloudflare-ai-gateway",
        );
    }
    let api_key = deps.prompter.password("Enter your API key")?;
    deps.auth
        .set(provider, json!({"type": "api", "key": api_key}))
        .map_err(server_error)?;
    outro(ui, "Done");
    Ok(())
}

fn login_url(ui: &mut Ui, deps: &mut LoginDeps, raw_url: &str) -> Result<(), TypedError> {
    let url = raw_url.trim_end_matches('/').to_string();
    let wellknown = deps.wellknown.fetch(&url).map_err(|err| {
        TypedError::Cli(CliError::new(format!(
            "Failed to load auth provider metadata from {url}: {err}"
        )))
    })?;
    log_info(ui, &format!("Running `{}`", wellknown.command.join(" ")));
    match deps.wellknown.run(&wellknown.command) {
        Err(err) => Err(TypedError::Cli(CliError::new(format!(
            "Failed to run auth provider command: {err}"
        )))),
        Ok((0, token)) => {
            deps.auth
                .set(
                    &url,
                    json!({
                        "type": "wellknown",
                        "key": wellknown.env,
                        "token": token.trim(),
                    }),
                )
                .map_err(server_error)?;
            log_success(ui, &format!("Logged into {url}"));
            outro(ui, "Done");
            Ok(())
        }
        Ok(_) => {
            log_error(ui, "Failed");
            outro(ui, "Done");
            Ok(())
        }
    }
}

fn login_provider(ui: &mut Ui, deps: &mut LoginDeps, args: &LoginArgs) -> Result<(), TypedError> {
    // `Effect.ignore(modelsDev.refresh(true))` — refresh errors are swallowed.
    deps.catalog.refresh(true);
    let catalog = deps.catalog.get().map_err(core_error)?;
    let providers = ordered_providers(deps, &catalog);
    let provider = match &args.provider {
        Some(input) => {
            let matched = providers.iter().find(|(_, id)| id == input).or_else(|| {
                providers
                    .iter()
                    .find(|(name, _)| name.eq_ignore_ascii_case(input))
            });
            match matched {
                Some((_, id)) => id.clone(),
                None => {
                    return Err(TypedError::Cli(CliError::new(format!(
                        "Unknown provider \"{input}\""
                    ))))
                }
            }
        }
        None => {
            let mut options = providers;
            options.push(("Other".to_string(), "other".to_string()));
            let value = deps.prompter.select("Select provider", &options)?;
            if value == "other" {
                let id = prompt_provider_id(deps.prompter)?;
                log_warn(
                    ui,
                    &format!("This only stores a credential for {id} - you will need configure it in opencode.json, check the docs for examples."),
                );
                id
            } else {
                value
            }
        }
    };
    login_api_key_flow(ui, deps, &provider)
}

pub fn login(ui: &mut Ui, args: &LoginArgs, deps: &mut LoginDeps) -> Result<(), TypedError> {
    ui.empty();
    intro(ui, "Add credential");
    if let Some(url) = args.url.as_deref() {
        login_url(ui, deps, url)
    } else {
        login_provider(ui, deps, args)
    }
}

// ---------------------------------------------------------------------------
// logout (providers.ts:491-534)
// ---------------------------------------------------------------------------

pub fn logout(
    ui: &mut Ui,
    auth: &FileAuthStore,
    catalog: &CatalogService,
    prompter: &mut dyn Prompter,
    arg: Option<&str>,
) -> Result<(), TypedError> {
    ui.empty();
    let credentials = auth.all().map_err(server_error)?;
    intro(ui, "Remove credential");
    if credentials.is_empty() {
        log_error(ui, "No credentials found");
        return Ok(());
    }
    let database = catalog.get().map_err(core_error)?;
    let provider = match arg {
        Some(input) => credentials
            .keys()
            .find(|key| {
                key.as_str() == input
                    || database
                        .get(key.as_str())
                        .is_some_and(|p| p.name.eq_ignore_ascii_case(input))
            })
            .cloned(),
        None => {
            let options: Vec<(String, String)> = credentials
                .iter()
                .map(|(key, info)| {
                    let name = database
                        .get(key)
                        .map(|p| p.name.clone())
                        .filter(|name| !name.is_empty())
                        .unwrap_or_else(|| key.clone());
                    let ty = info.get("type").and_then(Value::as_str).unwrap_or_default();
                    (format!("{name}{} ({ty})", style::TEXT_DIM), key.clone())
                })
                .collect();
            Some(prompter.select("Select provider", &options)?)
        }
    };
    let Some(provider) = provider else {
        return Err(TypedError::Cli(CliError::new(format!(
            "Unknown configured provider \"{}\"",
            arg.unwrap_or_default()
        ))));
    };
    auth.remove(&provider).map_err(server_error)?;
    outro(ui, "Logout successful");
    Ok(())
}

// ---------------------------------------------------------------------------
// Command entry
// ---------------------------------------------------------------------------

/// `instance: (args) => !args.url` — the provider form bootstraps the project
/// config; the URL form must not (a stale token would crash the bootstrap).
fn config_provider_filters(
    args: &LoginArgs,
) -> Result<(Vec<String>, Option<Vec<String>>), TypedError> {
    if args.url.is_some() {
        return Ok((Vec::new(), None));
    }
    let directory = std::env::current_dir().map_err(|err| TypedError::Unknown {
        raw: err.to_string(),
    })?;
    let (config, _) = ConfigLoader::new()
        .load(&LoadParams::new(directory))
        .map_err(core_error)?;
    Ok((
        config.disabled_providers.clone().unwrap_or_default(),
        config.enabled_providers.clone(),
    ))
}

pub fn run(matches: &ArgMatches, ui: &mut Ui) -> Result<(), TypedError> {
    let paths = GlobalPaths::from_env();
    let auth = FileAuthStore::new(paths.data.join("auth.json"));
    let catalog = crate::catalog::catalog_service();
    match matches.subcommand_name() {
        Some("list") => {
            let providers = catalog.get().map_err(core_error)?;
            list(
                ui,
                &auth,
                &providers,
                &paths.data.join("auth.json"),
                &paths.home,
            )
        }
        Some("login") => {
            let args = LoginArgs::from_matches(matches.subcommand_matches("login").expect("login"));
            let (disabled_providers, enabled_providers) = config_provider_filters(&args)?;
            let mut wellknown: Box<dyn WellKnown> = Box::new(HttpWellKnown);
            let mut prompter: Box<dyn Prompter> = Box::new(StdinPrompter);
            let mut deps = LoginDeps {
                auth: &auth,
                catalog: &catalog,
                wellknown: &mut *wellknown,
                prompter: &mut *prompter,
                disabled_providers,
                enabled_providers,
            };
            login(ui, &args, &mut deps)
        }
        Some("logout") => {
            let logout_matches = matches.subcommand_matches("logout").expect("logout");
            let arg = logout_matches.get_one::<String>("provider").cloned();
            let mut prompter: Box<dyn Prompter> = Box::new(StdinPrompter);
            logout(ui, &auth, &catalog, &mut *prompter, arg.as_deref())
        }
        _ => unreachable!("providers requires a subcommand"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use std::path::PathBuf;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::{Arc, Mutex};

    // ------------------------------------------------------------------
    // Fakes
    // ------------------------------------------------------------------

    struct ScriptPrompter {
        select_result: String,
        text_result: String,
        password_result: String,
        selects: AtomicUsize,
    }

    impl ScriptPrompter {
        fn new(select: &str, text: &str, password: &str) -> Self {
            ScriptPrompter {
                select_result: select.to_string(),
                text_result: text.to_string(),
                password_result: password.to_string(),
                selects: AtomicUsize::new(0),
            }
        }
    }

    impl Prompter for ScriptPrompter {
        fn select(
            &mut self,
            _message: &str,
            _options: &[(String, String)],
        ) -> Result<String, TypedError> {
            self.selects.fetch_add(1, Ordering::SeqCst);
            Ok(self.select_result.clone())
        }

        fn text(&mut self, _message: &str) -> Result<String, TypedError> {
            Ok(self.text_result.clone())
        }

        fn password(&mut self, _message: &str) -> Result<String, TypedError> {
            Ok(self.password_result.clone())
        }
    }

    struct ScriptWellKnown {
        fetch_result: Result<WellKnownAuth, String>,
        run_result: (i32, String),
        fetched: Mutex<Vec<String>>,
        commands: Mutex<Vec<Vec<String>>>,
    }

    impl ScriptWellKnown {
        fn ok(command: Vec<&str>, env: &str, token: &str) -> Self {
            ScriptWellKnown {
                fetch_result: Ok(WellKnownAuth {
                    command: command.iter().map(|s| s.to_string()).collect(),
                    env: env.to_string(),
                }),
                run_result: (0, token.to_string()),
                fetched: Mutex::new(Vec::new()),
                commands: Mutex::new(Vec::new()),
            }
        }

        fn fail_run() -> Self {
            ScriptWellKnown {
                fetch_result: Ok(WellKnownAuth {
                    command: vec!["opencode".to_string(), "auth".to_string()],
                    env: "MY_TOKEN".to_string(),
                }),
                run_result: (1, String::new()),
                fetched: Mutex::new(Vec::new()),
                commands: Mutex::new(Vec::new()),
            }
        }

        fn fail_fetch() -> Self {
            ScriptWellKnown {
                fetch_result: Err("fetch failed".to_string()),
                run_result: (0, String::new()),
                fetched: Mutex::new(Vec::new()),
                commands: Mutex::new(Vec::new()),
            }
        }
    }

    impl WellKnown for ScriptWellKnown {
        fn fetch(&self, url: &str) -> Result<WellKnownAuth, String> {
            self.fetched.lock().unwrap().push(url.to_string());
            self.fetch_result.clone()
        }

        fn run(&self, command: &[String]) -> Result<(i32, String), String> {
            self.commands.lock().unwrap().push(command.to_vec());
            Ok(self.run_result.clone())
        }
    }

    struct FixtureFetcher {
        text: String,
    }

    impl opencode_core::catalog::Fetcher for FixtureFetcher {
        fn get(
            &self,
            _url: &str,
            _user_agent: &str,
        ) -> Result<String, opencode_core::catalog::FetchError> {
            Ok(self.text.clone())
        }
    }

    fn fixture_catalog(dir: &Path) -> Arc<CatalogService> {
        let fixture = dir.join("models.json");
        std::fs::write(&fixture, FIXTURE).unwrap();
        let cfg = opencode_core::CatalogConfig {
            models_path_override: Some(fixture),
            ..opencode_core::CatalogConfig::from_env()
        };
        Arc::new(CatalogService::new(
            dir.join("cache"),
            cfg,
            Arc::new(opencode_core::catalog::SystemClock),
            Arc::new(FixtureFetcher {
                text: FIXTURE.to_string(),
            }),
        ))
    }

    fn auth_store(dir: &Path) -> FileAuthStore {
        FileAuthStore::new(dir.join("auth.json"))
    }

    fn login_deps<'a>(
        auth: &'a FileAuthStore,
        catalog: &'a CatalogService,
        wellknown: &'a mut dyn WellKnown,
        prompter: &'a mut dyn Prompter,
    ) -> LoginDeps<'a> {
        LoginDeps {
            auth,
            catalog,
            wellknown,
            prompter,
            disabled_providers: Vec::new(),
            enabled_providers: None,
        }
    }

    // ------------------------------------------------------------------
    // list
    // ------------------------------------------------------------------

    /// `std::env` is process-global; the env-var assertions below must
    /// not interleave (the C6 chunk fixed the pre-existing race).
    static ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    #[test]
    fn list_prints_credentials_and_path() {
        let _guard = ENV_LOCK.lock().unwrap();
        std::env::remove_var("OCPROVIDERS_TEST_ENV_KEY");
        let dir = tempfile::tempdir().unwrap();
        let paths = GlobalPaths::resolve(PathBuf::from(dir.path()).join("home"));
        let auth = FileAuthStore::new(paths.data.join("auth.json"));
        auth.set("anthropic", serde_json::json!({"type": "api", "key": "sk"}))
            .unwrap();
        auth.set(
            "https://example.com",
            serde_json::json!({"type": "wellknown", "key": "MY_TOKEN", "token": "tok"}),
        )
        .unwrap();
        let catalog = fixture_catalog(dir.path());
        let providers = catalog.get().unwrap();

        let (mut ui, captured) = Ui::capture(false);
        list(
            &mut ui,
            &auth,
            &providers,
            &paths.data.join("auth.json"),
            &paths.home,
        )
        .unwrap();
        let stderr = captured.stderr();
        assert_eq!(
            stderr,
            format!(
                "{reset}\n┌ Credentials {dim}~{auth_file}{normal}\n│ Anthropic {dim}api{normal}\n│ https://example.com {dim}wellknown{normal}\n└ 2 credentials\n",
                reset = style::TEXT_NORMAL,
                dim = style::TEXT_DIM,
                normal = style::TEXT_NORMAL,
                auth_file = "/.local/share/opencode/auth.json",
            )
        );
    }

    #[test]
    fn list_falls_back_to_key_for_unknown_providers() {
        let dir = tempfile::tempdir().unwrap();
        let auth = auth_store(dir.path());
        auth.set(
            "not-a-provider",
            serde_json::json!({"type": "api", "key": "k"}),
        )
        .unwrap();
        let catalog = fixture_catalog(dir.path());
        let (mut ui, captured) = Ui::capture(false);
        list(
            &mut ui,
            &auth,
            &catalog.get().unwrap(),
            &dir.path().join("auth.json"),
            Path::new("/nope"),
        )
        .unwrap();
        let stderr = captured.stderr();
        assert!(
            stderr.contains(&format!("│ not-a-provider {}api", style::TEXT_DIM)),
            "{stderr}"
        );
        assert!(stderr.contains("└ 1 credentials"), "{stderr}");
        assert_eq!(captured.stdout(), "");
    }

    #[test]
    fn list_prints_environment_section_for_set_env_vars() {
        let _guard = ENV_LOCK.lock().unwrap();
        let dir = tempfile::tempdir().unwrap();
        let auth = auth_store(dir.path());
        std::env::set_var("OCPROVIDERS_TEST_ENV_KEY", "1");
        let catalog = fixture_catalog(dir.path());
        let (mut ui, captured) = Ui::capture(false);
        let result = list(
            &mut ui,
            &auth,
            &catalog.get().unwrap(),
            &dir.path().join("auth.json"),
            Path::new("/nope"),
        );
        std::env::remove_var("OCPROVIDERS_TEST_ENV_KEY");
        result.unwrap();
        let stderr = captured.stderr();
        assert!(
            stderr.contains(&format!(
                "│ Acme {}OCPROVIDERS_TEST_ENV_KEY",
                style::TEXT_DIM
            )),
            "{stderr}"
        );
        assert!(stderr.contains("└ 1 environment variable\n"), "{stderr}");
    }

    #[test]
    fn list_without_env_vars_has_no_environment_section() {
        let _guard = ENV_LOCK.lock().unwrap();
        std::env::remove_var("OCPROVIDERS_TEST_ENV_KEY");
        let dir = tempfile::tempdir().unwrap();
        let auth = auth_store(dir.path());
        let catalog = fixture_catalog(dir.path());
        let (mut ui, captured) = Ui::capture(false);
        list(
            &mut ui,
            &auth,
            &catalog.get().unwrap(),
            &dir.path().join("auth.json"),
            Path::new("/nope"),
        )
        .unwrap();
        assert!(!captured.stderr().contains("Environment"));
    }

    #[test]
    fn display_path_replaces_home_prefix_only() {
        assert_eq!(
            display_path(
                Path::new("/home/user/.local/share/opencode/auth.json"),
                Path::new("/home/user")
            ),
            "~/.local/share/opencode/auth.json"
        );
        assert_eq!(
            display_path(Path::new("/elsewhere/auth.json"), Path::new("/home/user")),
            "/elsewhere/auth.json"
        );
    }

    // ------------------------------------------------------------------
    // login — well-known URL flow
    // ------------------------------------------------------------------

    #[test]
    fn login_url_stores_wellknown_token() {
        let dir = tempfile::tempdir().unwrap();
        let auth = auth_store(dir.path());
        let catalog = fixture_catalog(dir.path());
        let mut wellknown =
            ScriptWellKnown::ok(vec!["opencode", "auth"], "MY_TOKEN", "  secret \n");
        let mut prompter = ScriptPrompter::new("", "", "");
        let args = LoginArgs {
            url: Some("https://example.com".to_string()),
            provider: None,
        };
        let (mut ui, captured) = Ui::capture(false);
        login(
            &mut ui,
            &args,
            &mut login_deps(&auth, &catalog, &mut wellknown, &mut prompter),
        )
        .unwrap();
        let stored = auth.all().unwrap();
        assert_eq!(
            stored["https://example.com"],
            serde_json::json!({"type": "wellknown", "key": "MY_TOKEN", "token": "secret"})
        );
        let stderr = captured.stderr();
        assert!(stderr.contains("│ Running `opencode auth`"), "{stderr}");
        assert!(
            stderr.contains("│ Logged into https://example.com"),
            "{stderr}"
        );
        assert!(stderr.ends_with("└ Done\n"), "{stderr}");
        // metadata fetch goes to the normalized URL
        let fetched = wellknown.fetched.lock().unwrap().clone();
        assert_eq!(fetched, vec!["https://example.com".to_string()]);
    }

    #[test]
    fn login_url_trims_trailing_slashes() {
        let dir = tempfile::tempdir().unwrap();
        let auth = auth_store(dir.path());
        let catalog = fixture_catalog(dir.path());
        let mut wellknown = ScriptWellKnown::ok(vec!["cmd"], "MY_TOKEN", "tok");
        let mut prompter = ScriptPrompter::new("", "", "");
        let args = LoginArgs {
            url: Some("https://example.com//".to_string()),
            provider: None,
        };
        let (mut ui, _captured) = Ui::capture(false);
        login(
            &mut ui,
            &args,
            &mut login_deps(&auth, &catalog, &mut wellknown, &mut prompter),
        )
        .unwrap();
        let stored = auth.all().unwrap();
        assert!(stored.contains_key("https://example.com"));
        assert_eq!(stored.len(), 1);
    }

    #[test]
    fn login_url_failed_command_prints_failed() {
        let dir = tempfile::tempdir().unwrap();
        let auth = auth_store(dir.path());
        let catalog = fixture_catalog(dir.path());
        let mut wellknown = ScriptWellKnown::fail_run();
        let mut prompter = ScriptPrompter::new("", "", "");
        let args = LoginArgs {
            url: Some("https://example.com".to_string()),
            provider: None,
        };
        let (mut ui, captured) = Ui::capture(false);
        login(
            &mut ui,
            &args,
            &mut login_deps(&auth, &catalog, &mut wellknown, &mut prompter),
        )
        .unwrap();
        let stderr = captured.stderr();
        assert!(stderr.contains("│ Failed\n"), "{stderr}");
        assert!(stderr.ends_with("└ Done\n"), "{stderr}");
        assert!(auth.all().unwrap().is_empty());
    }

    #[test]
    fn login_url_metadata_failure_is_a_cli_error() {
        let dir = tempfile::tempdir().unwrap();
        let auth = auth_store(dir.path());
        let catalog = fixture_catalog(dir.path());
        let mut wellknown = ScriptWellKnown::fail_fetch();
        let mut prompter = ScriptPrompter::new("", "", "");
        let args = LoginArgs {
            url: Some("https://example.com".to_string()),
            provider: None,
        };
        let (mut ui, _captured) = Ui::capture(false);
        let err = login(
            &mut ui,
            &args,
            &mut login_deps(&auth, &catalog, &mut wellknown, &mut prompter),
        )
        .unwrap_err();
        assert_eq!(
            crate::error::format_error(&err),
            Some(
                "Failed to load auth provider metadata from https://example.com: fetch failed"
                    .to_string()
            )
        );
        assert_eq!(err.exit_code(), 1);
    }

    // ------------------------------------------------------------------
    // login — provider flow
    // ------------------------------------------------------------------

    #[test]
    fn login_provider_matches_by_id_and_name() {
        for input in ["anthropic", "Anthropic", "ANTHROPIC"] {
            let dir = tempfile::tempdir().unwrap();
            let auth = auth_store(dir.path());
            let catalog = fixture_catalog(dir.path());
            let mut wellknown = ScriptWellKnown::fail_run();
            let mut prompter = ScriptPrompter::new("", "", "sk-test");
            let args = LoginArgs {
                url: None,
                provider: Some(input.to_string()),
            };
            let (mut ui, _captured) = Ui::capture(false);
            login(
                &mut ui,
                &args,
                &mut login_deps(&auth, &catalog, &mut wellknown, &mut prompter),
            )
            .unwrap();
            let stored = auth.all().unwrap();
            assert_eq!(
                stored["anthropic"],
                serde_json::json!({"type": "api", "key": "sk-test"}),
                "{input}"
            );
        }
    }

    #[test]
    fn login_unknown_provider_fails() {
        let dir = tempfile::tempdir().unwrap();
        let auth = auth_store(dir.path());
        let catalog = fixture_catalog(dir.path());
        let mut wellknown = ScriptWellKnown::fail_run();
        let mut prompter = ScriptPrompter::new("", "", "sk-test");
        let args = LoginArgs {
            url: None,
            provider: Some("nope".to_string()),
        };
        let (mut ui, _captured) = Ui::capture(false);
        let err = login(
            &mut ui,
            &args,
            &mut login_deps(&auth, &catalog, &mut wellknown, &mut prompter),
        )
        .unwrap_err();
        assert_eq!(
            crate::error::format_error(&err),
            Some("Unknown provider \"nope\"".to_string())
        );
        assert!(auth.all().unwrap().is_empty());
    }

    #[test]
    fn login_filters_disabled_and_enabled_providers() {
        let dir = tempfile::tempdir().unwrap();
        let auth = auth_store(dir.path());
        let catalog = fixture_catalog(dir.path());
        let mut wellknown = ScriptWellKnown::fail_run();
        let mut prompter = ScriptPrompter::new("", "", "sk");
        let mut deps = login_deps(&auth, &catalog, &mut wellknown, &mut prompter);
        deps.disabled_providers = vec!["anthropic".to_string()];
        let all = catalog.get().unwrap();
        let providers = ordered_providers(&deps, &all);
        let ids: Vec<&str> = providers.iter().map(|(_, id)| id.as_str()).collect();
        assert!(!ids.contains(&"anthropic"));

        let mut deps = login_deps(&auth, &catalog, &mut wellknown, &mut prompter);
        deps.enabled_providers = Some(vec!["acme".to_string()]);
        let providers = ordered_providers(&deps, &all);
        assert_eq!(providers, vec![("Acme".to_string(), "acme".to_string())]);
    }

    #[test]
    fn login_provider_order_follows_priority_map() {
        let dir = tempfile::tempdir().unwrap();
        let auth = auth_store(dir.path());
        let catalog = fixture_catalog(dir.path());
        let mut wellknown = ScriptWellKnown::fail_run();
        let mut prompter = ScriptPrompter::new("", "", "sk");
        let deps = login_deps(&auth, &catalog, &mut wellknown, &mut prompter);
        let providers = ordered_providers(&deps, &catalog.get().unwrap());
        let ids: Vec<&str> = providers.iter().map(|(_, id)| id.as_str()).collect();
        assert_eq!(ids, vec!["opencode", "openai", "anthropic", "acme"]);
    }

    #[test]
    fn login_provider_hints() {
        for (provider, hint) in [
            ("opencode", "Create an api key at https://opencode.ai/auth"),
            (
                "vercel",
                "You can create an api key at https://vercel.link/ai-gateway-token",
            ),
            (
                "cloudflare",
                "Cloudflare AI Gateway can be configured with CLOUDFLARE_GATEWAY_ID",
            ),
        ] {
            let dir = tempfile::tempdir().unwrap();
            let auth = auth_store(dir.path());
            let catalog = fixture_catalog(dir.path());
            let mut wellknown = ScriptWellKnown::fail_run();
            let mut prompter = ScriptPrompter::new("", "", "sk");
            let mut deps = login_deps(&auth, &catalog, &mut wellknown, &mut prompter);
            deps.disabled_providers = Vec::new();
            let (mut ui, captured) = Ui::capture(false);
            login_api_key_flow(&mut ui, &mut deps, provider).unwrap();
            assert!(captured.stderr().contains(hint), "{provider}");
        }
    }

    #[test]
    fn login_bedrock_hint_lists_credentials() {
        let dir = tempfile::tempdir().unwrap();
        let auth = auth_store(dir.path());
        let catalog = fixture_catalog(dir.path());
        let mut wellknown = ScriptWellKnown::fail_run();
        let mut prompter = ScriptPrompter::new("", "", "sk");
        let (mut ui, captured) = Ui::capture(false);
        login_api_key_flow(
            &mut ui,
            &mut login_deps(&auth, &catalog, &mut wellknown, &mut prompter),
            "amazon-bedrock",
        )
        .unwrap();
        let stderr = captured.stderr();
        assert!(
            stderr.contains("Amazon Bedrock authentication priority:"),
            "{stderr}"
        );
        assert!(
            stderr.contains("AWS_BEARER_TOKEN_BEDROCK or /connect"),
            "{stderr}"
        );
    }

    #[test]
    fn login_selects_provider_via_prompter() {
        let dir = tempfile::tempdir().unwrap();
        let auth = auth_store(dir.path());
        let catalog = fixture_catalog(dir.path());
        let mut wellknown = ScriptWellKnown::fail_run();
        let mut prompter = ScriptPrompter::new("anthropic", "", "sk-test");
        let args = LoginArgs::default();
        let (mut ui, _captured) = Ui::capture(false);
        login(
            &mut ui,
            &args,
            &mut login_deps(&auth, &catalog, &mut wellknown, &mut prompter),
        )
        .unwrap();
        assert_eq!(prompter.selects.load(Ordering::SeqCst), 1);
        assert_eq!(
            auth.all().unwrap()["anthropic"],
            serde_json::json!({"type": "api", "key": "sk-test"})
        );
    }

    #[test]
    fn login_other_degrades_to_api_key_flow() {
        let dir = tempfile::tempdir().unwrap();
        let auth = auth_store(dir.path());
        let catalog = fixture_catalog(dir.path());
        let mut wellknown = ScriptWellKnown::fail_run();
        let mut prompter = ScriptPrompter::new("other", "acme-custom", "sk-test");
        let args = LoginArgs::default();
        let (mut ui, captured) = Ui::capture(false);
        login(
            &mut ui,
            &args,
            &mut login_deps(&auth, &catalog, &mut wellknown, &mut prompter),
        )
        .unwrap();
        let stored = auth.all().unwrap();
        assert_eq!(
            stored["acme-custom"],
            serde_json::json!({"type": "api", "key": "sk-test"})
        );
        assert!(
            captured
                .stderr()
                .contains("This only stores a credential for acme-custom"),
            "{}",
            captured.stderr()
        );
    }

    // ------------------------------------------------------------------
    // logout
    // ------------------------------------------------------------------

    #[test]
    fn logout_removes_credential_by_id_and_name() {
        for input in ["anthropic", "Anthropic"] {
            let dir = tempfile::tempdir().unwrap();
            let auth = auth_store(dir.path());
            auth.set("anthropic", serde_json::json!({"type": "api", "key": "sk"}))
                .unwrap();
            let catalog = fixture_catalog(dir.path());
            let mut prompter = ScriptPrompter::new("", "", "");
            let (mut ui, captured) = Ui::capture(false);
            logout(&mut ui, &auth, &catalog, &mut prompter, Some(input)).unwrap();
            assert!(auth.all().unwrap().is_empty(), "{input}");
            assert!(captured.stderr().contains("└ Logout successful"), "{input}");
        }
    }

    #[test]
    fn logout_unknown_provider_fails() {
        let dir = tempfile::tempdir().unwrap();
        let auth = auth_store(dir.path());
        auth.set("anthropic", serde_json::json!({"type": "api", "key": "sk"}))
            .unwrap();
        let catalog = fixture_catalog(dir.path());
        let mut prompter = ScriptPrompter::new("", "", "");
        let (mut ui, _captured) = Ui::capture(false);
        let err = logout(&mut ui, &auth, &catalog, &mut prompter, Some("nope")).unwrap_err();
        assert_eq!(
            crate::error::format_error(&err),
            Some("Unknown configured provider \"nope\"".to_string())
        );
        assert!(!auth.all().unwrap().is_empty());
    }

    #[test]
    fn logout_without_credentials_prints_no_credentials() {
        let dir = tempfile::tempdir().unwrap();
        let auth = auth_store(dir.path());
        let catalog = fixture_catalog(dir.path());
        let mut prompter = ScriptPrompter::new("", "", "");
        let (mut ui, captured) = Ui::capture(false);
        logout(&mut ui, &auth, &catalog, &mut prompter, Some("anthropic")).unwrap();
        let stderr = captured.stderr();
        assert!(stderr.contains("│ No credentials found"), "{stderr}");
        assert!(!stderr.contains("└ Logout successful"), "{stderr}");
    }

    #[test]
    fn logout_without_argument_prompts() {
        let dir = tempfile::tempdir().unwrap();
        let auth = auth_store(dir.path());
        auth.set("anthropic", serde_json::json!({"type": "api", "key": "sk"}))
            .unwrap();
        let catalog = fixture_catalog(dir.path());
        let mut prompter = ScriptPrompter::new("anthropic", "", "");
        let (mut ui, _captured) = Ui::capture(false);
        logout(&mut ui, &auth, &catalog, &mut prompter, None).unwrap();
        assert!(auth.all().unwrap().is_empty());
        assert_eq!(prompter.selects.load(Ordering::SeqCst), 1);
    }

    static FIXTURE: &str = r#"{
      "anthropic": {
        "name": "Anthropic",
        "env": ["ANTHROPIC_API_KEY"],
        "id": "anthropic",
        "models": {
          "claude-3-7": {
            "id": "claude-3-7",
            "name": "Claude 3.7",
            "release_date": "2025-02-24",
            "attachment": true,
            "reasoning": true,
            "temperature": true,
            "tool_call": true,
            "limit": {"context": 200000, "output": 8192}
          }
        }
      },
      "opencode": {
        "name": "opencode",
        "env": ["OPENCODE_API_KEY"],
        "id": "opencode",
        "models": {
          "grok-code": {
            "id": "grok-code",
            "name": "Grok Code",
            "release_date": "2025-08-01",
            "attachment": false,
            "reasoning": true,
            "temperature": true,
            "tool_call": true,
            "limit": {"context": 200000, "output": 8192}
          }
        }
      },
      "openai": {
        "name": "OpenAI",
        "env": ["OCPROVIDERS_TEST_OPENAI_KEY"],
        "id": "openai",
        "models": {
          "gpt-5.1": {
            "id": "gpt-5.1",
            "name": "GPT-5.1",
            "release_date": "2025-08-07",
            "attachment": true,
            "reasoning": true,
            "temperature": true,
            "tool_call": true,
            "limit": {"context": 400000, "output": 8192}
          }
        }
      },
      "acme": {
        "name": "Acme",
        "env": ["OCPROVIDERS_TEST_ENV_KEY"],
        "id": "acme",
        "models": {
          "acme-1": {
            "id": "acme-1",
            "name": "Acme One",
            "release_date": "2025-09-01",
            "attachment": false,
            "reasoning": false,
            "temperature": false,
            "tool_call": true,
            "limit": {"context": 100000, "output": 4096}
          }
        }
      }
    }"#;
}
