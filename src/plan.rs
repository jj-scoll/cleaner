//! The plan file (spec §5.1, §5.1a): candidate entries with fingerprints,
//! skipped items, and report-only rows. Saved as JSON under the plans dir.

use anyhow::{Context, bail};
use serde::{Deserialize, Serialize};
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};

use crate::fsutil::measure::{InodeSet, measure};

/// How many plan files `Plan::save` keeps.
const KEEP_PLANS: usize = 10;

/// What a discrete item looked like at plan time; apply re-checks it (§6.4).
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct Fingerprint {
    pub dev: u64,
    pub ino: u64,
    pub newest_mtime: i64,
}

/// lstat `(dev, ino)` of `path` plus the newest mtime at or under it.
/// A symlink is `Err(InvalidInput)` (from `measure`).
pub fn fingerprint(path: &Path) -> std::io::Result<Fingerprint> {
    let md = std::fs::symlink_metadata(path)?;
    let m = measure(path, &mut InodeSet::default())?;
    Ok(Fingerprint { dev: md.dev(), ino: md.ino(), newest_mtime: m.newest_mtime })
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct Target {
    #[serde(with = "path_codec")]
    pub path: PathBuf,
    pub fp: Fingerprint,
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub enum EntryKind {
    /// Discrete: approval means exactly these paths.
    Item { targets: Vec<Target> },
    /// A rule over a store, re-evaluated at apply time (§5.1a).
    StoreRule { rule: crate::category::store::StoreRule },
    /// Discrete things deleted by exact id through their owning tool.
    Object { obj: crate::object::Object },
    /// A tool command; the tool decides what goes (§5.1a).
    Operation { op: crate::object::Operation },
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct Entry {
    /// Empty from categories; `engine::build_plan` assigns it.
    pub id: String,
    pub category: String,
    pub title: String,
    pub kind: EntryKind,
    pub bytes: u64,
    pub reason: String,
    pub selected: bool,
    pub permanent: bool,
    pub note: Option<String>,
    /// Unix time the thing was last used, when the category knows better
    /// than its folders' mtimes (a project's last edit or commit). The Age
    /// sort uses it; plans saved before it existed load as `None`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_active: Option<i64>,
}

impl Entry {
    /// Whether `bytes` is a plan-time estimate rather than an exact size:
    /// store rules, tool operations, and Nix closure sizes (DECISION D5).
    pub fn is_estimate(&self) -> bool {
        matches!(self.kind, EntryKind::StoreRule { .. } | EntryKind::Operation { .. }) || self.is_closure_estimate()
    }

    /// A Nix row sized by store closure: generations, or a link that pins
    /// one. Deleting it frees nothing until the store is garbage collected.
    pub fn is_closure_estimate(&self) -> bool {
        use crate::object::Object;
        match &self.kind {
            EntryKind::Object { obj: Object::NixGenerations { .. } } => true,
            EntryKind::Object { obj: Object::Symlink { .. } } => self.bytes > 0,
            _ => false,
        }
    }
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct Skip {
    pub category: String,
    pub title: String,
    pub reason: String,
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct Plan {
    pub id: String,
    pub created: i64,
    pub entries: Vec<Entry>,
    pub skipped: Vec<Skip>,
    pub report_only: Vec<Entry>,
}

impl Plan {
    /// UTC timestamp id, e.g. `20261110T030000Z`; sorts chronologically.
    pub fn new_id(now: i64) -> String {
        jiff::Timestamp::from_second(now)
            .unwrap_or_default()
            .strftime("%Y%m%dT%H%M%SZ")
            .to_string()
    }

    pub fn selected_bytes(&self) -> u64 {
        self.entries.iter().filter(|e| e.selected).map(|e| e.bytes).sum()
    }

    /// Writes `<plans_dir>/<id>.json` (via a temp file and rename), then
    /// removes all but the newest 10 plan files.
    pub fn save(&self, plans_dir: &Path) -> anyhow::Result<PathBuf> {
        std::fs::create_dir_all(plans_dir)
            .with_context(|| format!("creating {}", plans_dir.display()))?;
        let path = plans_dir.join(format!("{}.json", self.id));
        let tmp = plans_dir.join(format!(".{}.json.tmp", self.id));
        let json = serde_json::to_vec_pretty(self)?;
        std::fs::write(&tmp, json).with_context(|| format!("writing {}", tmp.display()))?;
        std::fs::rename(&tmp, &path).with_context(|| format!("writing {}", path.display()))?;
        let plans = plan_files(plans_dir)?;
        let excess = plans.len().saturating_sub(KEEP_PLANS);
        for old in &plans[..excess] {
            std::fs::remove_file(old).with_context(|| format!("pruning {}", old.display()))?;
        }
        Ok(path)
    }

    pub fn load(path: &Path) -> anyhow::Result<Plan> {
        let bytes = std::fs::read(path).with_context(|| format!("reading {}", path.display()))?;
        serde_json::from_slice(&bytes).with_context(|| format!("parsing plan {}", path.display()))
    }

    /// The newest plan file, or `None` if there are none (or no plans dir).
    pub fn latest(plans_dir: &Path) -> anyhow::Result<Option<PathBuf>> {
        if !plans_dir.exists() {
            return Ok(None);
        }
        Ok(plan_files(plans_dir)?.pop())
    }

    /// `<plans_dir>/<id>.json`; a trailing `.json` on `id` is ignored. An
    /// empty id or one containing `/` is refused.
    pub fn path_for(plans_dir: &Path, id: &str) -> anyhow::Result<PathBuf> {
        let id = id.strip_suffix(".json").unwrap_or(id);
        if id.is_empty() || id.contains('/') {
            bail!("not a plan id: {id}");
        }
        Ok(plans_dir.join(format!("{id}.json")))
    }

    /// Records `id` as the plan `cleaner show` last printed; a no-id apply
    /// only uses the newest plan if it is that one.
    pub fn mark_shown(state_dir: &Path, id: &str) -> anyhow::Result<()> {
        let shown = state_dir.join(LAST_SHOWN);
        std::fs::write(&shown, id).with_context(|| format!("writing {}", shown.display()))
    }

    /// The id `mark_shown` last recorded, or `None` if there is none.
    pub fn last_shown(state_dir: &Path) -> Option<String> {
        let text = std::fs::read_to_string(state_dir.join(LAST_SHOWN)).ok()?;
        Some(text.trim().to_string())
    }
}

const LAST_SHOWN: &str = "last-shown-plan";

/// The plan `cleaner gui [ID]` should open.
pub enum Requested {
    /// No id given: show the latest plan as usual.
    Latest,
    Plan(PathBuf, Plan),
    /// The id is invalid, its plan is gone (e.g. pruned) or unreadable.
    Problem(String),
}

pub fn resolve_requested(plans_dir: &Path, id: Option<&str>) -> Requested {
    let Some(id) = id else { return Requested::Latest };
    let loaded = Plan::path_for(plans_dir, id).and_then(|path| Ok((Plan::load(&path)?, path)));
    match loaded {
        Ok((plan, path)) => Requested::Plan(path, plan),
        Err(e) => Requested::Problem(format!("can't open plan {id}: {e:#}")),
    }
}

/// `*.json` files in `plans_dir`, oldest first (ids sort chronologically).
fn plan_files(plans_dir: &Path) -> anyhow::Result<Vec<PathBuf>> {
    let mut files = Vec::new();
    let rd = std::fs::read_dir(plans_dir)
        .with_context(|| format!("listing {}", plans_dir.display()))?;
    for entry in rd {
        let entry = entry?;
        let name = entry.file_name();
        let name = name.as_encoded_bytes();
        if name.ends_with(b".json") && !name.starts_with(b".") && entry.file_type()?.is_file() {
            files.push(entry.path());
        }
    }
    files.sort();
    Ok(files)
}

/// `PathBuf` ⇄ JSON: a string when the path is valid UTF-8, otherwise
/// `{"bytes": [u8, ...]}`, so any Unix path round-trips exactly.
pub mod path_codec {
    use serde::{Deserialize, Deserializer, Serialize, Serializer};
    use std::ffi::OsString;
    use std::os::unix::ffi::{OsStrExt, OsStringExt};
    use std::path::{Path, PathBuf};

    #[derive(Serialize)]
    #[serde(untagged)]
    enum Out<'a> {
        Str(&'a str),
        Bytes { bytes: &'a [u8] },
    }

    #[derive(Deserialize)]
    #[serde(untagged)]
    enum In {
        Str(String),
        Bytes { bytes: Vec<u8> },
    }

    pub fn serialize<S: Serializer>(path: &Path, s: S) -> Result<S::Ok, S::Error> {
        match path.to_str() {
            Some(text) => Out::Str(text),
            None => Out::Bytes { bytes: path.as_os_str().as_bytes() },
        }
        .serialize(s)
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(d: D) -> Result<PathBuf, D::Error> {
        Ok(match In::deserialize(d)? {
            In::Str(text) => PathBuf::from(text),
            In::Bytes { bytes } => PathBuf::from(OsString::from_vec(bytes)),
        })
    }
}
