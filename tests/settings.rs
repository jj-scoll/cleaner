use cleaner::config::{self, Config, DEFAULT_CONFIG_TOML, StoreCfg};
use cleaner::settings::{ConfigDoc, validate};
use std::fs;
use std::path::PathBuf;
use tempfile::TempDir;

fn write_config(dir: &TempDir, text: &str) -> PathBuf {
    let p = dir.path().join("config.toml");
    fs::write(&p, text).unwrap();
    p
}

#[test]
fn update_preserves_comments_and_unknown_keys() {
    let dir = TempDir::new().unwrap();
    // At the top, so it is a root key and not part of the last table.
    let path = write_config(&dir, &format!("future_key = 1\n{DEFAULT_CONFIG_TOML}"));
    let mut doc = ConfigDoc::load(&path).unwrap();
    let mut c = doc.config().unwrap();
    c.trash.max_age_days = 14;
    doc.update(&c).unwrap();
    doc.save(&path).unwrap();

    let text = fs::read_to_string(&path).unwrap();
    assert!(text.contains("# cleaner configuration."), "{text}");
    assert!(text.contains("future_key = 1"), "{text}");
    let (loaded, warnings) = config::load(&path).unwrap();
    assert_eq!(loaded.trash.max_age_days, 14);
    assert_eq!(warnings, vec!["unknown config key: future_key".to_string()]);
}

#[test]
fn unchanged_config_writes_identical_text() {
    let dir = TempDir::new().unwrap();
    let path = write_config(&dir, DEFAULT_CONFIG_TOML);
    let mut doc = ConfigDoc::load(&path).unwrap();
    let c = doc.config().unwrap();
    doc.update(&c).unwrap();
    doc.save(&path).unwrap();
    assert_eq!(fs::read_to_string(&path).unwrap(), DEFAULT_CONFIG_TOML);
}

#[test]
fn user_rules_roundtrip() {
    let dir = TempDir::new().unwrap();
    let path = write_config(&dir, DEFAULT_CONFIG_TOML);
    let mut doc = ConfigDoc::load(&path).unwrap();
    let mut c = doc.config().unwrap();
    c.user_rules = vec![
        StoreCfg {
            path: "~/Downloads".into(),
            max_age_days: 30,
            unit: "files".into(),
            glob: Some("*.iso".into()),
            min_bytes: None,
        },
        StoreCfg {
            path: "/srv/dumps".into(),
            max_age_days: 7,
            unit: "files".into(),
            glob: None,
            min_bytes: Some(1 << 20),
        },
    ];
    doc.update(&c).unwrap();
    doc.save(&path).unwrap();

    let (loaded, warnings) = config::load(&path).unwrap();
    assert!(warnings.is_empty(), "{warnings:?}");
    assert_eq!(loaded.user_rules.len(), 2);
    assert_eq!(loaded.user_rules[0].unit, "files");
    assert_eq!(loaded.user_rules[0].glob.as_deref(), Some("*.iso"));
    assert_eq!(loaded.user_rules[0].max_age_days, 30);
    assert_eq!(loaded.user_rules[1].path, "/srv/dumps");
    assert_eq!(loaded.user_rules[1].min_bytes, Some(1 << 20));
}

/// Removing every folder rule must stick: an empty `[[user_rules]]` prints as
/// nothing, and serde would then bring back the default ~/Downloads rule.
#[test]
fn removing_all_user_rules_sticks() {
    for start in [DEFAULT_CONFIG_TOML, "[[user_rules]]\npath = \"~/Junk\"\nmax_age_days = 7\nunit = \"files\"\n"] {
        let dir = TempDir::new().unwrap();
        let path = write_config(&dir, start);
        let mut doc = ConfigDoc::load(&path).unwrap();
        let mut c = doc.config().unwrap();
        c.user_rules.clear();
        doc.update(&c).unwrap();
        doc.save(&path).unwrap();

        let (loaded, _) = config::load(&path).unwrap();
        let text = fs::read_to_string(&path).unwrap();
        assert!(loaded.user_rules.is_empty(), "rules came back: {:?}\n{text}", loaded.user_rules);

        // And a rule added afterwards is kept.
        let mut doc = ConfigDoc::load(&path).unwrap();
        let mut c = doc.config().unwrap();
        c.user_rules.push(StoreCfg { path: "~/Junk".into(), max_age_days: 7, unit: "files".into(), glob: None, min_bytes: None });
        doc.update(&c).unwrap();
        doc.save(&path).unwrap();
        let (loaded, _) = config::load(&path).unwrap();
        assert_eq!(loaded.user_rules.len(), 1, "{}", fs::read_to_string(&path).unwrap());
    }
}

