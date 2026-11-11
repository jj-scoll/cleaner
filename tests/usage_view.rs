//! The disk map's data model (spec §7.2): pure, no GTK.

use cleaner::category::store::{StoreRule, Unit};
use cleaner::fsutil::usage::{Node, Tree, UsageIndex};
use cleaner::plan::{Entry, EntryKind, Fingerprint, Plan, Target};
use cleaner::view::usage::{RowKind, level, plan_badge, roots, scanned_text};
use jiff::tz::TimeZone;
use std::path::{Path, PathBuf};

const NOW: i64 = 1_794_366_000; // 2026-11-11 03:00 UTC

fn dir(name: &str, bytes: u64, children: Vec<Node>) -> Node {
    Node { name: name.into(), bytes, dir: true, children, ..Node::default() }
}

fn file(name: &str, bytes: u64) -> Node {
    Node { name: name.into(), bytes, ..Node::default() }
}

fn index(node: Node, fs_used: u64) -> UsageIndex {
    UsageIndex {
        created: NOW,
        min_node: 1,
        trees: vec![Tree { root: "/r".into(), dev: 1, fs_total: 10_000, fs_used, node }],
    }
}

fn entry(kind: EntryKind, bytes: u64) -> Entry {
    Entry {
        id: String::new(),
        category: "caches".into(),
        title: "t".into(),
        kind,
        bytes,
        reason: "r".into(),
        selected: true,
        permanent: false,
        note: None,
        last_active: None,
    }
}

fn item(path: &str, bytes: u64) -> Entry {
    let fp = Fingerprint { dev: 1, ino: 2, newest_mtime: 3 };
    entry(EntryKind::Item { targets: vec![Target { path: path.into(), fp }] }, bytes)
}

fn store_rule(root: &str, bytes: u64) -> Entry {
    let rule = StoreRule { root: root.into(), max_age_days: 30, unit: Unit::Depth(1), glob: None, min_bytes: None };
    entry(EntryKind::StoreRule { rule }, bytes)
}

fn plan(entries: Vec<Entry>, report_only: Vec<Entry>) -> Plan {
    Plan { id: "p".into(), created: NOW, entries, skipped: vec![], report_only }
}

fn names(l: &cleaner::view::usage::Level) -> Vec<&str> {
    l.rows.iter().map(|r| r.name.as_str()).collect()
}

#[test]
fn level_sorts_and_fractions() {
    let root = dir("r", 100, vec![dir("a", 30, vec![]), dir("b", 10, vec![]), dir("c", 60, vec![])]);
    let idx = index(root, 100);
    let l = level(&idx, Path::new("/r"), None, NOW, &TimeZone::UTC).unwrap();
    assert_eq!(names(&l), ["c", "a", "b"]);
    let f: Vec<f64> = l.rows.iter().map(|r| r.fraction).collect();
    assert_eq!(f, [0.6, 0.3, 0.1]);
    assert_eq!(l.rows[0].path, Some(PathBuf::from("/r/c")));
    assert_eq!(l.path, Some(PathBuf::from("/r")));
}

#[test]
fn other_row_last() {
    let mut root = dir("r", 105, vec![dir("a", 100, vec![])]);
    root.other_bytes = 5;
    root.other_count = 4;
    let l = level(&index(root, 105), Path::new("/r"), None, NOW, &TimeZone::UTC).unwrap();
    let last = l.rows.last().unwrap();
    assert_eq!(last.kind, RowKind::Other { count: 4 });
    assert_eq!(last.name, "other (4 items)");
    assert_eq!(last.bytes, 5);
    assert_eq!(last.path, None);
}

#[test]
fn unreadable_row_owner_root() {
    let locked = |uid| Node { unreadable_owner: Some(uid), ..dir("locked", 0, vec![]) };
    let mut root = dir("r", 10, vec![locked(0), dir("a", 10, vec![])]);
    root.other_bytes = 0;
    let l = level(&index(root, 10), Path::new("/r"), None, NOW, &TimeZone::UTC).unwrap();
    assert_eq!(names(&l), ["a", "locked"]);
    assert_eq!(l.rows[1].kind, RowKind::Unreadable { owner: "root".into() });

    // A directory owned by someone else: the Global copy reads `uid {n}`.
    let euid = rustix::process::geteuid().as_raw();
    let root = dir("r", 0, vec![locked(euid + 1)]);
    let l = level(&index(root, 0), Path::new("/r"), None, NOW, &TimeZone::UTC).unwrap();
    assert_eq!(l.rows[0].kind, RowKind::Unreadable { owner: format!("uid {}", euid + 1) });
    assert_eq!(l.rows[0].name, "locked");
}

