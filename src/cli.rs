//! The `cleaner` commands (spec §7.1): plan, show, apply, status, log, timer.

use anyhow::{Context, bail};
use clap::{CommandFactory, Parser, Subcommand};
use std::collections::HashMap;
use std::io::{BufRead, Write};
use std::path::PathBuf;

use cleaner::category::Outcome;
use cleaner::config::{self, Config};
use cleaner::engine::{apply_selected, filesystem_paths, plan_and_save, registry};
use cleaner::fmt::human_bytes;
use cleaner::notify::{DbusNotifier, gui_launch_command, notify_if_worth_it};
use cleaner::paths::Paths;
use cleaner::plan::{Entry, Plan};
use cleaner::timer;
use cleaner::view::category_order;

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
    },
    /// Delete the selected entries of a plan.
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
        Some(Cmd::Show { plan, json }) => cmd_show(plan, json),
        Some(Cmd::Apply { plan, only, yes }) => cmd_apply(plan, &only, yes),
        Some(Cmd::Status { json }) => cmd_status(json),
        Some(Cmd::Log { since, json }) => cmd_log(since, json),
        Some(Cmd::Timer { action }) => cmd_timer(action),
    }
}

fn has_display() -> bool {
    ["WAYLAND_DISPLAY", "DISPLAY"].iter().any(|v| std::env::var_os(v).is_some_and(|s| !s.is_empty()))
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
        let sel: Vec<&Entry> = plan.entries.iter().filter(|e| e.category == cat && e.selected).collect();
        let bytes: u64 = sel.iter().map(|e| e.bytes).sum();
        let report = plan.report_only.iter().filter(|e| e.category == cat).count();
        let extra = if report > 0 { format!("  (+{report} report only)") } else { String::new() };
        println!("  {cat:<14} {:>4} selected  {:>10}{extra}", sel.len(), human_bytes(bytes));
    }
    let n = plan.entries.iter().filter(|e| e.selected).count();
    println!("Total selected: {} in {n} items", human_bytes(plan.selected_bytes()));
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
        let started = std::env::current_exe().and_then(|exe| gui_launch_command(&exe, Some(id)).status());
        match started {
            Ok(s) if s.success() => {}
            Ok(s) => eprintln!("warning: starting the GUI failed: systemd-run {s}"),
            Err(e) => eprintln!("warning: starting the GUI failed: {e}"),
        }
    };
    let notifier = DbusNotifier::default();
    if let Err(e) = notify_if_worth_it(plan, threshold, &notifier, &mut launch) {
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
        let started = std::env::current_exe().and_then(|exe| gui_launch_command(&exe, id).status());
        match started {
            Ok(s) if s.success() => {}
            Ok(s) => eprintln!("warning: starting the GUI failed: systemd-run {s}"),
            Err(e) => eprintln!("warning: starting the GUI failed: {e}"),
        }
    };
    let notifier = DbusNotifier::default();
    if let Err(e) = cleaner::notify::notify_low_space(&low, saved.as_ref().map_err(Clone::clone), &notifier, &mut launch)
    {
        eprintln!("warning: notification failed: {e:#}");
    }
    Ok(0)
}

fn print_entry(e: &Entry) {
    let mark = if e.selected { "[x]" } else { "[ ]" };
    let perm = if e.permanent { " [permanent]" } else { "" };
    let (size, est) = if e.is_estimate() {
        (format!("~{}", human_bytes(e.bytes)), " [estimate]")
    } else {
        (human_bytes(e.bytes), "")
    };
    println!("  {mark} {size:>10}  {}  ({}){perm}{est}", e.title, e.reason);
    if let Some(note) = &e.note {
        println!("      note: {note}");
    }
}

fn cmd_show(id: Option<String>, json: bool) -> anyhow::Result<i32> {
    let paths = Paths::from_env()?;
    let (path, plan) = resolve_plan(&paths, id)?;
    Plan::mark_shown(&paths.state_dir, &plan.id)?;
    if json {
        println!("{}", serde_json::to_string_pretty(&plan)?);
        return Ok(0);
    }
    println!("Plan {} (created {}), {}", plan.id, local_time(plan.created), path.display());
    println!("Total selected: {}", human_bytes(plan.selected_bytes()));
    for cat in category_order(&plan) {
        let entries: Vec<&Entry> = plan.entries.iter().filter(|e| e.category == cat).collect();
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
        for e in &plan.report_only {
            println!("  {:>10}  {}  {}  ({})", human_bytes(e.bytes), e.category, e.title, e.reason);
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
    let known: Vec<&str> = registry(&Config::default()).iter().map(|c| c.id()).collect();
    for c in only {
        if !known.contains(&c.as_str()) {
            bail!("unknown category: {c} (expected one of {})", known.join(", "));
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
    if !yes {
        print!("Delete {} items ({})? [y/N] ", chosen.len(), human_bytes(total));
        std::io::stdout().flush()?;
        let mut answer = String::new();
        std::io::stdin().lock().read_line(&mut answer)?;
        if !matches!(answer.trim().to_lowercase().as_str(), "y" | "yes") {
            eprintln!("Aborted; nothing deleted.");
            return Ok(1);
        }
    }

    let ids: Vec<String> = chosen.iter().map(|e| e.id.clone()).collect();
    let report = apply_selected(&paths, &config, &plan, &ids, &mut |_| {})?;

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
            Outcome::Skipped { reason } => println!("skipped {:>10}  {label}: {reason}", ""),
            Outcome::Failed { error } => println!("failed  {:>10}  {label}: {error}", ""),
        }
    }
    println!("Freed (filesystem free space after minus before):");
    for (label, delta) in &report.freed_by_fs {
        let sign = if *delta < 0 { "-" } else { "" };
        println!("  {label}: {sign}{}", human_bytes(delta.unsigned_abs()));
    }
    Ok(0)
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
        if total == 0 { 0.0 } else { (total - avail.min(total)) as f64 * 100.0 / total as f64 }
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
            "Latest plan: {} ({} selected in {} items)",
            p.id,
            human_bytes(p.selected_bytes()),
            p.entries.iter().filter(|e| e.selected).count()
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
        let detail = if l.detail.is_empty() { String::new() } else { format!("  ({})", l.detail) };
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

fn cmd_timer(action: TimerCmd) -> anyhow::Result<i32> {
    let paths = Paths::from_env()?;
    match action {
        TimerCmd::Install => {
            let config = load_config(&paths)?;
            let exe = std::env::current_exe()?.canonicalize()?;
            timer::install(&paths, &config, &exe)?;
            println!("Timer installed: `{}` runs daily at {}.", exe.display(), config.timer_time);
        }
        TimerCmd::Uninstall => {
            timer::uninstall(&paths)?;
            println!("Timer uninstalled.");
        }
        TimerCmd::Status => print!("{}", timer::status(&paths)?),
    }
    Ok(0)
}
