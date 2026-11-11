//! Runs external tools (docker, nix, uv, pkexec) behind a trait, so tests
//! inject a fake and never start them (spec §8).

use std::ffi::OsStr;
use std::io::Read;
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

pub const QUERY_TIMEOUT: Duration = Duration::from_secs(60);
pub const OP_TIMEOUT: Duration = Duration::from_secs(600);
pub const GC_TIMEOUT: Duration = Duration::from_secs(1800);
/// How long pkexec may take, authentication included. After authentication
/// the root child can't be killed, so the run can outlast it (see `run_child`).
pub const PKEXEC_TIMEOUT: Duration = Duration::from_secs(300);

#[derive(Clone, Debug, PartialEq)]
pub struct CmdOut {
    /// `None` if the process was killed by a signal.
    pub code: Option<i32>,
    pub stdout: String,
    pub stderr: String,
}

impl CmdOut {
    pub fn ok(&self) -> bool {
        self.code == Some(0)
    }
}

#[derive(Debug, thiserror::Error)]
pub enum ToolError {
    #[error("{0} is not installed")]
    Missing(String),
    #[error("{0} timed out after {1}s")]
    Timeout(String, u64),
    #[error("{0}: {1}")]
    Io(String, std::io::Error),
}

pub trait Runner: Send + Sync {
    /// The program's path, or `None` if it isn't installed.
    fn which(&self, program: &str) -> Option<PathBuf>;
    /// Runs `program` with stdin null and `LC_ALL=C`, killing it after `timeout`.
    fn run(&self, program: &str, args: &[&OsStr], timeout: Duration) -> Result<CmdOut, ToolError>;
    /// Like `run`, for a program that may prompt the user (pkexec's text
    /// agent, spec §5.2f).
    fn run_interactive(&self, program: &str, args: &[&OsStr], timeout: Duration) -> Result<CmdOut, ToolError> {
        self.run(program, args, timeout)
    }
}

/// Resolves programs via `PATH`. Each child gets its own process group, so a
/// timeout kills anything it started too.
pub struct SystemRunner;

/// `program` if it names an executable file, else the first `PATH` entry
/// that has one.
fn resolve(program: &str) -> Option<PathBuf> {
    let executable = |p: &Path| {
        p.metadata().is_ok_and(|m| m.is_file()) && rustix::fs::access(p, rustix::fs::Access::EXEC_OK).is_ok()
    };
    if program.contains('/') {
        let p = PathBuf::from(program);
        return executable(&p).then_some(p);
    }
    std::env::split_paths(&std::env::var_os("PATH")?)
        .map(|dir| dir.join(program))
        .find(|p| executable(p))
}

fn read_all(mut r: impl Read + Send + 'static) -> std::thread::JoinHandle<String> {
    std::thread::spawn(move || {
        let mut buf = Vec::new();
        let _ = r.read_to_end(&mut buf);
        String::from_utf8_lossy(&buf).into_owned()
    })
}

impl Runner for SystemRunner {
    fn which(&self, program: &str) -> Option<PathBuf> {
        resolve(program)
    }

    fn run(&self, program: &str, args: &[&OsStr], timeout: Duration) -> Result<CmdOut, ToolError> {
        run_child(program, args, timeout, false)
    }

    /// When stdin is a tty (the CLI), the child stays in our foreground
    /// process group with stdin and stderr inherited, so pkexec's text agent
    /// can prompt and Ctrl-C reaches it. Otherwise (GUI, timer) it is `run`.
    fn run_interactive(&self, program: &str, args: &[&OsStr], timeout: Duration) -> Result<CmdOut, ToolError> {
        use std::io::IsTerminal;
        run_child(program, args, timeout, std::io::stdin().is_terminal())
    }
}

fn run_child(program: &str, args: &[&OsStr], timeout: Duration, interactive: bool) -> Result<CmdOut, ToolError> {
    let path = resolve(program).ok_or_else(|| ToolError::Missing(program.into()))?;
    let mut cmd = Command::new(&path);
    cmd.args(args).env("LC_ALL", "C").stdout(Stdio::piped());
    if interactive {
        cmd.stdin(Stdio::inherit()).stderr(Stdio::inherit());
    } else {
        cmd.stdin(Stdio::null()).stderr(Stdio::piped()).process_group(0);
    }
    let mut child = cmd.spawn().map_err(|e| match e.kind() {
        std::io::ErrorKind::NotFound => ToolError::Missing(program.into()),
        _ => ToolError::Io(program.into(), e),
    })?;
    let stdout = read_all(child.stdout.take().expect("piped stdout"));
    let stderr = child.stderr.take().map(read_all);
    let deadline = Instant::now() + timeout;
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) if Instant::now() >= deadline => {
                // The whole group: a shell's own children die with it.
                // (An interactive child shares our group: only it is killed.)
                if !interactive && let Some(pgid) = rustix::process::Pid::from_raw(child.id() as i32) {
                    let _ = rustix::process::kill_process_group(pgid, rustix::process::Signal::KILL);
                }
                let _ = child.kill();
                // Not a hard bound: once pkexec has exec'd its program as
                // root, both kills get EPERM and this waits until that
                // program exits (e.g. nix-env waiting on the profile lock
                // while a nixos-rebuild holds it).
                let _ = child.wait();
                // The reader threads are left to finish on their own: a
                // process outside the group (e.g. setuid) may still hold a pipe.
                return Err(ToolError::Timeout(program.into(), timeout.as_secs()));
            }
            Ok(None) => std::thread::sleep(Duration::from_millis(20)),
            Err(e) => {
                let _ = child.kill();
                let _ = child.wait();
                return Err(ToolError::Io(program.into(), e));
            }
        }
    };
    Ok(CmdOut {
        code: status.code(),
        stdout: stdout.join().unwrap_or_default(),
        stderr: stderr.and_then(|h| h.join().ok()).unwrap_or_default(),
    })
}

/// "1.75GB" | "521kB" | "0B" | "12.3 MiB" → bytes. Docker units are decimal (kB=1e3, MB=1e6, GB=1e9, TB=1e12);
/// KiB/MiB/GiB/TiB are binary (nix-store, uv). None if unparseable.
pub fn parse_human_size(s: &str) -> Option<u64> {
    let s = s.trim();
    let split = s.find(|c: char| !(c.is_ascii_digit() || c == '.'))?;
    let (num, unit) = s.split_at(split);
    let n: f64 = num.parse().ok()?;
    let mult: f64 = match unit.trim_start() {
        "B" => 1.0,
        "kB" | "KB" => 1e3,
        "MB" => 1e6,
        "GB" => 1e9,
        "TB" => 1e12,
        "KiB" => 1024.0,
        "MiB" => 1024.0 * 1024.0,
        "GiB" => 1024.0 * 1024.0 * 1024.0,
        "TiB" => 1024.0 * 1024.0 * 1024.0 * 1024.0,
        _ => return None,
    };
    Some((n * mult).round() as u64)
}
