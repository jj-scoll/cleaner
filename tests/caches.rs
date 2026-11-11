mod common;

use cleaner::category::caches::Caches;
use cleaner::category::store::{StoreRule, Unit, apply_rule, evaluate, top_matches};
use cleaner::category::{Category, Outcome, ScanCtx};
use cleaner::config::{Config, KeepNewestCfg};
use cleaner::fsutil::measure::InodeSet;
use cleaner::object::Operation;
use cleaner::plan::{Entry, EntryKind};
use cleaner::protect::Protector;
use cleaner::tool::Runner;
use common::{FakeRunner, err_out, mkfile, now, ok_out, set_age};
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};

/// Sets the mtime of `root` and everything under it to `days` ago,
/// children before parents (so the parent's mtime is not bumped after).
fn age_tree(root: &Path, days: u64) {
    if root.is_dir() {
        for e in std::fs::read_dir(root).unwrap() {
            age_tree(&e.unwrap().path(), days);
        }
    }
    set_age(root, days);
}

fn rule(root: &Path, days: u64, unit: Unit) -> StoreRule {
    StoreRule { root: root.to_path_buf(), max_age_days: days, unit, glob: None, min_bytes: None }
}

fn matches(rule: &StoreRule, home: &Path) -> Vec<PathBuf> {
    let prot = Protector::new(home, &[]);
    let mut v: Vec<PathBuf> = evaluate(rule, now(), &prot, &mut InodeSet::default())
        .into_iter()
        .map(|m| m.path)
        .collect();
    v.sort();
    v
}

fn with_ctx<R>(home: &Path, config: &Config, f: impl FnOnce(&ScanCtx) -> R) -> R {
    with_tools(home, config, &FakeRunner::new(), f)
}

fn with_tools<R>(
    home: &Path,
    config: &Config,
    tools: &dyn Runner,
    f: impl FnOnce(&ScanCtx) -> R,
) -> R {
    let protector = Protector::new(home, &config.protect);
    let ctx = ScanCtx { home, now: now(), config, protector: &protector, tools, euid: common::my_uid(), usage: None };
    f(&ctx)
}

#[test]
fn unit_parses() {
    assert_eq!("files".parse::<Unit>().unwrap(), Unit::Files { recursive: false });
    assert_eq!("files-recursive".parse::<Unit>().unwrap(), Unit::Files { recursive: true });
    assert_eq!("depth:1".parse::<Unit>().unwrap(), Unit::Depth(1));
    assert_eq!("name-contains:@".parse::<Unit>().unwrap(), Unit::NameContains("@".into()));
    assert!("depth:x".parse::<Unit>().is_err());
    assert!("bogus".parse::<Unit>().is_err());
}

#[test]
fn files_recursive_by_age() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().join("go-build");
    let old = root.join("aa/old");
    let new = root.join("aa/new");
    mkfile(&old, 100);
    mkfile(&new, 100);
    set_age(&old, 40);
    set_age(&new, 1);
    let r = rule(&root, 30, Unit::Files { recursive: true });
    assert_eq!(matches(&r, tmp.path()), vec![old]);
}

#[test]
fn depth_unit_uses_newest_inside() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().join("terragrunt");
    mkfile(&root.join("modA/a/f1"), 100);
    mkfile(&root.join("modA/f2"), 100);
    mkfile(&root.join("modB/old"), 100);
    mkfile(&root.join("modB/deep/new"), 100);
    age_tree(&root.join("modA"), 40);
    age_tree(&root.join("modB"), 40);
    set_age(&root.join("modB/deep/new"), 1);
    let r = rule(&root, 30, Unit::Depth(1));
    assert_eq!(matches(&r, tmp.path()), vec![root.join("modA")]);
}

