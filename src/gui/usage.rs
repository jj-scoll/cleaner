//! The disk map page: one level of the saved usage index. Read-only: no
//! selection and no delete control (spec §7.2).

use adw::prelude::*;
use std::cell::RefCell;
use std::path::PathBuf;
use std::rc::Rc;

use crate::fmt::human_bytes;
use crate::view::usage::{Level, Row, RowKind};

type Callback<A> = RefCell<Option<Rc<dyn Fn(A)>>>;

/// The page's outputs. Widgets hold this; it never holds a widget, so there is no cycle.
#[derive(Default)]
struct Callbacks {
    on_open: Callback<PathBuf>,
    on_badge: Callback<()>,
    on_rescan: Callback<()>,
    on_open_in_files: Callback<PathBuf>,
}

fn fire<A>(cbs: &Rc<Callbacks>, pick: impl Fn(&Callbacks) -> &Callback<A>, arg: A) {
    let f = pick(cbs).borrow().clone();
    if let Some(f) = f {
        f(arg);
    }
}

pub struct UsagePage {
    page: adw::NavigationPage,
    cbs: Rc<Callbacks>,
}

impl UsagePage {
    /// `None` = no saved index: a prompt to scan.
    pub fn new(level: Option<&Level>) -> UsagePage {
        let cbs = Rc::new(Callbacks::default());
        let header = adw::HeaderBar::new();
        let rescan = gtk::Button::with_label("Rescan");
        let c = cbs.clone();
        rescan.connect_clicked(move |_| fire(&c, |c| &c.on_rescan, ()));
        header.pack_end(&rescan);
        if let Some(path) = level.and_then(|l| l.path.clone()) {
            let open_files = gtk::Button::with_label("Open in Files");
            let c = cbs.clone();
            open_files.connect_clicked(move |_| fire(&c, |c| &c.on_open_in_files, path.clone()));
            header.pack_end(&open_files);
        }

        let content: gtk::Widget = match level {
            None => {
                let scan = gtk::Button::builder().label("Scan now").halign(gtk::Align::Center).build();
                scan.add_css_class("pill");
                scan.add_css_class("suggested-action");
                let c = cbs.clone();
                scan.connect_clicked(move |_| fire(&c, |c| &c.on_rescan, ()));
                adw::StatusPage::builder()
                    .title("No disk index yet. Run a scan.")
                    .icon_name("drive-harddisk-symbolic")
                    .child(&scan)
                    .vexpand(true)
                    .build()
                    .upcast()
            }
            Some(l) => level_content(l, &cbs),
        };
        let toolbar = adw::ToolbarView::new();
        toolbar.add_top_bar(&header);
        toolbar.set_content(Some(&content));
        let title = level.map_or("What's using my disk", |l| l.title.as_str());
        let page = adw::NavigationPage::builder().title(title).child(&toolbar).build();
        UsagePage { page, cbs }
    }

    pub fn page(&self) -> adw::NavigationPage {
        self.page.clone()
    }

    /// A drillable row was clicked.
    pub fn connect_open(&self, f: impl Fn(PathBuf) + 'static) {
        *self.cbs.on_open.borrow_mut() = Some(Rc::new(f));
    }

    /// A row's `in plan` badge was clicked: open the review.
    pub fn connect_badge(&self, f: impl Fn() + 'static) {
        *self.cbs.on_badge.borrow_mut() = Some(Rc::new(move |()| f()));
    }

    pub fn connect_rescan(&self, f: impl Fn() + 'static) {
        *self.cbs.on_rescan.borrow_mut() = Some(Rc::new(move |()| f()));
    }

    pub fn connect_open_in_files(&self, f: impl Fn(PathBuf) + 'static) {
        *self.cbs.on_open_in_files.borrow_mut() = Some(Rc::new(f));
    }
}

fn dim(text: &str) -> gtk::Label {
    gtk::Label::builder().label(text).xalign(0.0).wrap(true).css_classes(["dim-label", "caption"]).build()
}

fn level_content(l: &Level, cbs: &Rc<Callbacks>) -> gtk::Widget {
    let body = gtk::Box::builder()
        .orientation(gtk::Orientation::Vertical)
        .spacing(12)
        .margin_top(24)
        .margin_bottom(24)
        .margin_start(12)
        .margin_end(12)
        .build();
    body.append(&dim(&l.scanned));
    let group = adw::PreferencesGroup::new();
    for r in &l.rows {
        group.add(&row_widget(r, cbs));
    }
    body.append(&group);
    for (root, bytes) in &l.unexplained {
        body.append(&dim(&format!("{root}: {} on the filesystem is not in this map", human_bytes(*bytes))));
    }
    let clamp = adw::Clamp::builder().maximum_size(860).child(&body).build();
    gtk::ScrolledWindow::builder()
        .hscrollbar_policy(gtk::PolicyType::Never)
        .child(&clamp)
        .vexpand(true)
        .build()
        .upcast()
}

fn row_widget(r: &Row, cbs: &Rc<Callbacks>) -> adw::ActionRow {
    let subtitle = match &r.kind {
        RowKind::Unreadable { owner } => format!("unreadable (owned by {owner})"),
        _ => String::new(),
    };
    let row = adw::ActionRow::builder().title(&r.name).subtitle(&subtitle).use_markup(false).build();
    if let Some(badge) = &r.badge {
        let b = gtk::Button::builder().label(badge).valign(gtk::Align::Center).css_classes(["flat", "caption"]).build();
        let c = cbs.clone();
        b.connect_clicked(move |_| fire(&c, |c| &c.on_badge, ()));
        row.add_suffix(&b);
    }
    let bar = gtk::LevelBar::builder()
        .min_value(0.0)
        .max_value(1.0)
        .value(r.fraction.clamp(0.0, 1.0))
        .valign(gtk::Align::Center)
        .width_request(120)
        .build();
    for name in ["low", "high", "full"] {
        bar.remove_offset_value(Some(name));
    }
    row.add_suffix(&bar);
    if !matches!(r.kind, RowKind::Unreadable { .. }) {
        row.add_suffix(&gtk::Label::builder().label(human_bytes(r.bytes)).css_classes(["numeric"]).build());
    }
    if let (RowKind::Dir { drillable: true }, Some(path)) = (&r.kind, &r.path) {
        row.set_activatable(true);
        row.add_suffix(&gtk::Image::from_icon_name("go-next-symbolic"));
        let (c, path) = (cbs.clone(), path.clone());
        row.connect_activated(move |_| fire(&c, |c| &c.on_open, path.clone()));
    }
    row
}
