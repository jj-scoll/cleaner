//! The opt-in holding area (`[hold]`): instead of deleting a plan entry's
//! files and folders, apply moves them into `$XDG_DATA_HOME/cleaner/hold`,
//! from where `restore` puts them back and `purge` deletes them for good.
//!
//! Every move is one `renameat2(RENAME_NOREPLACE)` between directory fds
//! opened with `O_NOFOLLOW`: never a copy, never across filesystems (both
//! ends are checked against the holding area's device, and the kernel would
//! refuse with `EXDEV` anyway), never through a symlink as the last
//! component, never over an existing path. Anything that can't be moved
//! that way is left where it is, not deleted.
//!
//! Layout: one directory per held plan entry, `<id>/` (`<UTC stamp>-<n>`),
//! holding `meta.json` (a [`Held`]) and the moved items, named `0`, `1`, ...

use anyhow::{Context, bail};
use rustix::fd::{AsFd, OwnedFd};
use rustix::fs::{
    AtFlags, CWD, Dir, FileType, Mode, OFlags, RenameFlags, fstat, mkdirat, openat, renameat,
    renameat_with, statat, unlinkat,
};
use rustix::io::Errno;
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, HashMap};
use std::ffi::OsStr;
use std::io::{Read, Write};
use std::os::unix::fs::{DirBuilderExt, MetadataExt};
use std::path::{Path, PathBuf};

use crate::config::HoldCfg;
use crate::plan::{Entry, EntryKind, Plan};

const META: &str = "meta.json";
const META_TMP: &str = ".meta.json.tmp";

const DIR_FLAGS: OFlags = OFlags::DIRECTORY
    .union(OFlags::NOFOLLOW)
    .union(OFlags::CLOEXEC)
    .union(OFlags::RDONLY);

/// One moved file or folder.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct HeldItem {
    /// Its name inside the entry directory.
    pub name: String,
    /// Where it was, and where `restore` puts it back.
    #[serde(with = "crate::plan::path_codec")]
    pub original: PathBuf,
    pub bytes: u64,
    /// `(dev, ino)` of the folder it was in: restore goes back into that
    /// same folder or not at all.
    #[serde(default)]
    pub parent: Option<(u64, u64)>,
}

/// One hold entry: what one applied plan entry moved aside.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Default)]
pub struct Held {
    /// The entry directory's name (never read from `meta.json`).
    #[serde(skip)]
    pub id: String,
    /// Unix time it was held; it expires `days` after this.
    pub held_at: i64,
    #[serde(default)]
    pub plan_id: String,
    #[serde(default)]
    pub entry_id: String,
    #[serde(default)]
    pub category: String,
    #[serde(default)]
    pub title: String,
    #[serde(default)]
    pub items: Vec<HeldItem>,
    /// Why `meta.json` couldn't be read (`held_at` is then the dir's mtime).
    #[serde(skip)]
    pub problem: Option<String>,
}

impl Held {
    /// Bytes moved into the holding area (freed only by a purge).
    pub fn bytes(&self) -> u64 {
        self.items.iter().map(|i| i.bytes).sum()
    }
}

/// The holding area, opened: a real directory (not a symlink) we own.
pub struct Area {
    dir: PathBuf,
    fd: OwnedFd,
    dev: u64,
}

impl Area {
    /// Opens `dir`, creating it (mode 0700, parents as needed) if missing.
    /// Refused: a symlink, not a directory, or owned by someone else.
    pub fn open(dir: &Path) -> Result<Area, String> {
        if let Some(parent) = dir.parent() {
            std::fs::create_dir_all(parent).map_err(|e| format!("{}: {e}", parent.display()))?;
        }
        match std::fs::DirBuilder::new().mode(0o700).create(dir) {
            Ok(()) => {}
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {}
            Err(e) => return Err(format!("{}: {e}", dir.display())),
        }
        Area::open_dir(dir).map_err(|e| format!("{}: {e}", dir.display()))
    }

    /// Opens `dir` if anything is there (`Ok(None)` if not); checks as `open`.
    fn open_existing(dir: &Path) -> std::io::Result<Option<Area>> {
        match dir.symlink_metadata() {
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
            _ => Area::open_dir(dir).map(Some),
        }
    }

