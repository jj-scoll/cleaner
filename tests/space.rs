//! The low-space decision and its state file (spec §5.1 low-space check).

use cleaner::disk::FsUsage;
use cleaner::space::{FsState, SpaceState, decide, load_state, save_error_reason, save_state};
use tempfile::TempDir;

const GIB: u64 = 1 << 30;
const NOW: i64 = 1_000_000;

fn fs(avail_gib: u64, total_gib: u64) -> FsUsage {
    FsUsage { label: "/home".into(), total: total_gib * GIB, avail: avail_gib * GIB }
}

fn state(armed: bool, notified_at: i64) -> SpaceState {
    SpaceState { fs: [("/home".to_string(), FsState { armed, notified_at })].into() }
}

#[test]
fn first_crossing_notifies() {
    let f = fs(8, 100);
    let (low, next) = decide(std::slice::from_ref(&f), &SpaceState::default(), 10, NOW);
    assert_eq!(low, [f]);
    assert_eq!(next.fs["/home"], FsState { armed: false, notified_at: NOW });
}

#[test]
fn still_low_within_6h_is_quiet() {
    let s = state(false, NOW - 3600);
    let (low, next) = decide(&[fs(8, 100)], &s, 10, NOW);
    assert!(low.is_empty());
    assert_eq!(next, s);
}

#[test]
fn still_low_after_6h_notifies_again() {
    let s = state(false, NOW - 6 * 3600 - 1);
    let (low, next) = decide(&[fs(8, 100)], &s, 10, NOW);
    assert_eq!(low.len(), 1);
    assert_eq!(next.fs["/home"], FsState { armed: false, notified_at: NOW });
}

#[test]
fn rearm_needs_15_percent() {
    let s = state(false, NOW - 60);
    let (low, s) = decide(&[fs(12, 100)], &s, 10, NOW);
    assert!(low.is_empty());
    assert!(!s.fs["/home"].armed);
    let (low, s) = decide(&[fs(16, 100)], &s, 10, NOW);
    assert!(low.is_empty());
    assert!(s.fs["/home"].armed);
    let (low, _) = decide(&[fs(8, 100)], &s, 10, NOW + 1);
    assert_eq!(low.len(), 1);
}

#[test]
fn healthy_fs_never_notifies() {
    let (low, _) = decide(&[fs(50, 100)], &SpaceState::default(), 10, NOW);
    assert!(low.is_empty());
}

#[test]
fn zero_total_ignored() {
    let f = FsUsage { label: "/home".into(), total: 0, avail: 0 };
    let (low, next) = decide(&[f], &SpaceState::default(), 10, NOW);
    assert!(low.is_empty());
    assert_eq!(next, SpaceState::default());
}

#[test]
fn state_roundtrip_and_corrupt() {
    let tmp = TempDir::new().unwrap();
    let p = tmp.path().join("cleaner-space.json");
    let s = state(false, 42);
    save_state(&p, &s).unwrap();
    assert_eq!(load_state(&p), s);
    std::fs::write(&p, "not json {").unwrap();
    assert_eq!(load_state(&p), SpaceState::default());
    assert_eq!(load_state(&tmp.path().join("missing.json")), SpaceState::default());
}

#[test]
fn save_error_reason_disk_full() {
    use anyhow::Context;
    let full = Err::<(), _>(std::io::Error::from_raw_os_error(28)).context("writing plan").unwrap_err();
    assert_eq!(save_error_reason(&full), "disk full");
    let other = anyhow::anyhow!("boom").context("writing plan");
    assert_eq!(save_error_reason(&other), "writing plan: boom");
}
