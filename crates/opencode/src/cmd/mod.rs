use std::ffi::OsString;

use clap::builder::PossibleValuesParser;
use clap::error::ErrorKind;
use clap::{Arg, ArgAction, ArgMatches, Command};

use crate::error::{CliError, TypedError};
use crate::ui::Ui;

pub fn cli() -> Command {
    Command::new("opencode")
        .version(env!("CARGO_PKG_VERSION"))
        .disable_version_flag(true)
        .arg(
            Arg::new("print-logs")
                .long("print-logs")
                .global(true)
                .help("print logs to stderr")
                .action(ArgAction::SetTrue),
        )
        .arg(
            Arg::new("log-level")
                .long("log-level")
                .global(true)
                .help("log level")
                .value_parser(PossibleValuesParser::new([
                    "DEBUG", "INFO", "WARN", "ERROR",
                ])),
        )
        .arg(
            Arg::new("pure")
                .long("pure")
                .global(true)
                .help("run without external plugins")
                .action(ArgAction::SetTrue),
        )
        .arg(
            Arg::new("version")
                .long("version")
                .short('v')
                .help("show version number")
                .action(ArgAction::Version),
        )
        .arg(Arg::new("project").help("path to start opencode in"))
        .subcommand(Command::new("acp").about("start ACP (Agent Client Protocol) server"))
        .subcommand(mcp())
        // $0 default command (TODO(C8): tui)
        .subcommand(attach())
        .subcommand(run())
        .subcommand(Command::new("generate"))
        .subcommand(debug())
        .subcommand(console())
        .subcommand(providers())
        .subcommand(agent())
        .subcommand(
            Command::new("upgrade").about("upgrade opencode to the latest or a specific version"),
        )
        .subcommand(
            Command::new("uninstall").about("uninstall opencode and remove all related files"),
        )
        .subcommand(Command::new("serve").about("starts a headless opencode server"))
        .subcommand(Command::new("web").about("start opencode server and open web interface"))
        .subcommand(
            Command::new("models")
                .about("list all available models")
                .arg(Arg::new("provider").help("list models for a specific provider")),
        )
        .subcommand(Command::new("stats").about("show token usage and cost statistics"))
        .subcommand(
            Command::new("export")
                .about("export session data as JSON")
                .arg(Arg::new("sessionID")),
        )
        .subcommand(
            Command::new("import")
                .about("import session data from JSON file or URL")
                .arg(
                    Arg::new("file")
                        .required(true)
                        .help("path to JSON file or share URL"),
                ),
        )
        .subcommand(github())
        .subcommand(
            Command::new("pr")
                .about("fetch and checkout a GitHub PR branch, then run opencode")
                .arg(
                    Arg::new("number")
                        .required(true)
                        .value_parser(clap::value_parser!(i64)),
                ),
        )
        .subcommand(session())
        .subcommand(plug())
        .subcommand(db())
}

fn attach() -> Command {
    Command::new("attach")
        .about("attach to a running opencode server")
        .arg(Arg::new("url").required(true).help("http://localhost:4096"))
}

fn run() -> Command {
    Command::new("run")
        .about("run opencode with a message")
        .arg(Arg::new("message").num_args(0..).help("message to send"))
}

fn mcp() -> Command {
    Command::new("mcp")
        .about("manage MCP (Model Context Protocol) servers")
        .subcommand_required(true)
        .subcommand(
            Command::new("list")
                .alias("ls")
                .about("list MCP servers and their status"),
        )
        .subcommand(
            Command::new("auth")
                .about("authenticate with an OAuth-enabled MCP server")
                .arg(Arg::new("name").help("name of the MCP server"))
                .subcommand(
                    Command::new("list")
                        .alias("ls")
                        .about("list OAuth-capable MCP servers and their auth status"),
                ),
        )
        .subcommand(
            Command::new("logout")
                .about("remove OAuth credentials for an MCP server")
                .arg(Arg::new("name").help("name of the MCP server")),
        )
        .subcommand(
            Command::new("add")
                .about("add an MCP server")
                .arg(Arg::new("name").help("name of the MCP server")),
        )
        .subcommand(
            Command::new("debug")
                .about("debug OAuth connection for an MCP server")
                .arg(
                    Arg::new("name")
                        .required(true)
                        .help("name of the MCP server"),
                ),
        )
}

