//! `config.toml`: defaults, loading with unknown-key warnings, and the
//! commented default file written on first run (spec §7.3).

use anyhow::Context;
use serde::{Deserialize, Serialize};
use std::fs;
use std::collections::BTreeMap;
use std::io::{ErrorKind, Write};
use std::path::Path;

#[derive(Deserialize, Serialize, Clone, Debug, PartialEq)]
#[serde(default)]
pub struct Config {
    pub notify_threshold_bytes: u64,
    pub timer_time: String,
    pub filesystems: Vec<String>,
    /// Where categories look for projects, worktrees, result links and large old files.
    pub scan_roots: Vec<String>,
    pub protect: Vec<String>,
    pub artifacts: ArtifactsCfg,
    pub caches: CachesCfg,
    pub user_rules: Vec<StoreCfg>,
    pub trash: TrashCfg,
    pub idle_folders: IdleCfg,
    pub worktrees: WorktreesCfg,
    pub app_caches: AppCachesCfg,
    pub docker: DockerCfg,
    pub nix: NixCfg,
    pub low_space: LowSpaceCfg,
    pub disk_map: DiskMapCfg,
}

#[derive(Deserialize, Serialize, Clone, Debug, PartialEq)]
#[serde(default)]
pub struct LowSpaceCfg {
    pub enabled: bool,
    pub percent: u8,
    pub interval: String,
}

#[derive(Deserialize, Serialize, Clone, Debug, PartialEq)]
#[serde(default)]
pub struct DiskMapCfg {
    pub roots: Vec<String>,
}

impl Config {
    /// `own` when set, else `scan_roots`.
    pub fn roots_for<'a>(&'a self, own: &'a Option<Vec<String>>) -> &'a [String] {
        own.as_deref().unwrap_or(&self.scan_roots)
    }

    /// Ids among "artifacts", "worktrees", "nix" whose roots are set, in that order.
    pub fn root_overrides(&self) -> Vec<&'static str> {
        [
            ("artifacts", self.artifacts.roots.is_some()),
            ("worktrees", self.worktrees.roots.is_some()),
            ("nix", self.nix.roots.is_some()),
        ]
        .into_iter()
        .filter_map(|(id, set)| set.then_some(id))
        .collect()
    }
}

#[derive(Deserialize, Serialize, Clone, Debug, PartialEq)]
#[serde(default)]
pub struct ArtifactsCfg {
    pub enabled: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub roots: Option<Vec<String>>,
    pub max_depth: usize,
    pub stale_after_days: u64,
    /// Extra `target@marker` rules, e.g. `"out@build.zig"`.
    pub extra_rules: Vec<String>,
}

#[derive(Deserialize, Serialize, Clone, Debug, PartialEq)]
#[serde(default)]
pub struct CachesCfg {
    pub enabled: bool,
    pub stores: Vec<StoreCfg>,
    pub keep_newest: Vec<KeepNewestCfg>,
    /// Offer `uv cache prune` as a tool operation.
    pub uv_prune: bool,
    /// Offer `go clean -modcache` as an unticked tool operation.
    pub go_clean: bool,
}

#[derive(Deserialize, Serialize, Clone, Debug, PartialEq)]
pub struct StoreCfg {
    pub path: String,
    pub max_age_days: u64,
    pub unit: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub glob: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub min_bytes: Option<u64>,
}

#[derive(Deserialize, Serialize, Clone, Debug, PartialEq)]
pub struct KeepNewestCfg {
    pub path: String,
    pub keep: usize,
}

#[derive(Deserialize, Serialize, Clone, Debug, PartialEq)]
#[serde(default)]
pub struct TrashCfg {
    pub enabled: bool,
    pub max_age_days: u64,
}

#[derive(Deserialize, Serialize, Clone, Debug, PartialEq)]
#[serde(default)]
pub struct IdleCfg {
    pub enabled: bool,
    pub min_bytes: u64,
    pub idle_days: u64,
}

#[derive(Deserialize, Serialize, Clone, Debug, PartialEq)]
#[serde(default)]
pub struct WorktreesCfg {
    pub enabled: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub roots: Option<Vec<String>>,
    pub max_depth: usize,
    pub idle_days: u64,
}

#[derive(Deserialize, Serialize, Clone, Debug, PartialEq)]
#[serde(default)]
pub struct AppCachesCfg {
    pub enabled: bool,
    /// Cache dir names looked for under each app dir.
    pub cache_dirs: Vec<String>,
    pub apps: Vec<AppCfg>,
}

