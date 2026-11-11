//! Disk usage index (spec §5.4): a pruned size tree per root, built once per
//! plan with lstat only and kept in `usage.json`.
//!
//! Byte accounting is the same as [`crate::fsutil::measure::measure`]: only
//! non-directories count, at `st_blocks * 512`; a directory's own blocks are
//! not counted; a hardlinked inode counts once per tree. So a root's `bytes`
//! equals `measure(root).bytes`.

use anyhow::Context;
use serde::{Deserialize, Serialize};
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};

use crate::config::Config;
use crate::fsutil::measure::InodeSet;

/// Smallest child kept in the tree (DECISION P1).
pub const MIN_NODE: u64 = 50 << 20;
/// Paths the index never enters.
pub const EXCLUDE: &[&str] = &["/nix"];

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Default)]
pub struct Node {
    #[serde(with = "crate::plan::path_codec")]
    pub name: PathBuf,
    pub bytes: u64,
    pub newest_mtime: i64,
    pub dir: bool,
    pub unreadable_owner: Option<u32>,
    pub other_bytes: u64,
    pub other_count: u64,
    pub children: Vec<Node>,
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct Tree {
    #[serde(with = "crate::plan::path_codec")]
    pub root: PathBuf,
    pub dev: u64,
    pub fs_total: u64,
    pub fs_used: u64,
    pub node: Node,
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Default)]
pub struct UsageIndex {
    pub created: i64,
    pub min_node: u64,
    pub trees: Vec<Tree>,
}

/// `expand_roots(disk_map.roots ∪ scan_roots)`.
pub fn index_roots(config: &Config, home: &Path) -> Vec<PathBuf> {
    let all: Vec<String> =
        config.disk_map.roots.iter().chain(&config.scan_roots).cloned().collect();
    crate::paths::expand_roots(&all, home)
}

/// One `Tree` per root; missing, symlinked and excluded roots are left out.
/// lstat only; stays on each root's `st_dev`; never enters symlinks or
/// `exclude`d paths. A child (dir or file) is kept in `children` when its bytes
/// are at least `min_node` or it is an unreadable dir (`bytes` 0); the rest add
/// to `other_bytes`/`other_count`. Post-order, so memory holds kept nodes only.
pub fn build(roots: &[PathBuf], exclude: &[PathBuf], min_node: u64, now: i64) -> UsageIndex {
    let mut trees = Vec::new();
    for root in roots {
        if exclude.iter().any(|x| root.starts_with(x)) {
            continue;
        }
        let Ok(md) = std::fs::symlink_metadata(root) else { continue };
        if !md.is_dir() {
            continue; // a symlink, or a plain file: not a root
        }
        let mut w = Walk { dev: md.dev(), exclude, min_node, seen: InodeSet::default() };
        let name = root.file_name().map(PathBuf::from).unwrap_or_else(|| root.clone());
        let node = w.dir(root, name, &md);
        let (fs_total, fs_used) = match rustix::fs::statvfs(root) {
            Ok(v) => (
                v.f_blocks.saturating_mul(v.f_frsize),
                v.f_blocks.saturating_sub(v.f_bfree).saturating_mul(v.f_frsize),
            ),
            Err(_) => (0, 0),
        };
        trees.push(Tree { root: root.clone(), dev: md.dev(), fs_total, fs_used, node });
    }
    UsageIndex { created: now, min_node, trees }
}

struct Walk<'a> {
    dev: u64,
    exclude: &'a [PathBuf],
    min_node: u64,
    seen: InodeSet,
}

impl Walk<'_> {
    fn dir(&mut self, path: &Path, name: PathBuf, md: &std::fs::Metadata) -> Node {
        let mut node = Node { name, dir: true, newest_mtime: md.mtime(), ..Node::default() };
        let Ok(rd) = std::fs::read_dir(path) else {
            node.unreadable_owner = Some(md.uid());
            return node;
        };
        for entry in rd.flatten() {
            // DirEntry::metadata is an lstat: it never follows symlinks.
            let Ok(cmd) = entry.metadata() else { continue };
            if cmd.dev() != self.dev {
                continue;
            }
            let cp = entry.path();
            if self.exclude.contains(&cp) {
                continue;
            }
            let name = PathBuf::from(entry.file_name());
            let child = if cmd.is_dir() {
                self.dir(&cp, name, &cmd)
            } else {
                // Only inodes with other links can repeat; the set stays small.
                let first = cmd.nlink() == 1 || self.seen.insert(cmd.dev(), cmd.ino());
                let bytes = if first { cmd.blocks() * 512 } else { 0 };
                Node { name, bytes, newest_mtime: cmd.mtime(), ..Node::default() }
            };
            node.bytes += child.bytes;
            node.newest_mtime = node.newest_mtime.max(child.newest_mtime);
            if child.bytes >= self.min_node || child.unreadable_owner.is_some() {
                node.children.push(child);
            } else {
                node.other_bytes += child.bytes;
                node.other_count += 1;
            }
        }
        node.children.sort_by(|a, b| b.bytes.cmp(&a.bytes).then_with(|| a.name.cmp(&b.name)));
        node
    }
}

impl UsageIndex {
    /// Writes `path` via a temp file and a rename.
    pub fn save(&self, path: &Path) -> anyhow::Result<()> {
        crate::timer::write_atomic(path, &serde_json::to_string(self)?)
    }

    /// `Ok(None)` if `path` does not exist.
    pub fn load(path: &Path) -> anyhow::Result<Option<UsageIndex>> {
        let text = match std::fs::read_to_string(path) {
            Ok(t) => t,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(e) => return Err(e).with_context(|| format!("reading {}", path.display())),
        };
        let idx = serde_json::from_str(&text)
            .with_context(|| format!("parsing {}", path.display()))?;
        Ok(Some(idx))
    }

    /// The tree and node for `path` (a tree root or a kept descendant).
    pub fn node(&self, path: &Path) -> Option<(&Tree, &Node)> {
        self.trees.iter().find_map(|t| {
            let rel = path.strip_prefix(&t.root).ok()?;
            let mut n = &t.node;
            for c in rel.components() {
                n = n.children.iter().find(|k| k.name.as_os_str() == c.as_os_str())?;
            }
            Some((t, n))
        })
    }

    /// Pre-order over every kept node of every tree, with its full path.
    pub fn visit(&self, f: &mut dyn FnMut(&Path, &Node)) {
        fn go(path: &mut PathBuf, n: &Node, f: &mut dyn FnMut(&Path, &Node)) {
            f(path, n);
            for c in &n.children {
                path.push(&c.name);
                go(path, c, f);
                path.pop();
            }
        }
        for t in &self.trees {
            go(&mut t.root.clone(), &t.node, f);
        }
    }
}