fn debug() -> Command {
    Command::new("debug")
        .about("debugging and troubleshooting tools")
        .subcommand_required(true)
        .subcommand(Command::new("config").about("show resolved configuration"))
        .subcommand(
            Command::new("lsp")
                .about("LSP debugging utilities")
                .subcommand_required(true)
                .subcommand(
                    Command::new("diagnostics")
                        .about("get diagnostics for a file")
                        .arg(Arg::new("file").required(true)),
                )
                .subcommand(
                    Command::new("symbols")
                        .about("search workspace symbols")
                        .arg(Arg::new("query").required(true)),
                )
                .subcommand(
                    Command::new("document-symbols")
                        .about("get symbols from a document")
                        .arg(Arg::new("uri").required(true)),
                ),
        )
        .subcommand(
            Command::new("rg")
                .about("ripgrep debugging utilities")
                .subcommand_required(true)
                .subcommand(Command::new("files").about("list files using ripgrep"))
                .subcommand(
                    Command::new("search")
                        .about("search file contents using ripgrep")
                        .arg(Arg::new("pattern").required(true)),
                ),
        )
        .subcommand(
            Command::new("file")
                .about("file system debugging utilities")
                .subcommand_required(true)
                .subcommand(
                    Command::new("search")
                        .about("search files by query")
                        .arg(Arg::new("query").required(true)),
                )
                .subcommand(
                    Command::new("read")
                        .about("read file contents as JSON")
                        .arg(Arg::new("path").required(true)),
                )
                .subcommand(
                    Command::new("list")
                        .about("list files in a directory")
                        .arg(Arg::new("path").required(true)),
                ),
        )
        .subcommand(Command::new("scrap").about("list all known projects"))
        .subcommand(Command::new("skill").about("list all available skills"))
        .subcommand(
            Command::new("snapshot")
                .about("snapshot debugging utilities")
                .subcommand_required(true)
                .subcommand(Command::new("track").about("track current snapshot state"))
                .subcommand(
                    Command::new("patch")
                        .about("show patch for a snapshot hash")
                        .arg(Arg::new("hash").required(true)),
                )
                .subcommand(
                    Command::new("diff")
                        .about("show diff for a snapshot hash")
                        .arg(Arg::new("hash").required(true)),
                ),
        )
        .subcommand(
            Command::new("agent")
                .about("show agent configuration details")
                .arg(Arg::new("name").required(true)),
        )
        .subcommand(Command::new("startup").about("print startup timing"))
        .subcommand(Command::new("v2").about("debug v2 catalog and built-in plugins"))
        .subcommand(Command::new("info").about("show debug information"))
        .subcommand(Command::new("paths").about("show global paths (data, config, cache, state)"))
        .subcommand(Command::new("wait").about("wait indefinitely (for debugging)"))
}

fn console() -> Command {
    Command::new("console")
        .hide(true)
        .subcommand_required(true)
        .subcommand(Command::new("login").hide(true).arg(Arg::new("url")))
        .subcommand(Command::new("logout").hide(true).arg(Arg::new("email")))
        .subcommand(Command::new("switch").hide(true))
        .subcommand(Command::new("orgs").hide(true))
        .subcommand(Command::new("open").hide(true))
}

