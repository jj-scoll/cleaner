//! XDG Trash expiry (spec §5.2h): `~/.local/share/Trash/{files,info}`.

use std::ffi::OsStr;
use std::os::unix::ffi::OsStrExt;
use std::path::Path;

use crate::category::{Category, ScanCtx, ScanOutput};
use crate::fsutil::measure::{InodeSet, measure};
use crate::plan::{Entry, EntryKind, Skip, Target, fingerprint};

pub struct Trash;

const ID: &str = "trash";

impl Category for Trash {
    fn id(&self) -> &'static str {
        ID
    }

    /// Each `info/<name>.trashinfo` whose `DeletionDate` is older than
    /// `max_age_days` becomes an Item of `[files/<name>, info/<name>.trashinfo]`,
    /// or of the record alone when the payload is gone.
    fn scan(&self, ctx: &ScanCtx, seen: &mut InodeSet) -> anyhow::Result<ScanOutput> {
        let mut out = ScanOutput::default();
        let cfg = &ctx.config.trash;
        if !cfg.enabled {
            return Ok(out);
        }
        let trash = ctx.home.join(".local/share/Trash");
        let (files_dir, info_dir) = (trash.join("files"), trash.join("info"));
        let Ok(rd) = std::fs::read_dir(&info_dir) else { return Ok(out) };
        let cutoff = ctx.now.saturating_sub(cfg.max_age_days.saturating_mul(86_400) as i64);

        let mut infos: Vec<_> = rd
            .flatten()
            .filter(|e| e.file_type().is_ok_and(|t| t.is_file()))
            .map(|e| e.path())
            .collect();
        infos.sort();
        for info in infos {
            let fname = info.file_name().unwrap_or_default().as_bytes();
            let Some(name) = fname.strip_suffix(b".trashinfo") else { continue };
            let payload = files_dir.join(OsStr::from_bytes(name));
            let skip = |reason: &str| Skip {
                category: ID.into(),
                title: info.display().to_string(),
                reason: reason.into(),
            };
            if matches!(name, b"" | b"." | b"..") {
                // The payload would be `files/` itself or its parent.
                out.skipped.push(skip("invalid trashinfo name"));
                continue;
            }

            let Some((date, deleted_at)) = deletion_date(&info) else {
                out.skipped.push(skip("unreadable trashinfo"));
                continue;
            };
            if deleted_at >= cutoff {
                continue;
            }
            if ctx.protector.is_protected(&info) || ctx.protector.is_protected(&payload) {
                out.skipped.push(skip("protected"));
                continue;
            }

            let (paths, reason) = match payload.symlink_metadata() {
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                    (vec![info.as_path()], "orphaned Trash record".to_string())
                }
                Err(_) => {
                    out.skipped.push(skip("cannot inspect Trash payload"));
                    continue;
                }
                Ok(md) if md.file_type().is_symlink() => {
                    // Fingerprints and apply refuse symlinks (§6.1).
                    out.skipped.push(skip("Trash payload is a symlink"));
                    continue;
                }
                Ok(_) => (vec![payload.as_path(), info.as_path()], format!("in Trash since {date}")),
            };
            match item(&paths, reason, seen) {
                Some(e) => out.entries.push(e),
                None => out.skipped.push(skip("changed during scan")),
            }
        }
        Ok(out)
    }
}

/// `DeletionDate=` (local time, `YYYY-MM-DDTHH:MM:SS`) as its date for
/// display and its Unix seconds, or `None` if missing or unparsable.
fn deletion_date(info: &Path) -> Option<(jiff::civil::Date, i64)> {
    let text = std::fs::read(info).ok()?;
    let text = String::from_utf8_lossy(&text);
    let value = text.lines().find_map(|l| l.trim().strip_prefix("DeletionDate="))?;
    let dt: jiff::civil::DateTime = value.trim().parse().ok()?;
    let ts = dt.to_zoned(jiff::tz::TimeZone::system()).ok()?.timestamp();
    Some((dt.date(), ts.as_second()))
}

/// An Item over `paths` (the first is the title), each fingerprinted.
fn item(paths: &[&Path], reason: String, seen: &mut InodeSet) -> Option<Entry> {
    let mut targets = Vec::new();
    let mut bytes = 0;
    for p in paths {
        targets.push(Target { path: p.to_path_buf(), fp: fingerprint(p).ok()? });
        bytes += measure(p, seen).ok()?.bytes;
    }
    Some(Entry {
        id: String::new(),
        category: ID.into(),
        title: paths[0].display().to_string(),
        kind: EntryKind::Item { targets },
        bytes,
        reason,
        selected: true,
        permanent: false,
        note: None,
    })
}
