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

/// A directory reached from the filesystem root without following a single
/// symbolic link, and held open: what is then created, replaced or removed
/// in it by name is in *that* directory, whatever happens to the names that
/// led to it afterwards.
///
/// This is the write side of this module. A caller resolves and judges a
/// path first (`protect::resolve_path` — the result has no link left in
/// it), then opens the parent of that resolved path here and acts through
/// the handle with the last component not followed either. A link (or
/// anything else) swapped in between the judgement and the write is then
/// not followed: the open fails. On a platform without `openat` the same
/// calls act by name, re-resolving the judged path immediately before each
/// one and refusing when it has changed ([`by_name`]): a much narrower
/// window, not a closed one.
#[derive(Debug)]
pub struct NoFollowDir {
    #[cfg(unix)]
    fd: std::os::fd::OwnedFd,
    #[cfg(not(unix))]
    path: PathBuf,
}

/// Create or truncate the file at `landing` (absolute, resolved) and write
/// `contents`, creating its directories — every component opened without
/// following a link ([`NoFollowDir`]).
pub fn write_no_follow(landing: &Path, contents: &[u8]) -> io::Result<()> {
    write_no_follow_mode(landing, contents, None)
}

/// [`write_no_follow`], also setting whether the file is executable
/// (`Some`) — on the handle that was written, not by name. Other
/// permission bits are left as they are. No effect where there is no such
/// bit.
pub fn write_no_follow_mode(
    landing: &Path,
    contents: &[u8],
    executable: Option<bool>,
) -> io::Result<()> {
    use std::io::Write;
    let (parent, name) = split_landing(landing)?;
    let mut file = NoFollowDir::open(parent, true)?.create(name, false)?;
    file.write_all(contents)?;
    #[cfg(unix)]
    if let Some(executable) = executable {
        use std::os::unix::fs::PermissionsExt;
        let mode = file.metadata()?.permissions().mode();
        let wanted = if executable {
            // Executable by whoever may read it.
            mode | ((mode & 0o444) >> 2)
        } else {
            mode & !0o111
        };
        if wanted != mode {
            file.set_permissions(std::fs::Permissions::from_mode(wanted))?;
        }
    }
    #[cfg(not(unix))]
    let _ = executable;
    Ok(())
}

/// Remove the file at `landing` (absolute, resolved) through its parent's
/// handle ([`NoFollowDir`]). A link by that name is itself removed, never
/// followed.
pub fn remove_no_follow(landing: &Path) -> io::Result<()> {
    let (parent, name) = split_landing(landing)?;
    NoFollowDir::open(parent, false)?.remove_file(name)
}

fn split_landing(landing: &Path) -> io::Result<(&Path, &std::ffi::OsStr)> {
    match (landing.parent(), landing.file_name()) {
        (Some(parent), Some(name)) if landing.is_absolute() => Ok((parent, name)),
        _ => Err(refused("not an absolute path to a file")),
    }
}

#[cfg(unix)]
impl NoFollowDir {
    /// Open `dir` (absolute, already resolved: no `.`/`..`, no link). With
    /// `create`, components that do not exist are made on the way, like
    /// `create_dir_all`. A component that is a symlink is a refusal
    /// (`PermissionDenied`).
    pub fn open(dir: &Path, create: bool) -> io::Result<NoFollowDir> {
        use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
        use std::os::unix::ffi::OsStrExt;
        if !dir.is_absolute() {
            return Err(refused("the directory must be an absolute path"));
        }
        let flags = libc::O_RDONLY | libc::O_DIRECTORY | libc::O_CLOEXEC;
        // SAFETY: a valid C string; the result is checked.
        let fd = unsafe { libc::open(c"/".as_ptr(), flags) };
        if fd < 0 {
            return Err(io::Error::last_os_error());
        }
        // SAFETY: a descriptor this call just opened.
        let mut at = unsafe { OwnedFd::from_raw_fd(fd) };
        for part in dir.components() {
            let name = match part {
                Component::RootDir => continue,
                Component::Normal(name) => Self::name(name.as_bytes())?,
                _ => return Err(refused("the directory must be a resolved path")),
            };
            if create {
                // SAFETY: `at` is open, `name` a valid C string; checked.
                let made = unsafe { libc::mkdirat(at.as_raw_fd(), name.as_ptr(), 0o777) };
                if made != 0 {
                    let error = io::Error::last_os_error();
                    if error.raw_os_error() != Some(libc::EEXIST) {
                        return Err(error);
                    }
                }
            }
            // SAFETY: as above.
            let fd =
                unsafe { libc::openat(at.as_raw_fd(), name.as_ptr(), flags | libc::O_NOFOLLOW) };
            if fd < 0 {
                return Err(Self::link_error(io::Error::last_os_error()));
            }
            // SAFETY: a descriptor this call just opened.
            at = unsafe { OwnedFd::from_raw_fd(fd) };
        }
        Ok(NoFollowDir { fd: at })
    }

