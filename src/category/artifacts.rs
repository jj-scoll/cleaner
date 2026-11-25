//! Project artifacts (spec §5.2a): kondo-equivalent discovery of build
//! output next to a project marker, aged by the project's own activity.

use anyhow::{Context, bail};
use glob::Pattern;
use std::collections::{HashMap, HashSet, VecDeque};
use std::ffi::OsStr;
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};

use crate::category::{Category, ScanCtx, ScanOutput, Why};
use crate::fmt::plural;
use crate::fsutil::measure::{InodeSet, measure, newest_mtime_excluding};
use crate::plan::{Entry, EntryKind, Skip, Target, fingerprint};
use crate::protect::Protector;

const ID: &str = "artifacts";

/// The one artifact found at any depth below a python project dir.
const PYCACHE: &str = "__pycache__";

pub struct EcoRule {
    pub name: String,
    pub markers: Vec<Pattern>,
    pub artifacts: Vec<Pattern>,
    /// False: matches are listed but never pre-selected (e.g. jupyter).
    pub preselect: bool,
}

fn eco(name: &str, markers: &[&str], artifacts: &[&str]) -> EcoRule {
    let pats = |v: &[&str]| {
        v.iter()
            .map(|s| Pattern::new(s).expect("builtin pattern"))
            .collect()
    };
    EcoRule {
        name: name.into(),
        markers: pats(markers),
        artifacts: pats(artifacts),
        preselect: true,
    }
}

/// The artifact names that existed before plan 4. The worktree "ignored
/// files" guard (§6 deletion path) and the nix prune use exactly these, so
/// newer ecosystems (`vendor`, `deps`, `.ipynb_checkpoints`, …) never widen
/// them. Deliberately a separate list: do not derive it from the table.
const LEGACY_ARTIFACT_NAMES: &[&str] = &[
    "target",
    "node_modules",
    ".next",
    ".turbo",
    ".parcel-cache",
    "dist",
    ".venv",
    "venv",
    PYCACHE,
    ".pytest_cache",
    ".mypy_cache",
    ".ruff_cache",
    ".tox",
    "*.egg-info",
    ".terraform",
    ".terragrunt-cache",
    "build",
    ".gradle",
    "cmake-build-*",
    "bin",
    "obj",
    "zig-cache",
    ".zig-cache",
    "zig-out",
];

/// Patterns for [`LEGACY_ARTIFACT_NAMES`].
pub fn legacy_artifact_patterns() -> Vec<Pattern> {
    LEGACY_ARTIFACT_NAMES
        .iter()
        .map(|s| Pattern::new(s).expect("builtin pattern"))
        .collect()
}

/// The ecosystem table from spec §5.2a (marker patterns → artifact patterns).
pub fn builtin_ecosystems() -> Vec<EcoRule> {
    vec![
        eco("rust", &["Cargo.toml"], &["target"]),
        eco(
            "node",
            &["package.json"],
            &["node_modules", ".next", ".turbo", ".parcel-cache", "dist"],
        ),
        eco(
            "python",
            &["pyproject.toml", "setup.py", "requirements*.txt"],
            &[
                ".venv",
                "venv",
                PYCACHE,
                ".pytest_cache",
                ".mypy_cache",
                ".ruff_cache",
                ".tox",
                "*.egg-info",
            ],
        ),
        eco(
            "terraform",
            &["*.tf", "terragrunt.hcl"],
            &[".terraform", ".terragrunt-cache"],
        ),
        eco("gradle", &["build.gradle*"], &["build", ".gradle"]),
        eco("maven", &["pom.xml"], &["target"]),
        eco("cmake", &["CMakeLists.txt"], &["build", "cmake-build-*"]),
        eco("dotnet", &["*.csproj"], &["bin", "obj"]),
        eco(
            "zig",
            &["build.zig"],
            &["zig-cache", ".zig-cache", "zig-out"],
        ),
        eco("composer", &["composer.json"], &["vendor"]),
        eco("elixir", &["mix.exs"], &["_build", "deps"]),
        eco("haskell", &["*.cabal", "cabal.project"], &["dist-newstyle"]),
        eco("stack", &["stack.yaml"], &[".stack-work"]),
        eco("godot", &["project.godot"], &[".godot", ".import"]),
        EcoRule {
            preselect: false,
            ..eco("jupyter", &["*.ipynb"], &[".ipynb_checkpoints"])
        },
    ]
}