/// One browser or Electron app: where its caches live and how to tell it's running.
#[derive(Deserialize, Serialize, Clone, Debug, PartialEq)]
pub struct AppCfg {
    pub name: String,
    #[serde(default)]
    pub dirs: Vec<String>,
    /// Chromium-style `SingletonLock` symlinks, or Firefox's `lock` (globs allowed).
    #[serde(default)]
    pub locks: Vec<String>,
    /// Executable file names, matched against `/proc/PID/exe` or argv[0]'s
    /// file name. `comm` is never read: it is truncated to 15 bytes.
    #[serde(default)]
    pub exe: Vec<String>,
}

#[derive(Deserialize, Serialize, Clone, Debug, PartialEq)]
#[serde(default)]
pub struct DockerCfg {
    pub enabled: bool,
    pub container_days: u64,
    pub build_cache_days: u64,
}

#[derive(Deserialize, Serialize, Clone, Debug, PartialEq)]
#[serde(default)]
pub struct NixCfg {
    pub enabled: bool,
    /// Generations kept per profile (by count only), current one included.
    pub keep: usize,
    /// Per-profile overrides of `keep`, by profile name.
    pub keep_profile: BTreeMap<String, usize>,
    pub profiles_dir: String,
    /// The system profile; empty turns system generations off.
    pub system_profile: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub roots: Option<Vec<String>>,
    pub max_depth: usize,
    pub devbox_idle_days: u64,
    pub gc: bool,
}

fn store(path: &str, max_age_days: u64, unit: &str) -> StoreCfg {
    StoreCfg {
        path: path.into(),
        max_age_days,
        unit: unit.into(),
        glob: None,
        min_bytes: None,
    }
}

/// The tool-cache stores from spec §5.2c.
pub fn default_stores() -> Vec<StoreCfg> {
    let mut v = vec![
        store("~/.cache/terragrunt*/providers", 30, "depth:4"), // <host>/<ns>/<name>/<ver>
        store("~/.cache/tofu-plugins*", 30, "depth:4"),
        store("~/.cache/regul8-verify/runs/*/tg-cache", 3, "depth:1"),
        store("~/.cargo/registry/src/*", 90, "depth:1"),
        store("~/.cargo/registry/cache/*", 90, "files"),
        store("~/.cargo/git/checkouts", 90, "depth:1"),
        store("~/.cache/go-build", 30, "files-recursive"),
        store("~/go/pkg/mod", 90, "name-contains:@"),
        store("~/.npm/_cacache", 30, "files-recursive"),
        store("~/.npm/_npx", 30, "depth:1"),
        store("~/.bun/install/cache", 30, "depth:1"),
        store("~/.cache/pip", 30, "files-recursive"),
        store("~/.cache/pypoetry", 30, "files-recursive"),
        store("~/.local/share/pnpm/store", 30, "files-recursive"),
        store("~/.cache/ms-playwright*", 90, "depth:1"),
        store("~/.cache/thumbnails", 90, "files-recursive"),
    ];
    for name in ["vscode-cpptools", "nix", "appimage-run", "restic", "Cypress", "puppeteer", "nvidia"] {
        v.push(store(&format!("~/.cache/{name}"), 30, "depth:1"));
    }
    v
}

fn app(name: &str, dirs: &[&str], locks: &[&str], exe: &[&str]) -> AppCfg {
    let v = |s: &[&str]| s.iter().map(|x| x.to_string()).collect();
    AppCfg { name: name.into(), dirs: v(dirs), locks: v(locks), exe: v(exe) }
}

/// The browsers and Electron apps from spec §5.2d. Never put `claude` in an
/// `exe` list: that's Claude Code, not Claude Desktop.
pub fn default_apps() -> Vec<AppCfg> {
    vec![
        app(
            "Chrome",
            &["~/.config/google-chrome", "~/.cache/google-chrome"],
            &["~/.config/google-chrome/SingletonLock"],
            &["chrome", "google-chrome", "google-chrome-stable"],
        ),
        app(
            "Chromium",
            &["~/.config/chromium", "~/.cache/chromium"],
            &["~/.config/chromium/SingletonLock"],
            &["chromium", "chromium-browser"],
        ),
        app("Firefox", &["~/.cache/mozilla"], &["~/.mozilla/firefox/*/lock"], &["firefox", "firefox-bin"]),
        app("Slack", &["~/.config/Slack"], &["~/.config/Slack/SingletonLock"], &["slack"]),
        app(
            "Teams",
            &["~/.config/teams-for-linux"],
            &["~/.config/teams-for-linux/SingletonLock"],
            &["teams-for-linux"],
        ),
        app("Claude", &["~/.config/Claude"], &["~/.config/Claude/SingletonLock"], &["claude-desktop"]),
        app("Cursor", &["~/.config/Cursor"], &["~/.config/Cursor/SingletonLock"], &["cursor"]),
        app("Discord", &["~/.config/discord"], &["~/.config/discord/SingletonLock"], &["discord", "Discord"]),
    ]
}

