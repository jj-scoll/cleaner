//! The GTK4/libadwaita window (spec §7.2).

pub mod dashboard;
pub mod history;
pub mod review;
pub mod settings;
pub mod testing;
pub mod usage;
pub mod worker;

use adw::prelude::*;
use gtk::glib;
use jiff::tz::TimeZone;
use std::cell::{Cell, RefCell};
use std::path::PathBuf;
use std::rc::Rc;

use crate::category::Outcome;
use crate::config::{self, Config};
use crate::engine::{Progress, RunReport, apply_selected, filesystem_paths, plan_and_save};
use crate::fmt::human_bytes;
use crate::fsutil::usage::UsageIndex;
use crate::paths::Paths;
use crate::plan::{Plan, Requested, resolve_requested};
use crate::settings::ConfigDoc;
use crate::view::review::ReviewModel;
use crate::view::usage as view_usage;
use dashboard::DashboardPage;
use review::{ReviewPage, confirm_dialog};

/// Shown when the run lock is held (by the CLI, the timer or another window).
pub const LOCK_COPY: &str = "Another cleaner run is in progress. Try again when it finishes.";

const CSS: &str = "
.disk-card, .plan-banner { padding: 12px 16px; }
levelbar.success block.filled { background-color: @success_bg_color; }
levelbar.warning block.filled { background-color: @warning_bg_color; }
levelbar.error block.filled { background-color: @error_bg_color; }
label.tag { padding: 1px 8px; border-radius: 999px; font-size: smaller; font-weight: bold;
            background-color: alpha(currentColor, 0.12); }
";

/// Refuses to close `window` while `busy` is set, calling `on_blocked` instead.
/// Closing mid-clean would exit the process mid-delete, before the run's log
/// lines are written.
pub fn block_close_while(window: &impl IsA<gtk::Window>, busy: Rc<Cell<bool>>, on_blocked: impl Fn() + 'static) {
    window.connect_close_request(move |_| {
        if busy.get() {
            on_blocked();
            glib::Propagation::Stop
        } else {
            glib::Propagation::Proceed
        }
    });
}

/// Opens the window; returns the process exit code. `plan_id` is the plan to
/// review on startup (from a notification click).
pub fn run(plan_id: Option<String>) -> anyhow::Result<i32> {
    let paths = Paths::from_env()?;
    let app = adw::Application::builder()
        .application_id(crate::APP_ID)
        .flags(gtk::gio::ApplicationFlags::NON_UNIQUE)
        .build();
    // The desktop portal reports "no preference" here; prefer dark unless the
    // system explicitly asks for light (matches adw-gtk3-dark).
    app.connect_startup(|_| {
        adw::StyleManager::default().set_color_scheme(adw::ColorScheme::PreferDark);
        let css = gtk::CssProvider::new();
        css.load_from_data(CSS);
        if let Some(display) = gtk::gdk::Display::default() {
            gtk::style_context_add_provider_for_display(
                &display,
                &css,
                gtk::STYLE_PROVIDER_PRIORITY_APPLICATION,
            );
        }
    });
    app.connect_activate(move |app| {
        App::start(app, paths.clone(), plan_id.clone());
    });
    // Empty args: GTK must not parse cleaner's own argv.
    Ok(i32::from(app.run_with_args::<&str>(&[])))
}

/// One window's state.
struct App {
    window: adw::ApplicationWindow,
    toasts: adw::ToastOverlay,
    nav: adw::NavigationView,
    dashboard: DashboardPage,
    paths: Paths,
    /// The plan the dashboard shows.
    latest: RefCell<Option<Plan>>,
    /// Set while a clean runs; the window refuses to close.
    cleaning: Rc<Cell<bool>>,
}

/// `e:#`, or the lock copy when the run lock was held.
fn error_text(e: &anyhow::Error) -> String {
    let text = format!("{e:#}");
    if text.contains("another cleaner run is in progress") { LOCK_COPY.to_string() } else { text }
}

