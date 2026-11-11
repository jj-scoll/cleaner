mod common;

use cleaner::config::Config;
use cleaner::fsutil::measure::{InodeSet, measure};
use cleaner::fsutil::usage::{Node, Tree, UsageIndex, build, index_roots};
use common::{mkfile, my_uid, now, set_age};
use std::ffi::OsString;
use std::os::unix::ffi::OsStringExt;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use tempfile::TempDir;

const MIN: u64 = 16384;

fn bytes_of(p: &Path) -> u64 {
    measure(p, &mut InodeSet::default()).unwrap().bytes
}

fn tree_of<'a>(idx: &'a UsageIndex, root: &Path) -> &'a Tree {
    idx.trees.iter().find(|t| t.root == root).unwrap_or_else(|| panic!("no tree for {root:?}"))
}

fn child<'a>(n: &'a Node, name: &str) -> Option<&'a Node> {
    n.children.iter().find(|c| c.name == Path::new(name))
}

fn names(n: &Node) -> Vec<String> {
    let mut v: Vec<String> = n.children.iter().map(|c| c.name.display().to_string()).collect();
    v.sort();
    v
}

#[test]
fn index_sizes_and_prunes() {
    let tmp = TempDir::new().unwrap();
    let r = tmp.path().join("r");
    for i in 0..3 {
        mkfile(&r.join(format!("a/f{i}")), 8192);
    }
    mkfile(&r.join("b/f"), 4096);
    mkfile(&r.join("big.bin"), 32768);
    mkfile(&r.join("small.txt"), 10);
    let idx = build(std::slice::from_ref(&r), &[], MIN, 7);
    assert_eq!((idx.created, idx.min_node), (7, MIN));
    let root = &tree_of(&idx, &r).node;
    assert_eq!(root.bytes, bytes_of(&r));
    assert_eq!(names(root), ["a", "big.bin"]);
    assert!(!child(root, "big.bin").unwrap().dir);
    assert!(child(root, "a").unwrap().dir);
    assert_eq!(root.other_count, 2);
    assert_eq!(root.other_bytes, bytes_of(&r.join("b")) + bytes_of(&r.join("small.txt")));
}

#[test]
fn index_counts_hardlinks_once() {
    let tmp = TempDir::new().unwrap();
    let r = tmp.path().join("r");
    mkfile(&r.join("a/f"), 32768);
    std::fs::create_dir_all(r.join("c")).unwrap();
    std::fs::hard_link(r.join("a/f"), r.join("c/f")).unwrap();
    let idx = build(std::slice::from_ref(&r), &[], MIN, 0);
    let root = &tree_of(&idx, &r).node;
    use std::os::unix::fs::MetadataExt;
    let once = std::fs::metadata(r.join("a/f")).unwrap().blocks() * 512;
    assert_eq!(root.bytes, once);
    assert_eq!(root.bytes, bytes_of(&r));
    let kids: u64 = root.children.iter().map(|c| c.bytes).sum();
    assert_eq!(kids, once);
}

#[test]
fn index_records_newest_mtime() {
    let tmp = TempDir::new().unwrap();
    let r = tmp.path().join("r");
    for i in 0..3 {
        mkfile(&r.join(format!("a/old{i}")), 8192);
        set_age(&r.join(format!("a/old{i}")), 100);
    }
    mkfile(&r.join("a/new"), 8192);
    set_age(&r.join("a/new"), 1);
    set_age(&r.join("a"), 100);
    set_age(&r, 100);
    let idx = build(std::slice::from_ref(&r), &[], MIN, 0);
    let root = &tree_of(&idx, &r).node;
    let a = child(root, "a").unwrap();
    let want = now() - 86_400;
    assert!((a.newest_mtime - want).abs() <= 60, "{} vs {want}", a.newest_mtime);
    assert_eq!(root.newest_mtime, a.newest_mtime);
}

#[test]
fn index_never_follows_symlinks() {
    let tmp = TempDir::new().unwrap();
    let other = tmp.path().join("other");
    mkfile(&other.join("f"), 1 << 20);
    let r = tmp.path().join("r");
    mkfile(&r.join("x"), 10);
    std::os::unix::fs::symlink(&other, r.join("l")).unwrap();
    let linked_root = tmp.path().join("lr");
    std::os::unix::fs::symlink(&other, &linked_root).unwrap();
    let idx = build(&[r.clone(), linked_root.clone(), tmp.path().join("missing")], &[], MIN, 0);
    assert_eq!(idx.trees.len(), 1);
    let root = &tree_of(&idx, &r).node;
    assert!(root.bytes < 64 * 1024, "{}", root.bytes);
    assert!(child(root, "l").is_none());
}