#[test]
fn validate_rejects_bad_values() {
    let mut c = Config { timer_time: "25:00".into(), ..Config::default() };
    c.artifacts.stale_after_days = 0;
    c.user_rules[0].path = "Downloads".into();
    let errors = validate(&c).unwrap_err();
    assert_eq!(errors.len(), 3, "{errors:?}");
    for want in ["HH:MM", "at least 1", "must start with ~/ or /"] {
        assert!(errors.iter().any(|e| e.contains(want)), "no {want:?} in {errors:?}");
    }
    assert!(validate(&Config::default()).is_ok());
}

/// A folder rule pre-selects every file older than its age under its path, so
/// age 0 or a path that is the whole home or root would select everything.
#[test]
fn validate_rejects_folder_rules_that_cover_everything() {
    for (path, age, want) in [
        ("~/Downloads", 0, "at least 1 day"),
        ("~/", 90, "whole home or root"),
        ("/", 90, "whole home or root"),
        ("/.", 90, "whole home or root"),
        ("~/./", 90, "whole home or root"),
        ("~/Downloads/../..", 90, "can't contain .."),
    ] {
        let mut c = Config::default();
        c.user_rules[0].path = path.into();
        c.user_rules[0].max_age_days = age;
        let errors = validate(&c).unwrap_err();
        assert!(errors.iter().any(|e| e.contains(want)), "{path} {age}: no {want:?} in {errors:?}");
    }
    let mut c = Config::default();
    c.user_rules[0].path = "/srv/dumps/".into();
    assert!(validate(&c).is_ok());
}

#[test]
fn plan2_keys_roundtrip() {
    let dir = TempDir::new().unwrap();
    let path = write_config(&dir, DEFAULT_CONFIG_TOML);
    let mut doc = ConfigDoc::load(&path).unwrap();
    let mut c = doc.config().unwrap();
    c.nix.keep = 3;
    c.docker.enabled = false;
    doc.update(&c).unwrap();
    doc.save(&path).unwrap();

    let text = fs::read_to_string(&path).unwrap();
    for comment in ["# cleaner configuration.", "# Generations kept per profile (by count), the current one included.", "# Stopped containers older than this many days."] {
        assert!(text.contains(comment), "lost {comment:?}:\n{text}");
    }
    let (loaded, warnings) = config::load(&path).unwrap();
    assert!(warnings.is_empty(), "{warnings:?}");
    assert_eq!(loaded.nix.keep, 3);
    assert!(!loaded.docker.enabled);
}

/// Every new editable key is written, and nothing else changes.
#[test]
fn plan2_every_key_roundtrips() {
    let dir = TempDir::new().unwrap();
    let path = write_config(&dir, DEFAULT_CONFIG_TOML);
    let mut doc = ConfigDoc::load(&path).unwrap();
    let mut c = doc.config().unwrap();
    c.caches.uv_prune = false;
    c.worktrees.enabled = false;
    c.worktrees.idle_days = 11;
    c.app_caches.enabled = false;
    c.docker.enabled = false;
    c.docker.container_days = 12;
    c.docker.build_cache_days = 13;
    c.nix.enabled = false;
    c.nix.keep = 2;
    c.nix.devbox_idle_days = 14;
    c.nix.gc = false;
    doc.update(&c).unwrap();
    doc.save(&path).unwrap();

    let (loaded, warnings) = config::load(&path).unwrap();
    assert!(warnings.is_empty(), "{warnings:?}");
    assert_eq!(loaded, c, "{}", fs::read_to_string(&path).unwrap());
}

#[test]
fn validate_plan2() {
    let mut c = Config::default();
    c.nix.keep = 0;
    c.worktrees.idle_days = 0;
    let errors = validate(&c).unwrap_err();
    assert_eq!(errors.len(), 2, "{errors:?}");
    for want in ["nix keep must be at least 1", "worktree idle_days must be at least 1"] {
        assert!(errors.iter().any(|e| e.contains(want)), "no {want:?} in {errors:?}");
    }
}

