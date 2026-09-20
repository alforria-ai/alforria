//! `pr <number>` — fetch and checkout a GitHub PR branch, then run
//! opencode (`cli/cmd/pr.ts:8-115`).

use std::ffi::OsString;
use std::process::Command as StdCommand;

use clap::ArgMatches;

use crate::error::{CliError, TypedError};
use crate::ui::Ui;

fn cli_error(message: impl Into<String>) -> TypedError {
    TypedError::Cli(CliError {
        message: message.into(),
        exit_code: 1,
    })
}

fn cli_die(message: impl Into<String>) -> TypedError {
    cli_error(message)
}

/// The `Process`/`Git` spawn seam (tests record invocations).
pub trait CommandRunner {
    fn run(&self, program: &str, args: &[&str]) -> std::io::Result<(i32, String)>;
}

#[derive(Default)]
pub struct SystemRunner;

impl CommandRunner for SystemRunner {
    fn run(&self, program: &str, args: &[&str]) -> std::io::Result<(i32, String)> {
        let output = StdCommand::new(program).args(args).output()?;
        let code = output.status.code().unwrap_or(-1);
        let text = String::from_utf8_lossy(&output.stdout).into_owned();
        Ok((code, text))
    }
}

/// The `Spawn` seam for the final `opencode -s {sessionID}` handoff
/// (stdio inherited).
pub trait OpencodeSpawn {
    fn spawn(&self, args: &[String]) -> std::io::Result<i32>;
}

pub struct SystemSpawn;

impl OpencodeSpawn for SystemSpawn {
    fn spawn(&self, args: &[String]) -> std::io::Result<i32> {
        let status = StdCommand::new("opencode").args(args).status()?;
        Ok(status.code().unwrap_or(-1))
    }
}

/// The instance `ctx.project.vcs === "git"` check — walk up from cwd
/// looking for a `.git` entry.
fn in_git_repository() -> bool {
    let mut dir = std::env::current_dir().unwrap_or_default();
    loop {
        if dir.join(".git").exists() {
            return true;
        }
        if !dir.pop() {
            return false;
        }
    }
}

/// pr.ts:52-58 — the `https://opncd.ai/s/{id}` session URL embedded in
/// the PR body.
pub fn session_url_from_body(body: &str) -> Option<String> {
    // /https:\/\/opncd\.ai\/s\/([a-zA-Z0-9_-]+)/ (pr.ts:78)
    const PREFIX: &str = "https://opncd.ai/s/";
    let start = body.find(PREFIX)?;
    let rest = &body[start + PREFIX.len()..];
    let end = rest
        .char_indices()
        .find(|(_, c)| !c.is_ascii_alphanumeric() && *c != '_' && *c != '-')
        .map(|(index, _)| index)
        .unwrap_or(rest.len());
    Some(format!("{}{}", PREFIX, &rest[..end]))
}

/// pr.ts:86-89 — the `Imported session: {id}` marker printed by
/// `opencode import`.
pub fn imported_session(text: &str) -> Option<String> {
    let marker = "Imported session: ";
    let start = text.find(marker)? + marker.len();
    let rest = &text[start..];
    let end = rest
        .char_indices()
        .find(|(_, c)| !c.is_ascii_alphanumeric() && *c != '_' && *c != '-')
        .map(|(index, _)| index)
        .unwrap_or(rest.len());
    Some(rest[..end].to_string())
}

