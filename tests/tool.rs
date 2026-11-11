mod common;

use cleaner::protect::Protector;
use cleaner::tool::{Runner, SystemRunner, ToolError, parse_human_size};
use cleaner::walk::walk_dirs;
use std::collections::BTreeSet;
use std::ffi::OsStr;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

fn args(v: &[&'static str]) -> Vec<&'static OsStr> {
    v.iter().map(|s| OsStr::new(*s)).collect()
}

#[test]
fn system_runner_missing_program() {
    let err = SystemRunner
        .run("cleaner-no-such-tool", &args(&["--version"]), Duration::from_secs(5))
        .unwrap_err();
    assert!(matches!(err, ToolError::Missing(_)), "{err:?}");
    assert_eq!(err.to_string(), "cleaner-no-such-tool is not installed");
    assert_eq!(SystemRunner.which("cleaner-no-such-tool"), None);
}

#[test]
fn runner_captures_output_and_code() {
    let out = SystemRunner
        .run("sh", &args(&["-c", "echo out; echo err >&2; exit 3"]), Duration::from_secs(10))
        .unwrap();
    assert_eq!(out.code, Some(3));
    assert!(!out.ok());
    assert_eq!(out.stdout, "out\n");
    assert_eq!(out.stderr, "err\n");
}

#[test]
fn runner_sets_c_locale_and_null_stdin() {
    let out = SystemRunner
        .run("sh", &args(&["-c", "echo \"$LC_ALL\"; cat"]), Duration::from_secs(10))
        .unwrap();
    assert!(out.ok(), "{out:?}");
    assert_eq!(out.stdout, "C\n");
}

/// `sleep` processes that are children of any thread of this test process.
fn sleep_children() -> Vec<String> {
    let mut found = Vec::new();
    for task in std::fs::read_dir("/proc/self/task").unwrap().flatten() {
        let Ok(kids) = std::fs::read_to_string(task.path().join("children")) else { continue };
        for pid in kids.split_whitespace() {
            let cmdline = std::fs::read(format!("/proc/{pid}/cmdline")).unwrap_or_default();
            if String::from_utf8_lossy(&cmdline).contains("sleep") {
                found.push(pid.to_string());
            }
        }
    }
    found
}

#[test]
fn runner_timeout_kills_child() {
    let start = Instant::now();
    let err = SystemRunner
        .run("sh", &args(&["-c", "sleep 30"]), Duration::from_secs(1))
        .unwrap_err();
    assert!(start.elapsed() < Duration::from_secs(5), "{:?}", start.elapsed());
    assert!(matches!(err, ToolError::Timeout(_, 1)), "{err:?}");
    assert_eq!(err.to_string(), "sh timed out after 1s");
    assert_eq!(sleep_children(), Vec::<String>::new());
}

#[test]
fn parse_sizes() {
    assert_eq!(parse_human_size("1.75GB"), Some(1_750_000_000));
    assert_eq!(parse_human_size("521kB"), Some(521_000));
    assert_eq!(parse_human_size("0B"), Some(0));
    assert_eq!(parse_human_size("12.5 MiB"), Some(13_107_200));
    assert_eq!(parse_human_size("x"), None);
}

#[test]
fn walk_skips_symlinks_dotdirs_and_pruned() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().join("root");
    for d in ["a/b", ".hidden/x", "node_modules/y"] {
        std::fs::create_dir_all(root.join(d)).unwrap();
    }
    std::os::unix::fs::symlink(root.join("a"), root.join("l")).unwrap();
    let prot = Protector::new(tmp.path(), &[]);
    let mut seen = BTreeSet::new();
    walk_dirs(
        std::slice::from_ref(&root),
        6,
        &prot,
        &|p: &Path| p.file_name().is_some_and(|n| n == "node_modules"),
        &mut |p: &Path, _depth| {
            seen.insert(p.to_path_buf());
        },
    );
    let want: BTreeSet<PathBuf> = [root.clone(), root.join("a"), root.join("a/b")].into();
    assert_eq!(seen, want);
}

#[test]
fn walk_respects_depth_and_protection() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().join("root");
    for d in ["a/b/c", "keep/inner", "x/.git/objects"] {
        std::fs::create_dir_all(root.join(d)).unwrap();
    }
    let prot = Protector::new(tmp.path(), &[root.join("keep").to_str().unwrap().into()]);
    let mut seen = Vec::new();
    walk_dirs(std::slice::from_ref(&root), 2, &prot, &|_: &Path| false, &mut |p: &Path, d| {
        seen.push((p.strip_prefix(&root).unwrap().to_path_buf(), d));
    });
    seen.sort();
    let want: Vec<(PathBuf, usize)> =
        vec![("".into(), 0), ("a".into(), 1), ("a/b".into(), 2), ("x".into(), 1)];
    assert_eq!(seen, want);
}

#[test]
fn run_interactive_without_tty_is_plain_run() {
    use std::io::IsTerminal;
    if std::io::stdin().is_terminal() {
        eprintln!("skipping: stdin is a tty");
        return;
    }
    // The GUI path: stdin null, output captured, own process group.
    let out = SystemRunner
        .run_interactive("sh", &args(&["-c", "echo out; echo err >&2; cat; exit 3"]), Duration::from_secs(10))
        .unwrap();
    assert_eq!((out.code, out.stdout.as_str(), out.stderr.as_str()), (Some(3), "out\n", "err\n"));
}

/// Run under a pty (e.g. `script -qec 'cargo test --test tool run_interactive_on_a_tty' /dev/null`);
/// returns early otherwise.
#[test]
fn run_interactive_on_a_tty_shares_stdin_and_process_group() {
    use std::io::IsTerminal;
    if !std::io::stdin().is_terminal() {
        eprintln!("skipping: stdin is not a tty");
        return;
    }
    let pgrp = |stat: &str| stat.split(' ').nth(4).unwrap().trim().to_string();
    let ours = pgrp(&std::fs::read_to_string("/proc/self/stat").unwrap());
    let out = SystemRunner
        .run_interactive("sh", &args(&["-c", "test -t 0 && echo tty; cat /proc/$$/stat"]), Duration::from_secs(10))
        .unwrap();
    let mut lines = out.stdout.lines();
    assert_eq!(lines.next(), Some("tty"), "{out:?}");
    assert_eq!(pgrp(lines.next().unwrap()), ours, "child not in our (foreground) process group");
    println!("tty path checked");
}
