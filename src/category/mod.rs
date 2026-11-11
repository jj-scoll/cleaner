//! The `Category` trait every cleanup category implements (scan → plan
//! entries; apply → outcome), plus the shared apply helpers.

pub mod app_caches;
pub mod artifacts;
pub mod caches;
pub mod docker;
pub mod large_old;
pub mod nix;
pub mod store;
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
    Deleted { bytes: u64, failures: Vec<String> },
    Skipped { reason: String },
    Failed { error: String },
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
}

pub trait Category {
    fn id(&self) -> &'static str;
    fn scan(&self, ctx: &ScanCtx, seen: &mut InodeSet) -> anyhow::Result<ScanOutput>;
    fn apply(&self, entry: &Entry, ctx: &ScanCtx) -> Outcome {
        apply_default(entry, ctx)
    }
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
pub fn apply_targets(targets: &[Target], ctx: &ScanCtx) -> Outcome {
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
        Outcome::Skipped { reason: first_reason.unwrap_or_else(|| "nothing to delete".into()) }
    }
}

/// Deletes one target if it still matches; `Err(reason)` says why not.
fn apply_target(t: &Target, ctx: &ScanCtx) -> Result<u64, String> {
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
    delete_tree(&t.path).map_err(|e| format!("delete failed: {e}"))
}
