//! Dynamic bash-tool prompt rendering — port of `tool/shell/prompt.ts`.
//!
//! `render` builds the bash tool's description from the vendored
//! `tool/txt/shell.txt` template by substituting the `${...}` placeholders
//! with the per-shell profile values (bash / pwsh / powershell / cmd).

use std::collections::HashMap;
use std::path::Path;

use anyhow::anyhow;

use crate::tool::shell::id::TOOL_ID;

const DESCRIPTION: &str = include_str!("../txt/shell.txt");

const PS: &[&str] = &["powershell", "pwsh"];
const CMD: &[&str] = &["cmd"];

/// `prompt.Limits` — resolved truncation limits.
#[derive(Debug, Clone, Copy)]
pub struct Limits {
    pub max_lines: usize,
    pub max_bytes: usize,
}

/// `prompt.render` result: `{ description, parameters }`.
#[derive(Debug, Clone)]
pub struct RenderedPrompt {
    pub description: String,
    pub parameters: serde_json::Value,
}

/// The JSON Schema of the shell tool parameters. Hand-authored to mirror the
/// TS `Schema.Struct` declaration (`command: String`, `timeout?:
/// PositiveInt`, `workdir?: String`) until the golden capture (spec §2.3)
/// lands.
pub fn parameter_schema() -> serde_json::Value {
    serde_json::json!({
        "type": "object",
        "properties": {
            "command": {
                "type": "string",
                "description": "The command to execute"
            },
            "timeout": {
                "type": "integer",
                "minimum": 1,
                "description": "Optional timeout in milliseconds"
            },
            "workdir": {
                "type": "string",
                "description": "The working directory to run the command in. Defaults to the current directory. Use this instead of 'cd' commands."
            }
        },
        "required": ["command"],
        "additionalProperties": false
    })
}

/// `renderPrompt` (prompt.ts:28-34): `${key}` substitution; a placeholder
/// without a value is an error.
fn render_prompt(template: &str, values: &HashMap<&str, String>) -> anyhow::Result<String> {
    let re = regex::Regex::new(r"\$\{(\w+)\}")?;
    let mut out = String::with_capacity(template.len());
    let mut last = 0;
    for cap in re.captures_iter(template) {
        let m = cap.get(0).expect("match exists");
        out.push_str(&template[last..m.start()]);
        let key = cap.get(1).expect("group 1 exists").as_str();
        let value = values
            .get(key)
            .ok_or_else(|| anyhow!("Missing shell prompt value: {key}"))?;
        out.push_str(value);
        last = m.end();
    }
    out.push_str(&template[last..]);
    Ok(out)
}

fn shell_display_name(name: &str) -> &str {
    match name {
        "pwsh" => "PowerShell (7+)",
        "powershell" => "Windows PowerShell (5.1)",
        "cmd" => "cmd.exe",
        other => other,
    }
}

fn powershell_notes(name: &str) -> &'static str {
    if name == "pwsh" {
        r#"# PowerShell (7+) shell notes
- This cross-platform shell supports pipeline chain operators (`&&` and `||`).
- Use double quotes for interpolated strings (`"Hello $name"`), single quotes for verbatim strings.
- Prefer full cmdlet names like `Get-ChildItem`, `Set-Content`, `Remove-Item`, and `New-Item` over aliases.
- Use `$(...)` for subexpressions. Use `@(...)` for array expressions.
- To call a native executable whose path contains spaces, use the call operator: `& "path/to/exe" args`.
- Escape special characters with the PowerShell backtick character."#
    } else if name == "powershell" {
        r#"# Windows PowerShell (5.1) shell notes
- Use `cmd1; if ($?) { cmd2 }` to chain dependent commands.
- Use double quotes for interpolated strings (`"Hello $name"`), single quotes for verbatim strings.
- Prefer full cmdlet names like `Get-ChildItem`, `Set-Content`, `Remove-Item`, and `New-Item` over aliases.
- Use `$(...)` for subexpressions. Use `@(...)` for array expressions.
- To call a native executable whose path contains spaces, use the call operator: `& "path/to/exe" args`.
- Escape special characters with the PowerShell backtick character."#
    } else {
        ""
    }
}

