//! Browser and Electron app caches (spec §5.2d): one store-rule row per app
//! dir, deleting only the named cache dirs, and never while the app runs.

use std::path::{Path, PathBuf};

use crate::category::store::{StoreRule, Unit, apply_rule, evaluate_as, foreign_skip};
use crate::category::{Category, Outcome, ScanCtx, ScanOutput, apply_default};
use crate::config::AppCfg;
use crate::fsutil::measure::InodeSet;
use crate::plan::{Entry, EntryKind};
use crate::procs::app_running;

pub struct AppCaches {
    /// Where running processes are read from (`/proc`; a temp dir in tests).
    pub proc_root: PathBuf,
}

const ID: &str = "app_caches";

/// Cache dirs shown as their own row: (dir name, what it is).
const OWN_ROW: &[(&str, &str)] = &[("OptGuideOnDeviceModel", "on-device AI model")];

impl Category for AppCaches {
    fn id(&self) -> &'static str {
        ID
    }

    fn scan(&self, ctx: &ScanCtx, seen: &mut InodeSet) -> anyhow::Result<ScanOutput> {
        let cfg = &ctx.config.app_caches;
        let mut out = ScanOutput::default();
        if !cfg.enabled {
            return Ok(out);
        }
        for app in &cfg.apps {
            let running = app_running(app, ctx.home, &self.proc_root);
            for root in app.dirs.iter().flat_map(|d| crate::paths::expand(d, ctx.home)) {
                let note = running.then(|| {
                    format!("{} is running now; will be skipped if still running when you clean", app.name)
                });
                let main: Vec<String> =
                    cfg.cache_dirs.iter().filter(|n| !OWN_ROW.iter().any(|(o, _)| o == n)).cloned().collect();
                let mut rules = vec![(main, None)];
                for (name, label) in OWN_ROW {
                    if cfg.cache_dirs.iter().any(|n| n == name) {
                        rules.push((vec![name.to_string()], Some(*label)));
                    }
                }
                // At most one foreign-owner Skip per root, whichever rule finds it.
                let mut skipped = false;
                for (names, label) in rules {
                    let rule = StoreRule {
                        root: root.clone(),
                        max_age_days: 0,
                        unit: Unit::DirsNamed(names),
                        glob: None,
                        min_bytes: None,
                    };
                    let found = evaluate_as(&rule, ctx.now, ctx.protector, seen, ctx.euid);
                    if !skipped && let Some(s) = foreign_skip(ID, &rule, &found) {
                        out.skipped.push(s);
                        skipped = true;
                    }
                    let matches = found.matches;
                    if matches.is_empty() {
                        continue;
                    }
                    let (title, reason) = match label {
                        None => (
                            format!("{}: {}", app.name, tilde(&rule.root, ctx.home)),
                            format!("{} cache dirs", matches.len()),
                        ),
                        Some(l) => (
                            format!("{} {l}", app.name),
                            format!("re-downloads while {}'s on-device AI features are on", app.name),
                        ),
                    };
                    out.entries.push(Entry {
                        id: String::new(),
                        category: ID.into(),
                        title,
                        bytes: matches.iter().map(|m| m.bytes).sum(),
                        reason,
                        kind: EntryKind::StoreRule { rule },
                        selected: true,
                        permanent: false,
                        note: note.clone(),
                        last_active: None,
                    });
                }
            }
        }
        Ok(out)
    }

    fn apply(&self, entry: &Entry, ctx: &ScanCtx) -> Outcome {
        let EntryKind::StoreRule { rule } = &entry.kind else {
            return apply_default(entry, ctx);
        };
        // Without its app we can't tell whether the cache is in use: fail safe.
        let Some(app) = owner(&ctx.config.app_caches.apps, &rule.root, ctx.home) else {
            return Outcome::Skipped { reason: "no configured app owns this cache dir now".into() };
        };
        if app_running(app, ctx.home, &self.proc_root) {
            return Outcome::Skipped { reason: format!("{} is running", app.name) };
        }
        apply_rule(rule, ctx)
    }
}

/// The app one of whose `dirs` expands to `root`.
fn owner<'a>(apps: &'a [AppCfg], root: &Path, home: &Path) -> Option<&'a AppCfg> {
    apps.iter()
        .find(|a| a.dirs.iter().any(|d| crate::paths::expand(d, home).iter().any(|p| p == root)))
}

/// `path` with a leading `home` shown as `~`.
fn tilde(path: &Path, home: &Path) -> String {
    match path.strip_prefix(home) {
        Ok(rel) if rel.as_os_str().is_empty() => "~".into(),
        Ok(rel) => format!("~/{}", rel.display()),
        Err(_) => path.display().to_string(),
    }
}