/// `"artifact@marker"`, e.g. `"out@build.zig"`, as an ecosystem named "custom".
pub fn parse_custom_rule(s: &str) -> anyhow::Result<EcoRule> {
    let Some((artifact, marker)) = s.split_once('@') else {
        bail!("extra rule {s:?} is not of the form artifact@marker");
    };
    if artifact.is_empty() || marker.is_empty() || artifact.contains('/') || marker.contains('/') {
        bail!("extra rule {s:?} needs a non-empty artifact and marker name without '/'");
    }
    let pat = |p: &str| Pattern::new(p).with_context(|| format!("extra rule {s:?}"));
    Ok(EcoRule {
        name: "custom".into(),
        markers: vec![pat(marker)?],
        artifacts: vec![pat(artifact)?],
        preselect: true,
    })
}

/// First 43 bytes of a valid `CACHEDIR.TAG` (bford.info/cachedir).
pub const CACHEDIR_SIGNATURE: &[u8; 43] = b"Signature: 8a477f597d28d172789f06886806bc55";

/// `dir/CACHEDIR.TAG` is a regular file (lstat, opened `O_NOFOLLOW`) whose
/// first 43 bytes equal [`CACHEDIR_SIGNATURE`].
pub fn has_cachedir_tag(dir: &Path) -> bool {
    use std::io::Read;
    use std::os::unix::fs::OpenOptionsExt;
    let tag = dir.join("CACHEDIR.TAG");
    if !tag
        .symlink_metadata()
        .is_ok_and(|m| m.file_type().is_file())
    {
        return false;
    }
    let Ok(mut f) = std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW)
        .open(&tag)
    else {
        return false;
    };
    let mut head = [0u8; CACHEDIR_SIGNATURE.len()];
    f.read_exact(&mut head).is_ok() && head == *CACHEDIR_SIGNATURE
}

pub struct Project {
    pub dir: PathBuf,
    pub artifacts: Vec<PathBuf>,
    /// Subset of `artifacts` from `preselect = false` ecosystems.
    pub unticked: Vec<PathBuf>,
}

/// What [`discover_all`] found.
pub struct Discovery {
    pub projects: Vec<Project>,
    pub skips: Vec<Skip>,
    /// Dirs holding a valid `CACHEDIR.TAG`: claimed, not denied, not descended.
    pub tagged: Vec<PathBuf>,
}

fn matches_any(pats: &[Pattern], name: &OsStr) -> bool {
    name.to_str()
        .is_some_and(|n| pats.iter().any(|p| p.matches(n)))
}

struct Child {
    name: std::ffi::OsString,
    path: PathBuf,
    md: std::fs::Metadata,
}

/// lstat'd children of `dir`; unreadable dirs and entries are skipped.
fn children(dir: &Path) -> Vec<Child> {
    let Ok(rd) = std::fs::read_dir(dir) else {
        return Vec::new();
    };
    rd.flatten()
        .filter_map(|e| {
            Some(Child {
                name: e.file_name(),
                path: e.path(),
                md: e.metadata().ok()?,
            })
        })
        .collect()
}

fn skip(path: &Path, home: &Path, reason: &str) -> Skip {
    Skip {
        category: ID.into(),
        title: crate::paths::tilde(path, home),
        reason: reason.into(),
    }
}

/// True if `dir` is at or inside a protected path, so the walk must not
/// enter it. `is_protected` alone is too broad for that: it is also true for
/// every ancestor of a protected root (home itself contains `~/.claude`).
/// A NUL component can't be part of any protected root, so the probe child
/// is protected only when `dir` is inside one or has a protected component.
fn inside_protected(prot: &Protector, dir: &Path) -> bool {
    prot.is_protected(&dir.join("\0"))
}

