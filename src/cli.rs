//! The `cleaner` commands (spec §7.1): plan, show, review, apply, explain,
//! status, log, stats, timer, hold.

use anyhow::{Context, bail};
use clap::{CommandFactory, Parser, Subcommand};
use std::collections::HashMap;
use std::io::{BufRead, Write};
use std::path::PathBuf;

use cleaner::category::Outcome;
use cleaner::config::{self, Config};
use cleaner::engine::{apply_selected, filesystem_paths, plan_and_save, registry};
use cleaner::fmt::{human_bytes, plural};
use cleaner::notify::{DbusNotifier, gui_launch_command, notify_if_worth_it};
use cleaner::paths::Paths;
use cleaner::plan::{Entry, Plan};
use cleaner::timer;
use cleaner::view::review::{ReviewModel, Sort, entry_line};
use cleaner::view::{category_label, category_order};

#[derive(Parser)]
#[command(name = "cleaner", version, about = "Approve-first disk cleanup")]
pub struct Cli {
    #[command(subcommand)]
    pub command: Option<Cmd>,
}

#[derive(Subcommand)]
pub enum Cmd {
    /// Scan every enabled category, save a plan and print a summary.
    Plan {
        /// Print the plan as JSON.
        #[arg(long)]
        json: bool,
        /// Then send a desktop notification if the plan can free enough.
        #[arg(long)]
        notify: bool,
        /// Low-space check: when a filesystem is low, plan and notify (never deletes).
        #[arg(long)]
        if_low: bool,
    },
    /// Print a plan grouped by category, with reasons.
    Show {
        /// Plan id (default: the latest plan).
        plan: Option<String>,
        #[arg(long)]
        json: bool,
        /// Order within each category: size (largest first), name, or age
        /// (least recently touched first).
        #[arg(long, default_value = "size", value_parser = parse_sort)]
        sort: Sort,
    },
    /// Tick a plan's entries in the terminal, then apply them (no display needed).
    Review {
        /// Plan id (default: the latest plan).
        plan: Option<String>,
    },
    /// Delete (or, with `[hold]` on, hold) the selected entries of a plan.
    Apply {
        /// Plan id (default: the latest plan, if `cleaner show` printed it).
        plan: Option<String>,
        /// Only apply entries of this category (repeatable).
        #[arg(long, value_name = "CATEGORY")]
        only: Vec<String>,
        /// Don't ask for confirmation.
        #[arg(long)]
        yes: bool,
    },
    /// Why a path is (or isn't) in the plan. Changes nothing.
    Explain {
        /// The file or folder to ask about.
        path: PathBuf,
        /// Plan id (default: the latest plan).
        #[arg(long)]
        plan: Option<String>,
        #[arg(long)]
        json: bool,
    },
    /// Disk usage, the latest plan and the last run.
    Status {
        #[arg(long)]
        json: bool,
    },
    /// What past runs deleted or skipped.
    Log {
        /// Only runs from the last DAYS days.
        #[arg(long, value_name = "DAYS")]
        since: Option<u64>,
        #[arg(long)]
        json: bool,
    },
    /// Space freed so far: all time, this year, last 30 days, runs, top
    /// categories and the biggest run.
    Stats {
        #[arg(long)]
        json: bool,
        /// Only runs on or after DATE (YYYY-MM-DD, local time).
        #[arg(long, value_name = "DATE")]
        since: Option<String>,
    },
    /// Open the window (the default with no command when a display is available).
    Gui {
        /// Plan id to review on startup.
        plan: Option<String>,
    },
    /// The nightly systemd user timer that builds a plan and notifies.
    Timer {
        #[command(subcommand)]
        action: TimerCmd,
    },
    /// The opt-in holding area (`[hold] enabled = true`): list, restore, purge.
    Hold {
        #[command(subcommand)]
        action: HoldCmd,
    },
}

#[derive(Subcommand)]
pub enum HoldCmd {
    /// What is held, where it came from, and when it expires.
    List,
    /// Move held items back where they were (never over anything there now).
    Restore {
        /// A hold id (from `cleaner hold list`) or an original path.
        target: String,
    },
    /// Delete expired held items now (all of them with --all).
    Purge {
        /// Every held item, not only expired ones.
        #[arg(long)]
        all: bool,
        /// Don't ask for confirmation (with --all).
        #[arg(long)]
        yes: bool,
    },
}

#[derive(Subcommand)]
pub enum TimerCmd {
    /// Write the service, timer and .desktop file, then enable the timer.
    Install,
    /// Disable the timer and remove those files.
    Uninstall,
    /// Whether the units are installed, and when the timer runs next.
    Status,
}

