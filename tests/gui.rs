//! GUI smoke test (harness = false): builds pages and inspects their widgets.
//! Checks arrive with the pages (plan 3, Tasks 5–7 and 10).

use adw::prelude::*;
use cleaner::disk::FsUsage;
use cleaner::engine::Progress;
use cleaner::gui::dashboard::DashboardPage;
use cleaner::gui::history::HistoryPage;
use cleaner::config::Config;
use cleaner::gui::review::ReviewPage;
use cleaner::gui::settings::SettingsPage;
use cleaner::gui::block_close_while;
use cleaner::gui::testing::{TestApp, texts};
use cleaner::gui::usage::UsagePage;
use cleaner::fsutil::usage::{Node, Tree, UsageIndex};
use cleaner::log::LogLine;
use cleaner::plan::{Entry, EntryKind, Plan, Skip};
use cleaner::view::dashboard::dashboard;
use cleaner::view::history::history;
use cleaner::view::review::ReviewModel;
use cleaner::view::usage::{level, roots};
use jiff::tz::TimeZone;
use std::cell::RefCell;
use std::rc::Rc;

const GIB: u64 = 1 << 30;

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
    }
}

fn plan(entries: Vec<Entry>) -> Plan {
    Plan {
        id: "20261111T030000Z".into(),
        created: 1_794_366_000,
        entries,
        skipped: vec![],
        report_only: vec![],
    }
}

fn fs() -> Vec<FsUsage> {
    vec![FsUsage { label: "/home".into(), total: 100 * GIB, avail: 18 * GIB }]
}

fn has(texts: &[String], want: &str) -> bool {
    texts.iter().any(|t| t == want)
}

fn assert_has(texts: &[String], want: &str) {
    assert!(has(texts, want), "missing {want:?} in {texts:#?}");
}

fn dashboard_shows_banner_and_categories() {
    let p = plan(vec![
        entry("caches:0", "caches", 34_037_000_000 - 4096, true),
        entry("trash:0", "trash", 4096, true),
    ]);
    let page = DashboardPage::new();
    page.update(&dashboard(&fs(), Some(&p), &[], &TimeZone::UTC));
    let t = texts(&page.page());
    for want in ["New plan ready: 31.7 GiB can be freed", "Review plan", "Clean all", "Tool caches", "Trash"] {
        assert_has(&t, want);
    }
}

fn dashboard_no_plan() {
    let page = DashboardPage::new();
    page.update(&dashboard(&fs(), None, &[], &TimeZone::UTC));
    let t = texts(&page.page());
    assert_has(&t, "No plan yet");
    assert_has(&t, "Scan now");
}

fn busy_shows_progress() {
    let page = DashboardPage::new();
    page.update(&dashboard(&fs(), None, &[], &TimeZone::UTC));
    page.set_busy(Some(&Progress { done: 1, total: 4, current: "Tool caches".into() }));
    assert_has(&texts(&page.page()), "Scanning Tool caches…");
}

fn model(p: Plan) -> Rc<RefCell<ReviewModel>> {
    Rc::new(RefCell::new(ReviewModel::new(p)))
}

/// Every CheckButton under `w`, depth-first.
fn check_buttons(w: &gtk::Widget) -> Vec<gtk::CheckButton> {
    let mut out = Vec::new();
    if let Some(c) = w.downcast_ref::<gtk::CheckButton>() {
        out.push(c.clone());
    }
    let mut child = w.first_child();
    while let Some(c) = child {
        out.extend(check_buttons(&c));
        child = c.next_sibling();
    }
    out
}

fn clean_label(page: &ReviewPage) -> String {
    page.clean_button().label().unwrap_or_default().to_string()
}

