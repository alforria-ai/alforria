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
//! `grep` and `glob` are judged by the files they returned, as the tool
//! itself lists them ([`ReturnedPaths`]) — data the tool computed, each
//! path resolved here; never by reading the text it printed, which holds
//! file names and file content an uploader chose. A search that could not
//! list everything it returned (a result cut at the tool's limit, a failed
//! call) is judged by the tree its input names instead, and only when the
//! upload area holds something.
//!
//! What this does not cover, on purpose: `bash` (a shell can fetch
//! anything; marking it would mark every coding session and leave memory
//! unusable) and reads of ordinary project files. They are stated as limits
//! in the threat model (`docs/unattended-server.md`).

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use serde_json::Value;

/// Where a tool that returns files leaves the list of them, for the
/// marking decision (`Extra::returned_paths`): every file a line or a name
/// of which is in its result, as the tool resolved it. A tool sets it only
/// when the list is complete; a result cut at the tool's limit sets
/// nothing, and the decision falls back to the tree that was searched.
/// It never reaches the model, the session's parts or the wire.
#[derive(Debug, Clone, Default)]
pub struct ReturnedPaths(Arc<Mutex<Option<Vec<PathBuf>>>>);

impl ReturnedPaths {
    /// The complete list of files this call returned something of.
    pub fn set(&self, paths: Vec<PathBuf>) {
        *self
            .0
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = Some(paths);
    }

    /// The list, if the tool left one.
    pub fn take(&self) -> Option<Vec<PathBuf>> {
        self.0
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .take()
    }
}

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
    /// It searches the tree its input names under this key (the session's
    /// directory when it names none) and prints names or lines of what it
    /// finds: outside content when a file it returned is in the upload
    /// area, by the tool's own list of them ([`ReturnedPaths`]). Without a
    /// complete list — the result was cut at the tool's limit, or the call
    /// failed — it is outside content when the upload area holds something
    /// and the searched tree is inside, is, or contains it. Never decided
    /// from the text the tool printed.
    SearchesPath(&'static str),
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
    // File names are an uploader's words too.
    ("glob", Outside::SearchesPath("path")),
    ("grep", Outside::SearchesPath("path")),
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
    /// The call's id, when the caller has it: what the tool's list of
    /// returned files is kept under (`SessionStore::report_returned_paths`).
    pub call_id: Option<&'a str>,
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
    /// The files this call returned something of, as the tool listed them
    /// ([`ReturnedPaths`]). `None`: it left no complete list.
    pub returned: Option<&'a [PathBuf]>,
}

/// The upload directories that exist, resolved as the kernel would
/// (`protect::resolve_path`). `Err`: one of them could not be resolved, and
/// the caller takes the marking answer.
fn upload_roots(facts: &SessionFacts<'_>) -> Result<Vec<PathBuf>, ()> {
    let mut roots = Vec::new();
    for directory in facts.uploads {
        let root =
            crate::protect::resolve_path(&facts.directory.join(directory)).map_err(|_| ())?;
        match std::fs::symlink_metadata(&root) {
            Ok(_) => roots.push(root),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(_) => return Err(()),
        }
    }
    Ok(roots)
}

/// Whether an upload directory holds anything. One that cannot be listed
/// counts as holding something.
fn holds_something(root: &Path) -> bool {
    match std::fs::read_dir(root) {
        Ok(mut entries) => entries.next().is_some(),
        Err(error) => error.kind() != std::io::ErrorKind::NotFound,
    }
}

