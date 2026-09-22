//! cli/network.ts port: the `--port/--hostname/--mdns/--mdns-domain/--cors`
//! options and their config-vs-flag precedence.

use std::collections::HashMap;
use std::ffi::OsString;
use std::fs;
use std::io::ErrorKind;
use std::path::{Path, PathBuf};

use alforria_core::config::schema::ServerInfo;
use alforria_core::config::variable::{substitute, Missing, Source};
use alforria_core::{merge_config_concat_arrays, parse_jsonc, GlobalPaths};
use clap::value_parser;
use clap::{Arg, ArgAction, ArgMatches, Command};
use serde_json::Value;

/// network.ts:7-11 — `port: { default: 0 }`.
pub const DEFAULT_PORT: u16 = 0;
/// network.ts:12-16 — `hostname: { default: "127.0.0.1" }`.
pub const DEFAULT_HOSTNAME: &str = "127.0.0.1";
/// network.ts:22-26 — `"mdns-domain": { default: "opencode.local" }`.
pub const DEFAULT_MDNS_DOMAIN: &str = "opencode.local";

/// network.ts:6-33 — the parsed network option values.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NetworkOptions {
    pub port: u16,
    pub hostname: String,
    pub mdns: bool,
    pub mdns_domain: String,
    pub cors: Vec<String>,
}

impl NetworkOptions {
    pub fn from_matches(matches: &ArgMatches) -> Self {
        Self {
            port: matches
                .get_one::<u16>("port")
                .copied()
                .unwrap_or(DEFAULT_PORT),
            hostname: matches
                .get_one::<String>("hostname")
                .cloned()
                .unwrap_or_else(|| DEFAULT_HOSTNAME.to_string()),
            mdns: matches.get_one::<bool>("mdns").copied().unwrap_or(false),
            mdns_domain: matches
                .get_one::<String>("mdns-domain")
                .cloned()
                .unwrap_or_else(|| DEFAULT_MDNS_DOMAIN.to_string()),
            cors: matches
                .get_many::<String>("cors")
                .map(|values| values.cloned().collect())
                .unwrap_or_default(),
        }
    }
}

/// The `config.server` subset consulted by `resolveNetworkOptionsNoConfig`.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ServerConfig {
    pub port: Option<u16>,
    pub hostname: Option<String>,
    pub mdns: Option<bool>,
    pub mdns_domain: Option<String>,
    pub cors: Option<Vec<String>>,
}

/// network.ts:62-79 — the resolved options handed to `Server.listen`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolvedNetworkOptions {
    pub port: u16,
    pub hostname: String,
    pub mdns: bool,
    pub mdns_domain: String,
    pub cors: Vec<String>,
}

impl ResolvedNetworkOptions {
    pub fn listen_options(&self) -> alforria_server::ListenOptions {
        alforria_server::ListenOptions {
            port: self.port,
            hostname: self.hostname.clone(),
            cors: self.cors.clone(),
        }
    }
}

/// `withNetworkOptions` (network.ts:37-38).
pub fn with_network_options(cmd: Command) -> Command {
    cmd.arg(
        Arg::new("port")
            .long("port")
            .help("port to listen on")
            .value_parser(value_parser!(u16))
            .default_value("0"),
    )
    .arg(
        Arg::new("hostname")
            .long("hostname")
            .help("hostname to listen on")
            .default_value(DEFAULT_HOSTNAME),
    )
    .arg(
        Arg::new("mdns")
            .long("mdns")
            .help("enable mDNS service discovery (defaults hostname to 0.0.0.0)")
            .value_parser(value_parser!(bool))
            .num_args(0..=1)
            .default_missing_value("true")
            .default_value("false"),
    )
    .arg(
        Arg::new("mdns-domain")
            .long("mdns-domain")
            .help("custom domain name for mDNS service (default: opencode.local)")
            .default_value(DEFAULT_MDNS_DOMAIN),
    )
    .arg(
        Arg::new("cors")
            .long("cors")
            .help("additional domains to allow for CORS")
            .action(ArgAction::Append),
    )
}

