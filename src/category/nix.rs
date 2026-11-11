//! Nix profile generations, `result` links and devbox roots (spec §5.2f).
//! Generations are read from the profile dir's `NAME-N-link` symlinks and
//! deleted by exact number. System generations go through pkexec; the store
//! GC is an Operation.

use std::collections::HashMap;
use std::ffi::{OsStr, OsString};
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};

use crate::category::{Category, Outcome, ScanCtx, ScanOutput, apply_default};
use crate::config::NixCfg;
use crate::fsutil::measure::{InodeSet, measure};
use crate::object::{Object, Operation};
use crate::plan::{Entry, EntryKind, Skip, Target, fingerprint};
use crate::tool::{GC_TIMEOUT, OP_TIMEOUT, PKEXEC_TIMEOUT, QUERY_TIMEOUT, Runner, ToolError, parse_human_size};

const ID: &str = "nix";
const STORE: &str = "/nix/store/";
/// Counted as kept when sizing generations (DECISION D5).
const CURRENT_SYSTEM: &str = "/run/current-system";
/// What `devbox shell` recreates. Never `.devbox/virtenv`, `devbox.json` or `devbox.lock`.
const DEVBOX_TARGETS: &[&str] = &[
    ".devbox/nix/profile",
    ".devbox/local.lock",
    ".devbox/state.json",
    ".devbox/.nix-print-dev-env-cache",
];

pub struct Nix;

/// `NAME-N-link` → `(NAME, N)`.
fn parse_gen_link(file: &OsStr) -> Option<(&str, u32)> {
    let (name, n) = file.to_str()?.strip_suffix("-link")?.rsplit_once('-')?;
    if name.is_empty() || n.is_empty() || !n.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    Some((name, n.parse().ok()?))
}

/// The `NAME-N-link` siblings of `profile` with their store paths, newest first.
pub fn generations(profile: &Path) -> Vec<(u32, PathBuf, String)> {
    let (Some(dir), Some(name)) = (profile.parent(), profile.file_name()) else { return Vec::new() };
    let Ok(rd) = std::fs::read_dir(dir) else { return Vec::new() };
    let mut gens: Vec<_> = rd
        .flatten()
        .filter_map(|e| {
            let file = e.file_name();
            let (link_name, n) = parse_gen_link(&file)?;
            if OsStr::new(link_name) != name {
                return None;
            }
            let store = std::fs::read_link(e.path()).ok()?.into_os_string().into_string().ok()?;
            Some((n, e.path(), store))
        })
        .collect();
    gens.sort_by_key(|g| std::cmp::Reverse(g.0));
    gens
}

/// N when `readlink(profile)` is the sibling `NAME-N-link`.
pub fn current_generation(profile: &Path) -> Option<u32> {
    let target = std::fs::read_link(profile).ok()?;
    if target.parent().is_some_and(|p| !p.as_os_str().is_empty()) {
        return None;
    }
    let (name, n) = parse_gen_link(target.as_os_str())?;
    (profile.file_name() == Some(OsStr::new(name))).then_some(n)
}

/// `nix path-info --json <flag> <paths…>` as (path, info) pairs. Accepts both
/// shapes plain `--json` has printed: an object keyed by path (Nix 2.19+) and
/// an array of objects with `path` (older). Invalid paths (null) are left out.
fn path_info(tools: &dyn Runner, flag: Option<&str>, paths: &[String]) -> Option<Vec<(String, serde_json::Value)>> {
    let mut args: Vec<&OsStr> = ["--extra-experimental-features", "nix-command", "path-info", "--json"]
        .into_iter()
        .chain(flag)
        .map(OsStr::new)
        .collect();
    args.extend(paths.iter().map(OsStr::new));
    let out = tools.run("nix", &args, QUERY_TIMEOUT).ok().filter(|o| o.ok())?;
    match serde_json::from_str(&out.stdout).ok()? {
        serde_json::Value::Object(m) => Some(m.into_iter().filter(|(_, v)| !v.is_null()).collect()),
        serde_json::Value::Array(a) => Some(
            a.into_iter()
                .filter_map(|v| Some((v.get("path")?.as_str()?.to_string(), v)))
                .collect(),
        ),
        _ => None,
    }
}

