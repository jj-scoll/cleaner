//! The opt-in holding area: apply moves files and folders aside (same
//! filesystem, rename only), `hold::restore` puts them back, `hold::purge`
//! deletes them with the safe deleter. Cross-filesystem cases re-run inside a
//! user + mount namespace (`unshare -rm`), as in tests/mounts.rs.

mod common;

use cleaner::category::Outcome;
use cleaner::config::{Config, HoldCfg, StoreCfg};
use cleaner::engine::{Ctx, IndexMode, apply_plan, build_plan, plan_and_save_with, registry};
use cleaner::hold::{self, Area, Fate, HoldTo, Pick, Preview, Session};
use cleaner::object::{Object, Operation};
use cleaner::paths::Paths;
use cleaner::plan::{Entry, EntryKind, Plan, Target, fingerprint};
use cleaner::protect::Protector;
use cleaner::view::review::ReviewModel;
use common::{mkfile, now, set_age};
use std::collections::BTreeMap;
use std::ffi::CString;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::{MetadataExt, symlink};
use std::path::{Path, PathBuf};
use std::process::Command;
use tempfile::TempDir;

const DAY: i64 = 86_400;

/// A context for `home` with only file-based categories on (no docker, nix,
/// uv or go), holding as asked.
fn ctx_for(home: &Path, hold: bool) -> Ctx {
    let mut config = Config::default();
    config.docker.enabled = false;
    config.nix.enabled = false;
    config.caches.uv_prune = false;
    config.caches.go_clean = false;
    config.hold.enabled = hold;
    Ctx {
        paths: Paths::for_home(home),
        config,
        protector: Protector::new(home, &[]),
        now: now(),
        tools: std::sync::Arc::new(common::FakeRunner::new()),
        usage: None,
    }
}

/// Home with one stale Rust project (`proj/target`, 50 kB) and an old Download.
fn fixture(hold: bool) -> (TempDir, Ctx) {
    let tmp = TempDir::new().unwrap();
    let home = tmp.path();
    for (f, n) in [
        ("proj/Cargo.toml", 10),
        ("proj/src/main.rs", 10),
        ("proj/target/debug/app", 50_000),
    ] {
        mkfile(&home.join(f), n);
        set_age(&home.join(f), 40);
    }
    mkfile(&home.join("Downloads/old.zip"), 20_000);
    set_age(&home.join("Downloads/old.zip"), 100);
    let ctx = ctx_for(home, hold);
    (tmp, ctx)
}

fn target(home: &Path) -> PathBuf {
    home.join("proj/target")
}

fn id_of(plan: &Plan, category: &str) -> String {
    plan.entries
        .iter()
        .find(|e| e.category == category)
        .unwrap_or_else(|| panic!("no {category} entry: {plan:#?}"))
        .id
        .clone()
}

fn outcome<'a>(report: &'a cleaner::engine::RunReport, id: &str) -> &'a Outcome {
    &report.results.iter().find(|(i, _)| i == id).unwrap().1
}

/// Plans and applies the artifacts entry (`proj/target`); returns its outcome.
fn hold_target(ctx: &Ctx) -> Outcome {
    let plan = build_plan(ctx, &registry(&ctx.config)).unwrap();
    let id = id_of(&plan, "artifacts");
    let report = apply_plan(
        ctx,
        &registry(&ctx.config),
        &plan,
        std::slice::from_ref(&id),
    )
    .unwrap();
    outcome(&report, &id).clone()
}

/// A hold session for `area` stamped `now`.
fn to<'a>(area: &'a Area, now: i64) -> HoldTo<'a> {
    HoldTo {
        area,
        plan_id: "20260101T000000Z",
        entry_id: "test:0",
        category: "test",
        title: "test item",
        now,
    }
}

/// Holds `path` in its own entry stamped `now`; returns the entry id.
fn hold_one(area: &Area, path: &Path, now: i64) -> String {
    let mut s = Session::new(to(area, now));
    let r = s.hold(path, None);
    assert!(r.is_ok(), "holding {}: {r:?}", path.display());
    s.finish().expect("an entry").0
}

fn held(paths: &Paths) -> Vec<hold::Held> {
    hold::list(&paths.hold_dir).unwrap()
}

// ---- default off ----

