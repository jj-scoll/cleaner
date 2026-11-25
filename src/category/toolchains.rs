//! Old toolchains and runtimes: rustup toolchains, nvm Node and pyenv
//! Python versions, the conda package cache and unused Flatpak runtimes.
//!
//! What a tool still uses is never offered: the default rustup toolchain,
//! the stable channel, the newest numbered release and pinned toolchains;
//! nvm's default alias and newest Node; pyenv's global, newest and pinned
//! Pythons (and bases of pyenv virtualenvs); Flatpak runtimes an app, a
//! pin or a used runtime needs. Apply checks all of that again first.
//! rustup and Flatpak removals go through the tool; nvm and pyenv versions
//! are plain directories, and conda decides for itself.

use std::collections::{BTreeMap, BTreeSet};
use std::ffi::{OsStr, OsString};
use std::path::{Path, PathBuf};
use std::sync::OnceLock;

use crate::category::{Category, Outcome, ScanCtx, ScanOutput, apply_default, apply_targets};
use crate::fsutil::measure::{InodeSet, measure};
use crate::object::{Object, Operation};
use crate::plan::{Entry, EntryKind, Skip, Target, fingerprint};
use crate::tool::{CmdOut, OP_TIMEOUT, QUERY_TIMEOUT, ToolError, parse_human_size};

const ID: &str = "toolchains";
const RUSTUP_MISSING: &str = "rustup is not installed; toolchain may still be used by an IDE";
const CONDA_TITLE: &str = "conda clean --packages --tarballs";
const CONDA_ARGS: [&str; 4] = ["clean", "--packages", "--tarballs", "--yes"];
const FLATPAK_RUNTIMES: [&str; 4] = ["list", "--user", "--runtime", "--columns=ref,size"];
const FLATPAK_APPS: [&str; 3] = ["list", "--app", "--columns=ref,runtime"];
const FLATPAK_PINS: [&str; 2] = ["pin", "--user"];
/// Dirs the pin search never enters (besides what `walk_dirs` skips).
const PIN_PRUNE: [&str; 2] = ["node_modules", "target"];

/// The tool-location variables, read once when the category is built.
/// Unset, empty or relative values count as unset.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct ToolEnv {
    pub rustup_home: Option<PathBuf>,
    pub cargo_home: Option<PathBuf>,
    pub nvm_dir: Option<PathBuf>,
    pub pyenv_root: Option<PathBuf>,
}

impl ToolEnv {
    pub fn from_process() -> ToolEnv {
        ToolEnv::from_vars(|name| std::env::var_os(name))
    }

    pub fn from_vars(get: impl Fn(&str) -> Option<OsString>) -> ToolEnv {
        let abs = |name: &str| {
            let p = PathBuf::from(get(name)?);
            p.is_absolute().then_some(p)
        };
        ToolEnv {
            rustup_home: abs("RUSTUP_HOME"),
            cargo_home: abs("CARGO_HOME"),
            nvm_dir: abs("NVM_DIR"),
            pyenv_root: abs("PYENV_ROOT"),
        }
    }
}

/// Versions pinned by files in scanned projects: `rust-toolchain(.toml)`
/// channels and `.python-version` names.
#[derive(Default)]
struct Pins {
    rust: Vec<String>,
    python: Vec<String>,
}

/// One per run (the engine builds categories per scan or apply).
pub struct Toolchains {
    env: ToolEnv,
    /// Found by one walk of the scan roots, the first time a candidate
    /// needs them.
    pins: OnceLock<Pins>,
}

impl Toolchains {
    pub fn new(env: ToolEnv) -> Toolchains {
        Toolchains {
            env,
            pins: OnceLock::new(),
        }
    }

    fn dir(&self, set: &Option<PathBuf>, home: &Path, default: &str) -> PathBuf {
        set.clone().unwrap_or_else(|| home.join(default))
    }

    fn pins(&self, ctx: &ScanCtx) -> &Pins {
        self.pins.get_or_init(|| find_pins(ctx))
    }
}

