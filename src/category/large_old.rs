//! Report-only: large and old files and folders (spec §5.2i), read from the
//! disk index. Never selected, never proposed for deletion; shown so the user
//! can judge them. Does no walk of its own.

use std::path::Path;

use crate::category::{Category, ScanCtx, ScanOutput};
use crate::config::IdleCfg;
use crate::fsutil::measure::InodeSet;
use crate::fsutil::usage::Node;
use crate::plan::{Entry, EntryKind, Skip, Target, fingerprint};

const ID: &str = "idle_folders";
const LABEL: &str = "Large and old";
/// An index older than this is called out on every row.
const STALE_SECS: i64 = 36 * 3600;

pub struct LargeOld;

impl Category for LargeOld {
    fn id(&self) -> &'static str {
        ID
    }

    /// For each scan root, the topmost kept nodes with `bytes >= min_bytes`,
    /// nothing modified within `idle_days`, readable, and not protected (which
    /// also rules out ancestors of protected roots). The root itself is never
    /// a row. `seen` is untouched: a report row shows the real size and must
    /// not claim inodes from actionable entries.
    fn scan(&self, ctx: &ScanCtx, _seen: &mut InodeSet) -> anyhow::Result<ScanOutput> {
        let cfg = &ctx.config.idle_folders;
        let mut out = ScanOutput::default();
        if !cfg.enabled {
            return Ok(out);
        }
        let Some(idx) = ctx.usage else {
            out.skipped.push(Skip {
                category: ID.into(),
                title: LABEL.into(),
                reason: "no disk index yet; run cleaner plan".into(),
            });
            return Ok(out);
        };
        let stale = (ctx.now - idx.created > STALE_SECS).then(|| {
            let when = crate::fmt::local_time(idx.created, &jiff::tz::TimeZone::system());
            format!("disk index from {when}")
        });
        for root in crate::paths::expand_roots(&ctx.config.scan_roots, ctx.home) {
            let Some((_, node)) = idx.node(&root) else { continue };
            let mut v = Visit { ctx, cfg, stale: stale.as_deref(), out: &mut out };
            for c in &node.children {
                v.node(&root.join(&c.name), c);
            }
        }
        out.report_only.sort_by(|a, b| b.bytes.cmp(&a.bytes).then_with(|| a.title.cmp(&b.title)));
        Ok(out)
    }
}

struct Visit<'a, 'c> {
    ctx: &'a ScanCtx<'c>,
    cfg: &'a IdleCfg,
    stale: Option<&'a str>,
    out: &'a mut ScanOutput,
}

impl Visit<'_, '_> {
    fn node(&mut self, path: &Path, n: &Node) {
        let cutoff = self.ctx.now - self.cfg.idle_days as i64 * 86_400;
        let qualifies = n.bytes >= self.cfg.min_bytes
            && n.newest_mtime < cutoff
            && n.unreadable_owner.is_none()
            && !self.ctx.protector.is_protected(path);
        if qualifies {
            self.row(path, n);
        } else {
            for c in &n.children {
                self.node(&path.join(&c.name), c);
            }
        }
    }

    fn row(&mut self, path: &Path, n: &Node) {
        let Ok(fp) = fingerprint(path) else { return };
        let days = (self.ctx.now - n.newest_mtime) / 86_400;
        let reason = if n.dir {
            format!("untouched for {days} days")
        } else {
            format!("file untouched for {days} days")
        };
        let note = [git_badge(path), self.stale.map(str::to_string)]
            .into_iter()
            .flatten()
            .collect::<Vec<_>>();
        let title = match path.strip_prefix(self.ctx.home) {
            Ok(rel) => format!("~/{}", rel.display()),
            Err(_) => path.display().to_string(),
        };
        self.out.report_only.push(Entry {
            id: String::new(),
            category: ID.into(),
            title,
            kind: EntryKind::Item { targets: vec![Target { path: path.to_path_buf(), fp }] },
            bytes: n.bytes,
            reason,
            selected: false,
            permanent: false,
            note: (!note.is_empty()).then(|| note.join(" · ")),
        });
    }
}

/// What a git repo at `path` would lose, or None when `path` has no `.git`
/// (or git cannot read it).
fn git_badge(path: &Path) -> Option<String> {
    path.join(".git").symlink_metadata().ok()?;
    if !crate::git::status(path, false)?.is_empty() {
        return Some("git: uncommitted changes".into());
    }
    if !crate::git::has_remote(path) {
        return Some("git: no remote".into());
    }
    Some(match crate::git::unpushed_commits(path) {
        Some(n) if n > 0 => format!("git: {n} unpushed commits"),
        _ => "git: clean, all branches pushed".into(),
    })
}
