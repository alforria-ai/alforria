//! Process groups started on the agent's behalf (alforria extension).
//!
//! The bash tool, the session `shell` command and PTYs each run their child
//! as the leader of its own process group. A supervised server that is told
//! to stop must not leave those running, and a cancelled turn that did not
//! settle in time must not either: every live group is recorded here so
//! shutdown can signal whatever is left.

use std::collections::BTreeSet;
use std::sync::{Mutex, MutexGuard};

static GROUPS: Mutex<BTreeSet<u32>> = Mutex::new(BTreeSet::new());

fn groups() -> MutexGuard<'static, BTreeSet<u32>> {
    GROUPS
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// Records a live process group until dropped.
#[derive(Debug)]
pub struct Registration(u32);

impl Drop for Registration {
    fn drop(&mut self) {
        groups().remove(&self.0);
    }
}

/// Record `pgid` (the pid of a child spawned as a group leader). A zero pid
/// (the spawn raced the exit) is not recorded.
pub fn register(pgid: u32) -> Registration {
    if pgid != 0 {
        groups().insert(pgid);
    }
    Registration(pgid)
}

/// The process groups currently recorded.
pub fn live() -> Vec<u32> {
    groups().iter().copied().collect()
}

/// Force-kill every recorded group. Returns how many were signalled.
pub fn kill_all() -> usize {
    let live = live();
    for pgid in &live {
        kill_group(*pgid);
    }
    live.len()
}

#[cfg(unix)]
fn kill_group(pgid: u32) {
    let pgid = pgid as libc::pid_t;
    if pgid > 1 {
        unsafe { libc::kill(-pgid, libc::SIGKILL) };
    }
}

#[cfg(not(unix))]
fn kill_group(pid: u32) {
    let _ = std::process::Command::new("taskkill")
        .args(["/PID", &pid.to_string(), "/T", "/F"])
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status();
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn registration_lasts_until_dropped() {
        let registration = register(4_000_001);
        assert!(live().contains(&4_000_001));
        drop(registration);
        assert!(!live().contains(&4_000_001));
    }

    #[test]
    fn a_zero_pid_is_never_recorded() {
        let _registration = register(0);
        assert!(!live().contains(&0));
    }

    #[cfg(unix)]
    #[test]
    fn kill_all_reaches_the_whole_group() {
        use std::os::unix::process::CommandExt;
        let mut child = std::process::Command::new("sh")
            .arg("-c")
            .arg("sleep 600 & wait")
            .process_group(0)
            .spawn()
            .unwrap();
        let _registration = register(child.id());
        assert!(kill_all() >= 1);
        let status = child.wait().unwrap();
        assert!(!status.success());
    }
}