/// narSize of every store path in the closure of `paths`.
fn closure(tools: &dyn Runner, paths: &[String]) -> Option<HashMap<String, u64>> {
    if paths.is_empty() {
        return Some(HashMap::new());
    }
    let infos = path_info(tools, Some("-r"), paths)?;
    Some(infos.into_iter().filter_map(|(p, v)| Some((p, v.get("narSize")?.as_u64()?))).collect())
}

/// narSize sum of closure(candidates) − `kept` (a closure).
fn estimate_against(tools: &dyn Runner, candidates: &[String], kept: &HashMap<String, u64>) -> Option<u64> {
    let c = closure(tools, candidates)?;
    Some(c.iter().filter(|(p, _)| !kept.contains_key(*p)).map(|(_, n)| n).sum())
}

/// narSize sum of closure(candidates) − closure(kept). Both closures come from
/// `nix path-info --json -r <paths…>` (with --extra-experimental-features nix-command).
pub fn closure_estimate(tools: &dyn Runner, candidates: &[String], kept: &[String]) -> Option<u64> {
    let kept = closure(tools, kept)?;
    estimate_against(tools, candidates, &kept)
}

/// `[1, 2, 3, 5]` → `"1–3, 5"`.
fn gen_list(ns: &[u32]) -> String {
    let mut parts: Vec<String> = Vec::new();
    let mut i = 0;
    while i < ns.len() {
        let mut j = i;
        while j + 1 < ns.len() && ns[j + 1] == ns[j] + 1 {
            j += 1;
        }
        parts.push(if i == j { ns[i].to_string() } else { format!("{}–{}", ns[i], ns[j]) });
        i = j + 1;
    }
    parts.join(", ")
}

/// One profile's generations under the keep-newest-N rule.
struct ProfilePlan {
    path: PathBuf,
    name: String,
    total: usize,
    keep: usize,
    /// Oldest first; never the current generation.
    candidates: Vec<(u32, String)>,
    /// Store paths of every generation that stays.
    kept: Vec<String>,
}

/// `keep_profile[NAME]` or `keep`, never below 1.
fn keep_for(cfg: &NixCfg, name: &str) -> usize {
    cfg.keep_profile.get(name).copied().unwrap_or(cfg.keep).max(1)
}

/// The keep rule for `profile`: generations past the newest `keep`, minus the
/// current one. `None` unless `profile` links to one of its `NAME-N-link`s.
fn plan_profile(profile: &Path, cfg: &NixCfg) -> Option<ProfilePlan> {
    let current = current_generation(profile)?;
    let name = profile.file_name()?.to_str()?.to_string();
    let keep = keep_for(cfg, &name);
    let gens = generations(profile);
    let (mut candidates, mut kept) = (Vec::new(), Vec::new());
    for (i, (n, _, store)) in gens.iter().enumerate() {
        if i < keep || *n == current {
            kept.push(store.clone());
        } else {
            candidates.push((*n, store.clone()));
        }
    }
    candidates.reverse();
    Some(ProfilePlan { path: profile.to_path_buf(), name, total: gens.len(), keep, candidates, kept })
}

/// `path` relative to home for titles (as artifacts does).
fn rel(home: &Path, path: &Path) -> String {
    match path.strip_prefix(home) {
        Ok(r) if r.as_os_str().is_empty() => "~".into(),
        Ok(r) => r.display().to_string(),
        Err(_) => path.display().to_string(),
    }
}

fn entry(title: String, kind: EntryKind, bytes: u64, reason: String, selected: bool) -> Entry {
    Entry {
        id: String::new(),
        category: ID.into(),
        title,
        kind,
        bytes,
        reason,
        selected,
        permanent: true,
        note: None,
    }
}

/// True if the repo enclosing `link` tracks it (a committed `result`
/// link); any git failure counts as tracked.
fn tracked(link: &Path) -> bool {
    link.parent().and_then(crate::git::repo_root).is_some_and(|repo| crate::git::is_tracked(&repo, link))
}

fn symlink_kind(path: PathBuf, target: PathBuf) -> EntryKind {
    EntryKind::Object { obj: Object::Symlink { path, target } }
}

