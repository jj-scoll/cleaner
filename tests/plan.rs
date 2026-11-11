mod common;

use cleaner::category::{Outcome, ScanCtx, apply_targets};
use cleaner::config::Config;
use cleaner::plan::{
    Entry, EntryKind, Fingerprint, Plan, Requested, Skip, Target, fingerprint, resolve_requested,
};
use cleaner::protect::Protector;
use common::{mkfile, now, set_age};
use std::ffi::OsStr;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};

fn item(path: PathBuf, bytes: u64, selected: bool) -> Entry {
    Entry {
        id: String::new(),
        category: "test".into(),
        title: path.display().to_string(),
        kind: EntryKind::Item {
            targets: vec![Target {
                path,
                fp: Fingerprint { dev: 1, ino: 2, newest_mtime: 3 },
            }],
        },
        bytes,
        reason: "because".into(),
        selected,
        permanent: false,
        note: None,
        last_active: None,
    }
}

fn plan_with(id: &str, entries: Vec<Entry>) -> Plan {
    Plan {
        id: id.into(),
        created: 1_700_000_000,
        entries,
        skipped: vec![Skip {
            category: "test".into(),
            title: "tracked thing".into(),
            reason: "git-tracked".into(),
        }],
        report_only: vec![],
    }
}

fn with_ctx<R>(home: &Path, f: impl FnOnce(&ScanCtx) -> R) -> R {
    let config = Config::default();
    let protector = Protector::new(home, &[]);
    let tools = common::FakeRunner::new();
    let ctx = ScanCtx { home, now: now(), config: &config, protector: &protector, tools: &tools, euid: common::my_uid(), usage: None };
    f(&ctx)
}

fn target_for(path: &Path) -> Target {
    Target { path: path.to_path_buf(), fp: fingerprint(path).unwrap() }
}

#[test]
fn plan_roundtrip() {
    let tmp = tempfile::tempdir().unwrap();
    let plans = tmp.path().join("plans");
    let plan = plan_with("20261110T030000Z", vec![item("/x/target".into(), 42, true)]);
    let saved = plan.save(&plans).unwrap();
    assert_eq!(saved, plans.join("20261110T030000Z.json"));
    assert_eq!(Plan::load(&saved).unwrap(), plan);
}

#[test]
fn non_utf8_path_roundtrips() {
    let tmp = tempfile::tempdir().unwrap();
    let plans = tmp.path().join("plans");
    let odd = PathBuf::from(OsStr::from_bytes(b"/x/bad\xffname (1).zip"));
    let plan = plan_with("20261110T030000Z", vec![item(odd.clone(), 1, true)]);
    let saved = plan.save(&plans).unwrap();
    let json = std::fs::read_to_string(&saved).unwrap();
    assert!(json.contains("\"bytes\""), "{json}");
    let loaded = Plan::load(&saved).unwrap();
    assert_eq!(loaded, plan);
    let EntryKind::Item { targets } = &loaded.entries[0].kind else { panic!("not an item") };
    assert_eq!(targets[0].path, odd);
}

#[test]
fn prune_keeps_ten() {
    let tmp = tempfile::tempdir().unwrap();
    let plans = tmp.path().join("plans");
    for i in 0..12 {
        plan_with(&format!("202611{:02}T030000Z", i + 1), vec![]).save(&plans).unwrap();
    }
    let mut names: Vec<String> = std::fs::read_dir(&plans)
        .unwrap()
        .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
        .collect();
    names.sort();
    assert_eq!(names.len(), 10, "{names:?}");
    assert!(!plans.join("20261101T030000Z.json").exists());
    assert!(!plans.join("20261102T030000Z.json").exists());
    assert_eq!(names[0], "20261103T030000Z.json");
    assert_eq!(names[9], "20261112T030000Z.json");
}

#[test]
fn latest_picks_newest() {
    let tmp = tempfile::tempdir().unwrap();
    let plans = tmp.path().join("plans");
    assert_eq!(Plan::latest(&plans).unwrap(), None);
    for id in ["20261101T030000Z", "20261103T030000Z", "20261102T030000Z"] {
        plan_with(id, vec![]).save(&plans).unwrap();
    }
    assert_eq!(
        Plan::latest(&plans).unwrap(),
        Some(plans.join("20261103T030000Z.json"))
    );
    assert_eq!(Plan::new_id(1_762_743_600), "20251110T030000Z");
}

