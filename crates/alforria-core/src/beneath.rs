//! Opening a file that must stay inside a directory.
//!
//! "Canonicalise the path, check it starts with the root, then open the
//! path" decides on one resolution and reads through another: a symlink
//! swapped in between escapes, and so does every fallback for a path that
//! does not canonicalise. [`open_beneath`] opens once and lets the kernel
//! decide during that one resolution: on Linux `openat2` with
//! `RESOLVE_BENEATH` (links inside the root are followed, nothing —
//! `..`, an absolute link, a magic link — leaves it); where that is not
//! available, a component-by-component `openat` with `O_NOFOLLOW`, which
//! refuses every symlink on the way (every other Unix, and a Linux kernel
//! without `openat2`: `ENOSYS`, or `EPERM`/`EINVAL` from a filter — never a
//! check of the name). On Windows the file is opened and the decision is
//! made from the opened handles' final paths (`GetFinalPathNameByHandleW`);
//! a platform with neither refuses. The caller reads from the descriptor it
//! gets back; there is no second lookup of the name.
//!
//! Every "path must stay inside root" read goes through here.

use std::fs::File;
use std::io;
use std::path::{Component, Path, PathBuf};

/// `relative` with `.` dropped and `..` folded, or `None` when it is
/// absolute, empty, or climbs above where it starts.
pub fn normalize_relative(relative: &Path) -> Option<PathBuf> {
    let mut parts: Vec<&std::ffi::OsStr> = Vec::new();
    for component in relative.components() {
        match component {
            Component::Normal(part) => parts.push(part),
            Component::CurDir => {}
            Component::ParentDir => {
                parts.pop()?;
            }
            Component::RootDir | Component::Prefix(_) => return None,
        }
    }
    if parts.is_empty() {
        return None;
    }
    Some(parts.iter().collect())
}

fn refused(reason: &str) -> io::Error {
    io::Error::new(io::ErrorKind::PermissionDenied, reason.to_string())
}

/// Open `relative` for reading beneath `root`. `root` is trusted and
/// resolved normally; nothing below it may lead out. Errors: `NotFound`
/// when the file does not exist, `PermissionDenied` when the path is not
/// relative or resolution would leave `root`.
pub fn open_beneath(root: &Path, relative: &Path) -> io::Result<File> {
    let relative = normalize_relative(relative)
        .ok_or_else(|| refused("the path must be relative and stay inside its root"))?;
    imp::open(root, &relative)
}

/// Read a whole file beneath `root` as text ([`open_beneath`]).
pub fn read_to_string_beneath(root: &Path, relative: &Path) -> io::Result<String> {
    use std::io::Read;
    let mut file = open_beneath(root, relative)?;
    if !file.metadata()?.is_file() {
        return Err(refused("not a regular file"));
    }
    let mut text = String::new();
    file.read_to_string(&mut text)?;
    Ok(text)
}

#[cfg(unix)]
mod imp {
    use super::refused;
    use std::ffi::CString;
    use std::fs::File;
    use std::io;
    use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
    use std::os::unix::ffi::OsStrExt;
    use std::path::Path;

    fn cstring(bytes: &[u8]) -> io::Result<CString> {
        CString::new(bytes).map_err(|_| refused("the path contains a NUL byte"))
    }

    fn open_root(root: &Path) -> io::Result<OwnedFd> {
        let path = cstring(root.as_os_str().as_bytes())?;
        // SAFETY: `path` is a valid C string; the result is checked.
        let fd = unsafe {
            libc::open(
                path.as_ptr(),
                libc::O_RDONLY | libc::O_DIRECTORY | libc::O_CLOEXEC,
            )
        };
        if fd < 0 {
            return Err(io::Error::last_os_error());
        }
        // SAFETY: `fd` is a descriptor this call just opened.
        Ok(unsafe { OwnedFd::from_raw_fd(fd) })
    }