/// `resolveNetworkOptionsNoConfig` (network.ts:62-79): explicit CLI flag >
/// `config.server` > parsed default. Explicitness is determined by raw argv
/// inspection, not by default equality.
pub fn resolve(
    args: &NetworkOptions,
    raw: &[OsString],
    config: &ServerConfig,
) -> ResolvedNetworkOptions {
    let argv = network_args(raw);
    let mdns = if has_boolean_arg(argv, "--mdns") {
        args.mdns
    } else {
        config.mdns.unwrap_or(args.mdns)
    };
    let mdns_domain = if has_arg(argv, "--mdns-domain") {
        args.mdns_domain.clone()
    } else {
        config
            .mdns_domain
            .clone()
            .unwrap_or_else(|| args.mdns_domain.clone())
    };
    let port = if has_arg(argv, "--port") {
        args.port
    } else {
        config.port.unwrap_or(args.port)
    };
    let hostname = if has_arg(argv, "--hostname") {
        args.hostname.clone()
    } else if mdns && config.hostname.is_none() {
        "0.0.0.0".to_string()
    } else {
        config
            .hostname
            .clone()
            .unwrap_or_else(|| args.hostname.clone())
    };
    let mut cors = config.cors.clone().unwrap_or_default();
    cors.extend(args.cors.iter().cloned());
    ResolvedNetworkOptions {
        port,
        hostname,
        mdns,
        mdns_domain,
        cors,
    }
}

/// network.ts:51-54 — raw argv up to a `--` separator.
pub fn raw_args(raw: &[OsString]) -> &[OsString] {
    network_args(raw)
}

fn network_args(raw: &[OsString]) -> &[OsString] {
    let separator = raw.iter().position(|arg| arg == "--");
    separator.map_or(raw, |index| &raw[..index])
}

/// network.ts:41-43 — `arg === name || arg.startsWith(name + "=")`.
pub fn has_arg(argv: &[OsString], name: &str) -> bool {
    let prefix = format!("{name}=");
    argv.iter().any(|arg| {
        let arg = arg.to_string_lossy();
        arg == name || arg.starts_with(&prefix)
    })
}

/// network.ts:45-48 — boolean flags also match `=true`/`=false`/`--no-` forms.
fn has_boolean_arg(argv: &[OsString], name: &str) -> bool {
    let true_form = format!("{name}=true");
    let false_form = format!("{name}=false");
    let negated = format!("--no-{}", &name[2..]);
    argv.iter().any(|arg| {
        let arg = arg.to_string_lossy();
        arg == name || arg == true_form || arg == false_form || arg == negated
    })
}

/// The empty global-config view of `resolveNetworkOptionsNoConfig`
/// (network.ts:62-79 with no config values).
pub fn empty_server_config() -> ServerConfig {
    ServerConfig::default()
}

/// `resolveNetworkOptions` (network.ts:56-59): the `Config.getGlobal()` view.
pub fn global_server_config() -> ServerConfig {
    let paths = GlobalPaths::from_env();
    seed_global_config(&paths);
    server_config(&paths)
}

/// config.ts:260-270 — seed the default global config file unless config is
/// routed through env-provided paths.
fn seed_global_config(paths: &GlobalPaths) {
    let routed = [
        "OPENCODE_CONFIG",
        "OPENCODE_CONFIG_DIR",
        "OPENCODE_CONFIG_CONTENT",
    ]
    .iter()
    .any(|name| std::env::var(name).is_ok_and(|value| !value.is_empty()));
    if routed {
        return;
    }
    let file = global_config_file(&paths.config);
    if !file.exists() {
        let seeded = serde_json::json!({ "$schema": "https://opencode.ai/config.json" });
        if let Ok(text) = serde_json::to_string_pretty(&seeded) {
            let _ = fs::create_dir_all(&paths.config);
            let _ = fs::write(&file, text);
        }
    }
}

