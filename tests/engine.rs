mod common;

use cleaner::category::Outcome;
use cleaner::config::Config;
use cleaner::engine::{
    Ctx, Progress, apply_plan, apply_plan_with, apply_selected, build_plan, build_plan_with,
    IndexMode, plan_and_save, plan_and_save_with, registry,
};
use cleaner::lock::acquire;
use cleaner::paths::Paths;
use cleaner::plan::Plan;
use cleaner::protect::Protector;
use common::{mkfile, now, set_age};
use std::collections::HashSet;
use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use tempfile::TempDir;

/// Home with one stale Rust project, an old pip cache file and an old Download.
fn fixture() -> (TempDir, Ctx) {
    let tmp = TempDir::new().unwrap();
    let home = tmp.path();
    let proj = home.join("proj");
    for (f, n) in [("Cargo.toml", 10), ("src/main.rs", 10), ("target/debug/app", 50_000)] {
        mkfile(&proj.join(f), n);
        set_age(&proj.join(f), 40);
    }
    let pip = home.join(".cache/pip/http/a");
    mkfile(&pip, 20_000);
    set_age(&pip, 40);
    let dl = home.join("Downloads/old.zip");
    mkfile(&dl, 20_000);
    set_age(&dl, 100);
    let ctx = Ctx {
        paths: Paths::for_home(home),
        config: Config::default(),
        protector: Protector::new(home, &[]),
        now: now(),
        tools: std::sync::Arc::new(common::FakeRunner::new()),
        usage: None,
    };
    (tmp, ctx)
}

fn plan_for(ctx: &Ctx) -> Plan {
    build_plan(ctx, &registry(&ctx.config)).unwrap()
}

fn id_of(plan: &Plan, category: &str) -> String {
    plan.entries.iter().find(|e| e.category == category).unwrap().id.clone()
}

fn outcome<'a>(report: &'a cleaner::engine::RunReport, id: &str) -> &'a Outcome {
    &report.results.iter().find(|(i, _)| i == id).unwrap().1
}

fn selected_ids(plan: &Plan) -> Vec<String> {
    plan.entries.iter().filter(|e| e.selected).map(|e| e.id.clone()).collect()
}

fn target(home: &Path) -> std::path::PathBuf {
    home.join("proj/target")
}

#[test]
fn build_plan_covers_categories() {
    let (tmp, ctx) = fixture();
    let plan = plan_for(&ctx);
    for cat in ["artifacts", "caches", "user_rules"] {
        assert!(plan.entries.iter().any(|e| e.category == cat), "no {cat} entry: {plan:#?}");
    }
    let ids: Vec<&str> = plan.entries.iter().chain(&plan.report_only).map(|e| e.id.as_str()).collect();
    assert!(ids.iter().all(|i| !i.is_empty()));
    assert_eq!(ids.iter().collect::<HashSet<_>>().len(), ids.len(), "ids not unique: {ids:?}");
    assert!(plan.selected_bytes() > 0);
    drop(tmp);
}

#[test]
fn apply_selected_deletes_and_logs() {
    let (tmp, ctx) = fixture();
    let home = tmp.path();
    let plan = plan_for(&ctx);
    let ids = selected_ids(&plan);
    let report = apply_plan(&ctx, &registry(&ctx.config), &plan, &ids).unwrap();

    assert_eq!(report.results.len(), ids.len());
    for (id, o) in &report.results {
        assert!(matches!(o, Outcome::Deleted { .. }), "{id}: {o:?}");
    }
    assert!(!target(home).exists());
    assert!(!home.join(".cache/pip/http/a").exists());
    assert!(!home.join("Downloads/old.zip").exists());

    let lines = cleaner::log::read(&ctx.paths.log_file, None).unwrap();
    assert_eq!(lines.len(), ids.len());
    assert!(lines.iter().all(|l| l.outcome == "deleted" && l.plan_id == plan.id));
    assert!(lines.iter().all(|l| l.run_id == report.run_id));
}