impl Default for Config {
    fn default() -> Self {
        Config {
            notify_threshold_bytes: 1 << 30,
            timer_time: "03:00".into(),
            filesystems: vec!["~".into(), "/".into()],
            scan_roots: vec!["~".into()],
            protect: Vec::new(),
            artifacts: ArtifactsCfg::default(),
            caches: CachesCfg::default(),
            user_rules: vec![store("~/Downloads", 90, "files")],
            trash: TrashCfg::default(),
            idle_folders: IdleCfg::default(),
            worktrees: WorktreesCfg::default(),
            app_caches: AppCachesCfg::default(),
            docker: DockerCfg::default(),
            nix: NixCfg::default(),
            low_space: LowSpaceCfg::default(),
            disk_map: DiskMapCfg::default(),
        }
    }
}

impl Default for LowSpaceCfg {
    fn default() -> Self {
        LowSpaceCfg { enabled: true, percent: 10, interval: "10min".into() }
    }
}

impl Default for DiskMapCfg {
    fn default() -> Self {
        DiskMapCfg { roots: vec!["~".into()] }
    }
}

impl Default for ArtifactsCfg {
    fn default() -> Self {
        ArtifactsCfg {
            enabled: true,
            roots: None,
            max_depth: 6,
            stale_after_days: 30,
            extra_rules: Vec::new(),
        }
    }
}

impl Default for CachesCfg {
    fn default() -> Self {
        CachesCfg {
            enabled: true,
            stores: default_stores(),
            keep_newest: vec![KeepNewestCfg {
                path: "~/.cache/regul8-verify/runs".into(),
                keep: 1,
            }],
            uv_prune: true,
            go_clean: true,
        }
    }
}

impl Default for TrashCfg {
    fn default() -> Self {
        TrashCfg {
            enabled: true,
            max_age_days: 30,
        }
    }
}

impl Default for IdleCfg {
    fn default() -> Self {
        IdleCfg {
            enabled: true,
            min_bytes: 1 << 30,
            idle_days: 90,
        }
    }
}

impl Default for WorktreesCfg {
    fn default() -> Self {
        WorktreesCfg { enabled: true, roots: None, max_depth: 6, idle_days: 3 }
    }
}

impl Default for AppCachesCfg {
    fn default() -> Self {
        AppCachesCfg {
            enabled: true,
            cache_dirs: ["Cache", "Code Cache", "GPUCache", "CacheStorage", "OptGuideOnDeviceModel", "cache2"]
                .map(String::from)
                .into(),
            apps: default_apps(),
        }
    }
}

impl Default for DockerCfg {
    fn default() -> Self {
        DockerCfg { enabled: true, container_days: 7, build_cache_days: 7 }
    }
}

impl Default for NixCfg {
    fn default() -> Self {
        NixCfg {
            enabled: true,
            keep: 5,
            keep_profile: BTreeMap::new(),
            profiles_dir: "~/.local/state/nix/profiles".into(),
            system_profile: "/nix/var/nix/profiles/system".into(),
            roots: None,
            max_depth: 6,
            devbox_idle_days: 30,
            gc: true,
        }
    }
}

/// Loads `path`. A missing file means defaults. Unknown keys become warnings
/// (`"unknown config key: a.b"`); a syntax or type error is an `Err` naming the file.
pub fn load(path: &Path) -> anyhow::Result<(Config, Vec<String>)> {
    let ctx = || format!("reading {}", path.display());
    let text = match fs::read_to_string(path) {
        Ok(t) => t,
        Err(e) if e.kind() == ErrorKind::NotFound => return Ok((Config::default(), Vec::new())),
        Err(e) => return Err(e).with_context(ctx),
    };
    parse(&text).with_context(ctx)
}

/// Parses config text with the same rules as `load`.
pub fn parse(text: &str) -> anyhow::Result<(Config, Vec<String>)> {
    let de = toml::Deserializer::parse(text)?;
    let mut warnings = Vec::new();
    let cfg = serde_ignored::deserialize(de, |p| {
        warnings.push(format!("unknown config key: {p}"));
    })?;
    Ok((cfg, warnings))
}

