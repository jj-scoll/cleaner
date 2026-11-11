//! Symlink-safe, mount-bounded recursive delete (spec §6.1, §6.2).
//!
//! Built on `openat`/`unlinkat` with `O_NOFOLLOW`, so a symlink anywhere in
//! the tree is unlinked, never followed, and every directory's `st_dev` is
//! checked before anything in it is deleted.

use rustix::fd::{AsFd, BorrowedFd, OwnedFd};
use rustix::fs::{AtFlags, Dir, FileType, Mode, OFlags, Stat, fchmod, fstat, openat, statat, unlinkat};
use rustix::io::Errno;
use std::ffi::{CStr, OsStr};
use std::path::{Path, PathBuf};

#[derive(Debug, thiserror::Error)]
pub enum DeleteError {
    #[error("{0}")]
    Io(#[from] std::io::Error),
    #[error("crosses a mount point at {0}")]
    CrossesMount(PathBuf),
}

impl From<Errno> for DeleteError {
    fn from(e: Errno) -> Self {
        DeleteError::Io(e.into())
    }
}

const DIR_FLAGS: OFlags = OFlags::DIRECTORY
    .union(OFlags::NOFOLLOW)
    .union(OFlags::CLOEXEC)
    .union(OFlags::RDONLY);

/// Deletes `path` (file, symlink or directory tree) without following
/// symlinks or leaving `path`'s filesystem. Returns the bytes actually freed:
/// `st_blocks * 512` of unlinked non-directories whose link count was 1.
pub fn delete_tree(path: &Path) -> Result<u64, DeleteError> {
    let st = std::fs::symlink_metadata(path)?;
    delete_tree_on_dev(path, std::os::unix::fs::MetadataExt::dev(&st))
}

/// Like [`delete_tree`] but every directory, including `path`'s parent, must
/// be on device `dev`.
#[doc(hidden)]
pub fn delete_tree_on_dev(path: &Path, dev: u64) -> Result<u64, DeleteError> {
    let name = path.file_name().ok_or_else(|| {
        std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            format!("no file name to delete in {}", path.display()),
        )
    })?;
    let parent = match path.parent() {
        Some(p) if !p.as_os_str().is_empty() => p,
        _ => Path::new("."),
    };
    let parent_fd = openat(rustix::fs::CWD, parent, DIR_FLAGS, Mode::empty())?;
    // A `path` on another device than its parent is a mount point: deleting
    // "it" would empty the filesystem mounted there.
    #[allow(clippy::unnecessary_cast)]
    if fstat(&parent_fd)?.st_dev as u64 != dev {
        return Err(DeleteError::CrossesMount(path.to_path_buf()));
    }
    // `path.parent()` lies outside the tree we were asked to delete: never chmod it.
    delete_entry(parent_fd.as_fd(), name, path, dev, false)
}

/// Deletes `name` inside `dir_fd`. `may_chmod_dir` says whether `dir_fd` is
/// inside the tree being deleted (and so may be made writable to proceed).
fn delete_entry<N: rustix::path::Arg + Copy>(
    dir_fd: BorrowedFd<'_>,
    name: N,
    full: &Path,
    dev: u64,
    may_chmod_dir: bool,
) -> Result<u64, DeleteError> {
    let st = statat(dir_fd, name, AtFlags::SYMLINK_NOFOLLOW)?;
    if FileType::from_raw_mode(st.st_mode) != FileType::Directory {
        unlink_in(dir_fd, name, AtFlags::empty(), may_chmod_dir)?;
        return Ok(freed_bytes(&st));
    }

    let fd: OwnedFd = openat(dir_fd, name, DIR_FLAGS, Mode::empty())?;
    let own = fstat(&fd)?;
    #[allow(clippy::unnecessary_cast)]
    if own.st_dev as u64 != dev {
        return Err(DeleteError::CrossesMount(full.to_path_buf()));
    }

    // Collect names first rather than unlinking while reading the directory.
    let mut names: Vec<std::ffi::CString> = Vec::new();
    let mut dir = Dir::read_from(&fd)?;
    while let Some(entry) = dir.read() {
        let entry = entry?;
        let n = entry.file_name();
        if n != c"." && n != c".." {
            names.push(n.to_owned());
        }
    }
    drop(dir);

    let mut freed = 0;
    for n in &names {
        let child = full.join(cstr_os(n));
        freed += delete_entry(fd.as_fd(), n.as_c_str(), &child, dev, true)?;
    }
    drop(fd);
    unlink_in(dir_fd, name, AtFlags::REMOVEDIR, may_chmod_dir)?;
    Ok(freed)
}

/// `unlinkat`, retrying once after `fchmod(dir, mode | 0o700)` when the
/// containing directory is read-only and lies inside the tree.
fn unlink_in<N: rustix::path::Arg + Copy>(
    dir_fd: BorrowedFd<'_>,
    name: N,
    flags: AtFlags,
    may_chmod_dir: bool,
) -> Result<(), DeleteError> {
    match unlinkat(dir_fd, name, flags) {
        Err(Errno::ACCESS | Errno::PERM) if may_chmod_dir => {
            let st = fstat(dir_fd)?;
            fchmod(dir_fd, Mode::from_raw_mode(st.st_mode) | Mode::RWXU)?;
            unlinkat(dir_fd, name, flags)?;
            Ok(())
        }
        r => Ok(r?),
    }
}

#[allow(clippy::unnecessary_cast)]
fn freed_bytes(st: &Stat) -> u64 {
    if st.st_nlink as u64 == 1 { st.st_blocks as u64 * 512 } else { 0 }
}

fn cstr_os(c: &CStr) -> &OsStr {
    use std::os::unix::ffi::OsStrExt;
    OsStr::from_bytes(c.to_bytes())
}
