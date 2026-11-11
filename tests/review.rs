use cleaner::category::store::{StoreRule, Unit};
use cleaner::object::Operation;
use cleaner::plan::{Entry, EntryKind, Fingerprint, Plan, Skip, Target};
use cleaner::view::review::{Confirm, ReviewModel, Sort};

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

fn store_entry(id: &str, bytes: u64) -> Entry {
    let rule = StoreRule {
        root: "/home/x/.cache/pip".into(),
        max_age_days: 30,
        unit: Unit::Files { recursive: true },
        glob: None,
        min_bytes: None,
    };
    Entry { kind: EntryKind::StoreRule { rule }, ..entry(id, "caches", bytes, true) }
}

fn plan(entries: Vec<Entry>, report_only: Vec<Entry>, skipped: Vec<Skip>) -> Plan {
    Plan { id: "20261111T030000Z".into(), created: 1_794_366_000, entries, skipped, report_only }
}

fn ids(v: &[&str]) -> Vec<String> {
    v.iter().map(|s| s.to_string()).collect()
}

#[test]
fn initial_selection_matches_plan() {
    let m = ReviewModel::new(plan(
        vec![
            entry("trash:0", "trash", 10, true),
            entry("trash:1", "trash", 20, false),
            entry("caches:0", "caches", 30, true),
        ],
        vec![],
        vec![],
    ));
    assert_eq!(m.selected_ids(None), ids(&["trash:0", "caches:0"]));
    assert_eq!(m.plan().id, "20261111T030000Z");
}

#[test]
fn toggle_and_totals() {
    let mut m = ReviewModel::new(plan(
        vec![entry("trash:0", "trash", 4096, true), entry("trash:1", "trash", 1024, true)],
        vec![],
        vec![],
    ));
    let before = m.selected_bytes(None);
    m.set("trash:0", false);
    assert_eq!(m.selected_bytes(None), before - 4096);
    assert_eq!(m.clean_label(None), "Clean selected (1.0 KiB)");
    m.set("trash:0", true);
    assert_eq!(m.selected_bytes(None), before);
    m.set_category("trash", false);
    assert_eq!(m.selected_ids(None), Vec::<String>::new());
    m.set_category("trash", true);
    assert_eq!(m.selected_ids(None), ids(&["trash:0", "trash:1"]));
}

#[test]
fn filter_limits_selection() {
    let m = ReviewModel::new(plan(
        vec![entry("caches:0", "caches", 10, true), entry("trash:0", "trash", 20, true)],
        vec![],
        vec![],
    ));
    assert_eq!(m.selected_ids(Some("caches")), ids(&["caches:0"]));
    assert_eq!(m.selected_bytes(Some("caches")), 10);
    let groups = m.groups(Some("caches"));
    assert_eq!(groups.len(), 1);
    assert_eq!(groups[0].category, "caches");
    assert_eq!(groups[0].label, "Tool caches");
    assert_eq!(m.groups(None).len(), 2);
}

#[test]
fn report_only_cannot_be_selected() {
    let mut m = ReviewModel::new(plan(
        vec![entry("trash:0", "trash", 10, true)],
        vec![entry("idle_folders:0", "idle_folders", 999, false)],
        vec![],
    ));
    m.set("idle_folders:0", true);
    m.set_category("idle_folders", true);
    assert_eq!(m.selected_ids(None), ids(&["trash:0"]));
    let groups = m.groups(Some("idle_folders"));
    assert_eq!(groups.len(), 1);
    assert!(groups[0].rows.is_empty());
    assert_eq!(groups[0].report_only.len(), 1);
    assert!(!groups[0].report_only[0].selected);
}

