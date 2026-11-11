//! Low-space decision and its state file (spec §5.1 low-space check). This
//! module only decides; it never plans or deletes anything.

use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::Path;

use crate::disk::FsUsage;

/// A filesystem re-arms once it is this many percentage points above the threshold.
pub const REARM_MARGIN: u8 = 5;
/// A still-low filesystem is announced again after this long.
pub const RENOTIFY_SECS: i64 = 6 * 3600;

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Default)]
pub struct SpaceState {
    /// Keyed by `FsUsage.label`.
    pub fs: BTreeMap<String, FsState>,
}

/// A missing entry means armed and never notified.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct FsState {
    pub armed: bool,
    pub notified_at: i64,
}

/// free% = avail*100/total (total 0 is ignored). Low = free% < `percent`.
/// A low filesystem that is armed, or was last announced `RENOTIFY_SECS` ago,
/// is returned and marked announced. A non-low one at or above
/// `percent + REARM_MARGIN` is re-armed. Anything else is unchanged.
pub fn decide(usage: &[FsUsage], state: &SpaceState, percent: u8, now: i64) -> (Vec<FsUsage>, SpaceState) {
    let mut next = state.clone();
    let mut low = Vec::new();
    for f in usage.iter().filter(|f| f.total > 0) {
        let free = f.avail as f64 * 100.0 / f.total as f64;
        let cur = state.fs.get(&f.label).cloned().unwrap_or(FsState { armed: true, notified_at: 0 });
        if free < f64::from(percent) {
            if cur.armed || now - cur.notified_at >= RENOTIFY_SECS {
                low.push(f.clone());
                next.fs.insert(f.label.clone(), FsState { armed: false, notified_at: now });
            }
        } else if free >= f64::from(percent) + f64::from(REARM_MARGIN) && !cur.armed {
            next.fs.insert(f.label.clone(), FsState { armed: true, notified_at: cur.notified_at });
        }
    }
    (low, next)
}

/// Missing or corrupt → the default.
pub fn load_state(path: &Path) -> SpaceState {
    std::fs::read(path).ok().and_then(|b| serde_json::from_slice(&b).ok()).unwrap_or_default()
}

pub fn save_state(path: &Path, s: &SpaceState) -> anyhow::Result<()> {
    use anyhow::Context;
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).with_context(|| format!("creating {}", parent.display()))?;
    }
    std::fs::write(path, serde_json::to_vec(s)?).with_context(|| format!("writing {}", path.display()))
}

/// "disk full" if any cause is `ErrorKind::StorageFull` (errno 28), else `{e:#}`.
pub fn save_error_reason(e: &anyhow::Error) -> String {
    let full = e
        .chain()
        .filter_map(|c| c.downcast_ref::<std::io::Error>())
        .any(|io| io.kind() == std::io::ErrorKind::StorageFull);
    if full { "disk full".to_string() } else { format!("{e:#}") }
}
