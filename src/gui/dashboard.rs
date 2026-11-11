//! The dashboard page (mockup A): disk cards, the latest-plan banner, the
//! plan's categories and the last run.

use adw::prelude::*;
use gtk::gio;
use std::cell::RefCell;
use std::rc::{Rc, Weak};

use crate::engine::Progress;
use crate::fmt::human_bytes;
use crate::view::category_label;
use crate::view::dashboard::{Banner, Dashboard};

type Callback<A> = RefCell<Option<Rc<dyn Fn(A)>>>;

struct Inner {
    page: adw::NavigationPage,
    scan_button: gtk::Button,
    actions: gio::SimpleActionGroup,
    progress_box: gtk::Box,
    progress_label: gtk::Label,
    progress_bar: gtk::ProgressBar,
    disks: gtk::Box,
    banner: gtk::Box,
    categories: gtk::Box,
    last_run: gtk::Label,
    on_review: Callback<Option<String>>,
    on_clean_all: Callback<()>,
    on_scan: Callback<()>,
    on_disk: Callback<()>,
}

#[derive(Clone)]
pub struct DashboardPage {
    inner: Rc<Inner>,
}

fn call<A>(inner: &Weak<Inner>, pick: impl Fn(&Inner) -> &Callback<A>, arg: A) {
    let Some(inner) = inner.upgrade() else { return };
    let f = pick(&inner).borrow().clone();
    if let Some(f) = f {
        f(arg);
    }
}

fn label(text: &str, classes: &[&str]) -> gtk::Label {
    let l = gtk::Label::builder().label(text).xalign(0.0).wrap(true).build();
    for c in classes {
        l.add_css_class(c);
    }
    l
}

fn clear(b: &gtk::Box) {
    while let Some(c) = b.first_child() {
        b.remove(&c);
    }
}

impl Default for DashboardPage {
    fn default() -> Self {
        Self::new()
    }
}

impl DashboardPage {
    pub fn new() -> DashboardPage {
        let scan_button = gtk::Button::with_label("Scan now");
        let menu = gio::Menu::new();
        menu.append(Some("History"), Some("dash.history"));
        menu.append(Some("Settings"), Some("dash.settings"));
        menu.append(Some("Disk map"), Some("dash.disk"));
        let menu_button = gtk::MenuButton::builder()
            .icon_name("open-menu-symbolic")
            .menu_model(&menu)
            .tooltip_text("Menu")
            .build();
        let header = adw::HeaderBar::new();
        header.pack_start(&scan_button);
        header.pack_end(&menu_button);

        let actions = gio::SimpleActionGroup::new();
        for name in ["history", "settings", "disk"] {
            let a = gio::SimpleAction::new(name, None);
            a.set_enabled(false);
            actions.add_action(&a);
        }

        let progress_label = label("", &["dim-label"]);
        let progress_bar = gtk::ProgressBar::new();
        let progress_box = gtk::Box::new(gtk::Orientation::Vertical, 6);
        progress_box.append(&progress_label);
        progress_box.append(&progress_bar);
        progress_box.set_visible(false);

        let disks = gtk::Box::builder().spacing(12).homogeneous(true).build();
        let disk_button = gtk::Button::builder().label("What's using my disk").halign(gtk::Align::Start).build();
        disk_button.add_css_class("flat");
        let banner = gtk::Box::new(gtk::Orientation::Vertical, 0);
        let categories = gtk::Box::new(gtk::Orientation::Vertical, 0);
        let last_run = label("", &["dim-label", "caption"]);
        last_run.set_visible(false);

        let content = gtk::Box::builder()
            .orientation(gtk::Orientation::Vertical)
            .spacing(24)
            .margin_top(24)
            .margin_bottom(24)
            .margin_start(12)
            .margin_end(12)
            .build();
        for w in [
            progress_box.upcast_ref::<gtk::Widget>(),
            disks.upcast_ref(),
            disk_button.upcast_ref(),
            banner.upcast_ref(),
            categories.upcast_ref(),
            last_run.upcast_ref(),
        ] {
            content.append(w);
        }
        let clamp = adw::Clamp::builder().maximum_size(860).child(&content).build();
        let scrolled = gtk::ScrolledWindow::builder()
            .hscrollbar_policy(gtk::PolicyType::Never)
            .child(&clamp)
            .vexpand(true)
            .build();
        let toolbar = adw::ToolbarView::new();
        toolbar.add_top_bar(&header);
        toolbar.set_content(Some(&scrolled));
        let page = adw::NavigationPage::builder().title("Cleaner").tag("dashboard").child(&toolbar).build();
        page.insert_action_group("dash", Some(&actions));

        let inner = Rc::new(Inner {
            page,
            scan_button: scan_button.clone(),
            actions,
            progress_box,
            progress_label,
            progress_bar,
            disks,
            banner,
            categories,
            last_run,
            on_review: RefCell::new(None),
            on_clean_all: RefCell::new(None),
            on_scan: RefCell::new(None),
            on_disk: RefCell::new(None),
        });
        let weak = Rc::downgrade(&inner);
        scan_button.connect_clicked(move |_| call(&weak, |i| &i.on_scan, ()));
        let weak = Rc::downgrade(&inner);
        disk_button.connect_clicked(move |_| call(&weak, |i| &i.on_disk, ()));
        DashboardPage { inner }
    }

    pub fn page(&self) -> adw::NavigationPage {
        self.inner.page.clone()
    }

