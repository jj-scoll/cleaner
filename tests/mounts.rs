//! Mount-point candidates (spec §6.2). Each test re-runs itself inside a
//! user + mount namespace (`unshare -rm`) so it can mount a tmpfs without
//! root; where user namespaces are unavailable the test is skipped.

mod common;

use cleaner::category::artifacts::{builtin_ecosystems, discover};
use cleaner::category::caches::Caches;
use cleaner::category::store::{StoreRule, Unit, apply_rule, evaluate};
use cleaner::category::{Category, ScanCtx};
use cleaner::config::{Config, KeepNewestCfg};
use cleaner::fsutil::delete::{DeleteError, delete_tree};
use cleaner::fsutil::measure::InodeSet;
use cleaner::plan::EntryKind;
use cleaner::protect::Protector;
use common::{mkfile, now, set_age};
use std::ffi::CString;
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};
use std::process::Command;

const DIR_ENV: &str = "CLEANER_TEST_NS_DIR";

/// Runs `body(dir)` inside a fresh user + mount namespace, where `dir` is an
/// empty temp dir. The temp dir is made and removed outside the namespace, so
/// the mounts are gone by the time it is cleaned up.
fn in_mount_ns(test: &str, body: impl FnOnce(&Path)) {
    if let Some(dir) = std::env::var_os(DIR_ENV) {
        body(Path::new(&dir));
        return;
    }
    let probe = Command::new("unshare").args(["--user", "--map-root-user", "--mount", "true"]).status();
    if !probe.is_ok_and(|s| s.success()) {
        eprintln!("skipping {test}: user+mount namespaces unavailable");
        return;
    }
    let tmp = tempfile::tempdir().unwrap();
    let out = Command::new("unshare")
        .args(["--user", "--map-root-user", "--mount"])
        .arg(std::env::current_exe().unwrap())
        .args([test, "--exact", "--nocapture", "--test-threads=1"])
        .env(DIR_ENV, tmp.path())
        .output()
        .unwrap();
    let stdout = String::from_utf8_lossy(&out.stdout);
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(out.status.success(), "{test} failed in namespace:\n{stdout}\n{stderr}");
    assert!(stdout.contains("1 passed"), "{test} did not run in namespace:\n{stdout}\n{stderr}");
}

/// Mounts a fresh tmpfs at `dir` (created if missing).
fn mount_tmpfs(dir: &Path) {
    std::fs::create_dir_all(dir).unwrap();
    let c = |s: &[u8]| CString::new(s).unwrap();
    let (src, target, fstype) = (c(b"tmpfs"), c(dir.as_os_str().as_bytes()), c(b"tmpfs"));
    let rc = unsafe {
        libc::mount(src.as_ptr(), target.as_ptr(), fstype.as_ptr(), 0, std::ptr::null())
    };
    assert_eq!(rc, 0, "mount tmpfs at {}: {}", dir.display(), std::io::Error::last_os_error());
}

/// Mounts a tmpfs at `dir` holding one file `precious`, all `days` old.
fn old_tmpfs(dir: &Path, days: u64) -> PathBuf {
    mount_tmpfs(dir);
    let precious = dir.join("precious");
    mkfile(&precious, 100);
    set_age(&precious, days);
    set_age(dir, days);
    precious
}

fn with_ctx<R>(home: &Path, config: &Config, f: impl FnOnce(&ScanCtx) -> R) -> R {
    let protector = Protector::new(home, &config.protect);
    let tools = common::FakeRunner::new();
    let ctx = ScanCtx { home, now: now(), config, protector: &protector, tools: &tools, euid: common::my_uid(), usage: None };
    f(&ctx)
}

fn rule(root: &Path, unit: Unit) -> StoreRule {
    StoreRule { root: root.to_path_buf(), max_age_days: 30, unit, glob: None, min_bytes: None }
}

#[test]
fn delete_tree_refuses_mount_point() {
    in_mount_ns("delete_tree_refuses_mount_point", |home| {
        let mnt = home.join("mnt");
        let precious = old_tmpfs(&mnt, 0);
        let res = delete_tree(&mnt);
        assert!(matches!(res, Err(DeleteError::CrossesMount(_))), "{res:?}");
        assert!(precious.exists());
    });
}

