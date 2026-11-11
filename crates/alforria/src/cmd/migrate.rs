//! `alforria migrate` — move an install from the shared `opencode`
//! directories into alforria's own (see `alforria_core::paths`).
//!
//! Nothing here runs implicitly. The data directory can be tens of
//! gigabytes, and other tools may still be using the `opencode` ones, so the
//! move is something the user asks for:
//!
//! * config, cache and state are **copied** — they are small, and upstream
//!   opencode keeps working with its own copy;
//! * data is **moved** (a rename, instant on one filesystem), or copied with
//!   `--copy`, or left behind with `--fresh`, which starts a new database and
//!   carries over only the saved logins.

use std::path::{Path, PathBuf};

use alforria_core::paths::{APP_DIR, LEGACY_DIR};
use alforria_core::GlobalPaths;
use clap::ArgMatches;

use crate::error::{CliError, TypedError};
use crate::ui::Ui;

/// Login files carried into a fresh data directory.
const CREDENTIAL_FILES: &[&str] = &["auth.json", "mcp-auth.json", "libertai-auth.json"];

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DataMode {
    Move,
    Copy,
    Fresh,
}

#[derive(Debug, Clone, Copy)]
pub struct Options {
    pub data: DataMode,
    pub dry_run: bool,
    pub force: bool,
}

#[derive(Debug, PartialEq, Eq)]
enum Action {
    /// The alforria directory is already there; leave both alone.
    AlreadyDone,
    /// Nothing to bring over.
    NothingToMigrate,
    Copy,
    Move,
    Fresh,
}

fn plan(from: &Path, to: &Path, wanted: Action) -> Action {
    if to.exists() {
        Action::AlreadyDone
    } else if !from.exists() {
        Action::NothingToMigrate
    } else {
        wanted
    }
}

pub fn run(matches: &ArgMatches, ui: &mut Ui) -> Result<(), TypedError> {
    let data = if matches.get_flag("fresh") {
        DataMode::Fresh
    } else if matches.get_flag("copy") {
        DataMode::Copy
    } else {
        DataMode::Move
    };
    let options = Options {
        data,
        dry_run: matches.get_flag("dry-run"),
        force: matches.get_flag("force"),
    };
    let home = GlobalPaths::home_from_env();
    let legacy = GlobalPaths::named(home.clone(), LEGACY_DIR);
    let own = GlobalPaths::named(home, APP_DIR);
    let report = migrate(&legacy, &own, options)
        .map_err(|message| TypedError::Cli(CliError::new(message)))?;
    for line in report {
        ui.println(&line);
    }
    Ok(())
}

