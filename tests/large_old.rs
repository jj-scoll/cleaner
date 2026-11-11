mod common;

use cleaner::category::large_old::LargeOld;
use cleaner::category::{Category, ScanCtx, ScanOutput};
use cleaner::config::Config;
use cleaner::fmt::local_time;
use cleaner::fsutil::measure::InodeSet;
use cleaner::fsutil::usage::{UsageIndex, build};
use cleaner::plan::EntryKind;
use cleaner::protect::Protector;
use common::{git, git_commit_all, git_init, mkfile, now, set_age};
use std::path::{Path, PathBuf};
use tempfile::TempDir;

const KIB128: usize = 128 * 1024;

/// Roots indexed with a small `min_node` so fixtures stay tiny.
fn index(roots: &[&Path], created: i64) -> UsageIndex {
    let roots: Vec<PathBuf> = roots.iter().map(|p| p.to_path_buf()).collect();
    build(&roots, &[], 16384, created)
}

fn scan_with(home: &Path, cfg: &Config, protect: &[String], idx: Option<&UsageIndex>) -> ScanOutput {
    let protector = Protector::new(home, protect);
    let tools = common::FakeRunner::new();
    let ctx = ScanCtx {
        home,
        now: now(),
        config: cfg,
        protector: &protector,
        tools: &tools,
        euid: common::my_uid(),
        usage: idx,
    };
    LargeOld.scan(&ctx, &mut InodeSet::default()).unwrap()
}

fn small_cfg() -> Config {
    let mut cfg = Config::default();
    cfg.idle_folders.min_bytes = 65536;
    cfg
}

/// Indexes `home`, scans, and checks that every row is report-only.
fn scan(home: &Path) -> ScanOutput {
    let idx = index(&[home], now());
    let out = scan_with(home, &small_cfg(), &[], Some(&idx));
    never_selected(&out);
    out
}

/// §6: large+old rows are never entries and never selected.
fn never_selected(out: &ScanOutput) {
    assert!(out.entries.is_empty(), "{:?}", out.entries);
    for e in &out.report_only {
        assert!(!e.selected, "{e:?}");
        assert_eq!(e.category, "idle_folders");
        assert!(matches!(e.kind, EntryKind::Item { ref targets } if targets.len() == 1), "{e:?}");
    }
}

fn titles(out: &ScanOutput) -> Vec<&str> {
    out.report_only.iter().map(|e| e.title.as_str()).collect()
}

/// Ages `path` and everything under it (dirs included, `.git` included).
fn age_all(path: &Path, days: u64) {
    if path.symlink_metadata().unwrap().is_dir() {
        for e in std::fs::read_dir(path).unwrap().flatten() {
            age_all(&e.path(), days);
        }
    }
    set_age(path, days);
}

fn file(home: &Path, rel: &str, bytes: usize, days: u64) {
    let f = home.join(rel);
    mkfile(&f, bytes);
    set_age(&f, days);
}

#[test]
fn topmost_old_dir_reported_once() {
    let tmp = TempDir::new().unwrap();
    let h = tmp.path();
    file(h, "old/a/blob", KIB128 / 2, 100);
    file(h, "old/b/blob", KIB128 / 2, 100);
    for d in ["old/a", "old/b", "old"] {
        set_age(&h.join(d), 100);
    }
    let out = scan(h);
    assert_eq!(titles(&out), ["~/old"]);
    assert_eq!(out.report_only[0].reason, "untouched for 100 days");
    match &out.report_only[0].kind {
        EntryKind::Item { targets } => assert_eq!(targets[0].path, h.join("old")),
        other => panic!("{other:?}"),
    }
}

#[test]
fn fresh_file_disqualifies_parent() {
    let tmp = TempDir::new().unwrap();
    let h = tmp.path();
    file(h, "mixed/old/blob", KIB128, 100);
    set_age(&h.join("mixed/old"), 100);
    file(h, "mixed/new.txt", 20000, 1);
    let out = scan(h);
    assert_eq!(titles(&out), ["~/mixed/old"]);
}

