use std::ffi::OsString;

use clap::builder::PossibleValuesParser;
use clap::error::ErrorKind;
use clap::{Arg, ArgAction, ArgMatches, Command};

use crate::error::{CliError, TypedError};
use crate::network;
use crate::ui::Ui;

pub mod acp;
pub mod agent;
pub mod attach;
pub mod db;
pub mod debug;
pub mod export;
pub mod generate;
pub mod import;
pub mod libertai;
pub mod mcp;
pub mod models;
pub mod pr;
pub mod providers;
pub mod run;
pub mod run_events;
pub mod run_files;
pub mod run_output;
pub mod serve;
pub mod session;
pub mod stats;
pub mod tui;
pub mod web;

pub fn cli() -> Command {
    network::with_network_options(
        Command::new("alforria")
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
            .arg(Arg::new("project").help("path to start alforria in"))
            // `$0` default-command flags (tui.ts:75-128).
            .arg(
                Arg::new("model")
                    .long("model")
                    .short('m')
                    .help("model to use in the format of provider/model"),
            )
            .arg(
                Arg::new("continue")
                    .long("continue")
                    .short('c')
                    .action(ArgAction::SetTrue)
                    .help("continue the last session"),
            )
            .arg(
                Arg::new("session")
                    .long("session")
                    .short('s')
                    .help("session id to continue"),
            )
            .arg(
                Arg::new("fork")
                    .long("fork")
                    .action(ArgAction::SetTrue)
                    .help("fork the session when continuing (use with --continue or --session)"),
            )
            .arg(Arg::new("prompt").long("prompt").help("prompt to use"))
            .arg(Arg::new("agent").long("agent").help("agent to use"))
            .arg(
                Arg::new("auto")
                    .long("auto")
                    .action(ArgAction::SetTrue)
                    .help("auto-approve permissions that are not explicitly denied (dangerous!)"),
            )
            .arg(
                Arg::new("yolo")
                    .long("yolo")
                    .action(ArgAction::SetTrue)
                    .hide(true),
            )
            .arg(
                Arg::new("dangerously-skip-permissions")
                    .long("dangerously-skip-permissions")
                    .action(ArgAction::SetTrue)
                    .hide(true),
            )
            .arg(
                Arg::new("mini")
                    .long("mini")
                    .action(ArgAction::SetTrue)
                    .help("start the minimal interactive interface"),
            )
            .arg(
                Arg::new("replay")
                    .long("replay")
                    .action(ArgAction::SetTrue)
                    .hide(true),
            )
            .arg(
                Arg::new("no-replay")
                    .long("no-replay")
                    .action(ArgAction::SetTrue)
                    .help("disable mini session history replay on resume and after resize"),
            )
            .arg(
                Arg::new("replay-limit")
                    .long("replay-limit")
                    .value_parser(clap::value_parser!(f64))
                    .help("cap visible mini replay to the newest N messages"),
            )
            .arg(
                Arg::new("demo")
                    .long("demo")
                    .action(ArgAction::SetTrue)
                    .hide(true),
            )
            .subcommand(acp())
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
                Command::new("upgrade")
                    .about("upgrade alforria to the latest or a specific version"),
            )
            .subcommand(
                Command::new("uninstall").about("uninstall alforria and remove all related files"),
            )
            .subcommand(network::with_network_options(
                Command::new("serve").about("starts a headless alforria server"),
            ))
            .subcommand(network::with_network_options(
                Command::new("web").about("start alforria server and open web interface"),
            ))
            .subcommand(
                Command::new("models")
                    .about("list all available models")
                    .arg(Arg::new("provider").help("list models for a specific provider"))
                    .arg(
                        Arg::new("verbose")
                            .long("verbose")
                            .help("use more verbose model output (includes metadata like costs)")
                            .action(ArgAction::SetTrue),
                    )
                    .arg(
                        Arg::new("refresh")
                            .long("refresh")
                            .help("refresh the models cache from models.dev")
                            .action(ArgAction::SetTrue),
                    ),
            )
            .subcommand(
                Command::new("stats")
                    .about("show token usage and cost statistics")
                    .arg(
                        Arg::new("days")
                            .long("days")
                            .help("show stats for the last N days (default: all time)")
                            .value_parser(clap::value_parser!(f64)),
                    )
                    .arg(
                        Arg::new("tools")
                            .long("tools")
                            .help("number of tools to show (default: all)")
                            .value_parser(clap::value_parser!(f64)),
                    )
                    .arg(
                        Arg::new("models")
                            .long("models")
                            .help("show model statistics (default: hidden). Pass a number to show top N, otherwise shows all")
                            .num_args(0..=1)
                            .default_missing_value("true"),
                    )
                    .arg(
                        Arg::new("project")
                            .long("project")
                            .help("filter by project (default: all projects, empty string: current project)"),
                    ),
            )
            .subcommand(
                Command::new("libertai")
                    .about("LibertAI account commands")
                    .subcommand(
                        Command::new("usage")
                            .about("show plan and usage (allowance windows, prepaid credits)")
                            .arg(
                                Arg::new("json")
                                    .long("json")
                                    .help("output raw JSON")
                                    .action(ArgAction::SetTrue),
                            ),
                    ),
            )
            .subcommand(
                Command::new("export")
                    .about("export session data as JSON")
                    .arg(Arg::new("sessionID"))
                    .arg(
                        Arg::new("sanitize")
                            .long("sanitize")
                            .help("redact sensitive transcript and file data")
                            .action(ArgAction::SetTrue),
                    ),
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
                    .about("fetch and checkout a GitHub PR branch, then run alforria")
                    .arg(
                        Arg::new("number")
                            .required(true)
                            .value_parser(clap::value_parser!(i64)),
                    ),
            )
            .subcommand(session())
            .subcommand(plug())
            .subcommand(db()),
    )
}