/// Carry out (or, with `dry_run`, describe) the migration. Returns one line
/// per location.
pub fn migrate(
    legacy: &GlobalPaths,
    own: &GlobalPaths,
    options: Options,
) -> Result<Vec<String>, String> {
    let data_action = plan(
        &legacy.data,
        &own.data,
        match options.data {
            DataMode::Move => Action::Move,
            DataMode::Copy => Action::Copy,
            DataMode::Fresh => Action::Fresh,
        },
    );
    // Renaming a directory out from under a process that has the database
    // open leaves it writing to paths that no longer exist.
    let mut blocked = None;
    if data_action == Action::Move && !options.force {
        let users = processes_using(&legacy.data);
        if !users.is_empty() {
            let message = format!(
                "{} is in use by process {}. Stop every running alforria and opencode \
                 (servers, TUIs, `alforria web`) and run this again, or pass --copy to \
                 copy the data instead of moving it.",
                legacy.data.display(),
                users
                    .iter()
                    .map(u32::to_string)
                    .collect::<Vec<_>>()
                    .join(", ")
            );
            // A dry run still shows the plan, with the obstacle named.
            if !options.dry_run {
                return Err(message);
            }
            blocked = Some(message);
        }
    }

    let steps = [
        (
            "config",
            &legacy.config,
            &own.config,
            plan(&legacy.config, &own.config, Action::Copy),
        ),
        (
            "cache",
            &legacy.cache,
            &own.cache,
            plan(&legacy.cache, &own.cache, Action::Copy),
        ),
        (
            "state",
            &legacy.state,
            &own.state,
            plan(&legacy.state, &own.state, Action::Copy),
        ),
        ("data", &legacy.data, &own.data, data_action),
    ];
    let verb = |done: &'static str, will: &'static str| if options.dry_run { will } else { done };
    let mut report = Vec::new();
    for (name, from, to, action) in steps {
        let line = match action {
            Action::AlreadyDone => format!("{name}: {} already exists, left as is", to.display()),
            Action::NothingToMigrate => {
                format!("{name}: nothing at {}, nothing to do", from.display())
            }
            Action::Copy => {
                if !options.dry_run {
                    copy_tree(from, to).map_err(|error| {
                        format!("copying {} to {}: {error}", from.display(), to.display())
                    })?;
                }
                format!(
                    "{name}: {} {} to {}",
                    verb("copied", "would copy"),
                    from.display(),
                    to.display()
                )
            }
            Action::Move => {
                if !options.dry_run {
                    if let Some(parent) = to.parent() {
                        std::fs::create_dir_all(parent).map_err(|error| error.to_string())?;
                    }
                    std::fs::rename(from, to).map_err(|error| {
                        format!(
                            "moving {} to {}: {error}. If they are on different filesystems, \
                             pass --copy.",
                            from.display(),
                            to.display()
                        )
                    })?;
                }
                format!(
                    "{name}: {} {} to {}",
                    verb("moved", "would move"),
                    from.display(),
                    to.display()
                )
            }
            Action::Fresh => {
                if !options.dry_run {
                    std::fs::create_dir_all(to).map_err(|error| error.to_string())?;
                    for file in CREDENTIAL_FILES {
                        let source = from.join(file);
                        if source.is_file() {
                            std::fs::copy(&source, to.join(file))
                                .map_err(|error| format!("copying {file}: {error}"))?;
                        }
                    }
                }
                format!(
                    "{name}: {} a new {} with the saved logins; {} is untouched",
                    verb("started", "would start"),
                    to.display(),
                    from.display()
                )
            }
        };
        report.push(line);
    }
    if let Some(blocked) = blocked {
        report.push(format!("blocked: {blocked}"));
    }
    Ok(report)
}

/// Recursive copy that keeps symlinks as symlinks (a config directory holds
/// a `node_modules` full of them).
fn copy_tree(from: &Path, to: &Path) -> std::io::Result<()> {
    let metadata = std::fs::symlink_metadata(from)?;
    if metadata.file_type().is_symlink() {
        let target = std::fs::read_link(from)?;
        #[cfg(unix)]
        return std::os::unix::fs::symlink(target, to);
        #[cfg(not(unix))]
        return std::fs::copy(from.parent().unwrap_or(from).join(target), to).map(|_| ());
    }
    if metadata.is_dir() {
        std::fs::create_dir_all(to)?;
        for entry in std::fs::read_dir(from)? {
            let entry = entry?;
            copy_tree(&entry.path(), &to.join(entry.file_name()))?;
        }
        return Ok(());
    }
    std::fs::copy(from, to).map(|_| ())
}

/// Processes holding a file open under `directory`. Linux only (via
/// `/proc`); elsewhere this finds nothing and the caller relies on the
/// user's word.
fn processes_using(directory: &Path) -> Vec<u32> {
    let mut users = Vec::new();
    let Ok(directory) = directory.canonicalize() else {
        return users;
    };
    let me = std::process::id();
    let Ok(processes) = std::fs::read_dir("/proc") else {
        return users;
    };
    for process in processes.flatten() {
        let Some(pid) = process
            .file_name()
            .to_str()
            .and_then(|name| name.parse::<u32>().ok())
        else {
            continue;
        };
        if pid == me {
            continue;
        }
        let Ok(descriptors) = std::fs::read_dir(process.path().join("fd")) else {
            continue;
        };
        let open = descriptors.flatten().any(|descriptor| {
            std::fs::read_link(descriptor.path())
                .is_ok_and(|target: PathBuf| target.starts_with(&directory))
        });
        if open {
            users.push(pid);
        }
    }
    users
}

#[cfg(test)]
mod tests {
    use super::*;

    fn layout(root: &Path, name: &str) -> GlobalPaths {
        GlobalPaths {
            home: root.to_path_buf(),
            config: root.join("config").join(name),
            data: root.join("data").join(name),
            cache: root.join("cache").join(name),
            state: root.join("state").join(name),
        }
    }