pub fn run<R, S>(
    matches: &ArgMatches,
    _ui: &mut Ui,
    runner: &R,
    spawn: &S,
) -> Result<(), TypedError>
where
    R: CommandRunner,
    S: OpencodeSpawn,
{
    let pr_number = *matches.get_one::<i64>("number").expect("number");
    if !in_git_repository() {
        return Err(cli_die(
            "Could not find git repository. Please run this command from a git repository.",
        ));
    }

    let local_branch = format!("pr/{pr_number}");
    _ui.println(&format!("Fetching and checking out PR #{pr_number}..."));

    let (code, _) = runner
        .run(
            "gh",
            &[
                "pr",
                "checkout",
                &pr_number.to_string(),
                "--branch",
                &local_branch,
                "--force",
            ],
        )
        .unwrap_or((-1, String::new()));
    if code != 0 {
        return Err(cli_die(format!(
            "Failed to checkout PR #{pr_number}. Make sure you have gh CLI installed and authenticated."
        )));
    }

    let (info_code, info_text) = runner
        .run(
            "gh",
            &[
                "pr",
                "view",
                &pr_number.to_string(),
                "--json",
                "headRepository,headRepositoryOwner,isCrossRepository,headRefName,body",
            ],
        )
        .unwrap_or((-1, String::new()));

    let mut session_id: Option<String> = None;
    if info_code == 0 && !info_text.trim().is_empty() {
        let pr_info = serde_json::from_str::<serde_json::Value>(&info_text);
        if let Ok(pr_info) = pr_info {
            if pr_info["isCrossRepository"].as_bool() == Some(true) {
                let fork_owner = pr_info["headRepositoryOwner"]["login"].as_str();
                let fork_name = pr_info["headRepository"]["name"].as_str();
                if let (Some(fork_owner), Some(fork_name)) = (fork_owner, fork_name) {
                    let (remotes_code, remotes) =
                        runner.run("git", &["remote"]).unwrap_or((0, String::new()));
                    if remotes_code == 0
                        && !remotes.lines().any(|remote| remote.trim() == fork_owner)
                    {
                        runner
                            .run(
                                "git",
                                &[
                                    "remote",
                                    "add",
                                    fork_owner,
                                    &format!("https://github.com/{fork_owner}/{fork_name}.git"),
                                ],
                            )
                            .ok();
                        _ui.println(&format!("Added fork remote: {fork_owner}"));
                    }
                    if let Some(head_ref) = pr_info["headRefName"].as_str() {
                        runner
                            .run(
                                "git",
                                &[
                                    "branch",
                                    &format!("--set-upstream-to={fork_owner}/{head_ref}"),
                                    &local_branch,
                                ],
                            )
                            .ok();
                    }
                }
            }

            if let Some(body) = pr_info["body"].as_str() {
                if let Some(session_url) = session_url_from_body(body) {
                    _ui.println(&format!("Found opencode session: {session_url}"));
                    _ui.println("Importing session...");
                    let (import_code, import_text) = runner
                        .run("opencode", &["import", &session_url])
                        .unwrap_or((-1, String::new()));
                    if import_code == 0 {
                        if let Some(imported) = imported_session(&import_text) {
                            session_id = Some(imported);
                            _ui.println(&format!(
                                "Session imported: {}",
                                session_id.clone().unwrap_or_default()
                            ));
                        }
                    }
                }
            }
        }
    }

    _ui.println(&format!(
        "Successfully checked out PR #{pr_number} as branch '{local_branch}'"
    ));
    _ui.println("");
    _ui.println("Starting opencode...");
    _ui.println("");

    let mut opencode_args: Vec<String> = Vec::new();
    if let Some(session_id) = &session_id {
        opencode_args.push("-s".to_string());
        opencode_args.push(session_id.clone());
    }
    let code = spawn.spawn(&opencode_args).unwrap_or(-1);
    if code != 0 {
        // Match legacy throw semantics — a defect through the top-level
        // catch (exit 1, "Unexpected error" banner).
        return Err(TypedError::Unknown {
            raw: format!("opencode exited with code {code}"),
        });
    }
    Ok(())
}

pub fn cli_run(matches: &ArgMatches, ui: &mut Ui, _raw: &[OsString]) -> Result<(), TypedError> {
    run(matches, ui, &SystemRunner, &SystemSpawn)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extracts_session_url_from_body() {
        assert_eq!(
            session_url_from_body("see https://opncd.ai/s/abc123-_ for context"),
            Some("https://opncd.ai/s/abc123-_".to_string())
        );
        assert_eq!(session_url_from_body("no url"), None);
    }

    #[test]
    fn extracts_imported_session() {
        assert_eq!(
            imported_session("done\nImported session: ses_123-abc"),
            Some("ses_123-abc".to_string())
        );
        assert_eq!(imported_session("nothing"), None);
    }

    struct FakeRunner {
        outputs: Vec<(String, String)>,
        calls: std::sync::Mutex<Vec<String>>,
    }

    impl CommandRunner for FakeRunner {
        fn run(&self, program: &str, args: &[&str]) -> std::io::Result<(i32, String)> {
            self.calls
                .lock()
                .unwrap()
                .push(format!("{program} {}", args.join(" ")));
            let joined = format!("{program} {}", args.join(" "));
            for (pattern, output) in &self.outputs {
                if joined.contains(pattern.as_str()) {
                    return Ok((0, output.clone()));
                }
            }
            Ok((0, String::new()))
        }
    }

    struct FakeSpawn(std::sync::Mutex<Vec<String>>);

    impl OpencodeSpawn for FakeSpawn {
        fn spawn(&self, args: &[String]) -> std::io::Result<i32> {
            self.0.lock().unwrap().push(args.join(" "));
            Ok(0)
        }
    }

    fn pr_matches() -> ArgMatches {
        clap::Command::new("opencode")
            .arg(
                clap::Arg::new("number")
                    .required(true)
                    .value_parser(clap::value_parser!(i64)),
            )
            .try_get_matches_from(["opencode", "42"])
            .expect("parse")
    }

    #[test]
    fn pr_happy_path_checks_out_and_spawns_opencode() {
        let matches = pr_matches();
        let runner = FakeRunner {
            outputs: vec![
                (
                    "pr view".to_string(),
                    r#"{"isCrossRepository":false,"body":"see https://opncd.ai/s/abc123 ok"}"#
                        .to_string(),
                ),
                (
                    "import".to_string(),
                    "Imported session: ses_789".to_string(),
                ),
            ],
            calls: std::sync::Mutex::new(Vec::new()),
        };
        let spawn_calls = FakeSpawn(std::sync::Mutex::new(Vec::new()));
        let (mut ui, _captured) = crate::ui::Ui::capture(false);
        run(&matches, &mut ui, &runner, &spawn_calls).expect("pr run");
        let calls = runner.calls.lock().unwrap().clone();
        assert!(
            calls
                .iter()
                .any(|call| call.starts_with("gh pr checkout 42 --branch pr/42 --force")),
            "{calls:?}"
        );
        assert!(
            calls
                .iter()
                .any(|call| call.starts_with("opencode import https://opncd.ai/s/abc123")),
            "{calls:?}"
        );
        let spawned = spawn_calls.0.lock().unwrap().clone();
        assert_eq!(spawned, vec!["-s ses_789".to_string()], "{spawned:?}");
    }
}