/// Runs the command; `Ok(code)` is the process exit code.
pub fn run(cli: Cli) -> anyhow::Result<i32> {
    match cli.command {
        None if cfg!(feature = "gui") && has_display() => cmd_gui(None),
        None => {
            Cli::command().print_help()?;
            println!();
            Ok(0)
        }
        Some(Cmd::Gui { plan }) => cmd_gui(plan),
        Some(Cmd::Plan { if_low: true, .. }) => cmd_if_low(),
        Some(Cmd::Plan { json, notify, .. }) => cmd_plan(json, notify),
        Some(Cmd::Show { plan, json, sort }) => cmd_show(plan, json, sort),
        Some(Cmd::Review { plan }) => cmd_review(plan),
        Some(Cmd::Apply { plan, only, yes }) => cmd_apply(plan, &only, yes),
        Some(Cmd::Explain { path, plan, json }) => cmd_explain(&path, plan, json),
        Some(Cmd::Status { json }) => cmd_status(json),
        Some(Cmd::Log { since, json }) => cmd_log(since, json),
        Some(Cmd::Stats { json, since }) => cmd_stats(json, since.as_deref()),
        Some(Cmd::Timer { action }) => cmd_timer(action),
        Some(Cmd::Hold { action }) => cmd_hold(action),
    }
}

fn has_display() -> bool {
    ["WAYLAND_DISPLAY", "DISPLAY"]
        .iter()
        .any(|v| std::env::var_os(v).is_some_and(|s| !s.is_empty()))
}

#[cfg(feature = "gui")]
fn cmd_gui(plan: Option<String>) -> anyhow::Result<i32> {
    cleaner::gui::run(plan)
}

#[cfg(not(feature = "gui"))]
fn cmd_gui(_plan: Option<String>) -> anyhow::Result<i32> {
    bail!("built without the gui feature")
}

fn now() -> i64 {
    jiff::Timestamp::now().as_second()
}

fn local_time(t: i64) -> String {
    cleaner::fmt::local_time(t, &jiff::tz::TimeZone::system())
}

/// Loads the config, printing unknown-key warnings to stderr.
fn load_config(paths: &Paths) -> anyhow::Result<Config> {
    let (cfg, warnings) = config::load(&paths.config_file)?;
    for w in warnings {
        eprintln!("warning: {w}");
    }
    Ok(cfg)
}

/// The named plan, or the latest one.
fn resolve_plan(paths: &Paths, id: Option<String>) -> anyhow::Result<(PathBuf, Plan)> {
    let path = match id {
        Some(id) => Plan::path_for(&paths.plans_dir, &id)?,
        None => Plan::latest(&paths.plans_dir)?.context("no plan yet; run `cleaner plan` first")?,
    };
    let plan = Plan::load(&path)?;
    Ok((path, plan))
}

fn cmd_plan(json: bool, notify: bool) -> anyhow::Result<i32> {
    let paths = Paths::from_env()?;
    config::write_default_if_missing(&paths.config_file)?;
    let config = load_config(&paths)?;
    let (plan, path) = plan_and_save(&paths, &config, now(), &mut |_| {})?;
    let cats = registry(&config);

    if json {
        println!("{}", serde_json::to_string_pretty(&plan)?);
        if notify {
            notify_plan(&plan, config.notify_threshold_bytes);
        }
        return Ok(0);
    }
    println!("Plan {}", plan.id);
    for cat in cats.iter().map(|c| c.id()) {
        let sel: Vec<&Entry> = plan
            .entries
            .iter()
            .filter(|e| e.category == cat && e.selected)
            .collect();
        let bytes: u64 = sel.iter().map(|e| e.bytes).sum();
        let report = plan
            .report_only
            .iter()
            .filter(|e| e.category == cat)
            .count();
        let extra = if report > 0 {
            format!("  (+{report} report only)")
        } else {
            String::new()
        };
        println!(
            "  {cat:<14} {:>4} selected  {:>10}{extra}",
            sel.len(),
            human_bytes(bytes)
        );
    }
    let n = plan.entries.iter().filter(|e| e.selected).count();
    println!(
        "Total selected: {} in {}",
        human_bytes(plan.selected_bytes()),
        plural(n, "item")
    );
    if !plan.skipped.is_empty() {
        println!("Skipped: {} (see `cleaner show`)", plan.skipped.len());
    }
    println!("Plan saved to {}", path.display());
    if notify {
        notify_plan(&plan, config.notify_threshold_bytes);
    }
    Ok(0)
}

