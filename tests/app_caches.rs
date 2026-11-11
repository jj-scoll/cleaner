mod common;

use cleaner::category::app_caches::AppCaches;
use cleaner::category::store::{StoreRule, Unit, evaluate};
use cleaner::category::{Category, Outcome, ScanCtx, ScanOutput};
use cleaner::config::{AppCfg, Config, default_apps};
use cleaner::fsutil::measure::InodeSet;
use cleaner::plan::{Entry, EntryKind};
use cleaner::procs::{app_running, exe_running, lock_owner_alive};
use cleaner::protect::Protector;
use common::{mkfile, now, set_age};
use std::os::unix::fs::symlink;
use std::path::{Path, PathBuf};

/// Ages `root` and everything under it by a day, children first, so a
/// `max_age_days: 0` rule sees it as not being written right now.
fn age_tree(root: &Path) {
    if root.is_dir() && !root.is_symlink() {
        for e in std::fs::read_dir(root).unwrap() {
            age_tree(&e.unwrap().path());
        }
    }
    if !root.is_symlink() {
        set_age(root, 1);
    }
}

/// A fake `/proc/<pid>` with an `exe` link and a NUL-separated `cmdline`.
fn fake_pid(proc_root: &Path, pid: u32, exe: &str, argv: &[&str]) {
    let dir = proc_root.join(pid.to_string());
    std::fs::create_dir_all(&dir).unwrap();
    symlink(exe, dir.join("exe")).unwrap();
    let mut cmdline = Vec::new();
    for a in argv {
        cmdline.extend_from_slice(a.as_bytes());
        cmdline.push(0);
    }
    std::fs::write(dir.join("cmdline"), cmdline).unwrap();
}

fn app(name: &str) -> AppCfg {
    default_apps().into_iter().find(|a| a.name == name).unwrap()
}

struct Fixture {
    _tmp: tempfile::TempDir,
    home: PathBuf,
    procs: PathBuf,
}

impl Fixture {
    fn new() -> Fixture {
        let tmp = tempfile::tempdir().unwrap();
        let home = tmp.path().join("home");
        let procs = tmp.path().join("proc");
        std::fs::create_dir_all(&home).unwrap();
        std::fs::create_dir_all(&procs).unwrap();
        Fixture { _tmp: tmp, home, procs }
    }

    fn chrome_profile(&self) -> PathBuf {
        let d = self.home.join(".config/google-chrome/Default");
        mkfile(&d.join("Cache/Cache_Data/a"), 1000);
        mkfile(&d.join("Code Cache/js/b"), 1000);
        mkfile(&d.join("GPUCache/c"), 1000);
        mkfile(&d.join("Service Worker/CacheStorage/x/d"), 1000);
        mkfile(&d.join("History"), 1000);
        age_tree(&self.home.join(".config"));
        d
    }

    fn with_ctx<R>(&self, config: &Config, f: impl FnOnce(&ScanCtx, &AppCaches) -> R) -> R {
        let protector = Protector::new(&self.home, &config.protect);
        let tools = common::FakeRunner::new();
        let ctx =
            ScanCtx { home: &self.home, now: now(), config, protector: &protector, tools: &tools, euid: common::my_uid(), usage: None };
        f(&ctx, &AppCaches { proc_root: self.procs.clone() })
    }

    fn scan(&self) -> ScanOutput {
        self.with_ctx(&Config::default(), |ctx, cat| cat.scan(ctx, &mut InodeSet::default()).unwrap())
    }

    fn apply(&self, entry: &Entry) -> Outcome {
        self.with_ctx(&Config::default(), |ctx, cat| cat.apply(entry, ctx))
    }
}

fn rule_of(e: &Entry) -> &StoreRule {
    match &e.kind {
        EntryKind::StoreRule { rule } => rule,
        k => panic!("expected StoreRule, got {k:?}"),
    }
}

fn match_paths(rule: &StoreRule, home: &Path) -> Vec<PathBuf> {
    let prot = Protector::new(home, &[]);
    let mut v: Vec<_> =
        evaluate(rule, now(), &prot, &mut InodeSet::default()).into_iter().map(|m| m.path).collect();
    v.sort();
    v
}