    fn name(bytes: &[u8]) -> io::Result<std::ffi::CString> {
        std::ffi::CString::new(bytes).map_err(|_| refused("the path contains a NUL byte"))
    }

    /// `ELOOP` (and `EMLINK` on some systems) is `O_NOFOLLOW` meeting a
    /// link; so is `ENOTDIR` where a directory stood when it was judged.
    fn link_error(error: io::Error) -> io::Error {
        match error.raw_os_error() {
            Some(libc::ELOOP) | Some(libc::EMLINK) | Some(libc::ENOTDIR) => refused(
                "the path goes through a symbolic link that was not there when it was checked",
            ),
            _ => error,
        }
    }

    /// Create `name` in the directory for writing: fails if it exists with
    /// `exclusive`, truncates it otherwise. A link by that name is not
    /// followed.
    pub fn create(&self, name: &std::ffi::OsStr, exclusive: bool) -> io::Result<File> {
        use std::os::fd::{AsRawFd, FromRawFd};
        use std::os::unix::ffi::OsStrExt;
        let name = Self::name(name.as_bytes())?;
        let how = if exclusive {
            libc::O_EXCL
        } else {
            libc::O_TRUNC
        };
        let flags = libc::O_WRONLY | libc::O_CREAT | libc::O_NOFOLLOW | libc::O_CLOEXEC | how;
        // SAFETY: the directory is open, `name` a valid C string; checked.
        let fd = unsafe { libc::openat(self.fd.as_raw_fd(), name.as_ptr(), flags, 0o666) };
        if fd < 0 {
            let error = io::Error::last_os_error();
            return Err(match error.raw_os_error() {
                Some(libc::ELOOP) | Some(libc::EMLINK) => Self::link_error(error),
                _ => error,
            });
        }
        // SAFETY: a descriptor this call just opened.
        Ok(unsafe { File::from_raw_fd(fd) })
    }

    /// Rename `from` to `to`, both in this directory; `to` is replaced, a
    /// link by that name included (it is not followed).
    pub fn rename(&self, from: &std::ffi::OsStr, to: &std::ffi::OsStr) -> io::Result<()> {
        use std::os::fd::AsRawFd;
        use std::os::unix::ffi::OsStrExt;
        let (from, to) = (Self::name(from.as_bytes())?, Self::name(to.as_bytes())?);
        let fd = self.fd.as_raw_fd();
        // SAFETY: the directory is open, the names valid C strings.
        match unsafe { libc::renameat(fd, from.as_ptr(), fd, to.as_ptr()) } {
            0 => Ok(()),
            _ => Err(io::Error::last_os_error()),
        }
    }

    /// The size of `name` in this directory as the directory lists it — a
    /// link's own, never what it points to — or `None` when there is no
    /// such name.
    pub fn len_of(&self, name: &std::ffi::OsStr) -> io::Result<Option<u64>> {
        use std::os::fd::AsRawFd;
        use std::os::unix::ffi::OsStrExt;
        let name = Self::name(name.as_bytes())?;
        let mut stat = std::mem::MaybeUninit::<libc::stat>::uninit();
        // SAFETY: the directory is open, `name` a valid C string, `stat`
        // room for one `struct stat`; the result is checked before it is
        // read.
        let status = unsafe {
            libc::fstatat(
                self.fd.as_raw_fd(),
                name.as_ptr(),
                stat.as_mut_ptr(),
                libc::AT_SYMLINK_NOFOLLOW,
            )
        };
        if status != 0 {
            let error = io::Error::last_os_error();
            return match error.kind() {
                io::ErrorKind::NotFound => Ok(None),
                _ => Err(error),
            };
        }
        // SAFETY: `fstatat` succeeded and filled it.
        let stat = unsafe { stat.assume_init() };
        Ok(Some(u64::try_from(stat.st_size).unwrap_or(0)))
    }

    /// Remove the file `name` from this directory (a link itself, never
    /// what it points to).
    pub fn remove_file(&self, name: &std::ffi::OsStr) -> io::Result<()> {
        use std::os::fd::AsRawFd;
        use std::os::unix::ffi::OsStrExt;
        let name = Self::name(name.as_bytes())?;
        // SAFETY: the directory is open, `name` a valid C string.
        match unsafe { libc::unlinkat(self.fd.as_raw_fd(), name.as_ptr(), 0) } {
            0 => Ok(()),
            _ => Err(io::Error::last_os_error()),
        }
    }
}

