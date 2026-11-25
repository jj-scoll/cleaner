//! AI model caches: Hugging Face hub repos, Ollama models and LM Studio
//! model dirs, one row per model and never pre-selected. File dates show
//! when a model was downloaded, not when it was last used (atime is
//! unreliable under relatime), so only the user can tell a stale 40 GiB
//! model from one loaded every day.
//!
//! Hugging Face and Ollama store content-addressed blobs that several models
//! may share. A row holds its manifest (Ollama) or repo dir (Hugging Face)
//! first, then only the blobs nothing else references; apply re-reads every
//! manifest and link and skips the row if one of those blobs is now shared.

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::ffi::OsString;
use std::os::unix::fs::MetadataExt;
use std::path::{Component, Path, PathBuf};

use crate::category::{
    Category, Outcome, ScanCtx, ScanOutput, apply_default, apply_targets, item_targets,
};
use crate::fmt::plural;
use crate::fsutil::measure::InodeSet;
use crate::plan::{Entry, EntryKind, Skip, Target};

pub struct AiModels {
    /// The environment variables that move the caches (see [`ENV_KEYS`]).
    pub env: HashMap<String, OsString>,
}

const ID: &str = "ai_models";

/// Read by [`AiModels::from_process_env`].
pub const ENV_KEYS: &[&str] = &[
    "HF_HUB_CACHE",
    "HUGGINGFACE_HUB_CACHE",
    "HF_HOME",
    "XDG_CACHE_HOME",
    "OLLAMA_MODELS",
];

/// LM Studio's models dir (current, then the 0.2.x location).
const LMSTUDIO_DIRS: &[&str] = &[".lmstudio/models", ".cache/lm-studio/models"];

const SYMLINK: &str = "is a symlink; cleaner never follows symlinks";

impl AiModels {
    pub fn from_process_env() -> AiModels {
        let env = ENV_KEYS
            .iter()
            .filter_map(|k| Some((k.to_string(), std::env::var_os(k)?)))
            .collect();
        AiModels { env }
    }

    /// `key` as an absolute path, with surrounding quotes or spaces trimmed
    /// and a leading `~` expanded; unset, empty or relative counts as unset.
    fn env_path(&self, key: &str, home: &Path) -> Option<PathBuf> {
        let raw = self.env.get(key)?;
        let p = match raw.to_str() {
            Some(s) => {
                let s = s.trim().trim_matches(['"', '\'']);
                match s.strip_prefix('~') {
                    Some("") => home.to_path_buf(),
                    Some(rest) if rest.starts_with('/') => home.join(&rest[1..]),
                    _ => PathBuf::from(s),
                }
            }
            None => PathBuf::from(raw),
        };
        p.is_absolute().then_some(p)
    }

    /// Where huggingface_hub keeps repos: `HF_HUB_CACHE`, the deprecated
    /// `HUGGINGFACE_HUB_CACHE`, `$HF_HOME/hub`, `$XDG_CACHE_HOME/huggingface/hub`,
    /// else `~/.cache/huggingface/hub`.
    pub fn hf_hub(&self, home: &Path) -> PathBuf {
        self.env_path("HF_HUB_CACHE", home)
            .or_else(|| self.env_path("HUGGINGFACE_HUB_CACHE", home))
            .or_else(|| self.env_path("HF_HOME", home).map(|h| h.join("hub")))
            .or_else(|| {
                self.env_path("XDG_CACHE_HOME", home)
                    .map(|c| c.join("huggingface/hub"))
            })
            .unwrap_or_else(|| home.join(".cache/huggingface/hub"))
    }

    /// `OLLAMA_MODELS`, else `~/.ollama/models`.
    pub fn ollama_models(&self, home: &Path) -> PathBuf {
        self.env_path("OLLAMA_MODELS", home)
            .unwrap_or_else(|| home.join(".ollama/models"))
    }
}