fn review_toggle_updates_clean_button() {
    let m = model(plan(vec![entry("trash:0", "trash", 1024, true), entry("trash:1", "trash", 1024, true)]));
    let page = ReviewPage::new(m.clone(), None);
    assert_eq!(clean_label(&page), "Clean selected (2.0 KiB)");
    let checks = check_buttons(page.page().upcast_ref());
    // Row check buttons sit inside their ActionRow; the group-level one doesn't.
    let in_row = |c: &gtk::CheckButton| c.ancestor(adw::ActionRow::static_type()).is_some();
    let row_check = checks.iter().find(|c| in_row(c)).expect("a row check button");
    row_check.set_active(false);
    assert_eq!(clean_label(&page), "Clean selected (1.0 KiB)");
    assert_eq!(m.borrow().selected_ids(None).len(), 1);

    let group_check = checks.iter().find(|c| !in_row(c)).expect("a group check button");
    group_check.set_active(true);
    assert_eq!(clean_label(&page), "Clean selected (2.0 KiB)");
    assert!(row_check.is_active(), "group check re-ticks the row");
    group_check.set_active(false);
    assert_eq!(clean_label(&page), "Clean selected (0 B)");
    assert!(!page.clean_button().is_sensitive(), "nothing selected: Clean is insensitive");
}

fn review_marks_skipped_and_permanent() {
    let mut p = plan(vec![Entry { permanent: true, ..entry("trash:0", "trash", 1024, true) }]);
    p.skipped.push(Skip { category: "trash".into(), title: "locked".into(), reason: "in use".into() });
    let page = ReviewPage::new(model(p), None);
    let t = texts(&page.page());
    assert_has(&t, "⊘ locked");
    assert_has(&t, "permanent");
}

fn review_filter_shows_one_group() {
    let p = plan(vec![entry("caches:0", "caches", 1024, true), entry("trash:0", "trash", 1024, true)]);
    let page = ReviewPage::new(model(p), Some("trash".into()));
    let t = texts(&page.page());
    assert_has(&t, "Trash");
    assert!(!has(&t, "Tool caches"), "filtered page shows other categories: {t:#?}");
    assert_eq!(clean_label(&page), "Clean selected (1.0 KiB)");
}

fn log_line(run_id: &str, time: i64, title: &str) -> LogLine {
    LogLine {
        time,
        run_id: run_id.into(),
        plan_id: "20261111T030000Z".into(),
        entry_id: "trash:0".into(),
        category: "trash".into(),
        title: title.into(),
        outcome: "deleted".into(),
        bytes: 4096,
        detail: String::new(),
    }
}

fn history_lists_runs() {
    let log = [log_line("A", 1_794_366_000, "old.zip"), log_line("B", 1_794_388_320, "new.zip")];
    let runs = history(&log, &TimeZone::UTC);
    let t = texts(&HistoryPage::new(&runs).page());
    assert_has(&t, "2026-11-11 03:00");
    assert_has(&t, "2026-11-11 09:12");
    assert!(
        t.iter().any(|s| s.contains("1 deleted, 0 skipped, 0 failed")),
        "missing run counts in {t:#?}"
    );
}

fn history_empty() {
    assert_has(&texts(&HistoryPage::new(&[]).page()), "No runs yet");
}

/// Every Button under `w` whose label is `label` and that is not hidden.
fn visible_buttons(w: &gtk::Widget, label: &str) -> usize {
    let mut n = 0;
    if let Some(b) = w.downcast_ref::<gtk::Button>()
        && b.label().as_deref() == Some(label)
        && b.is_visible()
    {
        n += 1;
    }
    let mut child = w.first_child();
    while let Some(c) = child {
        n += visible_buttons(&c, label);
        child = c.next_sibling();
    }
    n
}

fn settings_read_only() {
    let page = SettingsPage::new(&Config::default(), true);
    assert_has(&texts(&page.page()), "config.toml is read-only (managed elsewhere); settings are view-only");
    assert_eq!(visible_buttons(page.page().upcast_ref(), "Save"), 0, "Save is visible on a read-only config");
    // Sanity: the editable page does show it.
    let editable = SettingsPage::new(&Config::default(), false);
    assert_eq!(visible_buttons(editable.page().upcast_ref(), "Save"), 1);
}

fn settings_shows_builtins_locked() {
    assert_has(&texts(&SettingsPage::new(&Config::default(), false).page()), "~/.claude");
}

fn settings_shows_plan2_groups() {
    let texts = texts(&SettingsPage::new(&Config::default(), false).page());
    for want in ["Finished worktrees", "Browser and app caches", "Docker", "Nix", "System generations always ask for your password."] {
        assert_has(&texts, want);
    }
    // Right after Tool caches, in registry order.
    let at = |t: &str| texts.iter().position(|s| s == t).unwrap();
    let order = ["Tool caches", "Finished worktrees", "Browser and app caches", "Docker", "Nix", "Folder rules"].map(at);
    assert!(order.is_sorted(), "groups out of order: {order:?}");
}