#[test]
fn large_old_file_row() {
    let tmp = TempDir::new().unwrap();
    let h = tmp.path();
    file(h, "dump.bin", KIB128, 100);
    let out = scan(h);
    assert_eq!(titles(&out), ["~/dump.bin"]);
    assert_eq!(out.report_only[0].reason, "file untouched for 100 days");
}

#[test]
fn dotdirs_included() {
    let tmp = TempDir::new().unwrap();
    let h = tmp.path();
    file(h, ".steampipe/blob", KIB128, 100);
    set_age(&h.join(".steampipe"), 100);
    let out = scan(h);
    assert_eq!(titles(&out), ["~/.steampipe"]);
}

#[test]
fn protected_never_listed() {
    let tmp = TempDir::new().unwrap();
    let h = tmp.path();
    file(h, ".claude-memd/bench/blob", KIB128, 100);
    for d in [".claude-memd/bench", ".claude-memd"] {
        set_age(&h.join(d), 100);
    }
    let out = scan(h);
    assert!(out.report_only.is_empty(), "{:?}", out.report_only);

    let tmp = TempDir::new().unwrap();
    let h = tmp.path();
    file(h, "keep/inner/blob", KIB128, 100);
    file(h, "keep/other/blob", KIB128, 100);
    for d in ["keep/inner", "keep/other", "keep"] {
        set_age(&h.join(d), 100);
    }
    let idx = index(&[h], now());
    let out = scan_with(h, &small_cfg(), &["~/keep/inner".into()], Some(&idx));
    never_selected(&out);
    assert_eq!(titles(&out), ["~/keep/other"]);
}

#[test]
fn small_or_recent_not_listed() {
    let tmp = TempDir::new().unwrap();
    let h = tmp.path();
    file(h, "small/blob", 32 * 1024, 100);
    set_age(&h.join("small"), 100);
    file(h, "recent/blob", KIB128, 10);
    let out = scan(h);
    assert!(out.report_only.is_empty(), "{:?}", out.report_only);
}

/// A repo `proj/` with a 128 KiB committed blob and a bare remote, aged 100 days.
fn repo(h: &Path, remote: Option<&Path>) -> PathBuf {
    let p = h.join("proj");
    git_init(&p);
    mkfile(&p.join("blob"), KIB128);
    git_commit_all(&p);
    if let Some(r) = remote {
        std::fs::create_dir_all(r).unwrap();
        git(r, &["init", "-q", "--bare"]);
        git(&p, &["remote", "add", "origin", r.to_str().unwrap()]);
        git(&p, &["push", "-q", "-u", "origin", "main"]);
    }
    p
}

fn badge_note(out: &ScanOutput) -> String {
    assert_eq!(titles(out), ["~/proj"], "{:?}", out.report_only);
    out.report_only[0].note.clone().expect("a note")
}

#[test]
fn git_badge_clean_pushed() {
    let tmp = TempDir::new().unwrap();
    let rem = TempDir::new().unwrap();
    let p = repo(tmp.path(), Some(&rem.path().join("r.git")));
    age_all(&p, 100);
    assert_eq!(badge_note(&scan(tmp.path())), "git: clean, all branches pushed");
}

#[test]
fn git_badge_unpushed() {
    let tmp = TempDir::new().unwrap();
    let rem = TempDir::new().unwrap();
    let p = repo(tmp.path(), Some(&rem.path().join("r.git")));
    std::fs::write(p.join("more"), "m").unwrap();
    git_commit_all(&p);
    age_all(&p, 100);
    assert_eq!(badge_note(&scan(tmp.path())), "git: 1 unpushed commits");
}

#[test]
fn git_badge_dirty() {
    let tmp = TempDir::new().unwrap();
    let rem = TempDir::new().unwrap();
    let p = repo(tmp.path(), Some(&rem.path().join("r.git")));
    std::fs::write(p.join("untracked"), "u").unwrap();
    age_all(&p, 100);
    assert_eq!(badge_note(&scan(tmp.path())), "git: uncommitted changes");
}

#[test]
fn git_badge_no_remote() {
    let tmp = TempDir::new().unwrap();
    let p = repo(tmp.path(), None);
    age_all(&p, 100);
    assert_eq!(badge_note(&scan(tmp.path())), "git: no remote");
}

