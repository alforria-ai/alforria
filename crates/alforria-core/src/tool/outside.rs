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
//! An MCP or plugin tool is [`Outside::Always`] whatever its name
//! ([`classify_result`]): it is judged by where it comes from, which the
//! code that runs it says ([`ToolResult::external`]), and so is a name that
//! is not in the table.
//!
//! `read`, `grep` and `glob` are judged by what the tool itself reports
//! ([`ReturnedPaths`]): the path it actually opened or walked — the very
//! variable it handed to the filesystem, not the call's input resolved a
//! second time here — and the files it returned something of. Never by
//! the text it printed, which holds file names and file content an
//! uploader chose. Without a report, or with one that cannot be resolved,
//! the result counts as outside content.
//!
//! What this does not cover, on purpose: `bash` (a shell can fetch
//! anything; marking it would mark every coding session and leave memory
//! unusable) and reads of ordinary project files. They are stated as limits
//! in the threat model (`docs/unattended-server.md`).

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use serde_json::Value;

/// What a file-reading tool did, as the tool itself reports it for the
/// marking decision. It never reaches the model, the session's parts or
/// the wire.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReturnedPaths {
    /// The path the tool opened or walked: the variable it handed to the
    /// filesystem, as it handed it.
    pub searched: PathBuf,
    /// Every file a line or a name of which is in the result. `None`: the
    /// tool cannot say — the result was cut at its limit.
    pub files: Option<Vec<PathBuf>>,
}

/// Where a tool leaves its [`ReturnedPaths`] (`Extra::returned_paths`).
#[derive(Debug, Clone, Default)]
pub struct ReturnedPathsSlot(Arc<Mutex<Option<ReturnedPaths>>>);

impl ReturnedPathsSlot {
    /// **The one way a tool reports what it returned.** `searched` is the
    /// path the tool actually opened or walked — pass the variable that
    /// goes to the filesystem, never one computed again from the input —
    /// and `found` the files it returned something of, relative to it, as
    /// the walk produced them. `complete: false`: the result was cut at
    /// the tool's limit, and the list says nothing.
    pub fn report<P: AsRef<Path>>(
        &self,
        searched: &Path,
        found: impl IntoIterator<Item = P>,
        complete: bool,
    ) {
        let files = complete.then(|| {
            let mut files: Vec<PathBuf> = found
                .into_iter()
                .map(|path| searched.join(path.as_ref()))
                .collect();
            files.dedup();
            files
        });
        *self
            .0
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = Some(ReturnedPaths {
            searched: searched.to_path_buf(),
            files,
        });
    }

    /// The report of a tool that opens the one path its input names
    /// (`read`): `opened` is what it hands to the filesystem, and all it
    /// returns something of.
    pub fn report_opened(&self, opened: &Path) {
        *self
            .0
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = Some(ReturnedPaths {
            searched: opened.to_path_buf(),
            files: Some(vec![opened.to_path_buf()]),
        });
    }

