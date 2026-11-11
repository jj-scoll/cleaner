mod common;

use cleaner::category::store::evaluate;
use cleaner::category::trash::Trash;
use cleaner::category::user_rules::UserRules;
use cleaner::category::{Category, Outcome, ScanCtx, ScanOutput};
use cleaner::config::{Config, StoreCfg};
use cleaner::fsutil::measure::InodeSet;
use cleaner::plan::{Entry, EntryKind};
use cleaner::protect::Protector;
use common::{mkfile, now, set_age};
use std::ffi::OsStr;
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};

fn with_ctx<R>(home: &Path, config: &Config, f: impl FnOnce(&ScanCtx) -> R) -> R {
    let protector = Protector::new(home, &config.protect);
    let tools = common::FakeRunner::new();
    let ctx = ScanCtx { home, now: now(), config, protector: &protector, tools: &tools, euid: common::my_uid(), usage: None };
    f(&ctx)
}

fn scan(cat: &dyn Category, home: &Path, config: &Config) -> ScanOutput {
    with_ctx(home, config, |ctx| cat.scan(ctx, &mut InodeSet::default())).unwrap()
}

/// Paths the store-rule entry matches right now.
fn rule_matches(e: &Entry, home: &Path) -> Vec<PathBuf> {
    let EntryKind::StoreRule { rule } = &e.kind else { panic!("not a store rule") };
    let prot = Protector::new(home, &[]);
    let mut v: Vec<PathBuf> = evaluate(rule, now(), &prot, &mut InodeSet::default())
        .into_iter()
        .map(|m| m.path)
        .collect();
    v.sort();
    v
}

fn targets(e: &Entry) -> Vec<PathBuf> {
    let EntryKind::Item { targets } = &e.kind else { panic!("not an item") };
    targets.iter().map(|t| t.path.clone()).collect()
}

/// `DeletionDate` value for `days` ago, in local time as the XDG spec says.
fn deletion_date(days: i64) -> String {
    let t = jiff::Timestamp::from_second(now() - days * 86_400).unwrap();
    t.to_zoned(jiff::tz::TimeZone::system()).strftime("%Y-%m-%dT%H:%M:%S").to_string()
}

fn trash_dirs(home: &Path) -> (PathBuf, PathBuf) {
    let t = home.join(".local/share/Trash");
    (t.join("files"), t.join("info"))
}

fn write_info(home: &Path, name: &str, body: &str) -> PathBuf {
    let (_, info) = trash_dirs(home);
    std::fs::create_dir_all(&info).unwrap();
    let p = info.join(format!("{name}.trashinfo"));
    std::fs::write(&p, body).unwrap();
    p
}

fn info_body(days: i64) -> String {
    format!("[Trash Info]\nPath=/somewhere/x\nDeletionDate={}\n", deletion_date(days))
}

#[test]
fn downloads_old_files_only() {
    let tmp = tempfile::tempdir().unwrap();
    let home = tmp.path();
    let dl = home.join("Downloads");
    mkfile(&dl.join("old.zip"), 1000);
    mkfile(&dl.join("new.pdf"), 1000);
    set_age(&dl.join("old.zip"), 100);
    set_age(&dl.join("new.pdf"), 10);
    let out = scan(&UserRules, home, &Config::default());
    assert_eq!(out.entries.len(), 1, "{:?}", out.entries);
    let e = &out.entries[0];
    assert_eq!(e.category, "user_rules");
    assert!(e.selected);
    assert_eq!(e.reason, "1 files older than 90 days");
    assert_eq!(rule_matches(e, home), vec![dl.join("old.zip")]);
}

#[test]
fn user_rule_handles_odd_names() {
    let tmp = tempfile::tempdir().unwrap();
    let home = tmp.path();
    let dl = home.join("Downloads");
    let spaced = dl.join("file (1).zip");
    let bad = dl.join(OsStr::from_bytes(b"bad\xffname.zip"));
    for p in [&spaced, &bad] {
        mkfile(p, 1000);
        set_age(p, 100);
    }
    let config = Config::default();
    let out = scan(&UserRules, home, &config);
    assert_eq!(out.entries.len(), 1, "{:?}", out.entries);
    let mut want = vec![spaced.clone(), bad.clone()];
    want.sort();
    assert_eq!(rule_matches(&out.entries[0], home), want);

    let res = with_ctx(home, &config, |ctx| UserRules.apply(&out.entries[0], ctx));
    assert!(matches!(&res, Outcome::Deleted { failures, .. } if failures.is_empty()), "{res:?}");
    assert!(!spaced.exists());
    assert!(bad.symlink_metadata().is_err());
}

#[test]
fn user_rule_glob_and_min_bytes() {
    let tmp = tempfile::tempdir().unwrap();
    let home = tmp.path();
    let dl = home.join("Downloads");
    // Sizes are on-disk (st_blocks * 512), so a tiny file still occupies a
    // whole block; keep the threshold well above one block.
    for (name, bytes) in [("big.zip", 200_000), ("small.zip", 10), ("big.pdf", 200_000)] {
        mkfile(&dl.join(name), bytes);
        set_age(&dl.join(name), 100);
    }
    let config = Config {
        user_rules: vec![StoreCfg {
            path: "~/Downloads".into(),
            max_age_days: 90,
            unit: "files".into(),
            glob: Some("*.zip".into()),
            min_bytes: Some(100_000),
        }],
        ..Config::default()
    };
    let out = scan(&UserRules, home, &config);
    assert_eq!(out.entries.len(), 1, "{:?}", out.entries);
    assert_eq!(rule_matches(&out.entries[0], home), vec![dl.join("big.zip")]);
}