#[test]
fn finds_named_cache_dirs() {
    let f = Fixture::new();
    let d = f.chrome_profile();
    let out = f.scan();
    assert_eq!(out.entries.len(), 1, "{:?}", out.entries);
    let e = &out.entries[0];
    assert_eq!(e.category, "app_caches");
    assert_eq!(e.title, "Chrome: ~/.config/google-chrome");
    assert_eq!(e.reason, "4 cache dirs");
    assert!(e.selected);
    assert_eq!(e.note, None);
    assert!(e.bytes > 0);
    assert_eq!(
        match_paths(rule_of(e), &f.home),
        vec![
            d.join("Cache"),
            d.join("Code Cache"),
            d.join("GPUCache"),
            d.join("Service Worker/CacheStorage"),
        ]
    );
}

#[test]
fn dirs_named_stops_at_depth_six() {
    let f = Fixture::new();
    let root = f.home.join("app");
    mkfile(&root.join("1/2/3/4/5/Cache/f"), 10); // Cache at depth 6
    mkfile(&root.join("a/b/c/d/e/f/Cache/g"), 10); // Cache at depth 7
    mkfile(&root.join("Cache/Cache/h"), 10); // a match is not descended
    age_tree(&root);
    let rule = StoreRule {
        root: root.clone(),
        max_age_days: 0,
        unit: Unit::DirsNamed(vec!["Cache".into()]),
        glob: None,
        min_bytes: None,
    };
    assert_eq!(match_paths(&rule, &f.home), vec![root.join("1/2/3/4/5/Cache"), root.join("Cache")]);
}

#[test]
fn stale_lock_is_not_running() {
    let f = Fixture::new();
    let cfg = f.home.join(".config/google-chrome");
    std::fs::create_dir_all(&cfg).unwrap();
    let lock = cfg.join("SingletonLock");
    assert_eq!(lock_owner_alive(&lock, &f.procs), None, "missing lock");
    symlink("myhost-999999", &lock).unwrap();
    assert_eq!(lock_owner_alive(&lock, &f.procs), Some(false));
    assert!(!app_running(&app("Chrome"), &f.home, &f.procs));

    let junk = cfg.join("junk");
    symlink("not-a-pid", &junk).unwrap();
    assert_eq!(lock_owner_alive(&junk, &f.procs), None, "unparseable lock");
}

#[test]
fn live_lock_is_running() {
    let f = Fixture::new();
    let cfg = f.home.join(".config/google-chrome");
    std::fs::create_dir_all(&cfg).unwrap();
    symlink("my-host-name-4242", cfg.join("SingletonLock")).unwrap();
    std::fs::create_dir_all(f.procs.join("4242")).unwrap();
    assert_eq!(lock_owner_alive(&cfg.join("SingletonLock"), &f.procs), Some(true));
    assert!(app_running(&app("Chrome"), &f.home, &f.procs));
}

#[test]
fn firefox_wrapper_detected() {
    let f = Fixture::new();
    fake_pid(&f.procs, 77, "/nix/store/x-firefox/lib/firefox/firefox", &["/nix/store/y/.firefox-wrapped"]);
    assert!(exe_running(&["firefox".into()], &f.procs));
    assert!(app_running(&app("Firefox"), &f.home, &f.procs));

    let profile = f.home.join(".mozilla/firefox/abc.default");
    std::fs::create_dir_all(&profile).unwrap();
    symlink("127.0.0.2:+77", profile.join("lock")).unwrap();
    assert_eq!(lock_owner_alive(&profile.join("lock"), &f.procs), Some(true));
}

#[test]
fn firefox_lock_alone_counts() {
    let f = Fixture::new();
    std::fs::create_dir_all(f.procs.join("88")).unwrap(); // no exe/cmdline we can match
    let profile = f.home.join(".mozilla/firefox/abc.default");
    std::fs::create_dir_all(&profile).unwrap();
    symlink("127.0.0.2:+88", profile.join("lock")).unwrap();
    assert!(app_running(&app("Firefox"), &f.home, &f.procs));
}

