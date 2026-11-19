//! File mutations authorize both the caller's spelling and the resolved path.

use std::path::{Path, PathBuf};

use super::error::ToolError;
use super::external_directory::real_path;
use super::ripgrep::ts_relative;

pub(crate) fn edit_patterns<'a>(
    worktree: &Path,
    paths: impl IntoIterator<Item = &'a Path>,
) -> Vec<String> {
    let mut patterns = Vec::new();
    let canonical_root = real_path(worktree);
    for path in paths {
        let root = if !path.starts_with(worktree)
            && (path.starts_with(&canonical_root)
                || (cfg!(windows)
                    && path.components().next() == canonical_root.components().next()))
        {
            canonical_root.as_path()
        } else {
            worktree
        };
        let pattern = ts_relative(root, path);
        if !patterns.contains(&pattern) {
            patterns.push(pattern);
        }
    }
    patterns
}

pub(crate) fn verify(requested: &Path, resolved: &Path) -> Result<(), ToolError> {
    if real_path(requested) != resolved || real_path(resolved) != resolved {
        return Err(ToolError::Failed(format!(
            "File path changed while awaiting edit permission: {}",
            requested.display()
        )));
    }
    Ok(())
}

pub(crate) fn reject_move_aliases(source: &Path, destination: &Path) -> Result<(), ToolError> {
    for path in [source, destination] {
        match std::fs::symlink_metadata(path) {
            Ok(meta) if meta.file_type().is_symlink() => {
                return Err(ToolError::Failed(format!(
                    "Patch moves cannot use a symlink endpoint: {}",
                    path.display()
                )));
            }
            Ok(_) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(ToolError::Failed(error.to_string())),
        }
    }
    let source = real_path(source);
    let destination = real_path(destination);
    let same = source == destination
        || match same_file::is_same_file(&source, &destination) {
            Ok(same) => same,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => false,
            Err(error) => return Err(ToolError::Failed(error.to_string())),
        };
    if same {
        return Err(ToolError::Failed(
            "Patch move source and destination identify the same file".into(),
        ));
    }
    Ok(())
}