impl Category for Nix {
    fn id(&self) -> &'static str {
        ID
    }

    fn scan(&self, ctx: &ScanCtx, seen: &mut InodeSet) -> anyhow::Result<ScanOutput> {
        let cfg = &ctx.config.nix;
        let mut out = ScanOutput::default();
        if !cfg.enabled {
            return Ok(out);
        }
        if ctx.tools.which("nix-env").is_none() {
            out.skipped.push(Skip { category: ID.into(), title: "Nix".into(), reason: "nix is not installed".into() });
            return Ok(out);
        }

        let profiles_dir = crate::paths::expand(&cfg.profiles_dir, ctx.home).into_iter().next();
        let (profiles, orphans) = match &profiles_dir {
            Some(dir) => user_profiles(dir, cfg, ctx),
            None => (Vec::new(), Vec::new()),
        };

        // The system profile is read from its own dir; an unreadable one gives no row.
        let system = (!cfg.system_profile.is_empty())
            .then(|| plan_profile(Path::new(&cfg.system_profile), cfg))
            .flatten();

        // DECISION D5: sizes count only what no kept generation (or the running system) needs.
        let mut kept: Vec<String> =
            profiles.iter().chain(&system).flat_map(|p| p.kept.iter().cloned()).collect();
        if Path::new(CURRENT_SYSTEM).exists() {
            kept.push(CURRENT_SYSTEM.into());
        }
        let kept = closure(ctx.tools, &kept);
        let estimate =
            |paths: &[String]| kept.as_ref().and_then(|k| estimate_against(ctx.tools, paths, k)).unwrap_or(0);

        for p in profiles.iter().filter(|p| !p.candidates.is_empty()) {
            let ns: Vec<u32> = p.candidates.iter().map(|(n, _)| *n).collect();
            let stores: Vec<String> = p.candidates.iter().map(|(_, s)| s.clone()).collect();
            out.entries.push(entry(
                format!("profile {}: generations {}", p.name, gen_list(&ns)),
                EntryKind::Object {
                    obj: Object::NixGenerations { profile: p.path.clone(), gens: p.candidates.clone(), root: false },
                },
                estimate(&stores),
                format!("keeping newest {} of {}", p.keep, p.total),
                true,
            ));
        }

        // DECISION D3: one unselected row; applying it asks for the password.
        if let Some(p) = system.as_ref().filter(|p| !p.candidates.is_empty()) {
            let ns: Vec<u32> = p.candidates.iter().map(|(n, _)| *n).collect();
            let stores: Vec<String> = p.candidates.iter().map(|(_, s)| s.clone()).collect();
            out.entries.push(entry(
                format!("system: generations {}", gen_list(&ns)),
                EntryKind::Object {
                    obj: Object::NixGenerations { profile: p.path.clone(), gens: p.candidates.clone(), root: true },
                },
                estimate(&stores),
                format!("keeping newest {} of {}; needs your password (pkexec)", p.keep, p.total),
                false,
            ));
        }

        for (link, target) in orphans {
            let bytes = target.to_str().map_or(0, |t| estimate(&[t.to_string()]));
            let title = format!("orphaned profile link {}", link.file_name().unwrap_or_default().to_string_lossy());
            out.entries.push(entry(
                title,
                symlink_kind(link, target),
                bytes,
                "generation link of a profile that no longer exists".into(),
                true,
            ));
        }

        let (links, devboxes) = walk_roots(cfg, ctx);
        for (link, target) in links {
            let title = rel(ctx.home, &link);
            if tracked(&link) {
                out.skipped.push(Skip { category: ID.into(), title, reason: "tracked in git".into() });
                continue;
            }
            let e = if target.exists() {
                let bytes = target.to_str().map_or(0, |t| closure_size(ctx.tools, t));
                entry(title, symlink_kind(link, target), bytes, "live GC root; kept unless you tick it".into(), false)
            } else {
                entry(title, symlink_kind(link, target), 0, "dangling; pins nothing".into(), true)
            };
            out.entries.push(e);
        }
        for dir in devboxes {
            if let Some(e) = devbox_entry(&dir, ctx, seen) {
                out.entries.push(e);
            }
        }
        if cfg.gc && ctx.tools.which("nix-store").is_some() {
            out.entries.push(entry(
                "Nix store garbage collection".into(),
                EntryKind::Operation { op: Operation::NixStoreGc },
                dead_bytes(ctx.tools),
                "frees store paths nothing references (after the Nix rows above)".into(),
                true,
            ));
        }
        Ok(out)
    }

    fn apply(&self, entry: &Entry, ctx: &ScanCtx) -> Outcome {
        match &entry.kind {
            EntryKind::Object { obj: Object::NixGenerations { profile, gens, root } } => {
                apply_generations(profile, gens, *root, ctx)
            }
            EntryKind::Object { obj: Object::Symlink { path, target } } => unlink(path, target, ctx),
            EntryKind::Operation { op: Operation::NixStoreGc } => store_gc(ctx),
            _ => apply_default(entry, ctx),
        }
    }
}

