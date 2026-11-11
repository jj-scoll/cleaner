//! Finished worktrees (spec §5.2b): linked git worktrees that are clean and
//! whose branch is merged into the default branch (or whose upstream is
//! gone), removed with `git worktree remove`; plus orphaned worktree dirs
//! git no longer lists, removed with our own deleter.

use glob::Pattern;
use std::ffi::OsStr;
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};

use crate::category::artifacts::legacy_artifact_patterns;
use crate::category::{Category, Outcome, ScanCtx, ScanOutput};
use crate::fsutil::delete::delete_tree;
use crate::fsutil::measure::{InodeSet, measure, newest_mtime_excluding};
use crate::git::{self, WorktreeInfo};
use crate::object::{Object, WorktreeRef};
use crate::plan::{Entry, EntryKind, Fingerprint, Skip, fingerprint};

const ID: &str = "worktrees";

const ORPHAN_NOTE: &str = "git no longer lists this worktree; it can't check for uncommitted work";

/// How many ignored paths the note names before `(+n more)`.
const NOTE_PATHS: usize = 3;

pub struct Worktrees;

/// The pre-plan-4 artifact dir patterns (`target`, `node_modules`, …); newer
/// ecosystems never widen what counts as a disposable ignored path here.
fn artifact_patterns() -> Vec<Pattern> {
    legacy_artifact_patterns()
}

fn is_artifact(pats: &[Pattern], name: &OsStr) -> bool {
    name.to_str().is_some_and(|n| pats.iter().any(|p| p.matches(n)))
}

/// Newest file mtime under `wt`, not counting artifact dirs or `.git`, so
/// deleting the worktree's build output doesn't make it look changed.
fn newest_mtime(wt: &Path, pats: &[Pattern]) -> i64 {
    newest_mtime_excluding(wt, &|p| p.file_name().is_some_and(|n| n == ".git" || is_artifact(pats, n)))
}

/// Why the worktree counts as finished, or None (DECISION D7: no fetching,
/// no squash-merge detection). Cleanliness is checked separately. A worktree
/// on the default branch's short name (`main` for `origin/main`) is the
/// trunk, never finished work.
fn finished(main: &Path, branch: Option<&str>, head: &str) -> Option<String> {
    let default = git::default_branch(main)?;
    let trunk = default.rsplit('/').next().unwrap_or(&default);
    match branch {
        Some(b) if b == trunk => None,
        Some(b) if git::is_ancestor(main, &format!("refs/heads/{b}"), &default) => {
            Some(format!("branch {b} merged into {default}"))
        }
        Some(b) if git::upstream_gone(main, b) => Some(format!("upstream branch {b} is gone")),
        Some(_) => None,
        None => git::is_ancestor(main, head, &default).then(|| format!("detached at a commit in {default}")),
    }
}

