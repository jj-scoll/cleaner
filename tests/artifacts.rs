mod common;

use cleaner::category::artifacts::Artifacts;
use cleaner::category::{Category, Outcome, ScanCtx, ScanOutput};
use cleaner::config::Config;
use cleaner::fsutil::measure::InodeSet;
use cleaner::plan::{Entry, EntryKind};
use cleaner::protect::Protector;
use common::{git_commit_all, git_init, mkfile, now, set_age, set_times};
use std::path::{Path, PathBuf};
use std::process::Command;

fn with_ctx<R>(home: &Path, cfg: Config, f: impl FnOnce(&ScanCtx) -> R) -> R {
    let protector = Protector::new(home, &cfg.protect);
    let tools = common::FakeRunner::new();
    let ctx = ScanCtx { home, now: now(), config: &cfg, protector: &protector, tools: &tools, euid: common::my_uid(), usage: None };
    f(&ctx)
}

fn scan_with(home: &Path, cfg: Config) -> ScanOutput {
    with_ctx(home, cfg, |ctx| Artifacts.scan(ctx, &mut InodeSet::default()).unwrap())
}

fn scan(home: &Path) -> ScanOutput {
    scan_with(home, Config::default())
}

fn targets(e: &Entry) -> Vec<PathBuf> {
    match &e.kind {
        EntryKind::Item { targets } => targets.iter().map(|t| t.path.clone()).collect(),
        other => panic!("expected Item, got {other:?}"),
    }
}

fn entry_for<'a>(out: &'a ScanOutput, title: &str) -> &'a Entry {
    out.entries
        .iter()
        .find(|e| e.title == title)
        .unwrap_or_else(|| panic!("no entry titled {title}: {:?}", out.entries))
}

/// Commits everything with the current time as author and committer date.
fn git_commit_now(dir: &Path) {
    for args in [
        &["add", "-A"][..],
        &["-c", "commit.gpgsign=false", "commit", "-q", "--allow-empty", "-m", "now"][..],
    ] {
        let st = Command::new("git")
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
            .env("GIT_COMMITTER_NAME", "Test")
            .env("GIT_COMMITTER_EMAIL", "test@example.invalid")
            .env_remove("GIT_AUTHOR_DATE")
            .env_remove("GIT_COMMITTER_DATE")
            .status()
            .unwrap();
        assert!(st.success());
    }
}

#[test]
fn finds_rust_and_node() {
    let tmp = tempfile::tempdir().unwrap();
    let h = tmp.path();
    mkfile(&h.join("p1/Cargo.toml"), 10);
    mkfile(&h.join("p1/target/debug/bin"), 1000);
    mkfile(&h.join("p2/package.json"), 10);
    mkfile(&h.join("p2/node_modules/x/index.js"), 1000);

    let out = scan(h);
    assert_eq!(out.entries.len(), 2, "{:?}", out.entries);
    let p1 = entry_for(&out, "p1");
    assert_eq!(targets(p1), vec![h.join("p1/target")]);
    assert_eq!(p1.category, "artifacts");
    assert!(p1.bytes > 0);
    let p2 = entry_for(&out, "p2");
    assert_eq!(targets(p2), vec![h.join("p2/node_modules")]);
}

#[test]
fn build_without_marker_ignored() {
    let tmp = tempfile::tempdir().unwrap();
    let h = tmp.path();
    mkfile(&h.join("p3/build/out.o"), 1000);

    let out = scan(h);
    assert!(out.entries.is_empty(), "{:?}", out.entries);
    assert!(out.skipped.is_empty(), "{:?}", out.skipped);
}

#[test]
fn package_json_inside_node_modules_not_a_project() {
    let tmp = tempfile::tempdir().unwrap();
    let h = tmp.path();
    mkfile(&h.join("p2/package.json"), 10);
    mkfile(&h.join("p2/node_modules/x/package.json"), 10);
    mkfile(&h.join("p2/node_modules/x/node_modules/y/i.js"), 100);

    let out = scan(h);
    let all: Vec<PathBuf> = out.entries.iter().flat_map(targets).collect();
    assert_eq!(all, vec![h.join("p2/node_modules")]);
}

