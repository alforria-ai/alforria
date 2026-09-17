//! Tree-sitter parse pipeline + `ShellParser` seam (spec M4.4).
//!
//! TS parses commands with web-tree-sitter (bash + powershell grammars,
//! shell.ts:91-125) and collects, for every `command` node, its flat parts
//! and source text (shell.ts:68-125). The Rust port keeps the parser behind
//! a seam: the bash implementation uses the native `tree-sitter-bash`
//! crate; the PowerShell/cmd kinds delegate to a conservative fallback
//! (single command, whitespace tokens) — there is no maintained Rust
//! `tree-sitter-powershell` crate we can rely on.
//!
//! Native `tree-sitter-bash` node inventory (verified against 0.23 at
//! implementation time): `command` children are `command_name`, `word`,
//! `string`, `raw_string`, `concatenation`, `simple_expansion`,
//! `command_substitution`, `variable_assignment`, … — there is no
//! `command_elements` wrapper (arguments are direct children) and
//! redirections sit *outside* the `command` node, under
//! `redirected_statement`. Part collection accepts the same node kinds TS
//! does; unknown kinds are skipped, so both grammars yield the same part
//! lists on representative commands.

use anyhow::Result;

/// A single part of a parsed command (TS `Part`, shell.ts:68-71).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Part {
    /// Node type (e.g. `command_name`, `word`, `string`, …).
    pub kind: String,
    pub text: String,
}

/// One `command` node: its flat parts plus source text (TS `parts()` +
/// `source()`, shell.ts:91-125).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CommandNode {
    pub parts: Vec<Part>,
    pub source: String,
}

/// The parser seam (spec M4.4): flatten a command into the `command` nodes
/// TS sees.
pub trait ShellParser: Send + Sync {
    fn parse(&self, command: &str) -> Result<Vec<CommandNode>>;
}

/// Node kinds accepted as command parts. TS also accepts
/// `command_name_expr`, which the native bash grammar never produces; it is
/// kept here so the accept-list mirrors shell.ts:104-113 exactly.
const ACCEPTED_KINDS: &[&str] = &[
    "command_name",
    "command_name_expr",
    "word",
    "string",
    "raw_string",
    "concatenation",
];

/// Kinds skipped inside a `command_elements` wrapper in the wasm grammar
/// (shell.ts:98-99). The native grammar has no such wrapper, but skipping
/// redirections stays correct either way.
const SKIPPED_KINDS: &[&str] = &["command_argument_sep", "redirection"];

/// `parts(node)` (shell.ts:91-117), adapted to the native grammar: iterate
/// the direct children of a `command` node, descend into a
/// `command_elements` wrapper when present, and accept only the part kinds
/// the TS accept-list names.
fn parts(command: &tree_sitter::Node<'_>, src: &str) -> Vec<Part> {
    let mut out = Vec::new();
    let mut cursor = command.walk();
    for child in command.children(&mut cursor) {
        let kind = child.kind();
        if kind == "command_elements" {
            let mut inner = child.walk();
            for item in child.children(&mut inner) {
                if SKIPPED_KINDS.contains(&item.kind()) {
                    continue;
                }
                out.push(part(item, src));
            }
            continue;
        }
        if ACCEPTED_KINDS.contains(&kind) {
            out.push(part(child, src));
        }
    }
    out
}

fn part(node: tree_sitter::Node<'_>, src: &str) -> Part {
    let start = node.byte_range();
    Part {
        kind: node.kind().to_string(),
        text: src[start].to_string(),
    }
}

/// `source(node)` (shell.ts:119-121): prefer the parent
/// `redirected_statement` text so redirections appear in the pattern.
fn source(command: &tree_sitter::Node<'_>, src: &str) -> String {
    let node = match command.parent() {
        Some(parent) if parent.kind() == "redirected_statement" => parent,
        _ => *command,
    };
    let range = node.byte_range();
    src[range].trim().to_string()
}

/// `commands(node)` (shell.ts:123-125): all `command` descendants, in
/// document order.
fn commands<'a>(root: tree_sitter::Node<'a>) -> Vec<tree_sitter::Node<'a>> {
    let mut out = Vec::new();
    visit(&mut out, root);
    return out;

    fn visit<'a>(out: &mut Vec<tree_sitter::Node<'a>>, node: tree_sitter::Node<'a>) {
        if node.kind() == "command" {
            out.push(node);
        }
        let mut cursor = node.walk();
        for child in node.children(&mut cursor) {
            visit(out, child);
        }
    }
}

/// The bash tree-sitter implementation of [`ShellParser`].
#[derive(Debug, Default)]
pub struct BashParser;

impl BashParser {
    pub fn new() -> Self {
        BashParser
    }
}

impl ShellParser for BashParser {
    fn parse(&self, command: &str) -> Result<Vec<CommandNode>> {
        let mut parser = tree_sitter::Parser::new();
        parser
            .set_language(&tree_sitter_bash::LANGUAGE.into())
            .map_err(|err| anyhow::anyhow!("failed to load bash grammar: {err}"))?;
        let tree = parser
            .parse(command, None)
            .ok_or_else(|| anyhow::anyhow!("Failed to parse command"))?;
        let root = tree.root_node();
        let mut out = Vec::new();
        for node in commands(root) {
            out.push(CommandNode {
                parts: parts(&node, command),
                source: source(&node, command),
            });
        }
        Ok(out)
    }
}