#[test]
fn hold_is_off_by_default_and_apply_still_deletes() {
    assert_eq!(
        Config::default().hold,
        HoldCfg {
            enabled: false,
            days: 7
        }
    );
    let (tmp, ctx) = fixture(false);
    let o = hold_target(&ctx);
    assert!(matches!(o, Outcome::Deleted { .. }), "{o:?}");
    assert!(!target(tmp.path()).exists());
    assert!(
        !ctx.paths.hold_dir.exists(),
        "holding area created while off"
    );
}

// ---- holding through apply ----

#[test]
fn apply_moves_item_into_hold_with_metadata() {
    let (tmp, ctx) = fixture(true);
    let home = tmp.path();
    let plan = build_plan(&ctx, &registry(&ctx.config)).unwrap();
    let id = id_of(&plan, "artifacts");
    let report = apply_plan(
        &ctx,
        &registry(&ctx.config),
        &plan,
        std::slice::from_ref(&id),
    )
    .unwrap();
    let Outcome::Held {
        id: hold_id, bytes, ..
    } = outcome(&report, &id).clone()
    else {
        panic!("not held: {:?}", report.results);
    };
    assert!(bytes >= 50_000, "held bytes {bytes}");
    assert!(!target(home).exists(), "target still in place");

    let list = held(&ctx.paths);
    assert_eq!(list.len(), 1, "{list:#?}");
    let h = &list[0];
    assert_eq!(h.id, hold_id);
    assert_eq!(h.plan_id, plan.id);
    assert_eq!(h.category, "artifacts");
    assert!((h.held_at - ctx.now).abs() < 5, "held_at {}", h.held_at);
    assert_eq!(h.items.len(), 1);
    assert_eq!(h.items[0].original, target(home));
    assert_eq!(h.bytes(), bytes);
    let moved = ctx.paths.hold_dir.join(&h.id).join(&h.items[0].name);
    assert_eq!(
        std::fs::metadata(moved.join("debug/app")).unwrap().len(),
        50_000
    );
    // Same filesystem, so it was a rename: the inode survived.
    assert!(std::fs::metadata(&ctx.paths.hold_dir).unwrap().dev() == h_dev(home));
}

fn h_dev(p: &Path) -> u64 {
    std::fs::metadata(p).unwrap().dev()
}

#[test]
fn held_log_line_and_report_free_nothing() {
    let (_tmp, ctx) = fixture(true);
    let o = hold_target(&ctx);
    let Outcome::Held { id, .. } = o else {
        panic!("not held: {o:?}")
    };
    let lines = cleaner::log::read(&ctx.paths.log_file, None).unwrap();
    assert_eq!(lines.len(), 1, "{lines:#?}");
    let l = &lines[0];
    assert_eq!(l.outcome, "held");
    assert_eq!(l.bytes, 0, "a held item frees nothing");
    assert!(l.detail.contains(&id), "{}", l.detail);
    assert!(
        l.detail.contains("frees nothing until purged"),
        "{}",
        l.detail
    );
}

#[test]
fn store_rule_matches_are_held_too() {
    let (tmp, ctx) = fixture(true);
    let plan = build_plan(&ctx, &registry(&ctx.config)).unwrap();
    let id = id_of(&plan, "user_rules");
    let report = apply_plan(
        &ctx,
        &registry(&ctx.config),
        &plan,
        std::slice::from_ref(&id),
    )
    .unwrap();
    assert!(
        matches!(outcome(&report, &id), Outcome::Held { .. }),
        "{:?}",
        report.results
    );
    let dl = tmp.path().join("Downloads/old.zip");
    assert!(!dl.exists());
    let list = held(&ctx.paths);
    assert_eq!(list[0].items[0].original, dl);
    hold::restore(&ctx.paths.hold_dir, Pick::Path(&dl)).unwrap();
    assert_eq!(std::fs::metadata(&dl).unwrap().len(), 20_000);
}