fn attach() -> Command {
    Command::new("attach")
        .about("attach to a running alforria server")
        .arg(Arg::new("url").required(true).help("http://localhost:4096"))
        .arg(Arg::new("dir").long("dir").help("directory to run in"))
        .arg(
            Arg::new("continue")
                .long("continue")
                .short('c')
                .action(ArgAction::SetTrue)
                .help("continue the last session"),
        )
        .arg(
            Arg::new("session")
                .long("session")
                .short('s')
                .help("session id to continue"),
        )
        .arg(
            Arg::new("fork")
                .long("fork")
                .action(ArgAction::SetTrue)
                .help("fork the session when continuing (use with --continue or --session)"),
        )
        .arg(
            Arg::new("password")
                .long("password")
                .short('p')
                .help("basic auth password (defaults to OPENCODE_SERVER_PASSWORD)"),
        )
        .arg(
            Arg::new("username")
                .long("username")
                .short('u')
                .help("basic auth username (defaults to OPENCODE_SERVER_USERNAME or 'alforria')"),
        )
        .arg(
            Arg::new("mini")
                .long("mini")
                .action(ArgAction::SetTrue)
                .help("start the minimal interactive interface"),
        )
        .arg(
            Arg::new("replay")
                .long("replay")
                .action(ArgAction::SetTrue)
                .hide(true),
        )
        .arg(
            Arg::new("no-replay")
                .long("no-replay")
                .action(ArgAction::SetTrue)
                .help("disable mini session history replay on resume and after resize"),
        )
        .arg(
            Arg::new("replay-limit")
                .long("replay-limit")
                .value_parser(clap::value_parser!(f64))
                .help("cap visible mini replay to the newest N messages"),
        )
}

fn acp() -> Command {
    // The `process.cwd()` default is applied in the handler (clap
    // defaults are 'static).
    network::with_network_options(
        Command::new("acp").about("start ACP (Agent Client Protocol) server"),
    )
    .arg(Arg::new("cwd").long("cwd").help("working directory"))
}