/// Without `openat`: the same operations by name, each behind
/// [`by_name::unchanged`].
#[cfg(not(unix))]
impl NoFollowDir {
    pub fn open(dir: &Path, create: bool) -> io::Result<NoFollowDir> {
        by_name::open(dir, create)?;
        Ok(NoFollowDir {
            path: dir.to_path_buf(),
        })
    }

    pub fn create(&self, name: &std::ffi::OsStr, exclusive: bool) -> io::Result<File> {
        by_name::create(&self.path, name, exclusive)
    }

    pub fn rename(&self, from: &std::ffi::OsStr, to: &std::ffi::OsStr) -> io::Result<()> {
        by_name::rename(&self.path, from, to)
    }

    pub fn remove_file(&self, name: &std::ffi::OsStr) -> io::Result<()> {
        by_name::remove_file(&self.path, name)
    }

    pub fn len_of(&self, name: &std::ffi::OsStr) -> io::Result<Option<u64>> {
        by_name::len_of(&self.path, name)
    }
}

/// [`NoFollowDir`] where there is no `openat` (Windows): the directory is
/// not held by a handle, so every operation is made by name — and
/// immediately before each one the judged path is resolved again
/// (`protect::resolve_path`) and must still be itself. A link swapped in
/// since the judgement — for a directory on the way, or for the file — is
/// then a refusal, as it is with a handle.
///
/// What is left is the instant between that comparison and the operation:
/// narrower than judging once and writing later, not closed. This is the
/// one place that decides it, for every caller of [`NoFollowDir`]
/// (`protect::write_at`, `remove_at`, `open_parent_of`). It is compiled for
/// tests on every platform so the decision is exercised where the tests
/// run.
#[cfg(any(not(unix), test))]
mod by_name {
    use super::refused;
    use std::ffi::OsStr;
    use std::fs::File;
    use std::io;
    use std::path::Path;

    /// `path` — resolved when it was judged — still resolves to itself: no
    /// component of it has become a link since.
    pub(super) fn unchanged(path: &Path) -> io::Result<()> {
        match crate::protect::resolve_path(path) {
            Ok(resolved) if resolved == path => Ok(()),
            _ => Err(refused(
                "the path goes through a symbolic link that was not there when it was checked",
            )),
        }
    }

    pub(super) fn open(dir: &Path, create: bool) -> io::Result<()> {
        if !dir.is_absolute() {
            return Err(refused("the directory must be an absolute path"));
        }
        // Before creating anything through the name, and again after: the
        // creation itself walks the name.
        unchanged(dir)?;
        if create {
            std::fs::create_dir_all(dir)?;
            unchanged(dir)?;
        }
        if !std::fs::metadata(dir)?.is_dir() {
            return Err(io::Error::new(
                io::ErrorKind::NotADirectory,
                "not a directory",
            ));
        }
        Ok(())
    }

    pub(super) fn create(dir: &Path, name: &OsStr, exclusive: bool) -> io::Result<File> {
        // The file too: a link by that name would be written through.
        unchanged(&dir.join(name))?;
        let mut options = std::fs::OpenOptions::new();
        options.write(true);
        if exclusive {
            options.create_new(true);
        } else {
            options.create(true).truncate(true);
        }
        options.open(dir.join(name))
    }

    /// The destination is replaced, not followed, and the source is the
    /// caller's own temporary file: the directory is what must not have
    /// moved.
    pub(super) fn rename(dir: &Path, from: &OsStr, to: &OsStr) -> io::Result<()> {
        unchanged(dir)?;
        std::fs::rename(dir.join(from), dir.join(to))
    }

    /// Removing a link removes the link.
    pub(super) fn remove_file(dir: &Path, name: &OsStr) -> io::Result<()> {
        unchanged(dir)?;
        std::fs::remove_file(dir.join(name))
    }