/// `plan --notify`: notify, and on a click start `cleaner gui <id>` in its own
/// transient unit (see `gui_launch_command`).
/// A failure is only a warning; the plan is already saved.
fn notify_plan(plan: &Plan, threshold: u64) {
    let mut launch = |id: &str| {
        let started =
            cleaner::self_exe().and_then(|exe| gui_launch_command(&exe, Some(id)).status());
        match started {
            Ok(s) if s.success() => {}
            Ok(s) => eprintln!("warning: starting the GUI failed: systemd-run {s}"),
            Err(e) => eprintln!("warning: starting the GUI failed: {e}"),
        }
    };
    let gui = cfg!(feature = "gui");
    let notifier = DbusNotifier::new(gui);
    let launch: Option<&mut dyn FnMut(&str)> = if gui { Some(&mut launch) } else { None };
    if let Err(e) = notify_if_worth_it(plan, threshold, &notifier, launch) {
        eprintln!("warning: notification failed: {e:#}");
    }
}

/// `plan --if-low`: only plans and notifies, so it never reaches `apply_*`.
fn cmd_if_low() -> anyhow::Result<i32> {
    use cleaner::engine::{IndexMode, plan_and_save_with};
    use cleaner::space::{decide, load_state, save_error_reason, save_state};
    let paths = Paths::from_env()?;
    let config = load_config(&paths)?;
    if !config.low_space.enabled {
        return Ok(0);
    }
    let usage = cleaner::disk::usage(&filesystem_paths(&config, &paths.home));
    let state = load_state(&paths.space_state);
    let (low, next) = decide(&usage, &state, config.low_space.percent, now());
    if low.is_empty() {
        if next != state
            && let Err(e) = save_state(&paths.space_state, &next)
        {
            eprintln!("warning: saving the low-space state failed: {e:#}");
        }
        return Ok(0);
    }
    let saved = match plan_and_save_with(&paths, &config, now(), IndexMode::Reuse, &mut |_| {}) {
        Ok((plan, _)) => Ok(plan),
        // Another run owns the lock; leave the state alone so the next tick retries.
        Err(e) if format!("{e:#}").contains("another cleaner run is in progress") => return Ok(0),
        Err(e) => Err(save_error_reason(&e)),
    };
    if let Err(e) = save_state(&paths.space_state, &next) {
        eprintln!("warning: saving the low-space state failed: {e:#}");
    }
    let mut launch = |id: Option<&str>| {
        let started = cleaner::self_exe().and_then(|exe| gui_launch_command(&exe, id).status());
        match started {
            Ok(s) if s.success() => {}
            Ok(s) => eprintln!("warning: starting the GUI failed: systemd-run {s}"),
            Err(e) => eprintln!("warning: starting the GUI failed: {e}"),
        }
    };
    let gui = cfg!(feature = "gui");
    let notifier = DbusNotifier::new(gui);
    let launch: Option<cleaner::notify::LowSpaceLaunch<'_>> =
        if gui { Some(&mut launch) } else { None };
    if let Err(e) = cleaner::notify::notify_low_space(
        &low,
        saved.as_ref().map_err(Clone::clone),
        &notifier,
        launch,
    ) {
        eprintln!("warning: notification failed: {e:#}");
    }
    Ok(0)
}

fn print_entry(e: &Entry) {
    println!("  {}", entry_line(e, e.selected));
    if let Some(note) = &e.note {
        println!("      note: {note}");
    }
}

fn parse_sort(s: &str) -> Result<Sort, String> {
    Sort::parse(s).ok_or_else(|| format!("expected size, name or age, got {s:?}"))
}

fn cmd_show(id: Option<String>, json: bool, sort: Sort) -> anyhow::Result<i32> {
    let paths = Paths::from_env()?;
    let (path, plan) = resolve_plan(&paths, id)?;
    Plan::mark_shown(&paths.state_dir, &plan.id)?;
    if json {
        println!("{}", serde_json::to_string_pretty(&plan)?);
        return Ok(0);
    }
    println!(
        "Plan {} (created {}), {}",
        plan.id,
        local_time(plan.created),
        path.display()
    );
    println!("Total selected: {}", human_bytes(plan.selected_bytes()));
    for cat in category_order(&plan) {
        let mut entries: Vec<&Entry> = plan.entries.iter().filter(|e| e.category == cat).collect();
        sort.apply(&mut entries);
        if entries.is_empty() {
            continue;
        }
        let sel: u64 = entries.iter().filter(|e| e.selected).map(|e| e.bytes).sum();
        println!("\n{cat} ({} selected)", human_bytes(sel));
        entries.into_iter().for_each(print_entry);
    }
    if !plan.skipped.is_empty() {
        println!("\nSkipped");
        for s in &plan.skipped {
            println!("  {}  {}: {}", s.category, s.title, s.reason);
        }
    }
    if !plan.report_only.is_empty() {
        println!("\nReport only");
        let mut report_only: Vec<&Entry> = plan.report_only.iter().collect();
        sort.apply(&mut report_only);
        for e in report_only {
            println!(
                "  {:>10}  {}  {}  ({})",
                human_bytes(e.bytes),
                e.category,
                e.title,
                e.reason
            );
        }
    }
    Ok(0)
}

