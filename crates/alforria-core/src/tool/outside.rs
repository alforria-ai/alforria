//! Which tools return content from outside into a session (alforria
//! extension).
//!
//! A session that has seen outside content is marked
//! ([`crate::session::store::SessionMark::OutsideContent`]) — provenance,
//! not a permission: what it then writes to memory is unverified, a trigger
//! it schedules carries the mark, and excerpts of it are fenced. The mark is
//! set at the one place a tool's result is appended to a session
//! (`session::processor`), from this table. It does not depend on a tool
//! remembering to say so.
//!
//! Every built-in tool is classified here, and a test walks the registry
//! the server builds: a tool added without a line in [`BUILTIN`] fails it.
//! A name that is not in the table — every MCP tool, every plugin tool — is
//! [`Outside::Always`].
//!
//! What this does not cover, on purpose: `bash` (a shell can fetch
//! anything; marking it would mark every coding session and leave memory
//! unusable) and reads of ordinary project files. They are stated as limits
//! in the threat model (`docs/unattended-server.md`).

use std::path::{Path, PathBuf};

use serde_json::Value;

/// Whether a tool's result is outside content.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Outside {
    /// It returns only what the session, the user or the project already
    /// holds.
    Never,
    /// Whatever it returns was written elsewhere: a web page, a search
    /// result, another program's answer.
    Always,
    /// It says so itself, result by result, with [`FLAG`] in its metadata:
    /// `memory` when it returns an unverified record, `session_search`
    /// when a hit comes from a marked session.
    Flagged,
    /// It reads the file its input names under this key: outside content
    /// when the file is in the upload area.
    ReadsPath(&'static str),
    /// It prints the content of the files it lists: outside content when
    /// one of them is in the upload area.
    ListsFiles,
    /// It returns what a child session produced: outside content when a
    /// child of the session is marked.
    Child,
}

/// The metadata key a [`Outside::Flagged`] tool sets to `true`.
pub const FLAG: &str = "outsideContent";

/// Every built-in tool, classified. Keep the reason beside the entry.
pub const BUILTIN: &[(&str, Outside)] = &[
    // The runtime's stand-in for a malformed call.
    ("invalid", Outside::Never),
    // The user's own answers.
    ("question", Outside::Never),
    // See the module comment: a stated limit.
    ("bash", Outside::Never),
    ("read", Outside::ReadsPath("filePath")),
    // File names only.
    ("glob", Outside::Never),
    ("grep", Outside::ListsFiles),
    ("edit", Outside::Never),
    ("write", Outside::Never),
    ("apply_patch", Outside::Never),
    ("task", Outside::Child),
    ("subagents", Outside::Child),
    ("workflow", Outside::Child),
    ("webfetch", Outside::Always),
    ("websearch", Outside::Always),
    ("todowrite", Outside::Never),
    // Skill files are loader files: the write guard keeps outsiders off them.
    ("skill", Outside::Never),
    // Code mode runs MCP tools.
    ("execute", Outside::Always),
    ("lsp", Outside::Never),
    ("plan_exit", Outside::Never),
    ("schedule", Outside::Never),
    ("memory", Outside::Flagged),
    ("session_search", Outside::Flagged),
    // The structured-output tool: the model's own answer.
    ("StructuredOutput", Outside::Never),
];

/// How `tool` is classified, if it is a built-in.
pub fn builtin(tool: &str) -> Option<Outside> {
    BUILTIN
        .iter()
        .find(|(id, _)| *id == tool)
        .map(|(_, class)| *class)
}

/// How `tool` is classified. Anything that is not a built-in is an MCP or
/// plugin tool: its answer was written elsewhere.
pub fn classify(tool: &str) -> Outside {
    builtin(tool).unwrap_or(Outside::Always)
}

/// One tool result, as the processor is about to append it.
pub struct ToolResult<'a> {
    pub tool: &'a str,
    pub input: &'a Value,
    pub metadata: Option<&'a Value>,
    pub output: &'a str,
}

/// What the decision needs to know about the session.
pub struct SessionFacts<'a> {
    /// The session's directory.
    pub directory: &'a Path,
    /// The upload area, relative to the directory or absolute
    /// (`server.upload.directories`).
    pub uploads: &'a [String],
    /// Whether a child of the session is marked.
    pub child_marked: &'a dyn Fn() -> bool,
}

fn resolved(path: &Path) -> PathBuf {
    std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf())
}

/// The upload directories that exist, resolved.
fn upload_roots(facts: &SessionFacts<'_>) -> Vec<PathBuf> {
    facts
        .uploads
        .iter()
        .map(|directory| facts.directory.join(directory))
        .filter(|root| root.exists())
        .map(|root| resolved(&root))
        .collect()
}

