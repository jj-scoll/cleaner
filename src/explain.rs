//! `cleaner explain PATH`: why a path is (or isn't) in the plan. Read-only:
//! it consults a saved plan and the categories' own predicates, and never
//! writes a plan, the log or anything else.

use serde::Serialize;
use std::os::unix::fs::MetadataExt;
use std::path::{Component, Path, PathBuf};

use crate::category::{ScanCtx, Why};
use crate::fmt::plural;
use crate::object::Object;
use crate::plan::{Entry, EntryKind, Plan, Skip};
use crate::protect::Protection;

#[derive(Serialize, Clone, Copy, Debug, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum Status {
    /// The path is a plan entry's target (or store root).
    Planned,
    /// The path lies inside a plan entry's target.
    InsidePlanned,
    /// The path is (or is inside) a report-only row: listed, never deleted.
    ReportOnly,
    /// The plan's skip list names the path (or an ancestor).
    Skipped,
    /// Not in the plan, but a category's rules match it now: the plan predates it.
    WouldBePlanned,
    NotPlanned,
    /// Doesn't exist and isn't in the plan.
    Missing,
}

#[derive(Serialize, Clone, Debug, PartialEq)]
pub struct EntryView {
    pub id: String,
    pub category: String,
    pub title: String,
    /// The entry's path that equals or contains the query, under `~`.
    pub path: String,
    pub bytes: u64,
    pub reason: String,
    pub selected: bool,
    /// The rule that put it in the plan.
    pub rule: String,
}

#[derive(Serialize, Clone, Debug, PartialEq)]
pub struct SkipView {
    pub category: String,
    pub title: String,
    pub reason: String,
}

#[derive(Serialize, Clone, Debug, PartialEq)]
pub struct Explanation {
    /// The path asked about, under `~`, with `.`, `..` and symlinked parents
    /// resolved. A final symlink is never followed.
    pub path: String,
    pub exists: bool,
    /// Where the path points when it is itself a symlink.
    pub symlink_target: Option<String>,
    /// The id of the plan consulted, if there was one.
    pub plan: Option<String>,
    pub status: Status,
    /// The category the answer comes from, when one decided it.
    pub category: Option<String>,
    /// The answer in one line.
    pub reason: String,
    pub entry: Option<EntryView>,
    pub skip: Option<SkipView>,
}

/// Explains `path` (relative paths are taken from the current directory,
/// a leading `~` from home) against `plan`, and if the plan doesn't name it,
/// live against every category's rules. Never writes or deletes anything.
pub fn explain(path: &Path, plan: Option<&Plan>, ctx: &ScanCtx) -> Explanation {
    let q = resolve(path, ctx.home);
    let mut e = Explanation {
        path: tilde(&q.path, ctx.home),
        exists: q.md.is_some(),
        symlink_target: q
            .target
            .as_ref()
            .map(|t| tilde(&rehome(t, ctx.home), ctx.home)),
        plan: plan.map(|p| p.id.clone()),
        status: Status::NotPlanned,
        category: None,
        reason: String::new(),
        entry: None,
        skip: None,
    };
    if let Some(plan) = plan
        && let Some(hit) = lookup(plan, &q, ctx.home)
    {
        match hit {
            Hit::Entry {
                entry,
                path,
                exact,
                report_only,
            } => {
                e.status = match (report_only, exact) {
                    (true, _) => Status::ReportOnly,
                    (false, true) => Status::Planned,
                    (false, false) => Status::InsidePlanned,
                };
                e.category = Some(entry.category.clone());
                e.reason = if entry.selected || report_only {
                    entry.reason.clone()
                } else {
                    not_preselected(entry, &path, ctx)
                };
                // A store rule is re-evaluated at apply: inside its root,
                // only what matches the rule now is covered.
                if let (false, EntryKind::StoreRule { rule }) = (exact, &entry.kind)
                    && let Some(why) = crate::category::store::explain_rule(rule, &q.path, ctx)
                    && !matches!(why, Why::Match(_))
                {
                    e.status = Status::NotPlanned;
                    e.reason = why.text().to_string();
                }
                e.entry = Some(EntryView {
                    id: entry.id.clone(),
                    category: entry.category.clone(),
                    title: entry.title.clone(),
                    path: tilde(&path, ctx.home),
                    bytes: entry.bytes,
                    reason: entry.reason.clone(),
                    selected: entry.selected,
                    rule: rule_for(entry, report_only, ctx),
                });
            }
            Hit::Skip(s) => {
                e.status = Status::Skipped;
                e.category = Some(s.category.clone());
                e.reason = s.reason.clone();
                e.skip = Some(SkipView {
                    category: s.category.clone(),
                    title: s.title.clone(),
                    reason: s.reason.clone(),
                });
            }
        }
        return e;
    }
    let (status, category, reason) = live(&q, ctx);
    e.status = status;
    e.category = category;
    e.reason = reason;
    e
}