/// Whether a linked worktree is finished (before the idle and ignored-file
/// rules the scan adds on top).
enum Verdict {
    Finished(String),
    NotFinished,
    /// Listed as a skip with this reason (locked, dirty, mount point).
    Skip(&'static str),
}

/// The finished test shared by the scan and `is_finished`: not locked, merged,
/// clean, no mount point inside. A failed check is `NotFinished` (D2).
/// The mount search doesn't descend into artifact dirs (scan cost, M10);
/// apply repeats it in full.
fn verdict(main: &Path, info: &WorktreeInfo, pats: &[Pattern]) -> Verdict {
    if info.locked {
        return Verdict::Skip("locked");
    }
    let Some(head) = &info.head else { return Verdict::NotFinished };
    let Some(why) = finished(main, info.branch.as_deref(), head) else { return Verdict::NotFinished };
    match git::status(&info.path, false) {
        Some(st) if st.is_empty() => {}
        Some(_) => return Verdict::Skip("merged, but has uncommitted changes"),
        None => return Verdict::NotFinished,
    }
    let Ok(md) = info.path.symlink_metadata() else { return Verdict::NotFinished };
    if has_mount(&info.path, md.dev(), &|n| is_artifact(pats, n)) {
        return Verdict::Skip("contains a mount point");
    }
    Verdict::Finished(why)
}

/// True if `wt` is a linked git worktree that is finished: merged, clean,
/// unlocked, no mount point. No idle or ignored-file rule.
pub fn is_finished(wt: &Path) -> bool {
    let Ok(real) = std::fs::canonicalize(wt) else { return false };
    let Some(list) = git::worktree_list(&real) else { return false };
    let main = &list[0].path;
    list[1..]
        .iter()
        .find(|w| w.path == real)
        .is_some_and(|info| matches!(verdict(main, info, &artifact_patterns()), Verdict::Finished(_)))
}

/// Ignored paths (`!!`) not inside an artifact-named dir (DECISION D1), or
/// None if git fails.
fn ignored_outside_build(wt: &Path, pats: &[Pattern]) -> Option<Vec<String>> {
    let st = git::status(wt, true)?;
    Some(
        st.into_iter()
            .filter(|(code, p)| code == "!!" && !p.iter().any(|c| is_artifact(pats, c)))
            .map(|(_, p)| p.display().to_string())
            .collect(),
    )
}

/// Why `git worktree remove` must not run, if an ignored path in `wt` is
/// protected or holds a nested repo or worktree (§5.3): git would delete it.
/// Walks each ignored dir with lstat only.
fn ignored_hazard(ctx: &ScanCtx, map: &HomeMap, wt: &Path, ignored: &[String]) -> Option<&'static str> {
    const PROTECTED: &str = "contains a protected path";
    for p in ignored {
        let top = wt.join(p);
        if protected(ctx, map, &top) {
            return Some(PROTECTED);
        }
        let mut stack = vec![top];
        while let Some(d) = stack.pop() {
            let Ok(rd) = std::fs::read_dir(&d) else { continue };
            for e in rd.flatten() {
                let name = e.file_name();
                if name == ".git" {
                    return Some("contains a nested git repo");
                }
                if protected(ctx, map, &e.path()) {
                    return Some(PROTECTED);
                }
                if e.file_type().is_ok_and(|t| t.is_dir()) {
                    stack.push(e.path());
                }
            }
        }
    }
    None
}

/// True if `dir` is a mount point or has one below it (lstat only; symlinks
/// are not followed). `git worktree remove` would empty such a filesystem.
/// Dirs whose name `skip` accepts are checked but not descended into.
fn has_mount(dir: &Path, dev: u64, skip: &dyn Fn(&OsStr) -> bool) -> bool {
    let parent_dev = dir.parent().and_then(|p| p.symlink_metadata().ok()).map(|m| m.dev());
    if parent_dev.is_some_and(|d| d != dev) {
        return true;
    }
    let mut stack = vec![dir.to_path_buf()];
    while let Some(d) = stack.pop() {
        let Ok(rd) = std::fs::read_dir(&d) else { continue };
        for e in rd.flatten() {
            let Ok(md) = e.metadata() else { continue };
            if md.dev() != dev {
                return true;
            }
            if md.is_dir() && !skip(&e.file_name()) {
                stack.push(e.path());
            }
        }
    }
    false
}

/// Git records real paths; maps one back under `home` as configured, so
/// titles, root checks and protected paths see the same spelling.
struct HomeMap<'a> {
    home: &'a Path,
    real: PathBuf,
}

impl HomeMap<'_> {
    fn new(home: &Path) -> HomeMap<'_> {
        HomeMap { home, real: std::fs::canonicalize(home).unwrap_or_else(|_| home.to_path_buf()) }
    }

    fn local(&self, p: &Path) -> PathBuf {
        match p.strip_prefix(&self.real) {
            Ok(rel) => self.home.join(rel),
            Err(_) => p.to_path_buf(),
        }
    }

    fn title(&self, p: &Path) -> String {
        let local = self.local(p);
        match local.strip_prefix(self.home) {
            Ok(rel) if rel.as_os_str().is_empty() => "~".into(),
            Ok(rel) => rel.display().to_string(),
            Err(_) => local.display().to_string(),
        }
    }
}