#[test]
fn objects_and_operations_are_never_held() {
    let (tmp, mut ctx) = fixture(true);
    ctx.config.nix.enabled = true;
    let link = tmp.path().join("proj/result");
    symlink("/nix/store/aaaa-fake", &link).unwrap();
    let entry = Entry {
        id: "nix:0".into(),
        category: "nix".into(),
        title: "proj/result".into(),
        kind: EntryKind::Object {
            obj: Object::Symlink {
                path: link.clone(),
                target: "/nix/store/aaaa-fake".into(),
            },
        },
        bytes: 0,
        reason: "result link".into(),
        selected: true,
        permanent: false,
        note: None,
        last_active: None,
    };
    let plan = Plan {
        id: "p".into(),
        created: 0,
        entries: vec![entry.clone()],
        skipped: vec![],
        report_only: vec![],
    };
    let report = apply_plan(&ctx, &registry(&ctx.config), &plan, &["nix:0".into()]).unwrap();
    assert!(
        matches!(outcome(&report, "nix:0"), Outcome::Deleted { .. }),
        "{:?}",
        report.results
    );
    assert!(link.symlink_metadata().is_err(), "link not removed");
    assert!(held(&ctx.paths).is_empty());

    let dev = Ok(h_dev(tmp.path()));
    assert_eq!(hold::fate(&entry, &dev), Fate::Permanent);
    let op = Entry {
        kind: EntryKind::Operation {
            op: Operation::UvCachePrune,
        },
        ..entry
    };
    assert_eq!(hold::fate(&op, &dev), Fate::Permanent);
}

// ---- restore ----

#[test]
fn restore_by_id_puts_it_back() {
    let (tmp, ctx) = fixture(true);
    let Outcome::Held { id, .. } = hold_target(&ctx) else {
        panic!()
    };
    let (h, restored) = hold::restore(&ctx.paths.hold_dir, Pick::Id(&id)).unwrap();
    assert_eq!(h.id, id);
    assert_eq!(restored, vec![target(tmp.path())]);
    assert_eq!(
        std::fs::metadata(target(tmp.path()).join("debug/app"))
            .unwrap()
            .len(),
        50_000
    );
    assert!(held(&ctx.paths).is_empty());
    assert!(!ctx.paths.hold_dir.join(&id).exists(), "empty entry left");
}

#[test]
fn restore_by_original_path() {
    let (tmp, ctx) = fixture(true);
    hold_target(&ctx);
    let t = target(tmp.path());
    hold::restore(&ctx.paths.hold_dir, Pick::Path(&t)).unwrap();
    assert!(t.join("debug/app").exists());
}

#[test]
fn restore_refuses_to_overwrite() {
    let (tmp, ctx) = fixture(true);
    let Outcome::Held { id, .. } = hold_target(&ctx) else {
        panic!()
    };
    let t = target(tmp.path());
    // A new folder in its place.
    mkfile(&t.join("new"), 7);
    let err = hold::restore(&ctx.paths.hold_dir, Pick::Id(&id)).unwrap_err();
    assert!(format!("{err:#}").contains("exists"), "{err:#}");
    assert!(t.join("new").exists());
    assert!(!t.join("debug").exists(), "restored over the new folder");
    assert_eq!(held(&ctx.paths).len(), 1);
    // A dangling symlink in its place is refused just the same.
    std::fs::remove_dir_all(&t).unwrap();
    symlink(tmp.path().join("nowhere"), &t).unwrap();
    assert!(hold::restore(&ctx.paths.hold_dir, Pick::Id(&id)).is_err());
    assert!(t.symlink_metadata().unwrap().file_type().is_symlink());
    assert!(!tmp.path().join("nowhere").exists(), "followed the symlink");
    assert_eq!(held(&ctx.paths).len(), 1);
}

#[test]
fn restore_refuses_a_symlinked_parent() {
    let tmp = TempDir::new().unwrap();
    let home = tmp.path();
    let paths = Paths::for_home(home);
    let area = Area::open(&paths.hold_dir).unwrap();
    mkfile(&home.join("data/item/f"), 10);
    let id = hold_one(&area, &home.join("data/item"), now());
    // `data` is now a symlink to another folder.
    std::fs::rename(home.join("data"), home.join("real")).unwrap();
    symlink(home.join("real"), home.join("data")).unwrap();
    let err = hold::restore(&paths.hold_dir, Pick::Id(&id)).unwrap_err();
    assert!(!home.join("real/item").exists(), "followed {err:#}");
    assert_eq!(held(&paths).len(), 1);
}

