//! File locations (spec §7.3) and `~`/glob expansion.

use anyhow::{Context, bail};
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Paths {
    pub home: PathBuf,
    pub config_file: PathBuf,
    pub state_dir: PathBuf,
    pub plans_dir: PathBuf,
    pub log_file: PathBuf,
    pub lock_file: PathBuf,
    pub usage_index: PathBuf,
    pub space_state: PathBuf,
    /// `$XDG_CONFIG_HOME/systemd/user`: the timer's `.service` and `.timer`.
    pub systemd_user_dir: PathBuf,
    /// `$XDG_DATA_HOME/applications`: the `.desktop` file.
    pub applications_dir: PathBuf,
}

/// An XDG base-dir variable counts only when set, non-empty and absolute
/// (XDG Base Directory spec: relative values are invalid and ignored).
fn xdg_var(name: &str) -> Option<PathBuf> {
    let v = PathBuf::from(std::env::var_os(name)?);
    v.is_absolute().then_some(v)
}

impl Paths {
    /// Reads HOME, XDG_CONFIG_HOME, XDG_DATA_HOME, XDG_STATE_HOME and XDG_RUNTIME_DIR.
    pub fn from_env() -> anyhow::Result<Paths> {
        let home = std::env::var_os("HOME").context("HOME is not set")?;
        let home = PathBuf::from(home);
        if !home.is_absolute() {
            bail!("HOME is not an absolute path: {}", home.display());
        }
        let config_home = xdg_var("XDG_CONFIG_HOME").unwrap_or_else(|| home.join(".config"));
        let data_home = xdg_var("XDG_DATA_HOME").unwrap_or_else(|| home.join(".local/share"));
        let state_home = xdg_var("XDG_STATE_HOME").unwrap_or_else(|| home.join(".local/state"));
        let state_dir = state_home.join("cleaner");
        let run = xdg_var("XDG_RUNTIME_DIR");
        let lock_file = run.as_ref().map_or_else(|| state_dir.join("cleaner.lock"), |r| r.join("cleaner.lock"));
        let space_state =
            run.as_ref().map_or_else(|| state_dir.join("cleaner-space.json"), |r| r.join("cleaner-space.json"));
        Ok(Paths {
            config_file: config_home.join("cleaner/config.toml"),
            systemd_user_dir: config_home.join("systemd/user"),
            applications_dir: data_home.join("applications"),
            plans_dir: state_dir.join("plans"),
            log_file: state_dir.join("log.jsonl"),
            lock_file,
            usage_index: state_dir.join("usage.json"),
            space_state,
            state_dir,
            home,
        })
    }

    /// XDG defaults under `home`, with the lock in the state dir.
    pub fn for_home(home: &Path) -> Paths {
        let state_dir = home.join(".local/state/cleaner");
        Paths {
            home: home.to_path_buf(),
            config_file: home.join(".config/cleaner/config.toml"),
            plans_dir: state_dir.join("plans"),
            log_file: state_dir.join("log.jsonl"),
            lock_file: state_dir.join("cleaner.lock"),
            usage_index: state_dir.join("usage.json"),
            space_state: state_dir.join("cleaner-space.json"),
            systemd_user_dir: home.join(".config/systemd/user"),
            applications_dir: home.join(".local/share/applications"),
            state_dir,
        }
    }
}

/// Expands a leading `~` or `~/` to `home`, then glob characters.
/// Returns only paths that exist (symlinks included, not followed), sorted.
/// An invalid pattern yields nothing.
pub fn expand(pattern: &str, home: &Path) -> Vec<PathBuf> {
    let rest = if pattern == "~" {
        Some("")
    } else {
        pattern.strip_prefix("~/")
    };

    // Literal pattern: no glob parsing, so any home path (even non-UTF-8) works.
    if !pattern.contains(['*', '?', '[']) {
        let p = match rest {
            Some(r) => home.join(r),
            None => PathBuf::from(pattern),
        };
        return if p.symlink_metadata().is_ok() { vec![p] } else { Vec::new() };
    }

    let full = match rest {
        Some(r) => {
            let Some(h) = home.to_str() else {
                return Vec::new();
            };
            let h = glob::Pattern::escape(h);
            if r.is_empty() { h } else { format!("{}/{r}", h.trim_end_matches('/')) }
        }
        None => pattern.to_string(),
    };
    let Ok(paths) = glob::glob(&full) else {
        return Vec::new();
    };
    let mut out: Vec<PathBuf> = paths.filter_map(Result::ok).collect();
    out.sort();
    out
}

/// `expand()` each root, then drop any path lexically inside another one;
/// sorted and deduplicated.
pub fn expand_roots(roots: &[String], home: &Path) -> Vec<PathBuf> {
    let mut all: Vec<PathBuf> = roots.iter().flat_map(|r| expand(r, home)).collect();
    // Re-collecting components drops the trailing `/` that `expand("~")` leaves.
    all = all.into_iter().map(|p| p.components().collect()).collect();
    all.sort();
    all.dedup();
    all.iter().filter(|p| !all.iter().any(|q| q != *p && p.starts_with(q))).cloned().collect()
}