/// Writes `DEFAULT_CONFIG_TOML` (creating parent dirs) unless `path` already
/// exists, including as a symlink.
pub fn write_default_if_missing(path: &Path) -> anyhow::Result<()> {
    let ctx = || format!("writing {}", path.display());
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).with_context(ctx)?;
    }
    match fs::OpenOptions::new().write(true).create_new(true).open(path) {
        Ok(mut f) => f.write_all(DEFAULT_CONFIG_TOML.as_bytes()).with_context(ctx),
        Err(e) if e.kind() == ErrorKind::AlreadyExists => Ok(()),
        Err(e) => Err(e).with_context(ctx),
    }
}

/// True if the config is a symlink (e.g. Nix-managed) or exists but isn't
/// writable by us. A missing file is not read-only: we can create it.
pub fn is_read_only(path: &Path) -> bool {
    match path.symlink_metadata() {
        Ok(m) if m.file_type().is_symlink() => true,
        Ok(_) => rustix::fs::access(path, rustix::fs::Access::WRITE_OK).is_err(),
        Err(e) => e.kind() != ErrorKind::NotFound,
    }
}

/// Every key, commented out, showing its default. Plain table headers are
/// left active (an empty table means defaults); array-of-table headers are
/// commented, since an active one without its keys would be invalid.
pub const DEFAULT_CONFIG_TOML: &str = r#"# cleaner configuration.
# Every setting is shown commented out with its default value.
# Uncomment a line to change it. Unknown keys are warned about, never fatal.

# Notify when a plan could free at least this many bytes.
# notify_threshold_bytes = 1073741824
# Daily time the systemd user timer runs.
# timer_time = "03:00"
# Filesystems shown in status.
# filesystems = ["~", "/"]
# Where categories look for projects, worktrees, result links and large old files.
# scan_roots = ["~"]
# Extra protected paths, merged with the built-in list.
# protect = []

[artifacts]
# enabled = true
# roots: overrides scan_roots for this category, for example roots = ["~/src"]
# max_depth = 6
# stale_after_days = 30
# Extra rules in the form target@marker, for example out@build.zig.
# extra_rules = []

[caches]
# enabled = true
# Offer uv cache prune (uv decides what goes; it skips what is in use).
# uv_prune = true
# Offer go clean -modcache (unticked: it removes the whole module cache).
# go_clean = true
#
# Tool-cache stores. Defining any [[caches.stores]] replaces the whole list,
# so uncomment all of these to keep them alongside your own.
# Each store may also set: glob (file-name pattern) and min_bytes (size floor).
# Units: files, files-recursive, depth:N, name-contains:TEXT.
# [[caches.stores]]
# path = "~/.cache/terragrunt*/providers"
# max_age_days = 30
# unit = "depth:4"
# [[caches.stores]]
# path = "~/.cache/tofu-plugins*"
# max_age_days = 30
# unit = "depth:4"
# [[caches.stores]]
# path = "~/.cache/regul8-verify/runs/*/tg-cache"
# max_age_days = 3
# unit = "depth:1"
# [[caches.stores]]
# path = "~/.cargo/registry/src/*"
# max_age_days = 90
# unit = "depth:1"
# [[caches.stores]]
# path = "~/.cargo/registry/cache/*"
# max_age_days = 90
# unit = "files"
# [[caches.stores]]
# path = "~/.cargo/git/checkouts"
# max_age_days = 90
# unit = "depth:1"
# [[caches.stores]]
# path = "~/.cache/go-build"
# max_age_days = 30
# unit = "files-recursive"
# [[caches.stores]]
# path = "~/go/pkg/mod"
# max_age_days = 90
# unit = "name-contains:@"
# [[caches.stores]]
# path = "~/.npm/_cacache"
# max_age_days = 30
# unit = "files-recursive"
# [[caches.stores]]
# path = "~/.npm/_npx"
# max_age_days = 30
# unit = "depth:1"
# [[caches.stores]]
# path = "~/.bun/install/cache"
# max_age_days = 30
# unit = "depth:1"
# [[caches.stores]]
# path = "~/.cache/pip"
# max_age_days = 30
# unit = "files-recursive"
# [[caches.stores]]
# path = "~/.cache/pypoetry"
# max_age_days = 30
# unit = "files-recursive"
# [[caches.stores]]
# path = "~/.local/share/pnpm/store"
# max_age_days = 30
# unit = "files-recursive"
# [[caches.stores]]
# path = "~/.cache/ms-playwright*"
# max_age_days = 90
# unit = "depth:1"
# [[caches.stores]]
# path = "~/.cache/thumbnails"
# max_age_days = 90
# unit = "files-recursive"
# [[caches.stores]]
# path = "~/.cache/vscode-cpptools"
# max_age_days = 30
# unit = "depth:1"
# [[caches.stores]]
# path = "~/.cache/nix"
# max_age_days = 30
# unit = "depth:1"
# [[caches.stores]]
# path = "~/.cache/appimage-run"
# max_age_days = 30
# unit = "depth:1"
# [[caches.stores]]
# path = "~/.cache/restic"
# max_age_days = 30
# unit = "depth:1"
# [[caches.stores]]
# path = "~/.cache/Cypress"
# max_age_days = 30
# unit = "depth:1"
# [[caches.stores]]
# path = "~/.cache/puppeteer"
# max_age_days = 30
# unit = "depth:1"
# [[caches.stores]]
# path = "~/.cache/nvidia"
# max_age_days = 30
# unit = "depth:1"
#
# Keep only the newest N children of a directory.
# [[caches.keep_newest]]
# path = "~/.cache/regul8-verify/runs"
# keep = 1