#[test]
fn shown_roundtrip() {
    let tmp = tempfile::tempdir().unwrap();
    assert_eq!(Plan::last_shown(tmp.path()), None);
    Plan::mark_shown(tmp.path(), "X").unwrap();
    assert_eq!(Plan::last_shown(tmp.path()), Some("X".to_string()));
}

#[test]
fn requested_plan_missing_or_corrupt() {
    let tmp = tempfile::tempdir().unwrap();
    let plans = tmp.path().join("plans");
    let id = "20261110T030000Z";
    plan_with(id, vec![item("/x/target".into(), 42, true)]).save(&plans).unwrap();

    assert!(matches!(resolve_requested(&plans, None), Requested::Latest));
    match resolve_requested(&plans, Some(id)) {
        Requested::Plan(path, plan) => {
            assert_eq!(plan.id, id);
            assert_eq!(path, plans.join(format!("{id}.json")));
        }
        _ => panic!("expected Plan"),
    }
    let problem = |r: Requested| match r {
        Requested::Problem(m) => m,
        _ => panic!("expected Problem"),
    };
    let m = problem(resolve_requested(&plans, Some("20000101T000000Z")));
    assert!(m.contains("20000101T000000Z"), "{m}");
    std::fs::write(plans.join("20261111T030000Z.json"), "{").unwrap();
    let m = problem(resolve_requested(&plans, Some("20261111T030000Z")));
    assert!(m.contains("parsing plan"), "{m}");
    let m = problem(resolve_requested(&plans, Some("../x")));
    assert!(m.contains("not a plan id"), "{m}");
}

#[test]
fn selected_bytes_sums_selected_only() {
    let plan = plan_with(
        "x",
        vec![
            item("/a".into(), 100, true),
            item("/b".into(), 50, false),
            item("/c".into(), 7, true),
        ],
    );
    assert_eq!(plan.selected_bytes(), 107);
}

#[test]
fn apply_targets_deletes_matching() {
    let home = tempfile::tempdir().unwrap();
    let dir = home.path().join("proj/target");
    mkfile(&dir.join("debug/a.o"), 8192);
    let t = target_for(&dir);
    let out = with_ctx(home.path(), |ctx| apply_targets(&[t], ctx));
    match out {
        Outcome::Deleted { bytes, failures } => {
            assert!(bytes > 0);
            assert!(failures.is_empty(), "{failures:?}");
        }
        other => panic!("expected Deleted, got {other:?}"),
    }
    assert!(!dir.exists());
}

#[test]
fn apply_targets_skips_changed() {
    let home = tempfile::tempdir().unwrap();
    let dir = home.path().join("proj/target");
    mkfile(&dir.join("a.o"), 100);
    set_age(&dir.join("a.o"), 10);
    set_age(&dir, 10);
    let t = target_for(&dir);
    mkfile(&dir.join("b.o"), 100); // newer mtime than anything planned
    let out = with_ctx(home.path(), |ctx| apply_targets(&[t], ctx));
    match out {
        Outcome::Skipped { reason } => assert!(reason.contains("changed since plan"), "{reason}"),
        other => panic!("expected Skipped, got {other:?}"),
    }
    assert!(dir.join("a.o").exists());
    assert!(dir.join("b.o").exists());
}

#[test]
fn apply_targets_reports_delete_error() {
    if rustix::process::geteuid().is_root() {
        eprintln!("skipping: permission checks do not apply to root");
        return;
    }
    let home = tempfile::tempdir().unwrap();
    let parent = home.path().join("ro");
    let dir = parent.join("target");
    mkfile(&dir.join("a.o"), 100);
    let t = target_for(&dir);
    std::fs::set_permissions(&parent, std::fs::Permissions::from_mode(0o555)).unwrap();
    let out = with_ctx(home.path(), |ctx| apply_targets(&[t], ctx));
    std::fs::set_permissions(&parent, std::fs::Permissions::from_mode(0o755)).unwrap();
    match out {
        Outcome::Skipped { reason } => {
            assert!(reason.contains("Permission denied"), "{reason}")
        }
        other => panic!("expected Skipped, got {other:?}"),
    }
    assert!(dir.exists());
}

fn kind_entry(kind: EntryKind) -> Entry {
    Entry { kind, ..item("/x".into(), 1, true) }
}

