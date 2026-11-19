//! BOM helpers — port of `util/bom.ts` (spec M4.3).

use std::path::Path;

use crate::tool::error::ToolError;

const BOM: char = '\u{feff}';

/// `Bom.split` (bom.ts:7-10): strips a leading U+FEFF.
pub fn split(text: &str) -> Split {
    if text.starts_with(BOM) {
        Split {
            bom: true,
            text: text[BOM.len_utf8()..].to_string(),
        }
    } else {
        Split {
            bom: false,
            text: text.to_string(),
        }
    }
}

/// `Bom.split` result (`{ bom, text }`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Split {
    pub bom: bool,
    pub text: String,
}

/// `Bom.join` (bom.ts:12-16): re-adds the BOM (stripping first).
pub fn join(text: &str, bom: bool) -> String {
    let stripped = split(text).text;
    if bom {
        format!("{BOM}{stripped}")
    } else {
        stripped
    }
}

/// `Bom.readFile` (bom.ts:18-20): reads the file (utf-8, lossy like
/// `TextDecoder`) and splits off the BOM.
pub async fn read_file(file_path: &Path) -> Result<Split, ToolError> {
    let bytes = tokio::fs::read(file_path)
        .await
        .map_err(|e| ToolError::Failed(e.to_string()))?;
    Ok(split(&String::from_utf8_lossy(bytes.as_slice())))
}

/// `writeWithDirs` (fs-util.ts:127): `create_dir_all(parent)` then write
/// (spec §2.5).
///
/// The one write of the `write` and `edit` tools: it goes through the
/// instance (`InstanceContext::write_file`), where a guarded session's
/// write is judged and made on the path that was judged.
pub async fn write_with_dirs(
    instance: &crate::tool::def::InstanceContext,
    file_path: &Path,
    contents: &str,
) -> Result<(), ToolError> {
    instance
        .write_file(file_path, contents.as_bytes())
        .await
        .map_err(|error| error.into_tool_error(|_, e| e.to_string(), |e| e.to_string()))
}

/// Replace the directory entry at `file_path` with a new file holding
/// `contents`, rather than truncating an inode that may have another
/// protected hard-link name. Callers pin symlink resolution. This is the
/// write of a session with no write guard (`InstanceContext::write_file`).
pub(crate) fn replace_file(file_path: &Path, contents: &[u8]) -> std::io::Result<()> {
    use std::io::Write;
    let parent = file_path.parent().unwrap_or(Path::new("."));
    std::fs::create_dir_all(parent)?;
    // Match normal file creation permissions, including the umask.
    let mut staged = tempfile::Builder::new().make_in(parent, |path| {
        std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create_new(true)
            .open(path)
    })?;
    match std::fs::metadata(file_path) {
        Ok(meta) => {
            if meta.permissions().readonly() {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::PermissionDenied,
                    "file is read-only",
                ));
            }
            staged.as_file().set_permissions(meta.permissions())?;
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => return Err(error),
    }
    staged.write_all(contents)?;
    staged.persist(file_path).map_err(|error| error.error)?;
    Ok(())
}

