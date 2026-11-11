mod common;

use cleaner::git::{is_tracked, last_commit_time, repo_root};
use cleaner::protect::Protector;
use common::{git_commit_all, git_init, mkfile, now};
use tempfile::TempDir;

#[test]
fn protects_inside_builtin() {
    let home = TempDir::new().unwrap();
    let prot = Protector::new(home.path(), &[]);
    assert!(prot.is_protected(&home.path().join(".claude/projects/x")));
}

#[test]
fn protects_ancestor_of_builtin() {
    let home = TempDir::new().unwrap();
    let prot = Protector::new(home.path(), &[]);
    // Deleting ~/.local/share would delete ~/.local/share/memory.
    assert!(prot.is_protected(&home.path().join(".local/share")));
}

#[test]
fn protects_vendored_and_git_components() {
    let home = TempDir::new().unwrap();
    let prot = Protector::new(home.path(), &[]);
    assert!(prot.is_protected(&home.path().join("src/a/vendored/b")));
    assert!(prot.is_protected(&home.path().join("src/a/.git/objects")));
}

#[test]
fn unrelated_not_protected() {
    let home = TempDir::new().unwrap();
    let prot = Protector::new(home.path(), &[]);
    assert!(!prot.is_protected(&home.path().join(".cache/go-build")));
}

#[test]
fn user_entries_added() {
    let home = TempDir::new().unwrap();
    let prot = Protector::new(home.path(), &["~/keep".into()]);
    assert!(prot.is_protected(&home.path().join("keep/x")));
    assert!(!Protector::new(home.path(), &[]).is_protected(&home.path().join("keep/x")));
}

#[test]
fn tracked_dir_detected() {
    let tmp = TempDir::new().unwrap();
    let repo = tmp.path().join("repo");
    git_init(&repo);
    mkfile(&repo.join("node_modules/a.js"), 10);
    git_commit_all(&repo);
    mkfile(&repo.join("other/b.js"), 10); // untracked, never committed

    assert_eq!(repo_root(&repo.join("node_modules")), Some(repo.clone()));
    assert!(is_tracked(&repo, &repo.join("node_modules")));
    assert!(!is_tracked(&repo, &repo.join("other")));
}

#[test]
fn last_commit_time_some() {
    let tmp = TempDir::new().unwrap();
    let repo = tmp.path().join("repo");
    git_init(&repo);
    mkfile(&repo.join("a.txt"), 1);
    git_commit_all(&repo);
    // git_commit_all pins GIT_COMMITTER_DATE to 2026-01-01T00:00:00Z, so the
    // commit time is that fixed instant, not "within 60 s of now()".
    assert_eq!(last_commit_time(&repo), Some(1_767_225_600));
    assert!(last_commit_time(&repo).unwrap() <= now());

    let plain = tmp.path().join("plain");
    std::fs::create_dir_all(&plain).unwrap();
    assert_eq!(last_commit_time(&plain), None);
}