#[test]
fn object_and_operation_roundtrip() {
    use cleaner::object::{DockerKind, DockerRef, Object, Operation, WorktreeRef};
    let tmp = tempfile::tempdir().unwrap();
    let plans = tmp.path().join("plans");
    let fp = Fingerprint { dev: 1, ino: 2, newest_mtime: 3 };
    let objects = vec![
        Object::Worktree(WorktreeRef {
            path: "/h/src/app-feat".into(),
            main: "/h/src/app".into(),
            head: "0123456789abcdef0123456789abcdef01234567".into(),
            branch: Some("feat".into()),
            newest_mtime: 1_700_000_000,
            dev: 7,
            ino: 8,
            ignored: vec![".env".into()],
        }),
        Object::OrphanWorktree { path: "/h/src/gone".into(), fp: fp.clone() },
        Object::Docker {
            refs: vec![
                DockerRef { kind: DockerKind::Volume, id: "v1".into(), created: None },
                DockerRef {
                    kind: DockerKind::Container,
                    id: "c1".into(),
                    created: Some("2026-11-01 10:00:00 +0000 UTC".into()),
                },
            ],
        },
        Object::NixGenerations {
            profile: "/nix/var/nix/profiles/system".into(),
            gens: vec![(41, "/nix/store/abc-system".into()), (42, "/nix/store/def-system".into())],
            root: true,
        },
        Object::Symlink {
            path: PathBuf::from(OsStr::from_bytes(b"/h/proj/result\xff")),
            target: "/nix/store/xyz-out".into(),
        },
    ];
    let ops = vec![
        Operation::UvCachePrune,
        Operation::DockerBuilderPrune { until_hours: 168 },
        Operation::NixStoreGc,
        Operation::GoCleanModcache,
    ];
    let entries = objects
        .into_iter()
        .map(|obj| kind_entry(EntryKind::Object { obj }))
        .chain(ops.into_iter().map(|op| kind_entry(EntryKind::Operation { op })))
        .collect();
    let plan = plan_with("20261110T030000Z", entries);
    let saved = plan.save(&plans).unwrap();
    assert_eq!(Plan::load(&saved).unwrap(), plan);
}

#[test]
fn go_operation_roundtrip() {
    use cleaner::object::Operation;
    let tmp = tempfile::tempdir().unwrap();
    let plan = plan_with(
        "20261110T030000Z",
        vec![kind_entry(EntryKind::Operation { op: Operation::GoCleanModcache })],
    );
    let saved = plan.save(tmp.path()).unwrap();
    assert_eq!(Plan::load(&saved).unwrap(), plan);
}

#[test]
fn estimate_kinds() {
    use cleaner::category::store::{StoreRule, Unit};
    use cleaner::object::{Object, Operation};
    let rule = StoreRule {
        root: "/h/.cache/pip".into(),
        max_age_days: 30,
        unit: Unit::Files { recursive: true },
        glob: None,
        min_bytes: None,
    };
    assert!(kind_entry(EntryKind::StoreRule { rule }).is_estimate());
    assert!(kind_entry(EntryKind::Operation { op: Operation::UvCachePrune }).is_estimate());
    assert!(!item("/x".into(), 1, true).is_estimate());
    // D5: closure sizes are estimates; a dangling link (0 bytes) is exact.
    let obj = Object::Symlink { path: "/h/result".into(), target: "/nix/store/x".into() };
    assert!(kind_entry(EntryKind::Object { obj: obj.clone() }).is_estimate());
    assert!(!Entry { bytes: 0, ..kind_entry(EntryKind::Object { obj }) }.is_estimate());
    let gens = Object::NixGenerations { profile: "/p/system".into(), gens: vec![(1, "/nix/store/x".into())], root: true };
    assert!(kind_entry(EntryKind::Object { obj: gens }).is_estimate());
    let docker = Object::Docker { refs: vec![] };
    assert!(!kind_entry(EntryKind::Object { obj: docker }).is_estimate());
}

#[test]
fn apply_default_refuses_new_kinds() {
    use cleaner::category::apply_default;
    use cleaner::object::Operation;
    let home = tempfile::tempdir().unwrap();
    let e = kind_entry(EntryKind::Operation { op: Operation::UvCachePrune });
    match with_ctx(home.path(), |ctx| apply_default(&e, ctx)) {
        Outcome::Skipped { reason } => assert!(reason.contains("can't apply"), "{reason}"),
        other => panic!("expected Skipped, got {other:?}"),
    }
}
