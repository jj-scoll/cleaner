//! The settings page: the config's editable keys (spec §7.2, §7.3).

use adw::prelude::*;
use gtk::glib;
use std::cell::RefCell;
use std::rc::Rc;

use crate::config::{Config, StoreCfg};
use crate::protect::BUILTIN_PROTECTED;
use crate::settings::validate;

pub const READ_ONLY_COPY: &str = "config.toml is read-only (managed elsewhere); settings are view-only";

const GIB: f64 = (1u64 << 30) as f64;
const MIB: f64 = (1u64 << 20) as f64;

fn group(title: &str, description: Option<&str>) -> adw::PreferencesGroup {
    adw::PreferencesGroup::builder().title(title).description(description.unwrap_or_default()).build()
}

fn spin(title: &str, min: f64, max: f64, step: f64, digits: u32, value: f64) -> adw::SpinRow {
    let s = adw::SpinRow::with_range(min, max, step);
    s.set_title(title);
    s.set_digits(digits);
    s.set_value(value);
    s
}

fn switch(title: &str, active: bool) -> adw::SwitchRow {
    adw::SwitchRow::builder().title(title).active(active).build()
}

fn entry(title: &str, text: &str) -> adw::EntryRow {
    adw::EntryRow::builder().title(title).text(text).build()
}

fn remove_button() -> gtk::Button {
    gtk::Button::builder()
        .icon_name("user-trash-symbolic")
        .tooltip_text("Remove")
        .valign(gtk::Align::Center)
        .css_classes(["flat"])
        .build()
}

/// A spin row showing bytes in `unit`s. Reading it back returns the original
/// bytes while the value is untouched, so display rounding never rewrites the config.
struct Scaled {
    row: adw::SpinRow,
    shown: f64,
    bytes: u64,
    unit: f64,
}

impl Scaled {
    fn new(row: adw::SpinRow, bytes: u64, unit: f64) -> Scaled {
        row.set_value(bytes as f64 / unit);
        Scaled { shown: row.value(), row, bytes, unit }
    }

    fn bytes(&self) -> u64 {
        let v = self.row.value();
        if v == self.shown { self.bytes } else { (v * self.unit).round() as u64 }
    }
}

/// Strings shown as removable rows, then an entry row that adds one; both sit
/// at the end of `group`.
struct StrList {
    rows: Rc<RefCell<Vec<adw::ActionRow>>>,
}

impl StrList {
    fn new(group: &adw::PreferencesGroup, add_title: &str, items: &[String]) -> StrList {
        let rows: Rc<RefCell<Vec<adw::ActionRow>>> = Rc::default();
        let add = adw::EntryRow::builder().title(add_title).show_apply_button(true).build();
        for s in items {
            Self::push(&rows, group, s);
        }
        group.add(&add);
        add.connect_apply(glib::clone!(
            #[weak]
            rows,
            #[weak]
            group,
            move |add| {
                let text = add.text().trim().to_string();
                if text.is_empty() {
                    return;
                }
                group.remove(add);
                Self::push(&rows, &group, &text);
                group.add(add);
                add.set_text("");
            }
        ));
        StrList { rows }
    }

    fn push(rows: &Rc<RefCell<Vec<adw::ActionRow>>>, group: &adw::PreferencesGroup, text: &str) {
        let row = adw::ActionRow::builder().title(text).use_markup(false).build();
        let remove = remove_button();
        remove.connect_clicked(glib::clone!(
            #[weak]
            rows,
            #[weak]
            group,
            #[weak]
            row,
            move |_| {
                group.remove(&row);
                rows.borrow_mut().retain(|r| r != &row);
            }
        ));
        row.add_suffix(&remove);
        group.add(&row);
        rows.borrow_mut().push(row);
    }

    fn values(&self) -> Vec<String> {
        self.rows.borrow().iter().map(|r| r.title().to_string()).collect()
    }
}

/// One folder rule. `unit` is not editable and is kept from `base`.
struct RuleRow {
    expander: adw::ExpanderRow,
    base: StoreCfg,
    path: adw::EntryRow,
    days: adw::SpinRow,
    glob: adw::EntryRow,
    min: Scaled,
}

