//! Plan and apply across all categories (spec §5.1): one shared inode set
//! for the scan; lock, re-validate, delete, measure and log for apply.

use anyhow::{Context, bail};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use crate::category::ai_models::AiModels;
use crate::category::app_caches::AppCaches;
use crate::category::artifacts::Artifacts;
use crate::category::caches::Caches;
use crate::category::docker::Docker;
use crate::category::ide_versions::IdeVersions;
use crate::category::large_old::LargeOld;
use crate::category::nix::Nix;
use crate::category::toolchains::{ToolEnv, Toolchains};
use crate::category::trash::Trash;
use crate::category::user_rules::UserRules;
use crate::category::worktrees::Worktrees;
use crate::category::{Category, Outcome, ScanCtx};
use crate::config::Config;
use crate::fsutil::measure::InodeSet;
use crate::log::{FsFreed, LogLine, RunLine};
use crate::paths::Paths;
use crate::plan::{Entry, Plan, Skip};
use crate::protect::Protector;
use crate::tool::{Runner, SystemRunner};

pub struct Ctx {
    pub paths: Paths,
    pub config: Config,
    pub protector: Protector,
    pub now: i64,
    pub tools: Arc<dyn Runner>,
    pub usage: Option<crate::fsutil::usage::UsageIndex>,
}

impl Ctx {
    pub fn scan_ctx(&self) -> ScanCtx<'_> {
        ScanCtx {
            home: &self.paths.home,
            now: self.now,
            config: &self.config,
            protector: &self.protector,
            tools: &*self.tools,
            euid: rustix::process::geteuid().as_raw(),
            usage: self.usage.as_ref(),
            hold: None,
        }
    }
}

/// The enabled categories, in display order. Artifacts come before
/// worktrees: with the shared `InodeSet`, a worktree's size then leaves out
/// artifact bytes already in the plan. Constructors do no I/O.
pub fn registry(cfg: &Config) -> Vec<Box<dyn Category>> {
    let mut cats: Vec<Box<dyn Category>> = Vec::new();
    if cfg.artifacts.enabled {
        cats.push(Box::new(Artifacts));
    }
    if cfg.worktrees.enabled {
        cats.push(Box::new(Worktrees));
    }
    if cfg.caches.enabled {
        cats.push(Box::new(Caches));
    }
    if cfg.app_caches.enabled {
        cats.push(Box::new(AppCaches {
            proc_root: PathBuf::from("/proc"),
        }));
    }
    if cfg.ide_versions.enabled {
        cats.push(Box::new(IdeVersions));
    }
    if cfg.ai_models.enabled {
        cats.push(Box::new(AiModels::from_process_env()));
    }
    if cfg.toolchains.enabled {
        cats.push(Box::new(Toolchains::new(ToolEnv::from_process())));
    }
    cats.push(Box::new(UserRules));
    if cfg.trash.enabled {
        cats.push(Box::new(Trash));
    }
    if cfg.idle_folders.enabled {
        cats.push(Box::new(LargeOld));
    }
    if cfg.docker.enabled {
        cats.push(Box::new(Docker::default()));
    }
    if cfg.nix.enabled {
        cats.push(Box::new(Nix));
    }
    cats
}

/// How far a scan or apply has got: `done` of `total` items finished, and
/// `current` is the category id (scan) or entry title (apply) starting next.
/// The last event has `done == total` and an empty `current`.
#[derive(Clone, Debug, PartialEq)]
pub struct Progress {
    pub done: usize,
    pub total: usize,
    pub current: String,
    /// Scan only: bytes of the entries found so far. Always 0 for apply.
    pub found: u64,
}

/// `build_plan_with` without progress.
pub fn build_plan(ctx: &Ctx, cats: &[Box<dyn Category>]) -> anyhow::Result<Plan> {
    build_plan_with(ctx, cats, &mut |_| {})
}

