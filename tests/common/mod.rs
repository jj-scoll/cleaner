//! Shared helpers for integration tests. Every test uses a TempDir as home;
//! nothing here touches the real `$HOME`.
#![allow(dead_code)]

use cleaner::tool::{CmdOut, Runner, ToolError};
use std::ffi::OsStr;
use std::fs::{self, File};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Mutex;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

/// Creates parent dirs and writes `bytes` bytes to `path`.
pub fn mkfile(path: &Path, bytes: usize) {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).unwrap();
    }
    fs::write(path, vec![b'x'; bytes]).unwrap();
}

/// Sets the mtime (and atime) of `path` to now minus `days`.
pub fn set_age(path: &Path, days: u64) {
    set_times(path, days, days);
}

/// Sets the mtime and atime of `path` to now minus the given days.
pub fn set_times(path: &Path, mtime_days: u64, atime_days: u64) {
    let ago = |d: u64| SystemTime::now() - Duration::from_secs(d * 86_400);
    let f = File::open(path).unwrap();
    f.set_times(fs::FileTimes::new().set_modified(ago(mtime_days)).set_accessed(ago(atime_days)))
        .unwrap();
}

pub fn git(dir: &Path, args: &[&str]) {
    let status = Command::new("git")
        .args(args)
        .current_dir(dir)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_AUTHOR_NAME", "Test")
        .env("GIT_AUTHOR_EMAIL", "test@example.invalid")
        .env("GIT_AUTHOR_DATE", "2026-01-01T00:00:00Z")
        .env("GIT_COMMITTER_NAME", "Test")
        .env("GIT_COMMITTER_EMAIL", "test@example.invalid")
        .env("GIT_COMMITTER_DATE", "2026-01-01T00:00:00Z")
        .status()
        .expect("run git");
    assert!(status.success(), "git {args:?} failed in {}", dir.display());
}

pub fn git_init(dir: &Path) {
    fs::create_dir_all(dir).unwrap();
    git(dir, &["init", "-q", "-b", "main"]);
}

/// Stages everything and commits with a fixed author/committer.
pub fn git_commit_all(dir: &Path) {
    git(dir, &["add", "-A"]);
    git(
        dir,
        &["-c", "commit.gpgsign=false", "commit", "-q", "--allow-empty", "-m", "test"],
    );
}

/// Current time as Unix seconds.
pub fn now() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_secs() as i64
}

enum Reply {
    Out(CmdOut),
    /// `Err(ToolError::Timeout(program, secs))`.
    Timeout(u64),
}

/// A `Runner` that never starts a process. Records `"program arg arg…"`
/// calls. `respond(prefix, out)` answers calls starting with `prefix` (the
/// longest matching prefix wins); `installed(program, path)` makes `which()`
/// succeed. Anything unanswered → `Err(ToolError::Missing(program))`.
#[derive(Default)]
pub struct FakeRunner {
    replies: Vec<(String, Reply)>,
    installed: Vec<(String, PathBuf)>,
    calls: Mutex<Vec<String>>,
    interactive: Mutex<Vec<String>>,
}

impl FakeRunner {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn respond(mut self, prefix: &str, out: CmdOut) -> Self {
        self.replies.push((prefix.into(), Reply::Out(out)));
        self
    }

    /// Calls starting with `prefix` time out after `secs`.
    pub fn timeout(mut self, prefix: &str, secs: u64) -> Self {
        self.replies.push((prefix.into(), Reply::Timeout(secs)));
        self
    }

    pub fn installed(mut self, program: &str, path: &str) -> Self {
        self.installed.push((program.into(), path.into()));
        self
    }

    pub fn calls(&self) -> Vec<String> {
        self.calls.lock().unwrap().clone()
    }

    /// The calls that came through `run_interactive` (also in `calls()`).
    pub fn interactive_calls(&self) -> Vec<String> {
        self.interactive.lock().unwrap().clone()
    }
}

impl Runner for FakeRunner {
    fn which(&self, program: &str) -> Option<PathBuf> {
        self.installed.iter().find(|(p, _)| p == program).map(|(_, path)| path.clone())
    }

    fn run(&self, program: &str, args: &[&OsStr], _timeout: Duration) -> Result<CmdOut, ToolError> {
        let call = std::iter::once(program.to_string())
            .chain(args.iter().map(|a| a.to_string_lossy().into_owned()))
            .collect::<Vec<_>>()
            .join(" ");
        self.calls.lock().unwrap().push(call.clone());
        let reply = self
            .replies
            .iter()
            .filter(|(prefix, _)| call.starts_with(prefix.as_str()))
            .max_by_key(|(prefix, _)| prefix.len());
        match reply {
            Some((_, Reply::Out(out))) => Ok(out.clone()),
            Some((_, Reply::Timeout(secs))) => Err(ToolError::Timeout(program.into(), *secs)),
            None => Err(ToolError::Missing(program.into())),
        }
    }

    fn run_interactive(&self, program: &str, args: &[&OsStr], timeout: Duration) -> Result<CmdOut, ToolError> {
        let res = self.run(program, args, timeout);
        let last = self.calls.lock().unwrap().last().cloned().unwrap_or_default();
        self.interactive.lock().unwrap().push(last);
        res
    }
}

pub fn ok_out(stdout: &str) -> CmdOut {
    CmdOut { code: Some(0), stdout: stdout.into(), stderr: String::new() }
}

pub fn err_out(code: i32, stderr: &str) -> CmdOut {
    CmdOut { code: Some(code), stdout: String::new(), stderr: stderr.into() }
}

/// Creates `dir/bin` holding only a symlink to the real `git` and returns
/// it, for use as `PATH` so the binary under test finds no other tool.
pub fn git_only_path(dir: &Path) -> PathBuf {
    let git = std::env::var_os("PATH")
        .iter()
        .flat_map(std::env::split_paths)
        .map(|d| d.join("git"))
        .find(|p| p.is_file())
        .expect("git on PATH");
    let bin = dir.join("bin");
    fs::create_dir_all(&bin).unwrap();
    std::os::unix::fs::symlink(git, bin.join("git")).unwrap();
    bin
}

/// The effective uid of the test process.
pub fn my_uid() -> u32 {
    rustix::process::geteuid().as_raw()
}
