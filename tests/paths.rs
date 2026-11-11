mod common;

use cleaner::fmt::human_bytes;
use cleaner::paths::{Paths, expand};
use std::fs;
use tempfile::TempDir;

#[test]
fn for_home_layout() {
    let tmp = TempDir::new().unwrap();
    let home = tmp.path();
    let p = Paths::for_home(home);
    assert_eq!(p.home, home);
    assert_eq!(p.config_file, home.join(".config/cleaner/config.toml"));
    assert_eq!(p.state_dir, home.join(".local/state/cleaner"));
    assert_eq!(p.plans_dir, home.join(".local/state/cleaner/plans"));
    assert_eq!(p.log_file, home.join(".local/state/cleaner/log.jsonl"));
    assert_eq!(p.lock_file, home.join(".local/state/cleaner/cleaner.lock"));
    assert_eq!(p.systemd_user_dir, home.join(".config/systemd/user"));
    assert_eq!(p.applications_dir, home.join(".local/share/applications"));
}

#[test]
fn expand_tilde_and_glob() {
    let tmp = TempDir::new().unwrap();
    let home = tmp.path();
    for d in ["terragrunt", "terragrunt-inf", "other"] {
        fs::create_dir_all(home.join(".cache").join(d)).unwrap();
    }
    let got = expand("~/.cache/terragrunt*", home);
    assert_eq!(
        got,
        vec![
            home.join(".cache/terragrunt"),
            home.join(".cache/terragrunt-inf"),
        ]
    );
}

#[test]
fn expand_missing_is_empty() {
    let tmp = TempDir::new().unwrap();
    assert!(expand("~/nope", tmp.path()).is_empty());
}

#[test]
fn human_bytes_values() {
    assert_eq!(human_bytes(0), "0 B");
    assert_eq!(human_bytes(512), "512 B");
    assert_eq!(human_bytes(1536), "1.5 KiB");
    assert_eq!(human_bytes(76_665_405_849), "71.4 GiB");
}

#[test]
fn plan4_paths() {
    let h = std::path::Path::new("/home/x");
    let p = Paths::for_home(h);
    assert_eq!(p.usage_index, h.join(".local/state/cleaner/usage.json"));
    assert_eq!(p.space_state, h.join(".local/state/cleaner/cleaner-space.json"));
}

#[test]
fn expand_roots_drops_nested() {
    let home = TempDir::new().unwrap();
    let h = home.path();
    for d in ["a/b", "c"] {
        fs::create_dir_all(h.join(d)).unwrap();
    }
    let roots: Vec<String> = ["~/a", "~/a/b", "~/c", "~/missing"].map(String::from).into();
    assert_eq!(cleaner::paths::expand_roots(&roots, h), vec![h.join("a"), h.join("c")]);
}