    /// `openat2(RESOLVE_BENEATH | RESOLVE_NO_MAGICLINKS)`; `None` when the
    /// kernel has no `openat2` (before 5.6) or a filter denies it.
    #[cfg(target_os = "linux")]
    fn open_resolving(root: &OwnedFd, relative: &Path) -> Option<io::Result<File>> {
        #[repr(C)]
        struct OpenHow {
            flags: u64,
            mode: u64,
            resolve: u64,
        }
        const RESOLVE_NO_MAGICLINKS: u64 = 0x02;
        const RESOLVE_BENEATH: u64 = 0x08;
        let path = match cstring(relative.as_os_str().as_bytes()) {
            Ok(path) => path,
            Err(error) => return Some(Err(error)),
        };
        let how = OpenHow {
            flags: (libc::O_RDONLY | libc::O_CLOEXEC | libc::O_NONBLOCK) as u64,
            mode: 0,
            resolve: RESOLVE_BENEATH | RESOLVE_NO_MAGICLINKS,
        };
        // SAFETY: the pointers are valid for the call and `how` has the
        // size passed; the result is checked.
        let fd = unsafe {
            libc::syscall(
                libc::SYS_openat2,
                root.as_raw_fd(),
                path.as_ptr(),
                &how as *const OpenHow,
                std::mem::size_of::<OpenHow>(),
            )
        };
        if fd >= 0 {
            // SAFETY: a descriptor this call just opened.
            return Some(Ok(unsafe { File::from_raw_fd(fd as i32) }));
        }
        let error = io::Error::last_os_error();
        match error.raw_os_error() {
            Some(libc::ENOSYS) | Some(libc::EPERM) | Some(libc::EINVAL) => None,
            // EXDEV: resolution would have left the root. ELOOP: a magic
            // link, or too many links.
            Some(libc::EXDEV) | Some(libc::ELOOP) => {
                Some(Err(refused("the path leads outside its root")))
            }
            _ => Some(Err(error)),
        }
    }

    #[cfg(not(target_os = "linux"))]
    fn open_resolving(_root: &OwnedFd, _relative: &Path) -> Option<io::Result<File>> {
        None
    }

    /// One `openat(O_NOFOLLOW)` per component: no symlink is followed at
    /// all, so none can lead out.
    fn open_stepwise(root: OwnedFd, relative: &Path) -> io::Result<File> {
        let parts: Vec<_> = relative.components().collect();
        let mut dir = root;
        for (index, part) in parts.iter().enumerate() {
            let last = index + 1 == parts.len();
            let name = cstring(part.as_os_str().as_bytes())?;
            let mut flags = libc::O_RDONLY | libc::O_CLOEXEC | libc::O_NOFOLLOW | libc::O_NONBLOCK;
            if !last {
                flags |= libc::O_DIRECTORY;
            }
            // SAFETY: `dir` is open, `name` a valid C string; checked.
            let fd = unsafe { libc::openat(dir.as_raw_fd(), name.as_ptr(), flags) };
            if fd < 0 {
                let error = io::Error::last_os_error();
                return Err(match error.raw_os_error() {
                    // A symlink where a plain name was required.
                    Some(libc::ELOOP) | Some(libc::EMLINK) => {
                        refused("the path goes through a symbolic link")
                    }
                    Some(libc::ENOTDIR) if !last => {
                        refused("the path goes through a symbolic link")
                    }
                    _ => error,
                });
            }
            // SAFETY: a descriptor this call just opened.
            dir = unsafe { OwnedFd::from_raw_fd(fd) };
        }
        Ok(File::from(dir))
    }

    pub fn open(root: &Path, relative: &Path) -> io::Result<File> {
        let root = open_root(root)?;
        match open_resolving(&root, relative) {
            Some(result) => result,
            None => open_stepwise(root, relative),
        }
    }

    #[cfg(test)]
    pub fn open_stepwise_for_tests(root: &Path, relative: &Path) -> io::Result<File> {
        open_stepwise(open_root(root)?, relative)
    }
}