fn protected(ctx: &ScanCtx, map: &HomeMap, p: &Path) -> bool {
    ctx.protector.is_protected(p) || ctx.protector.is_protected(&map.local(p))
}

/// True if an enclosing repo tracks `dir` (e.g. a submodule checkout whose
/// module dir is gone): then it isn't an orphaned worktree (§6.3).
fn tracked_by_enclosing_repo(dir: &Path) -> bool {
    dir.parent()
        .and_then(git::repo_root)
        .is_some_and(|repo| git::is_tracked(&repo, dir))
}

fn skip(title: String, reason: &str) -> Skip {
    Skip { category: ID.into(), title, reason: reason.into() }
}

fn entry(title: String, obj: Object, bytes: u64, reason: String, selected: bool, note: Option<String>) -> Entry {
    Entry {
        id: String::new(),
        category: ID.into(),
        title,
        kind: EntryKind::Object { obj },
        bytes,
        reason,
        selected,
        permanent: false,
        note,
        last_active: None,
    }
}

impl Worktrees {
    #[allow(clippy::too_many_arguments)]
    fn scan_linked(
        &self,
        ctx: &ScanCtx,
        map: &HomeMap,
        pats: &[Pattern],
        main: &Path,
        info: &WorktreeInfo,
        seen: &mut InodeSet,
        out: &mut ScanOutput,
    ) {
        let cfg = &ctx.config.worktrees;
        let wt = &info.path;
        let title = map.title(wt);
        let why = match verdict(main, info, pats) {
            Verdict::Finished(why) => why,
            Verdict::NotFinished => return,
            Verdict::Skip(reason) => return out.skipped.push(skip(title, reason)),
        };
        let Some(head) = &info.head else { return };
        let Ok(md) = wt.symlink_metadata() else { return };
        let Some(ignored) = ignored_outside_build(wt, pats) else { return };
        if let Some(reason) = ignored_hazard(ctx, map, wt, &ignored) {
            return out.skipped.push(skip(title, reason));
        }
        let newest = newest_mtime(wt, pats);
        let idle = ctx.now - newest.max(git::last_commit_time(wt).unwrap_or(0));
        let bytes = match measure(wt, seen) {
            Ok(m) => m.bytes,
            Err(e) => return out.skipped.push(skip(title, &format!("cannot measure: {e}"))),
        };
        let (selected, note) = if ignored.is_empty() {
            (idle > cfg.idle_days as i64 * 86_400, None)
        } else {
            let mut note = format!("has ignored files outside build dirs: {}", ignored[..ignored.len().min(NOTE_PATHS)].join(", "));
            if ignored.len() > NOTE_PATHS {
                note.push_str(&format!(" (+{} more)", ignored.len() - NOTE_PATHS));
            }
            (false, Some(note))
        };
        let r = WorktreeRef {
            path: wt.clone(),
            main: main.to_path_buf(),
            head: head.clone(),
            branch: info.branch.clone(),
            newest_mtime: newest,
            dev: md.dev(),
            ino: md.ino(),
            ignored,
        };
        let reason = format!("{why} · idle {} days", idle.max(0) / 86_400);
        out.entries.push(entry(title, Object::Worktree(r), bytes, reason, selected, note));
    }