/// Profiles in `dir` (a `NAME` symlink to its `NAME-N-link`), by name, and
/// orphaned `NAME-N-link`s with no `NAME` at all, as (link, target).
fn user_profiles(dir: &Path, cfg: &NixCfg, ctx: &ScanCtx) -> (Vec<ProfilePlan>, Vec<(PathBuf, PathBuf)>) {
    let mut profiles = Vec::new();
    let mut orphans = Vec::new();
    let Ok(rd) = std::fs::read_dir(dir) else { return (profiles, orphans) };
    for e in rd.flatten() {
        if !e.file_type().is_ok_and(|t| t.is_symlink()) || ctx.protector.is_protected(&e.path()) {
            continue;
        }
        let file = e.file_name();
        if let Some((name, _)) = parse_gen_link(&file) {
            if dir.join(name).symlink_metadata().is_err()
                && let Ok(target) = std::fs::read_link(e.path())
            {
                orphans.push((e.path(), target));
            }
        } else if let Some(p) = plan_profile(&e.path(), cfg) {
            profiles.push(p);
        }
    }
    profiles.sort_by(|a, b| a.name.cmp(&b.name));
    orphans.sort();
    (profiles, orphans)
}

/// One walk over `nix.roots` for `result*` links into the store (as (link,
/// target)) and devbox project dirs, each sorted by path.
fn walk_roots(cfg: &NixCfg, ctx: &ScanCtx) -> (Vec<(PathBuf, PathBuf)>, Vec<PathBuf>) {
    let roots = crate::paths::expand_roots(ctx.config.roots_for(&cfg.roots), ctx.home);
    let artifact_names: Vec<glob::Pattern> = crate::category::artifacts::legacy_artifact_patterns()
        .into_iter()
        .chain(
            ctx.config
                .artifacts
                .extra_rules
                .iter()
                .filter_map(|r| crate::category::artifacts::parse_custom_rule(r).ok())
                .flat_map(|e| e.artifacts),
        )
        .collect();
    // Artifact dirs, and `.devbox` itself (its contents are never result links).
    let prune = |p: &Path| {
        p.file_name().and_then(OsStr::to_str).is_some_and(|n| {
            n == ".devbox" || artifact_names.iter().any(|pat| pat.matches(n))
        })
    };
    let mut links = Vec::new();
    let mut devboxes = Vec::new();
    crate::walk::walk_dirs(&roots, cfg.max_depth, ctx.protector, &prune, &mut |dir, _| {
        let Ok(rd) = std::fs::read_dir(dir) else { return };
        for e in rd.flatten() {
            let name = e.file_name();
            let is_result = name == "result" || name.as_encoded_bytes().starts_with(b"result-");
            if !is_result || !e.file_type().is_ok_and(|t| t.is_symlink()) {
                continue;
            }
            let path = e.path();
            let Ok(target) = std::fs::read_link(&path) else { continue };
            if target.as_os_str().as_encoded_bytes().starts_with(STORE.as_bytes())
                && !ctx.protector.is_protected(&path)
            {
                links.push((path, target));
            }
        }
        let is_file = |p: PathBuf| p.symlink_metadata().is_ok_and(|m| m.is_file());
        let is_dir = |p: PathBuf| p.symlink_metadata().is_ok_and(|m| m.is_dir());
        if is_file(dir.join("devbox.json")) && is_dir(dir.join(".devbox")) {
            devboxes.push(dir.to_path_buf());
        }
    });
    links.sort();
    devboxes.sort();
    (links, devboxes)
}

