mod common;

use cleaner::fsutil::measure::{InodeSet, measure, measure_as, newest_mtime_excluding};
use common::{mkfile, my_uid, now, set_age};
use std::fs::{self, Permissions};
use std::os::unix::fs::{PermissionsExt, symlink};
use tempfile::TempDir;

#[test]
fn measure_sums_blocks() {
    let tmp = TempDir::new().unwrap();
    let root = tmp.path().join("r");
    mkfile(&root.join("a"), 10_000);
    mkfile(&root.join("sub/b"), 10_000);
    let m = measure(&root, &mut InodeSet::default()).unwrap();
    assert!(m.bytes >= 20_000, "{m:?}");
    assert_eq!(m.files, 2);
    assert_eq!(m.errors, 0);
}

#[test]
fn measure_dedupes_hardlinks() {
    let tmp = TempDir::new().unwrap();
    let a = tmp.path().join("a");
    let b = tmp.path().join("b");
    mkfile(&a.join("f"), 100_000);
    fs::create_dir_all(&b).unwrap();
    fs::hard_link(a.join("f"), b.join("f")).unwrap();
    let mut seen = InodeSet::default();
    let ma = measure(&a, &mut seen).unwrap();
    let mb = measure(&b, &mut seen).unwrap();
    assert!(ma.bytes >= 100_000, "{ma:?}");
    assert_eq!(mb.bytes, 0, "{mb:?}");
}

#[test]
fn measure_newest_mtime() {
    let tmp = TempDir::new().unwrap();
    let root = tmp.path().join("r");
    mkfile(&root.join("old"), 10);
    mkfile(&root.join("new"), 10);
    set_age(&root.join("old"), 40);
    set_age(&root.join("new"), 5);
    // newest_mtime covers the root dir too, so age it past the newest file.
    set_age(&root, 40);
    let m = measure(&root, &mut InodeSet::default()).unwrap();
    let want = now() - 5 * 86_400;
    assert!((m.newest_mtime - want).abs() <= 2, "{} vs {want}", m.newest_mtime);
}

#[test]
fn measure_rejects_symlink_root() {
    let tmp = TempDir::new().unwrap();
    let target = tmp.path().join("t");
    mkfile(&target.join("f"), 10);
    let link = tmp.path().join("link");
    symlink(&target, &link).unwrap();
    let err = measure(&link, &mut InodeSet::default()).unwrap_err();
    assert_eq!(err.kind(), std::io::ErrorKind::InvalidInput);
}

#[test]
fn measure_counts_unreadable_as_error() {
    let tmp = TempDir::new().unwrap();
    let root = tmp.path().join("r");
    mkfile(&root.join("locked/secret"), 10_000);
    mkfile(&root.join("open/visible"), 10_000);
    fs::set_permissions(root.join("locked"), Permissions::from_mode(0o000)).unwrap();
    let res = measure(&root, &mut InodeSet::default());
    fs::set_permissions(root.join("locked"), Permissions::from_mode(0o755)).unwrap();
    let m = res.unwrap();
    assert_eq!(m.errors, 1, "{m:?}");
    assert_eq!(m.files, 1, "{m:?}");
    assert!(m.bytes >= 10_000, "{m:?}");
}

#[test]
fn newest_excluding_skips() {
    let tmp = TempDir::new().unwrap();
    let root = tmp.path().join("proj");
    mkfile(&root.join("src/a.rs"), 10);
    mkfile(&root.join("node_modules/x"), 10);
    set_age(&root.join("src/a.rs"), 100);
    set_age(&root.join("node_modules/x"), 1);
    let skip = |p: &std::path::Path| p.file_name().is_some_and(|n| n == "node_modules");
    let got = newest_mtime_excluding(&root, &skip);
    let want = now() - 100 * 86_400;
    assert!((got - want).abs() <= 2, "{got} vs {want}");
}

#[test]
fn measure_counts_foreign() {
    let tmp = TempDir::new().unwrap();
    let root = tmp.path().join("r");
    mkfile(&root.join("a"), 100);
    mkfile(&root.join("b"), 100);
    let own = measure_as(&root, &mut InodeSet::default(), my_uid()).unwrap();
    assert_eq!((own.foreign, own.foreign_uid), (0, None));
    let other = measure_as(&root, &mut InodeSet::default(), my_uid() + 1).unwrap();
    assert_eq!(other.foreign, 3);
    assert_eq!(other.foreign_uid, Some(my_uid()));
}