impl Category for Toolchains {
    fn id(&self) -> &'static str {
        ID
    }

    fn scan(&self, ctx: &ScanCtx, seen: &mut InodeSet) -> anyhow::Result<ScanOutput> {
        let cfg = &ctx.config.toolchains;
        let mut out = ScanOutput::default();
        if !cfg.enabled {
            return Ok(out);
        }
        if cfg.rustup {
            self.scan_rustup(ctx, seen, &mut out);
        }
        if cfg.nvm {
            let root = self.dir(&self.env.nvm_dir, ctx.home, ".nvm");
            self.scan_versions(Tool::Nvm, &root, ctx, seen, &mut out);
        }
        if cfg.pyenv {
            let root = self.dir(&self.env.pyenv_root, ctx.home, ".pyenv");
            self.scan_versions(Tool::Pyenv, &root, ctx, seen, &mut out);
        }
        if cfg.conda {
            scan_conda(ctx, seen, &mut out);
        }
        if cfg.flatpak {
            scan_flatpak(ctx, &mut out);
        }
        Ok(out)
    }

    fn apply(&self, entry: &Entry, ctx: &ScanCtx) -> Outcome {
        match &entry.kind {
            EntryKind::Object {
                obj: Object::RustupToolchain { name, path },
            } => self.apply_rustup(name, path, ctx),
            EntryKind::Object {
                obj: Object::Flatpak { flatpak_ref },
            } => apply_flatpak(flatpak_ref, entry.bytes, ctx),
            EntryKind::Operation {
                op: Operation::CondaClean,
            } => apply_conda(ctx),
            EntryKind::Item { targets } => {
                if let Some(reason) = targets.iter().find_map(|t| self.still_kept(&t.path, ctx)) {
                    return Outcome::Skipped { reason };
                }
                apply_targets(targets, ctx)
            }
            _ => apply_default(entry, ctx),
        }
    }
}

fn skip(title: &str, reason: String) -> Skip {
    Skip {
        category: ID.into(),
        title: title.into(),
        reason,
    }
}

fn first_line(s: &str) -> &str {
    s.lines()
        .map(str::trim)
        .find(|l| !l.is_empty())
        .unwrap_or("no error output")
}

fn run(
    ctx: &ScanCtx,
    prog: &str,
    args: &[&str],
    timeout: std::time::Duration,
) -> Result<CmdOut, ToolError> {
    let args: Vec<&OsStr> = args.iter().map(OsStr::new).collect();
    ctx.tools.run(prog, &args, timeout)
}

/// Runs a removal: `Deleted { bytes }` on success.
fn removal(ctx: &ScanCtx, prog: &str, args: &[&str], title: &str, bytes: u64) -> Outcome {
    match run(ctx, prog, args, OP_TIMEOUT) {
        Ok(o) if o.ok() => Outcome::Deleted {
            bytes,
            failures: Vec::new(),
        },
        Ok(o) => Outcome::Failed {
            error: format!("{title} failed: {}", first_line(&o.stderr)),
        },
        Err(e @ ToolError::Missing(_)) => Outcome::Skipped {
            reason: e.to_string(),
        },
        Err(e) => Outcome::Failed {
            error: format!("{title} failed: {e}"),
        },
    }
}

/// A file small enough to be a pin or alias, read as text.
fn read_small(path: &Path) -> Option<String> {
    let md = std::fs::symlink_metadata(path).ok()?;
    if !md.is_file() || md.len() > 64 * 1024 {
        return None;
    }
    std::fs::read_to_string(path).ok()
}

/// Real (non-symlink) subdirectories of `dir`, by name, sorted.
fn subdirs(dir: &Path) -> Vec<String> {
    let Ok(rd) = std::fs::read_dir(dir) else {
        return Vec::new();
    };
    let mut v: Vec<String> = rd
        .flatten()
        .filter(|e| e.file_type().is_ok_and(|t| t.is_dir()))
        .filter_map(|e| e.file_name().into_string().ok())
        .collect();
    v.sort();
    v
}

/// A one-target `Item` for `path`, or `None` if it vanished or is a symlink.
fn item(
    path: &Path,
    reason: &str,
    note: Option<&str>,
    permanent: bool,
    ctx: &ScanCtx,
    seen: &mut InodeSet,
) -> Option<Entry> {
    let fp = fingerprint(path).ok()?;
    let bytes = measure(path, seen).ok()?.bytes;
    Some(Entry {
        id: String::new(),
        category: ID.into(),
        title: crate::paths::tilde(path, ctx.home),
        kind: EntryKind::Item {
            targets: vec![Target {
                path: path.to_path_buf(),
                fp,
            }],
        },
        bytes,
        reason: reason.into(),
        selected: false,
        permanent,
        note: note.map(Into::into),
        last_active: None,
    })
}

/// `rust-toolchain.toml`'s `toolchain.channel`, or a legacy one-line file.
fn rust_channel(text: &str) -> Option<String> {
    if let Ok(t) = toml::from_str::<toml::Table>(text) {
        return t.get("toolchain")?.get("channel")?.as_str().map(Into::into);
    }
    let line = text.lines().map(str::trim).find(|l| !l.is_empty())?;
    (!line.contains(['=', '[', ' '])).then(|| line.to_string())
}