#[test]
fn confirm_copy() {
    let m = ReviewModel::new(plan(
        vec![entry("trash:0", "trash", 1024, true), entry("trash:1", "trash", 1024, true)],
        vec![],
        vec![],
    ));
    assert_eq!(
        m.confirm(None),
        Confirm {
            heading: "Delete 2 items?".into(),
            body: "2.0 KiB will be deleted now. This cannot be undone.".into(),
        }
    );

    let m = ReviewModel::new(plan(
        vec![entry("trash:0", "trash", 1024, true), store_entry("caches:0", 1024)],
        vec![],
        vec![],
    ));
    let c = m.confirm(None);
    assert_eq!(c.heading, "Delete 2 items?");
    assert!(c.body.ends_with(" Cache rule sizes are estimates."), "{}", c.body);
    // Filtered to trash, no store rule is included.
    assert!(!m.confirm(Some("trash")).body.contains("estimates"));
}

#[test]
fn rows_mark_estimate_and_permanent() {
    let permanent = Entry { permanent: true, ..entry("trash:0", "trash", 10, true) };
    let m = ReviewModel::new(plan(
        vec![store_entry("caches:0", 100), permanent, entry("trash:1", "trash", 5, true)],
        vec![],
        vec![],
    ));
    let groups = m.groups(None);
    let caches = groups.iter().find(|g| g.category == "caches").unwrap();
    assert!(caches.rows[0].estimate);
    assert!(!caches.rows[0].permanent);
    assert!(!caches.permanent);
    let trash = groups.iter().find(|g| g.category == "trash").unwrap();
    assert!(trash.permanent);
    assert!(trash.rows[0].permanent);
    assert!(!trash.rows[0].estimate);
    assert!(!trash.rows[1].permanent);
}

#[test]
fn skipped_grouped_by_category() {
    let skip = Skip { category: "trash".into(), title: "x".into(), reason: "git-tracked".into() };
    let m = ReviewModel::new(plan(
        vec![entry("trash:0", "trash", 10, true), entry("caches:0", "caches", 10, true)],
        vec![],
        vec![skip.clone()],
    ));
    let groups = m.groups(None);
    let trash = groups.iter().find(|g| g.category == "trash").unwrap();
    assert_eq!(trash.skipped, vec![skip]);
    let caches = groups.iter().find(|g| g.category == "caches").unwrap();
    assert!(caches.skipped.is_empty());
}

#[test]
fn operation_row_is_estimate_and_confirm_copy() {
    use cleaner::object::Operation;
    let op = Entry {
        kind: EntryKind::Operation { op: Operation::UvCachePrune },
        ..entry("caches:1", "caches", 1024, true)
    };
    let m = ReviewModel::new(plan(vec![entry("trash:0", "trash", 1024, true), op.clone()], vec![], vec![]));
    let groups = m.groups(None);
    let caches = groups.iter().find(|g| g.category == "caches").unwrap();
    assert!(caches.rows[0].estimate);
    let trash = groups.iter().find(|g| g.category == "trash").unwrap();
    assert!(!trash.rows[0].estimate);
    assert_eq!(
        m.confirm(None).body,
        "2.0 KiB will be deleted now. This cannot be undone. Tool operation sizes are estimates."
    );

    let m = ReviewModel::new(plan(
        vec![entry("trash:0", "trash", 1024, true), op, store_entry("caches:0", 1024)],
        vec![],
        vec![],
    ));
    let body = m.confirm(None).body;
    assert!(
        body.contains(" Cache rule sizes are estimates. Tool operation sizes are estimates."),
        "{body}"
    );
}

#[test]
fn password_suffix() {
    use cleaner::object::{Object, Operation};
    let sys = |selected| Entry {
        kind: EntryKind::Object {
            obj: Object::NixGenerations { profile: "/p/system".into(), gens: vec![(1, "/nix/store/x".into())], root: true },
        },
        ..entry("nix:0", "nix", 1024, selected)
    };
    let gc = Entry {
        kind: EntryKind::Operation { op: Operation::NixStoreGc },
        ..entry("nix:1", "nix", 1024, true)
    };
    let m = ReviewModel::new(plan(vec![sys(false), gc.clone()], vec![], vec![]));
    assert!(!m.confirm(None).body.contains("password"));

    let m = ReviewModel::new(plan(vec![sys(true), gc], vec![], vec![]));
    assert!(
        m.confirm(None).body.ends_with(
            " Tool operation and Nix closure sizes are estimates. You'll be asked for your password for the system generations."
        ),
        "{}",
        m.confirm(None).body
    );
}