impl RuleRow {
    fn new(r: &StoreCfg) -> RuleRow {
        let expander = adw::ExpanderRow::builder().title(&r.path).use_markup(false).build();
        let path = entry("Path", &r.path);
        let days = spin("Older than (days)", 1.0, 36500.0, 1.0, 0, r.max_age_days as f64);
        let glob = entry("File pattern (optional, e.g. *.iso)", r.glob.as_deref().unwrap_or_default());
        let min = Scaled::new(spin("Minimum size (MiB, 0 = any)", 0.0, 1_048_576.0, 10.0, 0, 0.0), r.min_bytes.unwrap_or(0), MIB);
        path.connect_changed(glib::clone!(
            #[weak]
            expander,
            move |p| expander.set_title(&p.text())
        ));
        expander.add_row(&path);
        expander.add_row(&days);
        expander.add_row(&glob);
        expander.add_row(&min.row);
        RuleRow { expander, base: r.clone(), path, days, glob, min }
    }

    fn config(&self) -> StoreCfg {
        let glob = self.glob.text().trim().to_string();
        let min = self.min.bytes();
        StoreCfg {
            path: self.path.text().trim().to_string(),
            max_age_days: self.days.value().round() as u64,
            unit: self.base.unit.clone(),
            glob: (!glob.is_empty()).then_some(glob),
            min_bytes: if min == self.base.min_bytes.unwrap_or(0) { self.base.min_bytes } else { (min > 0).then_some(min) },
        }
    }
}

struct Inner {
    base: Config,
    prefs: adw::PreferencesPage,
    errors: adw::PreferencesGroup,
    error_rows: RefCell<Vec<adw::ActionRow>>,
    threshold: Scaled,
    time: adw::EntryRow,
    filesystems: StrList,
    protect: StrList,
    artifacts_enabled: adw::SwitchRow,
    roots: StrList,
    low_enabled: adw::SwitchRow,
    low_percent: adw::SpinRow,
    max_depth: adw::SpinRow,
    stale: adw::SpinRow,
    caches_enabled: adw::SwitchRow,
    uv_prune: adw::SwitchRow,
    keeps: Vec<adw::SpinRow>,
    worktrees_enabled: adw::SwitchRow,
    worktrees_idle: adw::SpinRow,
    app_caches_enabled: adw::SwitchRow,
    docker_enabled: adw::SwitchRow,
    container_days: adw::SpinRow,
    build_cache_days: adw::SpinRow,
    nix_enabled: adw::SwitchRow,
    nix_keep: adw::SpinRow,
    devbox_idle: adw::SpinRow,
    nix_gc: adw::SwitchRow,
    rules: Rc<RefCell<Vec<RuleRow>>>,
    trash_enabled: adw::SwitchRow,
    trash_days: adw::SpinRow,
    idle_enabled: adw::SwitchRow,
    idle_days: adw::SpinRow,
    idle_min: Scaled,
    on_save: RefCell<Option<OnSave>>,
}

type OnSave = Rc<dyn Fn(Config)>;