fn find_pins(ctx: &ScanCtx) -> Pins {
    let cfg = &ctx.config.artifacts;
    let roots = crate::paths::expand_roots(ctx.config.roots_for(&cfg.roots), ctx.home);
    let mut pins = Pins::default();
    let prune = |p: &Path| {
        p.file_name()
            .is_some_and(|n| PIN_PRUNE.iter().any(|x| n == *x))
    };
    crate::walk::walk_dirs(
        &roots,
        cfg.max_depth,
        ctx.protector,
        &prune,
        &mut |dir, _| {
            for f in ["rust-toolchain.toml", "rust-toolchain"] {
                if let Some(c) = read_small(&dir.join(f)).as_deref().and_then(rust_channel) {
                    pins.rust.push(c);
                }
            }
            if let Some(text) = read_small(&dir.join(".python-version")) {
                pins.python.extend(names_in(&text));
            }
        },
    );
    pins
}

/// Whitespace-separated names, skipping `#` comment lines.
fn names_in(text: &str) -> Vec<String> {
    text.lines()
        .filter(|l| !l.trim_start().starts_with('#'))
        .flat_map(str::split_whitespace)
        .map(String::from)
        .collect()
}

// ---------------------------------------------------------------- rustup

struct Listed {
    name: String,
    default: bool,
    active: bool,
}

/// `rustup toolchain list`: `NAME [(active, default)]` per line.
fn parse_toolchains(stdout: &str) -> Vec<Listed> {
    stdout
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty() && !l.starts_with("no installed toolchains"))
        .map(|l| {
            let (name, rest) = l.split_once(' ').unwrap_or((l, ""));
            Listed {
                name: name.into(),
                default: rest.contains("default"),
                active: rest.contains("active") || rest.contains("override"),
            }
        })
        .collect()
}

/// `1.75.0-x86_64-…` or `1.75-…` → `[1, 75, 0]` / `[1, 75]`.
fn numbered(name: &str) -> Option<Vec<u64>> {
    let v = name.split('-').next()?;
    let parts: Vec<u64> = v
        .split('.')
        .map(|p| p.parse().ok())
        .collect::<Option<_>>()?;
    (2..=3).contains(&parts.len()).then_some(parts)
}

/// `NAME` matches a pin `P` if it is `P` or `P-<host triple>`.
fn rust_matches(pin: &str, name: &str) -> bool {
    name.strip_prefix(pin)
        .is_some_and(|r| r.is_empty() || r.starts_with('-'))
}

/// The toolchains `rustup override set` pinned to directories.
fn rustup_overrides(rustup_home: &Path) -> Vec<String> {
    let Some(text) = read_small(&rustup_home.join("settings.toml")) else {
        return Vec::new();
    };
    let Ok(t) = toml::from_str::<toml::Table>(&text) else {
        return Vec::new();
    };
    t.get("overrides")
        .and_then(|o| o.as_table())
        .map(|o| {
            o.values()
                .filter_map(|v| v.as_str().map(Into::into))
                .collect()
        })
        .unwrap_or_default()
}

impl Toolchains {
    /// `rustup toolchain list` via `rustup` on `PATH`, else
    /// `$CARGO_HOME/bin/rustup`; `Ok(None)` if neither is there.
    fn rustup_list(&self, ctx: &ScanCtx) -> Result<Option<(String, Vec<Listed>)>, String> {
        let cargo_bin = self
            .dir(&self.env.cargo_home, ctx.home, ".cargo")
            .join("bin/rustup");
        let mut progs = vec!["rustup".to_string()];
        if cargo_bin.is_file() {
            progs.push(cargo_bin.display().to_string());
        }
        for prog in progs {
            match run(ctx, &prog, &["toolchain", "list"], QUERY_TIMEOUT) {
                Err(ToolError::Missing(_)) => continue,
                Err(e) => return Err(e.to_string()),
                Ok(o) if !o.ok() => {
                    return Err(format!(
                        "rustup toolchain list failed: {}",
                        first_line(&o.stderr)
                    ));
                }
                Ok(o) => return Ok(Some((prog, parse_toolchains(&o.stdout)))),
            }
        }
        Ok(None)
    }

