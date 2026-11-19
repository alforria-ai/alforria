//! Apply fully staged recovery contents with reversible filesystem changes.

use std::collections::HashSet;
use std::fs;
use std::io;
use std::path::{Component, Path, PathBuf};

use tempfile::TempDir;

use crate::CoreError;

pub(super) struct Change {
    pub target: PathBuf,
    pub source: Option<PathBuf>,
}

pub(super) const CHECKPOINT_PREFIX: &str = ".alforria-recovery-";

#[derive(Clone, Copy, PartialEq, Eq)]
enum Target {
    Missing,
    File,
    Directory,
    Repository,
}

struct Prepared {
    target: PathBuf,
    area: TempDir,
    before: Target,
    removed_children: HashSet<PathBuf>,
    replacement: bool,
    backed_up: bool,
    installed: bool,
}

#[derive(Default)]
struct CreatedDirectories(Vec<PathBuf>);

impl Drop for CreatedDirectories {
    fn drop(&mut self) {
        for directory in self.0.iter().rev() {
            // Remove only empty directories created by this operation.
            let _ = fs::remove_dir(directory);
        }
    }
}

fn relative_path<'a>(root: &Path, target: &'a Path) -> io::Result<&'a Path> {
    let relative = target.strip_prefix(root).map_err(io::Error::other)?;
    if relative.as_os_str().is_empty()
        || relative
            .components()
            .any(|part| !matches!(part, Component::Normal(name) if !name.eq_ignore_ascii_case(std::ffi::OsStr::new(".git"))))
    {
        return Err(io::Error::other("invalid snapshot recovery path"));
    }
    Ok(relative)
}

fn target_kind(path: &Path) -> io::Result<Target> {
    match fs::symlink_metadata(path) {
        Ok(meta) if meta.is_dir() && !meta.file_type().is_symlink() => {
            match fs::symlink_metadata(path.join(".git")) {
                Ok(_) => Ok(Target::Repository),
                Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(Target::Directory),
                Err(error) => Err(error),
            }
        }
        Ok(_) => Ok(Target::File),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(Target::Missing),
        Err(error) => Err(error),
    }
}

fn inspect(root: &Path, target: &Path) -> io::Result<Target> {
    let relative = relative_path(root, target)?;
    let mut parent = root.to_path_buf();
    for part in relative.parent().unwrap_or(Path::new("")).components() {
        parent.push(part);
        match target_kind(&parent)? {
            Target::File => {
                return Err(io::Error::other(format!(
                    "recovery parent is not a plain directory: {}",
                    parent.display()
                )));
            }
            Target::Missing => return Ok(Target::Missing),
            Target::Repository => return Ok(Target::Repository),
            Target::Directory => {}
        }
    }
    target_kind(target)
}

fn create_parents(root: &Path, parent: &Path, created: &mut CreatedDirectories) -> io::Result<()> {
    let mut path = root.to_path_buf();
    for part in parent
        .strip_prefix(root)
        .map_err(io::Error::other)?
        .components()
    {
        path.push(part);
        match fs::create_dir(&path) {
            Ok(()) => created.0.push(path.clone()),
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {
                let meta = fs::symlink_metadata(&path)?;
                if !meta.is_dir() || meta.file_type().is_symlink() {
                    return Err(io::Error::other(
                        "recovery parent changed during preparation",
                    ));
                }
            }
            Err(error) => return Err(error),
        }
    }
    Ok(())
}

fn copy_entry(source: &Path, destination: &Path) -> io::Result<()> {
    let meta = fs::symlink_metadata(source)?;
    if meta.file_type().is_symlink() {
        let target = fs::read_link(source)?;
        #[cfg(unix)]
        std::os::unix::fs::symlink(target, destination)?;
        #[cfg(windows)]
        std::os::windows::fs::symlink_file(target, destination)?;
    } else if meta.is_file() {
        fs::copy(source, destination)?;
    } else {
        return Err(io::Error::other("unsupported snapshot entry"));
    }
    Ok(())
}

pub(super) fn apply(root: &Path, changes: Vec<Change>) -> Result<(), CoreError> {
    apply_with(root, changes, |from, to| fs::rename(from, to))
}

/// Replacing a directory is safe only if the patch accounts for everything
/// removed from it. Walk without following symlinks and keep unrelated work.
fn check_displaced_directory(directory: &Path, removed: &HashSet<PathBuf>) -> io::Result<()> {
    for entry in walkdir::WalkDir::new(directory)
        .min_depth(1)
        .follow_links(false)
    {
        let entry = entry.map_err(io::Error::other)?;
        if entry.file_type().is_dir() {
            continue;
        }
        let relative = entry
            .path()
            .strip_prefix(directory)
            .expect("entry below directory");
        if !removed.contains(relative) {
            return Err(io::Error::other(format!(
                "recovery directory contains an unrelated path: {}",
                entry.path().display()
            )));
        }
    }
    Ok(())
}

