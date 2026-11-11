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
        for (f, n) in [("Cargo.toml", 10), ("src/main.rs", 10), ("target/debug/app", 50_000)] {
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
        let home = self.home();
        let mut c = Command::new(env!("CARGO_BIN_EXE_cleaner"));
        c.args(args)
            .env("HOME", &home)
            .env("XDG_CONFIG_HOME", home.join(".config"))
            .env("XDG_STATE_HOME", home.join(".local/state"))
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
        let src = self.plans_dir().join(format!("{}.json", self.plan_ids().last().unwrap()));
        let mut v: serde_json::Value = serde_json::from_slice(&std::fs::read(src).unwrap()).unwrap();
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
    env.cmd(&["--help"]).assert().success().stdout(contains("gui"));
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

#[test]
fn cli_plan_then_show() {
    let env = Env::new();
    env.cmd(&["plan"])
        .assert()
        .success()
        .stdout(contains("artifacts"))
        .stdout(contains("Total selected"));
    assert!(env.home().join(".config/cleaner/config.toml").exists());
    env.cmd(&["show"]).assert().success().stdout(contains("proj")).stdout(contains("[x]"));
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
        .stderr(contains("another cleaner run is in progress"));
    assert!(env.artifact().exists());
    assert!(env.cache_file().exists());
}

#[test]
fn cli_apply_yes_deletes() {
    let env = Env::new();
    env.plan_and_show();
    env.cmd(&["apply", "--yes"]).assert().success();
    assert!(!env.artifact().exists());
    env.cmd(&["log"]).assert().success().stdout(contains("deleted"));
}

#[test]
fn cli_only_filters() {
    let env = Env::new();
    env.plan_and_show();
    env.cmd(&["apply", "--yes", "--only", "caches"]).assert().success();
    assert!(!env.cache_file().exists());
    assert!(env.artifact().exists());
}

#[test]
fn cli_bad_config_exits_nonzero() {
    let env = Env::new();
    mkfile(&env.home().join(".config/cleaner/config.toml"), 0);
    std::fs::write(env.home().join(".config/cleaner/config.toml"), "[artifacts").unwrap();
    env.cmd(&["plan"]).assert().code(1).stderr(contains("config.toml"));
    assert_eq!(plan_count(&env.plans_dir()), 0);
}

#[test]
fn cli_status_json() {
    let env = Env::new();
    env.plan();
    let out = env.cmd(&["status", "--json"]).assert().success().get_output().stdout.clone();
    let v: serde_json::Value = serde_json::from_slice(&out).unwrap();
    assert!(v["filesystems"].is_array(), "{v}");
    assert!(!v["filesystems"].as_array().unwrap().is_empty(), "{v}");
}

#[test]
fn cli_plan_without_tools_skips_cleanly() {
    use predicates::prelude::PredicateBooleanExt;
    let env = Env::new();
    env.cmd(&["plan"]).assert().success().stdout(contains("category failed").not());
    let plan: serde_json::Value = serde_json::from_slice(
        &std::fs::read(env.plans_dir().join(format!("{}.json", env.plan_ids()[0]))).unwrap(),
    )
    .unwrap();
    let skipped = plan["skipped"].as_array().unwrap();
    assert!(
        skipped.iter().all(|s| !s["reason"].as_str().unwrap().contains("category failed")),
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
    let out = env.cmd(&["show"]).assert().success().get_output().stdout.clone();
    let out = String::from_utf8(out).unwrap();
    let line = out.lines().find(|l| l.contains("system: generations")).expect("nix row");
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
        std::fs::write(self.home().join(".config/cleaner/config.toml"), format!("[low_space]\n{low_space}\n"))
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
    env.if_low("percent = 100").assert().success().stderr(contains("notification failed"));
    let ids = env.plan_ids();
    assert_eq!(ids.len(), 1);
    let plan: serde_json::Value =
        serde_json::from_slice(&std::fs::read(env.plans_dir().join(format!("{}.json", ids[0]))).unwrap()).unwrap();
    assert!(
        plan["entries"].as_array().unwrap().iter().any(|e| e["category"] == "artifacts"
            && e["selected"] == true
            && e.to_string().contains("/p/target")),
        "{plan}"
    );
    assert!(env.home().join("p/target/x").exists());
    assert!(!env.home().join(".local/state/cleaner/log.jsonl").exists());
    let st: serde_json::Value = serde_json::from_slice(&std::fs::read(env.space_state()).unwrap()).unwrap();
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
    env.if_low("enabled = false\npercent = 100").assert().success();
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
    let out = env.if_low("percent = 100").assert().success().get_output().clone();
    std::fs::set_permissions(&state_dir, std::fs::Permissions::from_mode(0o700)).unwrap();
    assert!(env.space_state().exists(), "{}", String::from_utf8_lossy(&out.stderr));
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
    for (i, (title, bytes)) in [("bravo", 1024), ("alpha", 4096), ("charlie", 2048)].iter().enumerate() {
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
            .map(|t| (out.find(&format!("  {t}  ")).unwrap_or_else(|| panic!("no {t} in {out}")), t.to_string()))
            .collect();
        found.sort();
        found.into_iter().map(|(_, t)| t).collect()
    };
    assert_eq!(order(&["show"]), ["alpha", "charlie", "bravo"]);
    assert_eq!(order(&["show", "--sort", "name"]), ["alpha", "bravo", "charlie"]);
    env.cmd(&["show", "--sort", "sideways"]).assert().code(2);
}
