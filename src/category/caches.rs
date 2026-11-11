//! Tool caches (spec §5.2c): store rules, keep-newest-N directories,
//! `uv cache prune` as a tool operation, and rustup toolchains as
//! report-only rows.

use std::ffi::OsStr;
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};

use crate::category::store::scan_stores;
use crate::category::{Category, Outcome, ScanCtx, ScanOutput, apply_default};
use crate::fsutil::measure::{InodeSet, measure};
use crate::object::Operation;
use crate::plan::{Entry, EntryKind, Skip, Target, fingerprint};
use crate::tool::{OP_TIMEOUT, QUERY_TIMEOUT, ToolError, parse_human_size};

pub struct Caches;

const ID: &str = "caches";

impl Category for Caches {
    fn id(&self) -> &'static str {
        ID
    }

    fn scan(&self, ctx: &ScanCtx, seen: &mut InodeSet) -> anyhow::Result<ScanOutput> {
        let cfg = &ctx.config.caches;
        if !cfg.enabled {
            return Ok(ScanOutput::default());
        }
        // Older keep-newest children are computed first so a store root inside
        // one of them is not listed twice; the Items are pushed after the stores.
        let mut older: Vec<(PathBuf, usize)> = Vec::new();
        for keep in &cfg.keep_newest {
            for root in crate::paths::expand(&keep.path, ctx.home) {
                for child in older_children(&root, keep.keep, ctx) {
                    older.push((child, keep.keep));
                }
            }
        }
        let mut out = scan_stores(
            ID,
            &cfg.stores,
            ctx,
            seen,
            &|n, days| format!("{n} items untouched for {days}+ days"),
            &|r| older.iter().any(|(o, _)| r.starts_with(o)),
        );

        for (child, keep) in &older {
            let reason = format!("older run; keeping newest {keep}");
            if let Some(e) = item(child, &reason, true, seen) {
                out.entries.push(e);
            }
        }

        if cfg.uv_prune {
            match uv_prune_entry(ctx) {
                Ok(e) => out.entries.push(e),
                Err(reason) => out.skipped.push(Skip {
                    category: ID.into(),
                    title: UV_TITLE.into(),
                    reason,
                }),
            }
        }

        if cfg.go_clean {
            match go_modcache_entry(ctx) {
                Ok(Some(e)) => out.entries.push(e),
                Ok(None) => {}
                Err(reason) => out.skipped.push(Skip {
                    category: ID.into(),
                    title: GO_TITLE.into(),
                    reason,
                }),
            }
        }

        for tc in crate::paths::expand("~/.rustup/toolchains/*", ctx.home) {
            if ctx.protector.is_protected(&tc) {
                continue;
            }
            let reason = "rustup is not installed; toolchain may still be used by an IDE";
            if let Some(e) = item(&tc, reason, false, seen) {
                out.report_only.push(e);
            }
        }
        Ok(out)
    }

    fn apply(&self, entry: &Entry, ctx: &ScanCtx) -> Outcome {
        match &entry.kind {
            EntryKind::Operation { op: Operation::UvCachePrune } => uv_prune(ctx),
            EntryKind::Operation { op: Operation::GoCleanModcache } => go_clean(ctx),
            _ => apply_default(entry, ctx),
        }
    }
}

const UV_TITLE: &str = "uv cache prune";

const UV_IN_USE: &str = "uv cache is in use by running uv processes";

/// `uv cache dir`, or `Err((uv is missing, message))`.
fn uv_cache_dir(ctx: &ScanCtx) -> Result<PathBuf, (bool, String)> {
    let out = ctx.tools.run("uv", &[OsStr::new("cache"), OsStr::new("dir")], QUERY_TIMEOUT);
    let out = out.map_err(|e| (matches!(e, ToolError::Missing(_)), e.to_string()))?;
    if !out.ok() {
        return Err((false, format!("uv cache dir failed: {}", first_line(&out.stderr))));
    }
    Ok(PathBuf::from(out.stdout.trim()))
}

/// True if a running uv holds `<cache>/.lock` (long-lived `uvx` servers
/// hold a shared flock all day). Without `--force`, which we never pass,
/// prune would wait for an exclusive lock until it times out. The probe
/// takes the exclusive lock without blocking and drops it at once.
fn uv_cache_in_use(dir: &Path) -> bool {
    let Ok(f) = std::fs::File::open(dir.join(".lock")) else { return false };
    matches!(f.try_lock(), Err(std::fs::TryLockError::WouldBlock))
}