fn cmd_apply(id: Option<String>, only: &[String], yes: bool) -> anyhow::Result<i32> {
    let paths = Paths::from_env()?;
    let config = load_config(&paths)?;
    let explicit = id.is_some();
    let (_, plan) = resolve_plan(&paths, id)?;
    if !explicit && Plan::last_shown(&paths.state_dir).as_deref() != Some(plan.id.as_str()) {
        bail!(
            "newest plan {} has not been shown; run `cleaner show` first, or pass the plan id",
            plan.id
        );
    }
    let known: Vec<&str> = registry(&Config::default())
        .iter()
        .map(|c| c.id())
        .collect();
    for c in only {
        if !known.contains(&c.as_str()) {
            bail!(
                "unknown category: {c} (expected one of {})",
                known.join(", ")
            );
        }
    }
    let chosen: Vec<&Entry> = plan
        .entries
        .iter()
        .filter(|e| e.selected && (only.is_empty() || only.contains(&e.category)))
        .collect();
    if chosen.is_empty() {
        println!("Nothing selected to apply.");
        return Ok(0);
    }
    let total: u64 = chosen.iter().map(|e| e.bytes).sum();
    println!("Plan {} (created {})", plan.id, local_time(plan.created));
    let preview = cleaner::hold::preview(&plan, &paths.hold_dir, &config.hold);
    if let Some(p) = &preview {
        print_fates(p, &chosen);
    }
    if !yes {
        print!(
            "{} {} ({})? [y/N] ",
            if preview.is_some() { "Clean" } else { "Delete" },
            plural(chosen.len(), "item"),
            human_bytes(total)
        );
        std::io::stdout().flush()?;
        let mut answer = String::new();
        std::io::stdin().lock().read_line(&mut answer)?;
        if !matches!(answer.trim().to_lowercase().as_str(), "y" | "yes") {
            eprintln!("Aborted; nothing deleted.");
            return Ok(1);
        }
    }

    let ids: Vec<String> = chosen.iter().map(|e| e.id.clone()).collect();
    let report = apply_with_progress(&paths, &config, &plan, &ids)?;
    print_report(&plan, &report);
    Ok(0)
}

/// `review`: the GUI's review page in the terminal. Like the GUI, it marks
/// the plan shown, starts from its own selection and the saved sort, leaves
/// the plan file as it is, and hands the ticked ids to `apply_selected`.
fn cmd_review(id: Option<String>) -> anyhow::Result<i32> {
    use std::io::IsTerminal;
    if !std::io::stdout().is_terminal() {
        bail!(
            "cleaner review needs a terminal (stdout is not a TTY); \
             use `cleaner show` to print the plan and `cleaner apply` to apply it"
        );
    }
    let paths = Paths::from_env()?;
    let config = load_config(&paths)?;
    let (_, plan) = resolve_plan(&paths, id)?;
    Plan::mark_shown(&paths.state_dir, &plan.id)?;
    let mut model = ReviewModel::new(plan.clone());
    model.set_sort(Sort::load(&paths.state_dir));
    model.set_hold(cleaner::hold::preview(&plan, &paths.hold_dir, &config.hold));
    let mut app = cleaner::tui::App::new(model);
    // Leaving without deleting exits 1, like answering no to `cleaner apply`.
    let Some(ids) = cleaner::tui::run(&mut app, &paths.state_dir)? else {
        eprintln!("Nothing deleted.");
        return Ok(1);
    };
    println!("Plan {} (created {})", plan.id, local_time(plan.created));
    cleaner::sys::lower_priority();
    let report = apply_with_progress(&paths, &config, &plan, &ids)?;
    print_report(&plan, &report);
    Ok(0)
}

/// `apply_selected` with progress on stderr (`cleaner::progress`): on a
/// terminal a ticker thread redraws the running entry every second, and the
/// line is ended while an interactive child (pkexec) has the terminal.
fn apply_with_progress(
    paths: &Paths,
    config: &Config,
    plan: &Plan,
    ids: &[String],
) -> anyhow::Result<cleaner::engine::RunReport> {
    use cleaner::progress::ApplyProgress;
    use std::io::IsTerminal;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::{Arc, Mutex};
    use std::time::{Duration, Instant};

    let tty = std::io::stderr().is_terminal();
    let width = tty
        .then(|| ratatui::crossterm::terminal::size().ok())
        .flatten()
        .map(|(w, _)| w as usize);
    // Locks below ignore poisoning: a progress line must never stop an apply.
    let shown = Arc::new(Mutex::new(ApplyProgress::new(
        std::io::stderr(),
        tty,
        width,
    )));

    let stop = Arc::new(AtomicBool::new(false));
    let ticker = tty.then(|| {
        let (shown, stop) = (shown.clone(), stop.clone());
        std::thread::spawn(move || {
            while !stop.load(Ordering::Relaxed) {
                std::thread::park_timeout(Duration::from_secs(1));
                if stop.load(Ordering::Relaxed) {
                    break;
                }
                shown
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .tick(Instant::now());
            }
        })
    });
    if tty {
        let shown = shown.clone();
        cleaner::tool::set_terminal_hook(Some(Box::new(move |busy| {
            let mut p = shown.lock().unwrap_or_else(|e| e.into_inner());
            if busy { p.pause() } else { p.resume() }
        })));
    }

    let result = apply_selected(paths, config, plan, ids, &mut |p| {
        shown
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .event(&p, Instant::now());
    });

    cleaner::tool::set_terminal_hook(None);
    stop.store(true, Ordering::Relaxed);
    if let Some(t) = ticker {
        t.thread().unpark();
        let _ = t.join();
    }
    result
}