impl App {
    fn start(app: &adw::Application, paths: Paths, plan_id: Option<String>) -> Rc<App> {
        let dashboard = DashboardPage::new();
        let nav = adw::NavigationView::new();
        nav.add(&dashboard.page());
        let toasts = adw::ToastOverlay::new();
        toasts.set_child(Some(&nav));
        let window = adw::ApplicationWindow::builder()
            .application(app)
            .title("Cleaner")
            .default_width(900)
            .default_height(700)
            .content(&toasts)
            .build();
        let cleaning = Rc::new(Cell::new(false));
        block_close_while(
            &window,
            cleaning.clone(),
            glib::clone!(
                #[weak]
                toasts,
                move || toasts.add_toast(adw::Toast::new("Wait for the clean to finish"))
            ),
        );
        let this = Rc::new(App { window, toasts, nav, dashboard, paths, latest: RefCell::new(None), cleaning });

        // The page callbacks hold the App strongly: that cycle is what keeps
        // it alive for as long as the window (one window per process).
        let app = this.clone();
        this.dashboard.connect_scan(move || app.scan());
        let app = this.clone();
        this.dashboard.connect_review(move |filter| {
            let latest = app.latest.borrow().clone();
            if let Some(plan) = latest {
                app.open_review(plan, filter);
            }
        });
        let app = this.clone();
        this.dashboard.connect_clean_all(move || app.clean_all());
        let app = this.clone();
        this.dashboard.connect_menu("history", move || app.open_history());
        let app = this.clone();
        this.dashboard.connect_menu("settings", move || app.open_settings());
        let app = this.clone();
        this.dashboard.connect_menu("disk", move || app.open_usage(None));
        let app = this.clone();
        this.dashboard.connect_disk(move || app.open_usage(None));

        this.window.present();
        match config::write_default_if_missing(&this.paths.config_file)
            .and_then(|()| config::load(&this.paths.config_file))
        {
            Ok((_, warnings)) => warnings.iter().for_each(|w| this.toast(w)),
            Err(e) => this.alert("Can't load the config", &format!("{e:#}")),
        }
        this.refresh();
        match resolve_requested(&this.paths.plans_dir, plan_id.as_deref()) {
            Requested::Latest => {}
            Requested::Problem(m) => this.toast(&m),
            Requested::Plan(_, plan) => this.open_review(plan, None),
        }
        this
    }

    fn toast(&self, text: &str) {
        self.toasts.add_toast(adw::Toast::builder().title(text).use_markup(false).build());
    }

    fn alert(&self, heading: &str, body: &str) {
        let d = adw::AlertDialog::new(Some(heading), Some(body));
        d.add_response("close", "Close");
        d.present(Some(&self.window));
    }

    /// The config for a scan or apply, or `None` after showing why it can't be loaded.
    fn config(&self) -> Option<Config> {
        match config::load(&self.paths.config_file) {
            Ok((c, _)) => Some(c),
            Err(e) => {
                self.alert("Can't load the config", &format!("{e:#}"));
                None
            }
        }
    }

    /// Re-reads disks, the latest plan and the log, and redraws the dashboard.
    fn refresh(&self) {
        // Display only: fall back to the default filesystems if the config is broken.
        let config = config::load(&self.paths.config_file).map(|(c, _)| c).unwrap_or_default();
        let fs = crate::disk::usage(&filesystem_paths(&config, &self.paths.home));
        let latest = match Plan::latest(&self.paths.plans_dir).and_then(|p| p.map(|p| Plan::load(&p)).transpose()) {
            Ok(p) => p,
            Err(e) => {
                self.toast(&format!("{e:#}"));
                None
            }
        };
        let log = crate::log::read(&self.paths.log_file, None).unwrap_or_else(|e| {
            self.toast(&format!("{e:#}"));
            Vec::new()
        });
        let d = crate::view::dashboard::dashboard(&fs, latest.as_ref(), &log, &TimeZone::system());
        self.dashboard.update(&d);
        *self.latest.borrow_mut() = latest;
    }

    fn scan(self: &Rc<Self>) {
        let Some(config) = self.config() else { return };
        let paths = self.paths.clone();
        self.dashboard.set_busy(Some(&Progress { done: 0, total: 0, current: String::new() }));
        let (on_progress, on_done) = (Rc::downgrade(self), Rc::downgrade(self));
        worker::run(
            move |progress| {
                plan_and_save(&paths, &config, jiff::Timestamp::now().as_second(), progress).map(|_| ())
            },
            move |p| {
                if let Some(app) = on_progress.upgrade() {
                    app.dashboard.set_busy(Some(&p));
                }
            },
            move |result| {
                let Some(app) = on_done.upgrade() else { return };
                app.dashboard.set_busy(None);
                if let Err(e) = result {
                    app.alert("Scan failed", &error_text(&e));
                }
                app.refresh();
            },
        );
    }

    /// Pushes the review page for `plan` (one category when `filter` is set)
    /// and records the plan as shown.
    fn open_review(self: &Rc<Self>, plan: Plan, filter: Option<String>) {
        if let Err(e) = Plan::mark_shown(&self.paths.state_dir, &plan.id) {
            self.toast(&format!("{e:#}"));
        }
        let model = Rc::new(RefCell::new(ReviewModel::new(plan.clone())));
        let page = ReviewPage::new(model, filter);
        let app = Rc::downgrade(self);
        page.connect_clean(move |ids| {
            if let Some(app) = app.upgrade() {
                app.apply(plan.clone(), ids);
            }
        });
        self.nav.push(&page.page());
    }