#[test]
fn name_contains_go_modules() {
    let tmp = tempfile::tempdir().unwrap();
    let home = tmp.path();
    let root = home.join("mod");
    let module = root.join("github.com/x/y@v1.0.0");
    mkfile(&module.join("go.mod"), 50);
    mkfile(&module.join("sub/a.go"), 500);
    age_tree(&root, 40);
    for d in [module.join("sub"), module.clone()] {
        std::fs::set_permissions(&d, std::fs::Permissions::from_mode(0o555)).unwrap();
    }
    let r = rule(&root, 90, Unit::NameContains("@".into()));
    // 40 days is not old enough for a 90-day rule.
    assert!(matches(&r, home).is_empty());

    let r = rule(&root, 30, Unit::NameContains("@".into()));
    assert_eq!(matches(&r, home), vec![module.clone()]);

    let config = Config::default();
    let out = with_ctx(home, &config, |ctx| apply_rule(&r, ctx));
    assert!(matches!(out, Outcome::Deleted { bytes, .. } if bytes > 0), "{out:?}");
    assert!(!module.exists());
    assert!(root.join("github.com/x").exists());
}

#[test]
fn keep_newest_one_run() {
    let tmp = tempfile::tempdir().unwrap();
    let home = tmp.path();
    let runs = home.join("runs");
    mkfile(&runs.join("a/out.log"), 100);
    mkfile(&runs.join("b/out.log"), 100);
    age_tree(&runs.join("a"), 5);
    age_tree(&runs.join("b"), 1);
    let mut config = Config::default();
    config.caches.stores.clear();
    config.caches.keep_newest =
        vec![KeepNewestCfg { path: runs.to_str().unwrap().into(), keep: 1 }];
    let out = with_ctx(home, &config, |ctx| Caches.scan(ctx, &mut InodeSet::default())).unwrap();
    assert_eq!(out.entries.len(), 1, "{:?}", out.entries);
    let e = &out.entries[0];
    assert!(e.selected);
    assert_eq!(e.reason, "older run; keeping newest 1");
    let EntryKind::Item { targets } = &e.kind else { panic!("not an item") };
    assert_eq!(targets.len(), 1);
    assert_eq!(targets[0].path, runs.join("a"));
}

#[test]
fn caches_scan_entry_shape() {
    let tmp = tempfile::tempdir().unwrap();
    let home = tmp.path();
    let pip = home.join(".cache/pip");
    mkfile(&pip.join("x"), 5000);
    set_age(&pip.join("x"), 40);
    let config = Config::default();
    let out = with_ctx(home, &config, |ctx| Caches.scan(ctx, &mut InodeSet::default())).unwrap();
    assert_eq!(out.entries.len(), 1, "{:?}", out.entries);
    let e = &out.entries[0];
    assert_eq!(e.category, "caches");
    assert!(e.selected);
    assert!(e.bytes > 0);
    assert_eq!(e.title, pip.display().to_string());
    assert_eq!(e.reason, "1 items untouched for 30+ days");
    let EntryKind::StoreRule { rule } = &e.kind else { panic!("not a store rule") };
    assert_eq!(rule.root, pip);
    assert_eq!(rule.unit, Unit::Files { recursive: true });
}

#[test]
fn top_matches_sorted_and_capped() {
    let tmp = tempfile::tempdir().unwrap();
    let home = tmp.path();
    let root = home.join("store");
    for kib in 1..=5 {
        let f = root.join(format!("f{kib}"));
        mkfile(&f, kib * 1024);
        set_age(&f, 40);
    }
    let r = rule(&root, 30, Unit::Files { recursive: false });
    let config = Config::default();
    let top = with_ctx(home, &config, |ctx| top_matches(&r, ctx, 3));
    assert_eq!(top.len(), 3, "{top:?}");
    let bytes: Vec<u64> = top.iter().map(|m| m.bytes).collect();
    assert!(bytes.windows(2).all(|w| w[0] >= w[1]), "{bytes:?}");
    // The largest file is first, whatever the block size.
    assert_eq!(top[0].path, root.join("f5"));
}

#[test]
fn apply_rule_reevaluates() {
    let tmp = tempfile::tempdir().unwrap();
    let home = tmp.path();
    let root = home.join("store");
    let f = root.join("f");
    mkfile(&f, 100);
    set_age(&f, 40);
    let r = rule(&root, 30, Unit::Files { recursive: false });
    assert_eq!(matches(&r, home), vec![f.clone()]);
    set_age(&f, 0);
    let config = Config::default();
    let out = with_ctx(home, &config, |ctx| apply_rule(&r, ctx));
    assert!(matches!(out, Outcome::Skipped { .. }), "{out:?}");
    assert!(f.exists());
}