/// Each entry's outcome, then the free space each filesystem gained.
fn print_report(plan: &Plan, report: &cleaner::engine::RunReport) {
    let by_id: HashMap<&str, &Entry> = plan.entries.iter().map(|e| (e.id.as_str(), e)).collect();
    for (id, outcome) in &report.results {
        let e = by_id[id.as_str()];
        let label = format!("[{}] {}", e.category, e.title);
        match outcome {
            Outcome::Deleted { bytes, failures } => {
                println!("deleted {:>10}  {label}", human_bytes(*bytes));
                for f in failures {
                    println!("                    ! {f}");
                }
            }
            Outcome::Held {
                id: hold_id,
                bytes,
                failures,
            } => {
                println!(
                    "held    {:>10}  {label}  (hold entry {hold_id}; frees nothing until purged)",
                    human_bytes(*bytes)
                );
                for f in failures {
                    println!("                    ! {f}");
                }
            }
            Outcome::Skipped { reason } => println!("skipped {:>10}  {label}: {reason}", ""),
            Outcome::Failed { error } => println!("failed  {:>10}  {label}: {error}", ""),
        }
    }
    for p in &report.purged {
        match &p.result {
            Ok(freed) => println!(
                "purged  {:>10}  hold entry {} ({}): expired",
                human_bytes(*freed),
                p.held.id,
                p.held.title
            ),
            Err(e) => println!("failed  {:>10}  hold entry {}: {e}", "", p.held.id),
        }
    }
    println!("Freed (filesystem free space after minus before):");
    for (label, delta) in &report.freed_by_fs {
        let sign = if *delta < 0 { "-" } else { "" };
        println!("  {label}: {sign}{}", human_bytes(delta.unsigned_abs()));
    }
}

/// Read-only: loads the config and a plan, writes nothing (no default
/// config, no shown-plan marker).
fn cmd_explain(path: &std::path::Path, id: Option<String>, json: bool) -> anyhow::Result<i32> {
    use cleaner::explain::{Status, explain};
    let paths = Paths::from_env()?;
    let config = load_config(&paths)?;
    let plan = match id {
        Some(id) => Some(resolve_plan(&paths, Some(id))?.1),
        None => match Plan::latest(&paths.plans_dir)? {
            Some(p) => Some(Plan::load(&p)?),
            None => None,
        },
    };
    let protector = cleaner::engine::protector_for(&paths, &config);
    let ctx = cleaner::category::ScanCtx {
        home: &paths.home,
        now: now(),
        config: &config,
        protector: &protector,
        tools: &cleaner::tool::SystemRunner,
        euid: rustix::process::geteuid().as_raw(),
        usage: None,
        hold: None,
    };
    let e = explain(path, plan.as_ref(), &ctx);
    if json {
        println!("{}", serde_json::to_string_pretty(&e)?);
        return Ok(0);
    }
    println!("{}", e.path);
    if let Some(target) = &e.symlink_target {
        println!("  is a symlink to {target}; cleaner never follows it");
    }
    let plan_id = e.plan.as_deref().unwrap_or("");
    let entry_id = e.entry.as_ref().map_or("", |x| x.id.as_str());
    let within = e.entry.as_ref().map_or("", |x| x.path.as_str());
    match e.status {
        Status::Planned => println!("  planned: entry {entry_id} of plan {plan_id}"),
        Status::InsidePlanned => {
            println!("  inside planned item {within}: entry {entry_id} of plan {plan_id}")
        }
        Status::ReportOnly if within == e.path => {
            println!("  report only (listed, never deleted): entry {entry_id} of plan {plan_id}")
        }
        Status::ReportOnly => println!(
            "  inside report-only item {within} (listed, never deleted): entry {entry_id} of plan {plan_id}"
        ),
        Status::Skipped => {
            let s = e.skip.as_ref();
            let (cat, title) = s.map_or(("", ""), |s| (s.category.as_str(), s.title.as_str()));
            println!(
                "  skipped by {cat} in plan {plan_id} ({title}): {}",
                e.reason
            );
        }
        Status::WouldBePlanned => {
            let cat = e.category.as_deref().unwrap_or("");
            match &e.plan {
                Some(id) => println!(
                    "  not in plan {id}, but {cat} would plan it now: {}",
                    e.reason
                ),
                None => println!("  no plan yet; {cat} would plan it: {}", e.reason),
            }
        }
        Status::NotPlanned if e.entry.is_some() => {
            println!(
                "  not planned: inside store-rule row {within} (entry {entry_id} of plan {plan_id}), but its rule doesn't match it now: {}",
                e.reason
            );
            if let Some(x) = &e.entry {
                println!("  rule:         {}", x.rule);
            }
            return Ok(0);
        }
        Status::NotPlanned | Status::Missing => {
            if e.plan.is_none() {
                println!("  no plan yet; checked against the rules now");
            }
            let by = e
                .category
                .as_ref()
                .map_or(String::new(), |c| format!(" ({c})"));
            println!("  not planned{by}: {}", e.reason);
        }
    }
    if let Some(x) = &e.entry {
        println!("  category:     {}", x.category);
        println!("  title:        {}", x.title);
        println!("  size:         {}", human_bytes(x.bytes));
        println!("  reason:       {}", x.reason);
        let sel = if x.selected { "yes" } else { "no" };
        println!("  pre-selected: {sel}");
        if !x.selected && x.reason != e.reason {
            println!("  not pre-selected because: {}", e.reason);
        }
        println!("  rule:         {}", x.rule);
    }
    Ok(0)
}