#[test]
fn restore_refuses_a_parent_redirected_higher_up() {
    let tmp = TempDir::new().unwrap();
    let home = tmp.path();
    let paths = Paths::for_home(home);
    let area = Area::open(&paths.hold_dir).unwrap();
    mkfile(&home.join("a/b/item/f"), 10);
    let id = hold_one(&area, &home.join("a/b/item"), now());
    // `a` is now a symlink to a different tree that also has a `b`.
    std::fs::rename(home.join("a"), home.join("old")).unwrap();
    std::fs::create_dir_all(home.join("other/b")).unwrap();
    symlink(home.join("other"), home.join("a")).unwrap();
    let err = hold::restore(&paths.hold_dir, Pick::Id(&id)).unwrap_err();
    assert!(format!("{err:#}").contains("replaced"), "{err:#}");
    assert!(!home.join("other/b/item").exists(), "restored through it");
    assert_eq!(held(&paths).len(), 1);
    // Back as it was (the same folder, by inode), restore goes ahead.
    std::fs::remove_file(home.join("a")).unwrap();
    std::fs::rename(home.join("old"), home.join("a")).unwrap();
    hold::restore(&paths.hold_dir, Pick::Id(&id)).unwrap();
    assert!(home.join("a/b/item/f").exists());
}

#[test]
fn a_crash_after_reserve_restores_only_what_moved() {
    let tmp = TempDir::new().unwrap();
    let home = tmp.path();
    let paths = Paths::for_home(home);
    let area = Area::open(&paths.hold_dir).unwrap();
    mkfile(&home.join("a/f"), 10);
    mkfile(&home.join("b/f"), 10);
    let (a, b) = (home.join("a"), home.join("b"));
    let mut s = Session::new(to(&area, now()));
    s.reserve(&[a.as_path(), b.as_path()]).unwrap();
    s.hold(&a, None).unwrap();
    // The process dies here: no `finish`, so meta.json is still the reserved one.
    std::mem::forget(s);
    let list = held(&paths);
    assert_eq!(list.len(), 1);
    assert_eq!(list[0].items.len(), 2, "write-ahead lists both");
    // `b` never moved and is still in place; restoring the entry puts `a` back
    // and leaves `b` alone instead of refusing because `b` exists.
    let (_, restored) = hold::restore(&paths.hold_dir, Pick::Id(&list[0].id)).unwrap();
    assert_eq!(restored, vec![a.clone()]);
    assert!(a.join("f").exists() && b.join("f").exists());
    assert!(held(&paths).is_empty());
}

#[test]
fn a_store_rule_with_many_matches_writes_metadata_once_per_entry() {
    // 2000 matches: per-match metadata rewrites would be ~2000 x 200 kB.
    let (tmp, mut ctx) = fixture(true);
    let home = tmp.path();
    for i in 0..2000 {
        let f = home.join(format!("bulk/{i:04}"));
        mkfile(&f, 1);
        set_age(&f, 100);
    }
    ctx.config.user_rules.push(StoreCfg {
        path: "~/bulk".into(),
        max_age_days: 30,
        unit: "files".into(),
        glob: None,
        min_bytes: None,
    });
    let plan = build_plan(&ctx, &registry(&ctx.config)).unwrap();
    let e = plan
        .entries
        .iter()
        .find(|e| e.title.contains("bulk"))
        .unwrap_or_else(|| panic!("{plan:#?}"));
    let t = std::time::Instant::now();
    let report = apply_plan(
        &ctx,
        &registry(&ctx.config),
        &plan,
        std::slice::from_ref(&e.id),
    )
    .unwrap();
    assert!(
        matches!(outcome(&report, &e.id), Outcome::Held { .. }),
        "{:?}",
        report.results
    );
    assert_eq!(held(&ctx.paths)[0].items.len(), 2000);
    assert!(t.elapsed().as_secs() < 30, "took {:?}", t.elapsed());
}

// ---- never follow symlinks ----

#[test]
fn hold_refuses_a_symlink_and_a_symlinked_parent() {
    let tmp = TempDir::new().unwrap();
    let home = tmp.path();
    let paths = Paths::for_home(home);
    let area = Area::open(&paths.hold_dir).unwrap();
    mkfile(&home.join("secret/f"), 10);
    symlink(home.join("secret"), home.join("link")).unwrap();

    let mut s = Session::new(to(&area, now()));
    assert!(s.hold(&home.join("link"), None).is_err());
    assert!(s.hold(&home.join("link/f"), None).is_err());
    assert!(s.finish().is_none(), "an entry with nothing in it");
    assert!(home.join("link").symlink_metadata().is_ok());
    assert!(home.join("secret/f").exists());
    assert!(held(&paths).is_empty());
}

