//! The review page (mockup option B): one group per category with check
//! buttons, skipped and report-only rows, and "Clean selected" behind one
//! confirmation.

use adw::prelude::*;
use std::cell::{Cell, RefCell};
use std::rc::{Rc, Weak};

use crate::category::store::{StoreRule, top_matches};
use crate::fmt::human_bytes;
use crate::plan::EntryKind;
use crate::view::category_label;
use crate::view::review::{Confirm, ReviewModel, Row, Sort};

struct Inner {
    clean_button: gtk::Button,
    model: Rc<RefCell<ReviewModel>>,
    filter: Option<String>,
    /// (entry id, its check button)
    row_checks: RefCell<Vec<(String, gtk::CheckButton)>>,
    /// (category id, its group-level check button)
    group_checks: RefCell<Vec<(String, gtk::CheckButton)>>,
    /// Set while `sync` writes check states, so their handlers don't react.
    syncing: Cell<bool>,
    on_clean: RefCell<Option<OnClean>>,
    groups_box: gtk::Box,
    on_sort: RefCell<Option<OnSort>>,
}

type OnClean = Rc<dyn Fn(Vec<String>)>;
type OnSort = Box<dyn Fn(Sort)>;

/// Owns the page widget; the page owns `Inner` (see `new`), so dropping this
/// handle after pushing the page is fine.
pub struct ReviewPage {
    page: adw::NavigationPage,
    inner: Rc<Inner>,
}

/// The confirmation dialog for `c`: `Cancel` and a destructive `Delete`
/// (response id "delete").
pub(crate) fn confirm_dialog(c: &Confirm) -> adw::AlertDialog {
    let d = adw::AlertDialog::new(Some(&c.heading), Some(&c.body));
    d.add_response("cancel", "Cancel");
    d.add_response("delete", "Delete");
    d.set_response_appearance("delete", adw::ResponseAppearance::Destructive);
    d.set_default_response(Some("cancel"));
    d.set_close_response("cancel");
    d
}

fn tag(text: &str, class: Option<&str>) -> gtk::Label {
    let l = gtk::Label::builder().label(text).valign(gtk::Align::Center).css_classes(["tag"]).build();
    if let Some(c) = class {
        l.add_css_class(c);
    }
    l
}

fn bytes_label(bytes: u64, estimate: bool) -> gtk::Label {
    let text = if estimate { format!("~{}", human_bytes(bytes)) } else { human_bytes(bytes) };
    gtk::Label::builder().label(text).css_classes(["numeric"]).build()
}

fn subtitle(r: &Row) -> String {
    match &r.note {
        Some(note) => format!("{}\n{note}", r.reason),
        None => r.reason.clone(),
    }
}

/// Re-applies the model's selection to every check button and the Clean button.
fn sync(i: &Inner) {
    i.syncing.set(true);
    {
        let m = i.model.borrow();
        let selected = m.selected_ids(None);
        for (id, check) in i.row_checks.borrow().iter() {
            check.set_active(selected.contains(id));
        }
        for (category, check) in i.group_checks.borrow().iter() {
            let ids: Vec<&String> =
                m.plan().entries.iter().filter(|e| &e.category == category).map(|e| &e.id).collect();
            let on = ids.iter().filter(|id| selected.contains(id)).count();
            check.set_active(on == ids.len());
            check.set_inconsistent(on > 0 && on < ids.len());
        }
        let filter = i.filter.as_deref();
        i.clean_button.set_label(&m.clean_label(filter));
        i.clean_button.set_sensitive(m.selected_bytes(filter) > 0);
    }
    i.syncing.set(false);
}

fn row_check(weak: &Weak<Inner>, id: &str, active: bool) -> gtk::CheckButton {
    let check = gtk::CheckButton::builder().active(active).valign(gtk::Align::Center).build();
    let (weak, id) = (weak.clone(), id.to_string());
    check.connect_toggled(move |c| {
        let Some(i) = weak.upgrade() else { return };
        if i.syncing.get() {
            return;
        }
        i.model.borrow_mut().set(&id, c.is_active());
        sync(&i);
    });
    check
}