/// Scans every category with one shared `InodeSet` and assigns entry ids
/// `"{category}:{n}"` (n counts entries and report-only rows together). A
/// category that errors becomes a `Skip` instead of failing the plan.
/// Reports progress before each category and once at the end.
pub fn build_plan_with(
    ctx: &Ctx,
    cats: &[Box<dyn Category>],
    progress: &mut dyn FnMut(Progress),
) -> anyhow::Result<Plan> {
    let sc = ctx.scan_ctx();
    let mut seen = InodeSet::default();
    let mut plan = Plan {
        id: Plan::new_id(ctx.now),
        created: ctx.now,
        entries: Vec::new(),
        skipped: Vec::new(),
        report_only: Vec::new(),
    };
    let total = cats.len();
    let mut found = 0;
    for (done, cat) in cats.iter().enumerate() {
        progress(Progress {
            done,
            total,
            current: cat.id().into(),
            found,
        });
        let out = match cat.scan(&sc, &mut seen) {
            Ok(out) => out,
            Err(e) => {
                plan.skipped.push(Skip {
                    category: cat.id().into(),
                    title: cat.id().into(),
                    reason: format!("category failed: {e:#}"),
                });
                continue;
            }
        };
        let mut n = 0;
        let mut number = |mut e: Entry| {
            e.id = format!("{}:{n}", cat.id());
            n += 1;
            e
        };
        found += out.entries.iter().map(|e| e.bytes).sum::<u64>();
        plan.entries
            .extend(out.entries.into_iter().map(&mut number));
        plan.report_only
            .extend(out.report_only.into_iter().map(&mut number));
        plan.skipped.extend(out.skipped);
    }
    plan.skipped
        .extend(root_skips(&ctx.config, &ctx.paths.home));
    progress(Progress {
        done: total,
        total,
        current: String::new(),
        found,
    });
    Ok(plan)
}

pub struct RunReport {
    pub run_id: String,
    /// (entry id, outcome), in plan order.
    pub results: Vec<(String, Outcome)>,
    /// (filesystem label, avail after − avail before) in bytes.
    pub freed_by_fs: Vec<(String, i64)>,
    /// Expired hold entries purged before anything else was applied.
    pub purged: Vec<crate::hold::Purged>,
}

/// Every root string the config names: `scan_roots`, the artifacts/worktrees/nix
/// overrides and `disk_map.roots`, distinct, in that order.
pub(crate) fn all_root_strings(config: &Config) -> Vec<&String> {
    let mut out: Vec<&String> = Vec::new();
    let lists = [
        Some(&config.scan_roots),
        config.artifacts.roots.as_ref(),
        config.worktrees.roots.as_ref(),
        config.nix.roots.as_ref(),
        Some(&config.disk_map.roots),
    ];
    for r in lists.into_iter().flatten().flatten() {
        if !out.contains(&r) {
            out.push(r);
        }
    }
    out
}

/// `config.filesystems` (`~` expanded, first so existing labels keep), then the
/// mount point of every root in `scan_roots`, `disk_map.roots` and the
/// artifacts/worktrees/nix overrides. `disk::usage` dedupes by `st_dev`.
pub fn filesystem_paths(config: &Config, home: &Path) -> Vec<PathBuf> {
    // Re-collecting components drops the trailing `/` that `expand("~")` leaves.
    let mut out: Vec<PathBuf> = config
        .filesystems
        .iter()
        .flat_map(|f| crate::paths::expand(f, home))
        .map(|p| p.components().collect())
        .collect();
    // Not `expand_roots`: a nested root (a symlink under `~`) can be on another filesystem.
    for root in all_root_strings(config)
        .into_iter()
        .flat_map(|r| crate::paths::expand(r, home))
    {
        // Resolve a symlinked root first so the label is its target's mount point.
        let real = std::fs::canonicalize(&root).unwrap_or(root);
        if let Some(m) = crate::disk::mount_point(&real)
            && !out.contains(&m)
        {
            out.push(m);
        }
    }
    out
}

/// One `Skip` per distinct root string (scan roots, overrides, disk-map roots)
/// whose expanded path is a symlink. The walks refuse a symlinked root, so
/// without this it would find nothing, silently.
pub fn root_skips(config: &Config, home: &Path) -> Vec<Skip> {
    let mut out = Vec::new();
    for root in all_root_strings(config) {
        for p in crate::paths::expand(root, home) {
            // `readlink` fails with EINVAL on a path ending in `/`; the walks strip it too.
            let p: PathBuf = p.components().collect();
            let Ok(target) = std::fs::read_link(&p) else {
                continue;
            };
            out.push(Skip {
                category: "scan_roots".into(),
                title: root.clone(),
                reason: format!(
                    "{root} is a symlink to {}; add that path instead",
                    target.display()
                ),
            });
            break;
        }
    }
    out
}