    fn seed(legacy: &GlobalPaths) {
        for dir in [&legacy.config, &legacy.data, &legacy.state] {
            std::fs::create_dir_all(dir).unwrap();
        }
        std::fs::write(legacy.config.join("opencode.json"), "{}").unwrap();
        std::fs::write(legacy.data.join("opencode.db"), "db").unwrap();
        std::fs::write(legacy.data.join("auth.json"), "secret").unwrap();
        std::fs::create_dir_all(legacy.data.join("storage/nested")).unwrap();
        std::fs::write(legacy.data.join("storage/nested/a"), "a").unwrap();
    }

    fn options(data: DataMode) -> Options {
        Options {
            data,
            dry_run: false,
            force: false,
        }
    }

    #[test]
    fn moves_data_and_copies_the_rest() {
        let root = tempfile::tempdir().unwrap();
        let (legacy, own) = (
            layout(root.path(), "opencode"),
            layout(root.path(), "alforria"),
        );
        seed(&legacy);
        let report = migrate(&legacy, &own, options(DataMode::Move)).unwrap();
        assert_eq!(report.len(), 4);
        // Upstream keeps its config; the data went with alforria.
        assert!(legacy.config.join("opencode.json").exists());
        assert!(own.config.join("opencode.json").exists());
        assert!(!legacy.data.exists());
        assert_eq!(
            std::fs::read_to_string(own.data.join("storage/nested/a")).unwrap(),
            "a"
        );
        // No cache directory existed: nothing is invented.
        assert!(!own.cache.exists());
        assert!(report[1].contains("nothing to do"), "{report:?}");
    }

    #[test]
    fn fresh_starts_a_new_database_and_keeps_logins() {
        let root = tempfile::tempdir().unwrap();
        let (legacy, own) = (
            layout(root.path(), "opencode"),
            layout(root.path(), "alforria"),
        );
        seed(&legacy);
        migrate(&legacy, &own, options(DataMode::Fresh)).unwrap();
        assert!(legacy.data.join("opencode.db").exists());
        assert!(!own.data.join("opencode.db").exists());
        assert_eq!(
            std::fs::read_to_string(own.data.join("auth.json")).unwrap(),
            "secret"
        );
    }

    #[test]
    fn dry_run_and_rerun_change_nothing() {
        let root = tempfile::tempdir().unwrap();
        let (legacy, own) = (
            layout(root.path(), "opencode"),
            layout(root.path(), "alforria"),
        );
        seed(&legacy);
        let mut dry = options(DataMode::Copy);
        dry.dry_run = true;
        let report = migrate(&legacy, &own, dry).unwrap();
        assert!(report.iter().any(|line| line.contains("would copy")));
        assert!(!own.config.exists() && !own.data.exists());

        migrate(&legacy, &own, options(DataMode::Copy)).unwrap();
        assert!(legacy.data.join("opencode.db").exists());
        assert!(own.data.join("opencode.db").exists());
        // A second run never overwrites what is already there.
        std::fs::write(own.data.join("opencode.db"), "newer").unwrap();
        let report = migrate(&legacy, &own, options(DataMode::Move)).unwrap();
        assert!(report[3].contains("already exists"), "{report:?}");
        assert_eq!(
            std::fs::read_to_string(own.data.join("opencode.db")).unwrap(),
            "newer"
        );
        assert!(legacy.data.exists());
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn refuses_to_move_data_that_is_open() {
        let root = tempfile::tempdir().unwrap();
        let (legacy, own) = (
            layout(root.path(), "opencode"),
            layout(root.path(), "alforria"),
        );
        seed(&legacy);
        // Held open by a child process, as a running server would.
        let mut child = std::process::Command::new("sleep")
            .arg("30")
            .stdin(std::fs::File::open(legacy.data.join("opencode.db")).unwrap())
            .spawn()
            .unwrap();
        let result = migrate(&legacy, &own, options(DataMode::Move));
        child.kill().ok();
        child.wait().ok();
        let error = result.unwrap_err();
        assert!(error.contains("is in use by process"), "{error}");
        assert!(legacy.data.exists() && !own.data.exists());
    }
}
