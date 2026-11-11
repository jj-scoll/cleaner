//! Helpers for `tests/gui.rs`: read what a widget tree shows.

use adw::prelude::*;
use gtk::glib;

/// Depth-first, every non-empty Label text, Button label, and row title and
/// subtitle (`adw::ActionRow`, `adw::ExpanderRow`, other `adw::PreferencesRow`
/// titles), plus `adw::StatusPage` titles.
pub fn texts(w: &impl IsA<gtk::Widget>) -> Vec<String> {
    let mut out = Vec::new();
    walk(w.upcast_ref(), &mut out);
    out
}

fn walk(w: &gtk::Widget, out: &mut Vec<String>) {
    let mut push = |s: glib::GString| {
        if !s.is_empty() {
            out.push(s.to_string());
        }
    };
    if let Some(l) = w.downcast_ref::<gtk::Label>() {
        push(l.text());
    } else if let Some(b) = w.downcast_ref::<gtk::Button>() {
        if let Some(l) = b.label() {
            push(l);
        }
    } else if let Some(r) = w.downcast_ref::<adw::ActionRow>() {
        push(r.title());
        push(r.subtitle().unwrap_or_default());
    } else if let Some(r) = w.downcast_ref::<adw::ExpanderRow>() {
        push(r.title());
        push(r.subtitle());
    } else if let Some(r) = w.downcast_ref::<adw::PreferencesRow>() {
        push(r.title());
    } else if let Some(s) = w.downcast_ref::<adw::StatusPage>() {
        push(s.title());
    }
    let mut child = w.first_child();
    while let Some(c) = child {
        walk(&c, out);
        child = c.next_sibling();
    }
}

/// A real window over temp `paths`, for tests that drive `App` methods.
pub struct TestApp {
    app: std::rc::Rc<super::App>,
}

impl TestApp {
    pub fn new(paths: crate::paths::Paths) -> TestApp {
        let gapp = adw::Application::builder()
            .application_id("dev.cleaner.test")
            .flags(gtk::gio::ApplicationFlags::NON_UNIQUE)
            .build();
        gapp.register(gtk::gio::Cancellable::NONE).expect("register");
        TestApp { app: super::App::start(&gapp, paths, None) }
    }

    /// `App::open_usage`: the disk map level for `path` (roots for `None`).
    pub fn open_usage(&self, path: Option<std::path::PathBuf>) {
        self.app.open_usage(path);
    }

    /// The navigation stack, bottom first.
    pub fn pages(&self) -> Vec<adw::NavigationPage> {
        let stack = self.app.nav.navigation_stack();
        (0..stack.n_items()).filter_map(|i| stack.item(i).and_downcast()).collect()
    }
}
