//! Git helpers. `git` runs directly, not through `tool::Runner`; when it is
//! missing or fails, a path counts as tracked and a worktree as not finished
//! (fail-safe).

use std::ffi::OsStr;
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};

/// Runs `git -C repo <args>`, isolated from any GIT_DIR-style overrides in our
/// own environment (e.g. when run from inside a git hook).
fn git(repo: &Path, args: &[&OsStr]) -> Option<Output> {
    let out = command(repo, args).stderr(Stdio::null()).output().ok()?;
    out.status.success().then_some(out)
}

/// `git --literal-pathspecs --no-optional-locks -C repo <args>` with stdin
/// null and no GIT_DIR-style overrides. No optional locks, so read-only
/// commands such as `status` never rewrite `.git/index` (which would make a
/// report-only scan touch the repo it inspects).
fn command(repo: &Path, args: &[&OsStr]) -> Command {
    let mut cmd = Command::new("git");
    cmd.arg("--literal-pathspecs")
        .arg("--no-optional-locks")
        .arg("-C")
        .arg(repo)
        .args(args)
        .env_remove("GIT_DIR")
        .env_remove("GIT_WORK_TREE")
        .env_remove("GIT_INDEX_FILE")
        .stdin(Stdio::null());
    cmd
}

/// Nearest ancestor-or-self of `p` that contains a `.git` dir or file.
pub fn repo_root(p: &Path) -> Option<PathBuf> {
    p.ancestors()
        .find(|a| {
            a.join(".git")
                .symlink_metadata()
                .is_ok_and(|m| m.is_dir() || m.is_file())
        })
        .map(Path::to_path_buf)
}

/// True if anything at or below `p` is in `repo`'s index. Any failure
/// (spawn error, non-zero exit, `p` outside `repo`) counts as tracked.
pub fn is_tracked(repo: &Path, p: &Path) -> bool {
    let Ok(rel) = p.strip_prefix(repo) else {
        return true;
    };
    let rel = if rel.as_os_str().is_empty() {
        Path::new(".")
    } else {
        rel
    };
    match git(repo, &["ls-files".as_ref(), "-z".as_ref(), "--".as_ref(), rel.as_os_str()]) {
        Some(out) => !out.stdout.is_empty(),
        None => true,
    }
}

/// Committer time (Unix seconds) of HEAD, or None if not a repo / no commits.
pub fn last_commit_time(repo: &Path) -> Option<i64> {
    let out = git(repo, &["log".as_ref(), "-1".as_ref(), "--format=%ct".as_ref()])?;
    String::from_utf8(out.stdout).ok()?.trim().parse().ok()
}

// ---- worktrees (spec §5.2b). Any failure reads as "not finished". ----

/// One record of `git worktree list --porcelain`.
#[derive(Debug, Clone, PartialEq)]
pub struct WorktreeInfo {
    pub path: PathBuf,
    pub head: Option<String>,
    /// Short name (`refs/heads/` stripped); None when detached or bare.
    pub branch: Option<String>,
    pub locked: bool,
    pub prunable: bool,
}

fn stdout(repo: &Path, args: &[&str]) -> Option<String> {
    let args: Vec<&OsStr> = args.iter().map(OsStr::new).collect();
    String::from_utf8(git(repo, &args)?.stdout).ok()
}

/// `git worktree list --porcelain -z`; `[0]` is the main worktree.
pub fn worktree_list(repo: &Path) -> Option<Vec<WorktreeInfo>> {
    let out = git(repo, &["worktree".as_ref(), "list".as_ref(), "--porcelain".as_ref(), "-z".as_ref()])?;
    let mut list = Vec::new();
    let mut cur: Option<WorktreeInfo> = None;
    // Attributes are NUL-terminated; an empty field ends a record.
    for field in out.stdout.split(|b| *b == 0) {
        if field.is_empty() {
            list.extend(cur.take());
            continue;
        }
        let (key, val) = match field.iter().position(|b| *b == b' ') {
            Some(i) => (&field[..i], &field[i + 1..]),
            None => (field, &b""[..]),
        };
        if key == b"worktree" {
            list.extend(cur.take());
            cur = Some(WorktreeInfo {
                path: PathBuf::from(OsStr::from_bytes(val)),
                head: None,
                branch: None,
                locked: false,
                prunable: false,
            });
            continue;
        }
        let Some(w) = cur.as_mut() else { continue };
        match key {
            b"HEAD" => w.head = std::str::from_utf8(val).ok().map(str::to_string),
            b"branch" => {
                let b = std::str::from_utf8(val).ok()?;
                w.branch = Some(b.strip_prefix("refs/heads/").unwrap_or(b).to_string());
            }
            b"locked" => w.locked = true,
            b"prunable" => w.prunable = true,
            _ => {}
        }
    }
    list.extend(cur);
    (!list.is_empty()).then_some(list)
}

