mod common;

use assert_cmd::Command;
use common::{mkfile, set_age};
use predicates::str::contains;
use std::path::{Path, PathBuf};
use tempfile::TempDir;

/// A temp dir holding `home/` (one stale Rust project, an old pip cache file,
/// an old Download) and `run/` for XDG_RUNTIME_DIR.
struct Env {
    tmp: TempDir,
    /// The only `PATH` entry: a dir holding just `git`, so no other tool runs.
    bin: PathBuf,
}

impl Env {
    fn new() -> Env {
        let tmp = TempDir::new().unwrap();
        let home = tmp.path().join("home");
        let proj = home.join("proj");
        for (f, n) in [
            ("Cargo.toml", 10),
            ("src/main.rs", 10),
            ("target/debug/app", 50_000),
        ] {
            mkfile(&proj.join(f), n);
            set_age(&proj.join(f), 40);
        }
        mkfile(&home.join(".cache/pip/http/a"), 20_000);
        set_age(&home.join(".cache/pip/http/a"), 40);
        mkfile(&home.join("Downloads/old.zip"), 20_000);
        set_age(&home.join("Downloads/old.zip"), 100);
        std::fs::create_dir_all(tmp.path().join("run")).unwrap();
        let bin = common::git_only_path(tmp.path());
        Env { tmp, bin }
    }

    fn home(&self) -> PathBuf {
        self.tmp.path().join("home")
    }

    fn artifact(&self) -> PathBuf {
        self.home().join("proj/target")
    }

    fn cache_file(&self) -> PathBuf {
        self.home().join(".cache/pip/http/a")
    }

    fn plans_dir(&self) -> PathBuf {
        self.home().join(".local/state/cleaner/plans")
    }

    /// `cleaner <args>` with HOME and every XDG dir inside the temp dir.
    fn cmd(&self, args: &[&str]) -> Command {
        Command::from_std(self.std_cmd(args))
    }

    /// `cmd` as a plain `std::process::Command`.
    fn std_cmd(&self, args: &[&str]) -> std::process::Command {
        let home = self.home();
        let mut c = std::process::Command::new(env!("CARGO_BIN_EXE_cleaner"));
        c.args(args)
            .env("HOME", &home)
            .env("XDG_CONFIG_HOME", home.join(".config"))
            .env("XDG_STATE_HOME", home.join(".local/state"))
            .env("XDG_DATA_HOME", home.join(".local/share"))
            .env("XDG_RUNTIME_DIR", self.tmp.path().join("run"))
            .env("TZ", "UTC")
            .env("PATH", &self.bin);
        c
    }

    fn plan(&self) {
        self.cmd(&["plan"]).assert().success();
    }

    /// `cleaner plan` then `cleaner show`, so a no-id apply may use it.
    fn plan_and_show(&self) {
        self.plan();
        self.cmd(&["show"]).assert().success();
    }

    /// Ids of the saved plans, oldest first.
    fn plan_ids(&self) -> Vec<String> {
        let mut ids: Vec<String> = std::fs::read_dir(self.plans_dir())
            .unwrap()
            .map(|e| e.unwrap().file_name().into_string().unwrap())
            .filter_map(|n| n.strip_suffix(".json").map(str::to_string))
            .collect();
        ids.sort();
        ids
    }

    /// Copies the newest plan as a later plan `id` (what a timer run would leave).
    fn add_newer_plan(&self, id: &str) {
        let src = self
            .plans_dir()
            .join(format!("{}.json", self.plan_ids().last().unwrap()));
        let mut v: serde_json::Value =
            serde_json::from_slice(&std::fs::read(src).unwrap()).unwrap();
        v["id"] = id.into();
        std::fs::write(self.plans_dir().join(format!("{id}.json")), v.to_string()).unwrap();
    }
}

fn plan_count(dir: &Path) -> usize {
    std::fs::read_dir(dir).map(|rd| rd.count()).unwrap_or(0)
}

#[test]
fn cli_help_lists_gui() {
    let env = Env::new();
    env.cmd(&["--help"])
        .assert()
        .success()
        .stdout(contains("gui"));
}