fn settings_general_has_scan_roots_and_low_space() {
    let t = texts(&SettingsPage::new(&Config::default(), false).page());
    for want in ["Add a scan root", "Warn when disk space is low", "Warn when free space is below (%)"] {
        assert_has(&t, want);
    }
    assert!(!has(&t, "Add a search root"), "{t:#?}");
}

fn settings_shows_root_overrides() {
    let mut c = Config::default();
    c.artifacts.roots = Some(vec!["~/src".into()]);
    assert_has(&texts(&SettingsPage::new(&c, false).page()), "Overridden in config.toml for: artifacts");
    assert!(!texts(&SettingsPage::new(&Config::default(), false).page()).iter().any(|t| t.starts_with("Overridden")));
}

fn spin_rows(w: &gtk::Widget) -> Vec<adw::SpinRow> {
    let mut out = Vec::new();
    if let Some(s) = w.downcast_ref::<adw::SpinRow>() {
        out.push(s.clone());
    }
    let mut child = w.first_child();
    while let Some(c) = child {
        out.extend(spin_rows(&c));
        child = c.next_sibling();
    }
    out
}

/// 0 days would pre-select every stopped container or prune all unused build cache.
fn settings_docker_days_at_least_one() {
    let page = SettingsPage::new(&Config::default(), false);
    let rows = spin_rows(page.page().upcast_ref());
    for title in ["Stopped containers older than (days)", "Build cache unused for (days)"] {
        let row = rows.iter().find(|r| r.title() == title).unwrap_or_else(|| panic!("no spin {title:?}"));
        assert_eq!(row.adjustment().lower(), 1.0, "{title}");
    }
}

/// While a clean runs, closing the window is refused (the process would exit
/// mid-delete and the run's log lines would never be written); after, it closes.
fn close_blocked_while_cleaning() {
    let window = gtk::Window::new();
    window.present();
    let busy = Rc::new(std::cell::Cell::new(true));
    let blocked = Rc::new(std::cell::Cell::new(0));
    let b = blocked.clone();
    block_close_while(&window, busy.clone(), move || b.set(b.get() + 1));
    window.close();
    assert!(window.is_visible(), "window closed during a clean");
    assert_eq!(blocked.get(), 1);
    busy.set(false);
    window.close();
    assert!(!window.is_visible(), "window stayed open after the clean");
    assert_eq!(blocked.get(), 1);
}

fn usage_index() -> UsageIndex {
    let node = |name: &str, bytes: u64| Node { name: name.into(), bytes, dir: true, ..Node::default() };
    let euid = rustix::process::geteuid().as_raw();
    let locked = Node { unreadable_owner: Some(euid + 1), ..node("locked", 0) };
    let top = Node { children: vec![node("big", 600), node("small", 300), locked], ..node("r", 900) };
    let tree = Tree { root: "/r".into(), dev: 1, fs_total: 2000, fs_used: 1000, node: top };
    UsageIndex { created: 1_794_366_000, min_node: 1, trees: vec![tree] }
}

fn usage_page() -> UsagePage {
    let item = Entry {
        kind: EntryKind::Item {
            targets: vec![cleaner::plan::Target {
                path: "/r/big/x".into(),
                fp: cleaner::plan::Fingerprint { dev: 1, ino: 2, newest_mtime: 3 },
            }],
        },
        ..entry("caches:0", "caches", 4096, true)
    };
    let p = plan(vec![item]);
    let l = level(&usage_index(), std::path::Path::new("/r"), Some(&p), 1_794_366_000, &TimeZone::UTC);
    UsagePage::new(l.as_ref())
}

fn usage_page_lists_rows() {
    let t = texts(&usage_page().page());
    for want in ["big", "small", "in plan: 1 row \u{b7} 4.0 KiB", "Open in Files", "Rescan"] {
        assert_has(&t, want);
    }
    let euid = rustix::process::geteuid().as_raw();
    assert_has(&t, &format!("unreadable (owned by uid {})", euid + 1));
}