#[test]
fn extra_scan_root_searched() {
    let home = TempDir::new().unwrap();
    let extra = TempDir::new().unwrap();
    let e = extra.path();
    file(e, "repos/old/blob", KIB128, 100);
    set_age(&e.join("repos/old"), 100); // repos/ itself stays fresh
    let mut cfg = small_cfg();
    cfg.scan_roots = vec!["~".into(), e.to_str().unwrap().into()];
    let idx = index(&[home.path(), e], now());
    let out = scan_with(home.path(), &cfg, &[], Some(&idx));
    never_selected(&out);
    assert_eq!(titles(&out), [format!("{}/repos/old", e.display())]);
}

#[test]
fn scan_root_itself_never_a_row() {
    let tmp = TempDir::new().unwrap();
    let h = tmp.path();
    file(h, "a/blob", KIB128, 100);
    set_age(&h.join("a"), 100);
    set_age(h, 100);
    let out = scan(h);
    for e in &out.report_only {
        match &e.kind {
            EntryKind::Item { targets } => assert_ne!(targets[0].path, h),
            other => panic!("{other:?}"),
        }
    }
    assert_eq!(titles(&out), ["~/a"]);
}

#[test]
fn rows_sorted_by_bytes_descending() {
    let tmp = TempDir::new().unwrap();
    let h = tmp.path();
    file(h, "small.bin", KIB128, 100);
    file(h, "big.bin", KIB128 * 3, 100);
    let out = scan(h);
    assert_eq!(titles(&out), ["~/big.bin", "~/small.bin"]);
}

#[test]
fn no_index_is_skip() {
    let tmp = TempDir::new().unwrap();
    let out = scan_with(tmp.path(), &small_cfg(), &[], None);
    never_selected(&out);
    assert!(out.report_only.is_empty());
    assert_eq!(out.skipped.len(), 1);
    assert_eq!(out.skipped[0].title, "Large and old");
    assert_eq!(out.skipped[0].reason, "no disk index yet; run cleaner plan");
}

#[test]
fn stale_index_noted() {
    let tmp = TempDir::new().unwrap();
    let h = tmp.path();
    file(h, "dump.bin", KIB128, 100);
    let created = now() - 40 * 3600;
    let idx = index(&[h], created);
    let out = scan_with(h, &small_cfg(), &[], Some(&idx));
    never_selected(&out);
    let want = format!("disk index from {}", local_time(created, &jiff::tz::TimeZone::system()));
    assert_eq!(out.report_only[0].note.as_deref(), Some(want.as_str()));

    // A fresh index adds no note.
    let out = scan(h);
    assert_eq!(out.report_only[0].note, None);
}

#[test]
fn stale_note_joins_git_badge() {
    let tmp = TempDir::new().unwrap();
    let p = repo(tmp.path(), None);
    age_all(&p, 100);
    let created = now() - 40 * 3600;
    let idx = index(&[tmp.path()], created);
    let out = scan_with(tmp.path(), &small_cfg(), &[], Some(&idx));
    never_selected(&out);
    let want = format!(
        "git: no remote · disk index from {}",
        local_time(created, &jiff::tz::TimeZone::system())
    );
    assert_eq!(out.report_only[0].note.as_deref(), Some(want.as_str()));
}

#[test]
fn git_badge_does_not_write_the_repo() {
    use std::os::unix::fs::MetadataExt;
    let tmp = TempDir::new().unwrap();
    let p = repo(tmp.path(), None);
    // Aging the worktree makes the index's cached stat data stale, so a plain
    // `git status` would refresh and rewrite .git/index.
    age_all(&p, 100);
    let index = p.join(".git/index");
    let before = index.metadata().unwrap().mtime_nsec();
    let before_s = index.metadata().unwrap().mtime();
    let out = scan(tmp.path());
    assert_eq!(titles(&out), ["~/proj"]);
    let md = index.metadata().unwrap();
    assert_eq!((md.mtime(), md.mtime_nsec()), (before_s, before), "git status rewrote .git/index");
}