/// The path asked about, resolved without following a final symlink.
struct Query {
    /// Spelled under `home` as given (home may itself be a symlink).
    path: PathBuf,
    /// Parents canonicalized; compared against plan paths spelled the same way.
    real: PathBuf,
    /// lstat of the path, `None` if it doesn't exist.
    md: Option<std::fs::Metadata>,
    /// `readlink` of the path when it is a symlink.
    target: Option<PathBuf>,
}

fn resolve(arg: &Path, home: &Path) -> Query {
    let mut comps = arg.components();
    let arg = match comps.next() {
        Some(Component::Normal(t)) if t == "~" => home.join(comps.as_path()),
        _ => arg.to_path_buf(),
    };
    let abs = if arg.is_absolute() {
        arg
    } else {
        std::env::current_dir()
            .unwrap_or_else(|_| PathBuf::from("/"))
            .join(arg)
    };
    let real = spell(&abs);
    let md = std::fs::symlink_metadata(&real).ok();
    let target = md
        .as_ref()
        .filter(|m| m.file_type().is_symlink())
        .and_then(|_| std::fs::read_link(&real).ok())
        .map(|t| match real.parent() {
            Some(dir) if t.is_relative() => lexical(&dir.join(t)),
            _ => t,
        });
    Query {
        path: rehome(&real, home),
        real,
        md,
        target,
    }
}

/// `p` with its parent canonicalized (resolving `.`, `..` and symlinks
/// above it) and the last component kept as is, so a final symlink is not
/// followed. A path ending in `..` names a dir and is canonicalized whole;
/// a missing parent falls back to lexical cleanup.
fn spell(p: &Path) -> PathBuf {
    match (p.parent(), p.file_name()) {
        (Some(parent), Some(name)) => match std::fs::canonicalize(parent) {
            Ok(dir) => dir.join(name),
            Err(_) => lexical(p),
        },
        _ => std::fs::canonicalize(p).unwrap_or_else(|_| lexical(p)),
    }
}

/// Drops `.` and folds `..` without touching the filesystem.
fn lexical(p: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for c in p.components() {
        match c {
            Component::CurDir => {}
            Component::ParentDir => {
                out.pop();
            }
            c => out.push(c),
        }
    }
    out
}

/// A canonical path under the real home, spelled under `home` instead.
fn rehome(p: &Path, home: &Path) -> PathBuf {
    let real_home = std::fs::canonicalize(home).unwrap_or_else(|_| home.to_path_buf());
    match p.strip_prefix(&real_home) {
        Ok(rel) if rel.as_os_str().is_empty() => home.to_path_buf(),
        Ok(rel) => home.join(rel),
        Err(_) => p.to_path_buf(),
    }
}

fn tilde(p: &Path, home: &Path) -> String {
    crate::paths::tilde(p, home)
}

enum Hit<'a> {
    Entry {
        entry: &'a Entry,
        /// The entry path that equals or contains the query.
        path: PathBuf,
        exact: bool,
        report_only: bool,
    },
    Skip(&'a Skip),
}

/// The paths a plan entry covers.
fn entry_paths(e: &Entry) -> Vec<&Path> {
    match &e.kind {
        EntryKind::Item { targets } => targets.iter().map(|t| t.path.as_path()).collect(),
        EntryKind::StoreRule { rule } => vec![rule.root.as_path()],
        EntryKind::Object { obj } => match obj {
            Object::Worktree(r) => vec![r.path.as_path()],
            Object::OrphanWorktree { path, .. } | Object::Symlink { path, .. } => {
                vec![path.as_path()]
            }
            Object::NixGenerations { profile, .. } => vec![profile.as_path()],
            Object::RustupToolchain { path, .. } => vec![path.as_path()],
            Object::Docker { .. } | Object::Flatpak { .. } => Vec::new(),
        },
        EntryKind::Operation { .. } => Vec::new(),
    }
}