/// Lists, before the prompt, what holding will do with each chosen entry.
fn print_fates(p: &cleaner::hold::Preview, chosen: &[&Entry]) {
    use cleaner::hold::Fate;
    let fate = |e: &Entry| p.fates.get(&e.id).cloned().unwrap_or(Fate::Permanent);
    let held: Vec<&&Entry> = chosen.iter().filter(|e| fate(e) == Fate::Held).collect();
    if !held.is_empty() {
        println!(
            "Will be held for {} (restore with `cleaner hold restore`; holding frees no space until purged):",
            plural(p.days, "day")
        );
        for e in held {
            println!(
                "  {:>10}  [{}] {}",
                human_bytes(e.bytes),
                e.category,
                e.title
            );
        }
    }
    let skipped: Vec<(&&Entry, String)> = chosen
        .iter()
        .filter_map(|e| match fate(e) {
            Fate::Skipped(r) => Some((e, r)),
            _ => None,
        })
        .collect();
    if !skipped.is_empty() {
        println!("Can't be held, so will be skipped:");
        for (e, r) in skipped {
            println!("  [{}] {}: {r}", e.category, e.title);
        }
    }
    let gone: Vec<&&Entry> = chosen
        .iter()
        .filter(|e| fate(e) == Fate::Permanent)
        .collect();
    if !gone.is_empty() {
        println!("Will be deleted now and cannot be restored:");
        for e in gone {
            println!(
                "  {:>10}  [{}] {}",
                human_bytes(e.bytes),
                e.category,
                e.title
            );
        }
    }
}

/// "1 hold entry", "2 hold entries".
fn entries(n: usize) -> String {
    format!("{n} hold {}", if n == 1 { "entry" } else { "entries" })
}