fn chain_guidance(name: &str) -> &'static str {
    if name == "powershell" {
        "If the commands depend on each other and must run sequentially, avoid '&&' in this shell because Windows PowerShell (5.1) does not support it. Use PowerShell conditionals such as `cmd1; if ($?) { cmd2 }` when later commands must depend on earlier success."
    } else if PS.contains(&name) {
        "If the commands depend on each other and must run sequentially, use a single bash tool call with '&&' to chain them together (e.g., `git add . && git commit -m \"message\" && git push`). For instance, if one operation must complete before another starts (like New-Item before Copy-Item, Write before bash for git operations, or git add before git commit), run these operations sequentially instead."
    } else if CMD.contains(&name) {
        "If the commands depend on each other and must run sequentially, use a single bash tool call with `&&` to chain them together (e.g., `mkdir out && dir out`). For instance, if one operation must complete before another starts, run these operations sequentially instead."
    } else {
        "If the commands depend on each other and must run sequentially, use a single Bash call with '&&' to chain them together (e.g., `git add . && git commit -m \"message\" && git push`). For instance, if one operation must complete before another starts (like mkdir before cp, Write before Bash for git operations, or git add before git commit), run these operations sequentially instead."
    }
}

fn bash_command_section(chain: &str, limits: &Limits, default_timeout_ms: u64) -> String {
    format!(
        r#"Before executing the command, please follow these steps:

1. Directory Verification:
   - If the command will create new directories or files, first use `ls` to verify the parent directory exists and is the correct location
   - For example, before running "mkdir foo/bar", first use `ls foo` to check that "foo" exists and is the intended parent directory

2. Command Execution:
   - Always quote file paths that contain spaces with double quotes (e.g., rm "path with spaces/file.txt")
   - Examples of proper quoting:
     - mkdir "/Users/name/My Documents" (correct)
     - mkdir /Users/name/My Documents (incorrect - will fail)
     - python "/path/with spaces/script.py" (correct)
     - python /path/with spaces/script.py (incorrect - will fail)
   - After ensuring proper quoting, execute the command.
   - Capture the output of the command.

Usage notes:
  - The command argument is required.
  - You can specify an optional timeout in milliseconds. If not specified, commands will time out after {timeout}ms.
  - If the output exceeds {max_lines} lines or {max_bytes} bytes, it will be truncated and the full output will be written to a file. You can use Read with offset/limit to read specific sections or Grep to search the full content. Do NOT use `head`, `tail`, or other truncation commands to limit output; the full output will already be captured to a file for more precise searching.

  - Avoid using Bash with the `find`, `grep`, `cat`, `head`, `tail`, `sed`, `awk`, or `echo` commands, unless explicitly instructed or when these commands are truly necessary for the task. Instead, always prefer using the dedicated tools for these commands:
    - File search: Use Glob (NOT find or ls)
    - Content search: Use Grep (NOT grep or rg)
    - Read files: Use Read (NOT cat/head/tail)
    - Edit files: Use Edit (NOT sed/awk)
    - Write files: Use Write (NOT echo >/cat <<EOF)
    - Communication: Output text directly (NOT echo/printf)
  - When issuing multiple commands:
    - If the commands are independent and can run in parallel, make multiple bash tool calls in a single message. For example, if you need to run "git status" and "git diff", send a single message with two bash tool calls in parallel.
    - {chain}
    - Use ';' only when you need to run commands sequentially but don't care if earlier commands fail
    - DO NOT use newlines to separate commands (newlines are ok in quoted strings)
  - AVOID using `cd <directory> && <command>`. Use the `workdir` parameter to change directories instead.
    <good-example>
    Use workdir="/foo/bar" with command: pytest tests
    </good-example>
    <bad-example>
    cd /foo/bar && pytest tests
    </bad-example>"#,
        timeout = default_timeout_ms,
        max_lines = limits.max_lines,
        max_bytes = limits.max_bytes,
        chain = chain,
    )
}