fn uv_entry() -> Entry {
    Entry {
        id: "caches-uv".into(),
        category: "caches".into(),
        title: "uv cache prune".into(),
        kind: EntryKind::Operation { op: Operation::UvCachePrune },
        bytes: 0,
        reason: "removes cache entries no venv uses (uv decides)".into(),
        selected: true,
        permanent: false,
        note: None,
    }
}

/// A FakeRunner whose `uv cache dir` names `dir` (created, no lock held).
fn uv_tools(dir: &Path) -> FakeRunner {
    std::fs::create_dir_all(dir).unwrap();
    std::fs::write(dir.join(".lock"), "").unwrap();
    FakeRunner::new().respond("uv cache dir", ok_out(&format!("{}\n", dir.display())))
}

/// Holds a shared flock on `<dir>/.lock`, as a running `uvx` does.
fn hold_uv_lock(dir: &Path) -> std::fs::File {
    let f = std::fs::File::open(dir.join(".lock")).unwrap();
    f.lock_shared().unwrap();
    f
}

#[test]
fn uv_prune_skipped_while_uv_holds_its_lock() {
    let tmp = tempfile::tempdir().unwrap();
    let cache = tmp.path().join("uvcache");
    let tools = uv_tools(&cache).respond("uv cache prune", ok_out("Removed 1 file (1B)\n"));
    let _held = hold_uv_lock(&cache);
    let config = Config::default();
    let got = with_tools(tmp.path(), &config, &tools, |ctx| Caches.apply(&uv_entry(), ctx));
    assert_eq!(got, Outcome::Skipped { reason: "uv cache is in use by running uv processes".into() });
    assert_eq!(tools.calls(), vec!["uv cache dir".to_string()], "never prunes, never --force");
}

#[test]
fn uv_prune_note_says_cache_in_use() {
    let tmp = tempfile::tempdir().unwrap();
    let home = tmp.path().join("home");
    std::fs::create_dir_all(&home).unwrap();
    let cache = tmp.path().join("uvcache");
    let tools = uv_tools(&cache);
    let _held = hold_uv_lock(&cache);
    let config = Config::default();
    let out =
        with_tools(&home, &config, &tools, |ctx| Caches.scan(ctx, &mut InodeSet::default())).unwrap();
    let e = out.entries.iter().find(|e| e.title == "uv cache prune").expect("uv entry");
    let note = e.note.as_deref().unwrap();
    assert!(note.ends_with("; in use by running uv processes now, so apply may skip it"), "{note}");
}

#[test]
fn uv_prune_planned() {
    let tmp = tempfile::tempdir().unwrap();
    let home = tmp.path().join("home");
    std::fs::create_dir_all(&home).unwrap();
    let cache = tmp.path().join("uvcache");
    mkfile(&cache.join("archive-v0/blob"), 8192);
    let tools = FakeRunner::new().respond("uv cache dir", ok_out(&format!("{}\n", cache.display())));
    let config = Config::default();
    let out =
        with_tools(&home, &config, &tools, |ctx| Caches.scan(ctx, &mut InodeSet::default())).unwrap();
    let e = out.entries.iter().find(|e| e.title == "uv cache prune").expect("uv entry");
    assert_eq!(e.kind, EntryKind::Operation { op: Operation::UvCachePrune });
    assert!(e.selected);
    assert!(e.is_estimate());
    assert_eq!(e.bytes, 0);
    assert_eq!(e.reason, "removes cache entries no venv uses (uv decides)");
    let note = e.note.as_deref().unwrap();
    assert!(note.starts_with("uv reports the size after it runs; "), "{note}");
    assert!(note.ends_with("cache is 8.0 KiB"), "{note}");
    assert!(out.skipped.iter().all(|s| s.title != "uv cache prune"), "{:?}", out.skipped);
}