/// `apply_plan_with` without progress.
pub fn apply_plan(
    ctx: &Ctx,
    cats: &[Box<dyn Category>],
    plan: &Plan,
    ids: &[String],
) -> anyhow::Result<RunReport> {
    apply_plan_with(ctx, cats, plan, ids, &mut |_| {})
}

/// Applies the plan entries named by `ids` (in plan order) while holding the
/// run lock. Every id must name a plan entry, checked before anything is
/// deleted. Each entry goes to its category's `apply`; an entry whose
/// category isn't enabled now is skipped. Appends one log line per entry.
/// Reports progress before each entry and once at the end.
pub fn apply_plan_with(
    ctx: &Ctx,
    cats: &[Box<dyn Category>],
    plan: &Plan,
    ids: &[String],
    progress: &mut dyn FnMut(Progress),
) -> anyhow::Result<RunReport> {
    let _lock = crate::lock::acquire(&ctx.paths.lock_file)?;
    for id in ids {
        if !plan.entries.iter().any(|e| &e.id == id) {
            bail!("unknown entry id: {id}");
        }
    }
    let run_id = Plan::new_id(ctx.now);
    let fs_paths = filesystem_paths(&ctx.config, &ctx.paths.home);
    let before = crate::disk::usage(&fs_paths);

    let mut lines = Vec::new();
    let purged = purge_expired(ctx, &run_id, plan, &mut lines);
    // Opened (and created) only when holding is on; `Err` skips every entry it would hold.
    let area = ctx
        .config
        .hold
        .enabled
        .then(|| crate::hold::Area::open(&ctx.paths.hold_dir));

    let sc = ctx.scan_ctx();
    let mut results = Vec::new();
    let chosen: Vec<&Entry> = plan
        .entries
        .iter()
        .filter(|e| ids.contains(&e.id))
        .collect();
    let total = chosen.len();
    for (done, entry) in chosen.into_iter().enumerate() {
        progress(Progress {
            done,
            total,
            current: entry.title.clone(),
            found: 0,
        });
        let held_area = area.as_ref().filter(|_| crate::hold::holdable(entry));
        let outcome = match (cats.iter().find(|c| c.id() == entry.category), held_area) {
            (None, _) => Outcome::Skipped {
                reason: format!("category {} is not enabled", entry.category),
            },
            (Some(cat), None) => cat.apply(entry, &sc),
            (Some(_), Some(Err(e))) => Outcome::Skipped {
                reason: format!("holding area unavailable ({e}); left in place, not deleted"),
            },
            (Some(cat), Some(Ok(area))) => {
                let to = crate::hold::HoldTo {
                    area,
                    plan_id: &plan.id,
                    entry_id: &entry.id,
                    category: &entry.category,
                    title: &entry.title,
                    now: ctx.now,
                };
                cat.apply(
                    entry,
                    &ScanCtx {
                        hold: Some(to),
                        ..ctx.scan_ctx()
                    },
                )
            }
        };
        let (word, bytes, detail) = match &outcome {
            Outcome::Deleted { bytes, failures } => ("deleted", *bytes, failures.join("; ")),
            Outcome::Held {
                id,
                bytes,
                failures,
            } => {
                let mut d = format!(
                    "moved {} to hold entry {id}; frees nothing until purged",
                    crate::fmt::human_bytes(*bytes)
                );
                if !failures.is_empty() {
                    d.push_str("; ");
                    d.push_str(&failures.join("; "));
                }
                ("held", 0, d)
            }
            Outcome::Skipped { reason } => ("skipped", 0, reason.clone()),
            Outcome::Failed { error } => ("failed", 0, error.clone()),
        };
        lines.push(LogLine {
            time: jiff::Timestamp::now().as_second(),
            run_id: run_id.clone(),
            plan_id: plan.id.clone(),
            entry_id: entry.id.clone(),
            category: entry.category.clone(),
            title: entry.title.clone(),
            outcome: word.into(),
            bytes,
            detail,
        });
        results.push((entry.id.clone(), outcome));
    }
    progress(Progress {
        done: total,
        total,
        current: String::new(),
        found: 0,
    });

    let after = crate::disk::usage(&fs_paths);
    let freed_by_fs: Vec<(String, i64)> = before
        .iter()
        .filter_map(|b| {
            let a = after.iter().find(|a| a.label == b.label)?;
            Some((b.label.clone(), a.avail as i64 - b.avail as i64))
        })
        .collect();
    // With nothing deleted (or nothing measured) the delta is only noise.
    let run =
        (lines.iter().any(|l| l.outcome == "deleted") && !freed_by_fs.is_empty()).then(|| {
            RunLine {
                kind: "run".into(),
                time: jiff::Timestamp::now().as_second(),
                run_id: run_id.clone(),
                freed_by_fs: freed_by_fs
                    .iter()
                    .map(|(fs, bytes)| FsFreed {
                        fs: fs.clone(),
                        bytes: *bytes,
                    })
                    .collect(),
            }
        });
    crate::log::append(&ctx.paths.log_file, &lines, run.as_ref())?;
    Ok(RunReport {
        run_id,
        results,
        freed_by_fs,
        purged,
    })
}