    /// Why toolchain `name` must stay, if it must. Project pins are looked
    /// up last, so the walk only runs for a real candidate.
    fn rustup_kept(
        &self,
        name: &str,
        listed: &[Listed],
        overrides: &[String],
        ctx: &ScanCtx,
    ) -> Option<&'static str> {
        let t = listed.iter().find(|t| t.name == name)?;
        if t.default {
            return Some("is the default toolchain");
        }
        if t.active {
            return Some("is the active toolchain");
        }
        if rust_matches("stable", name) {
            return Some("is the stable channel");
        }
        let newest = listed.iter().filter_map(|t| numbered(&t.name)).max();
        if newest.is_some() && numbered(name) == newest {
            return Some("is the newest numbered release");
        }
        let pinned = |pins: &[String]| pins.iter().any(|p| rust_matches(p, name));
        if pinned(overrides) || pinned(&self.pins(ctx).rust) {
            return Some("is pinned by a project");
        }
        None
    }

    fn scan_rustup(&self, ctx: &ScanCtx, seen: &mut InodeSet, out: &mut ScanOutput) {
        let rh = self.dir(&self.env.rustup_home, ctx.home, ".rustup");
        let listed = match self.rustup_list(ctx) {
            Ok(Some((_, listed))) => listed,
            Ok(None) => {
                // Toolchains without rustup: shown, never offered.
                for name in subdirs(&rh.join("toolchains")) {
                    let path = rh.join("toolchains").join(name);
                    if ctx.protector.is_protected(&path) {
                        continue;
                    }
                    if let Some(e) = item(&path, RUSTUP_MISSING, None, false, ctx, seen) {
                        out.report_only.push(e);
                    }
                }
                return;
            }
            Err(reason) => {
                out.skipped.push(skip("rustup", reason));
                return;
            }
        };
        let overrides = rustup_overrides(&rh);
        for t in &listed {
            if self
                .rustup_kept(&t.name, &listed, &overrides, ctx)
                .is_some()
            {
                continue;
            }
            let path = rh.join("toolchains").join(&t.name);
            // A linked toolchain is a symlink: nothing of ours to free.
            let real_dir = std::fs::symlink_metadata(&path).is_ok_and(|m| m.is_dir());
            if !real_dir || ctx.protector.is_protected(&path) {
                continue;
            }
            let Ok(m) = measure(&path, seen) else {
                continue;
            };
            out.entries.push(Entry {
                id: String::new(),
                category: ID.into(),
                title: format!("rustup toolchain {}", t.name),
                kind: EntryKind::Object {
                    obj: Object::RustupToolchain {
                        name: t.name.clone(),
                        path,
                    },
                },
                bytes: m.bytes,
                reason: "not the default, the newest stable or pinned by a project".into(),
                selected: false,
                permanent: false,
                note: Some("rustup downloads it again if a project asks for it".into()),
                last_active: Some(m.newest_mtime),
            });
        }
    }

    fn apply_rustup(&self, name: &str, path: &Path, ctx: &ScanCtx) -> Outcome {
        let skipped = |reason: String| Outcome::Skipped { reason };
        let (prog, listed) = match self.rustup_list(ctx) {
            Ok(Some(x)) => x,
            Ok(None) => return skipped("rustup is not installed".into()),
            Err(e) => return skipped(e),
        };
        if !listed.iter().any(|t| t.name == name) {
            return skipped("no longer installed".into());
        }
        // The plan's RUSTUP_HOME, whatever this process was started with.
        let rh = path.parent().and_then(Path::parent).unwrap_or(path);
        if let Some(why) = self.rustup_kept(name, &listed, &rustup_overrides(rh), ctx) {
            return skipped(format!("{why} now"));
        }
        if ctx.protector.is_protected(path) {
            return skipped("protected".into());
        }
        let bytes = measure(path, &mut InodeSet::default()).map_or(0, |m| m.bytes);
        removal(
            ctx,
            &prog,
            &["toolchain", "uninstall", name],
            "rustup toolchain uninstall",
            bytes,
        )
    }
}

// ---------------------------------------------------------------- nvm, pyenv

#[derive(Clone, Copy, PartialEq)]
enum Tool {
    Nvm,
    Pyenv,
}

/// `X.Y.Z`, all digits.
fn semver(s: &str) -> Option<(u64, u64, u64)> {
    let mut it = s.split('.').map(|p| p.parse::<u64>().ok());
    let v = (it.next()??, it.next()??, it.next()??);
    it.next().is_none().then_some(v)
}

impl Tool {
    /// Where the version dirs live under the tool's root.
    fn versions(self, root: &Path) -> PathBuf {
        match self {
            Tool::Nvm => root.join("versions/node"),
            Tool::Pyenv => root.join("versions"),
        }
    }

    /// The version number of an offerable dir: `vX.Y.Z` (nvm) or `X.Y.Z`
    /// (pyenv). Anything else (pre-releases, other flavours) stays.
    fn version(self, name: &str) -> Option<(u64, u64, u64)> {
        match self {
            Tool::Nvm => semver(name.strip_prefix('v')?),
            Tool::Pyenv => semver(name),
        }
    }