    fn open_dir(dir: &Path) -> std::io::Result<Area> {
        let fd = openat(CWD, dir, DIR_FLAGS, Mode::empty()).map_err(|e| match e {
            Errno::LOOP | Errno::NOTDIR => std::io::Error::other("is a symlink or not a folder"),
            e => e.into(),
        })?;
        let st = fstat(&fd)?;
        if st.st_uid != rustix::process::geteuid().as_raw() {
            return Err(std::io::Error::other("is owned by another user"));
        }
        #[allow(clippy::unnecessary_cast)]
        Ok(Area {
            dir: dir.to_path_buf(),
            fd,
            dev: st.st_dev as u64,
        })
    }

    pub fn dir(&self) -> &Path {
        &self.dir
    }

    pub fn dev(&self) -> u64 {
        self.dev
    }
}

/// Where, and recorded as what, apply holds one plan entry.
#[derive(Clone, Copy)]
pub struct HoldTo<'a> {
    pub area: &'a Area,
    pub plan_id: &'a str,
    pub entry_id: &'a str,
    pub category: &'a str,
    pub title: &'a str,
    pub now: i64,
}

/// Holds one plan entry's paths in one hold entry, made when first needed.
pub struct Session<'a> {
    to: HoldTo<'a>,
    /// The entry directory and what has been moved into it so far.
    open: Option<(OwnedFd, Held)>,
    /// Names given by `reserve` to paths not yet held.
    reserved: HashMap<PathBuf, String>,
    /// The next unused item name.
    next: usize,
}