impl Category for AiModels {
    fn id(&self) -> &'static str {
        ID
    }

    fn scan(&self, ctx: &ScanCtx, seen: &mut InodeSet) -> anyhow::Result<ScanOutput> {
        let cfg = &ctx.config.ai_models;
        let mut out = ScanOutput::default();
        if !cfg.enabled {
            return Ok(out);
        }
        if cfg.huggingface {
            hf_scan(&self.hf_hub(ctx.home), ctx, seen, &mut out);
        }
        if cfg.ollama {
            ollama_scan(&self.ollama_models(ctx.home), ctx, seen, &mut out);
        }
        if cfg.lmstudio {
            for d in LMSTUDIO_DIRS {
                lmstudio_scan(&ctx.home.join(d), ctx, seen, &mut out);
            }
        }
        Ok(out)
    }

    fn apply(&self, entry: &Entry, ctx: &ScanCtx) -> Outcome {
        let EntryKind::Item { targets } = &entry.kind else {
            return apply_default(entry, ctx);
        };
        if targets.len() > 1
            && let Err(reason) = still_exclusive(targets)
        {
            return Outcome::Skipped { reason };
        }
        apply_targets(targets, ctx)
    }
}

fn skip(path: &Path, home: &Path, reason: &str) -> Skip {
    Skip {
        category: ID.into(),
        title: crate::paths::tilde(path, home),
        reason: reason.into(),
    }
}

/// `root`'s device if it is a real directory. A symlinked root becomes a
/// Skip; a missing one is nothing.
fn root_dev(root: &Path, ctx: &ScanCtx, out: &mut ScanOutput) -> Option<u64> {
    let md = std::fs::symlink_metadata(root).ok()?;
    if md.file_type().is_symlink() {
        out.skipped.push(skip(root, ctx.home, SYMLINK));
        return None;
    }
    md.is_dir().then(|| md.dev())
}

/// Real dirs directly under `dir` on `dev` whose name passes `want`, sorted.
/// Symlinks that pass become Skips; protected dirs and other filesystems
/// mounted here are left out.
fn child_dirs(
    dir: &Path,
    dev: u64,
    ctx: &ScanCtx,
    out: &mut ScanOutput,
    want: &dyn Fn(&str) -> bool,
) -> Vec<PathBuf> {
    let Ok(rd) = std::fs::read_dir(dir) else {
        return Vec::new();
    };
    let mut v = Vec::new();
    for e in rd.flatten() {
        if !want(&e.file_name().to_string_lossy()) {
            continue;
        }
        let Ok(md) = e.metadata() else { continue }; // lstat
        let p = e.path();
        if md.file_type().is_symlink() {
            out.skipped.push(skip(&p, ctx.home, SYMLINK));
        } else if md.is_dir() && md.dev() == dev && !ctx.protector.is_protected(&p) {
            v.push(p);
        }
    }
    v.sort();
    v
}

/// One unticked row for `paths` (the model's dir or manifest first), or a
/// Skip saying why not.
fn push_model(
    out: &mut ScanOutput,
    ctx: &ScanCtx,
    seen: &mut InodeSet,
    title: String,
    paths: Vec<PathBuf>,
    note: Option<String>,
) {
    match item_targets(&paths, ctx, seen) {
        Ok((targets, bytes, newest)) => {
            let days = (ctx.now - newest).max(0) / 86_400;
            out.entries.push(Entry {
                id: String::new(),
                category: ID.into(),
                title,
                kind: EntryKind::Item { targets },
                bytes,
                reason: format!(
                    "files last changed {} ago; downloads again when next used",
                    plural(days, "day")
                ),
                selected: false,
                permanent: false,
                note,
                last_active: Some(newest),
            });
        }
        Err(reason) => out.skipped.push(skip(&paths[0], ctx.home, &reason)),
    }
}

/// `p` is a regular file on `dev` reached from `base` through real
/// directories only (no symlinked dir in between).
fn plain_file_under(base: &Path, p: &Path, dev: u64) -> bool {
    let Ok(rel) = p.strip_prefix(base) else {
        return false;
    };
    let mut cur = base.to_path_buf();
    let comps: Vec<Component> = rel.components().collect();
    for (i, c) in comps.iter().enumerate() {
        let Component::Normal(n) = c else {
            return false;
        };
        cur.push(n);
        let Ok(md) = std::fs::symlink_metadata(&cur) else {
            return false;
        };
        let last = i + 1 == comps.len();
        let ok = if last { md.is_file() } else { md.is_dir() };
        if !ok || md.dev() != dev {
            return false;
        }
    }
    !comps.is_empty()
}