/// Finds artifact dirs next to ecosystem markers under `roots` (spec §5.2a).
///
/// Breadth-first to `max_depth`, staying on each root's filesystem. Never
/// descends into symlinks, protected paths, `.git`, artifact candidates, or
/// dotdirs directly under a root. Symlinked, protected and git-tracked
/// candidates become `Skip`s, titled under `~` (`home`). `__pycache__` is found
/// at any depth below a python project; every other artifact must sit next to its marker.
pub fn discover(
    roots: &[PathBuf],
    max_depth: usize,
    ecos: &[EcoRule],
    prot: &Protector,
    home: &Path,
) -> (Vec<Project>, Vec<Skip>) {
    let d = discover_all(roots, max_depth, ecos, prot, home, &|_| true);
    (d.projects, d.skips)
}

/// [`discover`] that also reports `CACHEDIR.TAG` dirs below a root (depth >= 1).
/// A tagged dir is neither a project nor descended; `deny` drops it silently,
/// otherwise it goes through the same `claim` as an artifact.
pub fn discover_all(
    roots: &[PathBuf],
    max_depth: usize,
    ecos: &[EcoRule],
    prot: &Protector,
    home: &Path,
    deny: &dyn Fn(&Path) -> bool,
) -> Discovery {
    let mut tagged: Vec<PathBuf> = Vec::new();
    let mut projects: Vec<Project> = Vec::new();
    let mut skips = Vec::new();
    let mut claimed: HashSet<PathBuf> = HashSet::new();
    let all_artifacts: Vec<Pattern> = ecos
        .iter()
        .flat_map(|e| e.artifacts.iter().cloned())
        .collect();

    for root in roots {
        let Ok(root_md) = root.symlink_metadata() else {
            continue;
        };
        if !root_md.is_dir() || inside_protected(prot, root) {
            continue;
        }
        let dev = root_md.dev();
        let mut queue = VecDeque::from([(root.clone(), 0usize)]);
        while let Some((dir, depth)) = queue.pop_front() {
            if depth >= 1 && has_cachedir_tag(&dir) {
                if !deny(&dir)
                    && let Ok(md) = dir.symlink_metadata()
                {
                    claim(&dir, &md, prot, home, &mut claimed, &mut tagged, &mut skips);
                }
                continue;
            }
            let kids = children(&dir);
            let mut candidates: Vec<&Child> = Vec::new();
            let mut unticked_paths: HashSet<&Path> = HashSet::new();
            let mut python = false;
            for e in ecos {
                if !kids.iter().any(|c| matches_any(&e.markers, &c.name)) {
                    continue;
                }
                python |= e.name == "python";
                for c in &kids {
                    let ft = c.md.file_type();
                    if (ft.is_dir() || ft.is_symlink()) && matches_any(&e.artifacts, &c.name) {
                        if !e.preselect {
                            unticked_paths.insert(c.path.as_path());
                        }
                        if !candidates.iter().any(|k| k.path == c.path) {
                            candidates.push(c);
                        }
                    }
                }
            }

            let mut found = Vec::new();
            for c in &candidates {
                if c.md.dev() != dev {
                    skips.push(skip(&c.path, home, "mount point, not crossed"));
                    continue;
                }
                claim(
                    &c.path,
                    &c.md,
                    prot,
                    home,
                    &mut claimed,
                    &mut found,
                    &mut skips,
                );
            }
            if python {
                let not_here: HashSet<&Path> =
                    candidates.iter().map(|c| c.path.as_path()).collect();
                find_pycache(
                    &dir,
                    dev,
                    &all_artifacts,
                    &not_here,
                    prot,
                    home,
                    &mut claimed,
                    &mut found,
                    &mut skips,
                );
            }
            if !found.is_empty() {
                let unticked = found
                    .iter()
                    .filter(|f| unticked_paths.contains(f.as_path()))
                    .cloned()
                    .collect();
                projects.push(Project {
                    dir: dir.clone(),
                    artifacts: found,
                    unticked,
                });
            }

            if depth >= max_depth {
                continue;
            }
            for c in &kids {
                let descend = c.md.is_dir() // lstat: false for symlinks
                    && c.md.dev() == dev
                    && enters(&c.name, depth)
                    && !candidates.iter().any(|k| k.path == c.path)
                    && !inside_protected(prot, &c.path);
                if descend {
                    queue.push_back((c.path.clone(), depth + 1));
                }
            }
        }
    }
    Discovery {
        projects,
        skips,
        tagged,
    }
}