    /// Rebuilds the disk cards, banner, category rows and last-run line.
    pub fn update(&self, d: &Dashboard) {
        let i = &self.inner;
        let weak = Rc::downgrade(i);

        clear(&i.disks);
        for fs in &d.filesystems {
            let card = gtk::Box::new(gtk::Orientation::Vertical, 6);
            card.add_css_class("card");
            card.add_css_class("disk-card");
            let pct = label(&format!("{:.0}%", fs.used_fraction * 100.0), &["title-1", fs.level.css_class()]);
            let bar = gtk::LevelBar::builder().min_value(0.0).max_value(1.0).value(fs.used_fraction).build();
            for name in ["low", "high", "full"] {
                bar.remove_offset_value(Some(name));
            }
            bar.add_css_class(fs.level.css_class());
            card.append(&label(&fs.label, &["heading"]));
            card.append(&pct);
            card.append(&bar);
            card.append(&label(&fs.free_text, &["dim-label", "caption"]));
            i.disks.append(&card);
        }

        clear(&i.banner);
        let card = gtk::Box::new(gtk::Orientation::Vertical, 6);
        card.add_css_class("card");
        card.add_css_class("plan-banner");
        let buttons = gtk::Box::builder().spacing(12).margin_top(12).build();
        match &d.banner {
            Banner::Ready { title, subtitle, .. } => {
                card.add_css_class("ready");
                card.append(&label(title, &["title-2"]));
                card.append(&label(subtitle, &["dim-label"]));
                let review = gtk::Button::with_label("Review plan");
                review.add_css_class("suggested-action");
                review.add_css_class("pill");
                let w = weak.clone();
                review.connect_clicked(move |_| call(&w, |i| &i.on_review, None));
                let clean = gtk::Button::with_label("Clean all");
                clean.add_css_class("destructive-action");
                clean.add_css_class("pill");
                let w = weak.clone();
                clean.connect_clicked(move |_| call(&w, |i| &i.on_clean_all, ()));
                buttons.append(&review);
                buttons.append(&clean);
            }
            Banner::NoPlan | Banner::Nothing => {
                let title = if d.banner == Banner::NoPlan { "No plan yet" } else { "Nothing to clean right now" };
                card.append(&label(title, &["title-2"]));
                let scan = gtk::Button::with_label("Scan now");
                scan.add_css_class("pill");
                let w = weak.clone();
                scan.connect_clicked(move |_| call(&w, |i| &i.on_scan, ()));
                buttons.append(&scan);
            }
        }
        card.append(&buttons);
        i.banner.append(&card);

        clear(&i.categories);
        if !d.categories.is_empty() {
            let group = adw::PreferencesGroup::builder().title("In the plan").build();
            for c in &d.categories {
                let subtitle = if c.selected_count == 0 && c.report_only_count > 0 {
                    "report only".to_string()
                } else {
                    format!("{} selected", c.selected_count)
                };
                let row = adw::ActionRow::builder()
                    .title(&c.label)
                    .subtitle(&subtitle)
                    .use_markup(false)
                    .activatable(true)
                    .build();
                row.add_suffix(&gtk::Label::builder().label(human_bytes(c.selected_bytes)).css_classes(["numeric"]).build());
                row.add_suffix(&gtk::Image::from_icon_name("go-next-symbolic"));
                let w = weak.clone();
                let id = c.id.clone();
                row.connect_activated(move |_| call(&w, |i| &i.on_review, Some(id.clone())));
                group.add(&row);
            }
            i.categories.append(&group);
        }

        i.last_run.set_label(d.last_run.as_deref().unwrap_or(""));
        i.last_run.set_visible(d.last_run.is_some());
    }

    /// `f(None)` reviews the whole plan; `f(Some(category))` one category.
    pub fn connect_review(&self, f: impl Fn(Option<String>) + 'static) {
        *self.inner.on_review.borrow_mut() = Some(Rc::new(f));
    }

    pub fn connect_clean_all(&self, f: impl Fn() + 'static) {
        *self.inner.on_clean_all.borrow_mut() = Some(Rc::new(move |()| f()));
    }

    pub fn connect_scan(&self, f: impl Fn() + 'static) {
        *self.inner.on_scan.borrow_mut() = Some(Rc::new(move |()| f()));
    }

    /// The `What's using my disk` button under the filesystem bars.
    pub fn connect_disk(&self, f: impl Fn() + 'static) {
        *self.inner.on_disk.borrow_mut() = Some(Rc::new(move |()| f()));
    }

    /// Shows scan progress and makes the buttons insensitive; `None` = idle.
    pub fn set_busy(&self, p: Option<&Progress>) {
        let i = &self.inner;
        let busy = p.is_some();
        i.progress_box.set_visible(busy);
        for w in [i.scan_button.upcast_ref::<gtk::Widget>(), i.banner.upcast_ref(), i.categories.upcast_ref()] {
            w.set_sensitive(!busy);
        }
        let Some(p) = p else { return };
        let text = if p.total > 0 && p.done == p.total {
            "Saving plan…".to_string()
        } else if p.current.is_empty() {
            "Scanning…".to_string()
        } else {
            format!("Scanning {}…", category_label(&p.current))
        };
        i.progress_label.set_label(&text);
        let fraction = if p.total == 0 { 0.0 } else { p.done as f64 / p.total as f64 };
        i.progress_bar.set_fraction(fraction);
    }

    /// Enables the menu's `name` item ("history", "settings" or "disk") and runs `f`
    /// when it is chosen.
    pub fn connect_menu(&self, name: &str, f: impl Fn() + 'static) {
        if let Some(a) = self.inner.actions.lookup_action(name).and_downcast::<gio::SimpleAction>() {
            a.set_enabled(true);
            a.connect_activate(move |_, _| f());
        }
    }
}