#[test]
fn apply_skips_missing_target() {
    let (tmp, ctx) = fixture();
    let plan = plan_for(&ctx);
    std::fs::remove_dir_all(target(tmp.path())).unwrap();
    let id = id_of(&plan, "artifacts");
    let report = apply_plan(&ctx, &registry(&ctx.config), &plan, std::slice::from_ref(&id)).unwrap();
    match outcome(&report, &id) {
        Outcome::Skipped { reason } => assert!(reason.contains("no longer exists"), "{reason}"),
        o => panic!("expected Skipped, got {o:?}"),
    }
}

#[test]
fn apply_skips_drifted_target() {
    let (tmp, ctx) = fixture();
    let plan = plan_for(&ctx);
    std::thread::sleep(std::time::Duration::from_millis(1100));
    mkfile(&target(tmp.path()).join("new.o"), 10);
    let id = id_of(&plan, "artifacts");
    let report = apply_plan(&ctx, &registry(&ctx.config), &plan, std::slice::from_ref(&id)).unwrap();
    match outcome(&report, &id) {
        Outcome::Skipped { reason } => assert!(reason.contains("changed since plan"), "{reason}"),
        o => panic!("expected Skipped, got {o:?}"),
    }
    assert!(target(tmp.path()).exists());
}

#[test]
fn apply_continues_after_failed_item() {
    let (tmp, ctx) = fixture();
    let proj = tmp.path().join("proj");
    let plan = plan_for(&ctx);
    let (art, cache) = (id_of(&plan, "artifacts"), id_of(&plan, "caches"));
    std::fs::set_permissions(&proj, std::fs::Permissions::from_mode(0o555)).unwrap();
    let report = apply_plan(&ctx, &registry(&ctx.config), &plan, &[art.clone(), cache.clone()]);
    std::fs::set_permissions(&proj, std::fs::Permissions::from_mode(0o755)).unwrap();
    let report = report.unwrap();

    match outcome(&report, &art) {
        Outcome::Skipped { .. } => {}
        Outcome::Deleted { failures, .. } => assert!(!failures.is_empty()),
        o => panic!("unexpected artifact outcome {o:?}"),
    }
    assert!(target(tmp.path()).exists());
    assert!(matches!(outcome(&report, &cache), Outcome::Deleted { .. }));
    assert!(!tmp.path().join(".cache/pip/http/a").exists());
}

#[test]
fn second_lock_fails() {
    let tmp = TempDir::new().unwrap();
    let path = tmp.path().join("run/cleaner.lock");
    let _held = acquire(&path).unwrap();
    let err = acquire(&path).err().expect("second acquire must fail");
    assert!(format!("{err:#}").contains("another cleaner run is in progress"), "{err:#}");
}

#[test]
fn build_plan_reports_progress() {
    let (_tmp, ctx) = fixture();
    let cats = registry(&ctx.config);
    let mut events: Vec<Progress> = Vec::new();
    build_plan_with(&ctx, &cats, &mut |p| events.push(p)).unwrap();
    assert_eq!(events.len(), cats.len() + 1, "{events:?}");
    assert_eq!(events[0].done, 0);
    assert_eq!(events[0].current, cats[0].id());
    let last = events.last().unwrap();
    assert_eq!((last.done, last.total), (cats.len(), cats.len()));
    assert_eq!(last.current, "");
}

#[test]
fn apply_plan_reports_progress() {
    let (_tmp, ctx) = fixture();
    let plan = plan_for(&ctx);
    let ids = [id_of(&plan, "artifacts"), id_of(&plan, "caches")];
    let mut events: Vec<Progress> = Vec::new();
    apply_plan_with(&ctx, &registry(&ctx.config), &plan, &ids, &mut |p| events.push(p)).unwrap();
    assert_eq!(events.len(), 3, "{events:?}");
    assert_eq!(events[0].done, 0);
    let last = events.last().unwrap();
    assert_eq!((last.done, last.total), (2, 2));
}

