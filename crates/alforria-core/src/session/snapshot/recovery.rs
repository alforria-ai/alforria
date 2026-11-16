//! Apply fully staged recovery contents with reversible filesystem changes.

use std::fs;
use std::io;
use std::path::{Component, Path, PathBuf};

use tempfile::TempDir;

use crate::CoreError;

pub(super) struct Change {
    pub target: PathBuf,
    pub source: Option<PathBuf>,
}

struct Prepared {
    target: PathBuf,
    area: TempDir,
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

fn inspect(root: &Path, target: &Path) -> io::Result<()> {
    let relative = target.strip_prefix(root).map_err(io::Error::other)?;
    if relative.as_os_str().is_empty()
        || relative
            .components()
            .any(|part| !matches!(part, Component::Normal(_)))
    {
        return Err(io::Error::other("invalid snapshot recovery path"));
    }
    let mut parent = root.to_path_buf();
    for part in relative.parent().unwrap_or(Path::new("")).components() {
        parent.push(part);
        match fs::symlink_metadata(&parent) {
            Ok(meta) if !meta.is_dir() || meta.file_type().is_symlink() => {
                return Err(io::Error::other(format!(
                    "recovery parent is not a plain directory: {}",
                    parent.display()
                )));
            }
            Ok(_) => {}
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Err(error) => return Err(error),
        }
    }
    match fs::symlink_metadata(target) {
        Ok(meta) if meta.is_dir() && !meta.file_type().is_symlink() => Err(io::Error::other(
            format!("recovery target is a directory: {}", target.display()),
        )),
        Ok(_) => Ok(()),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error),
    }
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

fn apply_with(
    root: &Path,
    changes: Vec<Change>,
    rename: impl Fn(&Path, &Path) -> io::Result<()>,
) -> Result<(), CoreError> {
    // Check every destination before creating even the temporary directories.
    for (index, change) in changes.iter().enumerate() {
        inspect(root, &change.target)?;
        if changes[..index].iter().any(|other| {
            change.target.starts_with(&other.target) || other.target.starts_with(&change.target)
        }) {
            return Err(CoreError::Storage(
                "overlapping snapshot recovery paths".into(),
            ));
        }
    }
    let mut created = CreatedDirectories::default();
    let mut prepared = Vec::new();
    for change in changes {
        if change.source.is_none()
            && fs::symlink_metadata(&change.target)
                .is_err_and(|error| error.kind() == io::ErrorKind::NotFound)
        {
            continue;
        }
        let parent = change.target.parent().expect("validated recovery path");
        create_parents(root, parent, &mut created)?;
        let area = tempfile::Builder::new()
            .prefix(".alforria-recovery-")
            .tempdir_in(parent)?;
        // Each retained checkpoint can be understood without process memory.
        fs::write(
            area.path().join("target.json"),
            serde_json::to_vec(&change.target).map_err(io::Error::other)?,
        )?;
        if let Some(source) = &change.source {
            copy_entry(source, &area.path().join("replacement"))?;
        }
        prepared.push(Prepared {
            target: change.target,
            area,
            replacement: change.source.is_some(),
            backed_up: false,
            installed: false,
        });
    }

    let result = (|| -> io::Result<()> {
        for change in &mut prepared {
            inspect(root, &change.target)?;
            match fs::symlink_metadata(&change.target) {
                Ok(_) => {
                    rename(&change.target, &change.area.path().join("backup"))?;
                    change.backed_up = true;
                }
                Err(error) if error.kind() == io::ErrorKind::NotFound => {}
                Err(error) => return Err(error),
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
            let rollback = (|| -> io::Result<()> {
                inspect(root, &change.target)?;
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
