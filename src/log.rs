//! The run log (spec §5.1 step 5): one JSON line per applied entry, then one
//! `RunLine` per run that deleted something.

use anyhow::Context;
use serde::{Deserialize, Serialize};
use std::io::{BufRead, Write};
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

/// The measured result of one run: filesystem free space after minus before
/// (statvfs), per filesystem. Written after the run's entry lines. It lacks
/// `LogLine`'s fields, so readers that only know `LogLine` skip it.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct RunLine {
    /// Always "run".
    pub kind: String,
    pub time: i64,
    pub run_id: String,
    pub freed_by_fs: Vec<FsFreed>,
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct FsFreed {
    /// The filesystem's label (`FsUsage.label`).
    pub fs: String,
    /// Free bytes after minus before; negative if something else wrote meanwhile.
    pub bytes: i64,
}

/// One parsed log line.
#[derive(Clone, Debug, PartialEq)]
pub enum Record {
    Entry(LogLine),
    Run(RunLine),
}

/// Appends `lines`, then `run` if given, to `path` (creating it and its parent
/// dirs) in one write.
pub fn append(path: &Path, lines: &[LogLine], run: Option<&RunLine>) -> anyhow::Result<()> {
    let ctx = || format!("writing {}", path.display());
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).with_context(ctx)?;
    }
    let mut buf = Vec::new();
    for l in lines {
        serde_json::to_writer(&mut buf, l)?;
        buf.push(b'\n');
    }
    if let Some(r) = run {
        serde_json::to_writer(&mut buf, r)?;
        buf.push(b'\n');
    }
    let mut f = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
        .with_context(ctx)?;
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

/// One line (trailing newline allowed) as a `Record`; `None` if it is neither
/// an entry line nor a run line (torn, corrupt or from a future version).
pub fn parse_record(line: &[u8]) -> Option<Record> {
    if let Ok(l) = serde_json::from_slice::<LogLine>(line) {
        return Some(Record::Entry(l));
    }
    serde_json::from_slice::<RunLine>(line)
        .ok()
        .filter(|r| r.kind == "run")
        .map(Record::Run)
}

/// Streams `path`'s records, oldest first, one line in memory at a time. A
/// missing log yields nothing; unparseable lines are skipped; a read error
/// is yielded and ends the stream.
pub fn records(path: &Path) -> anyhow::Result<Records<std::io::BufReader<std::fs::File>>> {
    let reader = match std::fs::File::open(path) {
        Ok(f) => Some(std::io::BufReader::new(f)),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => None,
        Err(e) => return Err(e).with_context(|| format!("reading {}", path.display())),
    };
    Ok(Records {
        reader,
        buf: Vec::new(),
    })
}

pub struct Records<R> {
    reader: Option<R>,
    buf: Vec<u8>,
}

impl<R: BufRead> Iterator for Records<R> {
    type Item = std::io::Result<Record>;

    fn next(&mut self) -> Option<Self::Item> {
        let reader = self.reader.as_mut()?;
        loop {
            self.buf.clear();
            // Bytes, not `lines()`: a line torn into invalid UTF-8 is skipped
            // like any other unparseable line instead of ending the stream.
            match reader.read_until(b'\n', &mut self.buf) {
                Ok(0) => {
                    self.reader = None;
                    return None;
                }
                Ok(_) => {
                    if let Some(r) = parse_record(&self.buf) {
                        return Some(Ok(r));
                    }
                }
                Err(e) => {
                    self.reader = None;
                    return Some(Err(e));
                }
            }
        }
    }
}