#[test]
fn nested_workspace_shares_target() {
    let tmp = tempfile::tempdir().unwrap();
    let h = tmp.path();
    mkfile(&h.join("ws/Cargo.toml"), 10);
    mkfile(&h.join("ws/target/debug/a"), 1000);
    mkfile(&h.join("ws/crates/a/Cargo.toml"), 10);
    mkfile(&h.join("ws/crates/a/src/lib.rs"), 10);

    let out = scan(h);
    let all: Vec<PathBuf> = out.entries.iter().flat_map(targets).collect();
    assert_eq!(all, vec![h.join("ws/target")]);
}

#[test]
fn tracked_node_modules_skipped() {
    let tmp = tempfile::tempdir().unwrap();
    let h = tmp.path();
    let p = h.join("p");
    git_init(&p);
    mkfile(&p.join("package.json"), 10);
    mkfile(&p.join("node_modules/a.js"), 10);
    git_commit_all(&p);

    let out = scan(h);
    assert!(out.entries.is_empty(), "{:?}", out.entries);
    assert!(
        out.skipped.iter().any(|s| s.reason.contains("tracked in git")),
        "{:?}",
        out.skipped
    );
}

#[test]
fn symlinked_node_modules_skipped() {
    let tmp = tempfile::tempdir().unwrap();
    let h = tmp.path();
    mkfile(&h.join("main/node_modules/dep/index.js"), 1000);
    mkfile(&h.join("wt/package.json"), 10);
    mkfile(&h.join("wt/dist/bundle.js"), 1000);
    set_age(&h.join("wt/package.json"), 40);
    std::os::unix::fs::symlink("../main/node_modules", h.join("wt/node_modules")).unwrap();

    let cfg = Config::default();
    let out = scan_with(h, cfg.clone());
    assert!(
        out.skipped.iter().any(|s| s.reason.contains("symlink")),
        "{:?}",
        out.skipped
    );
    let wt = entry_for(&out, "wt");
    assert_eq!(targets(wt), vec![h.join("wt/dist")]);

    with_ctx(h, cfg, |ctx| {
        for e in &out.entries {
            let o = Artifacts.apply(e, ctx);
            assert!(matches!(o, Outcome::Deleted { .. }), "{o:?}");
        }
    });
    assert!(!h.join("wt/dist").exists());
    assert!(h.join("main/node_modules/dep/index.js").exists());
    assert!(h.join("wt/node_modules").symlink_metadata().is_ok());
}

#[test]
fn stale_project_selected_active_not() {
    let tmp = tempfile::tempdir().unwrap();
    let h = tmp.path();
    mkfile(&h.join("old/Cargo.toml"), 10);
    mkfile(&h.join("old/src/main.rs"), 10);
    mkfile(&h.join("old/target/debug/x"), 1000);
    set_age(&h.join("old/Cargo.toml"), 40);
    set_age(&h.join("old/src/main.rs"), 40);

    mkfile(&h.join("new/Cargo.toml"), 10);
    mkfile(&h.join("new/src/main.rs"), 10);
    mkfile(&h.join("new/target/debug/x"), 1000);
    set_age(&h.join("new/Cargo.toml"), 40);
    set_age(&h.join("new/src/main.rs"), 2);

    let out = scan(h);
    let old = entry_for(&out, "old");
    assert!(old.selected);
    assert!(old.reason.contains("untouched for 40 days"), "{}", old.reason);
    let new = entry_for(&out, "new");
    assert!(!new.selected);
    assert!(new.reason.contains("active 2 days ago"), "{}", new.reason);
}

