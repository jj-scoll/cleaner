//! Editing `config.toml` from the settings page (spec §7.2, §7.3): only the
//! keys whose value changed are written, so comments, key order and unknown
//! keys survive.

use anyhow::{Context, bail};
use std::io::ErrorKind;
use std::path::{Component, Path};
use toml_edit::{ArrayOfTables, DocumentMut, Item, Table, TableLike, Value};

use crate::config::{self, Config, DEFAULT_CONFIG_TOML, KeepNewestCfg, StoreCfg};

pub struct ConfigDoc {
    doc: DocumentMut,
}

impl ConfigDoc {
    /// Parses `path`; a missing file starts from `DEFAULT_CONFIG_TOML`.
    pub fn load(path: &Path) -> anyhow::Result<ConfigDoc> {
        let ctx = || format!("reading {}", path.display());
        let text = match std::fs::read_to_string(path) {
            Ok(t) => t,
            Err(e) if e.kind() == ErrorKind::NotFound => DEFAULT_CONFIG_TOML.to_string(),
            Err(e) => return Err(e).with_context(ctx),
        };
        Ok(ConfigDoc { doc: text.parse().with_context(ctx)? })
    }

    /// The document as a `Config`, parsed like `config::load` (warnings dropped).
    pub fn config(&self) -> anyhow::Result<Config> {
        Ok(config::parse(&self.doc.to_string())?.0)
    }

    /// Writes into the document only the editable keys whose value differs from
    /// `self.config()`; comments, key order and unknown keys are kept. Calls
    /// `validate` first.
    pub fn update(&mut self, new: &Config) -> anyhow::Result<()> {
        if let Err(errors) = validate(new) {
            bail!("{}", errors.join("; "));
        }
        let (n, o) = (new, &self.config()?);
        // (table, key, value); "" is the root table.
        let mut changes: Vec<(&str, &str, Value)> = Vec::new();
        if n.notify_threshold_bytes != o.notify_threshold_bytes {
            changes.push(("", "notify_threshold_bytes", int(n.notify_threshold_bytes)?));
        }
        if n.timer_time != o.timer_time {
            changes.push(("", "timer_time", n.timer_time.as_str().into()));
        }
        if n.filesystems != o.filesystems {
            changes.push(("", "filesystems", strings(&n.filesystems)));
        }
        if n.scan_roots != o.scan_roots {
            changes.push(("", "scan_roots", strings(&n.scan_roots)));
        }
        if n.low_space.enabled != o.low_space.enabled {
            changes.push(("low_space", "enabled", n.low_space.enabled.into()));
        }
        if n.low_space.percent != o.low_space.percent {
            changes.push(("low_space", "percent", int(n.low_space.percent as u64)?));
        }
        if n.protect != o.protect {
            changes.push(("", "protect", strings(&n.protect)));
        }
        let (na, oa) = (&n.artifacts, &o.artifacts);
        if na.enabled != oa.enabled {
            changes.push(("artifacts", "enabled", na.enabled.into()));
        }
        if let (true, Some(v)) = (na.roots != oa.roots, &na.roots) {
            changes.push(("artifacts", "roots", strings(v)));
        }
        if na.max_depth != oa.max_depth {
            changes.push(("artifacts", "max_depth", int(na.max_depth as u64)?));
        }
        if na.stale_after_days != oa.stale_after_days {
            changes.push(("artifacts", "stale_after_days", int(na.stale_after_days)?));
        }
        if n.caches.enabled != o.caches.enabled {
            changes.push(("caches", "enabled", n.caches.enabled.into()));
        }
        if n.trash.enabled != o.trash.enabled {
            changes.push(("trash", "enabled", n.trash.enabled.into()));
        }
        if n.trash.max_age_days != o.trash.max_age_days {
            changes.push(("trash", "max_age_days", int(n.trash.max_age_days)?));
        }
        let (ni, oi) = (&n.idle_folders, &o.idle_folders);
        if ni.enabled != oi.enabled {
            changes.push(("idle_folders", "enabled", ni.enabled.into()));
        }
        if ni.idle_days != oi.idle_days {
            changes.push(("idle_folders", "idle_days", int(ni.idle_days)?));
        }
        if ni.min_bytes != oi.min_bytes {
            changes.push(("idle_folders", "min_bytes", int(ni.min_bytes)?));
        }
        if n.caches.uv_prune != o.caches.uv_prune {
            changes.push(("caches", "uv_prune", n.caches.uv_prune.into()));
        }
        let (nw, ow) = (&n.worktrees, &o.worktrees);
        if nw.enabled != ow.enabled {
            changes.push(("worktrees", "enabled", nw.enabled.into()));
        }
        if nw.idle_days != ow.idle_days {
            changes.push(("worktrees", "idle_days", int(nw.idle_days)?));
        }
        if n.app_caches.enabled != o.app_caches.enabled {
            changes.push(("app_caches", "enabled", n.app_caches.enabled.into()));
        }
        let (nd, od) = (&n.docker, &o.docker);
        if nd.enabled != od.enabled {
            changes.push(("docker", "enabled", nd.enabled.into()));
        }
        if nd.container_days != od.container_days {
            changes.push(("docker", "container_days", int(nd.container_days)?));
        }
        if nd.build_cache_days != od.build_cache_days {
            changes.push(("docker", "build_cache_days", int(nd.build_cache_days)?));
        }
        let (nn, on) = (&n.nix, &o.nix);
        if nn.enabled != on.enabled {
            changes.push(("nix", "enabled", nn.enabled.into()));
        }
        if nn.keep != on.keep {
            changes.push(("nix", "keep", int(nn.keep as u64)?));
        }
        if nn.devbox_idle_days != on.devbox_idle_days {
            changes.push(("nix", "devbox_idle_days", int(nn.devbox_idle_days)?));
        }
        if nn.gc != on.gc {
            changes.push(("nix", "gc", nn.gc.into()));
        }

        let root = self.doc.as_table_mut();
        for (tbl, key, v) in changes {
            if tbl.is_empty() {
                set(root, key, v);
            } else {
                set(table(root, tbl)?, key, v);
            }
        }
        if n.artifacts.roots.is_none() && o.artifacts.roots.is_some() {
            table(root, "artifacts")?.remove("roots");
        }
        let keeps = |c: &Config| c.caches.keep_newest.iter().map(|k| k.keep).collect::<Vec<_>>();
        if keeps(n) != keeps(o) {
            write_keep_newest(table(root, "caches")?, &n.caches.keep_newest)?;
        }
        if n.user_rules != o.user_rules {
            write_rules(root, &n.user_rules)?;
        }
        Ok(())
    }

