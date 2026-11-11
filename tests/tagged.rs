mod common;

use cleaner::category::artifacts::{Artifacts, CACHEDIR_SIGNATURE, has_cachedir_tag};
use cleaner::category::{Category, ScanCtx, ScanOutput};
use cleaner::config::Config;
use cleaner::fsutil::measure::InodeSet;
use cleaner::plan::{Entry, EntryKind};
use cleaner::protect::Protector;
use common::{git_commit_all, git_init, mkfile, now, set_age};
use std::os::unix::fs::symlink;
use std::path::{Path, PathBuf};

const TAG_REASON: &str = "CACHEDIR.TAG cache · untouched 40 days";

fn scan_with(home: &Path, cfg: Config) -> ScanOutput {
    let protector = Protector::new(home, &cfg.protect);
    let tools = common::FakeRunner::new();
    let ctx = ScanCtx { home, now: now(), config: &cfg, protector: &protector, tools: &tools, euid: common::my_uid(), usage: None };
    Artifacts.scan(&ctx, &mut InodeSet::default()).unwrap()
}

/// Writes the exact signature plus a comment line, and ages the file.
fn tag(dir: &Path) {
    let p = dir.join("CACHEDIR.TAG");
    std::fs::create_dir_all(dir).unwrap();
    let mut body = CACHEDIR_SIGNATURE.to_vec();
    body.extend_from_slice(b"\n# comment\n");
    std::fs::write(&p, body).unwrap();
    set_age(&p, 40);
}

/// A tagged dir holding one 40-day-old blob.
fn tagged(dir: &Path) {
    mkfile(&dir.join("blob"), 100);
    set_age(&dir.join("blob"), 40);
    tag(dir);
}

fn tagged_entries(out: &ScanOutput) -> Vec<&Entry> {
    out.entries.iter().filter(|e| e.reason.starts_with("CACHEDIR.TAG")).collect()
}

fn titles(out: &ScanOutput) -> Vec<&str> {
    tagged_entries(out).iter().map(|e| e.title.as_str()).collect()
}

fn targets(e: &Entry) -> Vec<PathBuf> {
    match &e.kind {
        EntryKind::Item { targets } => targets.iter().map(|t| t.path.clone()).collect(),
        other => panic!("expected Item, got {other:?}"),
    }
}

#[test]
fn tagged_dir_listed_unticked() {
    let tmp = tempfile::tempdir().unwrap();
    let h = tmp.path();
    tagged(&h.join("proj/out"));
    let out = scan_with(h, Config::default());
    let e = tagged_entries(&out);
    assert_eq!(e.len(), 1, "{:?}", out.entries);
    assert_eq!(e[0].title, "~/proj/out");
    assert_eq!(e[0].category, "artifacts");
    assert!(!e[0].selected);
    assert_eq!(e[0].reason, TAG_REASON);
    assert_eq!(targets(e[0]), vec![h.join("proj/out")]);
    assert!(e[0].bytes > 0);
}

#[test]
fn bad_signature_ignored() {
    let tmp = tempfile::tempdir().unwrap();
    let h = tmp.path();
    tagged(&h.join("proj/good"));
    mkfile(&h.join("proj/bad/blob"), 100);
    std::fs::write(h.join("proj/bad/CACHEDIR.TAG"), b"Signature: 00000000000000000000000000000000\n").unwrap();
    mkfile(&h.join("proj/short/blob"), 100);
    std::fs::write(h.join("proj/short/CACHEDIR.TAG"), b"Signature: 8a47").unwrap();
    assert!(!has_cachedir_tag(&h.join("proj/bad")));
    assert!(!has_cachedir_tag(&h.join("proj/short")));
    let out = scan_with(h, Config::default());
    assert_eq!(titles(&out), vec!["~/proj/good"], "{:?}", out.entries);
}

#[test]
fn tag_symlink_ignored() {
    let tmp = tempfile::tempdir().unwrap();
    let h = tmp.path();
    tagged(&h.join("proj/good"));
    let real = h.join("elsewhere/good.tag");
    mkfile(&real, 0);
    let mut body = CACHEDIR_SIGNATURE.to_vec();
    body.push(b'\n');
    std::fs::write(&real, body).unwrap();
    mkfile(&h.join("proj/out/blob"), 100);
    symlink(&real, h.join("proj/out/CACHEDIR.TAG")).unwrap();
    assert!(!has_cachedir_tag(&h.join("proj/out")));
    let out = scan_with(h, Config::default());
    assert_eq!(titles(&out), vec!["~/proj/good"], "{:?}", out.entries);
}

#[test]
fn venv_tag_denied() {
    let tmp = tempfile::tempdir().unwrap();
    let h = tmp.path();
    tagged(&h.join("proj/ok"));
    mkfile(&h.join("proj/env/pyvenv.cfg"), 20);
    tagged(&h.join("proj/env"));
    let out = scan_with(h, Config::default());
    assert_eq!(titles(&out), vec!["~/proj/ok"], "{:?}", out.entries);
}