/// config.ts:140-148 — first existing of `opencode.jsonc`, `opencode.json`,
/// `config.json`, else `opencode.jsonc`.
fn global_config_file(config_dir: &Path) -> PathBuf {
    for name in ["opencode.jsonc", "opencode.json", "config.json"] {
        let candidate = config_dir.join(name);
        if candidate.exists() {
            return candidate;
        }
    }
    config_dir.join("opencode.jsonc")
}

/// The `config.server` subset of the merged global config (config.ts:260-283
/// `loadGlobal`): `config.json`, `opencode.json`, `opencode.jsonc` in order.
/// Any load failure degrades the whole config to `{}` (config.ts:295-301
/// `orElseSucceed`).
pub fn server_config(paths: &GlobalPaths) -> ServerConfig {
    let mut merged = Value::Object(serde_json::Map::new());
    for name in ["config.json", "opencode.json", "opencode.jsonc"] {
        match read_config_file(&paths.config.join(name)) {
            ConfigFile::Missing => {}
            ConfigFile::Fatal => return ServerConfig::default(),
            ConfigFile::Loaded(next) => merge_config_concat_arrays(&mut merged, &next),
        }
    }
    let Some(server) = merged.get("server") else {
        return ServerConfig::default();
    };
    let Ok(server) = serde_json::from_value::<ServerInfo>(server.clone()) else {
        return ServerConfig::default();
    };
    ServerConfig {
        port: server.port.and_then(|port| u16::try_from(port.get()).ok()),
        hostname: server.hostname,
        mdns: server.mdns,
        mdns_domain: server.mdns_domain,
        cors: server.cors,
    }
}

/// config.ts `loadFile` semantics: missing/empty files contribute `{}`;
/// substitution/parse failures are fatal.
enum ConfigFile {
    Missing,
    Loaded(Value),
    Fatal,
}