/// narSize sum of the paths `nix-store --gc --print-dead` lists; 0 if either
/// tool fails (the row stays, unsized).
fn dead_bytes(tools: &dyn Runner) -> u64 {
    let args = ["--gc", "--print-dead"].map(OsStr::new);
    let Some(out) = tools.run("nix-store", &args, QUERY_TIMEOUT).ok().filter(|o| o.ok()) else { return 0 };
    let dead: Vec<String> = out.stdout.lines().filter(|l| l.starts_with(STORE)).map(str::to_string).collect();
    dead.chunks(500)
        .filter_map(|chunk| path_info(tools, None, chunk))
        .flatten()
        .filter_map(|(_, v)| v.get("narSize")?.as_u64())
        .sum()
}

/// `nix-store --gc`; bytes from "N store paths deleted, 812.40 MiB freed".
fn store_gc(ctx: &ScanCtx) -> Outcome {
    match ctx.tools.run("nix-store", &[OsStr::new("--gc")], GC_TIMEOUT) {
        Ok(out) if out.ok() => {
            let freed = out.stdout.lines().chain(out.stderr.lines()).find_map(|l| {
                let (_, rest) = l.split_once(" store paths deleted, ")?;
                parse_human_size(rest.trim().strip_suffix("freed")?)
            });
            Outcome::Deleted { bytes: freed.unwrap_or(0), failures: Vec::new() }
        }
        Ok(out) => Outcome::Failed {
            error: format!("nix-store refused: {}", out.stderr.lines().next().unwrap_or("").trim()),
        },
        Err(e @ ToolError::Missing(_)) => Outcome::Skipped { reason: e.to_string() },
        Err(e) => Outcome::Failed { error: e.to_string() },
    }
}

/// `closureSize` from `nix path-info --json -S <path>` (its entry, or the
/// only one if nix printed the path differently), or 0.
fn closure_size(tools: &dyn Runner, path: &str) -> u64 {
    let Some(infos) = path_info(tools, Some("-S"), &[path.to_string()]) else { return 0 };
    let info = match infos.iter().find(|(p, _)| p == path) {
        Some((_, v)) => Some(v),
        None if infos.len() == 1 => Some(&infos[0].1),
        None => None,
    };
    info.and_then(|v| v.get("closureSize")?.as_u64()).unwrap_or(0)
}

/// An Item for the devbox state under `dir` when the project is idle: the
/// newest lstat mtime of `.devbox`, `devbox.json`, `devbox.lock` and each
/// existing target is older than `devbox_idle_days`.
fn devbox_entry(dir: &Path, ctx: &ScanCtx, seen: &mut InodeSet) -> Option<Entry> {
    let mtime = |p: &Path| p.symlink_metadata().ok().map(|m| m.mtime());
    let mut newest = ["devbox.json", "devbox.lock", ".devbox"].iter().filter_map(|f| mtime(&dir.join(f))).max()?;
    let mut targets = Vec::new();
    for rel_path in DEVBOX_TARGETS {
        let path = dir.join(rel_path);
        if ctx.protector.is_protected(&path) {
            continue;
        }
        // Missing targets and symlinks have no fingerprint.
        let (Ok(fp), Some(m)) = (fingerprint(&path), mtime(&path)) else { continue };
        newest = newest.max(m);
        targets.push(Target { path, fp });
    }
    let cutoff = ctx.now - ctx.config.nix.devbox_idle_days as i64 * 86_400;
    if targets.is_empty() || newest >= cutoff {
        return None;
    }
    let bytes = targets.iter().filter_map(|t| measure(&t.path, seen).ok()).map(|m| m.bytes).sum();
    let days = (ctx.now - newest) / 86_400;
    Some(entry(
        format!("devbox {}", rel(ctx.home, dir)),
        EntryKind::Item { targets },
        bytes,
        format!("devbox project untouched for {days} days"),
        true,
    ))
}