    /// DECISION D2: listed, never pre-selected.
    fn scan_orphan(&self, ctx: &ScanCtx, map: &HomeMap, pats: &[Pattern], dir: &Path, seen: &mut InodeSet, out: &mut ScanOutput) {
        let title = map.title(dir);
        if tracked_by_enclosing_repo(dir) {
            return out.skipped.push(skip(title, "tracked in git"));
        }
        let res = fingerprint(dir).and_then(|fp| Ok((fp, measure(dir, seen)?)));
        let (fp, m) = match res {
            Ok(x) => x,
            Err(e) => return out.skipped.push(skip(title, &format!("cannot measure: {e}"))),
        };
        let idle = ctx.now - newest_mtime(dir, pats);
        let days = idle.max(0) / 86_400;
        // A missing main repo may only be offline: say so rather than blame git (M9).
        let (reason, note) = match git::missing_main_repo(dir) {
            Some(repo) => {
                let gone = format!("main repo {} not found", map.local(&repo).display());
                (format!("{gone} · idle {days} days"), format!("{gone}; it can't check for uncommitted work"))
            }
            None => (format!("worktree git no longer lists · idle {days} days"), ORPHAN_NOTE.into()),
        };
        let obj = Object::OrphanWorktree { path: dir.to_path_buf(), fp };
        out.entries.push(entry(title, obj, m.bytes, reason, false, Some(note)));
    }

    /// Re-checks a planned worktree (§6.4) and removes it with git.
    fn apply_worktree(&self, r: &WorktreeRef, ctx: &ScanCtx) -> Outcome {
        let skipped = |reason: &str| Outcome::Skipped { reason: reason.into() };
        let map = HomeMap::new(ctx.home);
        let md = match std::fs::symlink_metadata(&r.path) {
            Ok(md) => md,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return skipped("no longer exists"),
            Err(e) => return skipped(&format!("cannot inspect: {e}")),
        };
        if md.file_type().is_symlink() {
            return skipped("is now a symlink");
        }
        if protected(ctx, &map, &r.path) {
            return skipped("protected");
        }
        if !md.is_dir() || md.dev() != r.dev || md.ino() != r.ino {
            return skipped("replaced since plan");
        }
        if has_mount(&r.path, r.dev, &|_| false) {
            return skipped("contains a mount point");
        }
        if git::head(&r.path).as_deref() != Some(r.head.as_str()) {
            return skipped("new commits since plan");
        }
        let listed = git::worktree_list(&r.main).and_then(|l| l.into_iter().skip(1).find(|w| w.path == r.path));
        let Some(info) = listed else { return skipped("no longer listed by git") };
        if info.branch != r.branch {
            return skipped("branch changed since plan");
        }
        if git::status(&r.path, false).is_none_or(|st| !st.is_empty()) {
            return skipped("has uncommitted changes");
        }
        if finished(&r.main, r.branch.as_deref(), &r.head).is_none() {
            return skipped("no longer merged");
        }
        let pats = artifact_patterns();
        // D1 at apply too: a file moved in keeps its old mtime, so check 6
        // alone can't see it.
        let Some(ignored) = ignored_outside_build(&r.path, &pats) else { return skipped("cannot list ignored files") };
        if ignored != r.ignored {
            return skipped(if r.ignored.is_empty() {
                "has ignored files outside build dirs now"
            } else {
                "ignored files outside build dirs changed since plan"
            });
        }
        if let Some(reason) = ignored_hazard(ctx, &map, &r.path, &ignored) {
            return skipped(reason);
        }
        if newest_mtime(&r.path, &pats) != r.newest_mtime {
            return skipped("changed since plan");
        }
        let bytes = measure(&r.path, &mut InodeSet::default()).map(|m| m.bytes).unwrap_or(0);
        match git::worktree_remove(&r.main, &r.path) {
            Ok(()) => Outcome::Deleted { bytes, failures: Vec::new() },
            Err(msg) => skipped(&format!("git refused: {msg}")),
        }
    }

    /// Like `apply_target`, minus the tracked check on the dir itself (git
    /// can't answer for an orphan, §5.2b); an enclosing repo is still asked.
    fn apply_orphan(&self, path: &Path, fp: &Fingerprint, ctx: &ScanCtx) -> Outcome {
        let skipped = |reason: String| Outcome::Skipped { reason };
        let md = match std::fs::symlink_metadata(path) {
            Ok(md) => md,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return skipped("no longer exists".into()),
            Err(e) => return skipped(format!("cannot inspect: {e}")),
        };
        if md.file_type().is_symlink() {
            return skipped("is now a symlink".into());
        }
        if protected(ctx, &HomeMap::new(ctx.home), path) {
            return skipped("protected".into());
        }
        match fingerprint(path) {
            Ok(now) if &now == fp => {}
            Ok(_) => return skipped("changed since plan".into()),
            Err(e) => return skipped(format!("cannot fingerprint: {e}")),
        }
        if tracked_by_enclosing_repo(path) {
            return skipped("tracked in git".into());
        }
        match delete_tree(path) {
            Ok(bytes) => Outcome::Deleted { bytes, failures: Vec::new() },
            Err(e) => Outcome::Failed { error: format!("delete failed: {e}") },
        }
    }
}

