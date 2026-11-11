//! The run log (spec §5.1 step 5): one JSON line per applied entry.

use anyhow::Context;
use serde::{Deserialize, Serialize};
use std::io::Write;
use std::path::Path;

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct LogLine {
    pub time: i64,
    pub run_id: String,
    pub plan_id: String,
    pub entry_id: String,
    pub category: String,
    pub title: String,
    /// "deleted" | "skipped" | "failed"
    pub outcome: String,
    pub bytes: u64,
    pub detail: String,
}

/// Appends `lines` to `path` (creating it and its parent dirs) in one write.
pub fn append(path: &Path, lines: &[LogLine]) -> anyhow::Result<()> {
    let ctx = || format!("writing {}", path.display());
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).with_context(ctx)?;
    }
    let mut buf = Vec::new();
    for l in lines {
        serde_json::to_writer(&mut buf, l)?;
        buf.push(b'\n');
    }
    let mut f = std::fs::OpenOptions::new().create(true).append(true).open(path).with_context(ctx)?;
    f.write_all(&buf).with_context(ctx)
}

/// Lines with `time >= since` (all if `None`), oldest first. A missing log is
/// empty; unparseable lines (e.g. one torn by a crash) are skipped.
pub fn read(path: &Path, since: Option<i64>) -> anyhow::Result<Vec<LogLine>> {
    let text = match std::fs::read_to_string(path) {
        Ok(t) => t,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => return Err(e).with_context(|| format!("reading {}", path.display())),
    };
    Ok(text
        .lines()
        .filter_map(|l| serde_json::from_str::<LogLine>(l).ok())
        .filter(|l| since.is_none_or(|s| l.time >= s))
        .collect())
}