/// Exact matches first (entries, report-only rows, skips), then the
/// deepest entry or report-only row containing the query, then a skip
/// naming an ancestor.
fn lookup<'a>(plan: &'a Plan, q: &Query, home: &Path) -> Option<Hit<'a>> {
    let rows = plan
        .entries
        .iter()
        .map(|e| (e, false))
        .chain(plan.report_only.iter().map(|e| (e, true)));
    let mut containing: Option<(&Entry, PathBuf, bool, usize)> = None;
    for (entry, report_only) in rows {
        for p in entry_paths(entry) {
            let real = spell(p);
            if real == q.real {
                return Some(Hit::Entry {
                    entry,
                    path: p.to_path_buf(),
                    exact: true,
                    report_only,
                });
            }
            let depth = real.components().count();
            if q.real.starts_with(&real) && containing.as_ref().is_none_or(|c| depth > c.3) {
                containing = Some((entry, p.to_path_buf(), report_only, depth));
            }
        }
    }
    let names = |p: &Path| {
        let rel = p
            .strip_prefix(home)
            .ok()
            .map(|r| r.display().to_string())
            .filter(|r| !r.is_empty());
        (tilde(p, home), rel)
    };
    let skip_for = |p: &Path| {
        let (t, rel) = names(p);
        plan.skipped
            .iter()
            .find(|s| s.title == t || rel.as_ref().is_some_and(|r| &s.title == r))
    };
    if let Some(s) = skip_for(&q.path) {
        return Some(Hit::Skip(s));
    }
    if let Some((entry, path, report_only, _)) = containing {
        return Some(Hit::Entry {
            entry,
            path,
            exact: false,
            report_only,
        });
    }
    q.path.ancestors().skip(1).find_map(skip_for).map(Hit::Skip)
}

/// Why a planned entry is not pre-selected.
fn not_preselected(entry: &Entry, path: &Path, ctx: &ScanCtx) -> String {
    let from_rule = match (&entry.kind, entry.category.as_str()) {
        (EntryKind::Item { .. }, "artifacts") => {
            crate::category::artifacts::not_preselected(path, ctx)
        }
        _ => None,
    };
    from_rule
        .or_else(|| entry.note.clone())
        .unwrap_or_else(|| entry.reason.clone())
}

/// The rule that put `entry` in the plan, in words.
fn rule_for(entry: &Entry, report_only: bool, ctx: &ScanCtx) -> String {
    let cfg = ctx.config;
    match (&entry.kind, entry.category.as_str()) {
        (EntryKind::StoreRule { rule }, _) => {
            format!(
                "store rule: {}",
                crate::category::store::describe(rule, ctx.home)
            )
        }
        (EntryKind::Item { targets }, "artifacts") => targets
            .first()
            .and_then(|t| crate::category::artifacts::rule_for(&t.path, ctx))
            .unwrap_or_else(|| "project artifact".into()),
        (EntryKind::Item { .. }, "trash") => format!(
            "Trash items deleted more than {} ago",
            plural(cfg.trash.max_age_days, "day")
        ),
        (EntryKind::Item { .. }, "caches") if report_only => {
            "rustup toolchain (report only)".into()
        }
        (EntryKind::Item { .. }, "caches") => format!("keep-newest dir: {}", entry.reason),
        (EntryKind::Item { .. }, "idle_folders") => format!(
            "large and old (report only): at least {}, untouched for {}+ days",
            crate::fmt::human_bytes(cfg.idle_folders.min_bytes),
            cfg.idle_folders.idle_days
        ),
        (EntryKind::Object { obj }, _) => match obj {
            Object::Worktree(_) => {
                "finished worktree: merged or upstream gone, clean, unlocked".into()
            }
            Object::OrphanWorktree { .. } => "worktree dir git no longer lists".into(),
            Object::Symlink { .. } => "Nix result or profile link".into(),
            Object::NixGenerations { .. } => {
                format!("Nix generations beyond the newest {}", cfg.nix.keep)
            }
            Object::Docker { .. } => "unused Docker object".into(),
            Object::RustupToolchain { .. } => {
                "rustup toolchain other than the default, newest stable and pinned ones".into()
            }
            Object::Flatpak { .. } => "Flatpak runtime no installed app uses".into(),
        },
        (EntryKind::Operation { .. }, _) => "tool operation".into(),
        _ => entry.category.clone(),
    }
}