# User-folder rules: same fields as a cache store.
# Defining any [[user_rules]] replaces the default rule.
# [[user_rules]]
# path = "~/Downloads"
# max_age_days = 90
# unit = "files"

[trash]
# enabled = true
# max_age_days = 30

[idle_folders]
# enabled = true
# min_bytes = 1073741824
# idle_days = 90

[worktrees]
# Linked git worktrees whose branch is merged and that sat idle this long.
# enabled = true
# roots: overrides scan_roots for this category, for example roots = ["~/src"]
# max_depth = 6
# idle_days = 3

[app_caches]
# Browser and Electron app caches, skipped while the app runs.
# enabled = true
# cache_dirs = ["Cache", "Code Cache", "GPUCache", "CacheStorage", "OptGuideOnDeviceModel", "cache2"]
#
# Apps. Defining any [[app_caches.apps]] replaces the whole list, so
# uncomment all of these to keep them alongside your own. locks are
# Chromium-style SingletonLock files (or Firefox's lock); exe are process
# names. Never list claude as an exe: that is Claude Code, not Claude Desktop.
# [[app_caches.apps]]
# name = "Chrome"
# dirs = ["~/.config/google-chrome", "~/.cache/google-chrome"]
# locks = ["~/.config/google-chrome/SingletonLock"]
# exe = ["chrome", "google-chrome", "google-chrome-stable"]
# [[app_caches.apps]]
# name = "Chromium"
# dirs = ["~/.config/chromium", "~/.cache/chromium"]
# locks = ["~/.config/chromium/SingletonLock"]
# exe = ["chromium", "chromium-browser"]
# [[app_caches.apps]]
# name = "Firefox"
# dirs = ["~/.cache/mozilla"]
# locks = ["~/.mozilla/firefox/*/lock"]
# exe = ["firefox", "firefox-bin"]
# [[app_caches.apps]]
# name = "Slack"
# dirs = ["~/.config/Slack"]
# locks = ["~/.config/Slack/SingletonLock"]
# exe = ["slack"]
# [[app_caches.apps]]
# name = "Teams"
# dirs = ["~/.config/teams-for-linux"]
# locks = ["~/.config/teams-for-linux/SingletonLock"]
# exe = ["teams-for-linux"]
# [[app_caches.apps]]
# name = "Claude"
# dirs = ["~/.config/Claude"]
# locks = ["~/.config/Claude/SingletonLock"]
# exe = ["claude-desktop"]
# [[app_caches.apps]]
# name = "Cursor"
# dirs = ["~/.config/Cursor"]
# locks = ["~/.config/Cursor/SingletonLock"]
# exe = ["cursor"]
# [[app_caches.apps]]
# name = "Discord"
# dirs = ["~/.config/discord"]
# locks = ["~/.config/discord/SingletonLock"]
# exe = ["discord", "Discord"]

[docker]
# enabled = true
# Stopped containers older than this many days.
# container_days = 7
# Build cache unused for this many days.
# build_cache_days = 7

[nix]
# enabled = true
# Generations kept per profile (by count), the current one included.
# keep = 5
# Per-profile overrides, for example keep_profile = { system = 10 }.
# keep_profile = {}
# profiles_dir = "~/.local/state/nix/profiles"
# The system profile; "" turns system generations off.
# system_profile = "/nix/var/nix/profiles/system"
# Where to look for result links and devbox roots.
# roots: overrides scan_roots for this category, for example roots = ["~/src"]
# max_depth = 6
# devbox_idle_days = 30
# Offer nix-store --gc after removing roots.
# gc = true

[low_space]
# enabled = true
# percent = 10
# interval = "10min"

[disk_map]
# roots = ["~"]
"#;