    /// Temp file + rename. Refuses a read-only (e.g. Nix-managed) config.
    pub fn save(&self, path: &Path) -> anyhow::Result<()> {
        if config::is_read_only(path) {
            bail!("config is read-only: {}", path.display());
        }
        crate::timer::write_atomic(path, &self.doc.to_string())
    }
}

/// Every problem with `c` that would make saving it a mistake.
pub fn validate(c: &Config) -> Result<(), Vec<String>> {
    let mut errors = Vec::new();
    if let Err(e) = crate::timer::parse_time(&c.timer_time) {
        errors.push(e.to_string());
    }
    if c.scan_roots.is_empty() {
        errors.push("add at least one scan root".into());
    }
    for r in &c.scan_roots {
        if !(r.starts_with('~') || r.starts_with('/')) {
            errors.push(format!("scan root {r:?} must start with ~ or /"));
        }
    }
    if !(1..=50).contains(&c.low_space.percent) {
        errors.push("low-space percent must be between 1 and 50".into());
    }
    if let Err(e) = crate::timer::parse_interval(&c.low_space.interval) {
        errors.push(format!("low-space interval: {e}"));
    }
    if c.artifacts.stale_after_days < 1 {
        errors.push("stale_after_days must be at least 1".into());
    }
    for k in &c.caches.keep_newest {
        if k.keep < 1 {
            errors.push(format!("keep must be at least 1 ({})", k.path));
        }
    }
    if c.nix.keep < 1 {
        errors.push("nix keep must be at least 1".into());
    }
    if c.worktrees.idle_days < 1 {
        errors.push("worktree idle_days must be at least 1".into());
    }
    for r in &c.user_rules {
        if !(r.path.starts_with("~/") || r.path.starts_with('/')) {
            errors.push(format!("folder rule path must start with ~/ or / ({})", r.path));
        }
        let rest = Path::new(r.path.strip_prefix('~').unwrap_or(&r.path));
        if rest.components().any(|c| c == Component::ParentDir) {
            errors.push(format!("folder rule path can't contain .. ({})", r.path));
        } else if !rest.components().any(|c| matches!(c, Component::Normal(_))) {
            errors.push(format!("folder rule can't be the whole home or root ({})", r.path));
        }
        if r.max_age_days < 1 {
            errors.push(format!("folder rule age must be at least 1 day ({})", r.path));
        }
        if let Some(Err(e)) = r.glob.as_deref().map(glob::Pattern::new) {
            errors.push(format!("bad glob in folder rule {}: {e}", r.path));
        }
    }
    if errors.is_empty() { Ok(()) } else { Err(errors) }
}