fn providers() -> Command {
    Command::new("providers")
        .about("manage AI providers and credentials")
        .alias("auth")
        .subcommand_required(true)
        .subcommand(
            Command::new("list")
                .alias("ls")
                .about("list providers and credentials"),
        )
        .subcommand(
            Command::new("login")
                .about("log in to a provider")
                .arg(Arg::new("url")),
        )
        .subcommand(
            Command::new("logout")
                .about("log out from a configured provider")
                .arg(Arg::new("provider").help("provider id or name to log out from")),
        )
}

fn agent() -> Command {
    Command::new("agent")
        .about("manage agents")
        .subcommand_required(true)
        .subcommand(Command::new("create").about("create a new agent"))
        .subcommand(Command::new("list").about("list all available agents"))
}

fn github() -> Command {
    Command::new("github")
        .about("manage GitHub agent")
        .subcommand_required(true)
        .subcommand(Command::new("install").about("install the GitHub agent"))
        .subcommand(Command::new("run").about("run the GitHub agent"))
}

fn session() -> Command {
    Command::new("session")
        .about("manage sessions")
        .subcommand_required(true)
        .subcommand(
            Command::new("delete").about("delete a session").arg(
                Arg::new("sessionID")
                    .required(true)
                    .help("session ID to delete"),
            ),
        )
        .subcommand(Command::new("list").about("list sessions"))
}

fn plug() -> Command {
    Command::new("plugin")
        .about("install plugin and update config")
        .alias("plug")
        .arg(Arg::new("module").required(true))
}

fn db() -> Command {
    Command::new("db")
        .about("database tools")
        .arg(Arg::new("query").help("open an interactive sqlite3 shell or run a query"))
        .subcommand(Command::new("path").about("print the database path"))
}

fn show(ui: &Ui, out: &str) {
    let text = out.trim_start();
    if !text.starts_with("opencode ") {
        ui.write_stderr(&format!("{}\n\n", ui.logo(None)));
        ui.write_stderr(text);
        ui.write_stderr("\n");
    } else {
        ui.write_stderr(out);
    }
}

/// Parse the command line. `args` excludes argv[0]. `Err(code)` means output
/// has already been rendered (help, version, or help-on-parse-error) and
/// `code` is the process exit code.
pub fn parse(mut cli: Command, args: &[OsString], ui: &mut Ui) -> Result<ArgMatches, i32> {
    let mut argv = Vec::with_capacity(args.len() + 1);
    argv.push(OsString::from("opencode"));
    argv.extend_from_slice(args);
    match cli.clone().try_get_matches_from(argv.iter().cloned()) {
        Ok(matches) => Ok(matches),
        Err(err) => match err.kind() {
            ErrorKind::DisplayHelp => {
                show(ui, &err.render().to_string());
                Err(0)
            }
            ErrorKind::DisplayVersion => {
                ui.write_stdout(&err.render().to_string());
                Err(0)
            }
            _ => {
                let help = cli.render_help().to_string();
                show(ui, &help);
                Err(1)
            }
        },
    }
}

/// index.ts:66-78 middleware: seed the process environment before dispatch.
pub fn apply_middleware(matches: &ArgMatches) {
    std::env::set_var("AGENT", "1");
    std::env::set_var("OPENCODE", "1");
    std::env::set_var("OPENCODE_PID", std::process::id().to_string());
    if matches.get_flag("print-logs") {
        std::env::set_var("OPENCODE_PRINT_LOGS", "1");
    }
    if let Some(level) = matches.get_one::<String>("log-level") {
        std::env::set_var("OPENCODE_LOG_LEVEL", level);
    }
    if matches.get_flag("pure") {
        std::env::set_var("OPENCODE_PURE", "1");
    }
}

fn stub(name: &str) -> TypedError {
    TypedError::Cli(CliError::new(format!("{name} is not implemented yet")))
}

