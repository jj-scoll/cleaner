//! A bounded, lstat-only directory walk shared by categories that look for
//! things anywhere under the home dir (worktrees, Nix `result` links).

use std::collections::VecDeque;
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};

use crate::protect::Protector;

/// True if `dir` is at or inside a protected path (see `artifacts::discover`:
/// a NUL component can't be part of any protected root, so this skips the
/// ancestors of protected roots, such as home itself).
fn inside_protected(prot: &Protector, dir: &Path) -> bool {
    prot.is_protected(&dir.join("\0"))
}

/// Calls `visit(dir, depth)` for each root (depth 0) and every directory
/// below it, breadth-first, to `max_depth`, with the same descent rules as
/// `artifacts::discover`: lstat only, stays on the root's filesystem, and
/// never enters symlinks, `.git`, protected paths, dotdirs directly under a
/// root, or dirs where `prune` is true. Unreadable dirs are passed over.
pub fn walk_dirs(
    roots: &[PathBuf],
    max_depth: usize,
    prot: &Protector,
    prune: &dyn Fn(&Path) -> bool,
    visit: &mut dyn FnMut(&Path, usize),
) {
    for root in roots {
        let Ok(root_md) = root.symlink_metadata() else { continue };
        if !root_md.is_dir() || inside_protected(prot, root) {
            continue;
        }
        let dev = root_md.dev();
        let mut queue = VecDeque::from([(root.clone(), 0usize)]);
        while let Some((dir, depth)) = queue.pop_front() {
            visit(&dir, depth);
            if depth >= max_depth {
                continue;
            }
            let Ok(rd) = std::fs::read_dir(&dir) else { continue };
            for entry in rd.flatten() {
                let Ok(md) = entry.metadata() else { continue }; // lstat
                let name = entry.file_name();
                let path = entry.path();
                let descend = md.is_dir()
                    && md.dev() == dev
                    && name != ".git"
                    && !(depth == 0 && name.as_encoded_bytes().starts_with(b"."))
                    && !inside_protected(prot, &path)
                    && !prune(&path);
                if descend {
                    queue.push_back((path, depth + 1));
                }
            }
        }
    }
}
