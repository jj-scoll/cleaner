//! Inode-deduplicating size walker and mtime helpers (spec §6.5, §6.6).

use std::collections::HashSet;
use std::io;
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};

/// `(dev, ino)` pairs already counted in this plan.
#[derive(Debug, Default)]
pub struct InodeSet(HashSet<(u64, u64)>);

impl InodeSet {
    /// Records `(dev, ino)`; returns true if it was not seen before.
    pub fn insert(&mut self, dev: u64, ino: u64) -> bool {
        self.0.insert((dev, ino))
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct Measure {
    /// `st_blocks * 512` of non-directories not already in the `InodeSet`.
    pub bytes: u64,
    /// Non-directories seen (symlinks included), whether or not deduplicated.
    pub files: u64,
    /// Max mtime (Unix seconds) over the root and everything under it.
    pub newest_mtime: i64,
    /// Unreadable directories and entries, skipped.
    pub errors: u64,
    /// Inodes (directories too) whose `st_uid` is not the given euid.
    pub foreign: u64,
    /// The uid of the first such inode.
    pub foreign_uid: Option<u32>,
}

/// Measures `path` without following symlinks or leaving its filesystem.
/// A symlink root is `Err(InvalidInput)`; a symlink inside counts as a file
/// of its own lstat size. Unreadable dirs increment `errors` and are skipped.
pub fn measure(path: &Path, seen: &mut InodeSet) -> io::Result<Measure> {
    measure_as(path, seen, rustix::process::geteuid().as_raw())
}

/// As [`measure`], counting inodes not owned by `euid` in `foreign`.
pub fn measure_as(path: &Path, seen: &mut InodeSet, euid: u32) -> io::Result<Measure> {
    let root = std::fs::symlink_metadata(path)?;
    if root.file_type().is_symlink() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("refusing to measure symlink {}", path.display()),
        ));
    }
    let dev = root.dev();
    let mut m = Measure::default();
    let mut stack: Vec<(PathBuf, std::fs::Metadata)> = vec![(path.to_path_buf(), root)];
    while let Some((p, md)) = stack.pop() {
        m.newest_mtime = m.newest_mtime.max(md.mtime());
        if md.uid() != euid {
            m.foreign += 1;
            m.foreign_uid.get_or_insert(md.uid());
        }
        if !md.is_dir() {
            m.files += 1;
            if seen.insert(md.dev(), md.ino()) {
                m.bytes += md.blocks() * 512;
            }
            continue;
        }
        let Ok(rd) = std::fs::read_dir(&p) else {
            m.errors += 1;
            continue;
        };
        for entry in rd {
            // DirEntry::metadata is an lstat: it never follows symlinks.
            match entry.and_then(|e| Ok((e.path(), e.metadata()?))) {
                Ok((cp, cmd)) if cmd.dev() == dev => stack.push((cp, cmd)),
                Ok(_) => {} // another filesystem mounted here: not ours to count
                Err(_) => m.errors += 1,
            }
        }
    }
    Ok(m)
}

/// Max mtime of files under `root`, not descending into symlinks or into
/// directories where `skip(path)` is true. 0 if there are none.
pub fn newest_mtime_excluding(root: &Path, skip: &dyn Fn(&Path) -> bool) -> i64 {
    let mut newest = 0;
    let mut stack = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        let Ok(rd) = std::fs::read_dir(&dir) else { continue };
        for entry in rd.flatten() {
            let Ok(ft) = entry.file_type() else { continue };
            let p = entry.path();
            if ft.is_dir() {
                if !skip(&p) {
                    stack.push(p);
                }
            } else if !ft.is_symlink()
                && let Ok(md) = entry.metadata()
            {
                newest = newest.max(md.mtime());
            }
        }
    }
    newest
}