/// Whether `result` brings outside content into the session.
pub fn carries_outside_content(result: &ToolResult<'_>, facts: &SessionFacts<'_>) -> bool {
    match classify(result.tool) {
        Outside::Never => false,
        Outside::Always => true,
        Outside::Flagged => {
            // Anything but a plain `false` counts.
            result
                .metadata
                .and_then(|metadata| metadata.get(FLAG))
                .is_some_and(|flag| !matches!(flag, Value::Bool(false) | Value::Null))
        }
        Outside::ReadsPath(key) => {
            let Some(path) = result.input.get(key).and_then(Value::as_str) else {
                return false;
            };
            let roots = upload_roots(facts);
            if roots.is_empty() {
                return false;
            }
            let path = resolved(&facts.directory.join(path));
            roots.iter().any(|root| path.starts_with(root))
        }
        Outside::ListsFiles => {
            // As the tool printed them: under the directory as it was
            // given, or as it resolves.
            let mut roots = upload_roots(facts);
            roots.extend(
                facts
                    .uploads
                    .iter()
                    .map(|directory| facts.directory.join(directory))
                    .filter(|root| root.exists()),
            );
            roots.iter().any(|root| {
                let root = root.to_string_lossy();
                result.output.contains(&format!("{root}/"))
            })
        }
        Outside::Child => (facts.child_marked)(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn facts<'a>(directory: &'a Path, uploads: &'a [String]) -> SessionFacts<'a> {
        SessionFacts {
            directory,
            uploads,
            child_marked: &|| false,
        }
    }

    fn result<'a>(tool: &'a str, input: &'a Value, metadata: &'a Value) -> ToolResult<'a> {
        ToolResult {
            tool,
            input,
            metadata: Some(metadata),
            output: "",
        }
    }

    #[test]
    fn an_unknown_tool_is_an_mcp_tool_and_always_outside() {
        let none = json!({});
        let dir = Path::new("/nowhere");
        for tool in [
            "github_create_issue",
            "a_tool_added_next_year",
            "read_mcp_resource",
        ] {
            assert_eq!(classify(tool), Outside::Always, "{tool}");
            assert!(carries_outside_content(
                &result(tool, &none, &none),
                &facts(dir, &[])
            ));
        }
        for tool in ["webfetch", "websearch"] {
            assert!(carries_outside_content(
                &result(tool, &none, &none),
                &facts(dir, &[])
            ));
        }
        for tool in ["bash", "edit", "write", "glob", "todowrite", "question"] {
            assert!(!carries_outside_content(
                &result(tool, &none, &none),
                &facts(dir, &[])
            ));
        }
    }

    #[test]
    fn a_flagged_tool_decides_per_result_and_a_child_tool_by_its_children() {
        let none = json!({});
        let dir = Path::new("/nowhere");
        for tool in ["memory", "session_search"] {
            assert!(!carries_outside_content(
                &result(tool, &none, &none),
                &facts(dir, &[])
            ));
            assert!(!carries_outside_content(
                &result(tool, &none, &json!({ FLAG: false })),
                &facts(dir, &[])
            ));
            for flag in [json!(true), json!(1), json!("yes")] {
                assert!(carries_outside_content(
                    &result(tool, &none, &json!({ FLAG: flag })),
                    &facts(dir, &[])
                ));
            }
        }
        for tool in ["task", "subagents", "workflow"] {
            assert!(!carries_outside_content(
                &result(tool, &none, &none),
                &facts(dir, &[])
            ));
            assert!(carries_outside_content(
                &result(tool, &none, &none),
                &SessionFacts {
                    directory: dir,
                    uploads: &[],
                    child_marked: &|| true,
                }
            ));
        }
    }

    #[test]
    fn a_read_under_the_upload_area_is_outside_content_however_it_is_named() {
        let temp = crate::storage::test_support::TempDir::new("outside-uploads");
        let dir = resolved(temp.path());
        std::fs::create_dir_all(dir.join("uploads/in")).unwrap();
        std::fs::create_dir_all(dir.join("src")).unwrap();
        std::fs::write(dir.join("uploads/in/a.txt"), "x").unwrap();
        std::fs::write(dir.join("src/lib.rs"), "x").unwrap();
        let uploads = vec!["uploads".to_string()];
        let none = json!({});
        let read = |path: &str| {
            carries_outside_content(
                &result("read", &json!({ "filePath": path }), &none),
                &facts(&dir, &uploads),
            )
        };
        assert!(read("uploads/in/a.txt"));
        assert!(read(&dir.join("uploads/in/a.txt").to_string_lossy()));
        assert!(read("src/../uploads/in/a.txt"));
        assert!(!read("src/lib.rs"));
        #[cfg(unix)]
        {
            std::os::unix::fs::symlink(dir.join("uploads/in/a.txt"), dir.join("src/link.txt"))
                .unwrap();
            assert!(read("src/link.txt"), "a link into the area reads the area");
        }
        // No upload area configured, or none on disk: nothing to tell.
        assert!(!carries_outside_content(
            &result("read", &json!({ "filePath": "uploads/in/a.txt" }), &none),
            &facts(&dir, &[]),
        ));
        // grep prints the files it matched in.
        let listing = format!(
            "Found 1 match\n{}/uploads/in/a.txt:\n  Line 1: x",
            dir.display()
        );
        let grep = ToolResult {
            tool: "grep",
            input: &none,
            metadata: None,
            output: &listing,
        };
        assert!(carries_outside_content(&grep, &facts(&dir, &uploads)));
        let elsewhere = format!("Found 1 match\n{}/src/lib.rs:\n  Line 1: x", dir.display());
        let grep = ToolResult {
            tool: "grep",
            input: &none,
            metadata: None,
            output: &elsewhere,
        };
        assert!(!carries_outside_content(&grep, &facts(&dir, &uploads)));
    }
}
