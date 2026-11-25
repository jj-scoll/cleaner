//! The `Category` trait every cleanup category implements (scan → plan
//! entries; apply → outcome), plus the shared apply helpers.

pub mod ai_models;
pub mod app_caches;
pub mod artifacts;
pub mod caches;
pub mod docker;
pub mod ide_versions;
pub mod large_old;
pub mod nix;
pub mod store;
pub mod toolchains;
pub mod trash;
pub mod user_rules;
pub mod worktrees;

use serde::{Deserialize, Serialize};
use std::path::Path;

use crate::fsutil::delete::delete_tree;
use crate::fsutil::measure::InodeSet;
use crate::plan::{Entry, EntryKind, Skip, Target, fingerprint};

#[derive(Debug, Default)]
pub struct ScanOutput {
    pub entries: Vec<Entry>,
    pub skipped: Vec<Skip>,
    pub report_only: Vec<Entry>,
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub enum Outcome {
    Deleted {
        bytes: u64,
        failures: Vec<String>,
    },
    /// Moved into the holding area as hold entry `id`; `bytes` were moved,
    /// not freed (nothing is freed until the entry is purged).
    Held {
        id: String,
        bytes: u64,
        failures: Vec<String>,
    },
    Skipped {
        reason: String,
    },
    Failed {
        error: String,
    },
}

pub struct ScanCtx<'a> {
    pub home: &'a Path,
    pub now: i64,
    pub config: &'a crate::config::Config,
    pub protector: &'a crate::protect::Protector,
    /// Every external binary except `git` runs through this.
    pub tools: &'a dyn crate::tool::Runner,
    /// Effective uid; files owned by anyone else are never offered.
    pub euid: u32,
    /// The disk index from the last scan, when one is loaded.
    pub usage: Option<&'a crate::fsutil::usage::UsageIndex>,
    /// Apply only: move this entry's files and folders into the holding
    /// area instead of deleting them. Always `None` while scanning.
    pub hold: Option<crate::hold::HoldTo<'a>>,
}

/// What a category's rules say about one path, for `cleaner explain`.
/// Ordered from least to most decisive.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Why {
    /// Something a rule looks for, but a structural test fails (no marker,
    /// not a unit of the store, …).
    NearMiss(String),
    /// A rule's subject that fails only its age (or size) threshold.
    NotYet(String),
    /// A scan now would plan it (perhaps unticked); the text says how.
    Match(String),
}

impl Why {
    pub fn rank(&self) -> u8 {
        match self {
            Why::NearMiss(_) => 0,
            Why::NotYet(_) => 1,
            Why::Match(_) => 2,
        }
    }

    /// The more decisive of the two; `a` on a tie.
    pub fn best(a: Option<Why>, b: Option<Why>) -> Option<Why> {
        match (a, b) {
            (Some(a), Some(b)) if b.rank() > a.rank() => Some(b),
            (Some(a), _) => Some(a),
            (None, b) => b,
        }
    }

    pub fn text(&self) -> &str {
        match self {
            Why::NearMiss(s) | Why::NotYet(s) | Why::Match(s) => s,
        }
    }
}

pub trait Category {
    fn id(&self) -> &'static str;
    fn scan(&self, ctx: &ScanCtx, seen: &mut InodeSet) -> anyhow::Result<ScanOutput>;
    fn apply(&self, entry: &Entry, ctx: &ScanCtx) -> Outcome {
        apply_default(entry, ctx)
    }
    /// What this category's rules say about `path` (an existing, non-symlink
    /// path that is not protected), ignoring whether the category is
    /// enabled. Read-only. `None`: the category has nothing to say.
    fn explain(&self, _path: &Path, _ctx: &ScanCtx) -> Option<Why> {
        None
    }
}

/// The `Item` targets for `paths`, their bytes counted against `seen`, and
/// their newest mtime. `Err(reason)` if any path is protected, a symlink,
/// partly unreadable, or holds files owned by another uid; then nothing is
/// claimed in `seen`.
pub fn item_targets(
    paths: &[std::path::PathBuf],
    ctx: &ScanCtx,
    seen: &mut InodeSet,
) -> Result<(Vec<Target>, u64, i64), String> {
    let mut targets = Vec::new();
    let mut newest = 0;
    for p in paths {
        if ctx.protector.is_protected(p) {
            return Err(format!("{} is protected", p.display()));
        }
        // Judged on a private set first, like `store::evaluate_as`.
        let m = crate::fsutil::measure::measure_as(p, &mut InodeSet::default(), ctx.euid)
            .map_err(|e| format!("cannot measure: {e}"))?;
        if m.errors > 0 {
            return Err("partly unreadable".into());
        }
        if m.foreign > 0 {
            return Err(store::foreign_reason(m.foreign_uid.unwrap_or(0)));
        }
        newest = newest.max(m.newest_mtime);
        let fp = fingerprint(p).map_err(|e| format!("cannot fingerprint: {e}"))?;
        targets.push(Target {
            path: p.clone(),
            fp,
        });
    }
    let bytes = paths
        .iter()
        .filter_map(|p| crate::fsutil::measure::measure(p, seen).ok())
        .map(|m| m.bytes)
        .sum();
    Ok((targets, bytes, newest))
}