/// The `uv cache prune` row (DECISION D5: size unknown until uv runs, so
/// `bytes` is 0 and the note gives the whole cache's size), or why not.
fn uv_prune_entry(ctx: &ScanCtx) -> Result<Entry, String> {
    let dir = uv_cache_dir(ctx).map_err(|(_, e)| e)?;
    if ctx.protector.is_protected(&dir) {
        return Err("protected".into());
    }
    // A fresh InodeSet: this is a note about the whole cache, not plan bytes.
    let cache = measure(&dir, &mut InodeSet::default()).map_or(0, |m| m.bytes);
    let mut note = format!("uv reports the size after it runs; cache is {}", crate::fmt::human_bytes(cache));
    if uv_cache_in_use(&dir) {
        note.push_str("; in use by running uv processes now, so apply may skip it");
    }
    Ok(Entry {
        id: String::new(),
        category: ID.into(),
        title: UV_TITLE.into(),
        kind: EntryKind::Operation { op: Operation::UvCachePrune },
        bytes: 0,
        reason: "removes cache entries no venv uses (uv decides)".into(),
        selected: true,
        permanent: false,
        note: Some(note),
    })
}

/// Runs `uv cache prune` (never `--force`: uv keeps its own lock and
/// in-use checks) and reports the size uv says it removed. Skipped while a
/// running uv holds the cache lock, instead of waiting out the timeout.
fn uv_prune(ctx: &ScanCtx) -> Outcome {
    match uv_cache_dir(ctx) {
        Ok(dir) if uv_cache_in_use(&dir) => return Outcome::Skipped { reason: UV_IN_USE.into() },
        Ok(_) => {}
        Err((true, e)) => return Outcome::Skipped { reason: e },
        Err((false, e)) => return Outcome::Failed { error: e },
    }
    match ctx.tools.run("uv", &[OsStr::new("cache"), OsStr::new("prune")], OP_TIMEOUT) {
        Ok(out) if out.ok() => {
            let text = format!("{}\n{}", out.stdout, out.stderr);
            Outcome::Deleted { bytes: removed_bytes(&text), failures: vec![] }
        }
        Ok(out) => Outcome::Failed {
            error: format!("{UV_TITLE} failed: {}", first_line(&out.stderr)),
        },
        Err(ToolError::Timeout(_, secs)) => Outcome::Failed {
            error: format!("{UV_TITLE} timed out after {secs}s (uv cache in use?)"),
        },
        Err(e @ ToolError::Missing(_)) => Outcome::Skipped { reason: e.to_string() },
        Err(e) => Outcome::Failed { error: format!("{UV_TITLE} failed: {e}") },
    }
}

const GO_TITLE: &str = "go clean -modcache";

/// `go env GOMODCACHE`, or `Err(go is missing)`; `Ok(None)` for any other
/// failure or an empty answer.
fn go_modcache_dir(ctx: &ScanCtx) -> Result<Option<PathBuf>, ToolError> {
    match ctx.tools.run("go", &[OsStr::new("env"), OsStr::new("GOMODCACHE")], QUERY_TIMEOUT) {
        Ok(out) if out.ok() && !out.stdout.trim().is_empty() => {
            Ok(Some(PathBuf::from(out.stdout.trim())))
        }
        Ok(_) => Ok(None),
        Err(e @ ToolError::Missing(_)) => Err(e),
        Err(_) => Ok(None),
    }
}

/// The `go clean -modcache` row: unticked, sized by measuring the whole
/// module cache (an estimate; go decides what goes). `Ok(None)` when go
/// names no usable directory.
fn go_modcache_entry(ctx: &ScanCtx) -> Result<Option<Entry>, String> {
    let Some(dir) = go_modcache_dir(ctx).map_err(|e| e.to_string())? else { return Ok(None) };
    let is_dir = std::fs::symlink_metadata(&dir).is_ok_and(|md| md.is_dir());
    if !is_dir || ctx.protector.is_protected(&dir) {
        return Ok(None);
    }
    let bytes = measure(&dir, &mut InodeSet::default()).map_or(0, |m| m.bytes);
    Ok(Some(Entry {
        id: String::new(),
        category: ID.into(),
        title: GO_TITLE.into(),
        kind: EntryKind::Operation { op: Operation::GoCleanModcache },
        bytes,
        reason: "removes the whole Go module cache, including modules in use".into(),
        selected: false,
        permanent: false,
        note: Some("go decides what goes; size is the whole cache".into()),
    }))
}