/// The decision for a platform without `openat`: made from the *opened
/// handles'* final paths, never from a name. `None` — the platform could
/// not say where a handle points — is a refusal.
#[cfg(any(windows, test))]
fn decide_from_final_paths(root: Option<&Path>, file: Option<&Path>) -> io::Result<()> {
    let (Some(root), Some(file)) = (root, file) else {
        return Err(refused(
            "cannot tell where the opened file is: refusing rather than trusting its name",
        ));
    };
    if file != root && file.starts_with(root) {
        Ok(())
    } else {
        Err(refused("the path leads outside its root"))
    }
}

#[cfg(windows)]
mod imp {
    use super::decide_from_final_paths;
    use std::fs::File;
    use std::io;
    use std::os::windows::ffi::OsStringExt;
    use std::os::windows::fs::OpenOptionsExt;
    use std::os::windows::io::AsRawHandle;
    use std::path::{Path, PathBuf};

    const FILE_FLAG_BACKUP_SEMANTICS: u32 = 0x0200_0000;
    /// `FILE_NAME_NORMALIZED | VOLUME_NAME_DOS`.
    const FINAL_PATH_FLAGS: u32 = 0;

    #[link(name = "kernel32")]
    extern "system" {
        fn GetFinalPathNameByHandleW(
            file: *mut core::ffi::c_void,
            path: *mut u16,
            len: u32,
            flags: u32,
        ) -> u32;
    }

    /// Where an open handle points, links and all resolved — asked of the
    /// handle, so it is the file that was opened whatever its name does
    /// afterwards.
    fn final_path(file: &File) -> Option<PathBuf> {
        let mut buffer = vec![0u16; 1024];
        loop {
            // SAFETY: the handle is open and the buffer has the length
            // passed; the result is checked.
            let len = unsafe {
                GetFinalPathNameByHandleW(
                    file.as_raw_handle() as *mut core::ffi::c_void,
                    buffer.as_mut_ptr(),
                    buffer.len() as u32,
                    FINAL_PATH_FLAGS,
                )
            } as usize;
            if len == 0 {
                return None;
            }
            if len > buffer.len() {
                buffer.resize(len, 0);
                continue;
            }
            return Some(PathBuf::from(std::ffi::OsString::from_wide(&buffer[..len])));
        }
    }

    pub fn open(root: &Path, relative: &Path) -> io::Result<File> {
        // A directory handle needs backup semantics.
        let root_handle = std::fs::OpenOptions::new()
            .read(true)
            .custom_flags(FILE_FLAG_BACKUP_SEMANTICS)
            .open(root)?;
        let file = File::open(root.join(relative))?;
        decide_from_final_paths(
            final_path(&root_handle).as_deref(),
            final_path(&file).as_deref(),
        )?;
        Ok(file)
    }
}

#[cfg(not(any(unix, windows)))]
mod imp {
    use super::refused;
    use std::fs::File;
    use std::io;
    use std::path::Path;