/// Age is mtime (spec: something reads every file, so atime is useless).
#[test]
fn age_uses_mtime_not_atime() {
    let tmp = tempfile::tempdir().unwrap();
    let h = tmp.path();
    for p in ["edited", "read"] {
        mkfile(&h.join(p).join("Cargo.toml"), 10);
        mkfile(&h.join(p).join("src/main.rs"), 10);
        mkfile(&h.join(p).join("target/debug/x"), 1000);
    }
    // Edited 2 days ago, not read for 100: active.
    set_times(&h.join("edited/Cargo.toml"), 2, 100);
    set_times(&h.join("edited/src/main.rs"), 2, 100);
    // Not edited for 40 days, read just now: stale.
    set_times(&h.join("read/Cargo.toml"), 40, 0);
    set_times(&h.join("read/src/main.rs"), 40, 0);

    let out = scan(h);
    let edited = entry_for(&out, "edited");
    assert!(!edited.selected, "{}", edited.reason);
    assert!(edited.reason.contains("active 2 days ago"), "{}", edited.reason);
    let read = entry_for(&out, "read");
    assert!(read.selected, "{}", read.reason);
    assert!(read.reason.contains("untouched for 40 days"), "{}", read.reason);
}

#[test]
fn recent_commit_keeps_active() {
    let tmp = tempfile::tempdir().unwrap();
    let h = tmp.path();
    let p = h.join("p");
    git_init(&p);
    mkfile(&p.join("Cargo.toml"), 10);
    mkfile(&p.join("src/main.rs"), 10);
    git_commit_now(&p);
    mkfile(&p.join("target/debug/x"), 1000);
    set_age(&p.join("Cargo.toml"), 60);
    set_age(&p.join("src/main.rs"), 60);

    let out = scan(h);
    let e = entry_for(&out, "p");
    assert!(!e.selected, "{}", e.reason);
}

#[test]
fn custom_rule() {
    let tmp = tempfile::tempdir().unwrap();
    let h = tmp.path();
    mkfile(&h.join("z/build.zig"), 10);
    mkfile(&h.join("z/out/bin"), 100);

    let mut cfg = Config::default();
    cfg.artifacts.extra_rules = vec!["out@build.zig".into()];
    let out = scan_with(h, cfg);
    let z = entry_for(&out, "z");
    assert_eq!(targets(z), vec![h.join("z/out")]);
}

#[test]
fn vendored_not_descended() {
    let tmp = tempfile::tempdir().unwrap();
    let h = tmp.path();
    mkfile(&h.join("a/vendored/b/package.json"), 10);
    mkfile(&h.join("a/vendored/b/node_modules/x.js"), 100);

    let out = scan(h);
    assert!(out.entries.is_empty(), "{:?}", out.entries);
}

#[test]
fn pycache_found_at_depth_but_not_inside_venv() {
    let tmp = tempfile::tempdir().unwrap();
    let h = tmp.path();
    mkfile(&h.join("py/pyproject.toml"), 10);
    mkfile(&h.join("py/__pycache__/a.pyc"), 100);
    mkfile(&h.join("py/pkg/sub/__pycache__/b.pyc"), 100);
    mkfile(&h.join("py/.venv/lib/site/__pycache__/c.pyc"), 100);

    let out = scan(h);
    let mut all: Vec<PathBuf> = out.entries.iter().flat_map(targets).collect();
    all.sort();
    assert_eq!(
        all,
        vec![h.join("py/.venv"), h.join("py/__pycache__"), h.join("py/pkg/sub/__pycache__")]
    );
}

#[test]
fn tracked_since_plan_not_deleted() {
    let tmp = tempfile::tempdir().unwrap();
    let h = tmp.path();
    let p = h.join("p");
    git_init(&p);
    mkfile(&p.join("package.json"), 10);
    git_commit_all(&p);
    let a = p.join("node_modules/a.js");
    mkfile(&a, 10);
    set_age(&a, 60);

    let out = scan(h);
    let e = entry_for(&out, "p");
    assert_eq!(targets(e), vec![p.join("node_modules")]);

    git_commit_all(&p); // node_modules is now tracked; no working-tree mtime changes
    let res = with_ctx(h, Config::default(), |ctx| Artifacts.apply(e, ctx));
    assert!(matches!(&res, Outcome::Skipped { reason } if reason == "tracked in git"), "{res:?}");
    assert!(a.exists(), "tracked file deleted");
}