/// `Bom.syncFile` (bom.ts:22-27): re-writes the file when its BOM-ness
/// differs from `bom`; returns the current (post-format) text either way.
pub async fn sync_file(
    instance: &crate::tool::def::InstanceContext,
    file_path: &Path,
    bom: bool,
) -> Result<String, ToolError> {
    let current = read_file(file_path).await?;
    if current.bom == bom {
        return Ok(current.text);
    }
    write_with_dirs(instance, file_path, &join(&current.text, bom)).await?;
    Ok(current.text)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::storage::test_support::TempDir;

    #[test]
    fn split_strips_leading_bom_only() {
        let s = split("\u{feff}hello");
        assert!(s.bom);
        assert_eq!(s.text, "hello");
        let s = split("hello\u{feff}");
        assert!(!s.bom);
        assert_eq!(s.text, "hello\u{feff}");
        let s = split("");
        assert!(!s.bom);
        assert_eq!(s.text, "");
    }

    #[test]
    fn join_strips_then_adds() {
        assert_eq!(join("\u{feff}hi", true), "\u{feff}hi");
        assert_eq!(join("\u{feff}hi", false), "hi");
        assert_eq!(join("hi", true), "\u{feff}hi");
        assert_eq!(join("hi", false), "hi");
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn atomic_writes_preserve_normal_creation_and_existing_file_permissions() {
        use std::os::unix::fs::PermissionsExt;
        let temp = TempDir::new("bom-permissions");
        let normal = temp.path().join("normal");
        let written = temp.path().join("written");
        std::fs::write(&normal, "normal").unwrap();
        let instance = crate::tool::def::InstanceContext {
            directory: temp.path().to_path_buf(),
            worktree: temp.path().to_path_buf(),
            write_guard: None,
        };
        write_with_dirs(&instance, &written, "new").await.unwrap();
        assert_eq!(
            std::fs::metadata(&written).unwrap().permissions().mode(),
            std::fs::metadata(&normal).unwrap().permissions().mode(),
        );
        std::fs::set_permissions(&written, std::fs::Permissions::from_mode(0o750)).unwrap();
        write_with_dirs(&instance, &written, "replacement")
            .await
            .unwrap();
        assert_eq!(
            std::fs::metadata(&written).unwrap().permissions().mode() & 0o777,
            0o750
        );
    }

    #[tokio::test]
    async fn read_file_and_sync_file_round_trip() {
        let temp = TempDir::new("bom-round");
        let path = temp.path().join("f.txt");

        std::fs::write(&path, "\u{feff}content\n").unwrap();
        let read = read_file(&path).await.unwrap();
        assert!(read.bom);
        assert_eq!(read.text, "content\n");

        // BOM-ness differs -> re-written without BOM.
        let text = sync_file(
            &crate::tool::def::InstanceContext {
                directory: temp.path().to_path_buf(),
                worktree: temp.path().to_path_buf(),
                write_guard: None,
            },
            &path,
            false,
        )
        .await
        .unwrap();
        assert_eq!(text, "content\n");
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "content\n");

        // Already synced -> no write, text returned.
        let text = sync_file(
            &crate::tool::def::InstanceContext {
                directory: temp.path().to_path_buf(),
                worktree: temp.path().to_path_buf(),
                write_guard: None,
            },
            &path,
            true,
        )
        .await
        .unwrap();
        assert_eq!(text, "content\n");
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "\u{feff}content\n");
        std::fs::remove_dir_all(temp.path()).ok();
    }

    /// The window between the guard's check and the write is closed: the
    /// write is made on the path that was judged, through its parent's
    /// handle, so a rename or link swap made in between — here by a hook
    /// that runs exactly there — does not land it on a protected file. It
    /// used to be made by name, and followed whatever the name had become.
    #[cfg(unix)]
    #[tokio::test]
    async fn a_swap_between_the_check_and_the_write_does_not_redirect_it() {
        use crate::protect::fail_point::between_check_and_write;
        use std::os::unix::fs::symlink;
        let temp = TempDir::new("bom-swap");
        let root = temp.path().canonicalize().unwrap();
        for dir in ["work", "trash", ".opencode/agent"] {
            std::fs::create_dir_all(root.join(dir)).unwrap();
        }
        std::fs::write(root.join(".opencode/agent/x.md"), "the operator's").unwrap();
        std::fs::write(root.join("AGENTS.md"), "the operator's").unwrap();
        std::fs::write(root.join("trash/x.md"), "to delete").unwrap();
        let guarded = crate::tool::def::InstanceContext {
            directory: root.clone(),
            worktree: root.clone(),
            write_guard: Some(std::sync::Arc::new(crate::protect::WriteGuard {
                protected: Default::default(),
                policy: Default::default(),
                worktree: root.clone(),
            })),
        };
        let agent_dir = root.join(".opencode/agent");

        // A directory on the way replaced by a link into `.opencode`.
        let (at, link_to, moved) = (root.join("work"), agent_dir.clone(), root.join("work-was"));
        let armed = between_check_and_write(&root.join("work"), move |_| {
            std::fs::rename(&at, &moved).unwrap();
            symlink(&link_to, &at).unwrap();
        });
        let refused = write_with_dirs(&guarded, &root.join("work/x.md"), "mine").await;
        assert!(refused.is_err(), "{refused:?}");
        drop(armed);
        assert!(root.join("work").is_symlink(), "the swap happened");
        assert_eq!(
            std::fs::read_to_string(agent_dir.join("x.md")).unwrap(),
            "the operator's"
        );

        // The file itself replaced by a link to a loader file.
        let (at, link_to) = (root.join("notes.md"), root.join("AGENTS.md"));
        let armed = between_check_and_write(&root.join("notes.md"), move |_| {
            symlink(&link_to, &at).unwrap();
        });
        let refused = write_with_dirs(&guarded, &root.join("notes.md"), "mine").await;
        assert!(refused.is_err(), "{refused:?}");
        drop(armed);
        assert_eq!(
            std::fs::read_to_string(root.join("AGENTS.md")).unwrap(),
            "the operator's"
        );

        // A removal: the directory swapped for the protected one.
        let (at, link_to, moved) = (
            root.join("trash"),
            agent_dir.clone(),
            root.join("trash-was"),
        );
        let armed = between_check_and_write(&root.join("trash"), move |_| {
            std::fs::rename(&at, &moved).unwrap();
            symlink(&link_to, &at).unwrap();
        });
        let refused = guarded.remove_file(&root.join("trash/x.md")).await;
        assert!(refused.is_err(), "{refused:?}");
        drop(armed);
        assert!(agent_dir.join("x.md").exists());

        // With nothing in between the same writes go through…
        write_with_dirs(&guarded, &root.join("fresh/dir/a.md"), "mine")
            .await
            .unwrap();
        assert_eq!(
            std::fs::read_to_string(root.join("fresh/dir/a.md")).unwrap(),
            "mine"
        );
        guarded
            .remove_file(&root.join("fresh/dir/a.md"))
            .await
            .unwrap();
        assert!(!root.join("fresh/dir/a.md").exists());
        // …and a session without a guard writes by name, as it always did:
        // through the link that now stands at `work`.
        let unguarded = crate::tool::def::InstanceContext {
            write_guard: None,
            ..guarded.clone()
        };
        write_with_dirs(&unguarded, &root.join("work/y.md"), "by name")
            .await
            .unwrap();
        assert_eq!(
            std::fs::read_to_string(agent_dir.join("y.md")).unwrap(),
            "by name"
        );
        std::fs::remove_dir_all(temp.path()).ok();
    }
}