pub(crate) fn resolve(path: &Path) -> PathBuf {
    real_path(path)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tool::def::Extra;
    use crate::tool::ripgrep::test_support::{ctx, fixed_agents, instance, RecordingAsk};
    use crate::tool::truncate::TruncateService;
    use crate::{EventBus, PermissionService, SessionAsk, Storage, ToolDef};
    use serde_json::{json, Value};
    use std::sync::Arc;

    fn tool(name: &str, root: &Path) -> ToolDef {
        let truncate = Arc::new(TruncateService::default_limits(root.join("output")));
        match name {
            "write" => crate::tool::write::write_tool(truncate, fixed_agents(), None, None, None),
            "edit" => crate::tool::edit::edit_tool(truncate, fixed_agents(), None, None, None),
            _ => crate::tool::apply_patch::apply_patch_tool(
                truncate,
                fixed_agents(),
                None,
                None,
                None,
            ),
        }
    }

    async fn execute(
        root: &Path,
        name: &str,
        args: Value,
        denied: &str,
    ) -> Result<crate::ExecuteResult, ToolError> {
        let events = Arc::new(EventBus::new(Storage::open_in_memory().unwrap(), None));
        let ask = SessionAsk::new(
            Arc::new(PermissionService::new(events)),
            "session",
            "message",
            "call",
            serde_json::from_value(json!([
                {"permission":"edit", "pattern":"*", "action":"allow"},
                {"permission":"edit", "pattern":denied, "action":"deny"}
            ]))
            .unwrap(),
        );
        let recorder = RecordingAsk::new();
        let instance = instance(root);
        let extra = Extra::default();
        let mut context = ctx(&recorder, &instance, &extra);
        context.ask = &ask;
        (tool(name, root).execute)(args, context).await
    }

    fn arguments(name: &str, path: &str) -> Value {
        match name {
            "write" => json!({"filePath":path,"content":"replacement\n"}),
            "edit" => json!({"filePath":path,"oldString":"protected","newString":"replacement"}),
            _ => {
                json!({"patchText":format!("*** Begin Patch\n*** Update File: {path}\n@@\n-protected\n+replacement\n*** End Patch")})
            }
        }
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn every_edit_tool_authorizes_resolved_symlink_targets() {
        for name in ["write", "edit", "apply_patch"] {
            for parent_alias in [true, false] {
                let root = tempfile::tempdir().unwrap();
                std::fs::create_dir(root.path().join("real")).unwrap();
                let target = root.path().join("real/protected.txt");
                std::fs::write(&target, "protected\n").unwrap();
                let alias = if parent_alias {
                    std::os::unix::fs::symlink("real", root.path().join("shortcut")).unwrap();
                    "shortcut/protected.txt"
                } else {
                    std::os::unix::fs::symlink("real/protected.txt", root.path().join("alias.txt"))
                        .unwrap();
                    "alias.txt"
                };
                assert!(
                    execute(
                        root.path(),
                        name,
                        arguments(name, alias),
                        "real/protected.txt"
                    )
                    .await
                    .is_err(),
                    "{name} {alias}"
                );
                assert_eq!(std::fs::read_to_string(&target).unwrap(), "protected\n");
                execute(root.path(), name, arguments(name, alias), "unrelated.txt")
                    .await
                    .unwrap();
                assert_eq!(std::fs::read_to_string(&target).unwrap(), "replacement\n");
                assert!(std::fs::symlink_metadata(root.path().join(if parent_alias {
                    "shortcut"
                } else {
                    "alias.txt"
                }))
                .unwrap()
                .file_type()
                .is_symlink());
            }
        }
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn symlink_move_destinations_fail_before_any_file_changes() {
        for target in ["source.txt", "protected.txt"] {
            let root = tempfile::tempdir().unwrap();
            std::fs::write(root.path().join("source.txt"), "source\n").unwrap();
            std::fs::write(root.path().join("protected.txt"), "protected\n").unwrap();
            std::os::unix::fs::symlink(target, root.path().join("alias.txt")).unwrap();
            let args = json!({"patchText":"*** Begin Patch\n*** Add File: new.txt\n+new\n*** Update File: source.txt\n*** Move to: alias.txt\n@@\n-source\n+replacement\n*** End Patch"});
            assert!(execute(root.path(), "apply_patch", args, "protected.txt")
                .await
                .is_err());
            assert_eq!(
                std::fs::read_to_string(root.path().join("source.txt")).unwrap(),
                "source\n"
            );
            assert_eq!(
                std::fs::read_to_string(root.path().join("protected.txt")).unwrap(),
                "protected\n"
            );
            assert!(!root.path().join("new.txt").exists());
            assert_eq!(
                std::fs::read_link(root.path().join("alias.txt")).unwrap(),
                Path::new(target)
            );
        }
    }

    #[tokio::test]
    async fn hard_link_moves_are_rejected_and_edits_replace_only_the_allowed_entry() {
        let root = tempfile::tempdir().unwrap();
        std::fs::write(root.path().join("source.txt"), "protected\n").unwrap();
        std::fs::hard_link(
            root.path().join("source.txt"),
            root.path().join("alias.txt"),
        )
        .unwrap();
        let args = json!({"patchText":"*** Begin Patch\n*** Update File: source.txt\n*** Move to: alias.txt\n@@\n-protected\n+replacement\n*** End Patch"});
        assert!(execute(root.path(), "apply_patch", args, "unrelated.txt")
            .await
            .is_err());
        assert_eq!(
            std::fs::read_to_string(root.path().join("source.txt")).unwrap(),
            "protected\n"
        );
        for name in ["write", "edit", "apply_patch"] {
            let root = tempfile::tempdir().unwrap();
            std::fs::write(root.path().join("protected.txt"), "protected\n").unwrap();
            std::fs::hard_link(
                root.path().join("protected.txt"),
                root.path().join("alias.txt"),
            )
            .unwrap();
            execute(
                root.path(),
                name,
                arguments(name, "alias.txt"),
                "protected.txt",
            )
            .await
            .unwrap();
            assert_eq!(
                std::fs::read_to_string(root.path().join("protected.txt")).unwrap(),
                "protected\n"
            );
            assert_eq!(
                std::fs::read_to_string(root.path().join("alias.txt")).unwrap(),
                "replacement\n"
            );
        }
    }
}
