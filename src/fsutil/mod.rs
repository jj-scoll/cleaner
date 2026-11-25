//! Filesystem primitives shared by every category.

pub mod delete;
pub mod measure;
pub mod usage;

use std::sync::atomic::{AtomicU64, Ordering};

static DIRS_READ: AtomicU64 = AtomicU64::new(0);

/// Directories the scan walkers have read since the process started. The
/// GUI shows the difference since a scan began as "N folders checked".
pub fn dirs_read() -> u64 {
    DIRS_READ.load(Ordering::Relaxed)
}

/// `std::fs::read_dir`, counted in [`dirs_read`].
pub fn read_dir(path: &std::path::Path) -> std::io::Result<std::fs::ReadDir> {
    DIRS_READ.fetch_add(1, Ordering::Relaxed);
    std::fs::read_dir(path)
}
