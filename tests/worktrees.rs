mod common;

use cleaner::category::worktrees::Worktrees;
use cleaner::category::{Category, Outcome, ScanCtx, ScanOutput};
use cleaner::config::Config;
use cleaner::fsutil::measure::InodeSet;
use cleaner::object::Object;
use cleaner::plan::{Entry, EntryKind};
use cleaner::protect::Protector;
use common::{git_commit_all, git_init, mkfile, now, set_age};
use std::path::{Path, PathBuf};
use std::process::Command;

// ---- git fixture helpers (kept here, per the plan) ----

/// Runs git in `dir` with a fixed identity and date, isolated from the
/// user's git config. Panics on failure.
fn git(dir: &Path, args: &[&str]) -> String {
    let out = Command::new("git")
        .args(args)
        .current_dir(dir)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        // git still reads $XDG_CONFIG_HOME/git/ignore without a global config.
        // No detached `git maintenance` left running after a commit (git >= 2.47).
        .env("GIT_CONFIG_COUNT", "2")
        .env("GIT_CONFIG_KEY_0", "core.excludesFile")
        .env("GIT_CONFIG_VALUE_0", "/dev/null")
        .env("GIT_CONFIG_KEY_1", "maintenance.auto")
        .env("GIT_CONFIG_VALUE_1", "false")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_AUTHOR_NAME", "Test")
        .env("GIT_AUTHOR_EMAIL", "test@example.invalid")
        .env("GIT_AUTHOR_DATE", "2026-01-01T00:00:00Z")
        .env("GIT_COMMITTER_NAME", "Test")
        .env("GIT_COMMITTER_EMAIL", "test@example.invalid")
        .env("GIT_COMMITTER_DATE", "2026-01-01T00:00:00Z")
        .output()
        .expect("run git");
    assert!(
        out.status.success(),
        "git {args:?} failed in {}: {}",
        dir.display(),
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8(out.stdout).unwrap()
}

/// Sets every file and dir at or under `p` (not following symlinks) to `days` old.
fn age_tree(p: &Path, days: u64) {
    let md = p.symlink_metadata().unwrap();
    if md.file_type().is_symlink() {
        return;
    }
    if md.is_dir() {
        for e in std::fs::read_dir(p).unwrap() {
            age_tree(&e.unwrap().path(), days);
        }
    }
    set_age(p, days);
}

struct Fx {
    _tmp: tempfile::TempDir,
    home: PathBuf,
    repo: PathBuf,
}

impl Fx {
    /// `home/src/r` with one commit on `main`.
    fn new() -> Fx {
        let tmp = tempfile::tempdir().unwrap();
        let home = tmp.path().to_path_buf();
        let repo = home.join("src/r");
        git_init(&repo);
        mkfile(&repo.join("README"), 10);
        git_commit_all(&repo);
        Fx { _tmp: tmp, home, repo }
    }

    fn wt(&self, name: &str) -> PathBuf {
        self.home.join("wt").join(name)
    }

    /// `git worktree add home/wt/<name> -b <name>`.
    fn add(&self, name: &str) -> PathBuf {
        let wt = self.wt(name);
        git(&self.repo, &["worktree", "add", "-q", wt.to_str().unwrap(), "-b", name]);
        wt
    }

    /// A worktree whose branch has one commit, fast-forward merged into main.
    fn merged(&self, name: &str) -> PathBuf {
        let wt = self.add(name);
        mkfile(&wt.join(format!("{name}.txt")), 100);
        git_commit_all(&wt);
        git(&self.repo, &["merge", "-q", "--ff-only", name]);
        wt
    }

    fn age(&self, days: u64) {
        age_tree(&self.home.join("wt"), days);
    }

    fn scan_with(&self, cfg: Config) -> ScanOutput {
        with_ctx(&self.home, cfg, |ctx| Worktrees.scan(ctx, &mut InodeSet::default()).unwrap())
    }

    fn scan(&self) -> ScanOutput {
        self.scan_with(Config::default())
    }

    fn apply(&self, e: &Entry) -> Outcome {
        with_ctx(&self.home, Config::default(), |ctx| Worktrees.apply(e, ctx))
    }
}

fn with_ctx<R>(home: &Path, cfg: Config, f: impl FnOnce(&ScanCtx) -> R) -> R {
    let protector = Protector::new(home, &cfg.protect);
    let tools = common::FakeRunner::new();
    let ctx = ScanCtx { home, now: now(), config: &cfg, protector: &protector, tools: &tools, euid: common::my_uid(), usage: None };
    f(&ctx)
}

fn only_entry(out: &ScanOutput) -> &Entry {
    assert_eq!(out.entries.len(), 1, "entries: {:?}\nskipped: {:?}", out.entries, out.skipped);
    &out.entries[0]
}

fn is_worktree(e: &Entry) -> bool {
    matches!(&e.kind, EntryKind::Object { obj: Object::Worktree(_) })
}

// ---- scan ----

#[test]
fn merged_clean_idle_is_selected() {
    let fx = Fx::new();
    fx.merged("m");
    fx.age(10);

    let out = fx.scan();
    let e = only_entry(&out);
    assert!(is_worktree(e), "{e:?}");
    assert_eq!(e.category, "worktrees");
    assert_eq!(e.title, "wt/m");
    assert!(e.selected, "{e:?}");
    assert!(e.reason.starts_with("branch m merged into main"), "{}", e.reason);
    assert!(e.reason.contains("idle 10 days"), "{}", e.reason);
    assert_eq!(e.note, None);
    assert!(e.bytes > 0);
    assert!(out.skipped.is_empty(), "{:?}", out.skipped);
}

#[test]
fn unmerged_is_not_listed() {
    let fx = Fx::new();
    let wt = fx.add("u");
    mkfile(&wt.join("u.txt"), 100);
    git_commit_all(&wt);
    fx.age(10);

    let out = fx.scan();
    assert!(out.entries.is_empty(), "{:?}", out.entries);
    assert!(out.skipped.is_empty(), "{:?}", out.skipped);
}

#[test]
fn merged_dirty_is_skipped() {
    let fx = Fx::new();
    let wt = fx.merged("d");
    mkfile(&wt.join("README"), 20);
    fx.age(10);

    let out = fx.scan();
    assert!(out.entries.is_empty(), "{:?}", out.entries);
    assert_eq!(out.skipped.len(), 1, "{:?}", out.skipped);
    assert_eq!(out.skipped[0].reason, "merged, but has uncommitted changes");
    assert_eq!(out.skipped[0].category, "worktrees");
}

#[test]
fn untracked_counts_as_dirty() {
    let fx = Fx::new();
    let wt = fx.merged("n");
    mkfile(&wt.join("sub/new.txt"), 20);
    fx.age(10);

    let out = fx.scan();
    assert!(out.entries.is_empty(), "{:?}", out.entries);
    assert_eq!(out.skipped.len(), 1, "{:?}", out.skipped);
    assert_eq!(out.skipped[0].reason, "merged, but has uncommitted changes");
}

#[test]
fn recent_is_listed_unselected() {
    let fx = Fx::new();
    fx.merged("r");
    fx.age(1);

    let e = fx.scan();
    let e = only_entry(&e);
    assert!(!e.selected, "{e:?}");
    assert!(e.reason.contains("idle 1 days"), "{}", e.reason);
}

#[test]
fn ignored_files_block_preselect() {
    let fx = Fx::new();
    mkfile(&fx.repo.join("Cargo.toml"), 10);
    std::fs::write(fx.repo.join(".gitignore"), ".env\ntarget/\n").unwrap();
    git_commit_all(&fx.repo);
    let wt = fx.merged("i");
    mkfile(&wt.join("target/x"), 100);
    fx.age(10);

    // Only build output is ignored: still pre-selected.
    let e = fx.scan();
    let e = only_entry(&e);
    assert!(e.selected, "{e:?}");
    assert_eq!(e.note, None);

    mkfile(&wt.join(".env"), 10);
    fx.age(10);
    let e = fx.scan();
    let e = only_entry(&e);
    assert!(!e.selected, "{e:?}");
    assert_eq!(e.note.as_deref(), Some("has ignored files outside build dirs: .env"));
}

#[test]
fn ignored_files_note_lists_three_and_counts_rest() {
    let fx = Fx::new();
    std::fs::write(fx.repo.join(".gitignore"), "*.local\n").unwrap();
    git_commit_all(&fx.repo);
    let wt = fx.merged("j");
    for n in ["a", "b", "c", "d", "e"] {
        mkfile(&wt.join(format!("{n}.local")), 10);
    }
    fx.age(10);

    let e = fx.scan();
    let e = only_entry(&e);
    assert!(!e.selected);
    assert_eq!(
        e.note.as_deref(),
        Some("has ignored files outside build dirs: a.local, b.local, c.local (+2 more)")
    );
}

#[test]
fn upstream_gone_counts() {
    let fx = Fx::new();
    let remote = fx.home.join("remote.git");
    std::fs::create_dir_all(&remote).unwrap();
    git(&remote, &["init", "-q", "--bare", "-b", "main"]);
    git(&fx.repo, &["remote", "add", "origin", remote.to_str().unwrap()]);
    git(&fx.repo, &["push", "-q", "origin", "main"]);
    let wt = fx.add("g");
    mkfile(&wt.join("g.txt"), 100);
    git_commit_all(&wt); // not on main
    git(&wt, &["push", "-q", "-u", "origin", "g"]);
    git(&remote, &["branch", "-q", "-D", "g"]);
    git(&fx.repo, &["fetch", "-q", "--prune", "origin"]);
    fx.age(10);

    let e = fx.scan();
    let e = only_entry(&e);
    assert!(e.reason.starts_with("upstream branch g is gone"), "{}", e.reason);
    assert!(e.selected, "{e:?}");
}

#[test]
fn detached_at_default_commit_counts() {
    let fx = Fx::new();
    let wt = fx.wt("det");
    git(&fx.repo, &["worktree", "add", "-q", "--detach", wt.to_str().unwrap(), "main"]);
    fx.age(10);

    let e = fx.scan();
    let e = only_entry(&e);
    assert!(e.reason.starts_with("detached at a commit in main"), "{}", e.reason);
}

#[test]
fn locked_is_skipped() {
    let fx = Fx::new();
    let wt = fx.merged("l");
    git(&fx.repo, &["worktree", "lock", wt.to_str().unwrap()]);
    fx.age(10);

    let out = fx.scan();
    assert!(out.entries.is_empty(), "{:?}", out.entries);
    assert_eq!(out.skipped.len(), 1, "{:?}", out.skipped);
    assert_eq!(out.skipped[0].reason, "locked");
    assert_eq!(out.skipped[0].title, "wt/l");
}

#[test]
fn protected_worktree_never_listed() {
    let fx = Fx::new();
    fx.merged("p");
    fx.age(10);

    let cfg = Config { protect: vec!["~/wt".into()], ..Default::default() };
    let out = fx.scan_with(cfg);
    assert!(out.entries.is_empty(), "{:?}", out.entries);
    assert!(out.skipped.is_empty(), "{:?}", out.skipped);
}

#[test]
fn outside_roots_not_listed() {
    let fx = Fx::new();
    fx.merged("o");
    fx.age(10);

    let mut cfg = Config::default();
    cfg.worktrees.roots = Some(vec!["~/src".into()]);
    let out = fx.scan_with(cfg);
    assert!(out.entries.is_empty(), "{:?}", out.entries);
}

#[test]
fn orphan_listed_unselected() {
    let fx = Fx::new();
    let wt = fx.merged("gone");
    std::fs::remove_dir_all(fx.repo.join(".git/worktrees/gone")).unwrap();
    fx.age(10);

    let out = fx.scan();
    let e = only_entry(&out);
    match &e.kind {
        EntryKind::Object { obj: Object::OrphanWorktree { path, .. } } => assert_eq!(path, &wt),
        other => panic!("expected OrphanWorktree, got {other:?}"),
    }
    assert!(!e.selected);
    assert_eq!(
        e.note.as_deref(),
        Some("git no longer lists this worktree; it can't check for uncommitted work")
    );
    assert!(e.bytes > 0);
}

// ---- apply ----

fn git_lists(fx: &Fx, wt: &Path) -> bool {
    git(&fx.repo, &["worktree", "list", "--porcelain"]).contains(wt.to_str().unwrap())
}

#[test]
fn apply_removes_finished() {
    let fx = Fx::new();
    let wt = fx.merged("m");
    fx.age(10);
    let out = fx.scan();
    let e = only_entry(&out);
    assert!(git_lists(&fx, &wt));

    match fx.apply(e) {
        Outcome::Deleted { bytes, failures } => {
            assert!(bytes > 0);
            assert!(failures.is_empty(), "{failures:?}");
        }
        other => panic!("expected Deleted, got {other:?}"),
    }
    assert!(!wt.exists());
    assert!(!git_lists(&fx, &wt));
    git(&fx.repo, &["rev-parse", "--verify", "refs/heads/m"]);
}

fn assert_skipped(o: Outcome, reason: &str) {
    match o {
        Outcome::Skipped { reason: r } => assert_eq!(r, reason),
        other => panic!("expected Skipped({reason}), got {other:?}"),
    }
}

#[test]
fn apply_skips_worktree_new_commit() {
    let fx = Fx::new();
    let wt = fx.merged("c");
    fx.age(10);
    let out = fx.scan();
    let e = only_entry(&out);

    mkfile(&wt.join("more.txt"), 10);
    git_commit_all(&wt);
    assert_skipped(fx.apply(e), "new commits since plan");
    assert!(wt.exists());
}

#[test]
fn apply_skips_worktree_dirty_since_plan() {
    let fx = Fx::new();
    let wt = fx.merged("d");
    fx.age(10);
    let out = fx.scan();
    let e = only_entry(&out);

    mkfile(&wt.join("README"), 99);
    assert_skipped(fx.apply(e), "has uncommitted changes");
    assert!(wt.exists());
}

#[test]
fn apply_skips_worktree_new_untracked() {
    let fx = Fx::new();
    let wt = fx.merged("u");
    fx.age(10);
    let out = fx.scan();
    let e = only_entry(&out);

    mkfile(&wt.join("notes/todo.txt"), 10);
    assert_skipped(fx.apply(e), "has uncommitted changes");
    assert!(wt.exists());
}

#[test]
fn apply_skips_branch_changed() {
    let fx = Fx::new();
    let wt = fx.merged("b");
    fx.age(10);
    let out = fx.scan();
    let e = only_entry(&out);

    git(&wt, &["checkout", "-q", "-b", "other"]);
    assert_skipped(fx.apply(e), "branch changed since plan");
    assert!(wt.exists());
}

#[test]
fn apply_skips_ignored_file_edited() {
    let fx = Fx::new();
    std::fs::write(fx.repo.join(".gitignore"), ".env\n").unwrap();
    git_commit_all(&fx.repo);
    let wt = fx.merged("e");
    mkfile(&wt.join(".env"), 10);
    fx.age(10);
    let out = fx.scan();
    let e = only_entry(&out);

    mkfile(&wt.join(".env"), 20); // mtime now
    assert_skipped(fx.apply(e), "changed since plan");
    assert!(wt.exists());
}

#[test]
fn apply_skips_replaced_dir() {
    let fx = Fx::new();
    let wt = fx.merged("x");
    fx.age(10);
    let out = fx.scan();
    let e = only_entry(&out);

    std::fs::rename(&wt, fx.home.join("wt/x-old")).unwrap();
    assert_skipped(fx.apply(e), "no longer exists");
    std::fs::create_dir(&wt).unwrap();
    assert_skipped(fx.apply(e), "replaced since plan");
}

#[test]
fn apply_orphan_deletes_dir() {
    let fx = Fx::new();
    let wt = fx.merged("gone");
    std::fs::remove_dir_all(fx.repo.join(".git/worktrees/gone")).unwrap();
    fx.age(10);
    let out = fx.scan();
    let e = only_entry(&out);

    match fx.apply(e) {
        Outcome::Deleted { bytes, .. } => assert!(bytes > 0),
        other => panic!("expected Deleted, got {other:?}"),
    }
    assert!(!wt.exists());
    assert!(fx.repo.join("README").exists());
}

// ---- mount points (§6.2): `git worktree remove` doesn't stop at mounts ----

const NS_DIR_ENV: &str = "CLEANER_TEST_NS_DIR";

/// Re-runs `test` inside a user + mount namespace (`unshare -rm`), as in
/// tests/mounts.rs; skipped where namespaces are unavailable.
fn in_mount_ns(test: &str, body: impl FnOnce()) {
    if std::env::var_os(NS_DIR_ENV).is_some() {
        return body();
    }
    let probe = Command::new("unshare").args(["--user", "--map-root-user", "--mount", "true"]).status();
    if !probe.is_ok_and(|s| s.success()) {
        eprintln!("skipping {test}: user+mount namespaces unavailable");
        return;
    }
    let out = Command::new("unshare")
        .args(["--user", "--map-root-user", "--mount"])
        .arg(std::env::current_exe().unwrap())
        .args([test, "--exact", "--nocapture", "--test-threads=1"])
        .env(NS_DIR_ENV, "1")
        .output()
        .unwrap();
    let stdout = String::from_utf8_lossy(&out.stdout);
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(out.status.success(), "{test} failed in namespace:\n{stdout}\n{stderr}");
    assert!(stdout.contains("1 passed"), "{test} did not run in namespace:\n{stdout}\n{stderr}");
}

fn mount_tmpfs(dir: &Path) {
    use std::os::unix::ffi::OsStrExt;
    let c = |s: &[u8]| std::ffi::CString::new(s).unwrap();
    let (src, target) = (c(b"tmpfs"), c(dir.as_os_str().as_bytes()));
    let rc = unsafe { libc::mount(src.as_ptr(), target.as_ptr(), src.as_ptr(), 0, std::ptr::null()) };
    assert_eq!(rc, 0, "mount tmpfs at {}: {}", dir.display(), std::io::Error::last_os_error());
}

#[test]
fn worktree_with_mount_inside_is_never_removed() {
    in_mount_ns("worktree_with_mount_inside_is_never_removed", || {
        let fx = Fx::new();
        std::fs::write(fx.repo.join(".gitignore"), "mnt/\n").unwrap();
        git_commit_all(&fx.repo);
        let wt = fx.merged("mt");
        std::fs::create_dir(wt.join("mnt")).unwrap();
        fx.age(10);
        let out = fx.scan();
        let e = only_entry(&out);

        mount_tmpfs(&wt.join("mnt"));
        mkfile(&wt.join("mnt/precious"), 10);
        assert_skipped(fx.apply(e), "contains a mount point");
        assert!(wt.join("mnt/precious").exists());

        let out = fx.scan();
        assert!(out.entries.is_empty(), "{:?}", out.entries);
        assert_eq!(out.skipped.len(), 1, "{:?}", out.skipped);
        assert_eq!(out.skipped[0].reason, "contains a mount point");
    });
}

#[test]
fn apply_orphan_skips_changed() {
    let fx = Fx::new();
    let wt = fx.merged("gone");
    std::fs::remove_dir_all(fx.repo.join(".git/worktrees/gone")).unwrap();
    fx.age(10);
    let out = fx.scan();
    let e = only_entry(&out);

    mkfile(&wt.join("new.txt"), 10);
    assert_skipped(fx.apply(e), "changed since plan");
    assert!(wt.exists());
}

#[test]
fn apply_skips_ignored_file_moved_in_with_old_mtime() {
    let fx = Fx::new();
    std::fs::write(fx.repo.join(".gitignore"), "data/\n*.sqlite\n").unwrap();
    git_commit_all(&fx.repo);
    let wt = fx.merged("mv");
    fx.age(10);
    let out = fx.scan();
    let e = only_entry(&out);
    assert!(e.selected, "{e:?}");
    assert_eq!(e.note, None);

    // `mv`/`unzip`/`cp -a` keep the old mtime, so the mtime check can't see it.
    mkfile(&wt.join("local.sqlite"), 10);
    set_age(&wt.join("local.sqlite"), 30);
    assert_skipped(fx.apply(e), "has ignored files outside build dirs now");
    assert!(wt.join("local.sqlite").exists());
}

#[test]
fn trunk_worktree_is_never_finished() {
    // Main checkout detached, trunk checked out in a linked worktree.
    let fx = Fx::new();
    git(&fx.repo, &["checkout", "-q", "--detach"]);
    let wt = fx.wt("trunk");
    git(&fx.repo, &["worktree", "add", "-q", wt.to_str().unwrap(), "main"]);
    fx.age(10);

    let out = fx.scan();
    assert!(out.entries.is_empty(), "{:?}", out.entries);
    assert!(out.skipped.is_empty(), "{:?}", out.skipped);
}

#[test]
fn trunk_worktree_with_origin_default_is_never_finished() {
    let fx = Fx::new();
    let remote = fx.home.join("remote.git");
    std::fs::create_dir_all(&remote).unwrap();
    git(&remote, &["init", "-q", "--bare", "-b", "main"]);
    git(&fx.repo, &["remote", "add", "origin", remote.to_str().unwrap()]);
    git(&fx.repo, &["push", "-q", "-u", "origin", "main"]);
    git(&fx.repo, &["remote", "set-head", "origin", "main"]);
    git(&fx.repo, &["checkout", "-q", "--detach"]);
    let wt = fx.wt("trunk");
    git(&fx.repo, &["worktree", "add", "-q", wt.to_str().unwrap(), "main"]);
    fx.age(10);

    let out = fx.scan();
    assert!(out.entries.is_empty(), "{:?}", out.entries);
}

#[test]
fn nested_repo_in_ignored_dir_is_skipped() {
    let fx = Fx::new();
    std::fs::write(fx.repo.join(".gitignore"), ".claude/\n").unwrap();
    git_commit_all(&fx.repo);
    let wt = fx.merged("nest");
    let sub = wt.join(".claude/worktrees/sub");
    git_init(&sub);
    mkfile(&sub.join("work.txt"), 10);
    fx.age(10);

    let out = fx.scan();
    assert!(out.entries.iter().all(|e| !e.title.starts_with("wt/nest")), "{:?}", out.entries);
    let s: Vec<_> = out.skipped.iter().filter(|s| s.title == "wt/nest").collect();
    assert_eq!(s.len(), 1, "{:?}", out.skipped);
    assert_eq!(s[0].reason, "contains a nested git repo");
}

/// Plan 4 ecosystem names (`.ipynb_checkpoints`, `vendor`) must not widen the
/// ignored-file guard: they stay ignored files that block pre-selection and
/// are named in the note, never silently removable build output.
#[test]
fn plan4_artifact_names_do_not_widen_ignored_guard() {
    for dir in [".ipynb_checkpoints", "vendor"] {
        let fx = Fx::new();
        std::fs::write(fx.repo.join(".gitignore"), format!("{dir}/\n")).unwrap();
        git_commit_all(&fx.repo);
        let wt = fx.merged("nb");
        mkfile(&wt.join(format!("{dir}/a-checkpoint.ipynb")), 10);
        fx.age(10);

        let out = fx.scan();
        let e = only_entry(&out);
        assert!(!e.selected, "{dir}: {e:?}");
        assert_eq!(e.note.as_deref(), Some(format!("has ignored files outside build dirs: {dir}/").as_str()), "{dir}");
    }
}

#[test]
fn ignored_vendored_dir_is_skipped() {
    let fx = Fx::new();
    std::fs::write(fx.repo.join(".gitignore"), "vendored/\n").unwrap();
    git_commit_all(&fx.repo);
    let wt = fx.merged("vend");
    mkfile(&wt.join("vendored/lib.c"), 10);
    fx.age(10);

    let out = fx.scan();
    assert!(out.entries.is_empty(), "{:?}", out.entries);
    assert_eq!(out.skipped.len(), 1, "{:?}", out.skipped);
    assert_eq!(out.skipped[0].reason, "contains a protected path");
}

#[test]
fn apply_skips_nested_repo_created_since_plan() {
    let fx = Fx::new();
    std::fs::write(fx.repo.join(".gitignore"), ".claude/\n").unwrap();
    git_commit_all(&fx.repo);
    let wt = fx.merged("late");
    mkfile(&wt.join(".claude/notes.txt"), 10);
    fx.age(10);
    let out = fx.scan();
    let e = only_entry(&out);
    assert_eq!(e.note.as_deref(), Some("has ignored files outside build dirs: .claude/"));

    // The user ticks it; then an empty repo appears (its files are all under .git).
    git_init(&wt.join(".claude/sub"));
    assert_skipped(fx.apply(e), "contains a nested git repo");
    assert!(wt.join(".claude/sub/.git").exists());
}

#[test]
fn orphan_with_missing_main_repo_says_so() {
    let fx = Fx::new();
    let wt = fx.merged("off");
    // The main repo is gone (or on an unmounted filesystem).
    std::fs::remove_dir_all(&fx.repo).unwrap();
    fx.age(10);

    let out = fx.scan();
    let e = only_entry(&out);
    assert!(matches!(&e.kind, EntryKind::Object { obj: Object::OrphanWorktree { path, .. } } if path == &wt), "{e:?}");
    assert!(!e.selected);
    let repo = fx.repo.display();
    assert!(e.reason.starts_with(&format!("main repo {repo} not found")), "{}", e.reason);
    assert_eq!(
        e.note.as_deref(),
        Some(format!("main repo {repo} not found; it can't check for uncommitted work").as_str())
    );
}

#[test]
fn scan_does_not_search_artifact_dirs_for_mounts_but_apply_does() {
    in_mount_ns("scan_does_not_search_artifact_dirs_for_mounts_but_apply_does", || {
        let fx = Fx::new();
        mkfile(&fx.repo.join("package.json"), 2);
        std::fs::write(fx.repo.join(".gitignore"), "node_modules/\n").unwrap();
        git_commit_all(&fx.repo);
        let wt = fx.merged("nm");
        std::fs::create_dir_all(wt.join("node_modules/pkg/mnt")).unwrap();
        mount_tmpfs(&wt.join("node_modules/pkg/mnt"));
        mkfile(&wt.join("node_modules/pkg/mnt/precious"), 10);
        fx.age(10);

        // The scan prunes artifact dirs (M10 scan cost)...
        let out = fx.scan();
        let e = only_entry(&out);
        // ...but apply, the last check before git deletes, walks everything.
        assert_skipped(fx.apply(e), "contains a mount point");
        assert!(wt.join("node_modules/pkg/mnt/precious").exists());
    });
}

#[test]
fn artifact_dir_that_is_a_mount_point_blocks_scan() {
    in_mount_ns("artifact_dir_that_is_a_mount_point_blocks_scan", || {
        let fx = Fx::new();
        mkfile(&fx.repo.join("Cargo.toml"), 2);
        std::fs::write(fx.repo.join(".gitignore"), "target/\n").unwrap();
        git_commit_all(&fx.repo);
        let wt = fx.merged("tg");
        std::fs::create_dir(wt.join("target")).unwrap();
        mount_tmpfs(&wt.join("target"));
        fx.age(10);

        let out = fx.scan();
        assert!(out.entries.is_empty(), "{:?}", out.entries);
        assert_eq!(out.skipped.len(), 1, "{:?}", out.skipped);
        assert_eq!(out.skipped[0].reason, "contains a mount point");
    });
}