/// Conservative fallback for PowerShell/cmd: a single command whose parts
/// are the whitespace-split tokens of the whole command (spec M4.4).
#[derive(Debug, Default)]
pub struct FallbackParser;

impl FallbackParser {
    pub fn new() -> Self {
        FallbackParser
    }
}

impl ShellParser for FallbackParser {
    fn parse(&self, command: &str) -> Result<Vec<CommandNode>> {
        let trimmed = command.trim();
        if trimmed.is_empty() {
            return Ok(Vec::new());
        }
        let parts = trimmed
            .split_whitespace()
            .map(|text| Part {
                kind: "word".to_string(),
                text: text.to_string(),
            })
            .collect();
        Ok(vec![CommandNode {
            parts,
            source: trimmed.to_string(),
        }])
    }
}

/// Select the parser for a shell: the bash grammar for anything that is not
/// a PowerShell shell (mirrors TS, which only distinguishes `ps`), and the
/// conservative fallback for `pwsh`/`powershell`.
pub fn parser_for(shell_name: &str) -> Box<dyn ShellParser> {
    match shell_name {
        "powershell" | "pwsh" => Box::new(FallbackParser::new()),
        _ => Box::new(BashParser::new()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn kinds(parts: &[Part]) -> Vec<(String, String)> {
        parts
            .iter()
            .map(|p| (p.kind.clone(), p.text.clone()))
            .collect()
    }

    #[test]
    fn parse_simple_command() {
        let nodes = BashParser::new().parse("ls -la").unwrap();
        assert_eq!(nodes.len(), 1);
        assert_eq!(
            kinds(&nodes[0].parts),
            vec![
                ("command_name".to_string(), "ls".to_string()),
                ("word".to_string(), "-la".to_string()),
            ]
        );
        assert_eq!(nodes[0].source, "ls -la");
    }

    #[test]
    fn parse_quoted_arguments() {
        let nodes = BashParser::new()
            .parse("echo 'hello world' \"a b\"")
            .unwrap();
        assert_eq!(
            kinds(&nodes[0].parts),
            vec![
                ("command_name".to_string(), "echo".to_string()),
                ("raw_string".to_string(), "'hello world'".to_string()),
                ("string".to_string(), "\"a b\"".to_string()),
            ]
        );
    }

    #[test]
    fn parse_redirection_source_prefers_parent() {
        let nodes = BashParser::new()
            .parse("echo 'hello world' > out.txt")
            .unwrap();
        assert_eq!(nodes.len(), 1);
        // The redirect target is not a part...
        assert_eq!(
            kinds(&nodes[0].parts),
            vec![
                ("command_name".to_string(), "echo".to_string()),
                ("raw_string".to_string(), "'hello world'".to_string()),
            ]
        );
        // ...but the source covers the whole redirected statement.
        assert_eq!(nodes[0].source, "echo 'hello world' > out.txt");
    }

    #[test]
    fn parse_lists_and_pipelines_yield_multiple_commands() {
        let nodes = BashParser::new().parse("cd /tmp && ls | grep foo").unwrap();
        assert_eq!(nodes.len(), 3);
        assert_eq!(nodes[0].source, "cd /tmp");
        assert_eq!(nodes[1].source, "ls");
        assert_eq!(nodes[2].source, "grep foo");
    }

    #[test]
    fn parse_subshell_commands() {
        let nodes = BashParser::new().parse("(cd /tmp && ls)").unwrap();
        assert_eq!(nodes.len(), 2);
        assert_eq!(nodes[0].source, "cd /tmp");
        assert_eq!(nodes[1].source, "ls");
    }

    #[test]
    fn parse_skips_command_substitution_from_parts() {
        let nodes = BashParser::new().parse("echo $(date)").unwrap();
        assert_eq!(
            kinds(&nodes[0].parts),
            vec![("command_name".to_string(), "echo".to_string())]
        );
        // Nested commands are separate command nodes.
        assert_eq!(nodes.len(), 2);
        assert_eq!(nodes[1].source, "date");
    }

    #[test]
    fn parse_concatenation() {
        let nodes = BashParser::new().parse("ls $HOME/doc").unwrap();
        assert_eq!(
            kinds(&nodes[0].parts),
            vec![
                ("command_name".to_string(), "ls".to_string()),
                ("concatenation".to_string(), "$HOME/doc".to_string()),
            ]
        );
    }

    #[test]
    fn fallback_whitespace_tokens() {
        let nodes = FallbackParser::new()
            .parse("Get-ChildItem -Path C:\\Temp")
            .unwrap();
        assert_eq!(nodes.len(), 1);
        assert_eq!(nodes[0].parts.len(), 3);
        assert_eq!(nodes[0].parts[0].text, "Get-ChildItem");
        assert_eq!(nodes[0].source, "Get-ChildItem -Path C:\\Temp");
    }

    #[test]
    fn parser_for_selects_by_shell_name() {
        assert_eq!(parser_for("bash").parse("ls").unwrap().len(), 1);
        assert_eq!(parser_for("pwsh").parse("Get-Date").unwrap().len(), 1);
    }
}