fn prepare(
    root: &Path,
    mut changes: &[Change],
    created: &mut CreatedDirectories,
) -> io::Result<Vec<Prepared>> {
    let mut prepared = Vec::new();
    while let Some((change, tail)) = changes.split_first() {
        let descendants = tail.partition_point(|child| child.target.starts_with(&change.target));
        let before = inspect(root, &change.target)?;
        // An already-present directory needs no action of its own. Its
        // children can be handled individually, leaving other entries alone.
        if before == Target::Directory && change.source.is_none() {
            changes = tail;
            continue;
        }
        let (children, rest) = tail.split_at(descendants);
        changes = rest;
        if before == Target::Repository {
            continue;
        }
        let directory_replacement =
            change.source.is_none() && children.iter().any(|child| child.source.is_some());
        let replacement = directory_replacement || change.source.is_some();
        if !replacement && before == Target::Missing {
            continue;
        }
        let removed_children = if before == Target::Directory {
            let removed = children
                .iter()
                .map(|child| {
                    child
                        .target
                        .strip_prefix(&change.target)
                        .unwrap()
                        .to_path_buf()
                })
                .collect();
            check_displaced_directory(&change.target, &removed)?;
            removed
        } else {
            HashSet::new()
        };

        let parent = change.target.parent().expect("validated recovery path");
        create_parents(root, parent, created)?;
        let area = tempfile::Builder::new()
            .prefix(CHECKPOINT_PREFIX)
            .tempdir_in(parent)?;
        fs::write(
            area.path().join("target.json"),
            serde_json::to_vec(&change.target).map_err(io::Error::other)?,
        )?;
        let staged = area.path().join("replacement");
        if let Some(source) = &change.source {
            copy_entry(source, &staged)?;
        } else if directory_replacement {
            // Stage the complete replacement subtree before removing the
            // file blocking it. The validated replacement paths are disjoint.
            for child in children {
                if let Some(source) = &child.source {
                    let destination =
                        staged.join(child.target.strip_prefix(&change.target).unwrap());
                    fs::create_dir_all(destination.parent().unwrap())?;
                    copy_entry(source, &destination)?;
                }
            }
        }
        prepared.push(Prepared {
            target: change.target.clone(),
            area,
            before,
            removed_children,
            replacement,
            backed_up: false,
            installed: false,
        });
    }
    Ok(prepared)
}