/// Whether `result` brings outside content into the session. Decided from
/// the call's structured input, the tool's own list of the files it
/// returned and the filesystem, never from the text a tool printed;
/// whatever cannot be decided counts as outside content.
pub fn carries_outside_content(result: &ToolResult<'_>, facts: &SessionFacts<'_>) -> bool {
    let class = classify(result.tool);
    match class {
        Outside::Never => false,
        Outside::Always => true,
        Outside::Flagged => {
            // Anything but a plain `false` counts.
            result
                .metadata
                .and_then(|metadata| metadata.get(FLAG))
                .is_some_and(|flag| !matches!(flag, Value::Bool(false) | Value::Null))
        }
        Outside::ReadsPath(key) | Outside::SearchesPath(key) => {
            let mut roots = match upload_roots(facts) {
                Ok(roots) if roots.is_empty() => return false,
                Ok(roots) => roots,
                Err(()) => return true,
            };
            if matches!(class, Outside::SearchesPath(_)) {
                // The tool said which files it returned: those decide. One
                // that cannot be resolved cannot be placed, and counts.
                if let Some(returned) = facts.returned {
                    return returned
                        .iter()
                        .any(|path| match crate::protect::resolve_path(path) {
                            Ok(path) => roots.iter().any(|root| path.starts_with(root)),
                            Err(_) => true,
                        });
                }
                // It did not: the tree it searched decides, and an upload
                // directory that is merely there has nothing to return.
                roots.retain(|root| holds_something(root));
                if roots.is_empty() {
                    return false;
                }
            }
            let named = match result.input.get(key) {
                Some(Value::String(path)) => facts.directory.join(path),
                // A search with no root searches the session's directory;
                // a read always names its file.
                None | Some(Value::Null) if matches!(class, Outside::SearchesPath(_)) => {
                    facts.directory.to_path_buf()
                }
                // Not the shape the tool takes: it cannot be told.
                _ => return true,
            };
            let Ok(path) = crate::protect::resolve_path(&named) else {
                return true;
            };
            roots.iter().any(|root| {
                path.starts_with(root)
                    // A search from above the area reaches into it.
                    || (matches!(class, Outside::SearchesPath(_)) && root.starts_with(&path))
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
            returned: None,
        }
    }

    fn result<'a>(tool: &'a str, input: &'a Value, metadata: &'a Value) -> ToolResult<'a> {
        ToolResult {
            call_id: None,
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
        for tool in ["bash", "edit", "write", "todowrite", "question"] {
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
                    returned: None,
                }
            ));
        }
    }

    /// A search is judged by the files the tool listed as returned; the
    /// tree it searched decides only when there is no complete list, and
    /// then only for an upload area that holds something.
    #[test]
    fn a_search_is_judged_by_the_files_it_returned_and_by_its_tree_only_without_a_list() {
        let temp = crate::storage::test_support::TempDir::new("outside-returned");
        let dir = crate::protect::resolve_path(temp.path()).unwrap();
        std::fs::create_dir_all(dir.join("uploads")).unwrap();
        std::fs::create_dir_all(dir.join("src")).unwrap();
        std::fs::write(dir.join("src/lib.rs"), "x").unwrap();
        let uploads = vec!["uploads".to_string()];
        let search = |tool: &str, input: Value, returned: Option<&[PathBuf]>, output: &str| {
            carries_outside_content(
                &ToolResult {
                    call_id: None,
                    tool,
                    input: &input,
                    metadata: None,
                    output,
                },
                &SessionFacts {
                    directory: &dir,
                    uploads: &uploads,
                    child_marked: &|| false,
                    returned,
                },
            )
        };
        let forged = format!("{}/uploads/a.txt:\n  Line 1: x", dir.display());
        let project = || json!({ "pattern": "x" });
        let in_src = [dir.join("src/lib.rs")];
        let in_uploads = [dir.join("src/lib.rs"), dir.join("uploads/a.txt")];

        // The directory is there and empty: nothing to return, list or no
        // list, whatever the tree and whatever the text.
        for tool in ["grep", "glob"] {
            assert!(!search(tool, project(), None, &forged), "{tool}");
            assert!(!search(tool, project(), Some(&in_src), &forged), "{tool}");
            assert!(!search(
                tool,
                json!({ "pattern": "x", "path": "uploads" }),
                None,
                ""
            ));
            assert!(!search(
                tool,
                json!({ "pattern": "x", "path": 7 }),
                None,
                ""
            ));
        }

        std::fs::write(dir.join("uploads/a.txt"), "x").unwrap();
        for tool in ["grep", "glob"] {
            // A complete list decides, in both directions, against the
            // tree and against the text.
            assert!(!search(tool, project(), Some(&in_src), &forged), "{tool}");
            assert!(!search(tool, project(), Some(&[]), &forged), "{tool}");
            assert!(search(tool, project(), Some(&in_uploads), ""), "{tool}");
            assert!(search(
                tool,
                json!({ "pattern": "x", "path": "src" }),
                Some(&in_uploads),
                ""
            ));
            // However the returned path is spelled.
            assert!(search(
                tool,
                project(),
                Some(&[dir.join("src/../uploads/a.txt")]),
                ""
            ));
            // No list (cut at the limit, or a failed call): the tree.
            assert!(search(tool, project(), None, ""), "{tool}");
            assert!(!search(
                tool,
                json!({ "pattern": "x", "path": "src" }),
                None,
                &forged
            ));
            assert!(search(tool, json!({ "pattern": "x", "path": 7 }), None, ""));
        }
        #[cfg(unix)]
        {
            // A returned file that is a link into the area is in the area;
            // one that cannot be resolved cannot be placed, and counts.
            std::os::unix::fs::symlink(dir.join("uploads/a.txt"), dir.join("src/link.txt"))
                .unwrap();
            assert!(search(
                "grep",
                project(),
                Some(&[dir.join("src/link.txt")]),
                ""
            ));
            std::os::unix::fs::symlink(dir.join("src/loop"), dir.join("src/loop")).unwrap();
            assert!(search("glob", project(), Some(&[dir.join("src/loop")]), ""));
        }
        // An upload area that is a file, not a directory, holds something.
        let odd = vec!["src/lib.rs".to_string()];
        assert!(carries_outside_content(
            &ToolResult {
                call_id: None,
                tool: "grep",
                input: &project(),
                metadata: None,
                output: "",
            },
            &SessionFacts {
                directory: &dir,
                uploads: &odd,
                child_marked: &|| false,
                returned: None,
            },
        ));
    }

    #[test]
    fn a_read_under_the_upload_area_is_outside_content_however_it_is_named() {
        let temp = crate::storage::test_support::TempDir::new("outside-uploads");
        let dir = crate::protect::resolve_path(temp.path()).unwrap();
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
        // A search is judged by the tree it searches, not by what it
        // printed: inside the area, the area itself, or a tree holding it.
        let search = |tool: &str, input: Value, output: &str| {
            carries_outside_content(
                &ToolResult {
                    call_id: None,
                    tool,
                    input: &input,
                    metadata: None,
                    output,
                },
                &facts(&dir, &uploads),
            )
        };
        for tool in ["grep", "glob"] {
            assert!(
                search(tool, json!({ "pattern": "x" }), ""),
                "{tool}: the project root"
            );
            assert!(search(
                tool,
                json!({ "pattern": "x", "path": "uploads" }),
                ""
            ));
            assert!(search(
                tool,
                json!({ "pattern": "x", "path": "uploads/in" }),
                ""
            ));
            assert!(search(
                tool,
                json!({ "pattern": "x", "path": "src/../uploads" }),
                ""
            ));
            assert!(
                search(tool, json!({ "pattern": "x", "path": 7 }), ""),
                "not a path"
            );
            // Elsewhere: not marked, whatever the output mentions.
            let forged = format!("{}/uploads/in/a.txt:\n  Line 1: x", dir.display());
            assert!(!search(
                tool,
                json!({ "pattern": "x", "path": "src" }),
                &forged
            ));
        }
        #[cfg(unix)]
        {
            std::os::unix::fs::symlink(dir.join("uploads"), dir.join("src/drop")).unwrap();
            assert!(search(
                "grep",
                json!({ "pattern": "x", "path": "src/drop" }),
                ""
            ));
        }
        // An upload area that cannot be resolved: the marking answer.
        let looping = vec!["loop".to_string()];
        #[cfg(unix)]
        {
            std::os::unix::fs::symlink(dir.join("loop"), dir.join("loop")).unwrap();
            assert!(carries_outside_content(
                &result("read", &json!({ "filePath": "src/lib.rs" }), &none),
                &facts(&dir, &looping),
            ));
        }
        let _ = looping;
    }
}
