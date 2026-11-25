mod common;

use cleaner::category::ide_versions::IdeVersions;
use cleaner::category::{Category, Outcome, ScanCtx, ScanOutput};
use cleaner::config::Config;
use cleaner::fsutil::measure::InodeSet;
use cleaner::plan::{Entry, EntryKind};
use cleaner::protect::Protector;
use common::{mkfile, now, set_age};
use std::os::unix::fs::symlink;
use std::path::{Path, PathBuf};

fn with_ctx<R>(home: &Path, config: &Config, f: impl FnOnce(&ScanCtx) -> R) -> R {
    let protector = Protector::new(home, &config.protect);
    let tools = common::FakeRunner::new();
    let ctx = ScanCtx {
        home,
        now: now(),
        config,
        protector: &protector,
        tools: &tools,
        euid: common::my_uid(),
        usage: None,
        hold: None,
    };
    f(&ctx)
}

fn scan_with(home: &Path, config: &Config) -> ScanOutput {
    with_ctx(home, config, |ctx| {
        IdeVersions.scan(ctx, &mut InodeSet::default())
    })
    .unwrap()
}

fn scan(home: &Path) -> ScanOutput {
    scan_with(home, &Config::default())
}

/// Sets `root` and everything under it to `days` ago, children first.
fn age_tree(root: &Path, days: u64) {
    if root.is_dir() && !root.is_symlink() {
        for e in std::fs::read_dir(root).unwrap() {
            age_tree(&e.unwrap().path(), days);
        }
    }
    if !root.is_symlink() {
        set_age(root, days);
    }
}

/// A version dir holding one file, last changed `days` ago.
fn version_dir(path: &Path, days: u64) -> PathBuf {
    mkfile(&path.join("data/index.bin"), 4096);
    age_tree(path, days);
    path.to_path_buf()
}

fn offered(out: &ScanOutput, home: &Path) -> Vec<String> {
    let mut v: Vec<String> = out
        .entries
        .iter()
        .flat_map(|e| match &e.kind {
            EntryKind::Item { targets } => targets
                .iter()
                .map(|t| t.path.strip_prefix(home).unwrap().display().to_string())
                .collect::<Vec<_>>(),
            other => panic!("not an item: {other:?}"),
        })
        .collect();
    v.sort();
    v
}

fn entry_for<'a>(out: &'a ScanOutput, home: &Path, rel: &str) -> &'a Entry {
    out.entries
        .iter()
        .find(
            |e| matches!(&e.kind, EntryKind::Item { targets } if targets[0].path == home.join(rel)),
        )
        .unwrap_or_else(|| panic!("no entry for {rel}: {:#?}", out.entries))
}

// ---- JetBrains ----------------------------------------------------------

#[test]
fn jetbrains_keeps_newest_per_product_and_never_touches_config() {
    let tmp = tempfile::tempdir().unwrap();
    let h = tmp.path();
    let cache = h.join(".cache/JetBrains");
    let share = h.join(".local/share/JetBrains");
    let config = h.join(".config/JetBrains");
    for v in ["2024.2", "2024.3", "2025.1"] {
        version_dir(&cache.join(format!("IntelliJIdea{v}")), 60);
        version_dir(&config.join(format!("IntelliJIdea{v}")), 60);
    }
    version_dir(&share.join("IntelliJIdea2024.3"), 60);
    version_dir(&share.join("IntelliJIdea2025.1"), 60);
    // One version only: nothing to offer.
    version_dir(&cache.join("PyCharmCE2024.3"), 60);
    // Not a version dir.
    version_dir(&cache.join("Toolbox"), 60);

    let out = scan(h);
    assert_eq!(
        offered(&out, h),
        [
            ".cache/JetBrains/IntelliJIdea2024.2",
            ".cache/JetBrains/IntelliJIdea2024.3",
            ".local/share/JetBrains/IntelliJIdea2024.3",
        ]
    );
    let e = entry_for(&out, h, ".cache/JetBrains/IntelliJIdea2024.2");
    assert_eq!(e.category, "ide_versions");
    assert!(e.selected, "untouched 60 days: pre-selected");
    assert!(e.reason.contains("2025.1"), "{}", e.reason);
}

#[test]
fn jetbrains_newest_version_may_exist_only_in_config() {
    let tmp = tempfile::tempdir().unwrap();
    let h = tmp.path();
    version_dir(&h.join(".cache/JetBrains/GoLand2024.3"), 60);
    version_dir(&h.join(".config/JetBrains/GoLand2025.1"), 60);

    assert_eq!(offered(&scan(h), h), [".cache/JetBrains/GoLand2024.3"]);
}