/// The walk's name rule for entering a child of a dir at `depth`: never
/// `.git`, and no dotdir directly under a root.
fn enters(name: &OsStr, depth: usize) -> bool {
    name != ".git" && !(depth == 0 && name.as_encoded_bytes().starts_with(b"."))
}

/// Records `path` as an artifact unless it's a symlink, protected, tracked
/// in git, or already claimed.
fn claim(
    path: &Path,
    md: &std::fs::Metadata,
    prot: &Protector,
    home: &Path,
    claimed: &mut HashSet<PathBuf>,
    found: &mut Vec<PathBuf>,
    skips: &mut Vec<Skip>,
) {
    if claimed.contains(path) {
        return;
    }
    if md.file_type().is_symlink() {
        skips.push(skip(path, home, "symlink, not followed"));
    } else if prot.is_protected(path) {
        skips.push(skip(path, home, "protected"));
    } else if crate::git::repo_root(path).is_some_and(|repo| crate::git::is_tracked(&repo, path)) {
        skips.push(skip(path, home, "tracked in git"));
    } else {
        claimed.insert(path.to_path_buf());
        found.push(path.to_path_buf());
    }
}

/// Claims every `__pycache__` dir below `dir`, at any depth. Does not descend
/// into symlinks, other filesystems, `.git`, protected paths, `not_here`
/// (this project's own artifacts), or any other artifact-named dir.
#[allow(clippy::too_many_arguments)]
fn find_pycache(
    dir: &Path,
    dev: u64,
    artifact_pats: &[Pattern],
    not_here: &HashSet<&Path>,
    prot: &Protector,
    home: &Path,
    claimed: &mut HashSet<PathBuf>,
    found: &mut Vec<PathBuf>,
    skips: &mut Vec<Skip>,
) {
    let mut stack = vec![dir.to_path_buf()];
    while let Some(d) = stack.pop() {
        for c in children(&d) {
            if !c.md.is_dir()
                || c.md.dev() != dev
                || c.name == ".git"
                || not_here.contains(c.path.as_path())
            {
                continue;
            }
            if c.name == PYCACHE {
                claim(&c.path, &c.md, prot, home, claimed, found, skips);
            } else if !matches_any(artifact_pats, &c.name) && !inside_protected(prot, &c.path) {
                stack.push(c.path);
            }
        }
    }
}

pub struct Artifacts;