    /// No way here to open beneath a directory or to ask a handle where it
    /// points: refuse, rather than check a name.
    pub fn open(_root: &Path, _relative: &Path) -> io::Result<File> {
        Err(refused(
            "confined file access is not supported on this platform",
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tree() -> (tempfile::TempDir, PathBuf) {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("root");
        std::fs::create_dir_all(root.join("sub")).unwrap();
        std::fs::write(root.join("sub/inside.txt"), "inside").unwrap();
        std::fs::write(dir.path().join("secret.txt"), "secret").unwrap();
        (dir, root)
    }

    /// The handle-based decision (Windows): inside only when both final
    /// paths are known and the file's is below the root's. The swap shape —
    /// a name inside the root whose handle resolved outside — is refused,
    /// and so is a platform that cannot say.
    #[test]
    fn the_final_path_decision_fails_closed() {
        let root = Path::new("/srv/root");
        let decide = |root: Option<&Path>, file: Option<&str>| {
            decide_from_final_paths(root, file.map(Path::new)).map_err(|error| error.kind())
        };
        assert_eq!(decide(Some(root), Some("/srv/root/sub/a.txt")), Ok(()));
        let denied = Err(io::ErrorKind::PermissionDenied);
        // Opened through a link that pointed out.
        assert_eq!(decide(Some(root), Some("/srv/secret.txt")), denied);
        assert_eq!(decide(Some(root), Some("/srv/root-other/a.txt")), denied);
        assert_eq!(decide(Some(root), Some("/srv/root")), denied);
        // No answer from the platform, for either handle.
        assert_eq!(decide(Some(root), None), denied);
        assert_eq!(decide(None, Some("/srv/root/a.txt")), denied);
    }

    #[test]
    fn reads_inside_and_folds_dots() {
        let (_dir, root) = tree();
        let read = |path: &str| read_to_string_beneath(&root, Path::new(path));
        assert_eq!(read("sub/inside.txt").unwrap(), "inside");
        assert_eq!(read("sub/../sub/./inside.txt").unwrap(), "inside");
        assert_eq!(
            read("sub/missing.txt").unwrap_err().kind(),
            io::ErrorKind::NotFound
        );
    }

    #[test]
    fn refuses_what_is_not_a_relative_path_inside() {
        let (dir, root) = tree();
        for path in [
            "../secret.txt",
            "sub/../../secret.txt",
            "/etc/passwd",
            "",
            ".",
            dir.path().join("secret.txt").to_str().unwrap(),
        ] {
            let error = read_to_string_beneath(&root, Path::new(path)).unwrap_err();
            assert_eq!(error.kind(), io::ErrorKind::PermissionDenied, "{path}");
        }
        // A directory is not a file to read.
        assert!(read_to_string_beneath(&root, Path::new("sub")).is_err());
    }

    #[cfg(unix)]
    #[test]
    fn no_symlink_leads_out_and_a_dangling_one_is_not_accepted() {
        use std::os::unix::fs::symlink;
        let (dir, root) = tree();
        symlink(dir.path().join("secret.txt"), root.join("file-link")).unwrap();
        symlink(dir.path(), root.join("dir-link")).unwrap();
        symlink("../secret.txt", root.join("relative-link")).unwrap();
        symlink(dir.path().join("nowhere"), root.join("dangling")).unwrap();
        symlink("sub/inside.txt", root.join("inner-link")).unwrap();
        for path in [
            "file-link",
            "dir-link/secret.txt",
            "relative-link",
            "dangling",
        ] {
            let error = read_to_string_beneath(&root, Path::new(path)).unwrap_err();
            assert_ne!(error.kind(), io::ErrorKind::NotFound, "{path}: {error}");
            // The stepwise fallback refuses them too.
            assert!(imp::open_stepwise_for_tests(&root, Path::new(path)).is_err());
        }
        // The fallback follows no link at all, an inner one included; the
        // kernel resolver follows one that stays inside.
        assert!(imp::open_stepwise_for_tests(&root, Path::new("inner-link")).is_err());
        assert!(imp::open_stepwise_for_tests(&root, Path::new("sub/inside.txt")).is_ok());
        #[cfg(target_os = "linux")]
        if let Ok(text) = read_to_string_beneath(&root, Path::new("inner-link")) {
            assert_eq!(text, "inside");
        }
    }

    /// The decision is made on what was opened: swapping the name for a
    /// link after the open changes nothing about the descriptor in hand.
    #[cfg(unix)]
    #[test]
    fn the_descriptor_outlives_a_swap_of_the_name() {
        use std::io::Read;
        let (dir, root) = tree();
        let mut file = open_beneath(&root, Path::new("sub/inside.txt")).unwrap();
        std::fs::remove_file(root.join("sub/inside.txt")).unwrap();
        std::os::unix::fs::symlink(dir.path().join("secret.txt"), root.join("sub/inside.txt"))
            .unwrap();
        let mut text = String::new();
        file.read_to_string(&mut text).unwrap();
        assert_eq!(text, "inside");
        // And a fresh open of the swapped name is refused.
        assert!(read_to_string_beneath(&root, Path::new("sub/inside.txt")).is_err());
    }
}