#[test]
fn uv_missing_skips() {
    let tmp = tempfile::tempdir().unwrap();
    let config = Config::default();
    let tools = FakeRunner::new();
    let out = with_tools(tmp.path(), &config, &tools, |ctx| {
        Caches.scan(ctx, &mut InodeSet::default())
    })
    .unwrap();
    assert!(out.entries.iter().all(|e| e.title != "uv cache prune"), "{:?}", out.entries);
    let skip = out.skipped.iter().find(|s| s.title == "uv cache prune").expect("uv skip");
    assert_eq!(skip.category, "caches");
    assert_eq!(skip.reason, "uv is not installed");
}

#[test]
fn uv_cache_protected_skips() {
    let tmp = tempfile::tempdir().unwrap();
    let home = tmp.path();
    let cache = home.join(".cache/uv");
    mkfile(&cache.join("blob"), 100);
    let mut config = Config::default();
    config.protect.push(cache.to_str().unwrap().into());
    let tools = FakeRunner::new().respond("uv cache dir", ok_out(cache.to_str().unwrap()));
    let out =
        with_tools(home, &config, &tools, |ctx| Caches.scan(ctx, &mut InodeSet::default())).unwrap();
    assert!(out.entries.iter().all(|e| e.title != "uv cache prune"), "{:?}", out.entries);
    let skip = out.skipped.iter().find(|s| s.title == "uv cache prune").expect("uv skip");
    assert_eq!(skip.reason, "protected");
}

#[test]
fn uv_prune_reports_freed() {
    let tmp = tempfile::tempdir().unwrap();
    let config = Config::default();
    let tools = uv_tools(&tmp.path().join("uvcache"))
        .respond("uv cache prune", ok_out("Pruning cache at: /x\nRemoved 120 files (12.5MiB)\n"));
    let out = with_tools(tmp.path(), &config, &tools, |ctx| Caches.apply(&uv_entry(), ctx));
    assert_eq!(out, Outcome::Deleted { bytes: 13_107_200, failures: vec![] });
    assert_eq!(tools.calls(), vec!["uv cache dir".to_string(), "uv cache prune".to_string()]);
}

#[test]
fn uv_prune_reads_stderr_and_nothing_to_do() {
    let tmp = tempfile::tempdir().unwrap();
    let config = Config::default();
    let mut out = ok_out("");
    out.stderr = "Removed 3 files (1.5KiB)\n".into();
    let tools = uv_tools(&tmp.path().join("uvcache")).respond("uv cache prune", out);
    let got = with_tools(tmp.path(), &config, &tools, |ctx| Caches.apply(&uv_entry(), ctx));
    assert_eq!(got, Outcome::Deleted { bytes: 1536, failures: vec![] });

    let tools = uv_tools(&tmp.path().join("uvcache")).respond("uv cache prune", ok_out("No unused entries found\n"));
    let got = with_tools(tmp.path(), &config, &tools, |ctx| Caches.apply(&uv_entry(), ctx));
    assert_eq!(got, Outcome::Deleted { bytes: 0, failures: vec![] });
}

#[test]
fn uv_prune_nonzero_is_failed() {
    let tmp = tempfile::tempdir().unwrap();
    let config = Config::default();
    let tools =
        uv_tools(&tmp.path().join("uvcache")).respond("uv cache prune", err_out(2, "error: lock held\nmore\n"));
    let got = with_tools(tmp.path(), &config, &tools, |ctx| Caches.apply(&uv_entry(), ctx));
    assert_eq!(got, Outcome::Failed { error: "uv cache prune failed: error: lock held".into() });
}

#[test]
fn uv_prune_timeout_is_failed() {
    let tmp = tempfile::tempdir().unwrap();
    let config = Config::default();
    let tools = uv_tools(&tmp.path().join("uvcache")).timeout("uv cache prune", 600);
    let got = with_tools(tmp.path(), &config, &tools, |ctx| Caches.apply(&uv_entry(), ctx));
    let Outcome::Failed { error } = got else { panic!("not failed: {got:?}") };
    assert!(error.contains("timed out"), "{error}");
    assert_eq!(error, "uv cache prune timed out after 600s (uv cache in use?)");
}