fn int(n: u64) -> anyhow::Result<Value> {
    Ok(i64::try_from(n).with_context(|| format!("{n} is too large for config.toml"))?.into())
}

fn strings(v: &[String]) -> Value {
    Value::Array(v.iter().map(String::as_str).collect())
}

/// Sets `key`, keeping its position, the key's comments and the old value's
/// surrounding whitespace and trailing comment.
fn set(t: &mut dyn TableLike, key: &str, mut v: Value) {
    match t.get_mut(key) {
        Some(Item::Value(old)) => {
            *v.decor_mut() = old.decor().clone();
            *old = v;
        }
        Some(item) => *item = Item::Value(v),
        None => {
            t.insert(key, Item::Value(v));
        }
    }
}

/// The table `name` under the root, created (at the end) if missing.
fn table<'a>(root: &'a mut Table, name: &str) -> anyhow::Result<&'a mut dyn TableLike> {
    root.entry(name)
        .or_insert_with(|| Item::Table(Table::new()))
        .as_table_like_mut()
        .with_context(|| format!("config key {name} is not a table"))
}

/// Only `keep` is editable: update it in place when the document already has
/// the same number of `[[caches.keep_newest]]` tables, else write them all.
fn write_keep_newest(caches: &mut dyn TableLike, keeps: &[KeepNewestCfg]) -> anyhow::Result<()> {
    let in_place = caches
        .get("keep_newest")
        .and_then(Item::as_array_of_tables)
        .is_some_and(|aot| aot.len() == keeps.len());
    if in_place {
        let aot = caches.get_mut("keep_newest").and_then(Item::as_array_of_tables_mut).expect("checked above");
        for (t, k) in aot.iter_mut().zip(keeps) {
            set(t, "keep", int(k.keep as u64)?);
        }
    } else {
        let mut aot = ArrayOfTables::new();
        for k in keeps {
            let mut t = Table::new();
            t.insert("path", toml_edit::value(k.path.as_str()));
            t.insert("keep", Item::Value(int(k.keep as u64)?));
            aot.push(t);
        }
        caches.insert("keep_newest", Item::ArrayOfTables(aot));
    }
    Ok(())
}

/// The whole `[[user_rules]]` array. Existing tables are updated in place
/// (keeping their comments and unknown keys); extra ones are added or dropped.
fn write_rules(root: &mut Table, rules: &[StoreCfg]) -> anyhow::Result<()> {
    let fill = |t: &mut Table, r: &StoreCfg| -> anyhow::Result<()> {
        set(t, "path", r.path.as_str().into());
        set(t, "max_age_days", int(r.max_age_days)?);
        set(t, "unit", r.unit.as_str().into());
        match &r.glob {
            Some(g) => set(t, "glob", g.as_str().into()),
            None => drop(t.remove("glob")),
        }
        match r.min_bytes {
            Some(b) => set(t, "min_bytes", int(b)?),
            None => drop(t.remove("min_bytes")),
        }
        Ok(())
    };
    // An empty [[user_rules]] prints as nothing, and a missing key loads as
    // the default ~/Downloads rule; write `user_rules = []` so "none" sticks.
    if rules.is_empty() {
        root.insert("user_rules", toml_edit::value(toml_edit::Array::new()));
        return Ok(());
    }
    if let Some(aot) = root.get_mut("user_rules").and_then(Item::as_array_of_tables_mut) {
        while aot.len() > rules.len() {
            aot.remove(aot.len() - 1);
        }
        for (i, r) in rules.iter().enumerate() {
            match aot.get_mut(i) {
                Some(t) => fill(t, r)?,
                None => {
                    let mut t = Table::new();
                    fill(&mut t, r)?;
                    aot.push(t);
                }
            }
        }
    } else {
        let mut aot = ArrayOfTables::new();
        for r in rules {
            let mut t = Table::new();
            fill(&mut t, r)?;
            aot.push(t);
        }
        root.insert("user_rules", Item::ArrayOfTables(aot));
    }
    Ok(())
}