impl Category for Artifacts {
    fn id(&self) -> &'static str {
        ID
    }

    fn explain(&self, path: &Path, ctx: &ScanCtx) -> Option<Why> {
        self.explain_dir(path, ctx)
    }

    fn scan(&self, ctx: &ScanCtx, seen: &mut InodeSet) -> anyhow::Result<ScanOutput> {
        let cfg = &ctx.config.artifacts;
        let mut out = ScanOutput::default();
        if !cfg.enabled {
            return Ok(out);
        }
        let mut ecos = builtin_ecosystems();
        for r in &cfg.extra_rules {
            match parse_custom_rule(r) {
                Ok(e) => ecos.push(e),
                Err(e) => out.skipped.push(Skip {
                    category: ID.into(),
                    title: r.clone(),
                    reason: format!("invalid extra rule: {e:#}"),
                }),
            }
        }
        let roots = crate::paths::expand_roots(ctx.config.roots_for(&cfg.roots), ctx.home);
        let deny = tag_deny(ctx);
        let Discovery {
            projects,
            skips,
            mut tagged,
        } = discover_all(&roots, cfg.max_depth, &ecos, ctx.protector, ctx.home, &deny);
        out.skipped.extend(skips);

        // Direct children of ~/.cache: the walk never enters dotdirs under a root.
        let listed: HashSet<&Path> = tagged
            .iter()
            .map(PathBuf::as_path)
            .chain(
                projects
                    .iter()
                    .flat_map(|p| &p.artifacts)
                    .map(PathBuf::as_path),
            )
            .collect();
        let mut cache_tagged = Vec::new();
        for c in children(&ctx.home.join(".cache")) {
            if c.md.is_dir()
                && !listed.contains(c.path.as_path())
                && !listed.iter().any(|l| l.starts_with(&c.path))
                && has_cachedir_tag(&c.path)
                && !deny(&c.path)
                && !ctx.protector.is_protected(&c.path)
                && !crate::git::repo_root(&c.path)
                    .is_some_and(|repo| crate::git::is_tracked(&repo, &c.path))
            {
                cache_tagged.push(c.path);
            }
        }
        tagged.extend(cache_tagged);
        tagged.sort();

        // `is_finished` per repo root, not per project (M10 scan cost).
        let mut finished: HashMap<PathBuf, bool> = HashMap::new();
        for p in projects {
            let mut ticked = (Vec::new(), 0u64);
            let mut unticked = (Vec::new(), 0u64);
            let mut unticked_ecos: Vec<&str> = Vec::new();
            for a in &p.artifacts {
                match measure(a, seen).and_then(|m| Ok((m, fingerprint(a)?))) {
                    Ok((m, fp)) => {
                        let side = if p.unticked.contains(a) {
                            let name = a.file_name().unwrap_or_default();
                            for e in ecos
                                .iter()
                                .filter(|e| !e.preselect && matches_any(&e.artifacts, name))
                            {
                                if !unticked_ecos.contains(&e.name.as_str()) {
                                    unticked_ecos.push(&e.name);
                                }
                            }
                            &mut unticked
                        } else {
                            &mut ticked
                        };
                        side.1 += m.bytes;
                        side.0.push(Target {
                            path: a.clone(),
                            fp,
                        });
                    }
                    Err(e) => out
                        .skipped
                        .push(skip(a, ctx.home, &format!("cannot measure: {e}"))),
                }
            }
            if ticked.0.is_empty() && unticked.0.is_empty() {
                continue;
            }

            let excluded: HashSet<&Path> = p.artifacts.iter().map(PathBuf::as_path).collect();
            let (last_active, root) = project_last_active(&p.dir, &excluded);
            let idle_secs = ctx.now - last_active;
            let days = idle_secs.max(0) / 86_400;
            let selected = idle_secs > cfg.stale_after_days as i64 * 86_400;
            let age_reason = if selected {
                format!("untouched for {}", plural(days, "day"))
            } else {
                format!("active {} ago", plural(days, "day"))
            };
            let mut reason = age_reason.clone();
            // A project inside a linked worktree (`.git` is a file) that is
            // finished is pre-selected whatever its age (§5.2a).
            let finished_worktree = root.is_some_and(|r| {
                *finished.entry(r).or_insert_with_key(|r| {
                    r.join(".git").is_file() && crate::category::worktrees::is_finished(r)
                })
            });
            if finished_worktree {
                reason.push_str("; worktree is finished");
            }
            let selected = selected || finished_worktree;

            let title = match p.dir.strip_prefix(ctx.home) {
                Ok(rel) if rel.as_os_str().is_empty() => "~".to_string(),
                Ok(rel) => rel.display().to_string(),
                Err(_) => p.dir.display().to_string(),
            };
            let mut push = |targets: Vec<Target>, bytes: u64, reason: String, selected: bool| {
                out.entries.push(Entry {
                    id: String::new(),
                    category: ID.into(),
                    title: title.clone(),
                    kind: EntryKind::Item { targets },
                    bytes,
                    reason,
                    selected,
                    permanent: false,
                    note: None,
                    last_active: Some(last_active),
                });
            };
            if !ticked.0.is_empty() {
                push(ticked.0, ticked.1, reason, selected);
            }
            if !unticked.0.is_empty() {
                let reason = format!(
                    "{} artifacts are never pre-selected; {age_reason}",
                    unticked_ecos.join(", ")
                );
                push(unticked.0, unticked.1, reason, false);
            }
        }
        for dir in tagged {
            match measure(&dir, seen).and_then(|m| Ok((m, fingerprint(&dir)?))) {
                Ok((m, fp)) => {
                    let days = (ctx.now - newest_mtime_excluding(&dir, &|_| false)).max(0) / 86_400;
                    out.entries.push(Entry {
                        id: String::new(),
                        category: ID.into(),
                        title: crate::paths::tilde(&dir, ctx.home),
                        kind: EntryKind::Item {
                            targets: vec![Target { path: dir, fp }],
                        },
                        bytes: m.bytes,
                        reason: format!("CACHEDIR.TAG cache · untouched {}", plural(days, "day")),
                        selected: false,
                        permanent: false,
                        note: None,
                        last_active: None,
                    });
                }
                Err(e) => out
                    .skipped
                    .push(skip(&dir, ctx.home, &format!("cannot measure: {e}"))),
            }
        }
        Ok(out)
    }
}