#[test]
fn apply_selected_respects_lock() {
    let (tmp, ctx) = fixture();
    let plan = plan_for(&ctx);
    let ids = selected_ids(&plan);
    let _held = acquire(&ctx.paths.lock_file).unwrap();
    let err = apply_selected(&ctx.paths, &ctx.config, &plan, &ids, &mut |_| {})
        .err()
        .expect("apply must fail while the lock is held");
    assert!(format!("{err:#}").contains("another cleaner run is in progress"), "{err:#}");
    assert!(target(tmp.path()).exists());
    assert!(tmp.path().join(".cache/pip/http/a").exists());
    assert!(tmp.path().join("Downloads/old.zip").exists());
}

/// `plan_and_save` builds its own `SystemRunner`, so switch off every
/// tool-backed category: no docker, nix, uv or go runs (spec §8, hermetic).
fn hermetic(mut ctx: Ctx) -> Ctx {
    ctx.config.docker.enabled = false;
    ctx.config.nix.enabled = false;
    ctx.config.caches.uv_prune = false;
    ctx.config.caches.go_clean = false;
    ctx
}

#[test]
fn plan_and_save_writes_plan() {
    let (_tmp, ctx) = fixture();
    let ctx = hermetic(ctx);
    let (plan, path) = plan_and_save(&ctx.paths, &ctx.config, ctx.now, &mut |_| {}).unwrap();
    assert!(path.exists(), "{}", path.display());
    assert_eq!(Plan::load(&path).unwrap().id, plan.id);
}

#[test]
fn unknown_id_errors() {
    let (tmp, ctx) = fixture();
    let plan = plan_for(&ctx);
    let ids = [id_of(&plan, "artifacts"), "nope:9".to_string()];
    assert!(apply_plan(&ctx, &registry(&ctx.config), &plan, &ids).is_err());
    // Ids are validated before anything is deleted.
    assert!(target(tmp.path()).exists());
}

#[test]
fn registry_order_plan2() {
    let ids: Vec<&str> = registry(&Config::default()).iter().map(|c| c.id()).collect();
    assert_eq!(
        ids,
        [
            "artifacts",
            "worktrees",
            "caches",
            "app_caches",
            "user_rules",
            "trash",
            "idle_folders",
            "docker",
            "nix"
        ]
    );
}

