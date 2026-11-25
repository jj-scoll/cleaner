//! Rules over a store (spec §5.1a): planned as one row per store root, and
//! re-evaluated at apply time, deleting whatever matches the rule *now*.

use anyhow::{Context, anyhow, bail};
use serde::{Deserialize, Serialize};
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};

use crate::category::{Outcome, ScanCtx, ScanOutput};
use crate::config::StoreCfg;
use crate::fsutil::delete::delete_tree;
use crate::fsutil::measure::{InodeSet, measure, measure_as};
use crate::plan::{Entry, EntryKind, Skip};
use crate::protect::Protector;

/// Which entries under a store root are judged as one unit.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub enum Unit {
    Files {
        recursive: bool,
    },
    Depth(u8),
    NameContains(String),
    /// Any dir below the root (to depth `DIRS_NAMED_MAX_DEPTH`) whose name
    /// is in the list; a match is not descended. Not user-configurable, so no
    /// `FromStr` form.
    DirsNamed(Vec<String>),
}

const DIRS_NAMED_MAX_DEPTH: usize = 6;

/// Never descended by `DirsNamed`: Chrome's installed extension packages,
/// which may ship their own `Cache` dir; deleting it corrupts the extension.
const DIRS_NAMED_NEVER: &str = "Extensions";

impl std::str::FromStr for Unit {
    type Err = anyhow::Error;