fn read_config_file(path: &Path) -> ConfigFile {
    let text = match fs::read_to_string(path) {
        Ok(text) => text,
        Err(err) if err.kind() == ErrorKind::NotFound => return ConfigFile::Missing,
        Err(err) if err.kind() == ErrorKind::PermissionDenied => return ConfigFile::Missing,
        Err(_) => return ConfigFile::Fatal,
    };
    if text.is_empty() {
        return ConfigFile::Missing;
    }
    let empty = HashMap::new();
    let expanded = match substitute(
        &text,
        &Source::Path(path.to_path_buf()),
        &empty,
        Missing::Error,
    ) {
        Ok(expanded) => expanded,
        Err(_) => return ConfigFile::Fatal,
    };
    match parse_jsonc(&expanded, path) {
        Ok(value) => ConfigFile::Loaded(value),
        Err(_) => ConfigFile::Fatal,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn raw(parts: &[&str]) -> Vec<OsString> {
        parts.iter().map(OsString::from).collect()
    }

    fn options() -> NetworkOptions {
        NetworkOptions {
            port: DEFAULT_PORT,
            hostname: DEFAULT_HOSTNAME.to_string(),
            mdns: false,
            mdns_domain: DEFAULT_MDNS_DOMAIN.to_string(),
            cors: Vec::new(),
        }
    }

    fn server_config_of(
        port: Option<u16>,
        hostname: Option<&str>,
        mdns: Option<bool>,
        mdns_domain: Option<&str>,
        cors: &[&str],
    ) -> ServerConfig {
        ServerConfig {
            port,
            hostname: hostname.map(str::to_string),
            mdns,
            mdns_domain: mdns_domain.map(str::to_string),
            cors: (!cors.is_empty()).then(|| cors.iter().map(|s| s.to_string()).collect()),
        }
    }

    #[test]
    fn has_arg_matches_exact_and_equals_forms() {
        let argv = raw(&["serve", "--port", "9000", "--hostname=example.com"]);
        assert!(has_arg(&argv, "--port"));
        assert!(has_arg(&argv, "--hostname"));
        assert!(!has_arg(&argv, "--host"));
        assert!(!has_arg(&argv, "--mdns-domain"));
    }

    #[test]
    fn has_boolean_arg_matches_all_four_forms() {
        for argv in [
            raw(&["--mdns"]),
            raw(&["--mdns=true"]),
            raw(&["--mdns=false"]),
            raw(&["--no-mdns"]),
        ] {
            assert!(has_boolean_arg(&argv, "--mdns"), "{argv:?}");
        }
        assert!(!has_boolean_arg(&raw(&["--mdns-domain", "x"]), "--mdns"));
        assert!(!has_boolean_arg(&raw(&[]), "--mdns"));
    }

    #[test]
    fn resolve_uses_defaults_when_nothing_is_explicit() {
        let resolved = resolve(&options(), &raw(&["serve"]), &ServerConfig::default());
        assert_eq!(resolved.port, 0);
        assert_eq!(resolved.hostname, "127.0.0.1");
        assert!(!resolved.mdns);
        assert_eq!(resolved.mdns_domain, "opencode.local");
        assert!(resolved.cors.is_empty());
    }

    #[test]
    fn resolve_explicit_flags_override_config() {
        let mut args = options();
        args.port = 9000;
        args.hostname = "example.com".to_string();
        let config = server_config_of(
            Some(8080),
            Some("cfg.example"),
            Some(true),
            Some("c.local"),
            &[],
        );
        let resolved = resolve(
            &args,
            &raw(&["serve", "--port", "9000", "--hostname", "example.com"]),
            &config,
        );
        assert_eq!(resolved.port, 9000);
        assert_eq!(resolved.hostname, "example.com");
    }

    #[test]
    fn resolve_equals_form_counts_as_explicit() {
        let mut args = options();
        args.port = 9000;
        let resolved = resolve(
            &args,
            &raw(&["serve", "--port=9000"]),
            &server_config_of(Some(8080), None, None, None, &[]),
        );
        assert_eq!(resolved.port, 9000);
    }

    #[test]
    fn resolve_config_used_when_flags_absent() {
        let config = server_config_of(
            Some(8080),
            Some("cfg.example"),
            Some(true),
            Some("cfg.local"),
            &["https://cfg"],
        );
        let resolved = resolve(&options(), &raw(&["serve"]), &config);
        assert_eq!(resolved.port, 8080);
        assert_eq!(resolved.hostname, "cfg.example");
        assert!(resolved.mdns);
        assert_eq!(resolved.mdns_domain, "cfg.local");
        assert_eq!(resolved.cors, vec!["https://cfg".to_string()]);
    }

    #[test]
    fn resolve_mdns_flag_forces_any_host_without_config_hostname() {
        let mut args = options();
        args.mdns = true;
        let resolved = resolve(&args, &raw(&["serve", "--mdns"]), &ServerConfig::default());
        assert!(resolved.mdns);
        assert_eq!(resolved.hostname, "0.0.0.0");
    }

    #[test]
    fn resolve_mdns_config_forces_any_host_without_config_hostname() {
        let resolved = resolve(
            &options(),
            &raw(&["serve"]),
            &server_config_of(None, None, Some(true), None, &[]),
        );
        assert!(resolved.mdns);
        assert_eq!(resolved.hostname, "0.0.0.0");
    }

    #[test]
    fn resolve_mdns_keeps_config_hostname() {
        let resolved = resolve(
            &options(),
            &raw(&["serve", "--mdns"]),
            &server_config_of(None, Some("example.com"), Some(true), None, &[]),
        );
        assert_eq!(resolved.hostname, "example.com");
    }

    #[test]
    fn resolve_mdns_false_flag_overrides_config_mdns() {
        let resolved = resolve(
            &options(),
            &raw(&["serve", "--mdns=false"]),
            &server_config_of(None, None, Some(true), None, &[]),
        );
        assert!(!resolved.mdns);
        assert_eq!(resolved.hostname, "127.0.0.1");
    }

    #[test]
    fn resolve_explicit_hostname_wins_over_mdns_force() {
        let mut args = options();
        args.mdns = true;
        let resolved = resolve(
            &args,
            &raw(&["serve", "--mdns", "--hostname", "127.0.0.1"]),
            &ServerConfig::default(),
        );
        assert!(resolved.mdns);
        assert_eq!(resolved.hostname, "127.0.0.1");
    }

    #[test]
    fn resolve_cors_merges_config_then_args() {
        let mut args = options();
        args.cors = vec!["https://arg".to_string()];
        let config = server_config_of(None, None, None, None, &["https://cfg"]);
        let resolved = resolve(&args, &raw(&["serve"]), &config);
        assert_eq!(
            resolved.cors,
            vec!["https://cfg".to_string(), "https://arg".to_string()]
        );
    }

    #[test]
    fn resolve_double_dash_hides_flags() {
        let resolved = resolve(
            &options(),
            &raw(&["serve", "--", "--port=9000"]),
            &server_config_of(Some(8080), None, None, None, &[]),
        );
        assert_eq!(resolved.port, 8080);
    }

    #[test]
    fn resolve_listen_options_maps_hostname_port_cors() {
        let mut args = options();
        args.port = 1;
        let resolved = resolve(
            &args,
            &raw(&["serve", "--port", "1"]),
            &ServerConfig::default(),
        );
        let listen = resolved.listen_options();
        assert_eq!(listen.port, 1);
        assert_eq!(listen.hostname, "127.0.0.1");
        assert!(listen.cors.is_empty());
    }

    #[test]
    fn server_config_merges_global_files_in_order() {
        let root = tempfile::tempdir().unwrap();
        let paths = paths_at(root.path());
        fs::create_dir_all(&paths.config).unwrap();
        fs::write(
            paths.config.join("config.json"),
            r#"{"server": {"port": 4096, "hostname": "localhost"}}"#,
        )
        .unwrap();
        fs::write(
            paths.config.join("opencode.json"),
            r#"{"server": {"hostname": "example.com", "mdns": true, "mdnsDomain": "dev.local", "cors": ["https://cfg"]}}"#,
        )
        .unwrap();
        assert_eq!(
            server_config(&paths),
            server_config_of(
                Some(4096),
                Some("example.com"),
                Some(true),
                Some("dev.local"),
                &["https://cfg"],
            )
        );
    }

    #[test]
    fn server_config_degrades_to_empty_on_invalid_file() {
        let root = tempfile::tempdir().unwrap();
        let paths = paths_at(root.path());
        fs::create_dir_all(&paths.config).unwrap();
        fs::write(paths.config.join("opencode.json"), "{ not json").unwrap();
        assert_eq!(server_config(&paths), ServerConfig::default());
    }

    #[test]
    fn server_config_ignores_out_of_range_port() {
        let root = tempfile::tempdir().unwrap();
        let paths = paths_at(root.path());
        fs::create_dir_all(&paths.config).unwrap();
        fs::write(
            paths.config.join("opencode.json"),
            r#"{"server": {"port": 70000}}"#,
        )
        .unwrap();
        assert_eq!(server_config(&paths).port, None);
    }

    #[test]
    fn seeding_writes_schema_only_when_missing() {
        let root = tempfile::tempdir().unwrap();
        let paths = paths_at(root.path());
        seed_global_config(&paths);
        let seeded = fs::read_to_string(paths.config.join("opencode.jsonc")).unwrap();
        assert_eq!(
            seeded,
            "{\n  \"$schema\": \"https://opencode.ai/config.json\"\n}"
        );
    }

    #[test]
    fn seeding_targets_existing_file_and_skips_write() {
        let root = tempfile::tempdir().unwrap();
        let paths = paths_at(root.path());
        fs::create_dir_all(&paths.config).unwrap();
        fs::write(paths.config.join("config.json"), "{}").unwrap();
        seed_global_config(&paths);
        assert!(!paths.config.join("opencode.jsonc").exists());
        assert_eq!(
            fs::read_to_string(paths.config.join("config.json")).unwrap(),
            "{}"
        );
    }

    fn paths_at(root: &Path) -> GlobalPaths {
        GlobalPaths {
            home: root.join("home"),
            config: root.join("config"),
            data: root.join("data"),
            cache: root.join("cache"),
            state: root.join("state"),
        }
    }
}