    /// The dir a version spec selects, as the tool picks it: an exact name,
    /// else the newest `SPEC.*` (nvm's `18`, pyenv's `3.11`).
    fn resolve(self, spec: &str, names: &[String]) -> Option<String> {
        let spec = spec.trim();
        let bare = |n: &str| match self {
            Tool::Nvm => n.strip_prefix('v').unwrap_or(n).to_string(),
            Tool::Pyenv => n.to_string(),
        };
        let spec = bare(spec);
        if let Some(n) = names.iter().find(|n| bare(n) == spec) {
            return Some(n.clone());
        }
        names
            .iter()
            .filter(|n| {
                bare(n)
                    .strip_prefix(&spec)
                    .is_some_and(|r| r.starts_with('.'))
            })
            .filter_map(|n| Some((self.version(n)?, n)))
            .max()
            .map(|(_, n)| n.clone())
    }

    fn reason(self) -> &'static str {
        match self {
            Tool::Nvm => "older Node version: not nvm's default or the newest",
            Tool::Pyenv => {
                "older Python version: not pyenv's global, the newest or in a .python-version"
            }
        }
    }

    fn note(self) -> &'static str {
        match self {
            Tool::Nvm => "global npm packages installed with it go too",
            Tool::Pyenv => "packages installed in it go too; venvs made from it stop working",
        }
    }
}

/// nvm's `alias/default` followed through other aliases (`lts/*` →
/// `lts/iron` → `v20.11.0`) to a version spec. `Ok(None)`: no default, or
/// one meaning the newest (which is kept anyway). `Err`: it can't be resolved.
fn nvm_default(root: &Path) -> Result<Option<String>, String> {
    let Some(first) = read_small(&root.join("alias/default")) else {
        return Ok(None);
    };
    let first = first.trim().to_string();
    let mut alias = first.clone();
    for _ in 0..10 {
        let bare = alias.strip_prefix('v').unwrap_or(&alias);
        if bare.starts_with(|c: char| c.is_ascii_digit()) {
            return Ok(Some(bare.to_string()));
        }
        if matches!(alias.as_str(), "node" | "stable" | "system") {
            return Ok(None);
        }
        let next = (!alias.is_empty() && !alias.contains(".."))
            .then(|| read_small(&root.join("alias").join(&alias)))
            .flatten();
        match next {
            Some(n) => alias = n.trim().to_string(),
            None => break,
        }
    }
    Err(format!("can't resolve nvm's default alias {first:?}"))
}

impl Toolchains {
    /// Every version dir that must stay, with why. `Err` when the default
    /// can't be told, so nothing is offered.
    fn kept_versions(
        &self,
        tool: Tool,
        root: &Path,
        names: &[String],
        ctx: &ScanCtx,
    ) -> Result<BTreeMap<String, &'static str>, String> {
        let mut keep = BTreeMap::new();
        if let Some(n) = names
            .iter()
            .filter_map(|n| Some((tool.version(n)?, n)))
            .max()
            .map(|(_, n)| n.clone())
        {
            keep.insert(n, "is the newest installed version");
        }
        let defaults = match tool {
            Tool::Nvm => nvm_default(root)?.into_iter().collect(),
            Tool::Pyenv => read_small(&root.join("version"))
                .map(|t| names_in(&t))
                .unwrap_or_default(),
        };
        let why = match tool {
            Tool::Nvm => "is nvm's default version",
            Tool::Pyenv => "is pyenv's global version",
        };
        for d in defaults {
            if let Some(n) = tool.resolve(&d, names) {
                keep.entry(n).or_insert(why);
            }
        }
        let open = names
            .iter()
            .any(|n| tool.version(n).is_some() && !keep.contains_key(n));
        if tool == Tool::Pyenv && open {
            for p in &self.pins(ctx).python {
                if let Some(n) = tool.resolve(p, names) {
                    keep.entry(n)
                        .or_insert("is pinned by a .python-version file");
                }
            }
        }
        Ok(keep)
    }

    fn scan_versions(
        &self,
        tool: Tool,
        root: &Path,
        ctx: &ScanCtx,
        seen: &mut InodeSet,
        out: &mut ScanOutput,
    ) {
        let dir = tool.versions(root);
        let names = subdirs(&dir);
        if names.is_empty() {
            return;
        }
        let keep = match self.kept_versions(tool, root, &names, ctx) {
            Ok(k) => k,
            Err(reason) => {
                out.skipped.push(skip("nvm", reason));
                return;
            }
        };
        for n in &names {
            if tool.version(n).is_none() || keep.contains_key(n) {
                continue;
            }
            let path = dir.join(n);
            if ctx.protector.is_protected(&path) {
                continue;
            }
            if has_virtualenvs(&path) {
                let title = crate::paths::tilde(&path, ctx.home);
                out.skipped
                    .push(skip(&title, "pyenv virtualenvs are built on it".into()));
                continue;
            }
            if let Some(e) = item(&path, tool.reason(), Some(tool.note()), true, ctx, seen) {
                out.entries.push(e);
            }
        }
    }

    /// Why an nvm or pyenv version dir must stay now, if it must. The tool
    /// root comes from the path, so a plan made under another `NVM_DIR` or
    /// `PYENV_ROOT` is still checked against its own root.
    fn still_kept(&self, path: &Path, ctx: &ScanCtx) -> Option<String> {
        let name = path.file_name()?.to_str()?.to_string();
        let parent = path.parent()?;
        let is = |p: &Path, n: &str| p.file_name() == Some(OsStr::new(n));
        let (tool, root) =
            if is(parent, "node") && parent.parent().is_some_and(|p| is(p, "versions")) {
                (Tool::Nvm, parent.parent()?.parent()?)
            } else if is(parent, "versions") {
                (Tool::Pyenv, parent.parent()?)
            } else {
                return None;
            };
        match self.kept_versions(tool, root, &subdirs(parent), ctx) {
            Err(e) => Some(e),
            Ok(keep) => match keep.get(&name) {
                Some(why) => Some(format!("{why} now")),
                None if has_virtualenvs(path) => {
                    Some("pyenv virtualenvs are built on it now".into())
                }
                None => None,
            },
        }
    }
}