#[test]
fn uv_prune_off() {
    let tmp = tempfile::tempdir().unwrap();
    let mut config = Config::default();
    config.caches.uv_prune = false;
    config.caches.go_clean = false;
    let tools = FakeRunner::new().respond("uv cache dir", ok_out("/nonexistent"));
    let out = with_tools(tmp.path(), &config, &tools, |ctx| {
        Caches.scan(ctx, &mut InodeSet::default())
    })
    .unwrap();
    assert!(out.entries.iter().all(|e| e.title != "uv cache prune"));
    assert!(out.skipped.iter().all(|s| s.title != "uv cache prune"));
    assert!(tools.calls().is_empty(), "{:?}", tools.calls());
}

fn caches_scan(home: &Path) -> cleaner::category::ScanOutput {
    let config = Config::default();
    with_ctx(home, &config, |ctx| Caches.scan(ctx, &mut InodeSet::default())).unwrap()
}

fn store_entry<'a>(out: &'a cleaner::category::ScanOutput, root: &Path) -> Option<&'a Entry> {
    out.entries.iter().find(|e| matches!(&e.kind, EntryKind::StoreRule { rule } if rule.root == root))
}

fn entry_matches(e: &Entry, home: &Path) -> Vec<PathBuf> {
    let EntryKind::StoreRule { rule } = &e.kind else { panic!("not a store rule") };
    matches(rule, home)
}

#[test]
fn default_stores_plan4() {
    let v = cleaner::config::default_stores();
    let has = |p: &str, d: u64, u: &str| v.iter().any(|s| s.path == p && s.max_age_days == d && s.unit == u);
    assert!(has("~/.cache/terragrunt*/providers", 30, "depth:4"));
    assert!(has("~/.cache/tofu-plugins*", 30, "depth:4"));
    assert!(v.iter().all(|s| s.path != "~/.cache/terragrunt*"));
}

#[test]
fn terragrunt_provider_version_is_the_unit() {
    let tmp = tempfile::tempdir().unwrap();
    let home = tmp.path();
    let tg = home.join(".cache/terragrunt");
    let old = tg.join("providers/registry.terraform.io/hashicorp/aws/6.58.0");
    let new = tg.join("providers/registry.terraform.io/hashicorp/aws/6.72.0");
    mkfile(&old.join("f"), 1000);
    mkfile(&new.join("f"), 1000);
    mkfile(&tg.join("cas/x"), 1000);
    age_tree(&tg, 40);
    age_tree(&new, 1);
    let out = caches_scan(home);
    let e = store_entry(&out, &tg.join("providers")).expect("providers entry");
    assert_eq!(e.title, format!("{}/.cache/terragrunt/providers", home.display()));
    assert_eq!(entry_matches(e, home), vec![old]);
    assert!(out.entries.iter().all(|e| !e.title.ends_with("cas")), "{:?}", out.entries);
}

#[test]
fn tofu_plugins_version_is_the_unit() {
    let tmp = tempfile::tempdir().unwrap();
    let home = tmp.path();
    let root = home.join(".cache/tofu-plugins-nb");
    mkfile(&root.join("registry.opentofu.org/datadog/datadog/4.25.0/f"), 1000);
    age_tree(&root, 40);
    let out = caches_scan(home);
    let e = store_entry(&out, &root).expect("tofu entry");
    let m = entry_matches(e, home);
    assert_eq!(m.len(), 1, "{m:?}");
    assert!(m[0].ends_with("4.25.0"));
}

/// Defaults plus the two rules a verify tool's run folder needs: its
/// per-run tg-cache store, and keeping only the newest run.
fn verify_tool_config() -> Config {
    let mut config = Config::default();
    config.caches.stores.push(cleaner::config::StoreCfg {
        path: "~/.cache/verify-tool/runs/*/tg-cache".into(),
        max_age_days: 3,
        unit: "depth:1".into(),
        glob: None,
        min_bytes: None,
    });
    config.caches.keep_newest.push(KeepNewestCfg { path: "~/.cache/verify-tool/runs".into(), keep: 1 });
    config
}

fn verify_tool_scan(home: &Path) -> cleaner::category::ScanOutput {
    let config = verify_tool_config();
    with_ctx(home, &config, |ctx| Caches.scan(ctx, &mut InodeSet::default())).unwrap()
}