    /// Every selected entry of the latest plan, after the same confirmation.
    fn clean_all(self: &Rc<Self>) {
        let Some(plan) = self.latest.borrow().clone() else { return };
        let model = ReviewModel::new(plan.clone());
        let ids = model.selected_ids(None);
        if ids.is_empty() {
            return;
        }
        let dialog = confirm_dialog(&model.confirm(None));
        let app = Rc::downgrade(self);
        dialog.connect_response(Some("delete"), move |_, _| {
            if let Some(app) = app.upgrade() {
                app.apply(plan.clone(), ids.clone());
            }
        });
        dialog.present(Some(&self.window));
    }

    /// Deletes `ids` of `plan` on a worker thread behind a modal progress
    /// dialog. On success, returns to a rebuilt dashboard and shows the result;
    /// on failure (e.g. the lock is held) shows why and stays on the page.
    fn apply(self: &Rc<Self>, plan: Plan, ids: Vec<String>) {
        let Some(config) = self.config() else { return };
        let label = gtk::Label::builder().label("Deleting…").xalign(0.0).ellipsize(gtk::pango::EllipsizeMode::Middle).build();
        let bar = gtk::ProgressBar::new();
        let body = gtk::Box::builder()
            .orientation(gtk::Orientation::Vertical)
            .spacing(12)
            .margin_top(12)
            .margin_bottom(24)
            .margin_start(24)
            .margin_end(24)
            .build();
        body.append(&label);
        body.append(&bar);
        let header = adw::HeaderBar::builder().show_end_title_buttons(false).show_start_title_buttons(false).build();
        let content = adw::ToolbarView::new();
        content.add_top_bar(&header);
        content.set_content(Some(&body));
        let dialog = adw::Dialog::builder().title("Cleaning").content_width(420).can_close(false).child(&content).build();
        dialog.present(Some(&self.window));

        let paths = self.paths.clone();
        let job_plan = plan.clone();
        let app = Rc::downgrade(self);
        let cleaning = self.cleaning.clone();
        cleaning.set(true);
        worker::run(
            move |progress| apply_selected(&paths, &config, &job_plan, &ids, progress),
            move |p| {
                let text = if p.current.is_empty() { "Finishing…".to_string() } else { format!("Deleting {}…", p.current) };
                label.set_label(&text);
                bar.set_fraction(if p.total == 0 { 0.0 } else { p.done as f64 / p.total as f64 });
            },
            move |result| {
                cleaning.set(false);
                dialog.force_close();
                let Some(app) = app.upgrade() else { return };
                match result {
                    Ok(report) => {
                        app.nav.pop_to_tag("dashboard");
                        app.refresh();
                        app.show_result(&plan, &report);
                    }
                    // Stay put (e.g. on the review page) so the selection survives a retry.
                    Err(e) => {
                        app.refresh();
                        app.alert("Clean failed", &error_text(&e));
                    }
                }
            },
        );
    }

    /// Counts, freed space per filesystem (as the CLI prints it) and each
    /// skipped or failed entry with its reason.
    fn show_result(&self, plan: &Plan, report: &RunReport) {
        let count = |f: fn(&Outcome) -> bool| report.results.iter().filter(|(_, o)| f(o)).count();
        let deleted = count(|o| matches!(o, Outcome::Deleted { .. }));
        let skipped = count(|o| matches!(o, Outcome::Skipped { .. }));
        let failed = count(|o| matches!(o, Outcome::Failed { .. }));
        let mut body = format!("{deleted} deleted, {skipped} skipped, {failed} failed\n\nFreed (filesystem free space after minus before):");
        for (label, delta) in &report.freed_by_fs {
            let sign = if *delta < 0 { "-" } else { "" };
            body.push_str(&format!("\n{label}: {sign}{}", human_bytes(delta.unsigned_abs())));
        }
        let problems: Vec<String> = report
            .results
            .iter()
            .filter_map(|(id, outcome)| {
                let title = plan.entries.iter().find(|e| &e.id == id).map_or(id.as_str(), |e| e.title.as_str());
                match outcome {
                    Outcome::Skipped { reason } => Some(format!("skipped  {title}: {reason}")),
                    Outcome::Failed { error } => Some(format!("failed  {title}: {error}")),
                    Outcome::Deleted { .. } => None,
                }
            })
            .collect();
        let d = adw::AlertDialog::new(Some("Clean finished"), Some(&body));
        if !problems.is_empty() {
            let list = gtk::Label::builder().label(problems.join("\n")).xalign(0.0).wrap(true).selectable(true).build();
            let scrolled = gtk::ScrolledWindow::builder()
                .hscrollbar_policy(gtk::PolicyType::Never)
                .propagate_natural_height(true)
                .max_content_height(240)
                .child(&list)
                .build();
            d.set_extra_child(Some(&scrolled));
        }
        d.add_response("close", "Close");
        d.present(Some(&self.window));
    }