/// The blobs after `targets[0]` (an Ollama manifest or a Hugging Face repo
/// dir) must be in that store and used by nothing else now.
fn still_exclusive(targets: &[Target]) -> Result<(), String> {
    let owner = &targets[0].path;
    let blobs = &targets[1..];
    let md = std::fs::symlink_metadata(owner).map_err(|_| "no longer exists".to_string())?;
    let users_of = |what: &str, by: &Path| format!("{what} is now also used by {}", by.display());
    if md.is_file() {
        let root = ollama_root_of(owner).ok_or("not in an Ollama models dir")?;
        let manifests = ollama_refs(&root)?;
        for t in blobs {
            if t.path.parent() != Some(root.join("blobs").as_path()) {
                return Err(format!("{} is not an Ollama blob", t.path.display()));
            }
            let name = t.path.file_name().unwrap_or_default().to_string_lossy();
            let key = name.replacen(':', "-", 1);
            if let Some((m, _)) = manifests
                .iter()
                .find(|(m, ds)| *m != owner && ds.contains(&key))
            {
                return Err(users_of(&name, m));
            }
        }
        Ok(())
    } else if md.is_dir() {
        let hub = owner
            .parent()
            .filter(|_| {
                hf_repo_kind(&owner.file_name().unwrap_or_default().to_string_lossy()).is_some()
            })
            .ok_or("not a Hugging Face repo")?;
        let refs = hf_shared_refs(hub).ok_or("can't tell which repos use the shared blobs")?;
        for t in blobs {
            if !t.path.starts_with(hub.join("blobs")) {
                return Err(format!("{} is not a shared blob", t.path.display()));
            }
            let s = t.path.as_os_str().to_string_lossy();
            let blob = PathBuf::from(s.strip_suffix(".refs").unwrap_or(&s));
            if let Some(other) = refs
                .get(&blob)
                .and_then(|users| users.iter().find(|u| *u != owner))
            {
                return Err(users_of(&blob.display().to_string(), other));
            }
        }
        Ok(())
    } else {
        Err("is no longer a file or directory".into())
    }
}

// ---- Hugging Face ---------------------------------------------------------

/// `models--org--name` → ("model", "org/name"); datasets and spaces alike.
fn hf_repo_kind(name: &str) -> Option<(&'static str, String)> {
    [
        ("models--", "model"),
        ("datasets--", "dataset"),
        ("spaces--", "space"),
    ]
    .into_iter()
    .find_map(|(prefix, kind)| {
        let rest = name.strip_prefix(prefix).filter(|r| !r.is_empty())?;
        Some((kind, rest.replace("--", "/")))
    })
}

/// `base.join(target)` with `.` and `..` resolved lexically. Only used on
/// links found inside real (non-symlink) directories, where that matches
/// what the kernel resolves.
fn resolve_link(link: &Path, target: &Path) -> Option<PathBuf> {
    let joined = link.parent()?.join(target); // an absolute target replaces the base
    let mut out = PathBuf::new();
    for c in joined.components() {
        match c {
            Component::ParentDir => {
                if !out.pop() {
                    return None;
                }
            }
            Component::CurDir => {}
            other => out.push(other),
        }
    }
    Some(out)
}

/// Every symlink under `dir` (never followed) with its target; None if
/// any directory or link can't be read.
fn symlinks_under(dir: &Path) -> Option<Vec<(PathBuf, PathBuf)>> {
    let mut out = Vec::new();
    let mut stack = vec![dir.to_path_buf()];
    while let Some(d) = stack.pop() {
        for e in std::fs::read_dir(&d).ok()? {
            let e = e.ok()?;
            let md = e.metadata().ok()?; // lstat
            let p = e.path();
            if md.file_type().is_symlink() {
                let t = std::fs::read_link(&p).ok()?;
                out.push((p, t));
            } else if md.is_dir() {
                stack.push(p);
            }
        }
    }
    Some(out)
}

/// For each file in the cache-wide shared blob store `hub/blobs`, the
/// top-level entries of `hub` that link to it. None when that can't be
/// told for sure: an unreadable dir, or a symlinked entry whose links we
/// won't follow.
fn hf_shared_refs(hub: &Path) -> Option<BTreeMap<PathBuf, BTreeSet<PathBuf>>> {
    let store = hub.join("blobs");
    let mut refs: BTreeMap<PathBuf, BTreeSet<PathBuf>> = BTreeMap::new();
    if !std::fs::symlink_metadata(&store).is_ok_and(|m| m.is_dir()) {
        return Some(refs);
    }
    for e in std::fs::read_dir(hub).ok()? {
        let e = e.ok()?;
        let md = e.metadata().ok()?;
        let p = e.path();
        if p == store {
            continue;
        }
        if md.file_type().is_symlink() {
            return None;
        }
        if !md.is_dir() {
            continue;
        }
        for (link, target) in symlinks_under(&p)? {
            if let Some(t) = resolve_link(&link, &target).filter(|t| t.starts_with(&store)) {
                refs.entry(t).or_default().insert(p.clone());
            }
        }
    }
    Some(refs)
}

