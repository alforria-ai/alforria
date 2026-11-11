//! Which process a piece of unfinished work belongs to, and which process
//! holds a role (an alforria extension).
//!
//! Several alforria processes can share a data directory: a TUI beside
//! `alforria serve`, or two servers a supervisor started. Work one of them
//! left unfinished (a running turn's marker, a trigger run) is another's to
//! clean up only when its owner is provably gone, and work only one of them
//! should do at a time (scheduling a directory's triggers, pruning the
//! event log) is the one's that holds the role's lock.
//!
//! * [`ProcessOwner`] names a process: a random id of its own, its process
//!   id, the machine boot it belongs to and, on Linux, when it started.
//! * [`RoleLock`] is an advisory lock on a file. The operating system drops
//!   it when the holder exits, however it exits.

use std::fs::{File, TryLockError};
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};

use serde::{Deserialize, Serialize};

/// Whether the process an owner names still exists.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Liveness {
    Alive,
    /// Provably gone: nothing it left unfinished will ever finish.
    Gone,
    /// This platform cannot tell (not a Unix).
    Unknown,
}

/// A server process, as recorded beside the work it owns.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ProcessOwner {
    /// A random id this process gave itself when it started: two processes
    /// never share one, whatever their process ids.
    pub instance: String,
    pub pid: u32,
    /// The boot of the machine the process belongs to
    /// (`/proc/sys/kernel/random/boot_id`). Linux only; elsewhere there is
    /// none, and a reboot is noticed by the process id being gone.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub boot: Option<String>,
    /// When the process started, in clock ticks since the machine booted
    /// (`/proc/<pid>/stat`). Linux only: it tells a process from a later
    /// one that was given the same id.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub started: Option<u64>,
}

fn machine_boot() -> Option<String> {
    std::fs::read_to_string("/proc/sys/kernel/random/boot_id")
        .ok()
        .map(|id| id.trim().to_string())
        .filter(|id| !id.is_empty())
}

/// Field 22 of `/proc/<pid>/stat`. The command name (field 2) may hold
/// spaces and parentheses, so the fields are counted from its last `)`.
fn start_ticks(pid: u32) -> Option<u64> {
    let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    let rest = &stat[stat.rfind(')')? + 1..];
    rest.split_whitespace().nth(19)?.parse().ok()
}

impl ProcessOwner {
    /// This process.
    pub fn current() -> &'static ProcessOwner {
        static CURRENT: OnceLock<ProcessOwner> = OnceLock::new();
        CURRENT.get_or_init(|| ProcessOwner {
            instance: ulid::Ulid::new().to_string(),
            pid: std::process::id(),
            boot: machine_boot(),
            started: start_ticks(std::process::id()),
        })
    }

    /// Whether the process this names still exists.
    ///
    /// After a reboot a process id proves nothing: the new server is likely
    /// to have the very id the old one had. So an owner from another boot
    /// of the machine, or one carrying this process's id without being this
    /// process, is gone; and on Linux so is one whose id now belongs to a
    /// process that started at another moment. On other Unix systems a
    /// process id that was given to something else reads as alive.
    pub fn liveness(&self) -> Liveness {
        let me = ProcessOwner::current();
        if self.instance == me.instance {
            return Liveness::Alive;
        }
        if self.pid == me.pid {
            return Liveness::Gone;
        }
        if let (Some(then), Some(now)) = (&self.boot, &me.boot) {
            if then != now {
                return Liveness::Gone;
            }
        }
        if cfg!(target_os = "linux") {
            if !Path::new("/proc").join(self.pid.to_string()).exists() {
                return Liveness::Gone;
            }
            return match (self.started, start_ticks(self.pid)) {
                (Some(then), Some(now)) if then != now => Liveness::Gone,
                _ => Liveness::Alive,
            };
        }
        signal_liveness(self.pid)
    }

    pub fn gone(&self) -> bool {
        self.liveness() == Liveness::Gone
    }
}