/// Purges hold entries older than `[hold] days`, whether or not holding is
/// on now (turning it off must not strand what it already holds), and logs
/// one `hold` line per entry. Runs only inside an apply the user started,
/// so nothing is ever deleted unattended.
fn purge_expired(
    ctx: &Ctx,
    run_id: &str,
    plan: &Plan,
    lines: &mut Vec<LogLine>,
) -> Vec<crate::hold::Purged> {
    let cutoff = crate::hold::cutoff(ctx.now, ctx.config.hold.days);
    let purged = match crate::hold::purge(&ctx.paths.hold_dir, Some(cutoff)) {
        Ok(p) => p,
        Err(e) => {
            lines.push(hold_line(
                run_id,
                &plan.id,
                "holding area",
                "failed",
                0,
                format!(
                    "expiry purge skipped: {}: {e}",
                    ctx.paths.hold_dir.display()
                ),
            ));
            return Vec::new();
        }
    };
    let why = format!(
        "expired after {}",
        crate::fmt::plural(ctx.config.hold.days, "day")
    );
    lines.extend(
        purged
            .iter()
            .map(|p| purged_line(run_id, &plan.id, p, &why)),
    );
    purged
}

fn purged_line(run_id: &str, plan_id: &str, p: &crate::hold::Purged, why: &str) -> LogLine {
    let title = format!("hold entry {} ({})", p.held.id, p.held.title);
    match &p.result {
        Ok(freed) => hold_line(run_id, plan_id, &title, "deleted", *freed, why.into()),
        Err(e) => hold_line(run_id, plan_id, &title, "failed", 0, e.clone()),
    }
}

/// `cleaner hold purge`: under the run lock, deletes the hold entries held
/// at or before `cutoff` (every one if `None`) and logs each (`why` is the
/// detail of a successful line).
pub fn purge_hold(
    paths: &Paths,
    cutoff: Option<i64>,
    why: &str,
) -> anyhow::Result<Vec<crate::hold::Purged>> {
    let _lock = crate::lock::acquire(&paths.lock_file)?;
    let purged = crate::hold::purge(&paths.hold_dir, cutoff)
        .with_context(|| format!("holding area {}", paths.hold_dir.display()))?;
    let run_id = Plan::new_id(jiff::Timestamp::now().as_second());
    let lines: Vec<LogLine> = purged
        .iter()
        .map(|p| purged_line(&run_id, "", p, why))
        .collect();
    crate::log::append(&paths.log_file, &lines, None)?;
    Ok(purged)
}

/// `cleaner hold restore`: `hold::restore` under the run lock, logged as
/// one `restored` line per path put back.
pub fn restore_held(
    paths: &Paths,
    pick: crate::hold::Pick,
) -> anyhow::Result<(crate::hold::Held, Vec<PathBuf>)> {
    let _lock = crate::lock::acquire(&paths.lock_file)?;
    let (held, restored) = crate::hold::restore(&paths.hold_dir, pick)?;
    let run_id = Plan::new_id(jiff::Timestamp::now().as_second());
    let lines: Vec<LogLine> = restored
        .iter()
        .map(|p| {
            let mut l = hold_line(
                &run_id,
                &held.plan_id,
                &held.title,
                "restored",
                0,
                format!("from hold entry {} to {}", held.id, p.display()),
            );
            l.entry_id = held.entry_id.clone();
            l.category = held.category.clone();
            l
        })
        .collect();
    crate::log::append(&paths.log_file, &lines, None)?;
    Ok((held, restored))
}