fn run() -> Command {
    Command::new("run")
        .about("run alforria with a message")
        .arg(Arg::new("message").num_args(0..).help("message to send"))
        .arg(
            Arg::new("command")
                .long("command")
                .help("the command to run, use message for args"),
        )
        .arg(
            Arg::new("continue")
                .long("continue")
                .short('c')
                .help("continue the last session")
                .action(ArgAction::SetTrue),
        )
        .arg(
            Arg::new("session")
                .long("session")
                .short('s')
                .help("session id to continue"),
        )
        .arg(
            Arg::new("fork")
                .long("fork")
                .help("fork the session before continuing (requires --continue or --session)")
                .action(ArgAction::SetTrue),
        )
        .arg(
            Arg::new("share")
                .long("share")
                .help("share the session")
                .action(ArgAction::SetTrue),
        )
        .arg(
            Arg::new("model")
                .long("model")
                .short('m')
                .help("model to use in the format of provider/model"),
        )
        .arg(Arg::new("agent").long("agent").help("agent to use"))
        .arg(
            Arg::new("format")
                .long("format")
                .help("format: default (formatted) or json (raw JSON events)")
                .value_parser(["default", "json"])
                .default_value("default"),
        )
        .arg(
            Arg::new("file")
                .long("file")
                .short('f')
                .help("file(s) to attach to message")
                .action(ArgAction::Append),
        )
        .arg(
            Arg::new("title")
                .long("title")
                .help("title for the session (uses truncated prompt if no value provided)"),
        )
        .arg(
            Arg::new("attach")
                .long("attach")
                .help("attach to a running alforria server (e.g., http://localhost:4096)"),
        )
        .arg(
            Arg::new("password")
                .long("password")
                .short('p')
                .help("basic auth password (defaults to OPENCODE_SERVER_PASSWORD)"),
        )
        .arg(
            Arg::new("username")
                .long("username")
                .short('u')
                .help("basic auth username (defaults to OPENCODE_SERVER_USERNAME or 'alforria')"),
        )
        .arg(
            Arg::new("dir")
                .long("dir")
                .help("directory to run in, path on remote server if attaching"),
        )
        .arg(
            Arg::new("port")
                .long("port")
                .help("port for the local server (defaults to random port if no value provided)")
                .value_parser(clap::value_parser!(u16))
                .default_value("0"),
        )
        .arg(
            Arg::new("variant").long("variant").help(
                "model variant (provider-specific reasoning effort, e.g., high, max, minimal)",
            ),
        )
        .arg(
            Arg::new("thinking")
                .long("thinking")
                .help("show thinking blocks")
                .action(ArgAction::SetTrue),
        )
        .arg(
            Arg::new("mini")
                .long("mini")
                .help("run in direct interactive split-footer mode")
                .action(ArgAction::SetTrue)
                .hide(true),
        )
        .arg(
            Arg::new("replay")
                .long("replay")
                .help("replay interactive session history on resume and after resize")
                .action(ArgAction::SetTrue)
                .hide(true),
        )
        .arg(
            Arg::new("no-replay")
                .long("no-replay")
                .help("disable replay of interactive session history")
                .action(ArgAction::SetTrue)
                .hide(true),
        )
        .arg(
            Arg::new("replay-limit")
                .long("replay-limit")
                .help("cap visible interactive replay to the newest N messages")
                .value_parser(clap::value_parser!(f64))
                .hide(true),
        )
        .arg(
            Arg::new("interactive")
                .long("interactive")
                .short('i')
                .help("run in direct interactive split-footer mode")
                .action(ArgAction::SetTrue)
                .hide(true),
        )
        .arg(
            Arg::new("auto")
                .long("auto")
                .help("auto-approve permissions that are not explicitly denied (dangerous!)")
                .action(ArgAction::SetTrue),
        )
        .arg(
            Arg::new("yolo")
                .long("yolo")
                .action(ArgAction::SetTrue)
                .hide(true),
        )
        .arg(
            Arg::new("dangerously-skip-permissions")
                .long("dangerously-skip-permissions")
                .action(ArgAction::SetTrue)
                .hide(true),
        )
        .arg(
            Arg::new("demo")
                .long("demo")
                .help("enable direct interactive demo slash commands")
                .action(ArgAction::SetTrue)
                .hide(true),
        )
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
                .arg(Arg::new("name").help("name of the MCP server"))
                .arg(
                    Arg::new("url")
                        .long("url")
                        .help("URL for a remote MCP server"),
                )
                .arg(
                    Arg::new("env")
                        .long("env")
                        .help("environment variable for a local MCP server (KEY=VALUE)")
                        .action(ArgAction::Append),
                )
                .arg(
                    Arg::new("header")
                        .long("header")
                        .help("HTTP header for a remote MCP server (KEY=VALUE)")
                        .action(ArgAction::Append),
                )
                .arg(
                    Arg::new("command")
                        .num_args(0..)
                        .last(true)
                        .help("command to run for a local MCP server (after --)"),
                ),
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
                .subcommand(
                    Command::new("files")
                        .about("list files using ripgrep")
                        .arg(
                            Arg::new("query")
                                .long("query")
                                .help("Filter files by query"),
                        )
                        .arg(
                            Arg::new("glob")
                                .long("glob")
                                .help("Glob pattern to match files"),
                        )
                        .arg(
                            Arg::new("limit")
                                .long("limit")
                                .help("Limit number of results")
                                .value_parser(clap::value_parser!(usize)),
                        ),
                )
                .subcommand(
                    Command::new("search")
                        .about("search file contents using ripgrep")
                        .arg(Arg::new("pattern").required(true))
                        .arg(
                            Arg::new("glob")
                                .long("glob")
                                .help("File glob patterns")
                                .action(ArgAction::Append),
                        )
                        .arg(
                            Arg::new("limit")
                                .long("limit")
                                .help("Limit number of results")
                                .value_parser(clap::value_parser!(usize)),
                        ),
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
                .arg(Arg::new("name").required(true).help("Agent name"))
                .arg(Arg::new("tool").long("tool").help("Tool id to execute"))
                .arg(
                    Arg::new("params")
                        .long("params")
                        .help("Tool params as JSON or a JS object literal"),
                ),
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
                .arg(Arg::new("url").help("alforria auth provider"))
                .arg(
                    Arg::new("provider")
                        .long("provider")
                        .short('p')
                        .help("provider id or name to log in to (skips provider selection)"),
                )
                .arg(
                    Arg::new("method")
                        .long("method")
                        .short('m')
                        .help("login method label (skips method selection)"),
                ),
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
        .subcommand(
            Command::new("create")
                .about("create a new agent")
                .arg(
                    Arg::new("path")
                        .long("path")
                        .help("directory path to generate the agent file"),
                )
                .arg(
                    Arg::new("description")
                        .long("description")
                        .help("what the agent should do"),
                )
                .arg(
                    Arg::new("mode")
                        .long("mode")
                        .help("agent mode")
                        .value_parser(["all", "primary", "subagent"]),
                )
                .arg(
                    Arg::new("permissions")
                        .long("permissions")
                        .alias("tools")
                        .help("comma-separated list of permissions to allow (default: all)"),
                )
                .arg(
                    Arg::new("model")
                        .long("model")
                        .short('m')
                        .help("model to use in the format of provider/model"),
                ),
        )
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
        .subcommand(
            Command::new("list")
                .about("list sessions")
                .arg(
                    Arg::new("max-count")
                        .long("max-count")
                        .short('n')
                        .help("limit to N most recent sessions")
                        .value_parser(clap::value_parser!(i64)),
                )
                .arg(
                    Arg::new("format")
                        .long("format")
                        .help("output format")
                        .value_parser(["table", "json"])
                        .default_value("table"),
                ),
        )
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
        .arg(
            Arg::new("format")
                .long("format")
                .help("Output format")
                .value_parser(["json", "tsv"])
                .default_value("tsv"),
        )
        .subcommand(Command::new("path").about("print the database path"))
}

fn show(ui: &Ui, out: &str) {
    let text = out.trim_start();
    if !text.starts_with("alforria ") {
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
    argv.push(OsString::from("alforria"));
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

/// The tokio runtime the embedded server listener runs on.
pub(crate) fn runtime() -> Result<tokio::runtime::Runtime, TypedError> {
    tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .map_err(|err| TypedError::Unknown {
            raw: err.to_string(),
        })
}

/// Dispatch a parsed command line. The `$0` default command routes to the TUI.
pub fn route(matches: &ArgMatches, ui: &mut Ui, raw: &[OsString]) -> Result<(), TypedError> {
    let name = matches.subcommand_name().unwrap_or("tui");
    match name {
        "serve" => serve::run(matches.subcommand_matches("serve").expect("serve"), ui, raw),
        "web" => web::run(matches.subcommand_matches("web").expect("web"), ui, raw),
        "run" => run::run(matches.subcommand_matches("run").expect("run"), ui),
        "providers" => providers::run(
            matches.subcommand_matches("providers").expect("providers"),
            ui,
        ),
        "libertai" => {
            let matches = matches.subcommand_matches("libertai").expect("libertai");
            match matches.subcommand_name() {
                Some("usage") => {
                    libertai::run(matches.subcommand_matches("usage").expect("usage"), ui)
                }
                _ => unreachable!("libertai requires a subcommand"),
            }
        }
        "models" => models::run(matches.subcommand_matches("models").expect("models"), ui),
        "agent" => agent::run(matches.subcommand_matches("agent").expect("agent"), ui),
        "session" => session::run(matches.subcommand_matches("session").expect("session"), ui),
        "db" => db::run(matches.subcommand_matches("db").expect("db"), ui),
        "debug" => debug::run(matches.subcommand_matches("debug").expect("debug"), ui),
        "mcp" => mcp::run(matches.subcommand_matches("mcp").expect("mcp"), ui),
        "tui" => tui::run(matches, ui, raw),
        "attach" => attach::run(
            matches.subcommand_matches("attach").expect("attach"),
            ui,
            raw,
        ),
        "acp" => acp::run(matches.subcommand_matches("acp").expect("acp"), ui, raw),
        "pr" => pr::cli_run(matches.subcommand_matches("pr").expect("pr"), ui, raw),
        "stats" => stats::run(matches.subcommand_matches("stats").expect("stats"), ui),
        "export" => export::run(matches.subcommand_matches("export").expect("export"), ui),
        "import" => import::run(matches.subcommand_matches("import").expect("import"), ui),
        "generate" => generate::run(
            matches.subcommand_matches("generate").expect("generate"),
            ui,
        ),
        _ => Err(stub(name)),
    }
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
            .try_get_matches_from(["alforria", "--print-logs", "--log-level", "DEBUG", "--pure"])
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
            .try_get_matches_from(["alforria", "serve", "--pure"])
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
        let matches = cli().try_get_matches_from(["alforria"]).unwrap();
        let (mut ui, _captured) = Ui::capture(false);
        tui::with_tui_runner(Box::new(|_, _, _, _| Err(stub("tui"))), || {
            let err = route(&matches, &mut ui, &[]).unwrap_err();
            assert!(format_error_needed(&err).contains("tui is not implemented yet"));
        });
    }

    #[test]
    fn routes_registered_commands_to_stubs() {
        let cases: Vec<(&str, Vec<&str>)> = vec![
            ("console", vec!["console", "orgs"]),
            ("upgrade", vec!["upgrade"]),
            ("uninstall", vec!["uninstall"]),
            ("github", vec!["github", "install"]),
            ("plugin", vec!["plugin", "module"]),
            ("plugin", vec!["plug", "module"]),
        ];
        for (name, argv) in cases {
            let invocation: Vec<&str> = std::iter::once("alforria")
                .chain(argv.iter().copied())
                .collect();
            let matches = cli()
                .try_get_matches_from(invocation)
                .unwrap_or_else(|err| panic!("parse failed for {name}: {err}"));
            let (mut ui, _captured) = Ui::capture(false);
            let raw: Vec<OsString> = argv.iter().map(OsString::from).collect();
            let err = route(&matches, &mut ui, &raw).unwrap_err();
            assert!(
                format_error_needed(&err).contains(&format!("{name} is not implemented yet")),
                "{name}: {}",
                format_error_needed(&err)
            );
        }
    }

    #[test]
    fn login_and_models_parse_their_flags() {
        let matches = cli()
            .try_get_matches_from([
                "alforria",
                "auth",
                "login",
                "-p",
                "anthropic",
                "--method",
                "oauth",
            ])
            .unwrap();
        let login = matches.subcommand_matches("providers").unwrap();
        let login = login.subcommand_matches("login").unwrap();
        assert_eq!(
            login.get_one::<String>("provider").map(String::as_str),
            Some("anthropic")
        );
        assert_eq!(
            login.get_one::<String>("method").map(String::as_str),
            Some("oauth")
        );

        let matches = cli()
            .try_get_matches_from(["alforria", "models", "openai", "--verbose", "--refresh"])
            .unwrap();
        let models = matches.subcommand_matches("models").unwrap();
        assert_eq!(
            models.get_one::<String>("provider").map(String::as_str),
            Some("openai")
        );
        assert!(models.get_flag("verbose"));
        assert!(models.get_flag("refresh"));
    }

    #[test]
    fn serve_parses_network_options() {
        let matches = cli()
            .try_get_matches_from([
                "alforria",
                "serve",
                "--port",
                "9000",
                "--hostname",
                "0.0.0.0",
                "--mdns",
                "--mdns-domain",
                "dev.local",
                "--cors",
                "https://a",
                "--cors",
                "https://b",
            ])
            .unwrap();
        let opts =
            network::NetworkOptions::from_matches(matches.subcommand_matches("serve").unwrap());
        assert_eq!(opts.port, 9000);
        assert_eq!(opts.hostname, "0.0.0.0");
        assert!(opts.mdns);
        assert_eq!(opts.mdns_domain, "dev.local");
        assert_eq!(opts.cors, vec!["https://a", "https://b"]);
    }

    #[test]
    fn web_parses_network_option_defaults() {
        let matches = cli().try_get_matches_from(["alforria", "web"]).unwrap();
        let opts =
            network::NetworkOptions::from_matches(matches.subcommand_matches("web").unwrap());
        assert_eq!(opts.port, 0);
        assert_eq!(opts.hostname, "127.0.0.1");
        assert!(!opts.mdns);
        assert_eq!(opts.mdns_domain, "opencode.local");
        assert!(opts.cors.is_empty());
    }

    #[test]
    fn c6_flags_parse_their_surface() {
        let matches = cli()
            .try_get_matches_from([
                "alforria",
                "agent",
                "create",
                "--path",
                "/tmp",
                "--description",
                "reviewer",
                "--mode",
                "subagent",
                "--tools",
                "read,edit",
                "-m",
                "anthropic/claude-4",
            ])
            .unwrap();
        let create = matches
            .subcommand_matches("agent")
            .unwrap()
            .subcommand_matches("create")
            .unwrap();
        assert_eq!(
            create.get_one::<String>("permissions").map(String::as_str),
            Some("read,edit")
        );
        assert_eq!(
            create.get_one::<String>("model").map(String::as_str),
            Some("anthropic/claude-4")
        );

        let matches = cli()
            .try_get_matches_from(["alforria", "session", "list", "-n", "5", "--format", "json"])
            .unwrap();
        let list = matches
            .subcommand_matches("session")
            .unwrap()
            .subcommand_matches("list")
            .unwrap();
        assert_eq!(list.get_one::<i64>("max-count"), Some(&5));
        assert_eq!(
            list.get_one::<String>("format").map(String::as_str),
            Some("json")
        );

        let matches = cli()
            .try_get_matches_from(["alforria", "db", "--format", "json", "SELECT 1"])
            .unwrap();
        let db = matches.subcommand_matches("db").unwrap();
        assert_eq!(
            db.get_one::<String>("query").map(String::as_str),
            Some("SELECT 1")
        );
        assert_eq!(
            db.get_one::<String>("format").map(String::as_str),
            Some("json")
        );

        let matches = cli()
            .try_get_matches_from(["alforria", "db", "path"])
            .unwrap();
        assert!(matches
            .subcommand_matches("db")
            .unwrap()
            .subcommand_matches("path")
            .is_some());

        let matches = cli()
            .try_get_matches_from([
                "alforria", "debug", "rg", "search", "pattern", "--glob", "*.rs", "--limit", "10",
            ])
            .unwrap();
        let search = matches
            .subcommand_matches("debug")
            .unwrap()
            .subcommand_matches("rg")
            .unwrap()
            .subcommand_matches("search")
            .unwrap();
        assert_eq!(
            search.get_one::<String>("pattern").map(String::as_str),
            Some("pattern")
        );
        assert_eq!(search.get_one::<usize>("limit"), Some(&10));

        let matches = cli()
            .try_get_matches_from(["alforria", "debug", "agent", "build", "--tool", "read"])
            .unwrap();
        let agent = matches
            .subcommand_matches("debug")
            .unwrap()
            .subcommand_matches("agent")
            .unwrap();
        assert_eq!(
            agent.get_one::<String>("name").map(String::as_str),
            Some("build")
        );
        assert_eq!(
            agent.get_one::<String>("tool").map(String::as_str),
            Some("read")
        );
    }

    #[test]
    fn agent_create_mode_is_validated() {
        let (mut ui, _captured) = Ui::capture(false);
        assert!(parse(
            cli(),
            &args(&[
                "agent",
                "create",
                "--mode",
                "bogus",
                "--path",
                "x",
                "--description",
                "d",
                "--permissions",
                "read"
            ]),
            &mut ui
        )
        .is_err());
    }

    #[test]
    fn mdns_flag_accepts_explicit_false() {
        let matches = cli()
            .try_get_matches_from(["alforria", "serve", "--mdns=false"])
            .unwrap();
        let opts =
            network::NetworkOptions::from_matches(matches.subcommand_matches("serve").unwrap());
        assert!(!opts.mdns);
    }

    #[test]
    fn c9_flags_parse_their_surface() {
        let matches = cli()
            .try_get_matches_from([
                "alforria",
                "stats",
                "--days",
                "7",
                "--tools",
                "3",
                "--models",
                "5",
                "--project",
                "prj_1",
            ])
            .unwrap();
        let stats = matches.subcommand_matches("stats").unwrap();
        assert_eq!(stats.get_one::<f64>("days"), Some(&7.0));
        assert_eq!(stats.get_one::<f64>("tools"), Some(&3.0));
        assert_eq!(
            stats.get_one::<String>("models").map(String::as_str),
            Some("5")
        );
        assert_eq!(
            stats.get_one::<String>("project").map(String::as_str),
            Some("prj_1")
        );

        let matches = cli()
            .try_get_matches_from(["alforria", "stats", "--models"])
            .unwrap();
        let stats = matches.subcommand_matches("stats").unwrap();
        assert_eq!(
            stats.get_one::<String>("models").map(String::as_str),
            Some("true")
        );

        let matches = cli()
            .try_get_matches_from(["alforria", "export", "ses_1", "--sanitize"])
            .unwrap();
        let export = matches.subcommand_matches("export").unwrap();
        assert!(export.get_flag("sanitize"));
        assert_eq!(
            export.get_one::<String>("sessionID").map(String::as_str),
            Some("ses_1")
        );
    }

    #[test]
    fn aliases_route_to_registered_commands() {
        for argv in [
            vec!["alforria", "mcp", "ls"],
            vec!["alforria", "auth", "list"],
            vec!["alforria", "providers", "ls"],
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
        let matches = cli().try_get_matches_from(["alforria"]).unwrap();
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
            .try_get_matches_from(["alforria", "--print-logs", "--log-level", "WARN", "--pure"])
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