/// Runs git in `dir` with a fixed identity, isolated from the user's config.
fn git_fixed(dir: &Path, args: &[&str]) {
    let st = Command::new("git")
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
        .status()
        .unwrap();
    assert!(st.success(), "git {args:?} failed in {}", dir.display());
}

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

/// `home/src/r` (a Rust repo ignoring `target`) with branch `w` merged into
/// main and checked out at `home/wt/w`, which holds a built `target/`.
fn merged_rust_worktree(home: &Path) -> PathBuf {
    let repo = home.join("src/r");
    git_init(&repo);
    mkfile(&repo.join(".gitignore"), 8);
    std::fs::write(repo.join(".gitignore"), "target\n").unwrap();
    mkfile(&repo.join("Cargo.toml"), 10);
    git_commit_all(&repo);
    let wt = home.join("wt/w");
    git_fixed(&repo, &["worktree", "add", "-q", wt.to_str().unwrap(), "-b", "w"]);
    mkfile(&wt.join("w.txt"), 100);
    git_commit_all(&wt);
    git_fixed(&repo, &["merge", "-q", "--ff-only", "w"]);
    mkfile(&wt.join("target/debug/app"), 5000);
    wt
}

#[test]
fn finished_worktree_artifacts_preselected() {
    let tmp = tempfile::tempdir().unwrap();
    let h = tmp.path();
    merged_rust_worktree(h);
    age_tree(&h.join("wt"), 1);

    let out = scan(h);
    let e = entry_for(&out, "wt/w");
    assert!(e.selected, "{e:?}");
    assert!(e.reason.starts_with("active 1 days ago"), "{}", e.reason);
    assert!(e.reason.ends_with("; worktree is finished"), "{}", e.reason);
}

#[test]
fn unfinished_worktree_artifacts_stay_unselected() {
    let tmp = tempfile::tempdir().unwrap();
    let h = tmp.path();
    let wt = merged_rust_worktree(h);
    mkfile(&wt.join("dirty.txt"), 5); // uncommitted work: not finished
    age_tree(&h.join("wt"), 1);

    let out = scan(h);
    let e = entry_for(&out, "wt/w");
    assert!(!e.selected, "{e:?}");
    assert!(!e.reason.contains("worktree is finished"), "{}", e.reason);
}

#[test]
fn trunk_worktree_artifacts_not_preselected() {
    let tmp = tempfile::tempdir().unwrap();
    let h = tmp.path();
    let repo = h.join("src/r");
    git_init(&repo);
    std::fs::write(repo.join(".gitignore"), "target\n").unwrap();
    mkfile(&repo.join("Cargo.toml"), 10);
    git_commit_all(&repo);
    git_fixed(&repo, &["checkout", "-q", "--detach"]);
    let wt = h.join("wt/w");
    git_fixed(&repo, &["worktree", "add", "-q", wt.to_str().unwrap(), "main"]);
    mkfile(&wt.join("target/debug/app"), 5000);
    age_tree(&h.join("wt"), 1);

    let out = scan(h);
    let e = entry_for(&out, "wt/w");
    assert!(!e.selected, "{e:?}");
    assert!(!e.reason.contains("worktree is finished"), "{}", e.reason);
}

#[test]
fn every_project_in_a_finished_worktree_is_preselected() {
    let tmp = tempfile::tempdir().unwrap();
    let h = tmp.path();
    let wt = merged_rust_worktree(h);
    mkfile(&wt.join("crates/b/Cargo.toml"), 10);
    git_commit_all(&wt);
    git_fixed(&h.join("src/r"), &["merge", "-q", "--ff-only", "w"]);
    mkfile(&wt.join("crates/b/target/debug/b"), 5000);
    age_tree(&h.join("wt"), 1);

    let out = scan(h);
    for title in ["wt/w", "wt/w/crates/b"] {
        let e = entry_for(&out, title);
        assert!(e.selected, "{e:?}");
        assert!(e.reason.ends_with("; worktree is finished"), "{}", e.reason);
    }
}