/// Last activity of a project dir as the scan measures it: the newest file
/// outside `excluded` (its artifacts) and `.git`, or HEAD's commit time if
/// later. Also returns the enclosing repo root.
fn project_last_active(dir: &Path, excluded: &HashSet<&Path>) -> (i64, Option<PathBuf>) {
    let skip_fn = |q: &Path| excluded.contains(q) || q.file_name() == Some(OsStr::new(".git"));
    let file_age = newest_mtime_excluding(dir, &skip_fn);
    let root = crate::git::repo_root(dir);
    let commit_age = root
        .as_deref()
        .and_then(crate::git::last_commit_time)
        .unwrap_or(0);
    (file_age.max(commit_age), root)
}

/// The built-in ecosystems plus the valid `extra_rules`.
fn configured_ecosystems(extra_rules: &[String]) -> Vec<EcoRule> {
    let mut ecos = builtin_ecosystems();
    ecos.extend(extra_rules.iter().filter_map(|r| parse_custom_rule(r).ok()));
    ecos
}

/// The first child of `dir` matching one of `eco`'s markers.
fn marker_in(eco: &EcoRule, dir: &Path) -> Option<std::ffi::OsString> {
    children(dir)
        .into_iter()
        .find(|c| matches_any(&eco.markers, &c.name))
        .map(|c| c.name)
}

/// The first ecosystem with an artifact pattern matching `name` and a marker
/// in `dir`, with that marker's name: what makes `dir/name` a candidate.
fn eco_with_marker<'a>(
    ecos: &'a [EcoRule],
    dir: &Path,
    name: &OsStr,
) -> Option<(&'a EcoRule, std::ffi::OsString)> {
    ecos.iter()
        .filter(|e| matches_any(&e.artifacts, name))
        .find_map(|e| Some((e, marker_in(e, dir)?)))
}

/// Children of `project` the walk would take as artifact candidates.
fn candidates_in(project: &Path, ecos: &[EcoRule]) -> Vec<PathBuf> {
    children(project)
        .into_iter()
        .filter(|c| {
            let ft = c.md.file_type();
            (ft.is_dir() || ft.is_symlink()) && eco_with_marker(ecos, project, &c.name).is_some()
        })
        .map(|c| c.path)
        .collect()
}

/// The python project `pycache` belongs to: the nearest ancestor with a
/// python marker that `find_pycache` would reach it from (no `.git` or
/// other artifact-named dir in between).
fn pycache_project(pycache: &Path, root: &Path, ecos: &[EcoRule]) -> Option<PathBuf> {
    let python = ecos.iter().find(|e| e.name == "python")?;
    let all: Vec<Pattern> = ecos.iter().flat_map(|e| e.artifacts.clone()).collect();
    let mut dir = pycache.parent()?;
    loop {
        if marker_in(python, dir).is_some() {
            return Some(dir.to_path_buf());
        }
        let name = dir.file_name()?;
        if dir == root || name == ".git" || matches_any(&all, name) {
            return None;
        }
        dir = dir.parent()?;
    }
}

/// Why the walk from `root` never lists the children of `dir`, or `None`.
fn unreached(
    root: &Path,
    dir: &Path,
    max_depth: usize,
    ecos: &[EcoRule],
    home: &Path,
) -> Option<String> {
    let rel = dir.strip_prefix(root).ok()?;
    let names: Vec<&OsStr> = rel.iter().collect();
    if names.len() > max_depth {
        return Some(format!(
            "deeper than artifacts.max_depth ({max_depth}) below {}",
            crate::paths::tilde(root, home)
        ));
    }
    let mut cur = root.to_path_buf();
    for (depth, name) in names.into_iter().enumerate() {
        let next = cur.join(name);
        let shown = crate::paths::tilde(&next, home);
        if !enters(name, depth) {
            return Some(if name == ".git" {
                "inside .git".into()
            } else {
                format!("under {shown}; the walk never enters dot-dirs at the top of a scan root")
            });
        }
        if eco_with_marker(ecos, &cur, name).is_some() {
            return Some(format!("inside another artifact ({shown})"));
        }
        if has_cachedir_tag(&next) {
            return Some(format!("inside a CACHEDIR.TAG cache ({shown})"));
        }
        cur = next;
    }
    None
}