fn hold_line(
    run_id: &str,
    plan_id: &str,
    title: &str,
    outcome: &str,
    bytes: u64,
    detail: String,
) -> LogLine {
    LogLine {
        time: jiff::Timestamp::now().as_second(),
        run_id: run_id.into(),
        plan_id: plan_id.into(),
        entry_id: "hold:purge".into(),
        category: "hold".into(),
        title: title.into(),
        outcome: outcome.into(),
        bytes,
        detail,
    }
}

/// Protects the user's `protect` list and the holding area: no scan ever
/// offers what is already held, or a folder containing it.
pub fn protector_for(paths: &Paths, config: &Config) -> Protector {
    Protector::new(&paths.home, &config.protect).with_root(paths.hold_dir.clone())
}

fn make_ctx(paths: &Paths, config: &Config, now: i64) -> Ctx {
    Ctx {
        protector: protector_for(paths, config),
        paths: paths.clone(),
        config: config.clone(),
        now,
        tools: Arc::new(SystemRunner),
        usage: None,
    }
}

/// Whether a plan rebuilds the disk usage index or reads the one on disk.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum IndexMode {
    Rebuild,
    Reuse,
}

/// `plan_and_save_with` rebuilding the index.
pub fn plan_and_save(
    paths: &Paths,
    config: &Config,
    now: i64,
    progress: &mut dyn FnMut(Progress),
) -> anyhow::Result<(Plan, PathBuf)> {
    plan_and_save_with(paths, config, now, IndexMode::Rebuild, progress)
}

/// Takes the run lock, builds (Rebuild, saved to `paths.usage_index`) or loads
/// (Reuse, nothing written) the disk usage index, scans the enabled
/// categories with it, saves the plan and drops the lock. A failed index
/// save is a `disk_map` skip; the plan is still saved. Rebuild reports one
/// `Progress` ("disk index") before the walk. Writing the default config and
/// loading it is the caller's job.
pub fn plan_and_save_with(
    paths: &Paths,
    config: &Config,
    now: i64,
    mode: IndexMode,
    progress: &mut dyn FnMut(Progress),
) -> anyhow::Result<(Plan, PathBuf)> {
    use crate::fsutil::usage::{EXCLUDE, MIN_NODE, UsageIndex, build, index_roots};
    let _lock = crate::lock::acquire(&paths.lock_file)?;
    let mut ctx = make_ctx(paths, config, now);
    let cats = registry(config);
    let mut save_error = None;
    match mode {
        IndexMode::Rebuild => {
            progress(Progress {
                done: 0,
                total: cats.len(),
                current: "disk index".into(),
                found: 0,
            });
            let exclude: Vec<PathBuf> = EXCLUDE.iter().map(PathBuf::from).collect();
            let idx = build(&index_roots(config, &paths.home), &exclude, MIN_NODE, now);
            if let Err(e) = idx.save(&paths.usage_index) {
                save_error = Some(format!("not saved: {e:#}"));
            }
            ctx.usage = Some(idx);
        }
        IndexMode::Reuse => ctx.usage = UsageIndex::load(&paths.usage_index).ok().flatten(),
    }
    let mut plan = build_plan_with(&ctx, &cats, progress)?;
    if let Some(reason) = save_error {
        plan.skipped.push(Skip {
            category: "disk_map".into(),
            title: "disk index".into(),
            reason,
        });
    }
    let path = plan.save(&paths.plans_dir)?;
    Ok((plan, path))
}

/// Applies the plan entries named by `ids` with the enabled categories, as of
/// now (`apply_plan_with` takes the run lock).
pub fn apply_selected(
    paths: &Paths,
    config: &Config,
    plan: &Plan,
    ids: &[String],
    progress: &mut dyn FnMut(Progress),
) -> anyhow::Result<RunReport> {
    let ctx = make_ctx(paths, config, jiff::Timestamp::now().as_second());
    apply_plan_with(&ctx, &registry(config), plan, ids, progress)
}