fn git_fixed(dir: &Path, args: &[&str]) {
    let st = std::process::Command::new("git")
        .args(args)
        .current_dir(dir)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        // git still reads $XDG_CONFIG_HOME/git/ignore without a global config.
        .env("GIT_CONFIG_COUNT", "1")
        .env("GIT_CONFIG_KEY_0", "core.excludesFile")
        .env("GIT_CONFIG_VALUE_0", "/dev/null")
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

#[test]
fn artifacts_then_worktree_same_run() {
    let tmp = TempDir::new().unwrap();
    let home = tmp.path();
    let repo = home.join("src/r");
    common::git_init(&repo);
    std::fs::write(repo.join(".gitignore"), "target\n").unwrap();
    mkfile(&repo.join("Cargo.toml"), 10);
    common::git_commit_all(&repo);
    let wt = home.join("wt/w");
    git_fixed(&repo, &["worktree", "add", "-q", wt.to_str().unwrap(), "-b", "w"]);
    mkfile(&wt.join("w.txt"), 100);
    common::git_commit_all(&wt);
    git_fixed(&repo, &["merge", "-q", "--ff-only", "w"]);
    mkfile(&wt.join("target/debug/app"), 50_000);
    age_tree(&home.join("wt"), 10);

    let ctx = Ctx {
        paths: Paths::for_home(home),
        config: Config::default(),
        protector: Protector::new(home, &[]),
        now: now(),
        tools: std::sync::Arc::new(common::FakeRunner::new()),
        usage: None,
    };
    let plan = plan_for(&ctx);
    let art = plan.entries.iter().find(|e| e.category == "artifacts").expect("artifacts entry");
    let wte = plan.entries.iter().find(|e| e.category == "worktrees").expect("worktree entry");
    assert!(art.selected && wte.selected, "{plan:#?}");
    assert!(art.bytes >= 50_000, "artifacts bytes {}", art.bytes);
    assert!(wte.bytes < 50_000, "worktree bytes must exclude target/: {}", wte.bytes);

    let ids = vec![art.id.clone(), wte.id.clone()];
    let report = apply_plan(&ctx, &registry(&ctx.config), &plan, &ids).unwrap();
    for id in &ids {
        assert!(matches!(outcome(&report, id), Outcome::Deleted { .. }), "{id}: {:?}", report.results);
    }
    assert!(!wt.exists());
}

fn ctx_with_roots(home: &Path, roots: Vec<String>) -> Ctx {
    Ctx {
        paths: Paths::for_home(home),
        config: Config { scan_roots: roots, ..Config::default() },
        protector: Protector::new(home, &[]),
        now: now(),
        tools: std::sync::Arc::new(common::FakeRunner::new()),
        usage: None,
    }
}

#[test]
fn symlinked_scan_root_is_a_skip() {
    for root in ["~/ws", "~/ws/"] {
        symlinked_root_case(root);
    }
}

fn symlinked_root_case(root: &str) {
    let tmp = TempDir::new().unwrap();
    let home = TempDir::new().unwrap();
    let real = tmp.path().join("real");
    for f in ["proj/Cargo.toml", "proj/target/x"] {
        mkfile(&real.join(f), 10);
    }
    age_tree(&real, 40);
    std::os::unix::fs::symlink(&real, home.path().join("ws")).unwrap();
    let ctx = ctx_with_roots(home.path(), vec![root.into()]);
    let plan = plan_for(&ctx);
    assert!(
        plan.skipped.contains(&cleaner::plan::Skip {
            category: "scan_roots".into(),
            title: root.into(),
            reason: format!("{root} is a symlink to {}; add that path instead", real.display()),
        }),
        "{root}: {:?}",
        plan.skipped
    );
    for e in plan.entries.iter().chain(&plan.report_only) {
        if let cleaner::plan::EntryKind::Item { targets } = &e.kind {
            assert!(targets.iter().all(|t| !t.path.starts_with(&real)), "{e:?}");
        }
    }
}

#[test]
fn extra_root_scanned() {
    let home = TempDir::new().unwrap();
    let extra = TempDir::new().unwrap();
    for f in ["proj/Cargo.toml", "proj/target/x"] {
        mkfile(&extra.path().join(f), 10);
    }
    age_tree(extra.path(), 40);
    let ctx = ctx_with_roots(home.path(), vec!["~".into(), extra.path().display().to_string()]);
    let plan = plan_for(&ctx);
    let arts: Vec<_> = plan.entries.iter().filter(|e| e.category == "artifacts").collect();
    assert_eq!(arts.len(), 1, "{arts:?}");
    assert!(arts[0].selected);
    match &arts[0].kind {
        cleaner::plan::EntryKind::Item { targets } => {
            assert_eq!(targets.iter().map(|t| t.path.clone()).collect::<Vec<_>>(), vec![extra.path().join("proj/target")])
        }
        k => panic!("{k:?}"),
    }
}

#[test]
fn filesystem_paths_adds_scan_root_mounts() {
    let home = TempDir::new().unwrap();
    let extra = TempDir::new().unwrap();
    let mut config = Config::default();
    let default_paths = cleaner::engine::filesystem_paths(&config, home.path());
    config.scan_roots = vec!["~".into(), extra.path().display().to_string()];
    let paths = cleaner::engine::filesystem_paths(&config, home.path());
    assert_eq!(paths[..default_paths.len()], default_paths[..], "config.filesystems come first");
    let mount = cleaner::disk::mount_point(extra.path()).unwrap();
    assert!(paths.contains(&mount), "{paths:?} lacks {mount:?}");
    let mut dedup = paths.clone();
    dedup.sort();
    dedup.dedup();
    assert_eq!(dedup.len(), paths.len(), "duplicates in {paths:?}");
}

#[test]
fn plan_and_save_writes_index() {
    let (tmp, ctx) = fixture();
    let ctx = hermetic(ctx);
    plan_and_save(&ctx.paths, &ctx.config, ctx.now, &mut |_| {}).unwrap();
    let idx = cleaner::fsutil::usage::UsageIndex::load(&ctx.paths.usage_index).unwrap().unwrap();
    let home: std::path::PathBuf = tmp.path().components().collect();
    assert!(idx.trees.iter().any(|t| t.root == home), "{:?}", idx.trees.iter().map(|t| &t.root).collect::<Vec<_>>());
    assert_eq!(idx.created, ctx.now);
}

#[test]
fn index_write_failure_is_skip() {
    let (_tmp, ctx) = fixture();
    let ctx = hermetic(ctx);
    std::fs::create_dir_all(&ctx.paths.usage_index).unwrap();
    let (plan, path) = plan_and_save(&ctx.paths, &ctx.config, ctx.now, &mut |_| {}).unwrap();
    assert!(path.exists());
    let s = plan.skipped.iter().find(|s| s.category == "disk_map").expect("disk_map skip");
    assert_eq!(s.title, "disk index");
    assert!(s.reason.starts_with("not saved:"), "{}", s.reason);
}

#[test]
fn reuse_mode_does_not_rebuild() {
    let (_tmp, ctx) = fixture();
    let ctx = hermetic(ctx);
    let old = cleaner::fsutil::usage::UsageIndex { created: 1, min_node: 5, trees: Vec::new() };
    old.save(&ctx.paths.usage_index).unwrap();
    plan_and_save_with(&ctx.paths, &ctx.config, ctx.now, IndexMode::Reuse, &mut |_| {}).unwrap();
    let after = cleaner::fsutil::usage::UsageIndex::load(&ctx.paths.usage_index).unwrap().unwrap();
    assert_eq!(after.created, 1);
}

#[test]
fn plan_and_save_reports_index_progress_first() {
    let (_tmp, ctx) = fixture();
    let ctx = hermetic(ctx);
    let cats = registry(&ctx.config).len();
    let mut events: Vec<Progress> = Vec::new();
    plan_and_save_with(&ctx.paths, &ctx.config, ctx.now, IndexMode::Rebuild, &mut |p| events.push(p)).unwrap();
    assert_eq!(events.len(), cats + 2, "{events:?}");
    assert_eq!((events[0].done, events[0].current.as_str()), (0, "disk index"));
    assert_eq!(events[1].done, 0);
    let last = events.last().unwrap();
    assert_eq!((last.done, last.total, last.current.as_str()), (cats, cats, ""));
    // Reuse walks nothing, so it reports no index step.
    let mut reuse: Vec<Progress> = Vec::new();
    plan_and_save_with(&ctx.paths, &ctx.config, ctx.now, IndexMode::Reuse, &mut |p| reuse.push(p)).unwrap();
    assert_eq!(reuse.len(), cats + 1, "{reuse:?}");
}

#[test]
fn rows_are_report_only_and_cannot_apply() {
    use cleaner::fsutil::usage::build;
    let (tmp, mut ctx) = fixture();
    let old = tmp.path().join("old");
    mkfile(&old.join("blob"), 128 * 1024);
    set_age(&old.join("blob"), 100);
    set_age(&old, 100);
    // The real index drops nodes under 50 MiB; a small min_node keeps the fixture tiny.
    ctx.usage = Some(build(&[tmp.path().to_path_buf()], &[], 16384, ctx.now));
    ctx.config.idle_folders.min_bytes = 65536;

    let plan = plan_for(&ctx);
    let row = plan.report_only.iter().find(|e| e.category == "idle_folders" && e.title == "~/old").unwrap();
    assert!(!row.selected);
    assert!(plan.entries.iter().all(|e| e.category != "idle_folders"));

    let res = apply_plan(&ctx, &registry(&ctx.config), &plan, std::slice::from_ref(&row.id));
    let err = res.err().expect("apply must refuse a report-only id");
    assert!(err.to_string().contains("unknown entry id"), "{err:#}");
    assert!(old.exists());
}