#[test]
fn jetbrains_single_version_product_offers_nothing() {
    let tmp = tempfile::tempdir().unwrap();
    let h = tmp.path();
    version_dir(&h.join(".cache/JetBrains/CLion2024.3"), 60);
    version_dir(&h.join(".local/share/JetBrains/CLion2024.3"), 60);
    version_dir(&h.join(".config/JetBrains/CLion2024.3"), 60);
    version_dir(&h.join(".cache/JetBrains/RustRover2025.1"), 60);

    let out = scan(h);
    assert!(out.entries.is_empty(), "{:#?}", out.entries);
}

#[test]
fn jetbrains_recently_used_old_version_is_offered_unticked() {
    let tmp = tempfile::tempdir().unwrap();
    let h = tmp.path();
    // An EAP is installed but the older release is still the daily driver.
    version_dir(&h.join(".cache/JetBrains/PyCharm2025.1"), 2);
    version_dir(&h.join(".cache/JetBrains/PyCharm2025.2"), 1);

    let out = scan(h);
    let e = entry_for(&out, h, ".cache/JetBrains/PyCharm2025.1");
    assert!(!e.selected, "used 2 days ago");

    let mut c = Config::default();
    c.ide_versions.idle_days = 1;
    let out = scan_with(h, &c);
    assert!(entry_for(&out, h, ".cache/JetBrains/PyCharm2025.1").selected);
}

#[test]
fn jetbrains_symlinked_version_dir_is_skipped() {
    let tmp = tempfile::tempdir().unwrap();
    let h = tmp.path();
    let elsewhere = version_dir(&h.join("elsewhere/WebStorm2024.1"), 60);
    std::fs::create_dir_all(h.join(".cache/JetBrains")).unwrap();
    symlink(&elsewhere, h.join(".cache/JetBrains/WebStorm2024.1")).unwrap();
    version_dir(&h.join(".cache/JetBrains/WebStorm2024.2"), 60);
    version_dir(&h.join(".cache/JetBrains/WebStorm2025.1"), 60);

    assert_eq!(offered(&scan(h), h), [".cache/JetBrains/WebStorm2024.2"]);
}

#[test]
fn jetbrains_apply_deletes_old_version() {
    let tmp = tempfile::tempdir().unwrap();
    let h = tmp.path();
    let old = version_dir(&h.join(".cache/JetBrains/DataGrip2024.1"), 60);
    let new = version_dir(&h.join(".cache/JetBrains/DataGrip2024.2"), 60);
    let config = Config::default();
    let out = scan_with(h, &config);
    let res = with_ctx(h, &config, |ctx| IdeVersions.apply(&out.entries[0], ctx));
    assert!(matches!(res, Outcome::Deleted { .. }), "{res:?}");
    assert!(!old.exists() && new.exists());
}

// ---- VS Code ------------------------------------------------------------

fn ext(dir: &Path, name: &str) -> PathBuf {
    let p = dir.join(name);
    mkfile(&p.join("package.json"), 500);
    age_tree(&p, 10);
    p
}

/// An `extensions.json` naming each `(id, version, dir name)`.
fn extensions_json(path: &Path, used: &[(&str, &str, &str)]) {
    let items: Vec<String> = used
        .iter()
        .map(|(id, v, dir)| {
            format!(
                r#"{{"identifier":{{"id":"{id}"}},"version":"{v}","location":{{"$mid":1,"path":"/x/{dir}","scheme":"file"}},"relativeLocation":"{dir}"}}"#
            )
        })
        .collect();
    mkfile(path, 0);
    std::fs::write(path, format!("[{}]", items.join(","))).unwrap();
}

fn vscode_fixture(h: &Path) -> PathBuf {
    let d = h.join(".vscode/extensions");
    ext(&d, "ms-python.python-2024.1.0");
    ext(&d, "ms-python.python-2024.2.0");
    ext(&d, "ms-python.python-2024.3.0");
    ext(&d, "rust-lang.rust-analyzer-0.3.1-linux-x64");
    ext(&d, "rust-lang.rust-analyzer-0.3.2-linux-x64");
    ext(&d, "ms-vscode-remote.remote-ssh-0.110.1");
    d
}

#[test]
fn vscode_keeps_newest_and_referenced_versions() {
    let tmp = tempfile::tempdir().unwrap();
    let h = tmp.path();
    let d = vscode_fixture(h);
    extensions_json(
        &d.join("extensions.json"),
        &[
            ("ms-python.python", "2024.2.0", "ms-python.python-2024.2.0"),
            (
                "rust-lang.rust-analyzer",
                "0.3.2",
                "rust-lang.rust-analyzer-0.3.2-linux-x64",
            ),
        ],
    );

    let out = scan(h);
    assert_eq!(
        offered(&out, h),
        [
            ".vscode/extensions/ms-python.python-2024.1.0",
            ".vscode/extensions/rust-lang.rust-analyzer-0.3.1-linux-x64",
        ]
    );
    assert!(out.entries.iter().all(|e| e.selected), "{:#?}", out.entries);
}