#[test]
fn trash_expired_item_both_targets() {
    let tmp = tempfile::tempdir().unwrap();
    let home = tmp.path();
    let (files, _) = trash_dirs(home);
    let payload = files.join("report (1).pdf");
    mkfile(&payload.join("inner"), 3000); // a trashed directory
    let info = write_info(home, "report (1).pdf", &info_body(40));
    let config = Config::default();
    let out = scan(&Trash, home, &config);
    assert!(out.skipped.is_empty(), "{:?}", out.skipped);
    assert_eq!(out.entries.len(), 1, "{:?}", out.entries);
    let e = &out.entries[0];
    assert_eq!(e.category, "trash");
    assert!(e.selected);
    assert!(e.bytes > 0);
    assert!(e.reason.starts_with("in Trash since "), "{}", e.reason);
    assert_eq!(targets(e), vec![payload.clone(), info.clone()]);

    let res = with_ctx(home, &config, |ctx| Trash.apply(e, ctx));
    assert!(matches!(&res, Outcome::Deleted { failures, .. } if failures.is_empty()), "{res:?}");
    assert!(!payload.exists());
    assert!(!info.exists());
}

#[test]
fn trash_recent_kept() {
    let tmp = tempfile::tempdir().unwrap();
    let home = tmp.path();
    let (files, _) = trash_dirs(home);
    mkfile(&files.join("fresh.txt"), 100);
    write_info(home, "fresh.txt", &info_body(5));
    let out = scan(&Trash, home, &Config::default());
    assert!(out.entries.is_empty(), "{:?}", out.entries);
    assert!(out.skipped.is_empty(), "{:?}", out.skipped);
}

#[test]
fn trash_orphan_record() {
    let tmp = tempfile::tempdir().unwrap();
    let home = tmp.path();
    std::fs::create_dir_all(trash_dirs(home).0).unwrap();
    let info = write_info(home, "gone.txt", &info_body(40));
    let out = scan(&Trash, home, &Config::default());
    assert_eq!(out.entries.len(), 1, "{:?}", out.entries);
    let e = &out.entries[0];
    assert_eq!(e.reason, "orphaned Trash record");
    assert_eq!(targets(e), vec![info]);
}

#[test]
fn trash_malformed_skipped() {
    let tmp = tempfile::tempdir().unwrap();
    let home = tmp.path();
    let (files, _) = trash_dirs(home);
    mkfile(&files.join("odd.txt"), 100);
    write_info(home, "odd.txt", "[Trash Info]\nPath=/somewhere/odd.txt\n");
    let out = scan(&Trash, home, &Config::default());
    assert!(out.entries.is_empty(), "{:?}", out.entries);
    assert_eq!(out.skipped.len(), 1, "{:?}", out.skipped);
    assert_eq!(out.skipped[0].category, "trash");
    assert_eq!(out.skipped[0].reason, "unreadable trashinfo");
}

#[test]
fn trash_dot_named_record_not_whole_files_dir() {
    let tmp = tempfile::tempdir().unwrap();
    let home = tmp.path();
    let (files, _) = trash_dirs(home);
    let recent = files.join("recent.txt");
    mkfile(&recent, 100);
    write_info(home, "recent.txt", &info_body(1));
    // Records named `..trashinfo`, `.trashinfo` and `...trashinfo` would name
    // the payloads `files/.`, `files/` and `files/..`.
    for name in [".", "", ".."] {
        write_info(home, name, &info_body(4000));
    }
    let config = Config::default();
    let out = scan(&Trash, home, &config);
    let planned: Vec<PathBuf> = out.entries.iter().flat_map(targets).collect();
    assert!(planned.is_empty(), "{planned:?}");
    assert_eq!(out.skipped.len(), 3, "{:?}", out.skipped);
    assert!(out.skipped.iter().all(|s| s.reason == "invalid trashinfo name"), "{:?}", out.skipped);
    assert!(recent.exists());
}

#[test]
fn user_rule_skips_git_tracked() {
    let tmp = tempfile::tempdir().unwrap();
    let home = tmp.path();
    let notes = home.join("notes");
    common::git_init(&notes);
    let tracked = notes.join("old.log");
    mkfile(&tracked, 100);
    common::git_commit_all(&notes);
    let untracked = notes.join("stray.log");
    mkfile(&untracked, 100);
    set_age(&tracked, 100);
    set_age(&untracked, 100);
    let config = Config {
        user_rules: vec![StoreCfg {
            path: "~/notes".into(),
            max_age_days: 90,
            unit: "files".into(),
            glob: Some("*.log".into()),
            min_bytes: None,
        }],
        ..Config::default()
    };
    let out = scan(&UserRules, home, &config);
    assert_eq!(out.entries.len(), 1, "{:?}", out.entries);
    assert_eq!(rule_matches(&out.entries[0], home), vec![untracked.clone()]);

    let res = with_ctx(home, &config, |ctx| UserRules.apply(&out.entries[0], ctx));
    assert!(matches!(&res, Outcome::Deleted { failures, .. } if failures.is_empty()), "{res:?}");
    assert!(tracked.exists(), "tracked file deleted");
    assert!(!untracked.exists());
}