#[test]
fn uv_tool_venv_never_listed() {
    let tmp = tempfile::tempdir().unwrap();
    let extra = tempfile::tempdir().unwrap();
    let h = tmp.path();
    let venv = extra.path().join("share/uv/tools/graphifyy");
    mkfile(&venv.join("bin/x"), 100);
    mkfile(&venv.join("pyvenv.cfg"), 20);
    tag(&venv);
    tagged(&extra.path().join("share/other"));
    let cfg = Config { scan_roots: vec!["~".into(), extra.path().display().to_string()], ..Config::default() };
    let out = scan_with(h, cfg);
    let t = titles(&out);
    assert_eq!(t.len(), 1, "{:?}", out.entries);
    assert!(t[0].ends_with("share/other"), "{t:?}");
}

#[test]
fn uv_cache_denied() {
    let tmp = tempfile::tempdir().unwrap();
    let h = tmp.path();
    tagged(&h.join(".cache/uv"));
    mkfile(&h.join(".cache/uv/archive-v0/x"), 100);
    tagged(&h.join(".cache/fontconfig"));
    // A tagged dir below the uv cache is just as dead to us.
    tagged(&h.join(".cache/uv/wheels-v5"));
    let out = scan_with(h, Config::default());
    assert_eq!(titles(&out), vec!["~/.cache/fontconfig"], "{:?}", out.entries);
}

#[test]
fn store_root_denied() {
    let tmp = tempfile::tempdir().unwrap();
    let h = tmp.path();
    tagged(&h.join(".cache/pip"));
    tagged(&h.join(".cache/fontconfig"));
    let out = scan_with(h, Config::default());
    assert_eq!(titles(&out), vec!["~/.cache/fontconfig"], "{:?}", out.entries);
}

#[test]
fn artifact_tag_not_double_listed() {
    let tmp = tempfile::tempdir().unwrap();
    let h = tmp.path();
    mkfile(&h.join("proj/Cargo.toml"), 10);
    mkfile(&h.join("proj/target/x"), 100);
    tag(&h.join("proj/target"));
    let out = scan_with(h, Config::default());
    assert_eq!(out.entries.len(), 1, "{:?}", out.entries);
    assert!(tagged_entries(&out).is_empty());
    assert_eq!(targets(&out.entries[0]), vec![h.join("proj/target")]);
}

#[test]
fn tracked_tag_dir_skipped() {
    let tmp = tempfile::tempdir().unwrap();
    let h = tmp.path();
    git_init(&h.join("proj"));
    tagged(&h.join("proj/out"));
    git_commit_all(&h.join("proj"));
    let out = scan_with(h, Config::default());
    assert!(tagged_entries(&out).is_empty(), "{:?}", out.entries);
    assert!(
        out.skipped.iter().any(|s| s.title == h.join("proj/out").display().to_string() && s.reason == "tracked in git"),
        "{:?}",
        out.skipped
    );
}

#[test]
fn tagged_dir_not_descended() {
    let tmp = tempfile::tempdir().unwrap();
    let h = tmp.path();
    tagged(&h.join("t"));
    mkfile(&h.join("t/inner/Cargo.toml"), 10);
    mkfile(&h.join("t/inner/target/x"), 100);
    let out = scan_with(h, Config::default());
    assert_eq!(out.entries.len(), 1, "{:?}", out.entries);
    assert_eq!(out.entries[0].title, "~/t");
}

#[test]
fn protected_tag_never_listed() {
    let tmp = tempfile::tempdir().unwrap();
    let h = tmp.path();
    tagged(&h.join("keep/c"));
    tagged(&h.join("proj/ok"));
    let cfg = Config { protect: vec!["~/keep".into()], ..Config::default() };
    let out = scan_with(h, cfg);
    assert_eq!(titles(&out), vec!["~/proj/ok"], "{:?}", out.entries);
}

#[test]
fn tagged_entries_sorted_by_path() {
    let tmp = tempfile::tempdir().unwrap();
    let h = tmp.path();
    tagged(&h.join("b/out"));
    tagged(&h.join("a/out"));
    tagged(&h.join(".cache/zz"));
    let out = scan_with(h, Config::default());
    assert_eq!(titles(&out), vec!["~/.cache/zz", "~/a/out", "~/b/out"], "{:?}", out.entries);
}

#[test]
fn cache_child_containing_protected_not_listed() {
    let tmp = tempfile::tempdir().unwrap();
    let h = tmp.path();
    tagged(&h.join(".cache/fontconfig"));
    mkfile(&h.join(".cache/fontconfig/keep/x"), 10);
    tagged(&h.join(".cache/other"));
    let cfg = Config { protect: vec!["~/.cache/fontconfig/keep".into()], ..Config::default() };
    let out = scan_with(h, cfg);
    assert_eq!(titles(&out), vec!["~/.cache/other"], "{:?}", out.entries);
}

#[test]
fn tagged_dir_containing_store_root_denied() {
    let tmp = tempfile::tempdir().unwrap();
    let h = tmp.path();
    mkfile(&h.join(".cache/verify-tool/runs/a/tg-cache/x"), 10);
    tagged(&h.join(".cache/verify-tool"));
    tagged(&h.join(".cache/other"));
    let mut cfg = Config::default();
    cfg.caches.stores.push(cleaner::config::StoreCfg {
        path: "~/.cache/verify-tool/runs/*/tg-cache".into(),
        max_age_days: 3,
        unit: "depth:1".into(),
        glob: None,
        min_bytes: None,
    });
    let out = scan_with(h, cfg);
    assert_eq!(titles(&out), vec!["~/.cache/other"], "{:?}", out.entries);
}