/// Whether a scan pre-selects the artifacts of `project` (`Ok(age words)`),
/// or why not (`Err`), by the same age and finished-worktree rules.
fn preselection(
    project: &Path,
    eco: &EcoRule,
    ecos: &[EcoRule],
    ctx: &ScanCtx,
) -> Result<String, String> {
    if !eco.preselect {
        return Err(format!("{} artifacts are never pre-selected", eco.name));
    }
    let cands = candidates_in(project, ecos);
    let excluded: HashSet<&Path> = cands.iter().map(PathBuf::as_path).collect();
    let (last_active, root) = project_last_active(project, &excluded);
    let idle = ctx.now - last_active;
    let days = plural(idle.max(0) / 86_400, "day");
    let stale = ctx.config.artifacts.stale_after_days;
    if idle > stale as i64 * 86_400 {
        Ok(format!("untouched for {days}"))
    } else if root
        .is_some_and(|r| r.join(".git").is_file() && crate::category::worktrees::is_finished(&r))
    {
        Ok(format!("active {days} ago, but its worktree is finished"))
    } else {
        Err(format!("untouched for {days}; rule needs {stale}"))
    }
}

/// The ecosystem rule behind an artifact at `path` and its project dir.
fn rule_of(path: &Path, ctx: &ScanCtx, ecos: &[EcoRule]) -> Option<(String, PathBuf, usize)> {
    let name = path.file_name()?;
    let parent = path.parent()?;
    if let Some((e, marker)) = eco_with_marker(ecos, parent, name) {
        let i = ecos.iter().position(|x| std::ptr::eq(x, e))?;
        let text = format!(
            "{}: {} next to {}",
            e.name,
            name.to_string_lossy(),
            marker.to_string_lossy()
        );
        return Some((text, parent.to_path_buf(), i));
    }
    if name == PYCACHE {
        let roots =
            crate::paths::expand_roots(ctx.config.roots_for(&ctx.config.artifacts.roots), ctx.home);
        let root = roots.iter().find(|r| path.starts_with(r))?;
        let project = pycache_project(path, root, ecos)?;
        let i = ecos.iter().position(|e| e.name == "python")?;
        return Some((
            "python: __pycache__ below a python project".into(),
            project,
            i,
        ));
    }
    None
}

/// The rule that planned the artifacts target `path`, in words.
pub fn rule_for(path: &Path, ctx: &ScanCtx) -> Option<String> {
    let ecos = configured_ecosystems(&ctx.config.artifacts.extra_rules);
    match rule_of(path, ctx, &ecos) {
        Some((text, _, _)) => Some(text),
        None => has_cachedir_tag(path).then(|| "CACHEDIR.TAG cache directory".to_string()),
    }
}

/// Why a scan doesn't pre-select the artifacts target `path`, or `None`
/// when it would (or the rule can't be found now).
pub fn not_preselected(path: &Path, ctx: &ScanCtx) -> Option<String> {
    let ecos = configured_ecosystems(&ctx.config.artifacts.extra_rules);
    match rule_of(path, ctx, &ecos) {
        Some((_, project, i)) => preselection(&project, &ecos[i], &ecos, ctx).err(),
        None => has_cachedir_tag(path).then(|| "CACHEDIR.TAG caches are never pre-selected".into()),
    }
}