#[cfg(unix)]
fn signal_liveness(pid: u32) -> Liveness {
    let Ok(pid) = libc::pid_t::try_from(pid) else {
        return Liveness::Gone;
    };
    if pid <= 0 {
        return Liveness::Gone;
    }
    // Signal 0 delivers nothing: it only asks whether the process is there.
    // SAFETY: `kill` takes two integers and touches no memory.
    if unsafe { libc::kill(pid, 0) } == 0 {
        return Liveness::Alive;
    }
    match std::io::Error::last_os_error().raw_os_error() {
        Some(libc::ESRCH) => Liveness::Gone,
        // Not ours to signal: it exists.
        _ => Liveness::Alive,
    }
}

#[cfg(not(unix))]
fn signal_liveness(_pid: u32) -> Liveness {
    Liveness::Unknown
}

/// An advisory lock on a file, for work one process at a time should do.
/// Taking it never waits: a process that does not get it is a follower and
/// may ask again later. The lock goes when the holder releases it or exits.
#[derive(Debug)]
pub struct RoleLock {
    path: PathBuf,
    held: Mutex<Option<File>>,
}

impl RoleLock {
    pub fn new(path: PathBuf) -> RoleLock {
        RoleLock {
            path,
            held: Mutex::new(None),
        }
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn held(&self) -> bool {
        self.held
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .is_some()
    }

    /// Take the role if nobody has it. `true` when this process holds it,
    /// now or already.
    ///
    /// A file that cannot be opened or a filesystem that has no locks also
    /// answers `true`: a process alone on its data directory must work as
    /// it always did, and telling two apart is not possible there.
    pub fn acquire(&self) -> bool {
        let mut held = self
            .held
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if held.is_some() {
            return true;
        }
        if let Some(parent) = self.path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        let file = match File::options()
            .create(true)
            .truncate(false)
            .write(true)
            .open(&self.path)
        {
            Ok(file) => file,
            Err(error) => {
                tracing::debug!("no role lock at {}: {error}", self.path.display());
                return true;
            }
        };
        match file.try_lock() {
            Ok(()) => {
                *held = Some(file);
                true
            }
            Err(TryLockError::WouldBlock) => false,
            Err(TryLockError::Error(error)) => {
                tracing::debug!("{} cannot be locked: {error}", self.path.display());
                true
            }
        }
    }

    /// Give the role up, so another process can take it.
    pub fn release(&self) {
        self.held
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .take();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn owner(instance: &str, pid: u32, boot: Option<&str>) -> ProcessOwner {
        ProcessOwner {
            instance: instance.to_string(),
            pid,
            boot: boot.map(str::to_string),
            started: None,
        }
    }

    #[test]
    fn an_owner_is_gone_only_when_its_process_provably_is() {
        let me = ProcessOwner::current();
        assert_eq!(me.liveness(), Liveness::Alive);
        // Another process under this one's id: after a reboot the new
        // server gets the id the old one had.
        assert!(owner("other", me.pid, me.boot.as_deref()).gone());
        if cfg!(unix) {
            // A process that does not exist.
            assert!(owner("other", i32::MAX as u32 - 7, me.boot.as_deref()).gone());
            // A live one: another server sharing the data directory.
            assert!(!owner("other", 1, me.boot.as_deref()).gone());
        }
        if cfg!(target_os = "linux") {
            // The same id, from before the machine restarted.
            assert!(owner("other", 1, Some("another-boot")).gone());
            // The same id, given to a process that started at another time.
            let reused = ProcessOwner {
                started: start_ticks(1).map(|ticks| ticks + 1),
                ..owner("other", 1, me.boot.as_deref())
            };
            assert!(reused.gone());
            let same = ProcessOwner {
                started: start_ticks(1),
                ..owner("other", 1, me.boot.as_deref())
            };
            assert!(!same.gone());
        }
    }

    #[test]
    fn a_role_has_one_holder_until_it_lets_go() {
        let dir = crate::storage::test_support::TempDir::new("role-lock");
        let path = dir.path().join("lock").join("role.lock");
        let first = RoleLock::new(path.clone());
        let second = RoleLock::new(path.clone());
        assert!(first.acquire());
        assert!(first.acquire(), "asking again keeps it");
        assert!(!second.acquire());
        assert!(!second.held());
        first.release();
        assert!(second.acquire());
        assert!(!first.acquire());
        // A holder that goes away without releasing lets go too.
        drop(second);
        assert!(first.acquire());
    }
}