fn usage_page_has_no_delete() {
    let page = usage_page().page();
    assert!(check_buttons(page.upcast_ref()).is_empty(), "disk map has check buttons");
    for label in ["Delete", "Clean selected"] {
        assert_eq!(visible_buttons(page.upcast_ref(), label), 0, "{label} on the disk map");
    }
}

fn usage_page_empty_index() {
    assert_has(&texts(&UsagePage::new(None).page()), "No disk index yet. Run a scan.");
}

fn usage_page_roots_level() {
    let l = roots(&usage_index(), None, 1_794_366_000, &TimeZone::UTC);
    let t = texts(&UsagePage::new(Some(&l)).page());
    assert_has(&t, "/r");
    assert_has(&t, "scanned 2026-11-11 03:00 \u{b7} under 1 h ago");
}

fn dashboard_has_disk_button() {
    let page = DashboardPage::new();
    page.update(&dashboard(&fs(), None, &[], &TimeZone::UTC));
    assert_eq!(visible_buttons(page.page().upcast_ref(), "What's using my disk"), 1);
}

/// Drilling pushes a second, distinct page showing the child's rows. Run under
/// `G_DEBUG=fatal-criticals`: a duplicate page tag is a g_critical.
fn usage_drill_pushes_distinct_pages() {
    let home = tempfile::tempdir().unwrap();
    let paths = cleaner::paths::Paths::for_home(home.path());
    let mut idx = usage_index();
    idx.trees[0].node.children[0].children = vec![Node { name: "inner".into(), bytes: 600, ..Node::default() }];
    std::fs::create_dir_all(&paths.state_dir).unwrap();
    idx.save(&paths.usage_index).unwrap();
    let app = TestApp::new(paths);
    let base = app.pages().len();
    app.open_usage(None);
    app.open_usage(Some("/r/big".into()));
    let pages = app.pages();
    assert_eq!(pages.len(), base + 2, "drill did not push");
    let (roots, drilled) = (&pages[base], &pages[base + 1]);
    assert_ne!(roots, drilled);
    assert_has(&texts(roots), "/r");
    let t = texts(drilled);
    assert_has(&t, "inner");
    assert!(!has(&t, "small"), "drilled page shows the parent's rows: {t:#?}");
}

fn main() {
    let display =["WAYLAND_DISPLAY", "DISPLAY"]
        .iter()
        .any(|v| std::env::var_os(v).is_some_and(|s| !s.is_empty()));
    if !display {
        println!("gui smoke: skipped (no display)");
        return;
    }
    gtk::init().expect("gtk init");
    adw::init().expect("adw init");
    let checks: &[(&str, fn())] = &[
        ("dashboard_shows_banner_and_categories", dashboard_shows_banner_and_categories),
        ("dashboard_no_plan", dashboard_no_plan),
        ("busy_shows_progress", busy_shows_progress),
        ("review_toggle_updates_clean_button", review_toggle_updates_clean_button),
        ("review_marks_skipped_and_permanent", review_marks_skipped_and_permanent),
        ("review_filter_shows_one_group", review_filter_shows_one_group),
        ("history_lists_runs", history_lists_runs),
        ("history_empty", history_empty),
        ("settings_read_only", settings_read_only),
        ("settings_shows_builtins_locked", settings_shows_builtins_locked),
        ("settings_shows_plan2_groups", settings_shows_plan2_groups),
        ("settings_docker_days_at_least_one", settings_docker_days_at_least_one),
        ("settings_general_has_scan_roots_and_low_space", settings_general_has_scan_roots_and_low_space),
        ("settings_shows_root_overrides", settings_shows_root_overrides),
        ("close_blocked_while_cleaning", close_blocked_while_cleaning),
        ("usage_page_lists_rows", usage_page_lists_rows),
        ("usage_page_has_no_delete", usage_page_has_no_delete),
        ("usage_page_empty_index", usage_page_empty_index),
        ("usage_page_roots_level", usage_page_roots_level),
        ("dashboard_has_disk_button", dashboard_has_disk_button),
        ("usage_drill_pushes_distinct_pages", usage_drill_pushes_distinct_pages),
    ];
    for (name, check) in checks {
        check();
        println!("ok {name}");
    }
}