/// A pyenv version with pyenv-virtualenv envs under it.
fn has_virtualenvs(path: &Path) -> bool {
    std::fs::read_dir(path.join("envs")).is_ok_and(|mut rd| rd.next().is_some())
}

// ---------------------------------------------------------------- conda

const CONDA_ROOTS: [&str; 3] = ["miniconda3", "anaconda3", "miniforge3"];

/// `conda` on `PATH`, else the first install root's `bin/conda`.
fn conda_program(ctx: &ScanCtx) -> Option<String> {
    if ctx.tools.which("conda").is_some() {
        return Some("conda".into());
    }
    CONDA_ROOTS
        .iter()
        .map(|r| ctx.home.join(r).join("bin/conda"))
        .find(|p| p.is_file())
        .map(|p| p.display().to_string())
}

/// The package caches that exist: each install root's `pkgs`, the one
/// beside a `conda` on `PATH`, and `~/.conda/pkgs`.
fn pkgs_dirs(ctx: &ScanCtx) -> Vec<PathBuf> {
    let mut dirs: Vec<PathBuf> = CONDA_ROOTS
        .iter()
        .map(|r| ctx.home.join(r).join("pkgs"))
        .collect();
    dirs.push(ctx.home.join(".conda/pkgs"));
    // <prefix>/bin/conda or <prefix>/condabin/conda
    if let Some(prefix) = ctx
        .tools
        .which("conda")
        .and_then(|p| Some(p.parent()?.parent()?.to_path_buf()))
    {
        let p = prefix.join("pkgs");
        if !dirs.contains(&p) {
            dirs.push(p);
        }
    }
    dirs.retain(|d| {
        std::fs::symlink_metadata(d).is_ok_and(|m| m.is_dir()) && !ctx.protector.is_protected(d)
    });
    dirs
}

/// Downloaded package files directly in a pkgs dir, sorted.
fn tarballs(dir: &Path) -> Vec<PathBuf> {
    let Ok(rd) = std::fs::read_dir(dir) else {
        return Vec::new();
    };
    let mut v: Vec<PathBuf> = rd
        .flatten()
        .filter(|e| e.file_type().is_ok_and(|t| t.is_file()))
        .map(|e| e.path())
        .filter(|p| {
            let n = p.as_os_str().as_encoded_bytes();
            n.ends_with(b".tar.bz2") || n.ends_with(b".conda")
        })
        .collect();
    v.sort();
    v
}

