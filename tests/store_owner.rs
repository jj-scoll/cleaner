mod common;

use cleaner::category::app_caches::AppCaches;
use cleaner::category::store::{StoreRule, Unit, apply_rule, evaluate_as, foreign_reason, scan_stores};
use cleaner::category::{Category, Outcome, ScanCtx};
use cleaner::config::{Config, StoreCfg};
use cleaner::fsutil::measure::InodeSet;
use cleaner::plan::Skip;
use cleaner::protect::Protector;
use common::{FakeRunner, mkfile, my_uid, now, set_age};
use std::path::{Path, PathBuf};

fn age_tree(root: &Path, days: u64) {
    if root.is_dir() {
        for e in std::fs::read_dir(root).unwrap() {
            age_tree(&e.unwrap().path(), days);
        }
    }
    set_age(root, days);
}

struct Fx {
    _tmp: tempfile::TempDir,
    home: PathBuf,
    root: PathBuf,
}

fn fx(days: u64) -> Fx {
    let tmp = tempfile::tempdir().unwrap();
    let home = tmp.path().join("home");
    let root = home.join(".cache/s");
    mkfile(&root.join("a/f"), 1000);
    mkfile(&root.join("b/f"), 1000);
    age_tree(&root, days);
    Fx { _tmp: tmp, home, root }
}

fn rule(root: &Path) -> StoreRule {
    StoreRule { root: root.to_path_buf(), max_age_days: 30, unit: Unit::Depth(1), glob: None, min_bytes: None }
}

fn with_ctx<R>(home: &Path, euid: u32, f: impl FnOnce(&ScanCtx) -> R) -> R {
    let config = Config::default();
    let protector = Protector::new(home, &config.protect);
    let tools = FakeRunner::new();
    let ctx = ScanCtx { home, now: now(), config: &config, protector: &protector, tools: &tools, euid, usage: None };
    f(&ctx)
}

#[test]
fn foreign_unit_is_not_a_match() {
    let f = fx(40);
    let prot = Protector::new(&f.home, &Config::default().protect);
    let r = rule(&f.root);
    let own = evaluate_as(&r, now(), &prot, &mut InodeSet::default(), my_uid());
    assert_eq!(own.matches.len(), 2);
    assert!(own.foreign.is_empty());
    let other = evaluate_as(&r, now(), &prot, &mut InodeSet::default(), my_uid() + 1);
    assert!(other.matches.is_empty());
    assert_eq!(other.foreign, vec![(f.root.join("a"), my_uid()), (f.root.join("b"), my_uid())]);
}

#[test]
fn scan_stores_skips_foreign_root_once() {
    let f = fx(40);
    let cfg = StoreCfg {
        path: f.root.display().to_string(),
        max_age_days: 30,
        unit: "depth:1".into(),
        glob: None,
        min_bytes: None,
    };
    let out = with_ctx(&f.home, my_uid() + 1, |ctx| {
        scan_stores("caches", &[cfg], ctx, &mut InodeSet::default(), &|n, d| format!("{n} {d}"), &|_| false)
    });
    assert!(out.entries.is_empty());
    assert_eq!(
        out.skipped,
        vec![Skip {
            category: "caches".into(),
            title: f.root.display().to_string(),
            reason: format!("contains files owned by uid {}; cleaner never runs as root", my_uid()),
        }]
    );
}

#[test]
fn foreign_reason_names_root() {
    assert_eq!(foreign_reason(0), "contains files owned by root; cleaner never runs as root");
    assert_eq!(foreign_reason(7), "contains files owned by uid 7; cleaner never runs as root");
}

#[test]
fn apply_rule_never_deletes_foreign() {
    let f = fx(40);
    let out = with_ctx(&f.home, my_uid() + 1, |ctx| apply_rule(&rule(&f.root), ctx));
    assert!(matches!(out, Outcome::Skipped { ref reason } if reason == "nothing matches the rule now"), "{out:?}");
    assert!(f.root.join("a").exists() && f.root.join("b").exists());
}

#[test]
fn young_foreign_unit_not_reported() {
    let f = fx(1);
    let cfg = StoreCfg {
        path: f.root.display().to_string(),
        max_age_days: 30,
        unit: "depth:1".into(),
        glob: None,
        min_bytes: None,
    };
    let out = with_ctx(&f.home, my_uid() + 1, |ctx| {
        scan_stores("caches", &[cfg], ctx, &mut InodeSet::default(), &|n, d| format!("{n} {d}"), &|_| false)
    });
    assert!(out.entries.is_empty());
    assert!(out.skipped.is_empty(), "{:?}", out.skipped);
}

#[test]
fn app_cache_foreign_dir_is_skip() {
    let tmp = tempfile::tempdir().unwrap();
    let home = tmp.path().join("home");
    let procs = tmp.path().join("proc");
    std::fs::create_dir_all(&procs).unwrap();
    let app_dir = home.join(".config/google-chrome");
    mkfile(&app_dir.join("Default/Cache/x"), 1000);
    age_tree(&home.join(".config"), 1);
    let out = with_ctx(&home, my_uid() + 1, |ctx| {
        AppCaches { proc_root: procs.clone() }.scan(ctx, &mut InodeSet::default()).unwrap()
    });
    assert!(out.entries.is_empty(), "{:?}", out.entries);
    assert_eq!(out.skipped.len(), 1, "{:?}", out.skipped);
    assert_eq!(out.skipped[0].category, "app_caches");
    assert_eq!(out.skipped[0].reason, foreign_reason(my_uid()));
}