fn powershell_command_section(
    name: &str,
    chain: &str,
    path_sep: &str,
    limits: &Limits,
    default_timeout_ms: u64,
) -> String {
    format!(
        r#"{notes}

Before executing the command, please follow these steps:

1. Directory Verification:
   - If the command will create new directories or files, first use `Test-Path -LiteralPath <parent>` to verify the parent directory exists and is the correct location
   - For example, before creating `foo{path_sep}bar`, first use `Test-Path -LiteralPath "foo"` to check that `foo` exists and is the intended parent directory

2. Command Execution:
   - Always quote file paths that contain spaces with double quotes (e.g., Remove-Item -LiteralPath "path with spaces{path_sep}file.txt")
   - Examples of proper quoting:
     - New-Item -ItemType Directory -Path "My Documents" (correct)
     - New-Item -ItemType Directory -Path My Documents (incorrect - path is split)
     - & "path with spaces{path_sep}script.ps1" (correct)
     - path with spaces{path_sep}script.ps1 (incorrect - path is split and not invoked)
   - After ensuring proper quoting, execute the command.
   - Capture the output of the command.

Usage notes:
  - The command argument is required.
  - You can specify an optional timeout in milliseconds. If not specified, commands will time out after {timeout}ms.
  - If the output exceeds {max_lines} lines or {max_bytes} bytes, it will be truncated and the full output will be written to a file. You can use Read with offset/limit to read specific sections or Grep to search the full content. Do NOT use `Select-Object -First`, `Select-Object -Last`, or other truncation commands to limit output; the full output will already be captured to a file for more precise searching.

  - Avoid using Shell with PowerShell file/content cmdlets unless explicitly instructed or when these cmdlets are truly necessary for the task. Instead, always prefer using the dedicated tools for these commands:
    - File search: Use Glob (NOT Get-ChildItem)
    - Content search: Use Grep (NOT Select-String)
    - Read files: Use Read (NOT Get-Content)
    - Edit files: Use Edit (NOT Set-Content)
    - Write files: Use Write (NOT Set-Content/Out-File or here-strings)
    - Communication: Output text directly (NOT Write-Output/Write-Host)
  - When issuing multiple commands:
    - If the commands are independent and can run in parallel, make multiple bash tool calls in a single message. For example, if you need to run "git status" and "git diff", send a single message with two bash tool calls in parallel.
    - {chain}
    - Use `;` only when you need to run commands sequentially but don't care if earlier commands fail
    - DO NOT use newlines to separate commands (newlines are ok in quoted strings)
  - AVOID changing directories inside the command. Use the `workdir` parameter to change directories instead.
    <good-example>
    Use workdir="project{path_sep}subdir" with command: pytest tests
    </good-example>
    <bad-example>
    {bad_example}
    </bad-example>"#,
        notes = powershell_notes(name),
        path_sep = path_sep,
        timeout = default_timeout_ms,
        max_lines = limits.max_lines,
        max_bytes = limits.max_bytes,
        chain = chain,
        bad_example = if name == "powershell" {
            r#"Set-Location -LiteralPath "project{path_sep}subdir"; if ($?) { pytest tests }"#
        } else {
            r#"Set-Location -LiteralPath "project{path_sep}subdir" && pytest tests"#
        },
    )
}

fn cmd_command_section(chain: &str, limits: &Limits, default_timeout_ms: u64) -> String {
    format!(
        r#"# cmd.exe shell notes
- Use double quotes for paths with spaces.
- Use %VAR% for environment variables.
- Use `if exist` for existence checks.
- Use `call` when invoking batch files from another batch-style command.

Before executing the command, please follow these steps:

1. Directory Verification:
   - If the command will create new directories or files, first use `if exist` to verify the parent directory exists and is the correct location
   - For example, before creating `foo\bar`, first use `if exist "foo\" dir "foo"` to check that `foo` exists and is the intended parent directory

2. Command Execution:
   - Always quote file paths that contain spaces with double quotes (e.g., del "path with spaces\file.txt")
   - Examples of proper quoting:
     - mkdir "My Documents" (correct)
     - mkdir My Documents (incorrect - path is split)
     - call "path with spaces\script.bat" (correct)
     - path with spaces\script.bat (incorrect - path is split and not invoked correctly)
   - After ensuring proper quoting, execute the command.
   - Capture the output of the command.

Usage notes:
  - The command argument is required.
  - You can specify an optional timeout in milliseconds. If not specified, commands will time out after {timeout}ms.
  - If the output exceeds {max_lines} lines or {max_bytes} bytes, it will be truncated and the full output will be written to a file. You can use Read with offset/limit to read specific sections or Grep to search the full content. Do NOT use `more` or other pagination commands to limit output; the full output will already be captured to a file for more precise searching.

  - Avoid using Shell with cmd.exe file/content commands unless explicitly instructed or when these commands are truly necessary for the task. Instead, always prefer using the dedicated tools for these commands:
    - File search: Use Glob (NOT dir /s)
    - Content search: Use Grep (NOT findstr)
    - Read files: Use Read (NOT type)
    - Edit files: Use Edit (NOT copy)
    - Write files: Use Write (NOT echo > file)
    - Communication: Output text directly (NOT echo)
  - When issuing multiple commands:
    - If the commands are independent and can run in parallel, make multiple bash tool calls in a single message. For example, if you need to run "dir" and "where cmd", send a single message with two bash tool calls in parallel.
    - {chain}
    - Use `&` only when you need to run commands sequentially but don't care if earlier commands fail
    - DO NOT use newlines to separate commands (newlines are ok in quoted strings)
  - AVOID changing directories inside the command. Use the `workdir` parameter to change directories instead.
    <good-example>
    Use workdir="project\subdir" with command: dir
    </good-example>
    <bad-example>
    cd /d "project\subdir" && dir
    </bad-example>"#,
        timeout = default_timeout_ms,
        max_lines = limits.max_lines,
        max_bytes = limits.max_bytes,
        chain = chain,
    )
}