fn cmd_hold(action: HoldCmd) -> anyhow::Result<i32> {
    use cleaner::hold::Pick;
    let paths = Paths::from_env()?;
    match action {
        HoldCmd::List => {
            let config = load_config(&paths)?;
            let held = cleaner::hold::list(&paths.hold_dir)
                .with_context(|| format!("holding area {}", paths.hold_dir.display()))?;
            if held.is_empty() {
                println!("Nothing held.");
                return Ok(0);
            }
            for h in &held {
                let expires = h.held_at + config.hold.days as i64 * 86_400;
                println!(
                    "{}  {:>10}  held {}, purged after {}  [{}] {}",
                    h.id,
                    human_bytes(h.bytes()),
                    local_time(h.held_at),
                    local_time(expires),
                    h.category,
                    h.title
                );
                if let Some(problem) = &h.problem {
                    println!("    ! {problem}");
                }
                const SHOWN: usize = 5;
                for i in h.items.iter().take(SHOWN) {
                    println!("    {}", cleaner::paths::tilde(&i.original, &paths.home));
                }
                if h.items.len() > SHOWN {
                    println!("    and {} more", h.items.len() - SHOWN);
                }
            }
            let total: u64 = held.iter().map(|h| h.bytes()).sum();
            println!(
                "Held: {} in {} (frees nothing until purged)",
                human_bytes(total),
                entries(held.len())
            );
            Ok(0)
        }
        HoldCmd::Restore { target } => {
            let path;
            let pick = if !target.contains('/') && target.starts_with(|c: char| c.is_ascii_digit())
            {
                Pick::Id(&target)
            } else {
                path = std::path::absolute(&target)?;
                Pick::Path(&path)
            };
            let (_, restored) = cleaner::engine::restore_held(&paths, pick)?;
            for p in restored {
                println!("Restored {}", p.display());
            }
            Ok(0)
        }
        HoldCmd::Purge { all, yes } => {
            let config = load_config(&paths)?;
            let cutoff = (!all).then(|| cleaner::hold::cutoff(now(), config.hold.days));
            if all {
                let held = cleaner::hold::list(&paths.hold_dir)?;
                if held.is_empty() {
                    println!("Nothing held.");
                    return Ok(0);
                }
                if !yes {
                    print!(
                        "Permanently delete {} ({})? [y/N] ",
                        entries(held.len()),
                        human_bytes(held.iter().map(|h| h.bytes()).sum())
                    );
                    std::io::stdout().flush()?;
                    let mut answer = String::new();
                    std::io::stdin().lock().read_line(&mut answer)?;
                    if !matches!(answer.trim().to_lowercase().as_str(), "y" | "yes") {
                        eprintln!("Aborted; nothing deleted.");
                        return Ok(1);
                    }
                }
            }
            let why = if all {
                "purged by `cleaner hold purge --all`".to_string()
            } else {
                format!("expired after {}", plural(config.hold.days, "day"))
            };
            let purged = cleaner::engine::purge_hold(&paths, cutoff, &why)?;
            if purged.is_empty() {
                println!("Nothing expired.");
                return Ok(0);
            }
            let mut freed = 0;
            let mut failed = 0;
            for p in &purged {
                match &p.result {
                    Ok(b) => freed += b,
                    Err(e) => {
                        failed += 1;
                        println!("failed  hold entry {}: {e}", p.held.id);
                    }
                }
            }
            println!(
                "Purged {} ({} freed)",
                entries(purged.len() - failed),
                human_bytes(freed)
            );
            Ok(if failed > 0 { 1 } else { 0 })
        }
    }
}

fn cmd_status(json: bool) -> anyhow::Result<i32> {
    let paths = Paths::from_env()?;
    let config = load_config(&paths)?;
    let fs = cleaner::disk::usage(&filesystem_paths(&config, &paths.home));
    let latest = match Plan::latest(&paths.plans_dir)? {
        Some(p) => Some((Plan::load(&p)?, p)),
        None => None,
    };
    let log = cleaner::log::read(&paths.log_file, None)?;
    let last_run: Vec<_> = match log.last() {
        Some(l) => log.iter().filter(|x| x.run_id == l.run_id).collect(),
        None => Vec::new(),
    };
    let count = |o: &str| last_run.iter().filter(|l| l.outcome == o).count();
    let deleted_bytes: u64 = last_run.iter().map(|l| l.bytes).sum();
    let used_pct = |total: u64, avail: u64| {
        if total == 0 {
            0.0
        } else {
            (total - avail.min(total)) as f64 * 100.0 / total as f64
        }
    };

    if json {
        let v = serde_json::json!({
            "filesystems": fs.iter().map(|f| serde_json::json!({
                "label": f.label, "total": f.total, "avail": f.avail,
                "used_percent": used_pct(f.total, f.avail),
            })).collect::<Vec<_>>(),
            "latest_plan": latest.as_ref().map(|(p, path)| serde_json::json!({
                "id": p.id, "path": path, "created": p.created,
                "selected_bytes": p.selected_bytes(),
                "selected_items": p.entries.iter().filter(|e| e.selected).count(),
            })),
            "last_run": last_run.last().map(|l| serde_json::json!({
                "run_id": l.run_id, "plan_id": l.plan_id, "time": l.time,
                "deleted_bytes": deleted_bytes,
                "deleted": count("deleted"), "skipped": count("skipped"), "failed": count("failed"),
            })),
        });
        println!("{}", serde_json::to_string_pretty(&v)?);
        return Ok(0);
    }
    println!("Filesystems");
    for f in &fs {
        println!(
            "  {:<24} {:>5.1}% used  {:>10} free of {}",
            f.label,
            used_pct(f.total, f.avail),
            human_bytes(f.avail),
            human_bytes(f.total)
        );
    }
    match &latest {
        Some((p, _)) => println!(
            "Latest plan: {} ({} selected in {})",
            p.id,
            human_bytes(p.selected_bytes()),
            plural(p.entries.iter().filter(|e| e.selected).count(), "item")
        ),
        None => println!("Latest plan: none"),
    }
    match last_run.last() {
        Some(l) => println!(
            "Last run: {}, deleted {} ({} deleted, {} skipped, {} failed)",
            local_time(l.time),
            human_bytes(deleted_bytes),
            count("deleted"),
            count("skipped"),
            count("failed")
        ),
        None => println!("Last run: none"),
    }
    Ok(0)
}