// ---- plan 4: composer, elixir, haskell, stack, godot, jupyter ----

/// Creates each file (10 bytes) and ages it `days`, then ages each project dir.
fn aged(h: &Path, files: &[&str], days: u64) {
    for f in files {
        mkfile(&h.join(f), 10);
        set_age(&h.join(f), days);
    }
}

fn names(e: &Entry) -> std::collections::BTreeSet<String> {
    targets(e).iter().map(|p| p.file_name().unwrap().to_string_lossy().into_owned()).collect()
}

fn set(v: &[&str]) -> std::collections::BTreeSet<String> {
    v.iter().map(|s| s.to_string()).collect()
}

#[test]
fn plan4_ecosystems_detected() {
    let tmp = tempfile::tempdir().unwrap();
    let h = tmp.path();
    aged(
        h,
        &[
            "c/composer.json",
            "c/vendor/x",
            "e/mix.exs",
            "e/_build/x",
            "e/deps/x",
            "h/a.cabal",
            "h/dist-newstyle/x",
            "s/stack.yaml",
            "s/.stack-work/x",
            "g/project.godot",
            "g/.godot/x",
            "g/.import/x",
        ],
        40,
    );
    let out = scan(h);
    assert_eq!(out.entries.len(), 5, "{:?}", out.entries);
    for (title, want) in [
        ("c", &["vendor"][..]),
        ("e", &["_build", "deps"]),
        ("h", &["dist-newstyle"]),
        ("s", &[".stack-work"]),
        ("g", &[".godot", ".import"]),
    ] {
        let e = entry_for(&out, title);
        assert!(e.selected, "{title}: {e:?}");
        assert_eq!(names(e), set(want), "{title}");
    }
}

#[test]
fn jupyter_checkpoints_never_preselected() {
    let tmp = tempfile::tempdir().unwrap();
    let h = tmp.path();
    aged(h, &["n/a.ipynb", "n/.ipynb_checkpoints/a-checkpoint.ipynb"], 400);
    let out = scan(h);
    assert_eq!(out.entries.len(), 1, "{:?}", out.entries);
    let e = &out.entries[0];
    assert!(!e.selected);
    assert_eq!(e.reason, "jupyter artifacts are never pre-selected; untouched for 400 days");
}

#[test]
fn jupyter_without_notebook_not_listed() {
    let tmp = tempfile::tempdir().unwrap();
    let h = tmp.path();
    aged(h, &["m/.ipynb_checkpoints/x"], 400);
    let out = scan(h);
    assert!(out.entries.is_empty(), "{:?}", out.entries);
}

#[test]
fn jupyter_split_from_ticked() {
    let tmp = tempfile::tempdir().unwrap();
    let h = tmp.path();
    aged(h, &["p/pyproject.toml", "p/.venv/x", "p/a.ipynb", "p/.ipynb_checkpoints/x"], 40);
    let out = scan(h);
    let ps: Vec<&Entry> = out.entries.iter().filter(|e| e.title == "p").collect();
    assert_eq!(ps.len(), 2, "{:?}", out.entries);
    let ticked = ps.iter().find(|e| e.selected).expect("selected entry");
    let unticked = ps.iter().find(|e| !e.selected).expect("unselected entry");
    assert_eq!(names(ticked), set(&[".venv"]));
    assert_eq!(names(unticked), set(&[".ipynb_checkpoints"]));
    assert_eq!(unticked.reason, "jupyter artifacts are never pre-selected; untouched for 40 days");
}

#[test]
fn tracked_vendor_skipped() {
    let tmp = tempfile::tempdir().unwrap();
    let h = tmp.path();
    let p = h.join("p");
    git_init(&p);
    mkfile(&p.join("composer.json"), 10);
    mkfile(&p.join("vendor/x"), 10);
    git_commit_all(&p);
    let out = scan(h);
    assert!(out.entries.iter().all(|e| !names(e).contains("vendor")), "{:?}", out.entries);
    assert!(out.skipped.iter().any(|s| s.reason == "tracked in git"), "{:?}", out.skipped);
}