impl Artifacts {
    fn explain_dir(&self, path: &Path, ctx: &ScanCtx) -> Option<Why> {
        let cfg = &ctx.config.artifacts;
        let md = path.symlink_metadata().ok()?;
        if !md.is_dir() {
            return None;
        }
        let name = path.file_name()?;
        let parent = path.parent()?;
        let ecos = configured_ecosystems(&cfg.extra_rules);
        let named: Vec<&EcoRule> = ecos
            .iter()
            .filter(|e| matches_any(&e.artifacts, name))
            .collect();
        let tagged = has_cachedir_tag(path);
        if named.is_empty() && !tagged {
            return None;
        }
        let roots = crate::paths::expand_roots(ctx.config.roots_for(&cfg.roots), ctx.home);
        let root = roots
            .iter()
            .find(|r| path.starts_with(r) && path != r.as_path());

        if !named.is_empty() {
            let Some(root) = root else {
                let shown: Vec<String> = roots
                    .iter()
                    .map(|r| crate::paths::tilde(r, ctx.home))
                    .collect();
                return Some(Why::NearMiss(format!(
                    "outside the artifacts scan roots ({})",
                    shown.join(", ")
                )));
            };
            let found = if name == PYCACHE {
                pycache_project(path, root, &ecos).map(|p| {
                    let i = ecos.iter().position(|e| e.name == "python").unwrap_or(0);
                    (
                        "python: __pycache__ below a python project".to_string(),
                        p,
                        i,
                    )
                })
            } else {
                rule_of(path, ctx, &ecos)
            };
            if let Some((rule, project, i)) = found {
                if let Some(why) = unreached(root, &project, cfg.max_depth, &ecos, ctx.home) {
                    return Some(Why::NearMiss(why));
                }
                return Some(Why::Match(
                    match preselection(&project, &ecos[i], &ecos, ctx) {
                        Ok(age) => format!("{rule}; {age}"),
                        Err(why) => format!("{rule}; listed but not pre-selected: {why}"),
                    },
                ));
            }
            if !tagged {
                let mut markers: Vec<&str> = Vec::new();
                for m in named.iter().flat_map(|e| &e.markers) {
                    if !markers.contains(&m.as_str()) {
                        markers.push(m.as_str());
                    }
                }
                let place = if name == PYCACHE {
                    " in it or any parent"
                } else {
                    ""
                };
                return Some(Why::NearMiss(format!(
                    "artifact without its project marker ({} not found{place})",
                    markers.join(" or ")
                )));
            }
        }

        // A CACHEDIR.TAG dir: found by the walk, or as a direct child of ~/.cache.
        let in_cache = parent == ctx.home.join(".cache");
        let reached = in_cache
            || root.is_some_and(|r| {
                let depth = parent.strip_prefix(r).map_or(0, |rel| rel.iter().count());
                enters(name, depth)
                    && unreached(r, parent, cfg.max_depth, &ecos, ctx.home).is_none()
            });
        if !reached {
            return Some(Why::NearMiss(
                "CACHEDIR.TAG cache the artifacts walk doesn't reach".into(),
            ));
        }
        if tag_deny(ctx)(path) {
            return Some(Why::NearMiss(
                "CACHEDIR.TAG cache owned by another rule (a venv, the uv cache, a configured store or app cache)"
                    .into(),
            ));
        }
        Some(Why::Match(
            "CACHEDIR.TAG cache directory; listed, never pre-selected".into(),
        ))
    }
}

/// True for tagged dirs that must never be offered: venvs (`pyvenv.cfg`), the
/// uv cache/store (hardlinked into live venvs), any configured store, keep-newest
/// or app-cache dir, at, under or containing one (those have their own rows), and anything protected.
fn tag_deny<'a>(ctx: &'a ScanCtx) -> impl Fn(&Path) -> bool + 'a {
    let home = ctx.home;
    let cfg = ctx.config;
    let mut owned: Vec<PathBuf> = vec![home.join(".cache/uv"), home.join(".local/share/uv")];
    let patterns = cfg
        .caches
        .stores
        .iter()
        .map(|s| &s.path)
        .chain(cfg.caches.keep_newest.iter().map(|k| &k.path))
        .chain(cfg.app_caches.apps.iter().flat_map(|a| &a.dirs));
    for pat in patterns {
        owned.extend(crate::paths::expand(pat, home));
    }
    move |p| {
        p.join("pyvenv.cfg").symlink_metadata().is_ok()
            || owned.iter().any(|o| p.starts_with(o) || o.starts_with(p))
            || inside_protected(ctx.protector, p)
    }
}
