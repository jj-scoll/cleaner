use cleaner::disk::FsUsage;
use cleaner::log::LogLine;
use cleaner::plan::{Entry, EntryKind, Plan};
use cleaner::view::dashboard::{Banner, dashboard};
use cleaner::view::history::history;
use cleaner::view::{Level, category_label, level_for};
use jiff::tz::TimeZone;

const GIB: u64 = 1 << 30;
const CREATED: i64 = 1_794_366_000; // 2026-11-11 03:00 UTC

fn entry(id: &str, category: &str, bytes: u64, selected: bool) -> Entry {
    Entry {
        id: id.into(),
        category: category.into(),
        title: format!("title {id}"),
        kind: EntryKind::Item { targets: vec![] },
        bytes,
        reason: "because".into(),
        selected,
        permanent: false,
        note: None,
        last_active: None,
    }
}

fn plan(entries: Vec<Entry>, report_only: Vec<Entry>) -> Plan {
    Plan {
        id: "20261111T030000Z".into(),
        created: CREATED,
        entries,
        skipped: vec![],
        report_only,
    }
}

fn line(run_id: &str, plan_id: &str, time: i64, outcome: &str, bytes: u64) -> LogLine {
    LogLine {
        time,
        run_id: run_id.into(),
        plan_id: plan_id.into(),
        entry_id: "caches:0".into(),
        category: "caches".into(),
        title: "pip".into(),
        outcome: outcome.into(),
        bytes,
        detail: String::new(),
    }
}

fn ready_plan() -> Plan {
    plan(
        vec![
            entry("caches:0", "caches", 34_000_000_000, true),
            entry("trash:0", "trash", 37_000_000, true),
        ],
        vec![],
    )
}

#[test]
fn level_thresholds() {
    for f in [0.0, 0.7499] {
        assert_eq!(level_for(f), Level::Ok, "{f}");
    }
    for f in [0.75, 0.8999] {
        assert_eq!(level_for(f), Level::High, "{f}");
    }
    for f in [0.9, 1.0] {
        assert_eq!(level_for(f), Level::Critical, "{f}");
    }
    assert_eq!(Level::Ok.css_class(), "success");
    assert_eq!(Level::High.css_class(), "warning");
    assert_eq!(Level::Critical.css_class(), "error");
}

#[test]
fn fs_bar_text() {
    let fs = [FsUsage { label: "/".into(), total: 100 * GIB, avail: 18 * GIB }];
    let d = dashboard(&fs, None, &[], &TimeZone::UTC);
    let bar = &d.filesystems[0];
    assert_eq!(bar.label, "/");
    assert_eq!(bar.used_fraction, 0.82);
    assert_eq!(bar.level, Level::High);
    assert_eq!(bar.free_text, "18.0 GiB free of 100.0 GiB");
}

#[test]
fn banner_ready() {
    let p = ready_plan();
    let d = dashboard(&[], Some(&p), &[], &TimeZone::UTC);
    assert_eq!(
        d.banner,
        Banner::Ready {
            plan_id: p.id.clone(),
            title: "New plan ready: 31.7 GiB can be freed".into(),
            subtitle: "Built 2026-11-11 03:00 · nothing has been touched yet".into(),
        }
    );
}

#[test]
fn banner_after_apply_is_nothing() {
    let p = ready_plan();
    let log = [line("R", &p.id, CREATED + 60, "deleted", 4096)];
    let d = dashboard(&[], Some(&p), &log, &TimeZone::UTC);
    assert_eq!(d.banner, Banner::Nothing);
}

#[test]
fn banner_no_plan() {
    let d = dashboard(&[], None, &[], &TimeZone::UTC);
    assert_eq!(d.banner, Banner::NoPlan);
    assert!(d.categories.is_empty());
    assert_eq!(d.last_run, None);
}

#[test]
fn banner_nothing_selected() {
    let p = plan(vec![entry("caches:0", "caches", 1024, false)], vec![]);
    let d = dashboard(&[], Some(&p), &[], &TimeZone::UTC);
    assert_eq!(d.banner, Banner::Nothing);
}

#[test]
fn category_rows_in_order() {
    let p = plan(
        vec![
            entry("trash:0", "trash", 100, true),
            entry("trash:1", "trash", 50, false),
            entry("caches:0", "caches", 7, true),
        ],
        vec![entry("idle_folders:0", "idle_folders", 999, false)],
    );
    let d = dashboard(&[], Some(&p), &[], &TimeZone::UTC);
    let labels: Vec<&str> = d.categories.iter().map(|c| c.label.as_str()).collect();
    assert_eq!(labels, ["Tool caches", "Trash", "Large and old"]);
    let trash = &d.categories[1];
    assert_eq!(trash.id, "trash");
    assert_eq!((trash.selected_count, trash.selected_bytes), (1, 100));
    let idle = &d.categories[2];
    assert_eq!((idle.selected_count, idle.report_only_count), (0, 1));
}

#[test]
fn unknown_category_label_is_id() {
    assert_eq!(category_label("no_such_category"), "no_such_category");
    assert_eq!(category_label("artifacts"), "Project artifacts");
    assert_eq!(category_label("user_rules"), "Folder rules");
}

#[test]
fn history_groups_runs() {
    let log = [
        line("A", "P1", CREATED, "deleted", 10),
        line("A", "P1", CREATED + 1, "skipped", 0),
        line("A", "P1", CREATED + 2, "failed", 0),
        line("B", "P2", CREATED + 3600, "deleted", 5),
    ];
    let runs = history(&log, &TimeZone::UTC);
    let ids: Vec<&str> = runs.iter().map(|r| r.run_id.as_str()).collect();
    assert_eq!(ids, ["B", "A"]);
    let a = &runs[1];
    assert_eq!(a.plan_id, "P1");
    assert_eq!(a.when, "2026-11-11 03:00");
    assert_eq!(a.deleted_bytes, 10);
    assert_eq!((a.deleted, a.skipped, a.failed), (1, 1, 1));
    let outcomes: Vec<&str> = a.items.iter().map(|l| l.outcome.as_str()).collect();
    assert_eq!(outcomes, ["deleted", "skipped", "failed"]);
}

#[test]
fn last_run_text() {
    let log = [line("R", "P", 1_794_388_320, "deleted", 4096)];
    let d = dashboard(&[], None, &log, &TimeZone::UTC);
    assert_eq!(d.last_run.as_deref(), Some("Last run 2026-11-11 09:12 · deleted 4.0 KiB"));
}

#[test]
fn plan2_labels() {
    assert_eq!(category_label("worktrees"), "Finished worktrees");
    assert_eq!(category_label("app_caches"), "Browser and app caches");
    assert_eq!(category_label("docker"), "Docker");
    assert_eq!(category_label("nix"), "Nix");
}

#[test]
fn category_order_includes_skip_only_categories() {
    let mut p = plan(vec![], vec![]);
    p.skipped.push(cleaner::plan::Skip {
        category: "scan_roots".into(),
        title: "~/ws".into(),
        reason: "r".into(),
    });
    assert!(cleaner::view::category_order(&p).contains(&"scan_roots".to_string()));
}

#[test]
fn plan4_skip_labels() {
    assert_eq!(category_label("scan_roots"), "Scan roots");
    assert_eq!(category_label("disk_map"), "Disk map");
}