fn hf_scan(hub: &Path, ctx: &ScanCtx, seen: &mut InodeSet, out: &mut ScanOutput) {
    let Some(dev) = root_dev(hub, ctx, out) else {
        return;
    };
    let repos = child_dirs(hub, dev, ctx, out, &|n| hf_repo_kind(n).is_some());
    if repos.is_empty() {
        return;
    }
    let refs = hf_shared_refs(hub);
    let store = hub.join("blobs");
    for repo in repos {
        let name = repo.file_name().unwrap_or_default().to_string_lossy();
        let (kind, id) = hf_repo_kind(&name).expect("filtered above");
        let mut paths = vec![repo.clone()];
        let mut kept = 0;
        let note = match &refs {
            Some(refs) => {
                for (blob, users) in refs.iter().filter(|(_, u)| u.contains(&repo)) {
                    if users.len() > 1 || !plain_file_under(&store, blob, dev) {
                        kept += 1;
                        continue;
                    }
                    paths.push(blob.clone());
                    let sidecar = PathBuf::from(format!("{}.refs", blob.display()));
                    if plain_file_under(&store, &sidecar, dev) {
                        paths.push(sidecar);
                    }
                }
                (kept > 0).then(|| {
                    format!(
                        "{} in the shared blob store also used by other models are kept",
                        plural(kept, "file")
                    )
                })
            }
            None => Some(
                "its files in the shared blob store are kept (can't tell what else uses them)"
                    .into(),
            ),
        };
        push_model(
            out,
            ctx,
            seen,
            format!("Hugging Face {kind} {id}"),
            paths,
            note,
        );
    }
}

// ---- Ollama ---------------------------------------------------------------

/// `sha256:<64 hex>` (or the `sha256-` file-name form) → `sha256-<hex>`;
/// anything else is refused, so a manifest can never name a path.
fn blob_key(digest: &str) -> Option<String> {
    let hex = digest
        .strip_prefix("sha256:")
        .or_else(|| digest.strip_prefix("sha256-"))?;
    let ok = hex.len() == 64
        && hex
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b));
    ok.then(|| format!("sha256-{hex}"))
}

/// The blob keys one manifest references (config and layers).
fn manifest_refs(text: &str) -> Result<BTreeSet<String>, String> {
    let v: serde_json::Value = serde_json::from_str(text).map_err(|e| e.to_string())?;
    let layers = v
        .get("layers")
        .and_then(|l| l.as_array())
        .ok_or("no layers list")?;
    let config = v.get("config").map(|c| c.get("digest"));
    let mut out = BTreeSet::new();
    for d in layers.iter().map(|l| l.get("digest")).chain(config) {
        let d = d.and_then(|d| d.as_str()).ok_or("a layer has no digest")?;
        out.insert(blob_key(d).ok_or_else(|| format!("bad digest {d:?}"))?);
    }
    Ok(out)
}

/// Every manifest under `root/manifests` with the blob keys it references.
/// `Err` if any can't be read safely (a symlink, unreadable, or not a
/// manifest): its blobs could be anything, so nothing may be deleted.
fn ollama_refs(root: &Path) -> Result<BTreeMap<PathBuf, BTreeSet<String>>, String> {
    let dir = root.join("manifests");
    let mut out = BTreeMap::new();
    match std::fs::symlink_metadata(&dir) {
        Ok(md) if md.file_type().is_symlink() => {
            return Err(format!("{} {SYMLINK}", dir.display()));
        }
        Ok(md) if md.is_dir() => {}
        _ => return Ok(out),
    }
    let mut stack = vec![dir];
    while let Some(d) = stack.pop() {
        let rd = std::fs::read_dir(&d).map_err(|e| format!("{}: {e}", d.display()))?;
        for e in rd {
            let e = e.map_err(|e| format!("{}: {e}", d.display()))?;
            let p = e.path();
            let md = e.metadata().map_err(|e| format!("{}: {e}", p.display()))?;
            if md.file_type().is_symlink() {
                return Err(format!("manifest {} {SYMLINK}", p.display()));
            } else if md.is_dir() {
                stack.push(p);
            } else {
                let text = std::fs::read_to_string(&p)
                    .map_err(|e| format!("manifest {}: {e}", p.display()))?;
                let refs =
                    manifest_refs(&text).map_err(|e| format!("manifest {}: {e}", p.display()))?;
                out.insert(p, refs);
            }
        }
    }
    Ok(out)
}

