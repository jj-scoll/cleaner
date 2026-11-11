mod common;

use cleaner::config::{
    Config, DEFAULT_CONFIG_TOML, default_stores, is_read_only, load, write_default_if_missing,
};
use common::mkfile;
use std::fs;
use std::os::unix::fs::PermissionsExt;
use tempfile::TempDir;

fn write_config(dir: &TempDir, text: &str) -> std::path::PathBuf {
    let p = dir.path().join(".config/cleaner/config.toml");
    fs::create_dir_all(p.parent().unwrap()).unwrap();
    fs::write(&p, text).unwrap();
    p
}

#[test]
fn missing_file_gives_defaults() {
    let home = TempDir::new().unwrap();
    let (cfg, warnings) = load(&home.path().join(".config/cleaner/config.toml")).unwrap();
    assert_eq!(cfg.artifacts.stale_after_days, 30);
    // The plan's table (spec §5.2c) has 15 rows plus 7 one-entry-each = 22.
    assert_eq!(default_stores().len(), 22);
    assert_eq!(cfg.caches.stores.len(), 22);
    assert_eq!(cfg.user_rules[0].path, "~/Downloads");
    assert!(warnings.is_empty(), "{warnings:?}");
}

#[test]
fn partial_file_merges_defaults() {
    let home = TempDir::new().unwrap();
    let p = write_config(&home, "[artifacts]\nstale_after_days = 7\n");
    let (cfg, warnings) = load(&p).unwrap();
    assert_eq!(cfg.artifacts.stale_after_days, 7);
    assert_eq!(cfg.artifacts.max_depth, 6);
    assert_eq!(cfg.trash.max_age_days, 30);
    assert!(warnings.is_empty(), "{warnings:?}");
}

#[test]
fn unknown_key_warns() {
    let home = TempDir::new().unwrap();
    let p = write_config(&home, "[artifacts]\nbogus = 1\n");
    let (cfg, warnings) = load(&p).unwrap();
    assert_eq!(cfg.artifacts.stale_after_days, 30);
    assert!(
        warnings.contains(&"unknown config key: artifacts.bogus".to_string()),
        "{warnings:?}"
    );
}

#[test]
fn malformed_config_errors() {
    let home = TempDir::new().unwrap();
    let p = write_config(&home, "[artifacts");
    let err = load(&p).unwrap_err();
    assert!(format!("{err:#}").contains("config.toml"), "{err:#}");
}

/// Strips `# ` from lines that are commented-out TOML (`[header]` or `key = value`).
fn uncomment(text: &str) -> String {
    text.lines()
        .map(|line| match line.strip_prefix("# ") {
            Some(rest)
                if rest.starts_with('[')
                    || rest.split_once(" = ").is_some_and(|(k, _)| {
                        !k.is_empty() && k.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
                    }) =>
            {
                rest
            }
            _ => line,
        })
        .collect::<Vec<_>>()
        .join("\n")
}

#[test]
fn default_toml_parses_to_defaults() {
    let want = toml::to_string(&Config::default()).unwrap();

    let uncommented = uncomment(DEFAULT_CONFIG_TOML);
    assert_ne!(uncommented, DEFAULT_CONFIG_TOML, "nothing was commented out");
    let mut parsed: Config = toml::from_str(&uncommented).unwrap();
    // The template's keep_newest block is a generic example; the default list is empty.
    assert_eq!(parsed.caches.keep_newest.len(), 1);
    parsed.caches.keep_newest.clear();
    assert_eq!(toml::to_string(&parsed).unwrap(), want);

    // As written (all commented), the file loads to defaults with no warnings.
    let home = TempDir::new().unwrap();
    let p = home.path().join(".config/cleaner/config.toml");
    write_default_if_missing(&p).unwrap();
    assert_eq!(fs::read_to_string(&p).unwrap(), DEFAULT_CONFIG_TOML);
    let (cfg, warnings) = load(&p).unwrap();
    assert_eq!(toml::to_string(&cfg).unwrap(), want);
    assert!(warnings.is_empty(), "{warnings:?}");

    // An existing file is left alone.
    fs::write(&p, "timer_time = \"04:00\"\n").unwrap();
    write_default_if_missing(&p).unwrap();
    assert_eq!(load(&p).unwrap().0.timer_time, "04:00");
}

#[test]
fn read_only_detection() {
    let home = TempDir::new().unwrap();
    let real = home.path().join("nix-store/config.toml");
    mkfile(&real, 10);
    let link = home.path().join("config.toml");
    std::os::unix::fs::symlink(&real, &link).unwrap();
    assert!(is_read_only(&link));
    assert!(!is_read_only(&real));

    fs::set_permissions(&real, fs::Permissions::from_mode(0o444)).unwrap();
    assert!(is_read_only(&real));
}

#[test]
fn plan2_sections_partial() {
    let home = TempDir::new().unwrap();
    let p = write_config(&home, "[nix]\nkeep = 3\n[nix.keep_profile]\nsystem = 10\n");
    let (cfg, warnings) = load(&p).unwrap();
    assert!(warnings.is_empty(), "{warnings:?}");
    assert_eq!(cfg.nix.keep, 3);
    assert_eq!(cfg.nix.keep_profile["system"], 10);
    assert_eq!(cfg.nix.keep_profile.len(), 1);
    let want = Config::default().nix;
    assert_eq!(cfg.nix.enabled, want.enabled);
    assert_eq!(cfg.nix.profiles_dir, "~/.local/state/nix/profiles");
    assert_eq!(cfg.nix.system_profile, "/nix/var/nix/profiles/system");
    assert_eq!(cfg.nix.roots, None);
    assert_eq!(cfg.nix.max_depth, 6);
    assert_eq!(cfg.nix.devbox_idle_days, 30);
    assert!(cfg.nix.gc);
    assert_eq!(cfg.worktrees, Config::default().worktrees);
    assert_eq!(cfg.docker, Config::default().docker);
    assert_eq!(cfg.app_caches, Config::default().app_caches);
}