#[test]
fn cli_no_args_without_display_prints_help() {
    let env = Env::new();
    env.cmd(&[])
        .env_remove("WAYLAND_DISPLAY")
        .env_remove("DISPLAY")
        .assert()
        .success()
        .stdout(contains("Usage:"));
}

/// Runs `cleaner <args>` with stdin, stdout and stderr on a fresh 80x24
/// pseudo-terminal, types `keys` once `ready` appears in its output, and
/// returns its exit code and everything it wrote.
fn run_in_pty(env: &Env, args: &[&str], ready: &str, keys: &[u8]) -> (i32, String) {
    use std::io::{Read, Write};
    use std::os::fd::FromRawFd;
    use std::os::unix::fs::OpenOptionsExt;
    use std::time::{Duration, Instant};

    // SAFETY: plain libc pty setup on a descriptor this function owns.
    let (master, slave_name) = unsafe {
        let fd = libc::posix_openpt(libc::O_RDWR | libc::O_NOCTTY);
        assert!(fd >= 0, "posix_openpt");
        assert_eq!(libc::grantpt(fd), 0, "grantpt");
        assert_eq!(libc::unlockpt(fd), 0, "unlockpt");
        let mut name = [0 as libc::c_char; 128];
        assert_eq!(libc::ptsname_r(fd, name.as_mut_ptr(), name.len()), 0);
        let ws = libc::winsize {
            ws_row: 24,
            ws_col: 80,
            ws_xpixel: 0,
            ws_ypixel: 0,
        };
        assert_eq!(libc::ioctl(fd, libc::TIOCSWINSZ, &ws), 0, "TIOCSWINSZ");
        let name = std::ffi::CStr::from_ptr(name.as_ptr());
        (
            std::fs::File::from_raw_fd(fd),
            PathBuf::from(name.to_str().unwrap()),
        )
    };
    let slave = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .custom_flags(libc::O_NOCTTY)
        .open(&slave_name)
        .unwrap();
    let mut child = env
        .std_cmd(args)
        .stdin(slave.try_clone().unwrap())
        .stdout(slave.try_clone().unwrap())
        .stderr(slave)
        .spawn()
        .unwrap();

    let out = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
    let mut reader = master.try_clone().unwrap();
    let sink = out.clone();
    // Ends with EIO once the child has exited and closed the terminal.
    std::thread::spawn(move || {
        let mut buf = [0u8; 4096];
        while let Ok(n) = reader.read(&mut buf) {
            if n == 0 {
                break;
            }
            sink.lock().unwrap().extend_from_slice(&buf[..n]);
        }
    });
    let text = || String::from_utf8_lossy(&out.lock().unwrap()).into_owned();
    let deadline = Instant::now() + Duration::from_secs(30);
    while !text().contains(ready) {
        if Instant::now() > deadline || child.try_wait().unwrap().is_some() {
            let _ = child.kill();
            panic!("never saw {ready:?}; output:\n{}", text());
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    let mut writer = master;
    for &k in keys {
        writer.write_all(&[k]).unwrap();
        std::thread::sleep(Duration::from_millis(50));
    }
    let status = loop {
        if let Some(s) = child.try_wait().unwrap() {
            break s;
        }
        if Instant::now() > deadline {
            let _ = child.kill();
            panic!("cleaner review did not exit; output:\n{}", text());
        }
        std::thread::sleep(Duration::from_millis(20));
    };
    std::thread::sleep(Duration::from_millis(100));
    (status.code().expect("exit code, not a signal"), text())
}

#[test]
fn cli_review_quit_without_deleting_exits_1() {
    let env = Env::new();
    env.plan();
    // q at once; then Enter (confirmation), n (back to the list), q.
    for keys in [&b"q"[..], &b"\rnq"[..]] {
        let (code, out) = run_in_pty(&env, &["review"], "sort:", keys);
        assert_eq!(code, 1, "keys {keys:?}; output:\n{out}");
        assert!(out.contains("Nothing deleted."), "{out}");
        assert!(env.artifact().exists());
        assert!(env.cache_file().exists());
    }
}

#[test]
fn cli_review_without_a_terminal_points_at_show_and_apply() {
    let env = Env::new();
    // assert_cmd pipes stdout, so it is not a TTY. The check comes before
    // anything else: no plan is needed to see it.
    env.cmd(&["review"])
        .assert()
        .code(1)
        .stderr(contains("cleaner review needs a terminal"))
        .stderr(contains("cleaner show"))
        .stderr(contains("cleaner apply"));
}

#[test]
fn cli_plan_then_show() {
    let env = Env::new();
    env.cmd(&["plan"])
        .assert()
        .success()
        .stdout(contains("artifacts"))
        .stdout(contains("Total selected"));
    assert!(env.home().join(".config/cleaner/config.toml").exists());
    env.cmd(&["show"])
        .assert()
        .success()
        .stdout(contains("proj"))
        .stdout(contains("[x]"));
}

#[test]
fn cli_apply_requires_confirmation() {
    let env = Env::new();
    env.plan_and_show();
    env.cmd(&["apply"])
        .write_stdin("n\n")
        .assert()
        .code(1)
        .stderr(contains("Aborted; nothing deleted."));
    assert!(env.artifact().exists());
    assert!(env.cache_file().exists());
}

#[test]
fn cli_apply_prompt_names_plan() {
    let env = Env::new();
    env.plan_and_show();
    let id = env.plan_ids().pop().unwrap();
    env.cmd(&["apply"])
        .write_stdin("n\n")
        .assert()
        .code(1)
        .stdout(contains(format!("Plan {id} (created ")))
        .stdout(contains("[y/N]"));
}

#[test]
fn cli_apply_refuses_unshown_plan() {
    let env = Env::new();
    env.plan();
    env.cmd(&["apply", "--yes"])
        .assert()
        .code(1)
        .stderr(contains("has not been shown"));
    assert!(env.artifact().exists());
    assert!(env.cache_file().exists());
}

#[test]
fn cli_apply_refuses_newer_plan_than_shown() {
    let env = Env::new();
    env.plan_and_show();
    env.add_newer_plan("29991231T000000Z");
    env.cmd(&["apply", "--yes"])
        .assert()
        .code(1)
        .stderr(contains("newest plan 29991231T000000Z has not been shown"));
    assert!(env.artifact().exists());
    // Showing that plan by id makes it the shown plan.
    env.cmd(&["show", "29991231T000000Z"]).assert().success();
    env.cmd(&["apply", "--yes"]).assert().success();
    assert!(!env.artifact().exists());
}

#[test]
fn cli_apply_explicit_id_needs_no_show() {
    let env = Env::new();
    env.plan();
    let id = env.plan_ids().pop().unwrap();
    env.cmd(&["apply", "--yes", &id]).assert().success();
    assert!(!env.artifact().exists());
}

#[test]
fn cli_respects_run_lock() {
    use predicates::prelude::PredicateBooleanExt;
    let env = Env::new();
    env.plan_and_show();
    let _held = cleaner::lock::acquire(&env.tmp.path().join("run/cleaner.lock")).unwrap();
    env.cmd(&["plan"])
        .assert()
        .code(1)
        .stderr(contains("another cleaner run is in progress"));
    env.cmd(&["apply", "--yes"])
        .assert()
        .code(1)
        .stderr(contains("another cleaner run is in progress"))
        .stderr(contains("[1/").not());
    assert!(env.artifact().exists());
    assert!(env.cache_file().exists());
}

#[test]
fn cli_apply_shows_progress_on_stderr() {
    let env = Env::new();
    env.plan_and_show();
    let out = env.cmd(&["apply", "--yes"]).output().unwrap();
    assert!(out.status.success(), "{out:?}");
    let stderr = String::from_utf8(out.stderr).unwrap();
    let stdout = String::from_utf8(out.stdout).unwrap();
    let lines: Vec<&str> = stderr.lines().filter(|l| l.starts_with('[')).collect();
    assert!(!lines.is_empty(), "no progress lines: {stderr:?}");
    assert!(lines[0].starts_with("[1/"), "{stderr:?}");
    assert!(
        !stderr.contains('\x1b'),
        "escape codes on a pipe: {stderr:?}"
    );
    assert!(!stdout.contains("[1/"), "progress on stdout: {stdout:?}");
}

#[test]
fn cli_apply_yes_deletes() {
    let env = Env::new();
    env.plan_and_show();
    env.cmd(&["apply", "--yes"]).assert().success();
    assert!(!env.artifact().exists());
    env.cmd(&["log"])
        .assert()
        .success()
        .stdout(contains("deleted"));
}

#[test]
fn cli_only_filters() {
    let env = Env::new();
    env.plan_and_show();
    env.cmd(&["apply", "--yes", "--only", "caches"])
        .assert()
        .success();
    assert!(!env.cache_file().exists());
    assert!(env.artifact().exists());
}

/// One selected entry (the pip cache): counts read "1 item", and the cache
/// row is shown under `~` while the saved plan keeps its absolute root.
#[test]
fn cli_single_item_wording() {
    let env = Env::new();
    std::fs::remove_dir_all(env.home().join("proj")).unwrap();
    std::fs::remove_dir_all(env.home().join("Downloads")).unwrap();
    env.cmd(&["plan"])
        .assert()
        .success()
        .stdout(contains(" in 1 item\n"));
    env.cmd(&["show"])
        .assert()
        .success()
        .stdout(contains("~/.cache/pip  (1 item untouched for 30+ days)"));
    env.cmd(&["status"])
        .assert()
        .success()
        .stdout(contains(" in 1 item)"));
    env.cmd(&["apply"])
        .write_stdin("n\n")
        .assert()
        .code(1)
        .stdout(contains("Delete 1 item ("));

    let plan: serde_json::Value = serde_json::from_slice(
        &std::fs::read(env.plans_dir().join(format!("{}.json", env.plan_ids()[0]))).unwrap(),
    )
    .unwrap();
    let e = &plan["entries"][0];
    assert_eq!(e["title"], "~/.cache/pip", "{e}");
    let root = env.home().join(".cache/pip");
    assert_eq!(
        e["kind"]["StoreRule"]["rule"]["root"],
        root.to_str().unwrap(),
        "{e}"
    );
}

#[test]
fn cli_bad_config_exits_nonzero() {
    let env = Env::new();
    mkfile(&env.home().join(".config/cleaner/config.toml"), 0);
    std::fs::write(env.home().join(".config/cleaner/config.toml"), "[artifacts").unwrap();
    env.cmd(&["plan"])
        .assert()
        .code(1)
        .stderr(contains("config.toml"));
    assert_eq!(plan_count(&env.plans_dir()), 0);
}

#[test]
fn cli_status_json() {
    let env = Env::new();
    env.plan();
    let out = env
        .cmd(&["status", "--json"])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let v: serde_json::Value = serde_json::from_slice(&out).unwrap();
    assert!(v["filesystems"].is_array(), "{v}");
    assert!(!v["filesystems"].as_array().unwrap().is_empty(), "{v}");
}

#[test]
fn cli_plan_without_tools_skips_cleanly() {
    use predicates::prelude::PredicateBooleanExt;
    let env = Env::new();
    env.cmd(&["plan"])
        .assert()
        .success()
        .stdout(contains("category failed").not());
    let plan: serde_json::Value = serde_json::from_slice(
        &std::fs::read(env.plans_dir().join(format!("{}.json", env.plan_ids()[0]))).unwrap(),
    )
    .unwrap();
    let skipped = plan["skipped"].as_array().unwrap();
    assert!(
        skipped
            .iter()
            .all(|s| !s["reason"].as_str().unwrap().contains("category failed")),
        "{skipped:?}"
    );
}

#[test]
fn cli_show_marks_nix_generations_as_estimate() {
    let env = Env::new();
    env.plan();
    let src = env.plans_dir().join(format!("{}.json", env.plan_ids()[0]));
    let mut v: serde_json::Value = serde_json::from_slice(&std::fs::read(&src).unwrap()).unwrap();
    v["entries"].as_array_mut().unwrap().push(serde_json::json!({
        "id": "nix:0", "category": "nix", "title": "system: generations 1424–1429",
        "kind": { "Object": { "obj": { "NixGenerations": {
            "profile": "/nix/var/nix/profiles/system", "gens": [[1424, "/nix/store/x"]], "root": true
        } } } },
        "bytes": 2048, "reason": "keeping newest 5 of 11", "selected": false, "permanent": true, "note": null
    }));
    std::fs::write(&src, v.to_string()).unwrap();
    let out = env
        .cmd(&["show"])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let out = String::from_utf8(out).unwrap();
    let line = out
        .lines()
        .find(|l| l.contains("system: generations"))
        .expect("nix row");
    assert!(line.contains("~2.0 KiB"), "{line}");
    assert!(line.ends_with("[estimate]"), "{line}");
}

// ---- `plan --if-low` (the low-space check: plans and notifies, never deletes) ----

impl Env {
    /// `cleaner plan --if-low` with an extra stale project `p`, `[low_space] <lines>`,
    /// and no reachable D-Bus.
    fn if_low(&self, low_space: &str) -> Command {
        let proj = self.home().join("p");
        for (f, n) in [("Cargo.toml", 10), ("target/x", 50_000)] {
            mkfile(&proj.join(f), n);
            set_age(&proj.join(f), 40);
        }
        std::fs::create_dir_all(self.home().join(".config/cleaner")).unwrap();
        std::fs::write(
            self.home().join(".config/cleaner/config.toml"),
            format!("[low_space]\n{low_space}\n"),
        )
        .unwrap();
        let mut c = self.cmd(&["plan", "--if-low"]);
        c.env("DBUS_SESSION_BUS_ADDRESS", "unix:path=/nonexistent");
        c
    }

    fn space_state(&self) -> PathBuf {
        self.tmp.path().join("run/cleaner-space.json")
    }
}

#[test]
fn if_low_quiet_when_space_ok() {
    let env = Env::new();
    env.if_low("percent = 0").assert().success();
    assert!(!env.plans_dir().exists());
}

#[test]
fn if_low_plans_but_never_applies() {
    let env = Env::new();
    env.if_low("percent = 100")
        .assert()
        .success()
        .stderr(contains("notification failed"));
    let ids = env.plan_ids();
    assert_eq!(ids.len(), 1);
    let plan: serde_json::Value = serde_json::from_slice(
        &std::fs::read(env.plans_dir().join(format!("{}.json", ids[0]))).unwrap(),
    )
    .unwrap();
    assert!(
        plan["entries"]
            .as_array()
            .unwrap()
            .iter()
            .any(|e| e["category"] == "artifacts"
                && e["selected"] == true
                && e.to_string().contains("/p/target")),
        "{plan}"
    );
    assert!(env.home().join("p/target/x").exists());
    assert!(!env.home().join(".local/state/cleaner/log.jsonl").exists());
    let st: serde_json::Value =
        serde_json::from_slice(&std::fs::read(env.space_state()).unwrap()).unwrap();
    assert!(st.to_string().contains("\"armed\":false"), "{st}");

    env.if_low("percent = 100").assert().success();
    assert_eq!(env.plan_ids().len(), 1);
}

#[test]
fn if_low_lock_held_is_quiet() {
    let env = Env::new();
    let _held = cleaner::lock::acquire(&env.tmp.path().join("run/cleaner.lock")).unwrap();
    env.if_low("percent = 100").assert().success();
    assert!(!env.plans_dir().exists());
    assert!(!env.space_state().exists());
}

#[test]
fn if_low_disabled_does_nothing() {
    let env = Env::new();
    env.if_low("enabled = false\npercent = 100")
        .assert()
        .success();
    assert!(!env.plans_dir().exists());
    assert!(!env.space_state().exists());
}

#[test]
fn if_low_unsaved_plan_still_exits_zero() {
    use std::os::unix::fs::PermissionsExt;
    let env = Env::new();
    let state_dir = env.home().join(".local/state/cleaner");
    std::fs::create_dir_all(&state_dir).unwrap();
    std::fs::set_permissions(&state_dir, std::fs::Permissions::from_mode(0o500)).unwrap();
    let out = env
        .if_low("percent = 100")
        .assert()
        .success()
        .get_output()
        .clone();
    std::fs::set_permissions(&state_dir, std::fs::Permissions::from_mode(0o700)).unwrap();
    assert!(
        env.space_state().exists(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(!env.plans_dir().exists());
}

#[test]
fn cli_show_sorts_largest_first_or_by_flag() {
    let env = Env::new();
    env.plan();
    let src = env.plans_dir().join(format!("{}.json", env.plan_ids()[0]));
    let mut v: serde_json::Value = serde_json::from_slice(&std::fs::read(&src).unwrap()).unwrap();
    let entries = v["entries"].as_array_mut().unwrap();
    entries.clear();
    for (i, (title, bytes)) in [("bravo", 1024), ("alpha", 4096), ("charlie", 2048)]
        .iter()
        .enumerate()
    {
        entries.push(serde_json::json!({
            "id": format!("trash:{i}"), "category": "trash", "title": title,
            "kind": { "Item": { "targets": [] } },
            "bytes": bytes, "reason": "r", "selected": true, "permanent": false, "note": null
        }));
    }
    std::fs::write(&src, v.to_string()).unwrap();
    let order = |args: &[&str]| -> Vec<String> {
        let out = env.cmd(args).assert().success().get_output().stdout.clone();
        let out = String::from_utf8(out).unwrap();
        let mut found: Vec<(usize, String)> = ["alpha", "bravo", "charlie"]
            .iter()
            .map(|t| {
                (
                    out.find(&format!("  {t}  "))
                        .unwrap_or_else(|| panic!("no {t} in {out}")),
                    t.to_string(),
                )
            })
            .collect();
        found.sort();
        found.into_iter().map(|(_, t)| t).collect()
    };
    assert_eq!(order(&["show"]), ["alpha", "charlie", "bravo"]);
    assert_eq!(
        order(&["show", "--sort", "name"]),
        ["alpha", "bravo", "charlie"]
    );
    env.cmd(&["show", "--sort", "sideways"]).assert().code(2);
}

fn log_file(env: &Env) -> PathBuf {
    env.home().join(".local/state/cleaner/log.jsonl")
}

fn log_line(run: &str, time: i64, category: &str, bytes: u64) -> cleaner::log::LogLine {
    cleaner::log::LogLine {
        time,
        run_id: run.into(),
        plan_id: "P".into(),
        entry_id: format!("{category}:0"),
        category: category.into(),
        title: format!("{category} item"),
        outcome: "deleted".into(),
        bytes,
        detail: String::new(),
    }
}

const GIB: u64 = 1 << 30;

/// Run "old" (10 days ago): 2 GiB caches + 1 GiB trash, no run line (an older
/// cleaner). Run "new" (2 days ago): 1 GiB artifacts, measured 5 GiB.
fn write_stats_log(env: &Env) -> i64 {
    let now = jiff::Timestamp::now().as_second();
    let path = log_file(env);
    let old = now - 10 * 86_400;
    cleaner::log::append(
        &path,
        &[
            log_line("old", old, "caches", 2 * GIB),
            log_line("old", old, "trash", GIB),
        ],
        None,
    )
    .unwrap();
    let new = now - 2 * 86_400;
    let run = cleaner::log::RunLine {
        kind: "run".into(),
        time: new,
        run_id: "new".into(),
        freed_by_fs: vec![cleaner::log::FsFreed {
            fs: "/home".into(),
            bytes: 5 * GIB as i64,
        }],
    };
    cleaner::log::append(&path, &[log_line("new", new, "artifacts", GIB)], Some(&run)).unwrap();
    now
}

#[test]
fn stats_without_log_says_nothing_freed() {
    let env = Env::new();
    env.cmd(&["stats"])
        .assert()
        .success()
        .stdout(contains("Nothing freed yet"));
    assert!(!log_file(&env).exists());
}

#[test]
fn stats_marks_measured_and_estimated() {
    let env = Env::new();
    write_stats_log(&env);
    let out = env.cmd(&["stats"]).assert().success();
    let text = String::from_utf8(out.get_output().stdout.clone()).unwrap();
    let row = |label: &str| {
        text.lines()
            .find(|l| l.trim_start().starts_with(label))
            .unwrap_or_else(|| panic!("no {label:?} row in:\n{text}"))
            .to_string()
    };
    let all = row("All time");
    assert!(all.contains("8.0 GiB"), "{text}");
    // The measured run freed 5 GiB but its items add up to 1 GiB: more than
    // 20% apart, so the estimate is shown next to the measurement.
    assert!(
        all.ends_with("(5.0 GiB measured, 3.0 GiB estimated; estimate 4.0 GiB)"),
        "{text}"
    );
    assert!(row("Last 30 days").contains("8.0 GiB"), "{text}");
    assert!(row("Runs").contains('2'), "{text}");
    assert!(
        row("Biggest run").ends_with("5.0 GiB (measured; estimate 1.0 GiB)"),
        "{text}"
    );
    assert!(text.contains("Top categories (estimated"), "{text}");
    assert!(row("Tool caches").contains("2.0 GiB"), "{text}");
    assert!(
        text.contains("measured:") && text.contains("estimated:"),
        "{text}"
    );
}

#[test]
fn stats_json() {
    let env = Env::new();
    write_stats_log(&env);
    let out = env.cmd(&["stats", "--json"]).assert().success();
    let v: serde_json::Value = serde_json::from_slice(&out.get_output().stdout).unwrap();
    assert_eq!(v["runs"], 2, "{v}");
    assert_eq!(v["all_time"]["bytes"], 8 * GIB, "{v}");
    assert_eq!(v["all_time"]["measured"], 5 * GIB, "{v}");
    assert_eq!(v["all_time"]["estimate_only"], 3 * GIB, "{v}");
    assert_eq!(v["all_time"]["estimated"], 4 * GIB, "{v}");
    assert_eq!(v["per_run"][1]["run_id"], "new", "{v}");
    assert_eq!(v["per_run"][1]["measured"], 5 * GIB, "{v}");
    assert_eq!(v["per_run"][1]["estimated"], GIB, "{v}");
    assert_eq!(v["biggest_run"]["estimated"], GIB, "{v}");
    assert_eq!(v["biggest_run"]["run_id"], "new", "{v}");
    assert_eq!(v["biggest_run"]["basis"], "measured", "{v}");
    assert_eq!(v["categories"][0]["category"], "caches", "{v}");
    assert_eq!(v["since"], serde_json::Value::Null, "{v}");
}

#[test]
fn stats_since_date() {
    let env = Env::new();
    let now = write_stats_log(&env);
    let five_days_ago = jiff::Timestamp::from_second(now - 5 * 86_400)
        .unwrap()
        .to_zoned(jiff::tz::TimeZone::UTC)
        .date()
        .to_string();
    let out = env
        .cmd(&["stats", "--since", &five_days_ago])
        .assert()
        .success();
    let text = String::from_utf8(out.get_output().stdout.clone()).unwrap();
    let since_row = format!("Since {five_days_ago}");
    assert!(
        text.lines().any(|l| l.trim_start().starts_with(&since_row)
            && l.contains("5.0 GiB")
            && l.ends_with("(measured; estimate 1.0 GiB)")),
        "{text}"
    );
    assert!(!text.contains("All time"), "{text}");
    assert!(!text.contains("Tool caches"), "{text}");
}

#[test]
fn stats_hides_an_estimate_within_20_percent_of_the_measurement() {
    let env = Env::new();
    let t = jiff::Timestamp::now().as_second() - 86_400;
    let run = cleaner::log::RunLine {
        kind: "run".into(),
        time: t,
        run_id: "r".into(),
        freed_by_fs: vec![cleaner::log::FsFreed {
            fs: "/home".into(),
            bytes: (11 * GIB / 10) as i64,
        }],
    };
    let lines = [log_line("r", t, "artifacts", GIB)];
    cleaner::log::append(&log_file(&env), &lines, Some(&run)).unwrap();
    let out = env.cmd(&["stats"]).assert().success();
    let text = String::from_utf8(out.get_output().stdout.clone()).unwrap();
    assert!(
        text.lines()
            .any(|l| l.trim_start().starts_with("All time") && l.ends_with("1.1 GiB  (measured)")),
        "{text}"
    );
    assert!(text.contains("Biggest run:"), "{text}");
    assert!(!text.contains("estimate 1.0 GiB"), "{text}");
}

#[test]
fn stats_rejects_bad_since() {
    let env = Env::new();
    env.cmd(&["stats", "--since", "last week"])
        .assert()
        .failure()
        .stderr(contains("YYYY-MM-DD"));
}

impl Env {
    /// Turns the holding area on in config.toml.
    fn enable_hold(&self) {
        let p = self.home().join(".config/cleaner/config.toml");
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        std::fs::write(&p, "[hold]\nenabled = true\ndays = 7\n").unwrap();
    }
}

#[test]
fn cli_apply_prompt_says_what_is_held() {
    let env = Env::new();
    env.enable_hold();
    env.plan_and_show();
    env.cmd(&["apply", "--only", "artifacts"])
        .write_stdin("n\n")
        .assert()
        .code(1)
        .stdout(contains("Will be held for 7 days"))
        .stdout(contains("[artifacts] "))
        .stdout(contains("Clean 1 item"));
    assert!(env.artifact().exists());
}

#[test]
fn cli_review_confirmation_says_what_is_held() {
    let env = Env::new();
    env.enable_hold();
    env.plan();
    // Enter opens the confirmation, n goes back, q leaves: nothing applied.
    let (code, out) = run_in_pty(&env, &["review"], "sort:", b"\rnq");
    assert_eq!(code, 1, "{out}");
    assert!(out.contains("holding"), "no hold text in:\n{out}");
    assert!(!out.contains("undone"), "{out}");
    assert!(env.artifact().exists());
}

#[test]
fn cli_hold_list_restore_purge() {
    let env = Env::new();
    env.cmd(&["hold", "list"])
        .assert()
        .success()
        .stdout(contains("Nothing held."));
    env.enable_hold();
    env.plan_and_show();
    env.cmd(&["apply", "--yes", "--only", "artifacts"])
        .assert()
        .success()
        .stdout(contains("held "))
        .stdout(contains("frees nothing until"));
    assert!(!env.artifact().exists());
    env.cmd(&["hold", "list"])
        .assert()
        .success()
        .stdout(contains("~/proj/target"));

    let artifact = env.artifact();
    let path = artifact.to_str().unwrap();
    env.cmd(&["hold", "restore", path])
        .assert()
        .success()
        .stdout(contains("Restored"));
    assert!(env.artifact().join("debug/app").exists());
    // Restoring again: nothing is held for it now.
    env.cmd(&["hold", "restore", path]).assert().failure();
    env.cmd(&["hold", "list"])
        .assert()
        .success()
        .stdout(contains("Nothing held."));

    // Hold it again, then purge everything.
    env.plan_and_show();
    env.cmd(&["apply", "--yes", "--only", "artifacts"])
        .assert()
        .success();
    env.cmd(&["hold", "purge"])
        .assert()
        .success()
        .stdout(contains("Nothing expired"));
    env.cmd(&["hold", "purge", "--all"])
        .write_stdin("n\n")
        .assert()
        .code(1);
    env.cmd(&["hold", "purge", "--all", "--yes"])
        .assert()
        .success()
        .stdout(contains("Purged 1"));
    env.cmd(&["hold", "list"])
        .assert()
        .success()
        .stdout(contains("Nothing held."));
    assert!(!env.artifact().exists());
}

#[test]
fn cli_hold_restore_refuses_overwrite() {
    let env = Env::new();
    env.enable_hold();
    env.plan_and_show();
    env.cmd(&["apply", "--yes", "--only", "artifacts"])
        .assert()
        .success();
    mkfile(&env.artifact().join("new"), 3);
    let artifact = env.artifact();
    env.cmd(&["hold", "restore", artifact.to_str().unwrap()])
        .assert()
        .failure()
        .stderr(contains("exists"));
    assert!(env.artifact().join("new").exists());
}