fn apply_with(
    root: &Path,
    mut changes: Vec<Change>,
    rename: impl Fn(&Path, &Path) -> io::Result<()>,
) -> Result<(), CoreError> {
    changes.sort_unstable_by(|a, b| a.target.components().cmp(b.target.components()));
    let mut previous_file: Option<&Path> = None;
    for change in &changes {
        relative_path(root, &change.target)?;
        if change.source.is_some() {
            if previous_file.is_some_and(|parent| change.target.starts_with(parent)) {
                return Err(CoreError::Storage("conflicting snapshot contents".into()));
            }
            previous_file = Some(&change.target);
        }
    }
    let mut created = CreatedDirectories::default();
    let mut prepared = prepare(root, &changes, &mut created)?;

    let result = (|| -> io::Result<()> {
        for change in &mut prepared {
            if inspect(root, &change.target)? != change.before {
                return Err(io::Error::other(
                    "recovery target changed during preparation",
                ));
            }
            if change.before != Target::Missing {
                let backup = change.area.path().join("backup");
                rename(&change.target, &backup)?;
                change.backed_up = true;
                if change.before == Target::Directory {
                    // Check the actual directory we would discard, even if
                    // its contents changed during staging.
                    check_displaced_directory(&backup, &change.removed_children)?;
                }
            }
            if change.replacement {
                rename(&change.area.path().join("replacement"), &change.target)?;
                change.installed = true;
            }
        }
        Ok(())
    })();
    if let Err(error) = result {
        let mut rollback_errors = Vec::new();
        for change in prepared.iter_mut().rev() {
            if !change.installed && !change.backed_up {
                continue;
            }
            let rollback = (|| -> io::Result<()> {
                if inspect(root, &change.target)? == Target::Repository {
                    return Err(io::Error::other(
                        "nested repository appeared during recovery",
                    ));
                }
                if change.installed {
                    rename(&change.target, &change.area.path().join("replacement"))?;
                    change.installed = false;
                }
                if change.backed_up {
                    if fs::symlink_metadata(&change.target).is_ok() {
                        return Err(io::Error::other(
                            "recovery target was recreated during rollback",
                        ));
                    }
                    rename(&change.area.path().join("backup"), &change.target)?;
                    change.backed_up = false;
                }
                Ok(())
            })();
            if let Err(rollback) = rollback {
                rollback_errors.push(rollback.to_string());
            }
        }
        if !rollback_errors.is_empty() {
            let checkpoints: Vec<_> = prepared
                .into_iter()
                .map(|change| change.area.keep())
                .collect();
            return Err(CoreError::Storage(format!("snapshot recovery failed: {error}; rollback incomplete: {}; backups retained at {checkpoints:?}", rollback_errors.join("; "))));
        }
        return Err(CoreError::Storage(format!(
            "snapshot recovery failed; original files restored: {error}"
        )));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::Cell;

    #[test]
    fn replacement_failure_rolls_back_files_and_deletions() {
        let root = tempfile::tempdir().unwrap();
        let sources = tempfile::tempdir().unwrap();
        for name in ["a", "b", "c"] {
            fs::write(root.path().join(name), format!("current {name}")).unwrap();
            fs::write(sources.path().join(name), format!("snapshot {name}")).unwrap();
        }
        let changes = ["a", "b", "c"]
            .into_iter()
            .map(|name| Change {
                target: root.path().join(name),
                source: (name != "b").then(|| sources.path().join(name)),
            })
            .collect();
        let failed = Cell::new(false);
        let result = apply_with(root.path(), changes, |from, to| {
            if to == root.path().join("c")
                && from.file_name() == Some(std::ffi::OsStr::new("replacement"))
                && !failed.replace(true)
            {
                return Err(io::Error::other("injected replacement failure"));
            }
            fs::rename(from, to)
        });
        assert!(result.is_err());
        for name in ["a", "b", "c"] {
            assert_eq!(
                fs::read_to_string(root.path().join(name)).unwrap(),
                format!("current {name}")
            );
        }
        assert_eq!(fs::read_dir(root.path()).unwrap().count(), 3);
    }

    #[test]
    fn directory_replacement_refuses_unrelated_files() {
        let root = tempfile::tempdir().unwrap();
        let target = root.path().join("foo");
        let expected = target.join("affected.txt");
        let unrelated = target.join("unrelated.txt");
        let source = root.path().join("snapshot");
        fs::create_dir(&target).unwrap();
        for (path, contents) in [
            (&expected, "affected"),
            (&unrelated, "unrelated"),
            (&source, "snapshot"),
        ] {
            fs::write(path, contents).unwrap();
        }
        let error = apply(
            root.path(),
            vec![
                Change {
                    target,
                    source: Some(source),
                },
                Change {
                    target: expected.clone(),
                    source: None,
                },
            ],
        )
        .unwrap_err();
        assert!(error.to_string().contains("unrelated path"));
        assert_eq!(fs::read_to_string(unrelated).unwrap(), "unrelated");
        assert_eq!(fs::read_to_string(expected).unwrap(), "affected");
    }

    #[test]
    fn replacement_failure_rolls_back_a_directory_swap() {
        let root = tempfile::tempdir().unwrap();
        let dir = root.path().join("foo");
        let child = dir.join("child");
        let later = root.path().join("later");
        let source = root.path().join("snapshot");
        fs::create_dir(&dir).unwrap();
        for (path, contents) in [
            (&child, "current child"),
            (&later, "current later"),
            (&source, "snapshot"),
        ] {
            fs::write(path, contents).unwrap();
        }
        let result = apply_with(
            root.path(),
            vec![
                Change {
                    target: child.clone(),
                    source: None,
                },
                Change {
                    target: dir,
                    source: Some(source.clone()),
                },
                Change {
                    target: later.clone(),
                    source: Some(source),
                },
            ],
            |from, to| {
                if to == later && from.file_name().is_some_and(|name| name == "replacement") {
                    return Err(io::Error::other("injected replacement failure"));
                }
                fs::rename(from, to)
            },
        );
        assert!(result.is_err());
        assert_eq!(fs::read_to_string(child).unwrap(), "current child");
        assert_eq!(fs::read_to_string(later).unwrap(), "current later");
        assert_eq!(fs::read_dir(root.path()).unwrap().count(), 3);
    }

    #[test]
    fn rollback_failure_keeps_original_contents_and_target_manifest() {
        let root = tempfile::tempdir().unwrap();
        let source = root.path().join("source");
        let target = root.path().join("target");
        fs::write(&source, "snapshot").unwrap();
        fs::write(&target, "current work").unwrap();
        let result = apply_with(
            root.path(),
            vec![Change {
                target: target.clone(),
                source: Some(source),
            }],
            |from, to| {
                if to == target {
                    return Err(io::Error::other("replacement and rollback unavailable"));
                }
                fs::rename(from, to)
            },
        );
        assert!(result.unwrap_err().to_string().contains("backups retained"));
        let checkpoint = fs::read_dir(root.path())
            .unwrap()
            .map(|entry| entry.unwrap().path())
            .find(|path| path.is_dir())
            .unwrap();
        assert_eq!(
            fs::read_to_string(checkpoint.join("backup")).unwrap(),
            "current work"
        );
        assert_eq!(
            serde_json::from_slice::<PathBuf>(&fs::read(checkpoint.join("target.json")).unwrap())
                .unwrap(),
            target
        );
    }
}