fn tg_run(home: &Path, run: &str, days: u64) -> PathBuf {
    let r = home.join(".cache/verify-tool/runs").join(run);
    mkfile(&r.join("report.md"), 100);
    mkfile(&r.join("tg-cache/emc-prod/m/x"), 1000);
    age_tree(&r, days);
    r
}

#[test]
fn tg_cache_store_matches_after_3_days() {
    let tmp = tempfile::tempdir().unwrap();
    let home = tmp.path();
    let r1 = tg_run(home, "r1", 4);
    let out = verify_tool_scan(home);
    let e = store_entry(&out, &r1.join("tg-cache")).expect("tg-cache entry");
    assert!(e.selected);
    assert_eq!(entry_matches(e, home), vec![r1.join("tg-cache/emc-prod")]);
    assert!(out.entries.iter().all(|e| matches!(e.kind, EntryKind::StoreRule { .. })), "{:?}", out.entries);

    let tmp = tempfile::tempdir().unwrap();
    let r1 = tg_run(tmp.path(), "r1", 2);
    let out = verify_tool_scan(tmp.path());
    assert!(store_entry(&out, &r1.join("tg-cache")).is_none(), "{:?}", out.entries);
    assert!(out.entries.is_empty(), "{:?}", out.entries);
}

#[test]
fn tg_cache_not_double_listed() {
    let tmp = tempfile::tempdir().unwrap();
    let home = tmp.path();
    let r1 = tg_run(home, "r1", 10);
    let r2 = tg_run(home, "r2", 5);
    let out = verify_tool_scan(home);
    let r1s = r1.display().to_string();
    let mentions: Vec<&Entry> = out.entries.iter().filter(|e| e.title.contains(&r1s)).collect();
    assert_eq!(mentions.len(), 1, "{:?}", out.entries);
    assert!(matches!(&mentions[0].kind, EntryKind::Item { targets } if targets[0].path == r1));
    assert!(store_entry(&out, &r2.join("tg-cache")).is_some());
    assert_eq!(out.entries.len(), 2, "{:?}", out.entries);
}

#[test]
fn terragrunt_inf_root_owned_is_skip() {
    let tmp = tempfile::tempdir().unwrap();
    let home = tmp.path();
    let root = home.join(".cache/terragrunt-inf/providers");
    mkfile(&root.join("h/n/aws/6.58.0/f"), 1000);
    age_tree(&home.join(".cache/terragrunt-inf"), 40);
    let config = Config::default();
    let protector = Protector::new(home, &config.protect);
    let tools = FakeRunner::new();
    let ctx = ScanCtx {
        home,
        now: now(),
        config: &config,
        protector: &protector,
        tools: &tools,
        euid: common::my_uid() + 1,
        usage: None,
    };
    let out = Caches.scan(&ctx, &mut InodeSet::default()).unwrap();
    assert!(store_entry(&out, &root).is_none(), "{:?}", out.entries);
    let title = root.display().to_string();
    let skips: Vec<_> = out.skipped.iter().filter(|s| s.title == title).collect();
    assert_eq!(skips.len(), 1, "{:?}", out.skipped);
    assert_eq!(skips[0].reason, cleaner::category::store::foreign_reason(common::my_uid()));
}

const GO_REASON: &str = "removes the whole Go module cache, including modules in use";

fn go_entry() -> Entry {
    Entry {
        id: "caches-go".into(),
        category: "caches".into(),
        title: "go clean -modcache".into(),
        kind: EntryKind::Operation { op: Operation::GoCleanModcache },
        bytes: 8192,
        reason: GO_REASON.into(),
        selected: false,
        permanent: false,
        note: None,
    }
}

/// A home holding an 8 KiB Go module cache, and a FakeRunner naming it.
fn go_tools(home: &Path) -> (PathBuf, FakeRunner) {
    let dir = home.join("go/pkg/mod");
    mkfile(&dir.join("x@v1/f"), 8192);
    let tools = FakeRunner::new().respond("go env GOMODCACHE", ok_out(&format!("{}\n", dir.display())));
    (dir, tools)
}