#[test]
fn symlinks_inside_a_held_tree_are_moved_never_followed() {
    let tmp = TempDir::new().unwrap();
    let home = tmp.path();
    let paths = Paths::for_home(home);
    let area = Area::open(&paths.hold_dir).unwrap();
    mkfile(&home.join("outside/precious"), 10);
    mkfile(&home.join("d/f"), 10);
    symlink(home.join("outside"), home.join("d/s")).unwrap();

    let id = hold_one(&area, &home.join("d"), now());
    assert!(home.join("outside/precious").exists());
    hold::restore(&paths.hold_dir, Pick::Id(&id)).unwrap();
    assert_eq!(
        std::fs::read_link(home.join("d/s")).unwrap(),
        home.join("outside")
    );

    hold_one(&area, &home.join("d"), now());
    let purged = hold::purge(&paths.hold_dir, None).unwrap();
    assert_eq!(purged.len(), 1);
    assert!(purged[0].result.is_ok(), "{:?}", purged[0].result);
    assert!(
        home.join("outside/precious").exists(),
        "purge followed a link"
    );
}

#[test]
fn apply_refuses_a_target_turned_symlink() {
    let (tmp, ctx) = fixture(true);
    let home = tmp.path();
    let plan = build_plan(&ctx, &registry(&ctx.config)).unwrap();
    let id = id_of(&plan, "artifacts");
    mkfile(&home.join("elsewhere/keep"), 10);
    std::fs::remove_dir_all(target(home)).unwrap();
    symlink(home.join("elsewhere"), target(home)).unwrap();
    let report = apply_plan(
        &ctx,
        &registry(&ctx.config),
        &plan,
        std::slice::from_ref(&id),
    )
    .unwrap();
    assert!(
        matches!(outcome(&report, &id), Outcome::Skipped { .. }),
        "{:?}",
        report.results
    );
    assert!(home.join("elsewhere/keep").exists());
    assert!(held(&ctx.paths).is_empty());
}

// ---- protect list ----

#[test]
fn protected_items_are_not_held() {
    let (tmp, mut ctx) = fixture(true);
    let plan = build_plan(&ctx, &registry(&ctx.config)).unwrap();
    let id = id_of(&plan, "artifacts");
    ctx.protector = Protector::new(tmp.path(), &["~/proj".into()]);
    let report = apply_plan(
        &ctx,
        &registry(&ctx.config),
        &plan,
        std::slice::from_ref(&id),
    )
    .unwrap();
    assert_eq!(
        outcome(&report, &id),
        &Outcome::Skipped {
            reason: "protected".into()
        }
    );
    assert!(target(tmp.path()).join("debug/app").exists());
    assert!(held(&ctx.paths).is_empty());
}

// ---- scans never plan the holding area ----

#[test]
fn plans_never_offer_the_holding_area() {
    let (tmp, mut ctx) = fixture(true);
    let home = tmp.path().to_path_buf();
    assert!(matches!(hold_target(&ctx), Outcome::Held { .. }));
    // Rules that would match the holding area and everything in it.
    for (path, unit) in [
        ("~/.local/share/cleaner/hold", "depth:1"),
        ("~/.local/share/cleaner", "depth:1"),
        ("~/.local/share", "depth:2"),
    ] {
        ctx.config.user_rules.push(StoreCfg {
            path: path.into(),
            max_age_days: 1,
            unit: unit.into(),
            glob: None,
            min_bytes: None,
        });
    }
    let walk = |p: &Path| {
        for e in walkdir(p) {
            set_age(&e, 40);
        }
    };
    walk(&ctx.paths.hold_dir);
    let (plan, _) = plan_and_save_with(
        &ctx.paths,
        &ctx.config,
        now(),
        IndexMode::Rebuild,
        &mut |_| {},
    )
    .unwrap();
    let hold_dir = &ctx.paths.hold_dir;
    for e in plan.entries.iter().chain(&plan.report_only) {
        let paths: Vec<&Path> = match &e.kind {
            EntryKind::Item { targets } => targets.iter().map(|t| t.path.as_path()).collect(),
            EntryKind::StoreRule { rule } => vec![rule.root.as_path()],
            _ => vec![],
        };
        for p in paths {
            assert!(
                !p.starts_with(hold_dir) && !hold_dir.starts_with(p),
                "plan offers {} ({e:#?})",
                p.display()
            );
        }
    }
    drop(home);
}

