//! Old IDE versions: JetBrains `<Product><Version>` cache and data dirs
//! older than the newest version of that product, and VS Code-style
//! extension dirs older than the newest version of that extension that no
//! `extensions.json` uses. JetBrains config dirs hold user settings: they
//! are read to find the newest version and never offered.

use std::collections::{HashMap, HashSet};
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};

use crate::category::{Category, ScanCtx, ScanOutput, item_targets};
use crate::fmt::plural;
use crate::fsutil::measure::InodeSet;
use crate::plan::{Entry, EntryKind, Skip};

pub struct IdeVersions;

const ID: &str = "ide_versions";

/// JetBrains per-version dirs under home, and whether old ones are offered.
const JETBRAINS: &[(&str, bool)] = &[
    (".cache/JetBrains", true),
    (".local/share/JetBrains", true),
    (".config/JetBrains", false),
];

/// Extension dirs under home, with the profile `extensions.json` globs of
/// the same editor (each profile lists the extension versions it uses).
const VSCODE: &[(&str, &[&str])] = &[
    (
        ".vscode/extensions",
        &[".config/Code/User/profiles/*/extensions.json"],
    ),
    (
        ".vscode-oss/extensions",
        &[
            ".config/VSCodium/User/profiles/*/extensions.json",
            ".config/Code - OSS/User/profiles/*/extensions.json",
        ],
    ),
    (
        ".cursor/extensions",
        &[".config/Cursor/User/profiles/*/extensions.json"],
    ),
];

type Version = Vec<u64>;

impl Category for IdeVersions {
    fn id(&self) -> &'static str {
        ID
    }

    fn scan(&self, ctx: &ScanCtx, seen: &mut InodeSet) -> anyhow::Result<ScanOutput> {
        let cfg = &ctx.config.ide_versions;
        let mut out = ScanOutput::default();
        if !cfg.enabled {
            return Ok(out);
        }
        if cfg.jetbrains {
            jetbrains(ctx, seen, &mut out);
        }
        if cfg.vscode {
            for (dir, profiles) in VSCODE {
                vscode(&ctx.home.join(dir), profiles, ctx, seen, &mut out);
            }
        }
        Ok(out)
    }
}

/// `1.2.3` → `[1, 2, 3]`; any non-numeric part → None.
fn dotted(s: &str) -> Option<Version> {
    s.split('.')
        .map(|p| {
            (!p.is_empty() && p.bytes().all(|b| b.is_ascii_digit()))
                .then(|| p.parse().ok())
                .flatten()
        })
        .collect()
}

fn show(v: &[u64]) -> String {
    v.iter().map(u64::to_string).collect::<Vec<_>>().join(".")
}

/// A version dir found in a scanned parent.
struct Found {
    path: PathBuf,
    key: String,
    version: Version,
    /// A real directory on the parent's filesystem (not a symlink or mount).
    plain: bool,
}

/// Entries of `dir` that `parse` names, if `dir` is a real directory.
fn version_dirs(dir: &Path, parse: &dyn Fn(&str) -> Option<(String, Version)>) -> Vec<Found> {
    let Ok(md) = std::fs::symlink_metadata(dir) else {
        return Vec::new();
    };
    if !md.is_dir() {
        return Vec::new();
    }
    let Ok(rd) = std::fs::read_dir(dir) else {
        return Vec::new();
    };
    let mut v: Vec<Found> = rd
        .flatten()
        .filter_map(|e| {
            let (key, version) = parse(&e.file_name().to_string_lossy())?;
            let emd = e.metadata().ok()?; // lstat
            Some(Found {
                path: e.path(),
                key,
                version,
                plain: emd.is_dir() && emd.dev() == md.dev(),
            })
        })
        .collect();
    v.sort_by(|a, b| a.path.cmp(&b.path));
    v
}

/// The newest version per key. Symlinked entries count too, so a newer
/// version is never missed (that only offers less).
fn newest(found: &[&Found]) -> HashMap<String, Version> {
    let mut m: HashMap<String, Version> = HashMap::new();
    for f in found {
        let cur = m.entry(f.key.clone()).or_default();
        if f.version > *cur {
            *cur = f.version.clone();
        }
    }
    m
}

/// One `Item` row for `path`, or a Skip saying why not.
fn push(
    out: &mut ScanOutput,
    ctx: &ScanCtx,
    seen: &mut InodeSet,
    path: &Path,
    reason: &dyn Fn(i64) -> (String, bool),
) {
    match item_targets(&[path.to_path_buf()], ctx, seen) {
        Ok((targets, bytes, newest)) => {
            let (reason, selected) = reason(newest);
            out.entries.push(Entry {
                id: String::new(),
                category: ID.into(),
                title: crate::paths::tilde(path, ctx.home),
                kind: EntryKind::Item { targets },
                bytes,
                reason,
                selected,
                permanent: false,
                note: None,
                last_active: Some(newest),
            });
        }
        Err(reason) => out.skipped.push(Skip {
            category: ID.into(),
            title: crate::paths::tilde(path, ctx.home),
            reason,
        }),
    }
}

// ---- JetBrains ------------------------------------------------------------