impl Inner {
    /// The base config with every editable key read from the widgets.
    fn config(&self) -> Config {
        let mut c = self.base.clone();
        c.notify_threshold_bytes = self.threshold.bytes();
        c.timer_time = self.time.text().trim().to_string();
        c.filesystems = self.filesystems.values();
        c.protect = self.protect.values();
        c.artifacts.enabled = self.artifacts_enabled.is_active();
        c.scan_roots = self.roots.values();
        c.low_space.enabled = self.low_enabled.is_active();
        c.low_space.percent = self.low_percent.value().round() as u8;
        c.artifacts.max_depth = self.max_depth.value().round() as usize;
        c.artifacts.stale_after_days = self.stale.value().round() as u64;
        c.caches.enabled = self.caches_enabled.is_active();
        c.caches.uv_prune = self.uv_prune.is_active();
        for (k, s) in c.caches.keep_newest.iter_mut().zip(&self.keeps) {
            k.keep = s.value().round() as usize;
        }
        c.user_rules = self.rules.borrow().iter().map(RuleRow::config).collect();
        c.trash.enabled = self.trash_enabled.is_active();
        c.trash.max_age_days = self.trash_days.value().round() as u64;
        c.idle_folders.enabled = self.idle_enabled.is_active();
        c.idle_folders.idle_days = self.idle_days.value().round() as u64;
        c.idle_folders.min_bytes = self.idle_min.bytes();
        c.worktrees.enabled = self.worktrees_enabled.is_active();
        c.worktrees.idle_days = self.worktrees_idle.value().round() as u64;
        c.app_caches.enabled = self.app_caches_enabled.is_active();
        c.docker.enabled = self.docker_enabled.is_active();
        c.docker.container_days = self.container_days.value().round() as u64;
        c.docker.build_cache_days = self.build_cache_days.value().round() as u64;
        c.nix.enabled = self.nix_enabled.is_active();
        c.nix.keep = self.nix_keep.value().round() as usize;
        c.nix.devbox_idle_days = self.devbox_idle.value().round() as u64;
        c.nix.gc = self.nix_gc.is_active();
        c
    }

    fn show_errors(&self, errors: &[String]) {
        for r in self.error_rows.borrow_mut().drain(..) {
            self.errors.remove(&r);
        }
        for e in errors {
            let row = adw::ActionRow::builder().title(e).use_markup(false).css_classes(["error"]).build();
            self.errors.add(&row);
            self.error_rows.borrow_mut().push(row);
        }
        self.errors.set_visible(!errors.is_empty());
        if !errors.is_empty() {
            self.prefs.scroll_to_top();
        }
    }
}

pub struct SettingsPage {
    page: adw::NavigationPage,
    inner: Rc<Inner>,
}

impl SettingsPage {
    /// The timers are declared in Nix, so their rows here would do nothing:
    /// grey them out and say where they are set.
    pub fn timers_managed_by_nix(&self) {
        self.inner.time.set_sensitive(false);
        self.inner.low_enabled.set_sensitive(false);
        self.inner.low_enabled.set_subtitle("The nightly time and low-space check are set in Nix (services.cleaner).");
    }

