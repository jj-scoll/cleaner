mod common;

use cleaner::category::nix::{Nix, closure_estimate, current_generation, generations};
use cleaner::category::{Category, Outcome, ScanCtx, ScanOutput};
use cleaner::config::Config;
use cleaner::fsutil::measure::InodeSet;
use cleaner::object::Object;
use cleaner::plan::{Entry, EntryKind};
use cleaner::protect::Protector;
use common::{FakeRunner, err_out, mkfile, now, ok_out, set_age};
use std::fs;
use std::os::unix::fs::symlink;
use std::path::{Path, PathBuf};

const X: &str = "/nix/store/aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa-x";
const Y: &str = "/nix/store/bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb-y";
const PATH_INFO: &str = "nix --extra-experimental-features nix-command path-info --json";

/// `nix path-info --json -r X` as Nix 2.34.9 prints it: X (narSize 10,
/// closureSize 30) references Y (narSize 20).
fn fixture() -> String {
    fs::read_to_string(Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/nix/path-info.json"))
        .unwrap()
}

/// Only Y's entry, in the same shape.
fn y_only() -> String {
    let v: serde_json::Value = serde_json::from_str(&fixture()).unwrap();
    serde_json::json!({ Y: v[Y] }).to_string()
}

fn cfg(home: &Path) -> Config {
    let mut c = Config::default();
    c.nix.profiles_dir = home.join("profiles").to_str().unwrap().into();
    c.nix.system_profile = String::new();
    c.nix.roots = Some(vec![home.to_str().unwrap().into()]);
    c
}

fn nix_tools() -> FakeRunner {
    FakeRunner::new().installed("nix-env", "/fake/bin/nix-env").installed("nix", "/fake/bin/nix")
}

fn scan(home: &Path, cfg: &Config, tools: &FakeRunner) -> ScanOutput {
    let protector = Protector::new(home, &[]);
    let ctx = ScanCtx { home, now: now(), config: cfg, protector: &protector, tools, euid: common::my_uid(), usage: None };
    Nix.scan(&ctx, &mut InodeSet::default()).unwrap()
}

fn apply(entry: &Entry, home: &Path, cfg: &Config, tools: &FakeRunner) -> Outcome {
    let protector = Protector::new(home, &[]);
    let ctx = ScanCtx { home, now: now(), config: cfg, protector: &protector, tools, euid: common::my_uid(), usage: None };
    Nix.apply(entry, &ctx)
}

fn store(name: &str, n: u32) -> String {
    format!("/nix/store/{n:0>32}-{name}-{n}")
}

/// `dir/NAME-N-link -> store(NAME, N)` for each n, and `dir/NAME -> NAME-current-link`.
fn make_profile(dir: &Path, name: &str, gens: std::ops::RangeInclusive<u32>, current: u32) -> PathBuf {
    fs::create_dir_all(dir).unwrap();
    for n in gens {
        symlink(store(name, n), dir.join(format!("{name}-{n}-link"))).unwrap();
    }
    set_current(dir, name, current);
    dir.join(name)
}

fn set_current(dir: &Path, name: &str, n: u32) {
    let _ = fs::remove_file(dir.join(name));
    symlink(format!("{name}-{n}-link"), dir.join(name)).unwrap();
}

/// The generation numbers of the single `NixGenerations` entry for `profile`.
fn gens_of(out: &ScanOutput, profile: &Path) -> Vec<u32> {
    let mut found = out.entries.iter().filter_map(|e| match &e.kind {
        EntryKind::Object { obj: Object::NixGenerations { profile: p, gens, root: false } } if p == profile => {
            Some(gens.iter().map(|(n, _)| *n).collect::<Vec<_>>())
        }
        _ => None,
    });
    let gens = found.next().expect("a generations entry");
    assert!(found.next().is_none(), "one entry per profile");
    gens
}

fn gen_entry(out: &ScanOutput) -> &Entry {
    out.entries
        .iter()
        .find(|e| matches!(e.kind, EntryKind::Object { obj: Object::NixGenerations { .. } }))
        .expect("a generations entry")
}

#[test]
fn keeps_newest_n_and_current() {
    let tmp = tempfile::tempdir().unwrap();
    let h = tmp.path();
    let dir = h.join("profiles");
    let profile = make_profile(&dir, "profile", 1..=9, 9);
    let c = cfg(h);

    assert_eq!(current_generation(&profile), Some(9));
    let g = generations(&profile);
    assert_eq!(g.iter().map(|(n, _, _)| *n).collect::<Vec<_>>(), [9, 8, 7, 6, 5, 4, 3, 2, 1]);
    assert_eq!(g[0].1, dir.join("profile-9-link"));
    assert_eq!(g[0].2, store("profile", 9));

    let out = scan(h, &c, &nix_tools());
    assert_eq!(gens_of(&out, &profile), [1, 2, 3, 4]);
    let e = gen_entry(&out);
    assert_eq!(e.category, "nix");
    assert_eq!(e.title, "profile profile: generations 1–4");
    assert_eq!(e.reason, "keeping newest 5 of 9");
    assert!(e.selected && e.permanent);
    match &e.kind {
        EntryKind::Object { obj: Object::NixGenerations { gens, .. } } => {
            assert_eq!(gens[0], (1, store("profile", 1)));
        }
        other => panic!("{other:?}"),
    }

    // Rolled back to 3: it is never a candidate.
    set_current(&dir, "profile", 3);
    let out = scan(h, &c, &nix_tools());
    assert_eq!(gens_of(&out, &profile), [1, 2, 4]);
    assert_eq!(gen_entry(&out).title, "profile profile: generations 1–2, 4");
}

#[test]
fn keep_profile_overrides() {
    let tmp = tempfile::tempdir().unwrap();
    let h = tmp.path();
    let profile = make_profile(&h.join("profiles"), "profile", 1..=9, 9);
    let mut c = cfg(h);
    c.nix.keep_profile.insert("profile".into(), 8);
    assert_eq!(gens_of(&scan(h, &c, &nix_tools()), &profile), [1]);

    // keep is never below 1: the newest generation stays even if it isn't current.
    set_current(&h.join("profiles"), "profile", 3);
    c.nix.keep_profile.insert("profile".into(), 0);
    assert_eq!(gens_of(&scan(h, &c, &nix_tools()), &profile), [1, 2, 4, 5, 6, 7, 8]);
}

#[test]
fn single_generation_gives_no_row() {
    let tmp = tempfile::tempdir().unwrap();
    let h = tmp.path();
    make_profile(&h.join("profiles"), "profile", 607..=607, 607);
    let out = scan(h, &cfg(h), &nix_tools());
    assert!(out.entries.is_empty(), "{:?}", out.entries);
}

#[test]
fn orphan_profile_link_selected() {
    let tmp = tempfile::tempdir().unwrap();
    let h = tmp.path();
    let dir = h.join("profiles");
    make_profile(&dir, "profile", 1..=2, 2);
    symlink(store("home-manager", 97), dir.join("home-manager-97-link")).unwrap();

    let out = scan(h, &cfg(h), &nix_tools());
    assert_eq!(out.entries.len(), 1, "{:?}", out.entries);
    let e = &out.entries[0];
    assert!(e.selected && e.permanent);
    assert_eq!(e.reason, "generation link of a profile that no longer exists");
    assert_eq!(
        e.kind,
        EntryKind::Object {
            obj: Object::Symlink {
                path: dir.join("home-manager-97-link"),
                target: PathBuf::from(store("home-manager", 97)),
            }
        }
    );
}

/// Some path that exists in the real store (read-only lookup).
fn real_store_path() -> String {
    fs::read_dir("/nix/store")
        .expect("a Nix store")
        .flatten()
        .map(|e| e.path())
        .find(|p| !p.file_name().unwrap().as_encoded_bytes().starts_with(b"."))
        .expect("a store path")
        .to_str()
        .unwrap()
        .into()
}

#[test]
fn result_links() {
    let tmp = tempfile::tempdir().unwrap();
    let h = tmp.path();
    // A live link needs a real store target. CI's Nix job always has one.
    if !Path::new("/nix/store").is_dir() {
        eprintln!("result_links: skipped (no /nix/store on this machine)");
        return;
    }
    let live = real_store_path();
    fs::create_dir_all(h.join("proj/node_modules/dep")).unwrap();
    symlink("/nix/store/zzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzz-gone", h.join("proj/result")).unwrap();
    symlink(&live, h.join("proj/result-dev")).unwrap();
    // Not result links: wrong name, not a store target, inside a pruned artifact dir.
    symlink(X, h.join("proj/resultant")).unwrap();
    symlink("/tmp", h.join("proj/result-tmp")).unwrap();
    symlink("/nix/store/zzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzz-gone", h.join("proj/node_modules/dep/result")).unwrap();

    let tools = nix_tools().respond(&format!("{PATH_INFO} -S {live}"), ok_out(&fixture().replace(X, &live)));
    let out = scan(h, &cfg(h), &tools);
    assert_eq!(out.entries.len(), 2, "{:?}", out.entries);

    let dangling = out.entries.iter().find(|e| e.title == "proj/result").expect("dangling row");
    assert!(dangling.selected && dangling.permanent);
    assert_eq!(dangling.bytes, 0);
    assert_eq!(dangling.reason, "dangling; pins nothing");
    assert_eq!(
        dangling.kind,
        EntryKind::Object {
            obj: Object::Symlink {
                path: h.join("proj/result"),
                target: "/nix/store/zzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzz-gone".into(),
            }
        }
    );

    let live_row = out.entries.iter().find(|e| e.title == "proj/result-dev").expect("live row");
    assert!(!live_row.selected && live_row.permanent);
    assert_eq!(live_row.bytes, 30);
    assert_eq!(live_row.reason, "live GC root; kept unless you tick it");
}

/// A devbox project at `home/name` with every marker and state file `days` old.
fn devbox(home: &Path, name: &str, days: u64) -> PathBuf {
    let p = home.join(name);
    mkfile(&p.join("devbox.json"), 10);
    mkfile(&p.join("devbox.lock"), 10);
    mkfile(&p.join(".devbox/local.lock"), 10);
    mkfile(&p.join(".devbox/state.json"), 10);
    mkfile(&p.join(".devbox/virtenv/postgres/data"), 100);
    fs::create_dir_all(p.join(".devbox/nix/profile")).unwrap();
    symlink(store("profile", 1), p.join(".devbox/nix/profile/default-1-link")).unwrap();
    symlink("default-1-link", p.join(".devbox/nix/profile/default")).unwrap();
    for f in [
        "devbox.json",
        "devbox.lock",
        ".devbox/local.lock",
        ".devbox/state.json",
        ".devbox/virtenv/postgres/data",
        ".devbox/virtenv/postgres",
        ".devbox/virtenv",
        ".devbox/nix/profile",
        ".devbox/nix",
        ".devbox",
    ] {
        set_age(&p.join(f), days);
    }
    p
}

#[test]
fn devbox_stale_only() {
    let tmp = tempfile::tempdir().unwrap();
    let h = tmp.path();
    let old = devbox(h, "old", 40);
    devbox(h, "new", 2);

    let out = scan(h, &cfg(h), &nix_tools());
    assert_eq!(out.entries.len(), 1, "{:?}", out.entries);
    let e = &out.entries[0];
    assert_eq!(e.title, "devbox old");
    assert!(e.selected && e.permanent);
    assert_eq!(e.reason, "devbox project untouched for 40 days");
    let EntryKind::Item { targets } = &e.kind else { panic!("{:?}", e.kind) };
    let paths: Vec<_> = targets.iter().map(|t| t.path.clone()).collect();
    assert_eq!(
        paths,
        [old.join(".devbox/nix/profile"), old.join(".devbox/local.lock"), old.join(".devbox/state.json")]
    );
}

#[test]
fn closure_estimate_diff() {
    let tools = FakeRunner::new()
        .respond(&format!("{PATH_INFO} -r {X}"), ok_out(&fixture()))
        .respond(&format!("{PATH_INFO} -r {Y}"), ok_out(&y_only()));
    assert_eq!(closure_estimate(&tools, &[X.into()], &[Y.into()]), Some(10));
    assert_eq!(closure_estimate(&tools, &[X.into()], &[]), Some(30));
    assert_eq!(closure_estimate(&FakeRunner::new(), &[X.into()], &[Y.into()]), None);
}

#[test]
fn nix_missing_skips() {
    let tmp = tempfile::tempdir().unwrap();
    let h = tmp.path();
    make_profile(&h.join("profiles"), "profile", 1..=9, 9);
    symlink("/nix/store/zzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzz-gone", h.join("result")).unwrap();
    let tools = FakeRunner::new();

    let out = scan(h, &cfg(h), &tools);
    assert!(out.entries.is_empty(), "{:?}", out.entries);
    assert_eq!(out.skipped.len(), 1, "{:?}", out.skipped);
    assert_eq!(out.skipped[0].reason, "nix is not installed");
    assert_eq!(out.skipped[0].category, "nix");
    assert!(tools.calls().is_empty(), "{:?}", tools.calls());
}

/// Scans a 1–9 profile (current 9, keep 5) and returns (home, profiles dir, profile, entry).
fn planned() -> (tempfile::TempDir, PathBuf, PathBuf, Entry) {
    let tmp = tempfile::tempdir().unwrap();
    let h = tmp.path().to_path_buf();
    let dir = h.join("profiles");
    let profile = make_profile(&dir, "profile", 1..=9, 9);
    let out = scan(&h, &cfg(&h), &nix_tools());
    let e = gen_entry(&out).clone();
    (tmp, dir, profile, e)
}

fn delete_call(profile: &Path, gens: &str) -> String {
    format!("nix-env -p {} --delete-generations {gens}", profile.display())
}

#[test]
fn apply_deletes_exact_numbers() {
    let (tmp, _dir, profile, e) = planned();
    let tools = nix_tools().respond("nix-env -p", ok_out(""));
    let outcome = apply(&e, tmp.path(), &cfg(tmp.path()), &tools);
    assert_eq!(outcome, Outcome::Deleted { bytes: 0, failures: vec![] });
    assert_eq!(tools.calls(), [delete_call(&profile, "1 2 3 4")]);
}

#[test]
fn new_generation_not_deleted() {
    let (tmp, dir, profile, e) = planned();
    symlink(store("profile", 10), dir.join("profile-10-link")).unwrap();
    set_current(&dir, "profile", 10);
    let tools = nix_tools().respond("nix-env -p", ok_out(""));
    apply(&e, tmp.path(), &cfg(tmp.path()), &tools);
    assert_eq!(tools.calls(), [delete_call(&profile, "1 2 3 4")]);
}

#[test]
fn drifted_generation_dropped() {
    let (tmp, dir, profile, e) = planned();
    fs::remove_file(dir.join("profile-2-link")).unwrap();
    symlink(store("other", 2), dir.join("profile-2-link")).unwrap();
    let tools = nix_tools().respond("nix-env -p", ok_out(""));
    let outcome = apply(&e, tmp.path(), &cfg(tmp.path()), &tools);
    assert_eq!(tools.calls(), [delete_call(&profile, "1 3 4")]);
    let Outcome::Deleted { failures, .. } = outcome else { panic!("{outcome:?}") };
    assert_eq!(failures, ["generation 2 changed since plan"]);
}

#[test]
fn now_current_generation_dropped() {
    let (tmp, dir, profile, e) = planned();
    set_current(&dir, "profile", 4);
    let tools = nix_tools().respond("nix-env -p", ok_out(""));
    let outcome = apply(&e, tmp.path(), &cfg(tmp.path()), &tools);
    assert_eq!(tools.calls(), [delete_call(&profile, "1 2 3")]);
    let Outcome::Deleted { failures, .. } = outcome else { panic!("{outcome:?}") };
    assert_eq!(failures, ["generation 4 changed since plan"]);
}

#[test]
fn all_drifted_is_skipped_without_call() {
    let (tmp, dir, _profile, e) = planned();
    for n in 1..=4 {
        fs::remove_file(dir.join(format!("profile-{n}-link"))).unwrap();
    }
    let tools = nix_tools().respond("nix-env -p", ok_out(""));
    let outcome = apply(&e, tmp.path(), &cfg(tmp.path()), &tools);
    assert_eq!(outcome, Outcome::Skipped { reason: "nothing left to delete".into() });
    assert!(tools.calls().is_empty(), "{:?}", tools.calls());
}

#[test]
fn nix_env_refusal_is_failed() {
    let (tmp, _dir, _profile, e) = planned();
    let tools = nix_tools().respond("nix-env -p", err_out(1, "error: cannot delete\nmore"));
    let outcome = apply(&e, tmp.path(), &cfg(tmp.path()), &tools);
    assert_eq!(outcome, Outcome::Failed { error: "nix-env refused: error: cannot delete".into() });
}

fn symlink_entry(path: &Path, target: &Path) -> Entry {
    Entry {
        id: "nix:0".into(),
        category: "nix".into(),
        title: "result".into(),
        kind: EntryKind::Object { obj: Object::Symlink { path: path.into(), target: target.into() } },
        bytes: 0,
        reason: String::new(),
        selected: true,
        permanent: true,
        note: None,
    }
}

#[test]
fn symlink_apply_unlinks_without_following() {
    let tmp = tempfile::tempdir().unwrap();
    let h = tmp.path();
    let target = h.join("elsewhere/file");
    mkfile(&target, 10);
    fs::create_dir_all(h.join("proj")).unwrap();
    let link = h.join("proj/result");
    symlink(&target, &link).unwrap();

    let outcome = apply(&symlink_entry(&link, &target), h, &cfg(h), &FakeRunner::new());
    assert_eq!(outcome, Outcome::Deleted { bytes: 0, failures: vec![] });
    assert!(link.symlink_metadata().is_err(), "link still there");
    assert!(target.is_file(), "target was followed");
}

#[test]
fn symlink_apply_skips_when_repointed() {
    let tmp = tempfile::tempdir().unwrap();
    let h = tmp.path();
    fs::create_dir_all(h.join("proj")).unwrap();
    let link = h.join("proj/result");
    symlink("/nix/store/zzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzz-new", &link).unwrap();

    let planned_target = Path::new("/nix/store/zzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzz-old");
    let outcome = apply(&symlink_entry(&link, planned_target), h, &cfg(h), &FakeRunner::new());
    assert_eq!(outcome, Outcome::Skipped { reason: "changed since plan".into() });
    assert!(link.symlink_metadata().is_ok());
}

// ---- system generations (pkexec) and store GC -------------------------------

const PKEXEC_NIX_ENV: &str = "/fake/bin/nix-env";

/// Scans a system profile 1424–1434 (current 1434, keep 5) under a temp dir.
fn planned_system() -> (tempfile::TempDir, PathBuf, Config, Entry) {
    let tmp = tempfile::tempdir().unwrap();
    let h = tmp.path().to_path_buf();
    let sys = make_profile(&h.join("sysprofiles"), "system", 1424..=1434, 1434);
    let mut c = cfg(&h);
    c.nix.system_profile = sys.to_str().unwrap().into();
    let out = scan(&h, &c, &nix_tools());
    let e = out
        .entries
        .iter()
        .find(|e| matches!(e.kind, EntryKind::Object { obj: Object::NixGenerations { root: true, .. } }))
        .expect("a system generations entry")
        .clone();
    (tmp, sys, c, e)
}

#[test]
fn system_gens_unselected_with_pkexec_reason() {
    let (_tmp, sys, _c, e) = planned_system();
    match &e.kind {
        EntryKind::Object { obj: Object::NixGenerations { profile, gens, root: true } } => {
            assert_eq!(profile, &sys);
            assert_eq!(gens.iter().map(|(n, _)| *n).collect::<Vec<_>>(), (1424..=1429).collect::<Vec<_>>());
        }
        other => panic!("{other:?}"),
    }
    assert!(!e.selected);
    assert!(e.permanent);
    assert_eq!(e.title, "system: generations 1424–1429");
    assert_eq!(e.reason, "keeping newest 5 of 11; needs your password (pkexec)");
    assert!(e.reason.contains("pkexec"));
}

#[test]
fn system_keep_falls_back_and_overrides() {
    let tmp = tempfile::tempdir().unwrap();
    let h = tmp.path();
    let sys = make_profile(&h.join("sysprofiles"), "system", 1..=9, 9);
    let mut c = cfg(h);
    c.nix.system_profile = sys.to_str().unwrap().into();
    c.nix.keep_profile.insert("system".into(), 7);
    let out = scan(h, &c, &nix_tools());
    let e = out.entries.iter().find(|e| e.title.starts_with("system:")).unwrap();
    assert_eq!(e.title, "system: generations 1–2");
    assert_eq!(e.reason, "keeping newest 7 of 9; needs your password (pkexec)");
}

fn pkexec_call(sys: &Path, gens: &str) -> String {
    format!("pkexec {PKEXEC_NIX_ENV} -p {} --delete-generations {gens}", sys.display())
}

#[test]
fn pkexec_command_exact() {
    let (tmp, sys, c, e) = planned_system();
    let tools = nix_tools().installed("pkexec", "/fake/bin/pkexec").respond("pkexec", ok_out(""));
    let outcome = apply(&e, tmp.path(), &c, &tools);
    assert_eq!(outcome, Outcome::Deleted { bytes: 0, failures: vec![] });
    assert_eq!(tools.calls(), [pkexec_call(&sys, "1424 1425 1426 1427 1428 1429")]);
}

#[test]
fn pkexec_rechecks_drift_like_user_profiles() {
    let (tmp, sys, c, e) = planned_system();
    let dir = sys.parent().unwrap();
    fs::remove_file(dir.join("system-1425-link")).unwrap();
    let tools = nix_tools().installed("pkexec", "/fake/bin/pkexec").respond("pkexec", ok_out(""));
    let outcome = apply(&e, tmp.path(), &c, &tools);
    assert_eq!(tools.calls(), [pkexec_call(&sys, "1424 1426 1427 1428 1429")]);
    let Outcome::Deleted { failures, .. } = outcome else { panic!("{outcome:?}") };
    assert_eq!(failures, ["generation 1425 changed since plan"]);
}

#[test]
fn pkexec_cancel_is_skipped() {
    for code in [126, 127] {
        let (tmp, _sys, c, e) = planned_system();
        let tools = nix_tools().installed("pkexec", "/fake/bin/pkexec").respond("pkexec", err_out(code, ""));
        assert_eq!(
            apply(&e, tmp.path(), &c, &tools),
            Outcome::Skipped { reason: "pkexec cancelled or not authorized".into() },
            "exit {code}"
        );
    }
}

#[test]
fn pkexec_other_failure_is_failed() {
    let (tmp, _sys, c, e) = planned_system();
    let tools = nix_tools().installed("pkexec", "/fake/bin/pkexec").respond("pkexec", err_out(1, "error: no\nmore"));
    assert_eq!(
        apply(&e, tmp.path(), &c, &tools),
        Outcome::Failed { error: "nix-env refused: error: no".into() }
    );
}

#[test]
fn pkexec_missing_is_skipped() {
    let (tmp, _sys, c, e) = planned_system();
    let tools = nix_tools();
    assert_eq!(
        apply(&e, tmp.path(), &c, &tools),
        Outcome::Skipped { reason: "pkexec is not installed".into() }
    );
    assert!(tools.calls().is_empty(), "{:?}", tools.calls());
}

const DEAD_A: &str = "/nix/store/dddddddddddddddddddddddddddddddd-a";
const DEAD_B: &str = "/nix/store/eeeeeeeeeeeeeeeeeeeeeeeeeeeeeeee-b";

fn gc_tools() -> FakeRunner {
    let info = serde_json::json!({ DEAD_A: { "narSize": 100 }, DEAD_B: { "narSize": 250 } }).to_string();
    nix_tools()
        .installed("nix-store", "/fake/bin/nix-store")
        .respond("nix-store --gc --print-dead", ok_out(&format!("{DEAD_A}\n{DEAD_B}\n")))
        .respond(&format!("{PATH_INFO} {DEAD_A}"), ok_out(&info))
}

fn gc_entry(out: &ScanOutput) -> &Entry {
    out.entries.iter().find(|e| matches!(e.kind, EntryKind::Operation { .. })).expect("a gc entry")
}

#[test]
fn gc_is_last_and_parses_freed() {
    use cleaner::object::Operation;
    let tmp = tempfile::tempdir().unwrap();
    let h = tmp.path();
    make_profile(&h.join("profiles"), "profile", 1..=9, 9);
    let c = cfg(h);
    let out = scan(h, &c, &gc_tools());
    let last = out.entries.last().unwrap();
    assert_eq!(last.kind, EntryKind::Operation { op: Operation::NixStoreGc });
    assert_eq!(last.title, "Nix store garbage collection");
    assert_eq!(last.reason, "frees store paths nothing references (after the Nix rows above)");
    assert!(last.selected && last.permanent);
    assert_eq!(last.bytes, 350);
    assert!(out.entries.len() > 1);

    let tools = nix_tools().respond(
        "nix-store --gc",
        ok_out("deleting garbage...\n12 store paths deleted, 812.40 MiB freed\n"),
    );
    assert_eq!(
        apply(last, h, &c, &tools),
        Outcome::Deleted { bytes: 851_863_142, failures: vec![] }
    );
    assert_eq!(tools.calls(), ["nix-store --gc"]);
}

#[test]
fn gc_off_or_unsized_degrades() {
    let tmp = tempfile::tempdir().unwrap();
    let h = tmp.path();
    let mut c = cfg(h);
    c.nix.gc = false;
    assert!(scan(h, &c, &gc_tools()).entries.is_empty());

    // print-dead fails: the row stays, sized 0, and the scan does not fail.
    c.nix.gc = true;
    let tools = nix_tools().installed("nix-store", "/fake/bin/nix-store").respond("nix-store", err_out(1, "boom"));
    let out = scan(h, &c, &tools);
    assert_eq!(gc_entry(&out).bytes, 0);

    // No nix-store: no row.
    assert!(scan(h, &c, &nix_tools()).entries.is_empty());
}

#[test]
fn gc_path_info_chunks_of_500() {
    let tmp = tempfile::tempdir().unwrap();
    let h = tmp.path();
    let dead: String = (0..501).map(|i| format!("/nix/store/{i:0>32}-d\n")).collect();
    let tools = nix_tools()
        .installed("nix-store", "/fake/bin/nix-store")
        .respond("nix-store --gc --print-dead", ok_out(&dead))
        .respond(PATH_INFO, ok_out("{}"));
    scan(h, &cfg(h), &tools);
    let infos: Vec<_> = tools.calls().into_iter().filter(|c| c.contains("path-info") && !c.contains(" -r ")).collect();
    assert_eq!(infos.len(), 2, "{infos:?}");
    assert_eq!(infos[0].split_whitespace().count(), 5 + 500);
    assert_eq!(infos[1].split_whitespace().count(), 5 + 1);
}

#[test]
fn gc_apply_failure_is_failed() {
    let tmp = tempfile::tempdir().unwrap();
    let h = tmp.path();
    let c = cfg(h);
    let out = scan(h, &c, &gc_tools());
    let tools = nix_tools().respond("nix-store --gc", err_out(1, "error: locked\nx"));
    assert_eq!(
        apply(gc_entry(&out), h, &c, &tools),
        Outcome::Failed { error: "nix-store refused: error: locked".into() }
    );
}

#[test]
fn tracked_result_link_is_skipped() {
    let tmp = tempfile::tempdir().unwrap();
    let h = tmp.path();
    let proj = h.join("proj");
    common::git_init(&proj);
    symlink("/nix/store/zzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzz-gone", proj.join("result")).unwrap();
    common::git_commit_all(&proj);

    let out = scan(h, &cfg(h), &nix_tools());
    assert!(out.entries.iter().all(|e| e.title != "proj/result"), "{:?}", out.entries);
    let s = out.skipped.iter().find(|s| s.title == "proj/result").expect("a skip");
    assert_eq!(s.reason, "tracked in git");
}

#[test]
fn symlink_apply_skips_tracked() {
    let tmp = tempfile::tempdir().unwrap();
    let h = tmp.path();
    let proj = h.join("proj");
    common::git_init(&proj);
    let target = Path::new("/nix/store/zzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzz-gone");
    let link = proj.join("result");
    symlink(target, &link).unwrap();
    common::git_commit_all(&proj); // committed after the plan

    let outcome = apply(&symlink_entry(&link, target), h, &cfg(h), &FakeRunner::new());
    assert_eq!(outcome, Outcome::Skipped { reason: "tracked in git".into() });
    assert!(link.symlink_metadata().is_ok());
}

#[test]
fn pkexec_runs_interactively_user_nix_env_does_not() {
    let (tmp, sys, c, e) = planned_system();
    let tools = nix_tools().installed("pkexec", "/fake/bin/pkexec").respond("pkexec", ok_out(""));
    assert_eq!(apply(&e, tmp.path(), &c, &tools), Outcome::Deleted { bytes: 0, failures: vec![] });
    assert_eq!(tools.interactive_calls(), [pkexec_call(&sys, "1424 1425 1426 1427 1428 1429")]);

    let (utmp, _dir, _profile, user) = planned();
    let tools = nix_tools().respond("nix-env", ok_out(""));
    apply(&user, utmp.path(), &cfg(utmp.path()), &tools);
    assert_eq!(tools.calls().len(), 1, "{:?}", tools.calls());
    assert!(tools.interactive_calls().is_empty(), "{:?}", tools.interactive_calls());
}

#[test]
fn pkexec_timeout_is_skipped() {
    let (tmp, _sys, c, e) = planned_system();
    let tools = nix_tools().installed("pkexec", "/fake/bin/pkexec").timeout("pkexec", 300);
    assert_eq!(
        apply(&e, tmp.path(), &c, &tools),
        Outcome::Skipped { reason: "pkexec timed out waiting for authentication".into() }
    );
}