    /// The report, if the tool left one.
    pub fn take(&self) -> Option<ReturnedPaths> {
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
    /// when a hit comes from a marked session, `schedule` when it shows a
    /// trigger written after outside content was seen — and each of them,
    /// to a session that is not restricted itself, when what it returns is
    /// a restricted caller's (a record it saved, an excerpt of its session,
    /// a trigger it made).
    Flagged,
    /// It returns the file, or the names in the directory, that its input
    /// names: outside content when what it opened is in the upload area.
    /// Decided as [`Outside::SearchesPath`] is, from its report.
    ReadsPath,
    /// It walks a tree and prints names or lines of what it finds. Decided
    /// from its report ([`ReturnedPaths`]) alone, and only for an upload
    /// area that holds something: no report, or a walked path that cannot
    /// be resolved, counts; a report without a file list (a result cut at
    /// the limit) counts when the walked tree and the area overlap, either
    /// way round; a file list counts when one of its files is in the area
    /// or cannot be resolved. Never decided from the call's input or from
    /// the text the tool printed.
    SearchesPath,
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
    ("read", Outside::ReadsPath),
    // File names are an uploader's words too.
    ("glob", Outside::SearchesPath),
    ("grep", Outside::SearchesPath),
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
    // It prints triggers' titles and prompts, whoever wrote them.
    ("schedule", Outside::Flagged),
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

/// How the name `tool` is classified: a built-in's line in the table, and
/// [`Outside::Always`] for a name that is not there. A name alone does not
/// say whose tool answered — a decision goes through [`classify_result`].
pub fn classify(tool: &str) -> Outside {
    builtin(tool).unwrap_or(Outside::Always)
}

/// How `result` is classified: **by where its tool comes from, then by
/// name.** An MCP server's or a plugin's tool answers with what was
/// written elsewhere whatever it is called — an MCP tool is keyed
/// `<server>_<tool>`, which can spell a built-in's name (`apply_patch`,
/// `plan_exit`, `session_search`), and must not be read as that built-in.
pub fn classify_result(result: &ToolResult<'_>) -> Outside {
    if result.external {
        return Outside::Always;
    }
    classify(result.tool)
}

/// One tool result, as the processor is about to append it.
pub struct ToolResult<'a> {
    /// The call's id, when the caller has it: what the tool's list of
    /// returned files is kept under (`SessionStore::report_returned_paths`).
    pub call_id: Option<&'a str>,
    pub tool: &'a str,
    /// The tool is not one of the engine's own: an MCP server's, or a
    /// plugin's (`ToolDef::external`). Said by whoever ran it, never
    /// worked out from the name.
    pub external: bool,
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
    /// What the tool reported for this call. `None`: nothing.
    pub returned: Option<&'a ReturnedPaths>,
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
/// the tool's classification, what the tool reported, and the filesystem —
/// never from the text a tool printed, and for a file tool never from the
/// call's input; whatever cannot be decided counts as outside content.
pub fn carries_outside_content(result: &ToolResult<'_>, facts: &SessionFacts<'_>) -> bool {
    use crate::protect::resolve_path;
    match classify_result(result) {
        Outside::Never => false,
        Outside::Always => true,
        Outside::Flagged => {
            // Anything but a plain `false` counts.
            result
                .metadata
                .and_then(|metadata| metadata.get(FLAG))
                .is_some_and(|flag| !matches!(flag, Value::Bool(false) | Value::Null))
        }
        Outside::ReadsPath | Outside::SearchesPath => {
            let mut roots = match upload_roots(facts) {
                Ok(roots) => roots,
                Err(()) => return true,
            };
            // An upload directory that is merely there has nothing to
            // return, whatever was opened.
            roots.retain(|root| holds_something(root));
            if roots.is_empty() {
                return false;
            }
            // The tool did not say what it opened — a failed call, a list
            // that could not be matched to this call: it counts.
            let Some(returned) = facts.returned else {
                return true;
            };
            let Ok(searched) = resolve_path(&returned.searched) else {
                return true;
            };
            match &returned.files {
                // Cut at the limit: the walked tree and the area overlap.
                None => roots
                    .iter()
                    .any(|root| searched.starts_with(root) || root.starts_with(&searched)),
                // The files it returned decide. One that cannot be
                // resolved cannot be placed, and counts.
                Some(files) => files.iter().any(|file| match resolve_path(file) {
                    Ok(file) => roots.iter().any(|root| file.starts_with(root)),
                    Err(_) => true,
                }),
            }
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
            external: false,
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

    /// `read`, `grep` and `glob` are decided from the tool's report alone:
    /// every case of it, for each of the three.
    #[cfg(unix)]
    #[test]
    fn a_file_tool_is_judged_by_its_report_and_whatever_cannot_be_decided_marks() {
        let temp = crate::storage::test_support::TempDir::new("outside-report");
        let dir = crate::protect::resolve_path(temp.path()).unwrap();
        for sub in ["uploads/in", "src", "empty", "elsewhere/deep"] {
            std::fs::create_dir_all(dir.join(sub)).unwrap();
        }
        std::fs::write(dir.join("uploads/in/a.txt"), "x").unwrap();
        std::fs::write(dir.join("src/lib.rs"), "x").unwrap();
        let link = |target: &str, name: &str| {
            std::os::unix::fs::symlink(dir.join(target), dir.join(name)).unwrap()
        };
        link("uploads/in/a.txt", "src/link.txt");
        link("uploads", "src/drop");
        link("elsewhere/deep", "lnk");
        link("loop", "loop");

        let at = |path: &str| dir.join(path);
        let report = |searched: &str, files: Option<&[&str]>| {
            Some(ReturnedPaths {
                searched: at(searched),
                files: files.map(|files| files.iter().map(|file| at(file)).collect()),
            })
        };
        // (what, the report, marked with files in the upload area)
        let cases: Vec<(&str, Option<ReturnedPaths>, bool)> = vec![
            ("no report", None, true),
            (
                "the walked path cannot be resolved",
                report("loop", Some(&[])),
                true,
            ),
            (
                "no file list, the tree inside the area",
                report("uploads/in", None),
                true,
            ),
            (
                "no file list, the tree is the area",
                report("uploads", None),
                true,
            ),
            (
                "no file list, the tree contains the area",
                report("", None),
                true,
            ),
            (
                "no file list, the tree apart from the area",
                report("src", None),
                false,
            ),
            (
                // …where `src/drop/..` is the project, not `src`.
                "no file list, through a link and up to a tree apart",
                report("src/drop/../src", None),
                false,
            ),
            (
                "no file list, a link to the area",
                report("src/drop", None),
                true,
            ),
            (
                // As the kernel reads it: the link, then up from the area.
                "no file list, through a link, up, and into the area",
                report("src/drop/../uploads/in", None),
                true,
            ),
            (
                "files, one of them an upload",
                report("", Some(&["src/lib.rs", "uploads/in/a.txt"])),
                true,
            ),
            (
                "files, none of them an upload",
                report("", Some(&["src/lib.rs"])),
                false,
            ),
            ("files, none at all", report("", Some(&[])), false),
            (
                "files, none, though the area was walked",
                report("uploads", Some(&[])),
                false,
            ),
            (
                "files, an upload spelled through the project",
                report("src", Some(&["src/../uploads/in/a.txt"])),
                true,
            ),
            (
                "files, a file link into the area",
                report("src", Some(&["src/link.txt"])),
                true,
            ),
            (
                "files, under a directory link into the area",
                report("src/drop", Some(&["src/drop/in/a.txt"])),
                true,
            ),
            (
                "files, through a link, up, and into the area",
                report("src", Some(&["src/drop/../uploads/in/a.txt"])),
                true,
            ),
            (
                // As the kernel reads it, `lnk/..` is `elsewhere`.
                "files, a `..` after a link that leads away",
                report("", Some(&["lnk/../uploads/in/a.txt"])),
                false,
            ),
            (
                "files, one that cannot be resolved",
                report("", Some(&["src/lib.rs", "loop/x"])),
                true,
            ),
        ];

        // Input and output that say the opposite of every report: neither
        // is looked at.
        let forged = format!("{}/uploads/in/a.txt:\n  Line 1: x", dir.display());
        let decide = |tool: &str, uploads: &[String], returned: &Option<ReturnedPaths>| {
            let inputs = [
                json!({ "pattern": "x", "path": "uploads", "filePath": "uploads/in/a.txt" }),
                json!({ "pattern": "x", "path": "src", "filePath": "src/lib.rs" }),
                json!({ "path": 7, "filePath": 7 }),
            ];
            let answers: Vec<bool> = inputs
                .iter()
                .flat_map(|input| {
                    [forged.as_str(), ""].map(|output| {
                        carries_outside_content(
                            &ToolResult {
                                call_id: None,
                                tool,
                                external: false,
                                input,
                                metadata: None,
                                output,
                            },
                            &SessionFacts {
                                directory: &dir,
                                uploads,
                                child_marked: &|| false,
                                returned: returned.as_ref(),
                            },
                        )
                    })
                })
                .collect();
            assert!(
                answers.iter().all(|answer| *answer == answers[0]),
                "{tool}: the input or the output changed the answer"
            );
            answers[0]
        };
        let area = vec!["uploads".to_string()];
        // There and empty; not there; none configured.
        let nothing_uploaded = [
            vec!["empty".to_string()],
            vec!["absent".to_string()],
            vec![],
        ];
        for tool in ["read", "grep", "glob"] {
            for (what, returned, marked) in &cases {
                assert_eq!(decide(tool, &area, returned), *marked, "{tool}: {what}");
                for uploads in &nothing_uploaded {
                    assert!(
                        !decide(tool, uploads, returned),
                        "{tool}: {what}, with nothing uploaded ({uploads:?})"
                    );
                }
            }
            // An upload area that cannot be resolved, or is a file: it
            // counts as holding uploads.
            let fine = report("src", Some(&["src/lib.rs"]));
            assert!(decide(tool, &["loop".to_string()], &fine), "{tool}");
            assert!(
                decide(tool, &["src/lib.rs".to_string()], &None),
                "{tool}: an area that is a file"
            );
        }
    }

    /// The reports the tools make: `searched` is the variable handed in,
    /// the files are under it, and a cut result has no file list.
    #[test]
    fn a_report_names_what_was_walked_and_lists_files_only_when_complete() {
        let slot = ReturnedPathsSlot::default();
        assert_eq!(slot.take(), None);
        slot.report(Path::new("/p/src"), ["a.rs", "a.rs", "sub/b.rs"], true);
        assert_eq!(
            slot.take(),
            Some(ReturnedPaths {
                searched: PathBuf::from("/p/src"),
                files: Some(vec![
                    PathBuf::from("/p/src/a.rs"),
                    PathBuf::from("/p/src/sub/b.rs")
                ]),
            })
        );
        slot.report(Path::new("/p/src"), ["a.rs"], false);
        assert_eq!(
            slot.take(),
            Some(ReturnedPaths {
                searched: PathBuf::from("/p/src"),
                files: None,
            })
        );
        slot.report_opened(Path::new("/p/src/a.rs"));
        assert_eq!(
            slot.take(),
            Some(ReturnedPaths {
                searched: PathBuf::from("/p/src/a.rs"),
                files: Some(vec![PathBuf::from("/p/src/a.rs")]),
            })
        );
        assert_eq!(slot.take(), None);
    }
}