struct Profile {
    intro: String,
    workdir_section: &'static str,
    command_section: String,
    git_commands: &'static str,
    git_command_restriction: &'static str,
    create_pr_instruction: String,
    create_pr_example: String,
}

fn profile(name: &str, platform: &str, limits: &Limits, default_timeout_ms: u64) -> Profile {
    let is_power_shell = PS.contains(&name);
    let chain = chain_guidance(name);
    if CMD.contains(&name) {
        return Profile {
            intro: format!(
                "Executes a given {} command with optional timeout, ensuring proper handling and security measures.",
                shell_display_name(name)
            ),
            workdir_section:
                "All commands run in the current working directory by default. Use the `workdir` parameter if you need to run a command in a different directory. AVOID changing directories inside the command - use `workdir` instead.",
            command_section: cmd_command_section(chain, limits, default_timeout_ms),
            git_commands: "git commands",
            git_command_restriction: "git commands",
            create_pr_instruction:
                "Create PR using a temporary body file so cmd.exe quoting stays simple."
                .to_string(),
            create_pr_example:
                "(\n  echo ## Summary\n  echo - ^<1-3 bullet points^>\n) > pr-body.txt\ngh pr create --title \"the pr title\" --body-file pr-body.txt".to_string(),
        };
    }
    if is_power_shell {
        return Profile {
            intro: format!(
                "Executes a given {} command with optional timeout, ensuring proper handling and security measures.",
                shell_display_name(name)
            ),
            workdir_section:
                "All commands run in the current working directory by default. Use the `workdir` parameter if you need to run a command in a different directory. AVOID changing directories inside the command - use `workdir` instead.",
            command_section: powershell_command_section(
                name,
                chain,
                if platform == "win32" { "\\" } else { "/" },
                limits,
                default_timeout_ms,
            ),
            git_commands: "git commands",
            git_command_restriction: "git commands",
            create_pr_instruction:
                "Create PR using gh pr create with a PowerShell here-string to pass the body correctly.".to_string(),
            create_pr_example: "gh pr create --title \"the pr title\" --body @'\n## Summary\n- <1-3 bullet points>\n'@".to_string(),
        };
    }
    Profile {
        intro:
            "Executes a given bash command in a persistent shell session with optional timeout, ensuring proper handling and security measures.".to_string(),
        workdir_section:
            "All commands run in the current working directory by default. Use the `workdir` parameter if you need to run a command in a different directory. AVOID using `cd <directory> && <command>` patterns - use `workdir` instead.",
        command_section: bash_command_section(chain, limits, default_timeout_ms),
        git_commands: "bash commands",
        git_command_restriction: "git bash commands",
        create_pr_instruction:
            "Create PR using gh pr create with the format below. Use a HEREDOC to pass the body to ensure correct formatting.".to_string(),
        create_pr_example:
            "gh pr create --title \"the pr title\" --body \"$(cat <<'EOF'\n## Summary\n<1-3 bullet points>".to_string(),
    }
}