#[test]
fn vscode_without_extensions_json_offers_older_unticked() {
    let tmp = tempfile::tempdir().unwrap();
    let h = tmp.path();
    vscode_fixture(h);

    let out = scan(h);
    assert_eq!(
        offered(&out, h),
        [
            ".vscode/extensions/ms-python.python-2024.1.0",
            ".vscode/extensions/ms-python.python-2024.2.0",
            ".vscode/extensions/rust-lang.rust-analyzer-0.3.1-linux-x64",
        ]
    );
    assert!(
        out.entries.iter().all(|e| !e.selected),
        "{:#?}",
        out.entries
    );
}

#[test]
fn vscode_unreadable_extensions_json_offers_nothing() {
    let tmp = tempfile::tempdir().unwrap();
    let h = tmp.path();
    let d = vscode_fixture(h);
    std::fs::write(d.join("extensions.json"), "[{oops").unwrap();

    let out = scan(h);
    assert!(out.entries.is_empty(), "{:#?}", out.entries);
    assert_eq!(out.skipped.len(), 1, "{:?}", out.skipped);
}

#[test]
fn vscode_profile_extensions_json_keeps_its_versions() {
    let tmp = tempfile::tempdir().unwrap();
    let h = tmp.path();
    let d = vscode_fixture(h);
    extensions_json(&d.join("extensions.json"), &[]);
    extensions_json(
        &h.join(".config/Code/User/profiles/-5a1b/extensions.json"),
        &[("ms-python.python", "2024.1.0", "ms-python.python-2024.1.0")],
    );

    assert_eq!(
        offered(&scan(h), h),
        [
            ".vscode/extensions/ms-python.python-2024.2.0",
            ".vscode/extensions/rust-lang.rust-analyzer-0.3.1-linux-x64",
        ]
    );
}

#[test]
fn vscode_oss_is_scanned_and_cursor_is_protected() {
    let tmp = tempfile::tempdir().unwrap();
    let h = tmp.path();
    for dir in [".vscode-oss/extensions", ".cursor/extensions"] {
        let d = h.join(dir);
        ext(&d, "foo.bar-1.0.0");
        ext(&d, "foo.bar-1.1.0");
        extensions_json(&d.join("extensions.json"), &[]);
    }

    // ~/.cursor is a built-in protected path.
    assert_eq!(
        offered(&scan(h), h),
        [".vscode-oss/extensions/foo.bar-1.0.0"]
    );
}

#[test]
fn vscode_symlinked_extension_is_skipped() {
    let tmp = tempfile::tempdir().unwrap();
    let h = tmp.path();
    let d = h.join(".vscode/extensions");
    let elsewhere = ext(&h.join("elsewhere"), "foo.bar-1.0.0");
    std::fs::create_dir_all(&d).unwrap();
    symlink(&elsewhere, d.join("foo.bar-1.0.0")).unwrap();
    ext(&d, "foo.bar-1.1.0");
    extensions_json(&d.join("extensions.json"), &[]);

    let out = scan(h);
    assert!(out.entries.is_empty(), "{:#?}", out.entries);
}

// ---- config -------------------------------------------------------------

#[test]
fn defaults_enable_both() {
    let c = Config::default().ide_versions;
    assert!(c.enabled && c.jetbrains && c.vscode);
    assert_eq!(c.idle_days, 30);
}

#[test]
fn config_disables_each_source() {
    let tmp = tempfile::tempdir().unwrap();
    let h = tmp.path();
    version_dir(&h.join(".cache/JetBrains/GoLand2024.3"), 60);
    version_dir(&h.join(".cache/JetBrains/GoLand2025.1"), 60);
    let d = vscode_fixture(h);
    extensions_json(&d.join("extensions.json"), &[]);
    assert_eq!(scan(h).entries.len(), 4);

    let mut c = Config::default();
    c.ide_versions.jetbrains = false;
    assert_eq!(scan_with(h, &c).entries.len(), 3);
    let mut c = Config::default();
    c.ide_versions.vscode = false;
    assert_eq!(scan_with(h, &c).entries.len(), 1);
    let mut c = Config::default();
    c.ide_versions.enabled = false;
    assert!(scan_with(h, &c).entries.is_empty());
}