/// `IntelliJIdea2024.3` → ("IntelliJIdea", [2024, 3]): a product name with
/// no digits, then a `YYYY.N[.M]` version.
fn jetbrains_version(name: &str) -> Option<(String, Version)> {
    let at = name.find(|c: char| c.is_ascii_digit())?;
    let (product, ver) = name.split_at(at);
    let v = dotted(ver)?;
    let year_ok = ver.split('.').next().is_some_and(|y| y.len() == 4);
    (!product.is_empty() && year_ok && v.len() >= 2).then(|| (product.to_string(), v))
}

fn jetbrains(ctx: &ScanCtx, seen: &mut InodeSet, out: &mut ScanOutput) {
    let all: Vec<(Found, bool)> = JETBRAINS
        .iter()
        .flat_map(|(dir, offer)| {
            version_dirs(&ctx.home.join(dir), &jetbrains_version)
                .into_iter()
                .map(|f| (f, *offer))
        })
        .collect();
    let newest = newest(&all.iter().map(|(f, _)| f).collect::<Vec<_>>());
    let idle_secs = ctx.config.ide_versions.idle_days.saturating_mul(86_400) as i64;
    for (f, offer) in &all {
        let top = &newest[&f.key];
        if !offer || !f.plain || f.version >= *top || ctx.protector.is_protected(&f.path) {
            continue;
        }
        let what = format!("{} {} is older than {}", f.key, show(&f.version), show(top));
        push(out, ctx, seen, &f.path, &|mtime| {
            let idle = ctx.now - mtime;
            let days = plural(idle.max(0) / 86_400, "day");
            if idle >= idle_secs {
                (format!("{what}; untouched for {days}"), true)
            } else {
                (format!("{what} but was used {days} ago"), false)
            }
        });
    }
}

// ---- VS Code --------------------------------------------------------------

/// `ms-python.python-2024.1.0` or `rust-lang.rust-analyzer-0.3.1-linux-x64`
/// → (lowercased `publisher.name`, version). The id is everything before
/// the first `-`-separated part that is a dotted version.
fn vscode_version(name: &str) -> Option<(String, Version)> {
    let parts: Vec<&str> = name.split('-').collect();
    (1..parts.len()).find_map(|i| {
        let id = parts[..i].join("-");
        let v = dotted(parts[i]).filter(|v| v.len() >= 2)?;
        id.contains('.').then(|| (id.to_lowercase(), v))
    })
}

/// What an `extensions.json` says is in use: dir names (lowercased) and
/// (id, version) pairs.
#[derive(Default)]
struct Used {
    dirs: HashSet<String>,
    versions: HashSet<(String, Version)>,
}

impl Used {
    fn read(&mut self, path: &Path) -> Result<(), String> {
        let text = std::fs::read_to_string(path).map_err(|e| e.to_string())?;
        let v: serde_json::Value = serde_json::from_str(&text).map_err(|e| e.to_string())?;
        let items = v.as_array().ok_or("not a list")?;
        for it in items {
            let s = |v: Option<&serde_json::Value>| v.and_then(|v| v.as_str()).map(str::to_string);
            if let Some(d) = s(it.get("relativeLocation")) {
                self.dirs.insert(d.to_lowercase());
            }
            let loc = it.get("location");
            let loc_path = s(loc.and_then(|l| l.get("path"))).or_else(|| s(loc));
            if let Some(name) = loc_path.as_deref().and_then(|p| p.rsplit('/').next()) {
                self.dirs.insert(name.to_lowercase());
            }
            let id = s(it.get("identifier").and_then(|i| i.get("id")));
            let ver = s(it.get("version")).as_deref().and_then(dotted);
            if let (Some(id), Some(ver)) = (id, ver) {
                self.versions.insert((id.to_lowercase(), ver));
            }
        }
        Ok(())
    }

    fn uses(&self, f: &Found) -> bool {
        let name = f.path.file_name().unwrap_or_default().to_string_lossy();
        self.dirs.contains(&name.to_lowercase())
            || self.versions.contains(&(f.key.clone(), f.version.clone()))
    }
}

fn vscode(dir: &Path, profiles: &[&str], ctx: &ScanCtx, seen: &mut InodeSet, out: &mut ScanOutput) {
    let found = version_dirs(dir, &vscode_version);
    if found.is_empty() {
        return;
    }
    let main = dir.join("extensions.json");
    let have_main = std::fs::symlink_metadata(&main).is_ok();
    let lists = have_main.then_some(main).into_iter().chain(
        profiles
            .iter()
            .flat_map(|g| crate::paths::expand(&format!("~/{g}"), ctx.home)),
    );
    let mut used = Used::default();
    for l in lists {
        if let Err(e) = used.read(&l) {
            out.skipped.push(Skip {
                category: ID.into(),
                title: crate::paths::tilde(dir, ctx.home),
                reason: format!(
                    "no old version offered: can't read {} ({e})",
                    crate::paths::tilde(&l, ctx.home)
                ),
            });
            return;
        }
    }
    let newest = newest(&found.iter().collect::<Vec<_>>());
    for f in &found {
        let top = &newest[&f.key];
        if !f.plain || f.version >= *top || used.uses(f) || ctx.protector.is_protected(&f.path) {
            continue;
        }
        let reason = if have_main {
            format!(
                "older than {} {}; no extensions.json uses it",
                f.key,
                show(top)
            )
        } else {
            format!(
                "older than {} {}; no extensions.json to confirm it's unused",
                f.key,
                show(top)
            )
        };
        push(out, ctx, seen, &f.path, &|_| (reason.clone(), have_main));
    }
}