#[test]
fn store_units_skip_mount_points() {
    in_mount_ns("store_units_skip_mount_points", |home| {
        let root = home.join(".cache/terragrunt");
        let precious = old_tmpfs(&root.join("modA"), 60);
        let module = root.join("x@v1");
        let precious2 = old_tmpfs(&module, 60);
        let same_fs = root.join("modB/f");
        mkfile(&same_fs, 100);
        set_age(&same_fs, 60);
        set_age(&root.join("modB"), 60);
        let prot = Protector::new(home, &[]);

        let depth = rule(&root, Unit::Depth(1));
        let got: Vec<PathBuf> = evaluate(&depth, now(), &prot, &mut InodeSet::default())
            .into_iter()
            .map(|m| m.path)
            .collect();
        assert_eq!(got, vec![root.join("modB")]);

        let named = rule(&root, Unit::NameContains("@".into()));
        assert!(evaluate(&named, now(), &prot, &mut InodeSet::default()).is_empty());

        let config = Config::default();
        with_ctx(home, &config, |ctx| {
            apply_rule(&depth, ctx);
            apply_rule(&named, ctx);
        });
        assert!(precious.exists());
        assert!(precious2.exists());
        assert!(!root.join("modB").exists());
    });
}

#[test]
fn artifact_mount_point_not_claimed() {
    in_mount_ns("artifact_mount_point_not_claimed", |home| {
        let p = home.join("proj");
        mkfile(&p.join("package.json"), 10);
        let precious = old_tmpfs(&p.join("node_modules"), 60);
        let prot = Protector::new(home, &[]);
        let (projects, skips) = discover(&[home.to_path_buf()], 6, &builtin_ecosystems(), &prot);
        let found: Vec<&PathBuf> = projects.iter().flat_map(|p| &p.artifacts).collect();
        assert!(found.is_empty(), "{found:?}");
        assert!(skips.iter().any(|s| s.title == p.join("node_modules").display().to_string()), "{skips:?}");
        assert!(precious.exists());
    });
}

#[test]
fn keep_newest_skips_mount_points() {
    in_mount_ns("keep_newest_skips_mount_points", |home| {
        let runs = home.join("runs");
        let precious = old_tmpfs(&runs.join("a"), 5);
        mkfile(&runs.join("b/out.log"), 100);
        let mut config = Config::default();
        config.caches.stores.clear();
        config.caches.keep_newest =
            vec![KeepNewestCfg { path: runs.to_str().unwrap().into(), keep: 1 }];
        let out = with_ctx(home, &config, |ctx| Caches.scan(ctx, &mut InodeSet::default())).unwrap();
        let planned: Vec<PathBuf> = out
            .entries
            .iter()
            .flat_map(|e| match &e.kind {
                EntryKind::Item { targets } => targets.iter().map(|t| t.path.clone()).collect(),
                _ => Vec::new(),
            })
            .collect();
        assert!(planned.is_empty(), "{planned:?}");
        assert!(precious.exists());
    });
}

#[test]
fn index_stays_on_device() {
    in_mount_ns("index_stays_on_device", |home| {
        let r = home.join("r");
        mkfile(&r.join("here/f"), 32768);
        let precious = old_tmpfs(&r.join("mnt"), 1);
        mkfile(&precious.with_file_name("more"), 1 << 20);
        let idx = cleaner::fsutil::usage::build(std::slice::from_ref(&r), &[], 16384, 0);
        let root = &idx.trees[0].node;
        assert!(root.children.iter().all(|c| c.name != Path::new("mnt")), "{root:?}");
        assert_eq!(root.other_count, 0);
        let want = cleaner::fsutil::measure::measure(&r.join("here"), &mut InodeSet::default()).unwrap().bytes;
        assert_eq!(root.bytes, want);
    });
}

#[test]
fn mount_point_is_same_dev_ancestor() {
    use std::os::unix::fs::MetadataExt;
    let tmp = tempfile::TempDir::new().unwrap();
    let m = cleaner::disk::mount_point(tmp.path()).unwrap();
    assert!(tmp.path().starts_with(&m), "{m:?}");
    let dev = |p: &Path| std::fs::metadata(p).unwrap().dev();
    assert_eq!(dev(&m), dev(tmp.path()));
    assert!(m == Path::new("/") || dev(m.parent().unwrap()) != dev(&m), "{m:?}");
    assert_eq!(cleaner::disk::mount_point(&tmp.path().join("missing")), None);
}