#[test]
fn argv0_detected() {
    let f = Fixture::new();
    // exe unreadable (another user's process): only cmdline is there.
    let dir = f.procs.join("55");
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("cmdline"), b"/usr/lib/slack/slack\0--startup\0").unwrap();
    assert!(app_running(&app("Slack"), &f.home, &f.procs));
}

#[test]
fn claude_code_is_not_claude_desktop() {
    let f = Fixture::new();
    fake_pid(&f.procs, 99, "/home/u/.local/share/claude/versions/2/claude.exe", &["claude", "--resume"]);
    assert!(!app_running(&app("Claude"), &f.home, &f.procs));
}

#[test]
fn running_note_at_plan() {
    let f = Fixture::new();
    f.chrome_profile();
    fake_pid(&f.procs, 10, "/opt/google/chrome/chrome", &["/opt/google/chrome/chrome"]);
    let out = f.scan();
    assert_eq!(out.entries.len(), 1);
    assert_eq!(
        out.entries[0].note.as_deref(),
        Some("Chrome is running now; will be skipped if still running when you clean")
    );
    assert!(out.entries[0].selected);
}

#[test]
fn apply_skips_when_app_started() {
    let f = Fixture::new();
    let d = f.chrome_profile();
    let out = f.scan();
    assert_eq!(out.entries[0].note, None);
    fake_pid(&f.procs, 10, "/opt/google/chrome/chrome", &["/opt/google/chrome/chrome"]);
    assert_eq!(f.apply(&out.entries[0]), Outcome::Skipped { reason: "Chrome is running".into() });
    assert!(d.join("Cache/Cache_Data/a").exists());
    assert!(d.join("GPUCache/c").exists());
}

#[test]
fn apply_deletes_when_closed() {
    let f = Fixture::new();
    let d = f.chrome_profile();
    fake_pid(&f.procs, 10, "/opt/google/chrome/chrome", &["/opt/google/chrome/chrome"]);
    let out = f.scan();
    std::fs::remove_dir_all(f.procs.join("10")).unwrap();
    match f.apply(&out.entries[0]) {
        Outcome::Deleted { bytes, failures } => {
            assert_eq!(bytes, out.entries[0].bytes);
            assert!(failures.is_empty(), "{failures:?}");
        }
        o => panic!("expected Deleted, got {o:?}"),
    }
    for n in ["Cache", "Code Cache", "GPUCache", "Service Worker/CacheStorage"] {
        assert!(!d.join(n).exists(), "{n} still exists");
    }
    assert!(d.join("History").exists());
    assert!(d.join("Service Worker").exists());
}

#[test]
fn apply_skips_rule_no_app_owns() {
    let f = Fixture::new();
    f.chrome_profile();
    let out = f.scan();
    let mut config = Config::default();
    config.app_caches.apps.retain(|a| a.name != "Chrome");
    let o = f.with_ctx(&config, |ctx, cat| cat.apply(&out.entries[0], ctx));
    assert!(matches!(o, Outcome::Skipped { .. }), "{o:?}");
    assert!(f.home.join(".config/google-chrome/Default/GPUCache").exists());
}

#[test]
fn symlinked_cache_dir_skipped() {
    let f = Fixture::new();
    let elsewhere = f.home.join("elsewhere");
    mkfile(&elsewhere.join("keep"), 1000);
    let d = f.home.join(".config/google-chrome/Default");
    mkfile(&d.join("GPUCache/c"), 1000);
    symlink(&elsewhere, d.join("Cache")).unwrap();
    age_tree(&f.home);
    let out = f.scan();
    assert_eq!(out.entries.len(), 1);
    assert_eq!(match_paths(rule_of(&out.entries[0]), &f.home), vec![d.join("GPUCache")]);
    let bytes = out.entries[0].bytes;
    assert_eq!(f.apply(&out.entries[0]), Outcome::Deleted { bytes, failures: vec![] });
    assert!(elsewhere.join("keep").exists());
}