/// The first decisive reason, checked now: existence, symlink, protection,
/// owner, filesystem, git, then every category's rules (enabled or not).
fn live(q: &Query, ctx: &ScanCtx) -> (Status, Option<String>, String) {
    let not = |reason: String| (Status::NotPlanned, None, reason);
    let Some(md) = &q.md else {
        return (Status::Missing, None, "does not exist".into());
    };
    if md.file_type().is_symlink() {
        return not("symlink, not followed".into());
    }
    if let Some(p) = ctx.protector.why_protected(&q.path) {
        return not(protection_words(&p, ctx.home));
    }
    if md.uid() != ctx.euid {
        let who = match md.uid() {
            0 => "root".to_string(),
            uid => format!("uid {uid}"),
        };
        return not(format!(
            "owned by {who}; cleaner only offers your own files"
        ));
    }
    if let Some(m) = crossed_mount(q, md, ctx) {
        return not(format!(
            "on another filesystem (mount point {})",
            tilde(&rehome(&m, ctx.home), ctx.home)
        ));
    }
    if crate::git::repo_root(&q.path).is_some_and(|r| crate::git::is_tracked(&r, &q.path)) {
        return not("tracked in git".into());
    }

    let mut all = ctx.config.clone();
    all.artifacts.enabled = true;
    all.worktrees.enabled = true;
    all.caches.enabled = true;
    all.app_caches.enabled = true;
    all.trash.enabled = true;
    all.idle_folders.enabled = true;
    all.docker.enabled = true;
    all.nix.enabled = true;
    let mut best: Option<(&'static str, Why)> = None;
    for cat in crate::engine::registry(&all) {
        if let Some(why) = cat.explain(&q.path, ctx)
            && best.as_ref().is_none_or(|(_, b)| why.rank() > b.rank())
        {
            best = Some((cat.id(), why));
        }
    }
    let Some((id, why)) = best else {
        return not("no rule matches".into());
    };
    let enabled = crate::engine::registry(ctx.config)
        .iter()
        .any(|c| c.id() == id);
    let cat = Some(id.to_string());
    match why {
        Why::Match(_) | Why::NotYet(_) if !enabled => (
            Status::NotPlanned,
            cat,
            format!("category {id} disabled in config"),
        ),
        Why::NearMiss(s) if !enabled => (
            Status::NotPlanned,
            cat,
            format!("{s}; category {id} is disabled in config too"),
        ),
        Why::Match(s) => (Status::WouldBePlanned, cat, s),
        Why::NotYet(s) | Why::NearMiss(s) => (Status::NotPlanned, cat, s),
    }
}

fn protection_words(p: &Protection, home: &Path) -> String {
    let list = |builtin: bool| {
        if builtin {
            "built-in protect list"
        } else {
            "protect list"
        }
    };
    match p {
        Protection::Component(".git") => "inside .git".into(),
        Protection::Component(name) => format!("protected path (has a `{name}` component)"),
        Protection::Inside { root, builtin } => {
            format!("protected path ({}: {})", list(*builtin), tilde(root, home))
        }
        Protection::Contains { root, builtin } => format!(
            "protected path (contains {}, on the {})",
            tilde(root, home),
            list(*builtin)
        ),
    }
}

/// The mount point the path sits on, when that is another filesystem than
/// the nearest configured root above it (scan roots and overrides, store
/// roots, app dirs, Trash): every walk stays on its root's filesystem.
fn crossed_mount(q: &Query, md: &std::fs::Metadata, ctx: &ScanCtx) -> Option<PathBuf> {
    let cfg = ctx.config;
    let mut patterns: Vec<&String> = crate::engine::all_root_strings(cfg);
    patterns.extend(cfg.caches.stores.iter().map(|s| &s.path));
    patterns.extend(cfg.caches.keep_newest.iter().map(|k| &k.path));
    patterns.extend(cfg.user_rules.iter().map(|s| &s.path));
    patterns.extend(cfg.app_caches.apps.iter().flat_map(|a| &a.dirs));
    let mut roots: Vec<PathBuf> = patterns
        .into_iter()
        .flat_map(|p| crate::paths::expand(p, ctx.home))
        .map(|p| p.components().collect())
        .collect();
    roots.push(ctx.home.join(".local/share/Trash"));
    let root = roots
        .iter()
        .filter(|r| q.path.starts_with(r))
        .max_by_key(|r| r.components().count())?;
    let root_dev = std::fs::symlink_metadata(root).ok()?.dev();
    if root_dev == md.dev() {
        return None;
    }
    crate::disk::mount_point(&q.real)
}