fn scan_conda(ctx: &ScanCtx, seen: &mut InodeSet, out: &mut ScanOutput) {
    let dirs = pkgs_dirs(ctx);
    if dirs.is_empty() {
        return;
    }
    if conda_program(ctx).is_some() {
        let bytes = dirs
            .iter()
            .flat_map(|d| tarballs(d))
            .filter_map(|t| measure(&t, seen).ok())
            .map(|m| m.bytes)
            .sum();
        out.entries.push(Entry {
            id: String::new(),
            category: ID.into(),
            title: CONDA_TITLE.into(),
            kind: EntryKind::Operation {
                op: Operation::CondaClean,
            },
            bytes,
            reason: "removes tarballs and extracted packages no environment uses (conda decides)"
                .into(),
            selected: true,
            permanent: false,
            note: Some("size counts tarballs only; conda frees more if packages are unused".into()),
            last_active: None,
        });
        return;
    }
    for dir in dirs {
        let mut targets = Vec::new();
        let mut bytes = 0;
        for t in tarballs(&dir) {
            if let (Ok(fp), Ok(m)) = (fingerprint(&t), measure(&t, seen)) {
                bytes += m.bytes;
                targets.push(Target { path: t, fp });
            }
        }
        if targets.is_empty() {
            continue;
        }
        out.entries.push(Entry {
            id: String::new(),
            category: ID.into(),
            title: format!("{} tarballs", crate::paths::tilde(&dir, ctx.home)),
            kind: EntryKind::Item { targets },
            bytes,
            reason: "downloaded conda packages; the extracted copies stay".into(),
            selected: true,
            permanent: false,
            note: Some("conda isn't installed, so only tarballs are offered".into()),
            last_active: None,
        });
    }
}

/// Runs `conda clean`; freed bytes are the pkgs dirs' size before minus after.
fn apply_conda(ctx: &ScanCtx) -> Outcome {
    let Some(prog) = conda_program(ctx) else {
        return Outcome::Skipped {
            reason: "conda is not installed".into(),
        };
    };
    let size = || -> u64 {
        pkgs_dirs(ctx)
            .iter()
            .filter_map(|d| measure(d, &mut InodeSet::default()).ok())
            .map(|m| m.bytes)
            .sum()
    };
    let before = size();
    match removal(ctx, &prog, &CONDA_ARGS, "conda clean", 0) {
        Outcome::Deleted { failures, .. } => Outcome::Deleted {
            bytes: before.saturating_sub(size()),
            failures,
        },
        other => other,
    }
}

// ---------------------------------------------------------------- flatpak

#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
struct FRef {
    id: String,
    arch: String,
    branch: String,
}

impl FRef {
    /// `ID/ARCH/BRANCH`, with or without an `app/` or `runtime/` prefix.
    fn parse(s: &str) -> Option<FRef> {
        let s = s.trim();
        let s = s
            .strip_prefix("runtime/")
            .or_else(|| s.strip_prefix("app/"))
            .unwrap_or(s);
        let mut it = s.split('/');
        let (id, arch, branch) = (it.next()?, it.next()?, it.next()?);
        if it.next().is_some() || id.is_empty() || arch.is_empty() || branch.is_empty() {
            return None;
        }
        Some(FRef {
            id: id.into(),
            arch: arch.into(),
            branch: branch.into(),
        })
    }

    fn full(&self) -> String {
        format!("runtime/{}/{}/{}", self.id, self.arch, self.branch)
    }

    /// `flatpak pin` patterns: `[runtime/]ID[/ARCH[/BRANCH]]`, globs allowed.
    fn pinned_by(&self, pattern: &str) -> bool {
        let pattern = pattern.trim();
        if pattern.is_empty() || pattern.starts_with("app/") {
            return false;
        }
        let pattern = pattern.strip_prefix("runtime/").unwrap_or(pattern);
        let parts = [&self.id, &self.arch, &self.branch];
        pattern.split('/').zip(parts).all(|(p, part)| {
            p.is_empty() || glob::Pattern::new(p).map_or(p == part.as_str(), |g| g.matches(part))
        })
    }
}

/// `(ref, rest)` per tab-separated line whose first column is a ref.
fn rows(stdout: &str) -> Vec<(FRef, String)> {
    stdout
        .lines()
        .filter_map(|l| {
            let (r, rest) = l.split_once('\t').unwrap_or((l, ""));
            Some((FRef::parse(r)?, rest.trim().to_string()))
        })
        .collect()
}

/// The user installation's runtimes with sizes, and which of them nothing
/// needs. `Ok(None)`: flatpak is not installed. `Err`: what's in use can't
/// be told, so nothing is offered.
type FlatpakState = (Vec<(FRef, u64)>, BTreeSet<FRef>);

