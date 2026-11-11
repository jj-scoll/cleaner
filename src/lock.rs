//! One cleaner run at a time (spec §7.2): an exclusive `flock` on the lock file.

use anyhow::{Context, bail};
use rustix::fs::{FlockOperation, flock};
use std::fs::{File, OpenOptions};
use std::path::Path;

/// Held for as long as this value lives; the kernel drops the lock when the
/// file is closed, including when the process dies.
pub struct RunLock {
    file: File,
}

/// Unlocks explicitly: a child forked by another thread holds a copy of the
/// fd until it execs, and the flock lives until every copy is closed, so
/// closing ours alone could leave the lock held after the run ends.
impl Drop for RunLock {
    fn drop(&mut self) {
        let _ = flock(&self.file, FlockOperation::Unlock);
    }
}

/// Takes the lock without blocking. Fails with "another cleaner run is in
/// progress" if another open file (in any process) holds it.
pub fn acquire(path: &Path) -> anyhow::Result<RunLock> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).with_context(|| format!("creating {}", parent.display()))?;
    }
    let file = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(path)
        .with_context(|| format!("opening lock {}", path.display()))?;
    match flock(&file, FlockOperation::NonBlockingLockExclusive) {
        Ok(()) => Ok(RunLock { file }),
        Err(rustix::io::Errno::WOULDBLOCK) => bail!("another cleaner run is in progress"),
        Err(e) => Err(e).with_context(|| format!("locking {}", path.display())),
    }
}