/// origin/HEAD's target (e.g. `origin/main`), else local `main`, else `master`.
pub fn default_branch(repo: &Path) -> Option<String> {
    if let Some(s) = stdout(repo, &["symbolic-ref", "--quiet", "--short", "refs/remotes/origin/HEAD"]) {
        let s = s.trim();
        if !s.is_empty() {
            return Some(s.to_string());
        }
    }
    if stdout(repo, &["rev-parse", "--verify", "--quiet", "refs/heads/main"]).is_some() {
        return Some("main".into());
    }
    Some("master".into())
}

/// `merge-base --is-ancestor commit of`; any failure → false.
pub fn is_ancestor(repo: &Path, commit: &str, of: &str) -> bool {
    stdout(repo, &["merge-base", "--is-ancestor", commit, of]).is_some()
}

/// True if `refs/heads/<branch>`'s upstream is `[gone]`.
pub fn upstream_gone(repo: &Path, branch: &str) -> bool {
    let r = format!("refs/heads/{branch}");
    stdout(repo, &["for-each-ref", "--format=%(upstream:track)", &r]).is_some_and(|s| s.trim() == "[gone]")
}

/// `status --porcelain=v1 -z --untracked-files=all [--ignored=matching]` as
/// `(XY, path)` pairs; None if git fails.
pub fn status(wt: &Path, ignored: bool) -> Option<Vec<(String, PathBuf)>> {
    let mut args = vec!["status", "--porcelain=v1", "-z", "--untracked-files=all"];
    if ignored {
        args.push("--ignored=matching");
    }
    let args: Vec<&OsStr> = args.iter().map(OsStr::new).collect();
    let out = git(wt, &args)?;
    let mut res = Vec::new();
    let mut fields = out.stdout.split(|b| *b == 0).filter(|f| !f.is_empty());
    while let Some(f) = fields.next() {
        if f.len() < 4 {
            return None;
        }
        let code = String::from_utf8_lossy(&f[..2]).into_owned();
        if code.contains(['R', 'C']) {
            fields.next(); // the rename/copy source
        }
        res.push((code, PathBuf::from(OsStr::from_bytes(&f[3..]))));
    }
    Some(res)
}

/// Commits on local branches that no remote-tracking ref has
/// (`rev-list --count --branches --not --remotes`); None if git fails.
pub fn unpushed_commits(repo: &Path) -> Option<usize> {
    stdout(repo, &["rev-list", "--count", "--branches", "--not", "--remotes"])?.trim().parse().ok()
}

/// True if `git remote` names any remote; any failure counts as none.
pub fn has_remote(repo: &Path) -> bool {
    stdout(repo, &["remote"]).is_some_and(|s| !s.trim().is_empty())
}

/// `rev-parse HEAD`.
pub fn head(wt: &Path) -> Option<String> {
    let s = stdout(wt, &["rev-parse", "--verify", "HEAD"])?;
    Some(s.trim().to_string()).filter(|s| !s.is_empty())
}

/// `git -C main worktree remove -- wt`, never with `--force`, so git refuses
/// a dirty or locked worktree itself. `Err` is git's stderr, trimmed.
pub fn worktree_remove(main: &Path, wt: &Path) -> Result<(), String> {
    let args: [&OsStr; 4] = ["worktree".as_ref(), "remove".as_ref(), "--".as_ref(), wt.as_os_str()];
    let out = command(main, &args).output().map_err(|e| format!("can't run git: {e}"))?;
    if out.status.success() {
        Ok(())
    } else {
        Err(String::from_utf8_lossy(&out.stderr).trim().to_string())
    }
}

/// The gitdir a linked worktree's `.git` file names, whether or not it exists.
fn gitdir_named(wt: &Path) -> Option<PathBuf> {
    let text = std::fs::read(wt.join(".git")).ok()?;
    let rest = text.strip_prefix(b"gitdir:")?.trim_ascii();
    if rest.is_empty() {
        return None;
    }
    Some(wt.join(OsStr::from_bytes(rest))) // an absolute gitdir replaces wt
}

/// The linked worktree's gitdir from its `.git` file, if that dir exists.
pub fn linked_gitdir(wt: &Path) -> Option<PathBuf> {
    let dir = gitdir_named(wt)?;
    std::fs::metadata(&dir).is_ok_and(|m| m.is_dir()).then_some(dir)
}

/// The main repo a linked worktree's `.git` file points into
/// (`<repo>/.git/worktrees/<name>` → `<repo>`), if that repo's git dir is
/// missing entirely, e.g. deleted or on an unmounted filesystem.
pub fn missing_main_repo(wt: &Path) -> Option<PathBuf> {
    let dir = gitdir_named(wt)?;
    let wts = dir.parent().filter(|p| p.file_name() == Some(OsStr::new("worktrees")))?;
    let common = wts.parent()?;
    if common.symlink_metadata().is_ok() {
        return None;
    }
    let repo = match common.file_name() {
        Some(n) if n == ".git" => common.parent()?,
        _ => common, // a bare repo
    };
    Some(repo.to_path_buf())
}