impl Category for Worktrees {
    fn id(&self) -> &'static str {
        ID
    }

    fn scan(&self, ctx: &ScanCtx, seen: &mut InodeSet) -> anyhow::Result<ScanOutput> {
        let cfg = &ctx.config.worktrees;
        let mut out = ScanOutput::default();
        if !cfg.enabled {
            return Ok(out);
        }
        let pats = artifact_patterns();
        let map = HomeMap::new(ctx.home);
        let roots = crate::paths::expand_roots(ctx.config.roots_for(&cfg.roots), ctx.home);

        let (mut repos, mut linked) = (Vec::new(), Vec::new());
        let prune = |p: &Path| p.file_name().is_some_and(|n| is_artifact(&pats, n));
        crate::walk::walk_dirs(&roots, cfg.max_depth, ctx.protector, &prune, &mut |dir, _| {
            match dir.join(".git").symlink_metadata() {
                Ok(m) if m.is_dir() => repos.push(dir.to_path_buf()),
                Ok(m) if m.is_file() => linked.push(dir.to_path_buf()),
                _ => {}
            }
        });

        // Each repo's worktree list once, keyed by its main worktree. A linked
        // worktree whose main repo the walk didn't reach adds its own list.
        let mut lists: Vec<Vec<WorktreeInfo>> = Vec::new();
        let add = |lists: &mut Vec<Vec<WorktreeInfo>>, l: Vec<WorktreeInfo>| {
            if !lists.iter().any(|k| k[0].path == l[0].path) {
                lists.push(l);
            }
        };
        for r in &repos {
            if let Some(l) = git::worktree_list(r) {
                add(&mut lists, l);
            }
        }
        let mut orphans = Vec::new();
        for l in &linked {
            if git::linked_gitdir(l).is_none() {
                orphans.push(l.clone());
                continue;
            }
            let real = std::fs::canonicalize(l).unwrap_or_else(|_| l.clone());
            if lists.iter().flatten().any(|w| w.path == real) {
                continue;
            }
            if let Some(list) = git::worktree_list(l) {
                add(&mut lists, list);
            }
        }

        let real_roots: Vec<PathBuf> = roots.iter().filter_map(|r| std::fs::canonicalize(r).ok()).collect();
        let in_roots = |p: &Path| {
            let local = map.local(p);
            roots.iter().any(|r| local.starts_with(r)) || real_roots.iter().any(|r| p.starts_with(r))
        };
        for list in &lists {
            let main = &list[0].path;
            for info in &list[1..] {
                let is_dir = info.path.symlink_metadata().is_ok_and(|m| m.is_dir());
                if info.prunable || !is_dir || !in_roots(&info.path) || protected(ctx, &map, &info.path) {
                    continue;
                }
                self.scan_linked(ctx, &map, &pats, main, info, seen, &mut out);
            }
        }
        for o in &orphans {
            if !protected(ctx, &map, o) {
                self.scan_orphan(ctx, &map, &pats, o, seen, &mut out);
            }
        }
        Ok(out)
    }

    fn apply(&self, entry: &Entry, ctx: &ScanCtx) -> Outcome {
        match &entry.kind {
            EntryKind::Object { obj: Object::Worktree(r) } => self.apply_worktree(r, ctx),
            EntryKind::Object { obj: Object::OrphanWorktree { path, fp } } => self.apply_orphan(path, fp, ctx),
            _ => crate::category::apply_default(entry, ctx),
        }
    }
}