/// Every path at or below `p`, children first.
fn walkdir(p: &Path) -> Vec<PathBuf> {
    let mut out = Vec::new();
    if let Ok(rd) = std::fs::read_dir(p) {
        for e in rd.flatten() {
            if e.file_type().unwrap().is_dir() {
                out.extend(walkdir(&e.path()));
            } else {
                out.push(e.path());
            }
        }
    }
    out.push(p.to_path_buf());
    out
}

// ---- purge ----

#[test]
fn purge_takes_only_expired_entries() {
    let tmp = TempDir::new().unwrap();
    let home = tmp.path();
    let paths = Paths::for_home(home);
    let area = Area::open(&paths.hold_dir).unwrap();
    mkfile(&home.join("old/f"), 4096);
    mkfile(&home.join("new/f"), 4096);
    let old = hold_one(&area, &home.join("old"), now() - 8 * DAY);
    let new = hold_one(&area, &home.join("new"), now() - DAY);
    let purged = hold::purge(&paths.hold_dir, Some(hold::cutoff(now(), 7))).unwrap();
    assert_eq!(purged.len(), 1);
    assert_eq!(purged[0].held.id, old);
    assert!(
        matches!(purged[0].result, Ok(b) if b >= 4096),
        "{:?}",
        purged[0].result
    );
    let left: Vec<String> = held(&paths).into_iter().map(|h| h.id).collect();
    assert_eq!(left, vec![new]);
}

#[test]
fn apply_purges_expired_entries_first_even_when_holding_is_off() {
    for on in [true, false] {
        let (tmp, ctx) = fixture(on);
        let home = tmp.path();
        let area = Area::open(&ctx.paths.hold_dir).unwrap();
        mkfile(&home.join("stale/f"), 4096);
        let stale = hold_one(&area, &home.join("stale"), now() - 10 * DAY);
        drop(area);
        let plan = build_plan(&ctx, &registry(&ctx.config)).unwrap();
        let id = id_of(&plan, "artifacts");
        let report = apply_plan(
            &ctx,
            &registry(&ctx.config),
            &plan,
            std::slice::from_ref(&id),
        )
        .unwrap();
        assert_eq!(report.purged.len(), 1, "on={on}");
        assert_eq!(report.purged[0].held.id, stale);
        assert!(report.purged[0].result.is_ok());
        assert!(held(&ctx.paths).iter().all(|h| h.id != stale), "on={on}");
        let lines = cleaner::log::read(&ctx.paths.log_file, None).unwrap();
        assert!(
            lines
                .iter()
                .any(|l| l.category == "hold" && l.outcome == "deleted" && l.bytes > 0),
            "on={on}: {lines:#?}"
        );
    }
}

#[test]
fn purge_touches_only_hold_entries() {
    let tmp = TempDir::new().unwrap();
    let home = tmp.path();
    let paths = Paths::for_home(home);
    let area = Area::open(&paths.hold_dir).unwrap();
    mkfile(&home.join("outside/precious"), 10);
    mkfile(&home.join("d/f"), 10);
    symlink(home.join("outside"), home.join("d/link")).unwrap();
    hold_one(&area, &home.join("d"), now() - 30 * DAY);
    // Things in and next to the holding area that are not hold entries.
    mkfile(&paths.hold_dir.join("notes.txt"), 10);
    mkfile(&paths.hold_dir.join(".hidden/x"), 10);
    mkfile(&home.join(".local/share/cleaner/keep"), 10);
    symlink(
        home.join("outside"),
        paths.hold_dir.join("20250101T000000Z-1"),
    )
    .unwrap();

    let purged = hold::purge(&paths.hold_dir, None).unwrap();
    assert_eq!(purged.len(), 1, "purged non-entries");
    assert!(home.join("outside/precious").exists());
    assert!(paths.hold_dir.join("notes.txt").exists());
    assert!(paths.hold_dir.join(".hidden/x").exists());
    assert!(home.join(".local/share/cleaner/keep").exists());
}