/// Re-checks each planned generation (its link still points at the planned
/// store path and it isn't current now), then deletes the rest by exact number.
fn apply_generations(profile: &Path, gens: &[(u32, String)], root: bool, ctx: &ScanCtx) -> Outcome {
    let Some(name) = profile.file_name().and_then(OsStr::to_str) else {
        return Outcome::Skipped { reason: "not a profile path".into() };
    };
    let current = current_generation(profile);
    let mut failures = Vec::new();
    let mut ns = Vec::new();
    for (n, store) in gens {
        let link = profile.with_file_name(format!("{name}-{n}-link"));
        let same = std::fs::read_link(&link).is_ok_and(|t| t.as_os_str() == OsStr::new(store));
        if same && current != Some(*n) {
            ns.push(n.to_string());
        } else {
            failures.push(format!("generation {n} changed since plan"));
        }
    }
    if ns.is_empty() {
        return Outcome::Skipped { reason: "nothing left to delete".into() };
    }
    let mut args: Vec<OsString> = Vec::new();
    // System generations run `pkexec <absolute nix-env>`; polkit wants a full path.
    let (program, timeout) = if root {
        if ctx.tools.which("pkexec").is_none() {
            return Outcome::Skipped { reason: "pkexec is not installed".into() };
        }
        let Some(nix_env) = ctx.tools.which("nix-env") else {
            return Outcome::Skipped { reason: "nix-env is not installed".into() };
        };
        args.push(nix_env.into_os_string());
        ("pkexec", PKEXEC_TIMEOUT)
    } else {
        ("nix-env", OP_TIMEOUT)
    };
    args.extend(["-p".into(), profile.into(), "--delete-generations".into()]);
    args.extend(ns.into_iter().map(OsString::from));
    let args: Vec<&OsStr> = args.iter().map(OsString::as_os_str).collect();
    // pkexec may need to show its text prompt in the CLI (spec §5.2f).
    let res = if root { ctx.tools.run_interactive(program, &args, timeout) } else { ctx.tools.run(program, &args, timeout) };
    match res {
        Ok(out) if root && matches!(out.code, Some(126 | 127)) => {
            Outcome::Skipped { reason: "pkexec cancelled or not authorized".into() }
        }
        // Store space comes back through the GC row.
        Ok(out) if out.ok() => Outcome::Deleted { bytes: 0, failures },
        Ok(out) => {
            // An interactive pkexec leaves stderr on the terminal.
            let why = out.stderr.lines().next().map(str::trim).filter(|l| !l.is_empty()).map_or_else(
                || out.code.map_or("killed by a signal".to_string(), |c| format!("exit code {c}")),
                str::to_string,
            );
            Outcome::Failed { error: format!("nix-env refused: {why}") }
        }
        Err(e @ ToolError::Missing(_)) => Outcome::Skipped { reason: e.to_string() },
        Err(ToolError::Timeout(..)) if root => {
            Outcome::Skipped { reason: "pkexec timed out waiting for authentication".into() }
        }
        Err(e) => Outcome::Failed { error: e.to_string() },
    }
}

/// Unlinks `path` if it is still a symlink to `target` and not protected.
/// `remove_file` is unlink(2): the target is never followed.
fn unlink(path: &Path, target: &Path, ctx: &ScanCtx) -> Outcome {
    let skip = |reason: &str| Outcome::Skipped { reason: reason.into() };
    match std::fs::symlink_metadata(path) {
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return skip("no longer exists"),
        Err(e) => return skip(&format!("cannot inspect: {e}")),
        Ok(md) if !md.file_type().is_symlink() => return skip("changed since plan"),
        Ok(_) => {}
    }
    if std::fs::read_link(path).ok().as_deref() != Some(target) {
        return skip("changed since plan");
    }
    if ctx.protector.is_protected(path) {
        return skip("protected");
    }
    if tracked(path) {
        return skip("tracked in git");
    }
    match std::fs::remove_file(path) {
        Ok(()) => Outcome::Deleted { bytes: 0, failures: Vec::new() },
        Err(e) => Outcome::Failed { error: format!("unlink failed: {e}") },
    }
}