#[test]
fn index_unreadable_dir_kept() {
    let tmp = TempDir::new().unwrap();
    let r = tmp.path().join("r");
    mkfile(&r.join("ok/f"), 100);
    let locked = r.join("locked");
    std::fs::create_dir_all(&locked).unwrap();
    std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o000)).unwrap();
    let readable = std::fs::read_dir(&locked).is_ok(); // root ignores the mode
    let idx = build(std::slice::from_ref(&r), &[], MIN, 0);
    std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o755)).unwrap();
    if readable {
        eprintln!("skipping: mode 000 is readable here");
        return;
    }
    let root = &tree_of(&idx, &r).node;
    let l = child(root, "locked").expect("locked dir kept");
    assert_eq!(l.unreadable_owner, Some(my_uid()));
    assert_eq!(l.bytes, 0);
    assert!(l.dir);
}

#[test]
fn index_excludes_given_paths() {
    let tmp = TempDir::new().unwrap();
    let r = tmp.path().join("r");
    mkfile(&r.join("skip/f"), 65536);
    mkfile(&r.join("keep/f"), 32768);
    let idx = build(std::slice::from_ref(&r), &[r.join("skip")], MIN, 0);
    let root = &tree_of(&idx, &r).node;
    assert!(child(root, "skip").is_none());
    assert_eq!(root.bytes, bytes_of(&r) - bytes_of(&r.join("skip")));
    assert!(idx.trees.len() == 1 && build(std::slice::from_ref(&r), std::slice::from_ref(&r), MIN, 0).trees.is_empty());
}

#[test]
fn index_save_load_roundtrip() {
    let tmp = TempDir::new().unwrap();
    let r = tmp.path().join("r");
    let weird = PathBuf::from(OsString::from_vec(b"bad\xffname".to_vec()));
    mkfile(&r.join(&weird), 32768);
    mkfile(&r.join("ok"), 32768);
    let idx = build(std::slice::from_ref(&r), &[], MIN, 5);
    assert!(child(&tree_of(&idx, &r).node, "ok").is_some());
    assert!(tree_of(&idx, &r).node.children.iter().any(|c| c.name == weird));
    let file = tmp.path().join("state/usage.json");
    idx.save(&file).unwrap();
    assert_eq!(UsageIndex::load(&file).unwrap(), Some(idx));
    assert_eq!(UsageIndex::load(&tmp.path().join("missing.json")).unwrap(), None);
}

#[test]
fn index_roots_union_dedup() {
    let tmp = TempDir::new().unwrap();
    let home = tmp.path().join("home");
    let extra = tmp.path().join("extra");
    std::fs::create_dir_all(&home).unwrap();
    std::fs::create_dir_all(&extra).unwrap();
    let mut config = Config::default();
    config.disk_map.roots = vec!["~".into()];
    config.scan_roots = vec!["~".into(), "~/src".into(), extra.display().to_string()];
    let mut got = index_roots(&config, &home);
    got.sort();
    let mut want = vec![home.clone(), extra];
    want.sort();
    assert_eq!(got, want);
}

#[test]
fn node_and_visit() {
    let tmp = TempDir::new().unwrap();
    let r = tmp.path().join("r");
    mkfile(&r.join("a/f"), 32768);
    mkfile(&r.join("z.bin"), 32768);
    let idx = build(std::slice::from_ref(&r), &[], MIN, 0);
    let (tree, a) = idx.node(&r.join("a")).unwrap();
    assert_eq!(tree.root, r);
    assert_eq!(a.name, Path::new("a"));
    assert_eq!(idx.node(&r).unwrap().1.bytes, bytes_of(&r));
    assert!(idx.node(&r.join("nope")).is_none());
    let mut seen: Vec<PathBuf> = Vec::new();
    idx.visit(&mut |p, _| seen.push(p.to_path_buf()));
    let pos = |p: PathBuf| seen.iter().position(|s| *s == p).unwrap_or_else(|| panic!("{p:?} not in {seen:?}"));
    assert!(pos(r.clone()) < pos(r.join("a")));
    assert!(pos(r.join("a")) < pos(r.join("a/f")));
    assert!(seen.contains(&r.join("z.bin")));
}