fn cmd_log(since_days: Option<u64>, json: bool) -> anyhow::Result<i32> {
    let paths = Paths::from_env()?;
    let since = since_days.map(|d| now().saturating_sub(d.saturating_mul(86_400) as i64));
    let lines = cleaner::log::read(&paths.log_file, since)?;
    if json {
        println!("{}", serde_json::to_string_pretty(&lines)?);
        return Ok(0);
    }
    if lines.is_empty() {
        println!("No runs logged.");
    }
    for l in &lines {
        let detail = if l.detail.is_empty() {
            String::new()
        } else {
            format!("  ({})", l.detail)
        };
        println!(
            "{}  {:<7} {:>10}  {}  {}{detail}",
            local_time(l.time),
            l.outcome,
            human_bytes(l.bytes),
            l.category,
            l.title
        );
    }
    Ok(0)
}

fn cmd_stats(json: bool, since: Option<&str>) -> anyhow::Result<i32> {
    use cleaner::stats::{Basis, Total};
    let paths = Paths::from_env()?;
    let tz = jiff::tz::TimeZone::system();
    let since_time = since
        .map(|s| cleaner::stats::parse_since(s, &tz))
        .transpose()
        .context("--since")?;
    let s = cleaner::stats::from_log(&paths.log_file, now(), &tz, since_time)?;
    if json {
        println!("{}", serde_json::to_string_pretty(&s)?);
        return Ok(0);
    }
    if s.all_time.bytes == 0 {
        match s.runs {
            0 => println!("Nothing freed yet."),
            n => println!("Nothing freed yet ({} logged).", plural(n, "run")),
        }
        return Ok(0);
    }
    // The estimate is shown next to a measurement more than 20% off it.
    let also = |differs: bool, estimated: u64| {
        if differs {
            format!("; estimate {}", human_bytes(estimated))
        } else {
            String::new()
        }
    };
    let basis = |t: &Total| {
        let est = also(t.estimate_differs(), t.estimated);
        match (t.measured, t.estimate_only) {
            (0, 0) if est.is_empty() => String::new(),
            (_, 0) => format!("(measured{est})"),
            (0, _) if est.is_empty() => "(estimated)".into(),
            (m, e) => format!(
                "({} measured, {} estimated{est})",
                human_bytes(m),
                human_bytes(e)
            ),
        }
    };
    let first = match since {
        Some(d) => format!("Since {d}"),
        None => "All time".into(),
    };
    println!("Space freed");
    for (label, t) in [
        (first.as_str(), &s.all_time),
        ("This year", &s.this_year),
        ("Last 30 days", &s.last_30_days),
    ] {
        let row = format!("  {label:<18} {:>10}  {}", human_bytes(t.bytes), basis(t));
        println!("{}", row.trim_end());
    }
    println!("Runs: {}", s.runs);
    if let Some(b) = &s.biggest_run {
        let word = match b.basis {
            Basis::Measured => "measured",
            Basis::Estimated => "estimated",
        };
        println!(
            "Biggest run: {}, {} ({word}{})",
            local_time(b.time),
            human_bytes(b.bytes),
            also(b.estimate_differs(), b.estimated)
        );
    }
    if !s.categories.is_empty() {
        println!("Top categories (estimated):");
        for c in s.categories.iter().take(5) {
            println!(
                "  {:<24} {:>10}",
                category_label(&c.category),
                human_bytes(c.bytes)
            );
        }
    }
    println!();
    println!("measured: filesystem free space after minus before a run (statvfs)");
    println!(
        "estimated: sum of the deleted items' sizes (categories, and runs logged by older versions)"
    );
    let periods = [&s.all_time, &s.this_year, &s.last_30_days];
    if periods.iter().any(|t| t.estimate_differs())
        || s.biggest_run.as_ref().is_some_and(|b| b.estimate_differs())
    {
        println!("estimate: shown next to a measurement more than 20% away from it");
    }
    Ok(0)
}

fn cmd_timer(action: TimerCmd) -> anyhow::Result<i32> {
    let paths = Paths::from_env()?;
    match action {
        TimerCmd::Install => {
            let config = load_config(&paths)?;
            let exe = cleaner::self_exe()?.canonicalize()?;
            timer::install(&paths, &config, &exe)?;
            println!(
                "Timer installed: `{}` runs daily at {}.",
                exe.display(),
                config.timer_time
            );
        }
        TimerCmd::Uninstall => {
            timer::uninstall(&paths)?;
            println!("Timer uninstalled.");
        }
        TimerCmd::Status => print!("{}", timer::status(&paths)?),
    }
    Ok(0)
}