fn flatpak_state(ctx: &ScanCtx) -> Result<Option<FlatpakState>, String> {
    let query = |args: &[&str]| -> Result<Option<String>, String> {
        match run(ctx, "flatpak", args, QUERY_TIMEOUT) {
            Ok(o) if o.ok() => Ok(Some(o.stdout)),
            Ok(o) => Err(format!(
                "flatpak {} failed: {}",
                args[..2].join(" "),
                first_line(&o.stderr)
            )),
            Err(ToolError::Missing(_)) => Ok(None),
            Err(e) => Err(e.to_string()),
        }
    };
    let Some(text) = query(&FLATPAK_RUNTIMES)? else {
        return Ok(None);
    };
    let runtimes: Vec<(FRef, u64)> = rows(&text)
        .into_iter()
        .map(|(r, size)| (r, parse_human_size(&size).unwrap_or(0)))
        .collect();
    if runtimes.is_empty() {
        return Ok(Some((runtimes, BTreeSet::new())));
    }
    // Apps of every installation: a system app may use a user runtime.
    let apps = rows(&query(&FLATPAK_APPS)?.ok_or("flatpak is not installed")?);
    // No `pin` command (flatpak < 1.9) means no pins.
    let pins: Vec<String> = query(&FLATPAK_PINS)
        .ok()
        .flatten()
        .map(|t| t.lines().map(|l| l.trim().to_string()).collect())
        .unwrap_or_default();

    let app_ids: Vec<&str> = apps.iter().map(|(a, _)| a.id.as_str()).collect();
    let used: BTreeSet<FRef> = apps.iter().filter_map(|(_, r)| FRef::parse(r)).collect();
    let installed: BTreeSet<&FRef> = runtimes.iter().map(|(r, _)| r).chain(&used).collect();
    let unused = runtimes
        .iter()
        .map(|(r, _)| r)
        .filter(|r| !in_use(r, &used, &installed, &app_ids, &pins))
        .cloned()
        .collect();
    Ok(Some((runtimes, unused)))
}

/// Whether a runtime is needed: an app runs on it, it is pinned, it extends
/// an app (`APP.Plugin…`), or it extends an installed runtime (`BASE.GL…`)
/// that is needed at the same branch. An extension whose branch no base
/// shares (GL drivers, codecs) is version-independent and kept.
fn in_use(
    r: &FRef,
    used: &BTreeSet<FRef>,
    installed: &BTreeSet<&FRef>,
    app_ids: &[&str],
    pins: &[String],
) -> bool {
    if used.contains(r) || pins.iter().any(|p| r.pinned_by(p)) {
        return true;
    }
    let extends = |base: &str| {
        r.id.strip_prefix(base)
            .is_some_and(|rest| rest.starts_with('.'))
    };
    if app_ids.iter().any(|a| extends(a)) {
        return true;
    }
    let bases: Vec<&FRef> = installed
        .iter()
        .copied()
        .filter(|b| b.arch == r.arch && extends(&b.id))
        .collect();
    if bases.is_empty() {
        return false;
    }
    let same_branch: Vec<&FRef> = bases.into_iter().filter(|b| b.branch == r.branch).collect();
    // A base's id is shorter than `r`'s, so this recursion ends.
    same_branch.is_empty()
        || same_branch
            .iter()
            .any(|b| in_use(b, used, installed, app_ids, pins))
}

fn scan_flatpak(ctx: &ScanCtx, out: &mut ScanOutput) {
    let (runtimes, unused) = match flatpak_state(ctx) {
        Ok(Some(s)) => s,
        Ok(None) => return,
        Err(reason) => {
            out.skipped.push(skip("flatpak", reason));
            return;
        }
    };
    for (r, bytes) in runtimes {
        if !unused.contains(&r) {
            continue;
        }
        out.entries.push(Entry {
            id: String::new(),
            category: ID.into(),
            title: format!("flatpak {}//{} ({})", r.id, r.branch, r.arch),
            kind: EntryKind::Object {
                obj: Object::Flatpak {
                    flatpak_ref: r.full(),
                },
            },
            bytes,
            reason: "runtime no installed app uses".into(),
            selected: true,
            permanent: false,
            note: None,
            last_active: None,
        });
    }
}

fn apply_flatpak(flatpak_ref: &str, bytes: u64, ctx: &ScanCtx) -> Outcome {
    let skipped = |reason: &str| Outcome::Skipped {
        reason: reason.into(),
    };
    let Some(want) = FRef::parse(flatpak_ref) else {
        return skipped("not a flatpak ref");
    };
    let (runtimes, unused) = match flatpak_state(ctx) {
        Ok(Some(s)) => s,
        Ok(None) => return skipped("flatpak is not installed"),
        Err(reason) => return skipped(&reason),
    };
    if !runtimes.iter().any(|(r, _)| *r == want) {
        return skipped("no longer installed");
    }
    if !unused.contains(&want) {
        return skipped("in use now");
    }
    removal(
        ctx,
        "flatpak",
        &["uninstall", "--user", "--noninteractive", &want.full()],
        "flatpak uninstall",
        bytes,
    )
}