#[test]
fn save_refuses_read_only() {
    let dir = TempDir::new().unwrap();
    let target = dir.path().join("managed.toml");
    fs::write(&target, DEFAULT_CONFIG_TOML).unwrap();
    let link = dir.path().join("config.toml");
    std::os::unix::fs::symlink(&target, &link).unwrap();

    let mut doc = ConfigDoc::load(&link).unwrap();
    let mut c = doc.config().unwrap();
    c.trash.max_age_days = 14;
    doc.update(&c).unwrap();
    let err = doc.save(&link).unwrap_err();
    assert!(format!("{err:#}").contains("read-only"), "{err:#}");
    assert_eq!(fs::read_to_string(&target).unwrap(), DEFAULT_CONFIG_TOML);
    assert!(fs::symlink_metadata(&link).unwrap().file_type().is_symlink());
}

#[test]
fn artifacts_roots_none_removes_key() {
    let dir = TempDir::new().unwrap();
    let path = write_config(&dir, "[artifacts]\nroots = [\"~/src\"]\n");
    let mut doc = ConfigDoc::load(&path).unwrap();
    let mut c = doc.config().unwrap();
    assert_eq!(c.artifacts.roots, Some(vec!["~/src".to_string()]));
    c.artifacts.roots = None;
    doc.update(&c).unwrap();
    doc.save(&path).unwrap();
    let text = fs::read_to_string(&path).unwrap();
    assert!(!text.contains("roots ="), "{text}");
}

#[test]
fn settings_writes_scan_roots_and_low_space() {
    let dir = TempDir::new().unwrap();
    let path = write_config(&dir, DEFAULT_CONFIG_TOML);
    let mut doc = ConfigDoc::load(&path).unwrap();
    let mut c = doc.config().unwrap();
    c.scan_roots = vec!["~".into(), "/data/repos".into()];
    c.low_space.percent = 8;
    doc.update(&c).unwrap();
    doc.save(&path).unwrap();

    let text = fs::read_to_string(&path).unwrap();
    let roots_at = text.find("scan_roots = [\"~\", \"/data/repos\"]").unwrap_or_else(|| panic!("{text}"));
    let first_table = text.lines().position(|l| l.starts_with('[')).unwrap();
    let before: usize = text.lines().take(first_table).map(|l| l.len() + 1).sum();
    assert!(roots_at < before, "scan_roots after the first table: {text}");
    let low = text.split("[low_space]").nth(1).unwrap_or_else(|| panic!("{text}"));
    assert!(low.lines().any(|l| l.trim() == "percent = 8"), "{text}");
    for l in DEFAULT_CONFIG_TOML.lines().filter(|l| l.trim_start().starts_with('#')) {
        assert!(text.lines().any(|t| t == l), "comment lost: {l}");
    }
    let (loaded, _) = config::load(&path).unwrap();
    assert_eq!(loaded.scan_roots, c.scan_roots);
    assert_eq!(loaded.low_space.percent, 8);
}

#[test]
fn save_writes_no_artifacts_roots_and_keeps_go_clean() {
    let dir = TempDir::new().unwrap();
    let path = write_config(&dir, "[caches]\ngo_clean = false\n");
    let mut doc = ConfigDoc::load(&path).unwrap();
    let mut c = doc.config().unwrap();
    c.scan_roots = vec!["~".into(), "/x".into()];
    doc.update(&c).unwrap();
    doc.save(&path).unwrap();
    let (loaded, _) = config::load(&path).unwrap();
    assert_eq!(loaded.scan_roots, vec!["~".to_string(), "/x".to_string()]);
    assert_eq!(loaded.artifacts.roots, None);
    assert!(!fs::read_to_string(&path).unwrap().contains("[artifacts]"));
    assert!(!loaded.caches.go_clean, "go_clean was rewritten");
}

#[test]
fn validate_rejects_bad_plan4_values() {
    let bad = |f: &dyn Fn(&mut Config)| {
        let mut c = Config::default();
        f(&mut c);
        validate(&c).unwrap_err()
    };
    assert_eq!(bad(&|c| c.scan_roots = vec![]), ["add at least one scan root"]);
    assert_eq!(bad(&|c| c.scan_roots = vec!["storage".into()]), ["scan root \"storage\" must start with ~ or /"]);
    assert_eq!(bad(&|c| c.low_space.percent = 0), ["low-space percent must be between 1 and 50"]);
    assert_eq!(bad(&|c| c.low_space.percent = 51), ["low-space percent must be between 1 and 50"]);
    assert_eq!(
        bad(&|c| c.low_space.interval = "soon".into()),
        ["low-space interval: invalid interval \"soon\" (expected e.g. 10min or 1h)"]
    );
}