    /// One group per section; a read-only config shows a banner, greys out
    /// every row and hides Save.
    pub fn new(config: &Config, read_only: bool) -> SettingsPage {
        let prefs = adw::PreferencesPage::new();
        let errors = adw::PreferencesGroup::builder().title("Can't save").visible(false).build();
        prefs.add(&errors);

        let general = group("General", None);
        let threshold = Scaled::new(
            spin("Notify when a plan can free (GiB)", 0.0, 1024.0, 0.5, 1, 0.0),
            config.notify_threshold_bytes,
            GIB,
        );
        let time = entry("Nightly plan time (HH:MM)", &config.timer_time);
        general.add(&threshold.row);
        general.add(&time);
        let filesystems = StrList::new(&general, "Add a filesystem to show", &config.filesystems);
        let roots = StrList::new(&general, "Add a scan root", &config.scan_roots);
        let low_enabled = switch("Warn when disk space is low", config.low_space.enabled);
        let low_percent = spin("Warn when free space is below (%)", 1.0, 50.0, 1.0, 0, config.low_space.percent as f64);
        general.add(&low_enabled);
        general.add(&low_percent);
        let overrides = config.root_overrides();
        if !overrides.is_empty() {
            let note = gtk::Label::builder()
                .label(format!("Overridden in config.toml for: {}", overrides.join(", ")))
                .halign(gtk::Align::Start)
                .wrap(true)
                .css_classes(["dim-label"])
                .build();
            general.add(&note);
        }

        let protected = group("Protected paths", Some("Never cleaned, even when a rule matches."));
        for p in BUILTIN_PROTECTED {
            let row = adw::ActionRow::builder().title(*p).subtitle("built-in").use_markup(false).build();
            row.add_prefix(&gtk::Image::from_icon_name("changes-prevent-symbolic"));
            protected.add(&row);
        }
        let protect = StrList::new(&protected, "Add a protected path", &config.protect);

        let a = &config.artifacts;
        let artifacts = group("Project artifacts", None);
        let artifacts_enabled = switch("Enabled", a.enabled);
        let max_depth = spin("Search depth", 1.0, 64.0, 1.0, 0, a.max_depth as f64);
        let stale = spin("Stale after (days)", 0.0, 36500.0, 1.0, 0, a.stale_after_days as f64);
        artifacts.add(&artifacts_enabled);
        artifacts.add(&max_depth);
        artifacts.add(&stale);

        let caches = group(
            "Tool caches",
            Some(&format!("{} cache stores; edit config.toml to change them.", config.caches.stores.len())),
        );
        let caches_enabled = switch("Enabled", config.caches.enabled);
        let uv_prune = switch("Offer uv cache prune", config.caches.uv_prune);
        caches.add(&caches_enabled);
        caches.add(&uv_prune);
        let keeps: Vec<adw::SpinRow> = config
            .caches
            .keep_newest
            .iter()
            .map(|k| {
                let s = spin(&format!("Keep newest in {}", k.path), 0.0, 1000.0, 1.0, 0, k.keep as f64);
                caches.add(&s);
                s
            })
            .collect();

        let w = &config.worktrees;
        let worktrees = group("Finished worktrees", Some("Linked git worktrees whose branch is merged."));
        let worktrees_enabled = switch("Enabled", w.enabled);
        let worktrees_idle = spin("Idle for (days)", 0.0, 36500.0, 1.0, 0, w.idle_days as f64);
        worktrees.add(&worktrees_enabled);
        worktrees.add(&worktrees_idle);

        let app_caches = group(
            "Browser and app caches",
            Some(&format!("{} apps, skipped while running; edit config.toml to change them.", config.app_caches.apps.len())),
        );
        let app_caches_enabled = switch("Enabled", config.app_caches.enabled);
        app_caches.add(&app_caches_enabled);

        let d = &config.docker;
        let docker = group("Docker", None);
        let docker_enabled = switch("Enabled", d.enabled);
        let container_days = spin("Stopped containers older than (days)", 1.0, 36500.0, 1.0, 0, d.container_days as f64);
        let build_cache_days = spin("Build cache unused for (days)", 1.0, 36500.0, 1.0, 0, d.build_cache_days as f64);
        docker.add(&docker_enabled);
        docker.add(&container_days);
        docker.add(&build_cache_days);

        let n = &config.nix;
        let nix = group("Nix", Some("System generations always ask for your password."));
        let nix_enabled = switch("Enabled", n.enabled);
        let nix_keep = spin("Generations to keep per profile", 0.0, 1000.0, 1.0, 0, n.keep as f64);
        let devbox_idle = spin("Devbox roots idle for (days)", 0.0, 36500.0, 1.0, 0, n.devbox_idle_days as f64);
        let nix_gc = switch("Offer store garbage collection", n.gc);
        nix.add(&nix_enabled);
        nix.add(&nix_keep);
        nix.add(&devbox_idle);
        nix.add(&nix_gc);

        let rules_group = group("Folder rules", Some("Files older than the given age in these folders."));
        let rules: Rc<RefCell<Vec<RuleRow>>> = Rc::default();
        let add_rule = adw::EntryRow::builder().title("Add a folder rule (path)").show_apply_button(true).build();
        let push_rule = |rules: &Rc<RefCell<Vec<RuleRow>>>, group: &adw::PreferencesGroup, r: &StoreCfg| {
            let rule = RuleRow::new(r);
            let remove = remove_button();
            let expander = rule.expander.clone();
            remove.connect_clicked(glib::clone!(
                #[weak]
                rules,
                #[weak]
                group,
                #[weak]
                expander,
                move |_| {
                    group.remove(&expander);
                    rules.borrow_mut().retain(|r| r.expander != expander);
                }
            ));
            expander.add_suffix(&remove);
            group.add(&expander);
            rules.borrow_mut().push(rule);
        };
        for r in &config.user_rules {
            push_rule(&rules, &rules_group, r);
        }
        rules_group.add(&add_rule);
        add_rule.connect_apply(glib::clone!(
            #[weak]
            rules,
            #[weak]
            rules_group,
            move |add| {
                let path = add.text().trim().to_string();
                if path.is_empty() {
                    return;
                }
                rules_group.remove(add);
                let r = StoreCfg { path, max_age_days: 90, unit: "files".into(), glob: None, min_bytes: None };
                push_rule(&rules, &rules_group, &r);
                rules_group.add(add);
                add.set_text("");
            }
        ));

        let trash = group("Trash", None);
        let trash_enabled = switch("Enabled", config.trash.enabled);
        let trash_days = spin("Trashed more than (days) ago", 0.0, 36500.0, 1.0, 0, config.trash.max_age_days as f64);
        trash.add(&trash_enabled);
        trash.add(&trash_days);

        let i = &config.idle_folders;
        let idle = group("Large and old", Some("Reported only; never selected for deletion."));
        let idle_enabled = switch("Enabled", i.enabled);
        let idle_days = spin("Untouched for (days)", 0.0, 36500.0, 1.0, 0, i.idle_days as f64);
        let idle_min = Scaled::new(spin("At least (MiB)", 0.0, 1_048_576.0, 50.0, 0, 0.0), i.min_bytes, MIB);
        idle.add(&idle_enabled);
        idle.add(&idle_days);
        idle.add(&idle_min.row);

        for g in [&general, &protected, &artifacts, &caches, &worktrees, &app_caches, &docker, &nix, &rules_group, &trash, &idle] {
            prefs.add(g);
            g.set_sensitive(!read_only);
        }

        let save = gtk::Button::builder().label("Save").css_classes(["suggested-action"]).visible(!read_only).build();
        let header = adw::HeaderBar::new();
        header.pack_end(&save);
        let toolbar = adw::ToolbarView::new();
        toolbar.add_top_bar(&header);
        if read_only {
            toolbar.add_top_bar(&adw::Banner::builder().title(READ_ONLY_COPY).use_markup(false).revealed(true).build());
        }
        toolbar.set_content(Some(&prefs));
        let page = adw::NavigationPage::builder().title("Settings").tag("settings").child(&toolbar).build();

        let inner = Rc::new(Inner {
            base: config.clone(),
            prefs,
            errors,
            error_rows: RefCell::default(),
            threshold,
            time,
            filesystems,
            protect,
            artifacts_enabled,
            roots,
            low_enabled,
            low_percent,
            max_depth,
            stale,
            caches_enabled,
            uv_prune,
            keeps,
            worktrees_enabled,
            worktrees_idle,
            app_caches_enabled,
            docker_enabled,
            container_days,
            build_cache_days,
            nix_enabled,
            nix_keep,
            devbox_idle,
            nix_gc,
            rules,
            trash_enabled,
            trash_days,
            idle_enabled,
            idle_days,
            idle_min,
            on_save: RefCell::default(),
        });
        let weak = Rc::downgrade(&inner);
        save.connect_clicked(move |_| {
            let Some(i) = weak.upgrade() else { return };
            let c = i.config();
            match validate(&c) {
                Err(errors) => i.show_errors(&errors),
                Ok(()) => {
                    i.show_errors(&[]);
                    let f = i.on_save.borrow().clone();
                    if let Some(f) = f {
                        f(c);
                    }
                }
            }
        });
        // As in ReviewPage: the page keeps `Inner` alive, handlers hold Weak refs.
        let keep = inner.clone();
        page.connect_destroy(move |_| {
            let _ = &keep;
        });
        SettingsPage { page, inner }
    }

    pub fn page(&self) -> adw::NavigationPage {
        self.page.clone()
    }

    /// `f(config)` runs on Save with the edited config, once it validates.
    pub fn connect_save(&self, f: impl Fn(Config) + 'static) {
        *self.inner.on_save.borrow_mut() = Some(Rc::new(f));
    }

    /// Lists `errors` in a group at the top; an empty slice hides it.
    pub fn show_errors(&self, errors: &[String]) {
        self.inner.show_errors(errors);
    }
}