#[test]
fn drillable_only_with_children() {
    let root = dir("r", 100, vec![dir("full", 60, vec![file("f", 60)]), dir("bare", 40, vec![])]);
    let l = level(&index(root, 100), Path::new("/r"), None, NOW, &TimeZone::UTC).unwrap();
    assert_eq!(l.rows[0].kind, RowKind::Dir { drillable: true });
    assert_eq!(l.rows[1].kind, RowKind::Dir { drillable: false });
    let l = level(&index(dir("r", 100, vec![dir("full", 60, vec![file("f", 60)])]), 100), Path::new("/r/full"), None, NOW, &TimeZone::UTC).unwrap();
    assert_eq!(l.rows[0].kind, RowKind::File);
}

#[test]
fn badge_counts_plan_rows_under_path() {
    let p = plan(
        vec![item("/r/a/x", 4096), store_rule("/r/a/store", 8192), item("/r/b/y", 1 << 20)],
        vec![item("/r/a/z", 1 << 30)],
    );
    assert_eq!(plan_badge(&p, Path::new("/r/a")), Some("in plan: 2 rows · 12.0 KiB".into()));
    let one = plan(vec![item("/r/a/x", 4096)], vec![]);
    assert_eq!(plan_badge(&one, Path::new("/r/a")), Some("in plan: 1 row · 4.0 KiB".into()));
    assert_eq!(plan_badge(&one, Path::new("/r/other")), None);
    // The path itself counts; a sibling sharing a name prefix does not.
    assert_eq!(plan_badge(&one, Path::new("/r/a/x")), Some("in plan: 1 row · 4.0 KiB".into()));
    assert_eq!(plan_badge(&one, Path::new("/r/a/xy")), None);
}

#[test]
fn level_rows_carry_badges() {
    let root = dir("r", 100, vec![dir("a", 60, vec![file("x", 60)]), dir("b", 40, vec![])]);
    let p = plan(vec![item("/r/a/x", 4096)], vec![]);
    let l = level(&index(root, 100), Path::new("/r"), Some(&p), NOW, &TimeZone::UTC).unwrap();
    assert_eq!(l.rows[0].badge.as_deref(), Some("in plan: 1 row · 4.0 KiB"));
    assert_eq!(l.rows[1].badge, None);
}

#[test]
fn roots_level_unexplained() {
    let root = dir("r", 400, vec![dir("a", 400, vec![])]);
    let l = roots(&index(root, 1000), None, NOW, &TimeZone::UTC);
    assert_eq!(l.path, None);
    assert_eq!(l.unexplained, [("/r".to_string(), 600)]);
    assert_eq!(names(&l), ["/r"]);
    assert_eq!(l.rows[0].fraction, 1.0);
    assert_eq!(l.rows[0].path, Some(PathBuf::from("/r")));
}

/// The footer shows the filesystem-level figure at every drill level.
#[test]
fn drill_level_keeps_root_unexplained() {
    let root = dir("r", 400, vec![dir("a", 400, vec![file("f", 400)])]);
    let l = level(&index(root, 1000), Path::new("/r/a"), None, NOW, &TimeZone::UTC).unwrap();
    assert_eq!(l.unexplained, [("/r".to_string(), 600)]);
}

#[test]
fn scanned_text_ages() {
    let tz = TimeZone::UTC;
    assert_eq!(scanned_text(NOW + 4 * 60, NOW + 4 * 60 + 30 * 60, &tz), "scanned 2026-11-11 03:04 · under 1 h ago");
    assert!(scanned_text(NOW - 30 * 60, NOW, &tz).ends_with("under 1 h ago"));
    assert!(scanned_text(NOW - 6 * 3600, NOW, &tz).ends_with("6 h ago"));
    assert!(scanned_text(NOW - 50 * 3600, NOW, &tz).ends_with("2 d ago"));
}

#[test]
fn level_unknown_path_none() {
    let idx = index(dir("r", 0, vec![]), 0);
    assert!(level(&idx, Path::new("/elsewhere"), None, NOW, &TimeZone::UTC).is_none());
}

fn tree(root: &str, dev: u64, fs_used: u64, bytes: u64) -> Tree {
    Tree { root: root.into(), dev, fs_total: 10_000, fs_used, node: dir(root, bytes, vec![]) }
}

/// Spec 7.2: one figure per filesystem, however many trees sit on it.
#[test]
fn unexplained_is_per_filesystem() {
    let idx = UsageIndex {
        created: NOW,
        min_node: 1,
        trees: vec![tree("/a", 1, 1000, 300), tree("/a/b", 1, 1000, 200), tree("/c", 2, 500, 100)],
    };
    let l = roots(&idx, None, NOW, &TimeZone::UTC);
    assert_eq!(l.unexplained, [("/a".to_string(), 500), ("/c".to_string(), 400)]);
}

/// Fully explained filesystems (and over-counted ones) print nothing.
#[test]
fn zero_unexplained_is_skipped() {
    let idx = UsageIndex {
        created: NOW,
        min_node: 1,
        trees: vec![tree("/a", 1, 500, 300), tree("/b", 1, 500, 200), tree("/c", 2, 100, 400)],
    };
    assert!(roots(&idx, None, NOW, &TimeZone::UTC).unexplained.is_empty());
}