#[test]
fn a_symlinked_holding_area_is_refused() {
    let (tmp, ctx) = fixture(true);
    let home = tmp.path();
    // The holding area is a symlink to a folder that looks like one.
    let fake = home.join("elsewhere");
    mkfile(&fake.join("20250101T000000Z-1/0/precious"), 10);
    std::fs::write(
        fake.join("20250101T000000Z-1/meta.json"),
        r#"{"held_at":0,"items":[]}"#,
    )
    .unwrap();
    std::fs::create_dir_all(ctx.paths.hold_dir.parent().unwrap()).unwrap();
    symlink(&fake, &ctx.paths.hold_dir).unwrap();

    assert!(Area::open(&ctx.paths.hold_dir).is_err());
    assert!(hold::purge(&ctx.paths.hold_dir, None).is_err());
    assert!(fake.join("20250101T000000Z-1/0/precious").exists());
    // Apply skips the entry rather than deleting it.
    let o = hold_target(&ctx);
    assert!(matches!(o, Outcome::Skipped { .. }), "{o:?}");
    assert!(target(home).join("debug/app").exists());
    assert!(fake.join("20250101T000000Z-1/0/precious").exists());
}

// ---- preview and confirmation text ----

fn item_entry(id: &str, path: &Path, bytes: u64) -> Entry {
    Entry {
        id: id.into(),
        category: "artifacts".into(),
        title: id.into(),
        kind: EntryKind::Item {
            targets: vec![Target {
                path: path.to_path_buf(),
                fp: fingerprint(path).unwrap(),
            }],
        },
        bytes,
        reason: "stale".into(),
        selected: true,
        permanent: false,
        note: None,
        last_active: None,
    }
}

#[test]
fn preview_says_what_each_entry_will_do() {
    let tmp = TempDir::new().unwrap();
    let home = tmp.path();
    let paths = Paths::for_home(home);
    mkfile(&home.join("a/f"), 10);
    let a = item_entry("artifacts:0", &home.join("a"), 10);
    let docker = Entry {
        id: "docker:0".into(),
        category: "docker".into(),
        kind: EntryKind::Object {
            obj: Object::Symlink {
                path: "/x".into(),
                target: "/y".into(),
            },
        },
        ..a.clone()
    };
    let plan = Plan {
        id: "p".into(),
        created: 0,
        entries: vec![a.clone(), docker],
        skipped: vec![],
        report_only: vec![],
    };
    let off = HoldCfg::default();
    assert_eq!(hold::preview(&plan, &paths.hold_dir, &off), None);
    let on = HoldCfg {
        enabled: true,
        days: 3,
    };
    // The holding area does not exist yet: its nearest existing parent decides.
    let p = hold::preview(&plan, &paths.hold_dir, &on).unwrap();
    assert_eq!(p.days, 3);
    assert_eq!(p.fates["artifacts:0"], Fate::Held);
    assert_eq!(p.fates["docker:0"], Fate::Permanent);
    assert!(!paths.hold_dir.exists(), "preview created the holding area");

    let other = Err::<u64, String>("holding area unavailable".into());
    assert!(matches!(hold::fate(&a, &other), Fate::Skipped(_)));
    let wrong_dev = Ok(h_dev(home).wrapping_add(1));
    assert!(matches!(hold::fate(&a, &wrong_dev), Fate::Skipped(_)));
}

#[test]
fn confirm_text_names_held_skipped_and_permanent() {
    let tmp = TempDir::new().unwrap();
    let home = tmp.path();
    mkfile(&home.join("a/f"), 10);
    mkfile(&home.join("b/f"), 10);
    let a = item_entry("artifacts:0", &home.join("a"), 1 << 20);
    let b = item_entry("artifacts:1", &home.join("b"), 1 << 20);
    let gens = Entry {
        id: "nix:0".into(),
        category: "nix".into(),
        title: "home-manager generations 1-3".into(),
        kind: EntryKind::Operation {
            op: Operation::NixStoreGc,
        },
        ..a.clone()
    };
    let plan = Plan {
        id: "p".into(),
        created: 0,
        entries: vec![a, b, gens],
        skipped: vec![],
        report_only: vec![],
    };
    let mut m = ReviewModel::new(plan);
    let mut fates = BTreeMap::new();
    fates.insert("artifacts:0".to_string(), Fate::Held);
    fates.insert(
        "artifacts:1".to_string(),
        Fate::Skipped("on another filesystem".into()),
    );
    fates.insert("nix:0".to_string(), Fate::Permanent);
    m.set_hold(Some(Preview { days: 7, fates }));
    let c = m.confirm(None);
    assert_eq!(c.heading, "Clean 3 items?");
    for want in [
        "1 item (1.0 MiB) will move to the holding area for 7 days",
        "cleaner hold restore",
        "Holding frees no space until held items are purged.",
        "1 item can't be held (on another filesystem) and will be skipped.",
        "1 item will be deleted now and cannot be restored: home-manager generations 1-3.",
    ] {
        assert!(c.body.contains(want), "missing {want:?} in {:?}", c.body);
    }
    assert!(!c.body.contains("This cannot be undone"), "{}", c.body);
}