/// Item → `apply_targets`; StoreRule → `store::apply_rule`. Object and
/// Operation are applied only by the categories that plan them.
pub fn apply_default(entry: &Entry, ctx: &ScanCtx) -> Outcome {
    match &entry.kind {
        EntryKind::Item { targets } => apply_targets(targets, ctx),
        EntryKind::StoreRule { rule } => store::apply_rule(rule, ctx),
        EntryKind::Object { .. } | EntryKind::Operation { .. } => Outcome::Skipped {
            reason: format!("category {} can't apply this entry kind", entry.category),
        },
    }
}

/// Re-validates each target against its plan fingerprint (§6.4) and deletes
/// the ones that still match. `Deleted` if anything was deleted (per-target
/// skips and errors in `failures`); otherwise `Skipped` with the first reason.
/// With `ctx.hold` set, matching targets are moved into the holding area
/// instead (`Held`), and nothing is deleted.
pub fn apply_targets(targets: &[Target], ctx: &ScanCtx) -> Outcome {
    if let Some(to) = ctx.hold {
        let paths: Vec<&Path> = targets.iter().map(|t| t.path.as_path()).collect();
        if let Some(reason) = off_hold_device(&paths, to.area) {
            return Outcome::Skipped { reason };
        }
        let mut session = crate::hold::Session::new(to);
        if let Err(reason) = session.reserve(&paths) {
            return Outcome::Skipped { reason };
        }
        let mut failures = Vec::new();
        let mut first_reason = None;
        for t in targets {
            let r = check_target(t, ctx)
                .and_then(|()| session.hold(&t.path, Some((t.fp.dev, t.fp.ino))));
            if let Err(reason) = r {
                failures.push(format!("{}: {reason}", t.path.display()));
                first_reason.get_or_insert(reason);
            }
        }
        return held_outcome(session, failures, first_reason);
    }
    let mut deleted_any = false;
    let mut bytes = 0;
    let mut failures = Vec::new();
    let mut first_reason = None;
    for t in targets {
        match apply_target(t, ctx) {
            Ok(freed) => {
                deleted_any = true;
                bytes += freed;
            }
            Err(reason) => {
                failures.push(format!("{}: {reason}", t.path.display()));
                first_reason.get_or_insert(reason);
            }
        }
    }
    if deleted_any {
        Outcome::Deleted { bytes, failures }
    } else {
        Outcome::Skipped {
            reason: first_reason.unwrap_or_else(|| "nothing to delete".into()),
        }
    }
}

/// `Some(reason)` when any of `paths` that exists is on another filesystem
/// than the holding area: such an entry is left whole rather than half held.
pub(crate) fn off_hold_device(paths: &[&Path], area: &crate::hold::Area) -> Option<String> {
    use std::os::unix::fs::MetadataExt;
    let off = paths
        .iter()
        .find(|p| std::fs::symlink_metadata(p).is_ok_and(|md| md.dev() != area.dev()))?;
    Some(format!(
        "{} is not on the holding area's filesystem ({}); left in place, not deleted",
        off.display(),
        area.dir().display()
    ))
}

/// `Held` if the session held anything (refusals in `failures`), else
/// `Skipped` with the first reason.
pub(crate) fn held_outcome(
    session: crate::hold::Session,
    failures: Vec<String>,
    first_reason: Option<String>,
) -> Outcome {
    match session.finish() {
        Some((id, bytes)) => Outcome::Held {
            id,
            bytes,
            failures,
        },
        None => Outcome::Skipped {
            reason: first_reason.unwrap_or_else(|| "nothing to hold".into()),
        },
    }
}

/// Deletes one target if it still matches; `Err(reason)` says why not.
fn apply_target(t: &Target, ctx: &ScanCtx) -> Result<u64, String> {
    check_target(t, ctx)?;
    delete_tree(&t.path).map_err(|e| format!("delete failed: {e}"))
}

/// `Ok` if `t` may still be removed: exists, is not a symlink, is not
/// protected, matches its plan fingerprint and holds nothing git tracks.
fn check_target(t: &Target, ctx: &ScanCtx) -> Result<(), String> {
    let md = match std::fs::symlink_metadata(&t.path) {
        Ok(md) => md,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            return Err("no longer exists".into());
        }
        Err(e) => return Err(format!("cannot inspect: {e}")),
    };
    if md.file_type().is_symlink() {
        return Err("is now a symlink".into());
    }
    if ctx.protector.is_protected(&t.path) {
        return Err("protected".into());
    }
    match fingerprint(&t.path) {
        Ok(fp) if fp == t.fp => {}
        Ok(_) => return Err("changed since plan".into()),
        Err(e) => return Err(format!("cannot fingerprint: {e}")),
    }
    // Committing changes no working-tree mtime, so the fingerprint can't see it (§6.3).
    if crate::git::repo_root(&t.path).is_some_and(|repo| crate::git::is_tracked(&repo, &t.path)) {
        return Err("tracked in git".into());
    }
    Ok(())
}