#[test]
fn plan2_defaults() {
    let cfg = Config::default();
    assert!(cfg.worktrees.enabled && cfg.app_caches.enabled && cfg.docker.enabled && cfg.nix.enabled);
    assert_eq!((cfg.worktrees.max_depth, cfg.worktrees.idle_days), (6, 3));
    assert_eq!((cfg.docker.container_days, cfg.docker.build_cache_days), (7, 7));
    assert_eq!(cfg.nix.keep, 5);
    assert!(cfg.caches.uv_prune);
    assert!(cfg.caches.go_clean);
    let names: Vec<&str> = cfg.app_caches.apps.iter().map(|a| a.name.as_str()).collect();
    assert_eq!(
        names,
        ["Chrome", "Chromium", "Firefox", "Slack", "Teams", "Claude", "Cursor", "Discord"]
    );
    // `claude` is Claude Code, never Claude Desktop.
    assert!(cfg.app_caches.apps.iter().all(|a| !a.exe.iter().any(|e| e == "claude")));
    let firefox = &cfg.app_caches.apps[2];
    assert_eq!(firefox.locks, ["~/.mozilla/firefox/*/lock"]);
}

#[test]
fn default_toml_lists_plan2_keys() {
    let t: toml::Table = toml::from_str(&uncomment(DEFAULT_CONFIG_TOML)).unwrap();
    assert_eq!(t["caches"]["uv_prune"].as_bool(), Some(true));
    assert_eq!(t["caches"]["go_clean"].as_bool(), Some(true));
    assert_eq!(t["worktrees"]["idle_days"].as_integer(), Some(3));
    assert_eq!(t["app_caches"]["cache_dirs"].as_array().unwrap().len(), 6);
    assert_eq!(t["app_caches"]["apps"].as_array().unwrap().len(), 8);
    assert_eq!(t["docker"]["build_cache_days"].as_integer(), Some(7));
    assert_eq!(t["nix"]["keep"].as_integer(), Some(5));
    assert!(t["nix"]["keep_profile"].as_table().unwrap().is_empty());
    assert_eq!(t["nix"]["gc"].as_bool(), Some(true));
}

#[test]
fn default_toml_lists_plan4_keys() {
    let t: toml::Table = toml::from_str(&uncomment(DEFAULT_CONFIG_TOML)).unwrap();
    assert_eq!(t["scan_roots"].as_array().unwrap().len(), 1);
    assert_eq!(t["scan_roots"][0].as_str(), Some("~"));
    assert_eq!(t["low_space"]["percent"].as_integer(), Some(10));
    assert_eq!(t["low_space"]["interval"].as_str(), Some("10min"));
    assert_eq!(t["low_space"]["enabled"].as_bool(), Some(true));
    assert_eq!(t["disk_map"]["roots"][0].as_str(), Some("~"));
    assert!(t["artifacts"].get("roots").is_none());
    assert!(t["worktrees"].get("roots").is_none());
    assert!(t["nix"].get("roots").is_none());
}

#[test]
fn roots_for_inherits_and_overrides() {
    let c = Config::default();
    assert_eq!(c.roots_for(&c.artifacts.roots), ["~"]);
    assert!(c.root_overrides().is_empty());

    let (c, w) = cleaner::config::parse("scan_roots = [\"~\", \"/x\"]\n").unwrap();
    assert!(w.is_empty(), "{w:?}");
    assert_eq!(c.roots_for(&c.artifacts.roots), ["~", "/x"]);
    assert_eq!(c.roots_for(&c.nix.roots), ["~", "/x"]);

    let (c, _) = cleaner::config::parse("[artifacts]\nroots = [\"~/src\"]\n").unwrap();
    assert_eq!(c.roots_for(&c.artifacts.roots), ["~/src"]);
    assert_eq!(c.roots_for(&c.worktrees.roots), ["~"]);
    assert_eq!(c.root_overrides(), vec!["artifacts"]);
}

#[test]
fn old_config_with_roots_still_loads() {
    let home = TempDir::new().unwrap();
    let p = write_config(&home, "[worktrees]\nroots = [\"~/wt\"]\n");
    let (cfg, warnings) = load(&p).unwrap();
    assert!(warnings.is_empty(), "{warnings:?}");
    assert_eq!(cfg.worktrees.roots, Some(vec!["~/wt".to_string()]));
}

#[test]
fn idle_defaults_plan4() {
    let c = Config::default();
    assert_eq!(c.idle_folders.min_bytes, 1 << 30);
    assert_eq!(c.idle_folders.idle_days, 90);
    assert!(c.idle_folders.enabled);
}

#[test]
fn built_in_defaults_name_no_personal_tool() {
    // Assembled so this file itself stays free of the name.
    let name = format!("regul{}", 8);
    let dumped = toml::to_string(&Config::default()).unwrap().to_lowercase();
    assert!(!dumped.contains(&name), "Config::default() mentions {name}");
    assert!(!DEFAULT_CONFIG_TOML.to_lowercase().contains(&name), "template mentions {name}");
}