#[test]
fn go_modcache_row_unticked() {
    let tmp = tempfile::tempdir().unwrap();
    let home = tmp.path();
    let (dir, tools) = go_tools(home);
    let mut config = Config::default();
    config.caches.uv_prune = false;
    let out =
        with_tools(home, &config, &tools, |ctx| Caches.scan(ctx, &mut InodeSet::default())).unwrap();
    let e = out.entries.iter().find(|e| e.title == "go clean -modcache").expect("go entry");
    assert_eq!(e.kind, EntryKind::Operation { op: Operation::GoCleanModcache });
    assert!(!e.selected);
    assert!(e.is_estimate());
    assert_eq!(e.bytes, cleaner::fsutil::measure::measure(&dir, &mut InodeSet::default()).unwrap().bytes);
    assert!(e.bytes >= 8192);
    assert_eq!(e.reason, GO_REASON);
    assert_eq!(e.note.as_deref(), Some("go decides what goes; size is the whole cache"));
    assert_eq!(tools.calls(), vec!["go env GOMODCACHE".to_string()]);
}

#[test]
fn go_missing_is_skip() {
    let tmp = tempfile::tempdir().unwrap();
    let mut config = Config::default();
    config.caches.uv_prune = false;
    let tools = FakeRunner::new();
    let out = with_tools(tmp.path(), &config, &tools, |ctx| {
        Caches.scan(ctx, &mut InodeSet::default())
    })
    .unwrap();
    assert!(out.entries.iter().all(|e| e.title != "go clean -modcache"));
    let skip = out.skipped.iter().find(|s| s.title == "go clean -modcache").expect("go skip");
    assert_eq!(skip.category, "caches");
    assert_eq!(skip.reason, "go is not installed");
}

#[test]
fn go_clean_apply_runs_command() {
    let tmp = tempfile::tempdir().unwrap();
    let (_, tools) = go_tools(tmp.path());
    let tools = tools.respond("go clean -modcache", ok_out(""));
    let config = Config::default();
    let out = with_tools(tmp.path(), &config, &tools, |ctx| Caches.apply(&go_entry(), ctx));
    // The fake does not delete anything, so before == after.
    assert_eq!(out, Outcome::Deleted { bytes: 0, failures: vec![] });
    assert_eq!(tools.calls(), vec!["go env GOMODCACHE".to_string(), "go clean -modcache".to_string()]);
}

#[test]
fn go_clean_failure_is_failed() {
    let tmp = tempfile::tempdir().unwrap();
    let (_, tools) = go_tools(tmp.path());
    let tools = tools.respond("go clean -modcache", err_out(1, "permission denied\nmore"));
    let config = Config::default();
    let out = with_tools(tmp.path(), &config, &tools, |ctx| Caches.apply(&go_entry(), ctx));
    assert_eq!(out, Outcome::Failed { error: "go clean -modcache failed: permission denied".into() });
}

#[test]
fn go_clean_timeout() {
    let tmp = tempfile::tempdir().unwrap();
    let (_, tools) = go_tools(tmp.path());
    let tools = tools.timeout("go clean", 600);
    let config = Config::default();
    let out = with_tools(tmp.path(), &config, &tools, |ctx| Caches.apply(&go_entry(), ctx));
    assert_eq!(out, Outcome::Failed { error: "go clean -modcache timed out after 600s".into() });
}

#[test]
fn go_clean_apply_missing_is_skipped() {
    let tmp = tempfile::tempdir().unwrap();
    let config = Config::default();
    let tools = FakeRunner::new();
    let out = with_tools(tmp.path(), &config, &tools, |ctx| Caches.apply(&go_entry(), ctx));
    assert_eq!(out, Outcome::Skipped { reason: "go is not installed".into() });
}

#[test]
fn go_clean_off() {
    let tmp = tempfile::tempdir().unwrap();
    let mut config = Config::default();
    config.caches.uv_prune = false;
    config.caches.go_clean = false;
    let (_, tools) = go_tools(tmp.path());
    let out = with_tools(tmp.path(), &config, &tools, |ctx| {
        Caches.scan(ctx, &mut InodeSet::default())
    })
    .unwrap();
    assert!(out.entries.iter().all(|e| e.title != "go clean -modcache"));
    assert!(out.skipped.iter().all(|s| s.title != "go clean -modcache"));
    assert!(tools.calls().is_empty(), "{:?}", tools.calls());
}
