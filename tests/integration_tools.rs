//! Opt-in checks against the real docker, nix and uv. They scan only: nothing
//! here calls `apply`, so nothing is deleted. Each test returns early unless
//! `CLEANER_IT=1`, which keeps the normal suites hermetic.

use std::ffi::OsStr;
use std::path::Path;

use cleaner::category::docker::Docker;
use cleaner::category::nix::Nix;
use cleaner::category::{Category, ScanCtx, ScanOutput};
use cleaner::config::Config;
use cleaner::fsutil::measure::InodeSet;
use cleaner::protect::Protector;
use cleaner::tool::{QUERY_TIMEOUT, Runner, SystemRunner};

fn enabled() -> bool {
    std::env::var("CLEANER_IT").is_ok_and(|v| v == "1")
}

fn scan_real(cat: &dyn Category) -> ScanOutput {
    let tmp = tempfile::tempdir().unwrap();
    let config = Config::default();
    let protector = Protector::new(tmp.path(), &config.protect);
    let now = jiff::Timestamp::now().as_second();
    let ctx = ScanCtx { home: tmp.path(), now, config: &config, protector: &protector, tools: &SystemRunner, euid: rustix::process::geteuid().as_raw(), usage: None };
    cat.scan(&ctx, &mut InodeSet::default()).unwrap()
}

#[test]
fn docker_scan_real() {
    if !enabled() {
        return;
    }
    let out = scan_real(&Docker::default());
    println!("docker: {} entries, {} skipped {:?}", out.entries.len(), out.skipped.len(), out.skipped);
    assert!(!out.entries.is_empty() || !out.skipped.is_empty(), "neither entries nor a skip");
    for e in &out.entries {
        assert!(e.permanent, "docker entry not permanent: {}", e.title);
    }
}

#[test]
fn nix_scan_real() {
    if !enabled() {
        return;
    }
    // Temp home, so user profiles are empty; the config's system_profile is the real one.
    let out = scan_real(&Nix);
    let system: Vec<_> = out.entries.iter().filter(|e| e.title.starts_with("system:")).collect();
    println!(
        "nix: {} entries, system row present={}, selected={:?}",
        out.entries.len(),
        !system.is_empty(),
        system.iter().map(|e| e.selected).collect::<Vec<_>>()
    );
    for e in system {
        assert!(!e.selected, "system row must be unselected");
    }
}

#[test]
fn uv_dir_real() {
    if !enabled() {
        return;
    }
    if SystemRunner.which("uv").is_none() {
        println!("uv: not installed, skipped");
        return;
    }
    let out = SystemRunner.run("uv", &[OsStr::new("cache"), OsStr::new("dir")], QUERY_TIMEOUT).unwrap();
    assert!(out.ok(), "uv cache dir failed: {}", out.stderr);
    let dir = out.stdout.trim();
    println!("uv cache dir: {dir}");
    assert!(Path::new(dir).is_absolute(), "not absolute: {dir}");
}