impl<'a> Session<'a> {
    pub fn new(to: HoldTo<'a>) -> Session<'a> {
        Session {
            to,
            open: None,
            reserved: HashMap::new(),
            next: 0,
        }
    }

    pub fn area(&self) -> &Area {
        self.to.area
    }

    fn entry(&mut self) -> Result<&mut (OwnedFd, Held), String> {
        if self.open.is_none() {
            let entry =
                new_entry(&self.to).map_err(|e| format!("cannot start a hold entry: {e}"))?;
            self.open = Some(entry);
        }
        Ok(self.open.as_mut().expect("opened above"))
    }

    /// Records every one of `paths` in `meta.json` with one write before
    /// anything moves, so a crash part-way still says where each came from
    /// (`restore` ignores the ones never moved), and `meta.json` is
    /// rewritten once, in `finish`, instead of once per path.
    pub fn reserve(&mut self, paths: &[&Path]) -> Result<(), String> {
        let first = self.next;
        let (fd, held) = self.entry()?;
        let mut ahead = held.clone();
        let mut names = Vec::with_capacity(paths.len());
        for (i, p) in paths.iter().enumerate() {
            let name = (first + i).to_string();
            ahead.items.push(HeldItem {
                name: name.clone(),
                original: p.to_path_buf(),
                bytes: 0,
                parent: None,
            });
            names.push(((*p).to_path_buf(), name));
        }
        write_meta(fd, &ahead).map_err(|e| format!("cannot write hold metadata: {e}"))?;
        self.next += paths.len();
        self.reserved.extend(names);
        Ok(())
    }

    /// Moves `path` into the hold entry and returns its size. `expect` is
    /// the `(dev, ino)` the plan saw; another inode at `path` now is refused.
    /// `Err` says why it was left in place: nothing is ever deleted here.
    pub fn hold(&mut self, path: &Path, expect: Option<(u64, u64)>) -> Result<u64, String> {
        let area = self.to.area;
        let (parent, name) = split(path)?;
        let parent_fd = openat(CWD, parent, DIR_FLAGS, Mode::empty()).map_err(|e| match e {
            Errno::LOOP | Errno::NOTDIR => "its folder is now a symlink; left in place".to_string(),
            e => format!("cannot open its folder: {e}"),
        })?;
        let parent_id = dev_ino(&parent_fd).map_err(|e| e.to_string())?;
        if parent_id.0 != area.dev {
            return Err(other_fs(area));
        }
        let st = statat(&parent_fd, name, AtFlags::SYMLINK_NOFOLLOW).map_err(|e| e.to_string())?;
        if FileType::from_raw_mode(st.st_mode) == FileType::Symlink {
            return Err("is now a symlink; left in place".into());
        }
        #[allow(clippy::unnecessary_cast)]
        let (dev, ino) = (st.st_dev as u64, st.st_ino as u64);
        if dev != area.dev {
            return Err(other_fs(area));
        }
        if expect.is_some_and(|e| e != (dev, ino)) {
            return Err("changed since plan".into());
        }
        let bytes = crate::fsutil::measure::measure(path, &mut Default::default())
            .map(|m| m.bytes)
            .unwrap_or(0);

        let reserved = self.reserved.remove(path);
        let item = reserved.clone().unwrap_or_else(|| {
            self.next += 1;
            (self.next - 1).to_string()
        });
        let (entry_fd, held) = self.entry()?;
        held.items.push(HeldItem {
            name: item.clone(),
            original: path.to_path_buf(),
            bytes,
            parent: Some(parent_id),
        });
        // Not reserved: record it first, so a crash mid-move still says where it came from.
        if reserved.is_none()
            && let Err(e) = write_meta(entry_fd, held)
        {
            held.items.pop();
            return Err(format!("cannot write hold metadata: {e}"));
        }
        let moved = renameat_with(
            &parent_fd,
            name,
            &*entry_fd,
            item.as_str(),
            RenameFlags::NOREPLACE,
        );
        if let Err(e) = moved {
            held.items.pop();
            if reserved.is_none() {
                let _ = write_meta(entry_fd, held);
            }
            return Err(match e {
                Errno::XDEV => "would cross filesystems; left in place".into(),
                Errno::INVAL => {
                    "this filesystem can't move it without risk of overwriting (no RENAME_NOREPLACE); left in place".into()
                }
                e => format!("move failed: {e}; left in place"),
            });
        }
        Ok(bytes)
    }

    /// `(hold id, bytes held)` if anything was held. An entry left empty
    /// (every move refused) is removed again. After `reserve`, writes the
    /// final `meta.json` (only what moved); if that fails, the reserved one
    /// stays, which `restore` reads correctly too.
    pub fn finish(self) -> Option<(String, u64)> {
        let (fd, held) = self.open?;
        if !held.items.is_empty() {
            let _ = write_meta(&fd, &held);
        }
        if held.items.is_empty() {
            let _ = unlinkat(&fd, META, AtFlags::empty());
            let _ = unlinkat(&fd, META_TMP, AtFlags::empty());
            drop(fd);
            let _ = unlinkat(&self.to.area.fd, held.id.as_str(), AtFlags::REMOVEDIR);
            return None;
        }
        Some((held.id.clone(), held.bytes()))
    }
}

fn other_fs(area: &Area) -> String {
    format!(
        "not on the holding area's filesystem ({}); left in place, not deleted",
        area.dir.display()
    )
}

#[allow(clippy::unnecessary_cast)]
fn dev_ino(fd: impl AsFd) -> rustix::io::Result<(u64, u64)> {
    let st = fstat(fd)?;
    Ok((st.st_dev as u64, st.st_ino as u64))
}

/// `path`'s parent (`.` if none) and file name.
fn split(path: &Path) -> Result<(&Path, &OsStr), String> {
    let name = path
        .file_name()
        .ok_or_else(|| format!("no file name in {}", path.display()))?;
    let parent = match path.parent() {
        Some(p) if !p.as_os_str().is_empty() => p,
        _ => Path::new("."),
    };
    Ok((parent, name))
}

/// Makes `<stamp>-<n>` (the first free n) in the area, mode 0700.
fn new_entry(to: &HoldTo) -> std::io::Result<(OwnedFd, Held)> {
    let stamp = Plan::new_id(to.now);
    for n in 1..10_000 {
        let id = format!("{stamp}-{n}");
        match mkdirat(&to.area.fd, id.as_str(), Mode::RWXU) {
            Ok(()) => {
                let fd = openat(&to.area.fd, id.as_str(), DIR_FLAGS, Mode::empty())?;
                let held = Held {
                    id,
                    held_at: to.now,
                    plan_id: to.plan_id.into(),
                    entry_id: to.entry_id.into(),
                    category: to.category.into(),
                    title: to.title.into(),
                    items: Vec::new(),
                    problem: None,
                };
                return Ok((fd, held));
            }
            Err(Errno::EXIST) => continue,
            Err(e) => return Err(e.into()),
        }
    }
    Err(std::io::Error::other("no free hold id"))
}

/// Writes `meta.json` in `entry` through a temp file and a rename.
fn write_meta(entry: &OwnedFd, held: &Held) -> std::io::Result<()> {
    let json = serde_json::to_vec_pretty(held)?;
    let fd = openat(
        entry,
        META_TMP,
        OFlags::WRONLY | OFlags::CREATE | OFlags::TRUNC | OFlags::NOFOLLOW | OFlags::CLOEXEC,
        Mode::RUSR | Mode::WUSR,
    )?;
    let mut f = std::fs::File::from(fd);
    f.write_all(&json)?;
    f.sync_all()?;
    drop(f);
    renameat(entry, META_TMP, entry, META)?;
    Ok(())
}

fn read_meta(entry: &OwnedFd) -> std::io::Result<Held> {
    let fd = openat(
        entry,
        META,
        OFlags::RDONLY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
        Mode::empty(),
    )?;
    let mut text = Vec::new();
    std::fs::File::from(fd).read_to_end(&mut text)?;
    serde_json::from_slice(&text).map_err(std::io::Error::other)
}

/// Hold ids look like `20261125T030000Z-1`; nothing else in the area is an entry.
fn is_id(name: &str) -> bool {
    name.starts_with(|c: char| c.is_ascii_digit())
        && name.chars().all(|c| c.is_ascii_alphanumeric() || c == '-')
}

/// Every hold entry in an opened area, oldest first: directories (never
/// symlinks) with an id-shaped name. An unreadable `meta.json` gives a
/// `Held` with `problem` set and the directory's mtime as `held_at`.
fn entries(area: &Area) -> std::io::Result<Vec<Held>> {
    let mut out = Vec::new();
    let mut dir = Dir::read_from(&area.fd)?;
    while let Some(e) = dir.read() {
        let e = e?;
        let Ok(id) = e.file_name().to_str() else {
            continue;
        };
        if !is_id(id) {
            continue;
        }
        let st = match statat(&area.fd, id, AtFlags::SYMLINK_NOFOLLOW) {
            Ok(st) if FileType::from_raw_mode(st.st_mode) == FileType::Directory => st,
            _ => continue,
        };
        let meta = openat(&area.fd, id, DIR_FLAGS, Mode::empty())
            .map_err(std::io::Error::from)
            .and_then(|fd| read_meta(&fd));
        let mut held = match meta {
            Ok(h) => h,
            Err(e) => Held {
                #[allow(clippy::unnecessary_cast)]
                held_at: st.st_mtime as i64,
                problem: Some(format!("unreadable {META}: {e}")),
                ..Held::default()
            },
        };
        held.id = id.to_string();
        out.push(held);
    }
    out.sort_by(|a, b| a.held_at.cmp(&b.held_at).then_with(|| a.id.cmp(&b.id)));
    Ok(out)
}

/// The hold entries in `dir`, oldest first; none if `dir` doesn't exist.
pub fn list(dir: &Path) -> std::io::Result<Vec<Held>> {
    match Area::open_existing(dir)? {
        Some(area) => entries(&area),
        None => Ok(Vec::new()),
    }
}

/// Entries held at or before this time have expired.
pub fn cutoff(now: i64, days: u64) -> i64 {
    now.saturating_sub(days.saturating_mul(86_400) as i64)
}

/// One purged hold entry: the bytes freed, or why it couldn't be deleted.
pub struct Purged {
    pub held: Held,
    pub result: Result<u64, String>,
}

/// Deletes, with the safe deleter (no symlink followed, no mount crossed),
/// every hold entry in `dir` held at or before `cutoff` (every one if
/// `None`). Only entry directories, looked up in the opened area, are
/// touched: nothing else in or near `dir`.
pub fn purge(dir: &Path, cutoff: Option<i64>) -> std::io::Result<Vec<Purged>> {
    let Some(area) = Area::open_existing(dir)? else {
        return Ok(Vec::new());
    };
    let mut out = Vec::new();
    for held in entries(&area)? {
        if cutoff.is_some_and(|c| held.held_at > c) {
            continue;
        }
        let shown = dir.join(&held.id);
        let result = crate::fsutil::delete::delete_at(
            area.fd.as_fd(),
            OsStr::new(&held.id),
            &shown,
            area.dev,
        )
        .map_err(|e| format!("delete failed: {e}"));
        out.push(Purged { held, result });
    }
    Ok(out)
}

/// What `restore` puts back: a whole entry, or the one item held from a path.
pub enum Pick<'a> {
    Id(&'a str),
    Path(&'a Path),
}

/// Moves held items back where they were. Refused, before anything moves:
/// an original path that exists now (even as a dangling symlink). Refused
/// per item: a parent folder that is gone, is a symlink, is on another
/// filesystem, or is not the folder (by inode) it was held from. Returns
/// the entry (as it was) and the paths put back.
pub fn restore(dir: &Path, pick: Pick) -> anyhow::Result<(Held, Vec<PathBuf>)> {
    let area = Area::open_existing(dir)
        .with_context(|| format!("holding area {}", dir.display()))?
        .context("nothing is held")?;
    let all = entries(&area)?;
    let (held, names): (Held, Vec<String>) = match pick {
        Pick::Id(id) => {
            let Some(h) = all.into_iter().find(|h| h.id == id) else {
                bail!("no hold entry {id}");
            };
            let names = h.items.iter().map(|i| i.name.clone()).collect();
            (h, names)
        }
        Pick::Path(p) => {
            let mut found: Vec<(Held, String)> = all
                .into_iter()
                .filter_map(|h| {
                    let name = h.items.iter().find(|i| i.original == p)?.name.clone();
                    Some((h, name))
                })
                .collect();
            match found.len() {
                0 => bail!("nothing held from {}", p.display()),
                1 => {
                    let (h, n) = found.remove(0);
                    (h, vec![n])
                }
                _ => bail!(
                    "{} was held more than once ({}); restore by hold id",
                    p.display(),
                    found
                        .iter()
                        .map(|(h, _)| h.id.as_str())
                        .collect::<Vec<_>>()
                        .join(", ")
                ),
            }
        }
    };
    if let Some(problem) = &held.problem {
        bail!("hold entry {}: {problem}", held.id);
    }
    if names.is_empty() {
        bail!("hold entry {} holds nothing", held.id);
    }
    let entry_fd = openat(&area.fd, held.id.as_str(), DIR_FLAGS, Mode::empty())?;
    // An item named in meta.json but not in the entry was never moved (a
    // crash after `reserve`) or is already gone: there is nothing to put back.
    let present =
        |i: &HeldItem| statat(&entry_fd, i.name.as_str(), AtFlags::SYMLINK_NOFOLLOW).is_ok();
    let mut left = held.clone();
    left.items.retain(|i| present(i));
    let items: Vec<HeldItem> = left
        .items
        .iter()
        .filter(|i| names.contains(&i.name))
        .cloned()
        .collect();
    if items.is_empty() {
        bail!(
            "hold entry {}: nothing left to restore (its items are missing)",
            held.id
        );
    }
    for item in &items {
        if !item.original.is_absolute() {
            bail!(
                "hold entry {}: {} is not an absolute path",
                held.id,
                item.original.display()
            );
        }
        if item.original.symlink_metadata().is_ok() {
            bail!(
                "{} exists; not restoring over it (move it away first)",
                item.original.display()
            );
        }
    }
    let mut restored = Vec::new();
    let mut failed = None;
    for item in items {
        if let Err(e) = put_back(&area, &held.id, &entry_fd, &item) {
            failed = Some(e);
            break;
        }
        left.items.retain(|i| i.name != item.name);
        restored.push(item.original);
    }
    // One metadata write for the whole restore, also after a failure part-way.
    if left.items.is_empty() {
        let _ = unlinkat(&entry_fd, META, AtFlags::empty());
        let _ = unlinkat(&entry_fd, META_TMP, AtFlags::empty());
        let _ = unlinkat(&area.fd, held.id.as_str(), AtFlags::REMOVEDIR);
    } else if left != held {
        write_meta(&entry_fd, &left)?;
    }
    match failed {
        None => Ok((held, restored)),
        Some(e) if restored.is_empty() => Err(e),
        Some(e) => {
            let done: Vec<String> = restored.iter().map(|p| p.display().to_string()).collect();
            Err(e.context(format!("restored {} before this", done.join(", "))))
        }
    }
}

/// Renames `item` from the entry back to its original path, never over
/// anything, never through a symlinked, replaced or foreign parent.
fn put_back(area: &Area, id: &str, entry_fd: &OwnedFd, item: &HeldItem) -> anyhow::Result<()> {
    let (parent, name) = split(&item.original).map_err(anyhow::Error::msg)?;
    let shown = item.original.display();
    // Every refusal says where the held copy is, so it can be moved by hand.
    let copy = area.dir.join(id).join(&item.name);
    let copy = copy.display();
    let parent_fd = openat(CWD, parent, DIR_FLAGS, Mode::empty()).map_err(|e| match e {
        Errno::NOENT => {
            anyhow::anyhow!("{shown}: its folder is gone; not restoring. The held copy is {copy}")
        }
        Errno::LOOP | Errno::NOTDIR => anyhow::anyhow!(
            "{shown}: its folder is now a symlink; not restoring through it. The held copy is {copy}"
        ),
        e => anyhow::anyhow!("{shown}: cannot open its folder: {e}"),
    })?;
    let here = dev_ino(&parent_fd)?;
    if here.0 != area.dev {
        bail!(
            "{shown}: its folder is now on another filesystem than the holding area; not copying across filesystems. The held copy is {copy}"
        );
    }
    if item.parent.is_some_and(|p| p != here) {
        bail!(
            "{shown}: its folder was replaced since it was held (deleted and recreated, or redirected by a symlink); not restoring there. The held copy is {copy}"
        );
    }
    renameat_with(
        entry_fd,
        item.name.as_str(),
        &parent_fd,
        name,
        RenameFlags::NOREPLACE,
    )
    .map_err(|e| match e {
        Errno::EXIST | Errno::NOTEMPTY => anyhow::anyhow!("{shown} exists; not restoring over it"),
        Errno::XDEV => anyhow::anyhow!("{shown}: would cross filesystems; not restoring"),
        e => anyhow::anyhow!("{shown}: restore failed: {e}"),
    })?;
    Ok(())
}

/// What applying an entry will do while holding is on.
#[derive(Clone, Debug, PartialEq)]
pub enum Fate {
    /// Moved into the holding area.
    Held,
    /// Left in place, with why it can't be held.
    Skipped(String),
    /// Not a plain file or folder (Docker, Nix generations, tool commands,
    /// worktree removal): applied as usual, permanently.
    Permanent,
}

/// `fate` for every plan entry, and how long held items are kept.
#[derive(Clone, Debug, PartialEq)]
pub struct Preview {
    pub days: u64,
    pub fates: BTreeMap<String, Fate>,
}

/// The device the holding area is on (or, made now, would be) without
/// creating it: its own when it exists, else its nearest existing parent's.
pub fn area_dev(dir: &Path) -> Result<u64, String> {
    let shown = |e: &dyn std::fmt::Display| format!("{}: {e}", dir.display());
    match Area::open_existing(dir) {
        Ok(Some(a)) => Ok(a.dev),
        Err(e) => Err(shown(&e)),
        Ok(None) => dir
            .ancestors()
            .skip(1)
            .find_map(|p| std::fs::metadata(p).ok())
            .map(|md| md.dev())
            .ok_or_else(|| shown(&"no existing parent")),
    }
}

/// Whether apply holds `entry` (when holding is on): items and store rules.
pub fn holdable(entry: &Entry) -> bool {
    matches!(
        entry.kind,
        EntryKind::Item { .. } | EntryKind::StoreRule { .. }
    )
}

/// The paths of `entry` apply would hold.
fn paths(entry: &Entry) -> Vec<&Path> {
    match &entry.kind {
        EntryKind::Item { targets } => targets.iter().map(|t| t.path.as_path()).collect(),
        EntryKind::StoreRule { rule } => vec![rule.root.as_path()],
        EntryKind::Object { .. } | EntryKind::Operation { .. } => Vec::new(),
    }
}

/// Whether any of `entry`'s paths that exist is on another device than `dev`.
pub fn off_device(entry: &Entry, dev: u64) -> bool {
    paths(entry)
        .iter()
        .any(|p| p.symlink_metadata().is_ok_and(|md| md.dev() != dev))
}

/// Items and store rules are held when on the holding area's device
/// (`area`), skipped otherwise; everything else is `Permanent`.
pub fn fate(entry: &Entry, area: &Result<u64, String>) -> Fate {
    if !holdable(entry) {
        return Fate::Permanent;
    }
    match area {
        Err(e) => Fate::Skipped(format!("holding area unavailable: {e}")),
        Ok(dev) if off_device(entry, *dev) => {
            Fate::Skipped("on another filesystem than the holding area".into())
        }
        Ok(_) => Fate::Held,
    }
}

/// What applying each of `plan`'s entries will do; `None` when holding is off.
pub fn preview(plan: &Plan, dir: &Path, cfg: &HoldCfg) -> Option<Preview> {
    if !cfg.enabled {
        return None;
    }
    let area = area_dev(dir);
    Some(Preview {
        days: cfg.days,
        fates: plan
            .entries
            .iter()
            .map(|e| (e.id.clone(), fate(e, &area)))
            .collect(),
    })
}