    /// `files` | `files-recursive` | `depth:N` (N ≥ 1) | `name-contains:S` (S non-empty).
    fn from_str(s: &str) -> anyhow::Result<Unit> {
        if s == "files" {
            return Ok(Unit::Files { recursive: false });
        }
        if s == "files-recursive" {
            return Ok(Unit::Files { recursive: true });
        }
        if let Some(n) = s.strip_prefix("depth:") {
            let n: u8 = n.parse().with_context(|| format!("invalid unit {s:?}"))?;
            if n == 0 {
                // depth:0 would make the store root itself the unit.
                bail!("invalid unit {s:?}: depth must be at least 1");
            }
            return Ok(Unit::Depth(n));
        }
        if let Some(text) = s.strip_prefix("name-contains:") {
            if text.is_empty() {
                bail!("invalid unit {s:?}: name-contains needs text");
            }
            return Ok(Unit::NameContains(text.into()));
        }
        Err(anyhow!(
            "invalid unit {s:?} (expected files, files-recursive, depth:N or name-contains:TEXT)"
        ))
    }
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct StoreRule {
    #[serde(with = "crate::plan::path_codec")]
    pub root: PathBuf,
    pub max_age_days: u64,
    pub unit: Unit,
    pub glob: Option<String>,
    pub min_bytes: Option<u64>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Match {
    pub path: PathBuf,
    pub bytes: u64,
    pub newest_mtime: i64,
}

/// What `evaluate_as` found: the matching units, and the units that would
/// match but hold files owned by another uid (with the first such uid).
#[derive(Debug, Default)]
pub struct Evaluated {
    pub matches: Vec<Match>,
    pub foreign: Vec<(PathBuf, u32)>,
}

/// Why a store root with foreign-owned units is not offered.
pub fn foreign_reason(uid: u32) -> String {
    let who = if uid == 0 {
        "root".to_string()
    } else {
        format!("uid {uid}")
    };
    format!("contains files owned by {who}; cleaner never runs as root")
}

/// The `Skip` for a rule root with foreign-owned units, if `found` has any;
/// the root is shown under `~` (`home`).
pub fn foreign_skip(
    category: &str,
    rule: &StoreRule,
    found: &Evaluated,
    home: &Path,
) -> Option<Skip> {
    let (_, uid) = found.foreign.first()?;
    Some(Skip {
        category: category.into(),
        title: crate::paths::tilde(&rule.root, home),
        reason: foreign_reason(*uid),
    })
}

/// The units of `rule` that match at the current euid; see [`evaluate_as`].
pub fn evaluate(rule: &StoreRule, now: i64, prot: &Protector, seen: &mut InodeSet) -> Vec<Match> {
    evaluate_as(rule, now, prot, seen, rustix::process::geteuid().as_raw()).matches
}

/// The units of `rule` that match at `now`. A unit matches when it is not a
/// symlink, not protected, fully readable, its newest mtime is older than
/// `max_age_days`, its name matches `glob` (if set), its size is at least
/// `min_bytes` (if set), nothing in it is owned by a uid other than `euid`,
/// and nothing in it is git-tracked. A unit that passes all but the
/// ownership test goes to `foreign`. `Match::bytes` is deduplicated against `seen`.
///
/// A store root that is missing, not a directory, or a symlink has no units.
pub fn evaluate_as(
    rule: &StoreRule,
    now: i64,
    prot: &Protector,
    seen: &mut InodeSet,
    euid: u32,
) -> Evaluated {
    let Ok(root_md) = std::fs::symlink_metadata(&rule.root) else {
        return Evaluated::default();
    };
    if !root_md.is_dir() {
        return Evaluated::default();
    }
    let glob = match &rule.glob {
        Some(g) => match glob::Pattern::new(g) {
            Ok(p) => Some(p),
            Err(_) => return Evaluated::default(), // an invalid pattern matches nothing
        },
        None => None,
    };
    let cutoff = now.saturating_sub(rule.max_age_days.saturating_mul(86_400) as i64);

    let mut out = Evaluated::default();
    for unit in units(&rule.root, root_md.dev(), &rule.unit, prot) {
        match judge(&unit, glob.as_ref(), cutoff, rule.min_bytes, prot, euid) {
            // Before the counting measure: a foreign unit never claims inodes in `seen`.
            Judged::Foreign(uid) => out.foreign.push((unit, uid)),
            Judged::Matches { newest_mtime } => {
                let Ok(counted) = measure(&unit, seen) else {
                    continue;
                };
                out.matches.push(Match {
                    path: unit,
                    bytes: counted.bytes,
                    newest_mtime,
                });
            }
            _ => {}
        }
    }
    out
}

/// What [`evaluate_as`] decides about one unit, in the order it checks.
#[derive(Debug, Clone, PartialEq)]
pub enum Judged {
    Protected,
    GlobMismatch,
    Unmeasurable,
    /// Has unreadable parts, which could hide something new.
    Unreadable,
    TooNew {
        newest_mtime: i64,
    },
    TooSmall {
        bytes: u64,
    },
    /// Holds a file owned by this uid (the first one found).
    Foreign(u32),
    Tracked,
    Matches {
        newest_mtime: i64,
    },
}

/// The per-unit tests of [`evaluate_as`]. Measures on a private set, so a
/// unit that doesn't match leaves the plan's `seen` untouched.
fn judge(
    unit: &Path,
    glob: Option<&glob::Pattern>,
    cutoff: i64,
    min_bytes: Option<u64>,
    prot: &Protector,
    euid: u32,
) -> Judged {
    if prot.is_protected(unit) {
        return Judged::Protected;
    }
    if let Some(p) = glob {
        let name = unit.file_name().unwrap_or_default().to_string_lossy();
        if !p.matches(&name) {
            return Judged::GlobMismatch;
        }
    }
    let Ok(m) = measure_as(unit, &mut InodeSet::default(), euid) else {
        return Judged::Unmeasurable;
    };
    if m.errors > 0 {
        return Judged::Unreadable; // fail safe
    }
    if m.newest_mtime >= cutoff {
        return Judged::TooNew {
            newest_mtime: m.newest_mtime,
        };
    }
    if min_bytes.is_some_and(|min| m.bytes < min) {
        return Judged::TooSmall { bytes: m.bytes };
    }
    // Before the git check.
    if m.foreign > 0 {
        return Judged::Foreign(m.foreign_uid.unwrap_or(0));
    }
    // §6.3, checked last: it spawns git for units inside a repo.
    if crate::git::repo_root(unit).is_some_and(|repo| crate::git::is_tracked(&repo, unit)) {
        return Judged::Tracked;
    }
    Judged::Matches {
        newest_mtime: m.newest_mtime,
    }
}

/// `rule` in words, e.g. "files directly under ~/Downloads, older than 90 days".
pub fn describe(rule: &StoreRule, home: &Path) -> String {
    let root = crate::paths::tilde(&rule.root, home);
    let unit = match &rule.unit {
        Unit::Files { recursive: false } => format!("files directly under {root}"),
        Unit::Files { recursive: true } => format!("files at any depth under {root}"),
        Unit::Depth(n) => format!("entries {n} level(s) below {root}"),
        Unit::NameContains(s) => format!("dirs under {root} whose name contains {s:?}"),
        Unit::DirsNamed(names) => format!("dirs named {} under {root}", names.join(", ")),
    };
    let mut out = match rule.max_age_days {
        0 => format!("{unit}, any age"),
        n => format!("{unit}, older than {}", crate::fmt::plural(n, "day")),
    };
    if let Some(g) = &rule.glob {
        out.push_str(&format!(", named {g}"));
    }
    if let Some(min) = rule.min_bytes {
        out.push_str(&format!(", at least {}", crate::fmt::human_bytes(min)));
    }
    out
}

/// What `rule` says about `path`, or `None` when `path` is not at or under
/// its root: the store root itself, a unit, or something inside a unit is
/// judged with the same tests [`evaluate_as`] uses. Read-only.
pub fn explain_rule(rule: &StoreRule, path: &Path, ctx: &ScanCtx) -> Option<crate::category::Why> {
    use crate::category::Why;
    use crate::fmt::plural;
    if !path.starts_with(&rule.root) {
        return None;
    }
    let root_md = std::fs::symlink_metadata(&rule.root).ok()?;
    if !root_md.is_dir() {
        return None;
    }
    let what = describe(rule, ctx.home);
    if path == rule.root {
        let found = evaluate_as(
            rule,
            ctx.now,
            ctx.protector,
            &mut InodeSet::default(),
            ctx.euid,
        );
        return Some(if !found.matches.is_empty() {
            Why::Match(format!(
                "store root of the rule \"{what}\"; {} matching now",
                plural(found.matches.len(), "unit")
            ))
        } else if let Some((_, uid)) = found.foreign.first() {
            Why::NearMiss(foreign_reason(*uid))
        } else {
            Why::NearMiss(format!(
                "store root of the rule \"{what}\", but nothing in it matches now"
            ))
        });
    }
    let units = units(&rule.root, root_md.dev(), &rule.unit, ctx.protector);
    let Some(unit) = units.into_iter().find(|u| path.starts_with(u)) else {
        return Some(Why::NearMiss(format!(
            "inside {} but not one of the rule's units ({what})",
            crate::paths::tilde(&rule.root, ctx.home)
        )));
    };
    let part_of = if unit == path {
        String::new()
    } else {
        format!(
            " (judged as part of {})",
            crate::paths::tilde(&unit, ctx.home)
        )
    };
    let glob = rule.glob.as_ref().and_then(|g| glob::Pattern::new(g).ok());
    let cutoff = ctx
        .now
        .saturating_sub(rule.max_age_days.saturating_mul(86_400) as i64);
    let why = match judge(
        &unit,
        glob.as_ref(),
        cutoff,
        rule.min_bytes,
        ctx.protector,
        ctx.euid,
    ) {
        Judged::Protected => Why::NearMiss("protected path".into()),
        Judged::GlobMismatch => Why::NearMiss(format!("name doesn't match the rule ({what})")),
        Judged::Unmeasurable => Why::NearMiss("cannot be measured".into()),
        Judged::Unreadable => Why::NearMiss("has unreadable parts; skipped to be safe".into()),
        Judged::TooNew { newest_mtime } => Why::NotYet(format!(
            "untouched for {}; rule needs {}",
            plural((ctx.now - newest_mtime).max(0) / 86_400, "day"),
            rule.max_age_days
        )),
        Judged::TooSmall { bytes } => Why::NotYet(format!(
            "only {}; rule needs at least {}",
            crate::fmt::human_bytes(bytes),
            crate::fmt::human_bytes(rule.min_bytes.unwrap_or(0))
        )),
        Judged::Foreign(uid) => Why::NearMiss(foreign_reason(uid)),
        Judged::Tracked => Why::NearMiss("tracked in git".into()),
        Judged::Matches { .. } => Why::Match(format!("matches the rule \"{what}\"")),
    };
    Some(match why {
        Why::NearMiss(s) => Why::NearMiss(s + &part_of),
        Why::NotYet(s) => Why::NotYet(s + &part_of),
        Why::Match(s) => Why::Match(s + &part_of),
    })
}

/// [`explain_rule`] over every expanded root of `stores`; the most decisive
/// answer wins. Stores with an invalid unit say nothing.
pub fn explain_stores(
    stores: &[StoreCfg],
    path: &Path,
    ctx: &ScanCtx,
) -> Option<crate::category::Why> {
    let mut best = None;
    for cfg in stores {
        let Ok(unit) = cfg.unit.parse::<Unit>() else {
            continue;
        };
        for root in crate::paths::expand(&cfg.path, ctx.home) {
            let rule = StoreRule {
                root,
                max_age_days: cfg.max_age_days,
                unit: unit.clone(),
                glob: cfg.glob.clone(),
                min_bytes: cfg.min_bytes,
            };
            best = crate::category::Why::best(best, explain_rule(&rule, path, ctx));
        }
    }
    best
}

/// The `n` largest units matching `rule` now (for the GUI's expandable store
/// row), largest first.
pub fn top_matches(rule: &StoreRule, ctx: &ScanCtx, n: usize) -> Vec<Match> {
    let mut matches = evaluate_as(
        rule,
        ctx.now,
        ctx.protector,
        &mut InodeSet::default(),
        ctx.euid,
    )
    .matches;
    matches.sort_by_key(|m| std::cmp::Reverse(m.bytes));
    matches.truncate(n);
    matches
}

/// Candidate unit paths under `root` (never symlinks, never across a mount).
fn units(root: &Path, dev: u64, unit: &Unit, prot: &Protector) -> Vec<PathBuf> {
    let mut out = Vec::new();
    // (dir, depth of dir below root)
    let mut stack: Vec<(PathBuf, usize)> = vec![(root.to_path_buf(), 0)];
    while let Some((dir, depth)) = stack.pop() {
        let Ok(rd) = std::fs::read_dir(&dir) else {
            continue;
        };
        for entry in rd.flatten() {
            // DirEntry::metadata is an lstat.
            let Ok(md) = entry.metadata() else { continue };
            if md.file_type().is_symlink() || md.dev() != dev {
                continue; // another filesystem mounted here is never a unit
            }
            let p = entry.path();
            let descend = md.is_dir() && !prot.is_protected(&p);
            match unit {
                Unit::Files { recursive } => {
                    if md.is_file() {
                        out.push(p);
                    } else if *recursive && descend {
                        stack.push((p, depth + 1));
                    }
                }
                Unit::Depth(n) => {
                    if depth + 1 == *n as usize {
                        out.push(p);
                    } else if descend {
                        stack.push((p, depth + 1));
                    }
                }
                Unit::NameContains(s) => {
                    if !md.is_dir() {
                        continue;
                    }
                    if entry.file_name().to_string_lossy().contains(s.as_str()) {
                        out.push(p);
                    } else if descend {
                        stack.push((p, depth + 1));
                    }
                }
                Unit::DirsNamed(names) => {
                    if !md.is_dir() {
                        continue;
                    }
                    if names.iter().any(|n| entry.file_name() == n.as_str()) {
                        out.push(p);
                    } else if descend
                        && depth + 1 < DIRS_NAMED_MAX_DEPTH
                        && entry.file_name() != DIRS_NAMED_NEVER
                    {
                        stack.push((p, depth + 1));
                    }
                }
            }
        }
    }
    out.sort();
    out
}

/// Re-evaluates `rule` at `ctx.now` and deletes every match. `Skipped` if
/// nothing matches now; `Failed` if matches existed but none could be deleted.
pub fn apply_rule(rule: &StoreRule, ctx: &ScanCtx) -> Outcome {
    let matches = evaluate_as(
        rule,
        ctx.now,
        ctx.protector,
        &mut InodeSet::default(),
        ctx.euid,
    )
    .matches;
    if matches.is_empty() {
        return Outcome::Skipped {
            reason: "nothing matches the rule now".into(),
        };
    }
    if let Some(to) = ctx.hold {
        let paths: Vec<&Path> = matches.iter().map(|m| m.path.as_path()).collect();
        if let Some(reason) = crate::category::off_hold_device(&paths, to.area) {
            return Outcome::Skipped { reason };
        }
        let mut session = crate::hold::Session::new(to);
        if let Err(reason) = session.reserve(&paths) {
            return Outcome::Skipped { reason };
        }
        let mut failures = Vec::new();
        let mut first_reason = None;
        for m in &matches {
            if let Err(reason) = session.hold(&m.path, None) {
                failures.push(format!("{}: {reason}", m.path.display()));
                first_reason.get_or_insert(reason);
            }
        }
        return crate::category::held_outcome(session, failures, first_reason);
    }
    let mut deleted_any = false;
    let mut bytes = 0;
    let mut failures = Vec::new();
    for m in &matches {
        match delete_tree(&m.path) {
            Ok(freed) => {
                deleted_any = true;
                bytes += freed;
            }
            Err(e) => failures.push(format!("{}: delete failed: {e}", m.path.display())),
        }
    }
    if deleted_any {
        Outcome::Deleted { bytes, failures }
    } else {
        Outcome::Failed {
            error: failures.swap_remove(0),
        }
    }
}

/// One `StoreRule` entry per expanded root of each config store that has at
/// least one match. `reason(n, days)` words the entry's reason. A store with
/// an invalid unit becomes a `Skip`. Roots for which `skip_root` is true are
/// not scanned (another entry already covers them).
pub fn scan_stores(
    category: &str,
    stores: &[StoreCfg],
    ctx: &ScanCtx,
    seen: &mut InodeSet,
    reason: &dyn Fn(usize, u64) -> String,
    skip_root: &dyn Fn(&Path) -> bool,
) -> ScanOutput {
    let mut out = ScanOutput::default();
    for cfg in stores {
        let unit: Unit = match cfg.unit.parse() {
            Ok(u) => u,
            Err(e) => {
                out.skipped.push(Skip {
                    category: category.into(),
                    title: cfg.path.clone(),
                    reason: format!("{e:#}"),
                });
                continue;
            }
        };
        for root in crate::paths::expand(&cfg.path, ctx.home) {
            if skip_root(&root) {
                continue;
            }
            let rule = StoreRule {
                root,
                max_age_days: cfg.max_age_days,
                unit: unit.clone(),
                glob: cfg.glob.clone(),
                min_bytes: cfg.min_bytes,
            };
            let found = evaluate_as(&rule, ctx.now, ctx.protector, seen, ctx.euid);
            out.skipped
                .extend(foreign_skip(category, &rule, &found, ctx.home));
            let matches = found.matches;
            if matches.is_empty() {
                continue;
            }
            out.entries.push(Entry {
                id: String::new(),
                category: category.into(),
                title: crate::paths::tilde(&rule.root, ctx.home),
                bytes: matches.iter().map(|m| m.bytes).sum(),
                reason: reason(matches.len(), cfg.max_age_days),
                kind: EntryKind::StoreRule { rule },
                selected: true,
                permanent: false,
                note: None,
                last_active: None,
            });
        }
    }
    out
}
