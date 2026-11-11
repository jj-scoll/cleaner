//! The disk map as plain data (spec §7.2): one level of the saved usage index
//! at a time, sorted, with plan badges. No GTK.

use jiff::tz::TimeZone;
use std::path::{Path, PathBuf};

use crate::fmt::{human_bytes, local_time};
use crate::fsutil::usage::{Node, Tree, UsageIndex};
use crate::object::Object;
use crate::plan::{EntryKind, Plan};

#[derive(Clone, Debug, PartialEq)]
pub enum RowKind {
    Dir { drillable: bool },
    File,
    Other { count: u64 },
    Unreadable { owner: String },
}

#[derive(Clone, Debug, PartialEq)]
pub struct Row {
    pub name: String,
    pub path: Option<PathBuf>,
    pub bytes: u64,
    pub fraction: f64,
    pub kind: RowKind,
    pub badge: Option<String>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Level {
    /// `None` = the roots level.
    pub path: Option<PathBuf>,
    pub title: String,
    pub scanned: String,
    pub rows: Vec<Row>,
    /// `(root, bytes the filesystem uses that the index does not account for)`.
    pub unexplained: Vec<(String, u64)>,
}

const TITLE: &str = "What's using my disk";

/// `scanned {local_time(created)} · {age}`; age = `under 1 h ago` | `{h} h ago` | `{d} d ago`.
pub fn scanned_text(created: i64, now: i64, tz: &TimeZone) -> String {
    let hours = (now - created).max(0) / 3600;
    let age = match hours {
        0 => "under 1 h ago".to_string(),
        1..=23 => format!("{hours} h ago"),
        _ => format!("{} d ago", hours / 24),
    };
    format!("scanned {} · {age}", local_time(created, tz))
}

/// Top level: one row per tree (name = root path), fraction against the largest tree.
pub fn roots(index: &UsageIndex, plan: Option<&Plan>, now: i64, tz: &TimeZone) -> Level {
    let largest = index.trees.iter().map(|t| t.node.bytes).max().unwrap_or(0);
    let rows = index
        .trees
        .iter()
        .map(|t| row(t.root.display().to_string(), t.root.clone(), &t.node, largest, plan))
        .collect();
    Level {
        path: None,
        title: TITLE.into(),
        scanned: scanned_text(index.created, now, tz),
        rows,
        unexplained: unexplained(index),
    }
}

/// Children of `path`, biggest first, then unreadable rows, then `other (n items)`.
/// `None` if `path` isn't in the index. The footer figure is the same at every
/// level: how much of each filesystem the index doesn't account for.
pub fn level(index: &UsageIndex, path: &Path, plan: Option<&Plan>, now: i64, tz: &TimeZone) -> Option<Level> {
    let (_, node) = index.node(path)?;
    let mut sized = Vec::new();
    let mut unreadable = Vec::new();
    for c in &node.children {
        let r = row(c.name.display().to_string(), path.join(&c.name), c, node.bytes, plan);
        if matches!(r.kind, RowKind::Unreadable { .. }) { unreadable.push(r) } else { sized.push(r) }
    }
    sized.sort_by(|a, b| b.bytes.cmp(&a.bytes).then_with(|| a.name.cmp(&b.name)));
    let mut rows = sized;
    rows.extend(unreadable);
    if node.other_count > 0 {
        rows.push(Row {
            name: format!("other ({} items)", node.other_count),
            path: None,
            bytes: node.other_bytes,
            fraction: fraction(node.other_bytes, node.bytes),
            kind: RowKind::Other { count: node.other_count },
            badge: None,
        });
    }
    Some(Level {
        path: Some(path.to_path_buf()),
        title: path.display().to_string(),
        scanned: scanned_text(index.created, now, tz),
        rows,
        unexplained: unexplained(index),
    })
}

/// Per filesystem (spec §7.2): used bytes minus every indexed tree on it,
/// labelled by its mount point. Fully explained filesystems are left out.
fn unexplained(index: &UsageIndex) -> Vec<(String, u64)> {
    let mut fs: Vec<(u64, &Tree, u64)> = Vec::new(); // (dev, first tree, summed bytes)
    for t in &index.trees {
        match fs.iter_mut().find(|(d, ..)| *d == t.dev) {
            Some(f) => f.2 = f.2.saturating_add(t.node.bytes),
            None => fs.push((t.dev, t, t.node.bytes)),
        }
    }
    fs.into_iter()
        .filter_map(|(_, t, bytes)| {
            let left = t.fs_used.saturating_sub(bytes);
            let label = crate::disk::mount_point(&t.root).unwrap_or_else(|| t.root.clone());
            (left > 0).then(|| (label.display().to_string(), left))
        })
        .collect()
}

fn fraction(bytes: u64, whole: u64) -> f64 {
    if whole == 0 { 0.0 } else { bytes as f64 / whole as f64 }
}

fn row(name: String, path: PathBuf, n: &Node, whole: u64, plan: Option<&Plan>) -> Row {
    let kind = match n.unreadable_owner {
        Some(0) => RowKind::Unreadable { owner: "root".into() },
        Some(uid) => RowKind::Unreadable { owner: format!("uid {uid}") },
        None if n.dir => RowKind::Dir { drillable: !n.children.is_empty() },
        None => RowKind::File,
    };
    let badge = plan.and_then(|p| plan_badge(p, &path));
    Row { name, bytes: n.bytes, fraction: fraction(n.bytes, whole), kind, badge, path: Some(path) }
}

/// Entries (not report-only) at or under `path`: Item targets, a StoreRule's
/// root, and the path of a Worktree, OrphanWorktree or Symlink object. Other
/// entries are ignored. `None` when there are none.
pub fn plan_badge(plan: &Plan, path: &Path) -> Option<String> {
    let under = |p: &Path| p.starts_with(path);
    let (mut rows, mut bytes) = (0u64, 0u64);
    for e in &plan.entries {
        let hit = match &e.kind {
            EntryKind::Item { targets } => targets.iter().any(|t| under(&t.path)),
            EntryKind::StoreRule { rule } => under(&rule.root),
            EntryKind::Object { obj } => match obj {
                Object::Worktree(w) => under(&w.path),
                Object::OrphanWorktree { path, .. } | Object::Symlink { path, .. } => under(path),
                _ => false,
            },
            EntryKind::Operation { .. } => false,
        };
        if hit {
            rows += 1;
            bytes += e.bytes;
        }
    }
    (rows > 0).then(|| format!("in plan: {rows} row{} · {}", if rows == 1 { "" } else { "s" }, human_bytes(bytes)))
}