/// `render` (prompt.ts:273-291): build the shell tool's description and
/// parameters. `platform` is the TS `process.platform`; `tmp` is
/// `Global.Path.tmp` (injected for testability).
pub fn render(
    name: &str,
    platform: &str,
    limits: Limits,
    default_timeout_ms: u64,
    tmp: &Path,
) -> anyhow::Result<RenderedPrompt> {
    let selected = profile(name, platform, &limits, default_timeout_ms);
    let mut values: HashMap<&str, String> = HashMap::new();
    values.insert("intro", selected.intro);
    values.insert("os", platform.to_string());
    values.insert("shell", name.to_string());
    values.insert("tmp", tmp.to_string_lossy().to_string());
    values.insert("workdirSection", selected.workdir_section.to_string());
    values.insert("commandSection", selected.command_section);
    values.insert("gitCommands", selected.git_commands.to_string());
    values.insert("toolName", TOOL_ID.to_string());
    values.insert(
        "gitCommandRestriction",
        selected.git_command_restriction.to_string(),
    );
    values.insert("createPrInstruction", selected.create_pr_instruction);
    values.insert("createPrExample", selected.create_pr_example);
    Ok(RenderedPrompt {
        description: render_prompt(DESCRIPTION, &values)?,
        parameters: parameter_schema(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn limits() -> Limits {
        Limits {
            max_lines: 2000,
            max_bytes: 50 * 1024,
        }
    }

    fn render_shell(name: &str) -> RenderedPrompt {
        render(name, "linux", limits(), 120_000, Path::new("/tmp/opencode")).expect("renders")
    }

    #[test]
    fn render_prompt_substitutes_values() {
        let mut values = HashMap::new();
        values.insert("a", "one".to_string());
        values.insert("b", "two".to_string());
        assert_eq!(
            render_prompt("${a} and ${b}!", &values).unwrap(),
            "one and two!"
        );
    }

    #[test]
    fn render_prompt_missing_value_errors() {
        let values = HashMap::new();
        assert_eq!(
            render_prompt("${missing}", &values)
                .unwrap_err()
                .to_string(),
            "Missing shell prompt value: missing"
        );
    }

    #[test]
    fn bash_profile_golden() {
        let out = render_shell("bash");
        let description = out.description;
        assert!(
            description.starts_with("Executes a given bash command in a persistent shell session"),
            "{description}"
        );
        assert!(description.contains("Be aware: OS: linux, Shell: bash"));
        assert!(description.contains("time out after 120000ms"));
        assert!(description.contains("exceeds 2000 lines or 51200 bytes"));
        assert!(description.contains("AVOID using `cd <directory> && <command>`"));
        assert!(
            description.contains("Use `/tmp/opencode` for temporary work outside the workspace.")
        );
        assert!(description.contains("git bash commands"));
        assert!(
            !description.contains("${"),
            "unsubstituted placeholder left: {description}"
        );
    }

    #[test]
    fn pwsh_profile_golden() {
        let description = render_shell("pwsh").description;
        assert!(
            description
                .starts_with("Executes a given PowerShell (7+) command with optional timeout"),
            "{description}"
        );
        assert!(description.contains("# PowerShell (7+) shell notes"));
        assert!(description.contains("Be aware: OS: linux, Shell: pwsh"));
        assert!(!description.contains("${"));
    }

    #[test]
    fn powershell_profile_golden() {
        let description = render_shell("powershell").description;
        assert!(
            description.starts_with(
                "Executes a given Windows PowerShell (5.1) command with optional timeout"
            ),
            "{description}"
        );
        assert!(
            description.contains("use a single bash tool call with '&&' to chain them together")
        );
        assert!(description
            .contains("Set-Location -LiteralPath \"project/subdir\"; if ($?) { pytest tests }"));
        assert!(!description.contains("${"));
    }

    #[test]
    fn cmd_profile_golden() {
        let description = render_shell("cmd").description;
        assert!(
            description.starts_with("Executes a given cmd.exe command with optional timeout"),
            "{description}"
        );
        assert!(description.contains("# cmd.exe shell notes"));
        assert!(description.contains("cd /d \"project\\subdir\" && dir"));
        assert!(description.contains("git commands"));
        assert!(!description.contains("${"));
    }

    #[test]
    fn parameters_schema_shape() {
        let schema = parameter_schema();
        assert_eq!(schema["type"], "object");
        assert_eq!(schema["properties"]["command"]["type"], "string");
        assert_eq!(
            schema["properties"]["command"]["description"],
            "The command to execute"
        );
        assert_eq!(schema["properties"]["timeout"]["type"], "integer");
        assert_eq!(schema["properties"]["timeout"]["minimum"], 1);
        assert_eq!(schema["required"], serde_json::json!(["command"]));
        assert_eq!(schema["additionalProperties"], serde_json::json!(false));
    }
}