/// Runs `go clean -modcache`; freed bytes are the cache's measured size
/// before minus after.
fn go_clean(ctx: &ScanCtx) -> Outcome {
    let dir = match go_modcache_dir(ctx) {
        Ok(d) => d,
        Err(e) => return Outcome::Skipped { reason: e.to_string() },
    };
    let size = |d: &Option<PathBuf>| {
        d.as_ref().and_then(|d| measure(d, &mut InodeSet::default()).ok()).map_or(0, |m| m.bytes)
    };
    let before = size(&dir);
    match ctx.tools.run("go", &[OsStr::new("clean"), OsStr::new("-modcache")], OP_TIMEOUT) {
        Ok(out) if out.ok() => {
            Outcome::Deleted { bytes: before.saturating_sub(size(&dir)), failures: vec![] }
        }
        Ok(out) => Outcome::Failed {
            error: format!("{GO_TITLE} failed: {}", first_line(&out.stderr)),
        },
        Err(ToolError::Timeout(_, secs)) => {
            Outcome::Failed { error: format!("{GO_TITLE} timed out after {secs}s") }
        }
        Err(e @ ToolError::Missing(_)) => Outcome::Skipped { reason: e.to_string() },
        Err(e) => Outcome::Failed { error: format!("{GO_TITLE} failed: {e}") },
    }
}

/// The size in `Removed N files (12.5MiB)`; 0 if uv printed no such line
/// (e.g. `No unused entries found`).
fn removed_bytes(text: &str) -> u64 {
    text.lines()
        .filter(|l| l.trim_start().starts_with("Removed "))
        .find_map(|l| {
            let inner = &l[l.rfind('(')? + 1..];
            parse_human_size(&inner[..inner.find(')')?])
        })
        .unwrap_or(0)
}

fn first_line(s: &str) -> &str {
    s.lines().map(str::trim).find(|l| !l.is_empty()).unwrap_or("no error output")
}

/// Children of `root` (not symlinks, not protected, not mount points) other
/// than the `keep` with the newest mtime inside them. A symlinked root has no
/// children.
fn older_children(root: &Path, keep: usize, ctx: &ScanCtx) -> Vec<PathBuf> {
    let dev = match std::fs::symlink_metadata(root) {
        Ok(md) if md.is_dir() => md.dev(),
        _ => return Vec::new(),
    };
    let Ok(rd) = std::fs::read_dir(root) else { return Vec::new() };
    let mut kids: Vec<(i64, PathBuf)> = rd
        .flatten()
        .filter(|e| e.metadata().is_ok_and(|md| md.dev() == dev)) // lstat
        .map(|e| e.path())
        .filter(|p| !ctx.protector.is_protected(p))
        .filter_map(|p| Some((measure(&p, &mut InodeSet::default()).ok()?.newest_mtime, p)))
        .collect();
    // Newest first; ties broken by path so the result is deterministic.
    kids.sort_by(|a, b| b.0.cmp(&a.0).then_with(|| a.1.cmp(&b.1)));
    kids.into_iter().skip(keep).map(|(_, p)| p).collect()
}

/// A one-target `Item` entry for `path`, or `None` if it vanished or is a
/// symlink (`measure`/`fingerprint` refuse symlinks).
fn item(path: &Path, reason: &str, selected: bool, seen: &mut InodeSet) -> Option<Entry> {
    let fp = fingerprint(path).ok()?;
    let bytes = measure(path, seen).ok()?.bytes;
    Some(Entry {
        id: String::new(),
        category: ID.into(),
        title: path.display().to_string(),
        kind: EntryKind::Item { targets: vec![Target { path: path.to_path_buf(), fp }] },
        bytes,
        reason: reason.into(),
        selected,
        permanent: false,
        note: None,
    })
}