    /// Pushes the history page, built from the run log.
    fn open_history(&self) {
        let log = crate::log::read(&self.paths.log_file, None).unwrap_or_else(|e| {
            self.toast(&format!("{e:#}"));
            Vec::new()
        });
        let runs = crate::view::history::history(&log, &TimeZone::system());
        self.nav.push(&history::HistoryPage::new(&runs).page());
    }

    /// Pushes the disk map for `path` (the roots level for `None`), read from
    /// the saved index as it is now. Read-only: nothing here deletes.
    fn open_usage(self: &Rc<Self>, path: Option<PathBuf>) {
        let index = match UsageIndex::load(&self.paths.usage_index) {
            Ok(i) => i,
            Err(e) => {
                self.toast(&format!("{e:#}"));
                None
            }
        };
        let plan = self.latest.borrow().clone();
        let (now, tz) = (jiff::Timestamp::now().as_second(), TimeZone::system());
        let level = match (&index, &path) {
            (None, _) => None,
            (Some(i), None) => Some(view_usage::roots(i, plan.as_ref(), now, &tz)),
            (Some(i), Some(p)) => match view_usage::level(i, p, plan.as_ref(), now, &tz) {
                Some(l) => Some(l),
                None => return self.toast(&format!("{} is not in the disk index", p.display())),
            },
        };
        let page = usage::UsagePage::new(level.as_ref());
        let app = Rc::downgrade(self);
        page.connect_open(move |p| {
            if let Some(app) = app.upgrade() {
                app.open_usage(Some(p));
            }
        });
        let app = Rc::downgrade(self);
        page.connect_badge(move || {
            if let Some(app) = app.upgrade() {
                let latest = app.latest.borrow().clone();
                if let Some(plan) = latest {
                    app.open_review(plan, None);
                }
            }
        });
        let app = Rc::downgrade(self);
        page.connect_rescan(move || {
            if let Some(app) = app.upgrade() {
                app.nav.pop_to_tag("dashboard");
                app.scan();
            }
        });
        let app = Rc::downgrade(self);
        page.connect_open_in_files(move |p| {
            let Some(app) = app.upgrade() else { return };
            let file = gtk::gio::File::for_path(&p);
            let weak = Rc::downgrade(&app);
            gtk::FileLauncher::new(Some(&file)).launch(Some(&app.window), gtk::gio::Cancellable::NONE, move |r| {
                if let (Err(e), Some(app)) = (r, weak.upgrade()) {
                    app.toast(&format!("{e}"));
                }
            });
        });
        self.nav.push(&page.page());
    }

    /// Pushes the settings page for the config file as it is on disk.
    fn open_settings(self: &Rc<Self>) {
        let path = &self.paths.config_file;
        let config = match ConfigDoc::load(path).and_then(|d| d.config()) {
            Ok(c) => c,
            Err(e) => return self.alert("Can't load the config", &format!("{e:#}")),
        };
        let page = settings::SettingsPage::new(&config, config::is_read_only(path));
        if crate::timer::nix_managed(&self.paths) {
            page.timers_managed_by_nix();
        }
        let app = Rc::downgrade(self);
        page.connect_save(move |new| {
            if let Some(app) = app.upgrade() {
                app.save_settings(&new);
            }
        });
        self.nav.push(&page.page());
    }

    /// Writes the changed keys into config.toml, re-renders an installed
    /// timer, then rebuilds the dashboard.
    fn save_settings(&self, new: &Config) {
        let path = &self.paths.config_file;
        let saved = ConfigDoc::load(path).and_then(|mut doc| {
            doc.update(new)?;
            doc.save(path)
        });
        if let Err(e) = saved {
            return self.alert("Can't save settings", &format!("{e:#}"));
        }
        let timer = crate::self_exe()
            .map_err(anyhow::Error::from)
            .and_then(|exe| crate::timer::refresh_if_installed(&self.paths, new, &exe));
        match timer {
            Ok(_) => self.toast("Settings saved"),
            Err(e) => self.alert("Settings saved, but the timer wasn't updated", &format!("{e:#}")),
        }
        self.refresh();
    }
}