/// Fills `row` with the store's largest matching units, computed off the
/// main thread with the current config.
fn load_top_matches(row: &adw::ExpanderRow, rule: StoreRule) {
    let placeholder = adw::ActionRow::builder().title("Loading…").build();
    row.add_row(&placeholder);
    let row = row.clone();
    super::worker::run(
        move |_| {
            let paths = crate::paths::Paths::from_env()?;
            let (config, _) = crate::config::load(&paths.config_file)?;
            let protector = crate::protect::Protector::new(&paths.home, &config.protect);
            let now = jiff::Timestamp::now().as_second();
            let ctx = crate::category::ScanCtx {
                home: &paths.home,
                now,
                config: &config,
                protector: &protector,
                tools: &crate::tool::SystemRunner,
                euid: rustix::process::geteuid().as_raw(),
                usage: None,
            };
            Ok((now, top_matches(&rule, &ctx, 20)))
        },
        |_| {},
        move |result| {
            row.remove(&placeholder);
            match result {
                Ok((_, matches)) if matches.is_empty() => {
                    row.add_row(&adw::ActionRow::builder().title("Nothing matches the rule now").build());
                }
                Ok((now, matches)) => {
                    for m in matches {
                        let days = (now - m.newest_mtime).max(0) / 86_400;
                        let child = adw::ActionRow::builder()
                            .title(m.path.display().to_string())
                            .subtitle(format!("{days} days old"))
                            .use_markup(false)
                            .build();
                        child.add_suffix(&bytes_label(m.bytes, false));
                        row.add_row(&child);
                    }
                }
                Err(e) => {
                    let child = adw::ActionRow::builder().title(format!("{e:#}")).use_markup(false).build();
                    row.add_row(&child);
                }
            }
        },
    );
}

/// (Re)builds the groups in the model's sort order, replacing any already shown.
fn fill(inner: &Rc<Inner>) {
    let groups_box = &inner.groups_box;
    while let Some(child) = groups_box.first_child() {
        groups_box.remove(&child);
    }
    inner.row_checks.borrow_mut().clear();
    inner.group_checks.borrow_mut().clear();
    let weak = Rc::downgrade(inner);
    let groups = inner.model.borrow().groups(inner.filter.as_deref());
        for g in groups {
            let group = adw::PreferencesGroup::builder().title(&g.label).build();
            let header = gtk::Box::builder().spacing(12).valign(gtk::Align::Center).build();
            if g.permanent {
                header.append(&tag("permanent", Some("error")));
            }
            if !g.rows.is_empty() {
                let check = gtk::CheckButton::builder().tooltip_text("Select all").build();
                let (w, category) = (weak.clone(), g.category.clone());
                check.connect_toggled(move |c| {
                    let Some(i) = w.upgrade() else { return };
                    if i.syncing.get() {
                        return;
                    }
                    i.model.borrow_mut().set_category(&category, c.is_active());
                    sync(&i);
                });
                inner.group_checks.borrow_mut().push((g.category.clone(), check.clone()));
                header.append(&check);
            }
            group.set_header_suffix(Some(&header));

            for r in &g.rows {
                let check = row_check(&weak, &r.id, r.selected);
                inner.row_checks.borrow_mut().push((r.id.clone(), check.clone()));
                let rule = match inner.model.borrow().plan().entries.iter().find(|e| e.id == r.id).map(|e| &e.kind) {
                    Some(EntryKind::StoreRule { rule }) => Some(rule.clone()),
                    _ => None,
                };
                match rule {
                    Some(rule) => {
                        let row = adw::ExpanderRow::builder()
                            .title(&r.title)
                            .subtitle(subtitle(r))
                            .use_markup(false)
                            .build();
                        row.add_prefix(&check);
                        // ExpanderRow prepends suffixes: add the tag first so it ends up after the bytes.
                        if r.estimate {
                            row.add_suffix(&tag("estimate", None));
                        }
                        row.add_suffix(&bytes_label(r.bytes, r.estimate));
                        let loaded = Cell::new(false);
                        row.connect_expanded_notify(move |row| {
                            if row.is_expanded() && !loaded.replace(true) {
                                load_top_matches(row, rule.clone());
                            }
                        });
                        group.add(&row);
                    }
                    None => {
                        let row = adw::ActionRow::builder()
                            .title(&r.title)
                            .subtitle(subtitle(r))
                            .use_markup(false)
                            .activatable_widget(&check)
                            .build();
                        row.add_prefix(&check);
                        row.add_suffix(&bytes_label(r.bytes, r.estimate));
                        if r.estimate {
                            row.add_suffix(&tag("estimate", None));
                        }
                        group.add(&row);
                    }
                }
            }
            for r in &g.report_only {
                let row = adw::ActionRow::builder().title(&r.title).subtitle(subtitle(r)).use_markup(false).build();
                row.add_suffix(&bytes_label(r.bytes, r.estimate));
                row.add_suffix(&tag("report only", None));
                group.add(&row);
            }
            for s in &g.skipped {
                let row = adw::ActionRow::builder()
                    .title(format!("⊘ {}", s.title))
                    .subtitle(&s.reason)
                    .use_markup(false)
                    .sensitive(false)
                    .build();
                group.add(&row);
            }
            groups_box.append(&group);
        }

}