#[test]
fn dirs_named_skips_extension_packages() {
    let f = Fixture::new();
    let root = f.home.join(".config/google-chrome");
    // An extension shipping a dir named Cache: Chrome would flag it corrupted.
    mkfile(&root.join("Default/Extensions/abcdef/1.0_0/Cache/data.js"), 10);
    mkfile(&root.join("Default/Cache/f"), 10);
    age_tree(&root);
    let rule = StoreRule {
        root: root.clone(),
        max_age_days: 0,
        unit: Unit::DirsNamed(vec!["Cache".into()]),
        glob: None,
        min_bytes: None,
    };
    assert_eq!(match_paths(&rule, &f.home), vec![root.join("Default/Cache")]);
}

const MODEL_REASON: &str = "re-downloads while Chromium's on-device AI features are on";

fn chromium_with_model(f: &Fixture) -> PathBuf {
    let d = f.home.join(".config/chromium");
    mkfile(&d.join("Default/Cache/x"), 1000);
    let model = d.join("OptGuideOnDeviceModel");
    mkfile(&model.join("2025.1/weights.bin"), 16 * 1024);
    age_tree(&f.home.join(".config"));
    model
}

#[test]
fn chromium_model_is_its_own_row() {
    let f = Fixture::new();
    let model = chromium_with_model(&f);
    let out = f.scan();
    assert_eq!(out.entries.len(), 2, "{:?}", out.entries);
    let main = out.entries.iter().find(|e| e.title == "Chromium: ~/.config/chromium").unwrap();
    assert_eq!(main.reason, "1 cache dirs");
    let Unit::DirsNamed(names) = &rule_of(main).unit else { panic!("not DirsNamed") };
    assert!(!names.iter().any(|n| n == "OptGuideOnDeviceModel"), "{names:?}");
    let m = out.entries.iter().find(|e| e.title == "Chromium on-device AI model").unwrap();
    assert_eq!(m.reason, MODEL_REASON);
    assert!(m.selected);
    let want = cleaner::fsutil::measure::measure(&model, &mut InodeSet::default()).unwrap().bytes;
    assert_eq!(m.bytes, want);
    assert_eq!(match_paths(rule_of(m), &f.home), vec![model]);
}

#[test]
fn model_row_skipped_while_running() {
    let f = Fixture::new();
    let model = chromium_with_model(&f);
    let out = f.scan();
    let m = out.entries.iter().find(|e| e.title == "Chromium on-device AI model").unwrap();
    fake_pid(&f.procs, 10, "/usr/lib/chromium/chromium", &["/usr/lib/chromium/chromium"]);
    assert_eq!(f.apply(m), Outcome::Skipped { reason: "Chromium is running".into() });
    assert!(model.join("2025.1/weights.bin").exists());
}

#[test]
fn model_only_when_configured() {
    let f = Fixture::new();
    let model = chromium_with_model(&f);
    let mut config = Config::default();
    config.app_caches.cache_dirs.retain(|n| n != "OptGuideOnDeviceModel");
    let out = f.with_ctx(&config, |ctx, cat| cat.scan(ctx, &mut InodeSet::default()).unwrap());
    assert_eq!(out.entries.len(), 1, "{:?}", out.entries);
    assert_eq!(out.entries[0].title, "Chromium: ~/.config/chromium");
    f.with_ctx(&config, |ctx, cat| {
        assert!(matches!(cat.apply(&out.entries[0], ctx), Outcome::Deleted { .. }));
    });
    assert!(model.join("2025.1/weights.bin").exists());
}

#[test]
fn foreign_app_root_yields_one_skip() {
    let f = Fixture::new();
    chromium_with_model(&f);
    let config = Config::default();
    let protector = Protector::new(&f.home, &config.protect);
    let tools = common::FakeRunner::new();
    let ctx = ScanCtx {
        home: &f.home,
        now: now(),
        config: &config,
        protector: &protector,
        tools: &tools,
        euid: common::my_uid() + 1,
        usage: None,
    };
    let out = AppCaches { proc_root: f.procs.clone() }.scan(&ctx, &mut InodeSet::default()).unwrap();
    assert!(out.entries.is_empty(), "{:?}", out.entries);
    assert_eq!(out.skipped.len(), 1, "{:?}", out.skipped);
}