    /// The name's own size, a link's included; `None` when there is none.
    #[cfg_attr(unix, allow(dead_code))]
    pub(super) fn len_of(dir: &Path, name: &OsStr) -> io::Result<Option<u64>> {
        unchanged(dir)?;
        match std::fs::symlink_metadata(dir.join(name)) {
            Ok(meta) => Ok(Some(meta.len())),
            Err(err) if err.kind() == io::ErrorKind::NotFound => Ok(None),
            Err(err) => Err(err),
        }
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

    /// The write side: every component is opened without following a
    /// link, so a link standing where a directory or the file was judged to
    /// be is a refusal, not a redirection.
    #[cfg(unix)]
    #[test]
    fn a_write_through_the_handle_follows_no_link() {
        use std::os::unix::fs::symlink;
        let dir = tempfile::tempdir().unwrap();
        let root = std::fs::canonicalize(dir.path()).unwrap();
        std::fs::create_dir_all(root.join("elsewhere")).unwrap();
        std::fs::write(root.join("elsewhere/target.txt"), "theirs").unwrap();

        // Plain: directories are created, the file written, then removed.
        write_no_follow(&root.join("a/b/new.txt"), b"one").unwrap();
        write_no_follow(&root.join("a/b/new.txt"), b"2").unwrap();
        assert_eq!(
            std::fs::read_to_string(root.join("a/b/new.txt")).unwrap(),
            "2"
        );
        remove_no_follow(&root.join("a/b/new.txt")).unwrap();
        assert!(!root.join("a/b/new.txt").exists());

        // A link as a directory on the way, and as the last component.
        symlink(root.join("elsewhere"), root.join("a/dir-link")).unwrap();
        symlink(root.join("elsewhere/target.txt"), root.join("a/file-link")).unwrap();
        for landing in ["a/dir-link/x.txt", "a/dir-link/sub/x.txt", "a/file-link"] {
            let error = write_no_follow(&root.join(landing), b"mine").unwrap_err();
            assert_eq!(error.kind(), io::ErrorKind::PermissionDenied, "{landing}");
        }
        assert!(!root.join("elsewhere/x.txt").exists());
        assert!(!root.join("elsewhere/sub").exists());
        assert_eq!(
            std::fs::read_to_string(root.join("elsewhere/target.txt")).unwrap(),
            "theirs"
        );
        // Removing a link removes the link.
        remove_no_follow(&root.join("a/file-link")).unwrap();
        assert!(root.join("elsewhere/target.txt").exists());
        assert!(remove_no_follow(&root.join("a/dir-link/target.txt")).is_err());
        assert!(root.join("elsewhere/target.txt").exists());
        // Not a resolved absolute path.
        assert!(write_no_follow(Path::new("relative.txt"), b"x").is_err());
        assert!(write_no_follow(&root.join("a/../x.txt"), b"x").is_err());
    }

    /// The name-based half (platforms without `openat`): the judged path
    /// is resolved again immediately before each operation, so a link
    /// swapped in since is refused there too.
    #[cfg(unix)]
    #[test]
    fn the_by_name_fallback_refuses_a_path_that_changed_since_it_was_judged() {
        use std::ffi::OsStr;
        use std::os::unix::fs::symlink;
        let dir = tempfile::tempdir().unwrap();
        let root = std::fs::canonicalize(dir.path()).unwrap();
        std::fs::create_dir_all(root.join("elsewhere")).unwrap();
        std::fs::write(root.join("elsewhere/target.txt"), "theirs").unwrap();
        let denied = |result: io::Result<()>| {
            assert_eq!(result.unwrap_err().kind(), io::ErrorKind::PermissionDenied);
        };

        // Unchanged: directories are made, files created, renamed, removed.
        let work = root.join("work/deep");
        by_name::open(&work, true).unwrap();
        drop(by_name::create(&work, OsStr::new("tmp"), true).unwrap());
        by_name::rename(&work, OsStr::new("tmp"), OsStr::new("a.txt")).unwrap();
        assert!(work.join("a.txt").is_file());
        by_name::remove_file(&work, OsStr::new("a.txt")).unwrap();
        assert!(by_name::open(&root.join("missing"), false).is_err());
        assert!(by_name::open(Path::new("relative"), true).is_err());

        // The file's name became a link after the directory was opened.
        symlink(root.join("elsewhere/target.txt"), work.join("note.txt")).unwrap();
        denied(by_name::create(&work, OsStr::new("note.txt"), false).map(drop));
        assert_eq!(
            std::fs::read_to_string(root.join("elsewhere/target.txt")).unwrap(),
            "theirs"
        );

        // A directory on the way became a link: every operation refuses.
        std::fs::rename(root.join("work"), root.join("work-was")).unwrap();
        symlink(root.join("elsewhere"), root.join("work")).unwrap();
        denied(by_name::open(&work, true));
        denied(by_name::open(&root.join("work"), false));
        denied(by_name::create(&root.join("work"), OsStr::new("x.txt"), true).map(drop));
        denied(by_name::rename(
            &root.join("work"),
            OsStr::new("target.txt"),
            OsStr::new("moved.txt"),
        ));
        denied(by_name::remove_file(
            &root.join("work"),
            OsStr::new("target.txt"),
        ));
        assert!(!root.join("elsewhere/deep").exists());
        assert!(!root.join("elsewhere/x.txt").exists());
        assert!(root.join("elsewhere/target.txt").exists());
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