impl ReviewPage {
    /// The plan's groups (one category when `filter` is set) with check
    /// buttons bound to `model`.
    pub fn new(model: Rc<RefCell<ReviewModel>>, filter: Option<String>) -> ReviewPage {
        let title = filter.as_deref().map_or("Review".to_string(), category_label);
        let clean_button = gtk::Button::builder().css_classes(["suggested-action", "pill"]).build();
        let groups_box = gtk::Box::builder()
            .orientation(gtk::Orientation::Vertical)
            .spacing(24)
            .margin_top(24)
            .margin_bottom(24)
            .margin_start(12)
            .margin_end(12)
            .build();
        let clamp = adw::Clamp::builder().maximum_size(860).child(&groups_box).build();
        let scrolled = gtk::ScrolledWindow::builder()
            .hscrollbar_policy(gtk::PolicyType::Never)
            .child(&clamp)
            .vexpand(true)
            .build();
        clean_button.set_halign(gtk::Align::End);
        clean_button.set_margin_start(12);
        clean_button.set_margin_end(12);
        // Same column (and inner margin) as the groups, so the button lines up with them.
        let bottom = adw::Clamp::builder()
            .maximum_size(860)
            .margin_top(12)
            .margin_bottom(12)
            .child(&clean_button)
            .build();
        let picker = gtk::DropDown::from_strings(&Sort::ALL.map(Sort::label));
        picker.set_selected(Sort::ALL.iter().position(|&s| s == model.borrow().sort()).unwrap_or(0) as u32);
        picker.set_tooltip_text(Some("Sort each category by size, name or age (oldest first)"));
        let sort_by = gtk::Box::builder().spacing(6).build();
        sort_by.append(&gtk::Label::new(Some("Sort by")));
        sort_by.append(&picker);
        let header = adw::HeaderBar::new();
        header.pack_end(&sort_by);
        let toolbar = adw::ToolbarView::new();
        toolbar.add_top_bar(&header);
        toolbar.set_content(Some(&scrolled));
        toolbar.add_bottom_bar(&bottom);
        let page = adw::NavigationPage::builder().title(&title).tag("review").child(&toolbar).build();

        let inner = Rc::new(Inner {
            clean_button: clean_button.clone(),
            model: model.clone(),
            filter: filter.clone(),
            row_checks: RefCell::new(Vec::new()),
            group_checks: RefCell::new(Vec::new()),
            syncing: Cell::new(false),
            on_clean: RefCell::new(None),
            groups_box,
            on_sort: RefCell::new(None),
        });
        let weak = Rc::downgrade(&inner);

        let w = weak.clone();
        picker.connect_selected_notify(move |p| {
            let Some(i) = w.upgrade() else { return };
            let Some(&sort) = Sort::ALL.get(p.selected() as usize) else { return };
            i.model.borrow_mut().set_sort(sort);
            fill(&i);
            sync(&i);
            if let Some(f) = i.on_sort.borrow().as_ref() {
                f(sort);
            }
        });

        fill(&inner);

        let w = weak.clone();
        clean_button.connect_clicked(move |button| {
            let Some(i) = w.upgrade() else { return };
            let (confirm, ids) = {
                let m = i.model.borrow();
                (m.confirm(i.filter.as_deref()), m.selected_ids(i.filter.as_deref()))
            };
            let dialog = confirm_dialog(&confirm);
            let w = w.clone();
            dialog.connect_response(Some("delete"), move |_, _| {
                let Some(i) = w.upgrade() else { return };
                let f = i.on_clean.borrow().clone();
                if let Some(f) = f {
                    f(ids.clone());
                }
            });
            dialog.present(Some(button));
        });

        sync(&inner);
        // The page keeps `Inner` alive; the widgets' handlers only hold Weak
        // refs, so nothing is leaked once the popped page is disposed.
        let keep = inner.clone();
        page.connect_destroy(move |_| {
            let _ = &keep;
        });
        ReviewPage { page, inner }
    }

    pub fn page(&self) -> adw::NavigationPage {
        self.page.clone()
    }

    pub fn clean_button(&self) -> gtk::Button {
        self.inner.clean_button.clone()
    }

    /// `f(sort)` runs when the user picks another sort order.
    pub fn connect_sort(&self, f: impl Fn(Sort) + 'static) {
        *self.inner.on_sort.borrow_mut() = Some(Box::new(f));
    }

    /// `f(ids)` runs after the confirm dialog's `Delete`, with the selected
    /// entry ids (of the filtered category, if any).
    pub fn connect_clean(&self, f: impl Fn(Vec<String>) + 'static) {
        *self.inner.on_clean.borrow_mut() = Some(Rc::new(f));
    }
}
