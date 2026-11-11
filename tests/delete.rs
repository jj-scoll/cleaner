mod common;

use cleaner::fsutil::delete::{DeleteError, delete_tree, delete_tree_on_dev};
use common::mkfile;
use std::fs::{self, Permissions};
use std::os::unix::fs::{MetadataExt, PermissionsExt, symlink};
use tempfile::TempDir;

fn mode(p: &std::path::Path) -> u32 {
    fs::symlink_metadata(p).unwrap().mode() & 0o7777
}

#[test]
fn deletes_nested_tree() {
    let tmp = TempDir::new().unwrap();
    let root = tmp.path().join("tree");
    mkfile(&root.join("a.txt"), 5000);
    mkfile(&root.join("l1/b.txt"), 5000);
    mkfile(&root.join("l1/l2/c.txt"), 5000);
    mkfile(&root.join("l1/l2/l3/d.txt"), 5000);
    let freed = delete_tree(&root).unwrap();
    assert!(!root.exists());
    assert!(freed > 0);
    assert!(tmp.path().exists());
}

#[test]
fn does_not_follow_symlink_inside() {
    let tmp = TempDir::new().unwrap();
    let outside = tmp.path().join("outside");
    mkfile(&outside.join("keep.txt"), 100);
    let victim = tmp.path().join("victim");
    fs::create_dir_all(&victim).unwrap();
    symlink(&outside, victim.join("link")).unwrap();
    delete_tree(&victim).unwrap();
    assert!(fs::symlink_metadata(&victim).is_err());
    assert!(outside.join("keep.txt").exists());
}

#[test]
fn deletes_symlink_itself_not_target() {
    let tmp = TempDir::new().unwrap();
    let target = tmp.path().join("target");
    mkfile(&target.join("f"), 100);
    let link = tmp.path().join("link_to_dir");
    symlink(&target, &link).unwrap();
    delete_tree(&link).unwrap();
    assert!(fs::symlink_metadata(&link).is_err());
    assert!(target.join("f").exists());
}

#[test]
fn deletes_readonly_dirs() {
    let tmp = TempDir::new().unwrap();
    let root = tmp.path().join("mod");
    mkfile(&root.join("pkg@v1/a/x.go"), 100);
    mkfile(&root.join("pkg@v1/b.go"), 100);
    for d in ["pkg@v1/a", "pkg@v1", ""] {
        let p = if d.is_empty() { root.clone() } else { root.join(d) };
        fs::set_permissions(&p, Permissions::from_mode(0o555)).unwrap();
    }
    delete_tree(&root).unwrap();
    assert!(fs::symlink_metadata(&root).is_err());
}

#[test]
fn refuses_wrong_device() {
    let tmp = TempDir::new().unwrap();
    let dir = tmp.path().join("dir");
    let files = [dir.join("a"), dir.join("sub/b"), dir.join("sub/deeper/c")];
    for f in &files {
        mkfile(f, 100);
    }
    let dev = fs::symlink_metadata(&dir).unwrap().dev();
    let err = delete_tree_on_dev(&dir, dev + 1).unwrap_err();
    assert!(matches!(err, DeleteError::CrossesMount(_)), "{err:?}");
    for f in &files {
        assert!(f.exists(), "{} was deleted", f.display());
    }
}

#[test]
fn hardlinked_file_not_counted_as_freed() {
    let tmp = TempDir::new().unwrap();
    let a = tmp.path().join("a");
    let b = tmp.path().join("b");
    mkfile(&a.join("f"), 100_000);
    fs::create_dir_all(&b).unwrap();
    fs::hard_link(a.join("f"), b.join("f")).unwrap();
    let freed = delete_tree(&a).unwrap();
    assert_eq!(freed, 0);
    assert!(!a.exists());
    assert!(b.join("f").exists());
}

#[test]
fn readonly_parent_fails_without_chmod() {
    let tmp = TempDir::new().unwrap();
    let p = tmp.path().join("p");
    mkfile(&p.join("t/f"), 100);
    fs::set_permissions(&p, Permissions::from_mode(0o555)).unwrap();
    let res = delete_tree(&p.join("t"));
    let p_mode = mode(&p);
    // Restore perms before asserting so TempDir cleanup always works.
    fs::set_permissions(&p, Permissions::from_mode(0o755)).unwrap();
    assert!(matches!(res, Err(DeleteError::Io(_))), "{res:?}");
    assert_eq!(p_mode, 0o555);
}