// ---- cross-filesystem (mount namespace) ----

const DIR_ENV: &str = "CLEANER_TEST_NS_DIR";

/// Runs `body(dir)` inside a fresh user + mount namespace (see tests/mounts.rs).
fn in_mount_ns(test: &str, body: impl FnOnce(&Path)) {
    if let Some(dir) = std::env::var_os(DIR_ENV) {
        body(Path::new(&dir));
        return;
    }
    let probe = Command::new("unshare")
        .args(["--user", "--map-root-user", "--mount", "true"])
        .status();
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
    assert!(
        out.status.success(),
        "{test} failed in namespace:\n{stdout}\n{stderr}"
    );
    assert!(
        stdout.contains("1 passed"),
        "{test} did not run in namespace:\n{stdout}\n{stderr}"
    );
}

fn mount_tmpfs(dir: &Path) {
    std::fs::create_dir_all(dir).unwrap();
    let c = |s: &[u8]| CString::new(s).unwrap();
    let (src, target, fstype) = (c(b"tmpfs"), c(dir.as_os_str().as_bytes()), c(b"tmpfs"));
    let rc = unsafe {
        libc::mount(
            src.as_ptr(),
            target.as_ptr(),
            fstype.as_ptr(),
            0,
            std::ptr::null(),
        )
    };
    assert_eq!(rc, 0, "mount tmpfs: {}", std::io::Error::last_os_error());
}

#[test]
fn an_item_on_another_filesystem_is_skipped_never_copied_or_deleted() {
    in_mount_ns(
        "an_item_on_another_filesystem_is_skipped_never_copied_or_deleted",
        |dir| {
            let home = dir.join("home");
            let ctx = ctx_for(&home, true);
            mount_tmpfs(&home.join("mnt"));
            let item = home.join("mnt/item");
            mkfile(&item.join("f"), 4096);

            let area = Area::open(&ctx.paths.hold_dir).unwrap();
            let mut s = Session::new(to(&area, now()));
            let err = s.hold(&item, None).unwrap_err();
            assert!(err.contains("filesystem"), "{err}");
            assert!(s.finish().is_none());
            drop(area);
            assert!(item.join("f").exists());

            // Through apply: skipped, not deleted.
            let plan = Plan {
                id: "p".into(),
                created: 0,
                entries: vec![item_entry("artifacts:0", &item, 4096)],
                skipped: vec![],
                report_only: vec![],
            };
            let report =
                apply_plan(&ctx, &registry(&ctx.config), &plan, &["artifacts:0".into()]).unwrap();
            let o = outcome(&report, "artifacts:0");
            assert!(matches!(o, Outcome::Skipped { .. }), "{o:?}");
            assert!(item.join("f").exists(), "deleted instead of held");
            assert!(held(&ctx.paths).is_empty());
            let p = hold::preview(&plan, &ctx.paths.hold_dir, &ctx.config.hold).unwrap();
            assert!(matches!(p.fates["artifacts:0"], Fate::Skipped(_)), "{p:?}");
        },
    );
}

#[test]
fn restore_never_crosses_filesystems() {
    in_mount_ns("restore_never_crosses_filesystems", |dir| {
        let home = dir.join("home");
        let paths = Paths::for_home(&home);
        let area = Area::open(&paths.hold_dir).unwrap();
        mkfile(&home.join("m/item/f"), 10);
        let id = hold_one(&area, &home.join("m/item"), now());
        // The original parent is now another filesystem.
        mount_tmpfs(&home.join("m"));
        let err = hold::restore(&paths.hold_dir, Pick::Id(&id)).unwrap_err();
        assert!(format!("{err:#}").contains("filesystem"), "{err:#}");
        assert!(!home.join("m/item").exists());
        assert_eq!(held(&paths).len(), 1);
    });
}