#[test]
fn nix_closure_rows_are_estimates_in_confirm() {
    use cleaner::object::Object;
    let sys = Entry {
        kind: EntryKind::Object {
            obj: Object::NixGenerations { profile: "/p/system".into(), gens: vec![(1, "/nix/store/x".into())], root: true },
        },
        ..entry("nix:0", "nix", 1024, true)
    };
    let m = ReviewModel::new(plan(vec![sys], vec![], vec![]));
    let groups = m.groups(None);
    assert!(groups[0].rows[0].estimate);
    assert_eq!(
        m.confirm(None).body,
        "About 1.0 KiB will be deleted now. This cannot be undone. Nix closure sizes are estimates. \
         You'll be asked for your password for the system generations."
    );
}

/// An artifacts entry titled `title` whose targets were last touched at `mtimes`.
fn aged(id: &str, title: &str, bytes: u64, mtimes: &[i64]) -> Entry {
    let targets = mtimes
        .iter()
        .map(|&m| Target { path: format!("/p/{id}").into(), fp: Fingerprint { dev: 1, ino: 1, newest_mtime: m } })
        .collect();
    Entry { title: title.into(), kind: EntryKind::Item { targets }, ..entry(id, "artifacts", bytes, true) }
}

fn titles(rows: &[cleaner::view::review::Row]) -> Vec<String> {
    rows.iter().map(|r| r.title.clone()).collect()
}

#[test]
fn groups_sort_by_size_name_or_age() {
    let op = Entry {
        title: "delta".into(),
        kind: EntryKind::Operation { op: Operation::UvCachePrune },
        ..entry("a:3", "artifacts", 5, true)
    };
    let mut m = ReviewModel::new(plan(
        vec![
            aged("a:0", "beta", 10, &[50]),
            aged("a:1", "Alpha", 30, &[100]),
            // Its age is its newest target's.
            aged("a:2", "gamma", 20, &[10, 200]),
            op,
        ],
        vec![aged("r:0", "small", 1, &[0]), aged("r:1", "big", 9, &[0])],
        vec![],
    ));
    assert_eq!(m.sort(), Sort::Size, "largest first by default");
    let g = &m.groups(None)[0];
    assert_eq!(titles(&g.rows), ["Alpha", "gamma", "beta", "delta"]);
    assert_eq!(titles(&g.report_only), ["big", "small"]);
    m.set_sort(Sort::Name);
    assert_eq!(titles(&m.groups(None)[0].rows), ["Alpha", "beta", "delta", "gamma"], "A-Z, ignoring case");
    m.set_sort(Sort::Age);
    assert_eq!(titles(&m.groups(None)[0].rows), ["beta", "Alpha", "gamma", "delta"], "oldest first, no age last");
}

/// A category's own idea of when a thing was last used (a project's last
/// edit or commit) wins over its folders' mtimes.
#[test]
fn age_sort_prefers_last_active() {
    let fresh_folder_old_project = Entry { last_active: Some(10), ..aged("a:0", "old project", 1, &[900]) };
    let mut m = ReviewModel::new(plan(vec![aged("a:1", "newer", 1, &[500]), fresh_folder_old_project], vec![], vec![]));
    m.set_sort(Sort::Age);
    assert_eq!(titles(&m.groups(None)[0].rows), ["old project", "newer"]);
}

#[test]
fn sort_is_remembered_in_the_state_dir() {
    let dir = tempfile::TempDir::new().unwrap();
    assert_eq!(Sort::load(dir.path()), Sort::Size, "nothing saved yet");
    Sort::Age.save(dir.path()).unwrap();
    assert_eq!(Sort::load(dir.path()), Sort::Age);
    std::fs::write(dir.path().join("review_sort"), "sideways").unwrap();
    assert_eq!(Sort::load(dir.path()), Sort::Size, "an unknown value falls back");
}