/// Dispatch a parsed command line. The `$0` default command routes to the TUI.
pub fn route(matches: &ArgMatches) -> Result<(), TypedError> {
    let name = matches.subcommand_name().unwrap_or("tui");
    // TODO(C2): serve, web
    // TODO(C3/C4): run
    // TODO(C5): models, providers
    // TODO(C6): agent, session, db, debug
    // TODO(C7): mcp
    // TODO(C8): tui ($0), attach, acp, pr
    // TODO(C9): stats, export, import, generate
    Err(stub(name))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(parts: &[&str]) -> Vec<OsString> {
        parts.iter().map(OsString::from).collect()
    }

    #[test]
    fn parses_global_flags() {
        let matches = cli()
            .try_get_matches_from(["opencode", "--print-logs", "--log-level", "DEBUG", "--pure"])
            .unwrap();
        assert!(matches.get_flag("print-logs"));
        assert_eq!(
            matches.get_one::<String>("log-level").map(String::as_str),
            Some("DEBUG")
        );
        assert!(matches.get_flag("pure"));
    }

    #[test]
    fn global_flags_work_after_subcommand() {
        let matches = cli()
            .try_get_matches_from(["opencode", "serve", "--pure"])
            .unwrap();
        assert!(matches.get_flag("pure"));
    }

    #[test]
    fn invalid_log_level_is_rejected() {
        let (mut ui, _captured) = Ui::capture(false);
        let result = parse(cli(), &args(&["--log-level", "TRACE"]), &mut ui);
        assert!(matches!(result, Err(1)));
    }

    #[test]
    fn unknown_argument_shows_help_and_exits_one() {
        let (mut ui, captured) = Ui::capture(false);
        let result = parse(cli(), &args(&["--bogus"]), &mut ui);
        assert!(matches!(result, Err(1)));
        let stderr = captured.stderr();
        assert!(stderr.contains("Usage"), "{stderr}");
    }

    #[test]
    fn strict_fails_on_unknown_command() {
        let (mut ui, _captured) = Ui::capture(false);
        // Unknown subcommand-looking token falls to the `project` positional,
        // but a flag-looking one is rejected by strict parsing.
        let result = parse(cli(), &args(&["run", "--nope"]), &mut ui);
        assert!(matches!(result, Err(1)));
    }

    #[test]
    fn help_flag_prepends_logo_and_exits_zero() {
        let (mut ui, captured) = Ui::capture(false);
        let result = parse(cli(), &args(&["--help"]), &mut ui);
        assert!(matches!(result, Err(0)));
        let stderr = captured.stderr();
        assert!(stderr.contains("Usage"), "{stderr}");
        assert!(stderr.contains("\n\nUsage"), "logo not prepended: {stderr}");
        assert_eq!(captured.stdout(), "");
    }

    #[test]
    fn version_flag_writes_to_stdout_and_exits_zero() {
        let (mut ui, captured) = Ui::capture(false);
        let result = parse(cli(), &args(&["--version"]), &mut ui);
        assert!(matches!(result, Err(0)));
        let stdout = captured.stdout();
        assert!(stdout.contains(env!("CARGO_PKG_VERSION")), "{stdout}");
        assert_eq!(captured.stderr(), "");
    }

    #[test]
    fn short_version_flag_works() {
        let (mut ui, captured) = Ui::capture(false);
        let result = parse(cli(), &args(&["-v"]), &mut ui);
        assert!(matches!(result, Err(0)));
        assert!(captured.stdout().contains(env!("CARGO_PKG_VERSION")));
    }

    #[test]
    fn no_args_routes_to_default_tui() {
        let matches = cli().try_get_matches_from(["opencode"]).unwrap();
        let err = route(&matches).unwrap_err();
        assert!(format_error_needed(&err).contains("tui is not implemented yet"));
    }

    #[test]
    fn routes_registered_commands_to_stubs() {
        let cases: Vec<(&str, Vec<&str>)> = vec![
            ("acp", vec!["acp"]),
            ("mcp", vec!["mcp", "list"]),
            ("attach", vec!["attach", "http://localhost:4096"]),
            ("run", vec!["run", "hello"]),
            ("generate", vec!["generate"]),
            ("debug", vec!["debug", "info"]),
            ("console", vec!["console", "orgs"]),
            ("providers", vec!["providers", "list"]),
            ("agent", vec!["agent", "list"]),
            ("upgrade", vec!["upgrade"]),
            ("uninstall", vec!["uninstall"]),
            ("serve", vec!["serve"]),
            ("web", vec!["web"]),
            ("models", vec!["models"]),
            ("stats", vec!["stats"]),
            ("export", vec!["export"]),
            ("import", vec!["import", "file.json"]),
            ("github", vec!["github", "install"]),
            ("pr", vec!["pr", "42"]),
            ("session", vec!["session", "list"]),
            ("plugin", vec!["plugin", "module"]),
            ("plugin", vec!["plug", "module"]),
            ("db", vec!["db"]),
        ];
        for (name, argv) in cases {
            let invocation: Vec<&str> = std::iter::once("opencode")
                .chain(argv.iter().copied())
                .collect();
            let matches = cli()
                .try_get_matches_from(invocation)
                .unwrap_or_else(|err| panic!("parse failed for {name}: {err}"));
            let err = route(&matches).unwrap_err();
            assert!(
                format_error_needed(&err).contains(&format!("{name} is not implemented yet")),
                "{name}: {}",
                format_error_needed(&err)
            );
        }
    }

    #[test]
    fn aliases_route_to_registered_commands() {
        for argv in [
            vec!["opencode", "mcp", "ls"],
            vec!["opencode", "auth", "list"],
            vec!["opencode", "providers", "ls"],
        ] {
            assert!(
                cli().try_get_matches_from(argv.clone()).is_ok(),
                "parse failed for {argv:?}"
            );
        }
    }

    #[test]
    fn demand_command_groups_fail_without_subcommand() {
        for name in [
            "mcp",
            "debug",
            "providers",
            "agent",
            "github",
            "session",
            "console",
        ] {
            let (mut ui, _captured) = Ui::capture(false);
            let result = parse(cli(), &args(&[name]), &mut ui);
            assert!(matches!(result, Err(1)), "{name}");
        }
    }

    #[test]
    fn middleware_sets_env_vars() {
        std::env::remove_var("OPENCODE_PRINT_LOGS");
        std::env::remove_var("OPENCODE_LOG_LEVEL");
        std::env::remove_var("OPENCODE_PURE");
        let matches = cli().try_get_matches_from(["opencode"]).unwrap();
        apply_middleware(&matches);
        assert_eq!(
            std::env::var("OPENCODE_PRINT_LOGS").unwrap_err(),
            std::env::VarError::NotPresent
        );
        assert_eq!(
            std::env::var("OPENCODE_LOG_LEVEL").unwrap_err(),
            std::env::VarError::NotPresent
        );
        assert_eq!(
            std::env::var("OPENCODE_PURE").unwrap_err(),
            std::env::VarError::NotPresent
        );
        assert_eq!(std::env::var("AGENT").unwrap(), "1");
        assert_eq!(std::env::var("OPENCODE").unwrap(), "1");
        assert_eq!(
            std::env::var("OPENCODE_PID").unwrap(),
            std::process::id().to_string()
        );

        let matches = cli()
            .try_get_matches_from(["opencode", "--print-logs", "--log-level", "WARN", "--pure"])
            .unwrap();
        apply_middleware(&matches);
        assert_eq!(std::env::var("OPENCODE_PRINT_LOGS").unwrap(), "1");
        assert_eq!(std::env::var("OPENCODE_LOG_LEVEL").unwrap(), "WARN");
        assert_eq!(std::env::var("OPENCODE_PURE").unwrap(), "1");
    }

    fn format_error_needed(err: &TypedError) -> String {
        crate::error::format_error(err).unwrap_or_default()
    }
}