/// The models dir holding `manifest` (the parent of its `manifests` ancestor).
fn ollama_root_of(manifest: &Path) -> Option<PathBuf> {
    manifest
        .ancestors()
        .find(|a| a.file_name().is_some_and(|n| n == "manifests"))?
        .parent()
        .map(Path::to_path_buf)
}

/// `registry.ollama.ai/library/llama3/latest` → `llama3:latest`; other
/// namespaces and registries stay in the name.
fn ollama_name(manifests: &Path, manifest: &Path) -> String {
    let rel = manifest.strip_prefix(manifests).unwrap_or(manifest);
    let mut parts: Vec<String> = rel
        .components()
        .map(|c| c.as_os_str().to_string_lossy().into_owned())
        .collect();
    let Some(tag) = parts.pop() else {
        return rel.display().to_string();
    };
    if parts.first().is_some_and(|p| p == "registry.ollama.ai") {
        parts.remove(0);
        if parts.first().is_some_and(|p| p == "library") {
            parts.remove(0);
        }
    }
    format!("{}:{tag}", parts.join("/"))
}

fn ollama_scan(root: &Path, ctx: &ScanCtx, seen: &mut InodeSet, out: &mut ScanOutput) {
    let Some(dev) = root_dev(root, ctx, out) else {
        return;
    };
    let manifests = match ollama_refs(root) {
        Ok(m) => m,
        Err(e) => {
            let reason = format!("no model offered: can't read every manifest safely ({e})");
            out.skipped.push(skip(root, ctx.home, &reason));
            return;
        }
    };
    let blobs = root.join("blobs");
    if std::fs::symlink_metadata(&blobs).is_ok_and(|m| m.file_type().is_symlink()) {
        out.skipped.push(skip(&blobs, ctx.home, SYMLINK));
        return;
    }
    let mut users: HashMap<&str, usize> = HashMap::new();
    for keys in manifests.values() {
        for k in keys {
            *users.entry(k.as_str()).or_default() += 1;
        }
    }
    'models: for (m, keys) in &manifests {
        if ctx.protector.is_protected(m) {
            continue;
        }
        let mut paths = vec![m.clone()];
        let mut kept = 0;
        for k in keys {
            if users[k.as_str()] > 1 {
                kept += 1;
                continue;
            }
            for name in [k.clone(), k.replacen('-', ":", 1)] {
                let p = blobs.join(name);
                if std::fs::symlink_metadata(&p).is_err() {
                    continue; // never downloaded, or the other name form
                }
                if !plain_file_under(&blobs, &p, dev) {
                    let reason = format!("blob {} is not a plain file here", p.display());
                    out.skipped.push(skip(m, ctx.home, &reason));
                    continue 'models;
                }
                paths.push(p);
            }
        }
        let note = (kept > 0).then(|| {
            format!(
                "{} shared with other models {} kept",
                plural(kept, "blob"),
                if kept == 1 { "is" } else { "are" }
            )
        });
        let title = format!("Ollama {}", ollama_name(&root.join("manifests"), m));
        push_model(out, ctx, seen, title, paths, note);
    }
}

// ---- LM Studio ------------------------------------------------------------

/// `<models>/<publisher>/<model>` dirs, one row each.
fn lmstudio_scan(root: &Path, ctx: &ScanCtx, seen: &mut InodeSet, out: &mut ScanOutput) {
    let Some(dev) = root_dev(root, ctx, out) else {
        return;
    };
    let visible = |n: &str| !n.starts_with('.');
    for publisher in child_dirs(root, dev, ctx, out, &visible) {
        for model in child_dirs(&publisher, dev, ctx, out, &visible) {
            let name = |p: &Path| {
                p.file_name()
                    .unwrap_or_default()
                    .to_string_lossy()
                    .into_owned()
            };
            let title = format!("LM Studio {}/{}", name(&publisher), name(&model));
            push_model(out, ctx, seen, title, vec![model], None);
        }
    }
}
