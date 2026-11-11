//! Filesystem usage via `statvfs`, used for status and "freed" (spec §6.6).

use serde::Serialize;
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};

#[derive(Serialize, Clone, Debug, PartialEq)]
pub struct FsUsage {
    pub label: String,
    pub total: u64,
    pub avail: u64,
}

/// One row per distinct filesystem among `paths` (deduped by `st_dev`,
/// keeping the first path as the label). Paths that can't be stat'ed are left out.
pub fn usage(paths: &[PathBuf]) -> Vec<FsUsage> {
    let mut seen = Vec::new();
    let mut out = Vec::new();
    for p in paths {
        let Ok(md) = std::fs::metadata(p) else { continue };
        if seen.contains(&md.dev()) {
            continue;
        }
        let Ok(vfs) = rustix::fs::statvfs(p) else { continue };
        seen.push(md.dev());
        out.push(FsUsage {
            label: p.display().to_string(),
            total: vfs.f_blocks.saturating_mul(vfs.f_frsize),
            avail: vfs.f_bavail.saturating_mul(vfs.f_frsize),
        });
    }
    out
}

/// The highest ancestor of `p` (`p` included) with the same `st_dev`; `/` at
/// most. `None` if `p` can't be stat'ed.
pub fn mount_point(p: &Path) -> Option<PathBuf> {
    let dev = std::fs::metadata(p).ok()?.dev();
    let mut best = p.to_path_buf();
    for a in p.ancestors().skip(1) {
        match std::fs::metadata(a) {
            Ok(md) if md.dev() == dev => best = a.to_path_buf(),
            _ => break,
        }
    }
    Some(best)
}
