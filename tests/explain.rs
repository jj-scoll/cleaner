//! `cleaner explain PATH`: why a path is or isn't in the plan.

mod common;

use assert_cmd::Command;
use common::{git_commit_all, git_init, mkfile, set_age};
use serde_json::Value;
use std::collections::BTreeMap;
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use tempfile::TempDir;

/// A temp dir holding `home/` and `run/` (XDG_RUNTIME_DIR). Home has a stale
/// Rust project (`proj`), a 12-day-old one (`young`), a symlinked artifact
/// candidate (`linkproj/target`), a `target` with no marker, a git repo with
/// a tracked file, an old and a new Download and a plain document.
struct Env {
    tmp: TempDir,
    bin: PathBuf,
}

impl Env {
    fn new() -> Env {
        let tmp = TempDir::new().unwrap();
        let home = tmp.path().join("home");
        for (dir, days) in [("proj", 40), ("young", 12)] {
            for (f, n) in [
                ("Cargo.toml", 10),
                ("src/main.rs", 10),
                ("target/debug/app", 50_000),
            ] {
                let p = home.join(dir).join(f);
                mkfile(&p, n);
                set_age(&p, days);
            }
        }
        mkfile(&home.join("linkproj/Cargo.toml"), 10);
        set_age(&home.join("linkproj/Cargo.toml"), 40);
        mkfile(&home.join("elsewhere/big"), 10_000);
        std::os::unix::fs::symlink(home.join("elsewhere"), home.join("linkproj/target")).unwrap();
        mkfile(&home.join("nomarker/target/out"), 10_000);
        set_age(&home.join("nomarker/target/out"), 40);
        mkfile(&home.join("Downloads/old.zip"), 20_000);
        set_age(&home.join("Downloads/old.zip"), 100);
        mkfile(&home.join("Downloads/new.zip"), 20_000);
        set_age(&home.join("Downloads/new.zip"), 12);
        mkfile(&home.join("Documents/report.txt"), 100);
        set_age(&home.join("Documents/report.txt"), 400);
        let repo = home.join("repo");
        git_init(&repo);
        mkfile(&repo.join("notes.txt"), 10);
        git_commit_all(&repo);
        std::fs::create_dir_all(tmp.path().join("run")).unwrap();
        let bin = common::git_only_path(tmp.path());
        Env { tmp, bin }
    }

    fn home(&self) -> PathBuf {
        self.tmp.path().join("home")
    }

    fn cmd(&self, args: &[&str]) -> Command {
        let home = self.home();
        let mut c = Command::new(env!("CARGO_BIN_EXE_cleaner"));
        c.args(args)
            .current_dir(&home)
            .env("HOME", &home)
            .env("XDG_CONFIG_HOME", home.join(".config"))
            .env("XDG_STATE_HOME", home.join(".local/state"))
            .env("XDG_RUNTIME_DIR", self.tmp.path().join("run"))
            .env("TZ", "UTC")
            .env("PATH", &self.bin);
        c
    }

    fn write_config(&self, text: &str) {
        mkfile(&self.home().join(".config/cleaner/config.toml"), 0);
        std::fs::write(self.home().join(".config/cleaner/config.toml"), text).unwrap();
    }

    fn plan(&self) {
        self.cmd(&["plan"]).assert().success();
    }

    /// `cleaner explain <path> --json`, parsed.
    fn json(&self, path: &str) -> Value {
        let out = self.cmd(&["explain", path, "--json"]).output().unwrap();
        assert!(out.status.success(), "{out:?}");
        serde_json::from_slice(&out.stdout)
            .unwrap_or_else(|e| panic!("{e}: {}", String::from_utf8_lossy(&out.stdout)))
    }

    /// `cleaner explain <path>` text.
    fn text(&self, path: &str) -> String {
        let out = self.cmd(&["explain", path]).output().unwrap();
        assert!(out.status.success(), "{out:?}");
        String::from_utf8(out.stdout).unwrap()
    }
}

fn s(v: &Value) -> &str {
    v.as_str().unwrap_or_else(|| panic!("not a string: {v}"))
}

#[test]
fn planned_item_shows_entry_and_rule() {
    let env = Env::new();
    env.plan();
    let v = env.json("proj/target");
    assert_eq!(s(&v["status"]), "planned", "{v:#}");
    assert_eq!(s(&v["path"]), "~/proj/target");
    assert_eq!(v["exists"], true);
    let e = &v["entry"];
    assert_eq!(s(&e["category"]), "artifacts");
    assert_eq!(s(&e["title"]), "proj");
    assert_eq!(e["selected"], true);
    assert!(s(&e["reason"]).contains("untouched for 40 days"), "{v:#}");
    assert_eq!(s(&e["rule"]), "rust: target next to Cargo.toml");
    assert!(e["bytes"].as_u64().unwrap() > 0);

    let text = env.text("proj/target");
    for want in [
        "~/proj/target",
        "planned: entry artifacts:",
        "category:     artifacts",
        "pre-selected: yes",
        "rule:         rust: target next to Cargo.toml",
    ] {
        assert!(text.contains(want), "missing {want:?} in:\n{text}");
    }
}

#[test]
fn path_inside_an_item_names_the_item() {
    let env = Env::new();
    env.plan();
    let v = env.json(&env.home().join("proj/target/debug/app").to_string_lossy());
    assert_eq!(s(&v["status"]), "inside_planned", "{v:#}");
    assert_eq!(s(&v["path"]), "~/proj/target/debug/app");
    assert_eq!(s(&v["entry"]["path"]), "~/proj/target");
    let text = env.text("proj/target/debug/app");
    assert!(text.contains("inside planned item ~/proj/target"), "{text}");
}

#[test]
fn skipped_item_prints_the_skip_reason() {
    let env = Env::new();
    env.plan();
    let v = env.json("linkproj/target");
    assert_eq!(s(&v["status"]), "skipped", "{v:#}");
    assert_eq!(s(&v["skip"]["category"]), "artifacts");
    assert_eq!(s(&v["reason"]), "symlink, not followed");
    // The final symlink is reported, not followed.
    assert_eq!(s(&v["path"]), "~/linkproj/target");
    assert_eq!(s(&v["symlink_target"]), "~/elsewhere");
    let text = env.text("linkproj/target");
    assert!(text.contains("skipped by artifacts"), "{text}");
    assert!(text.contains("is a symlink to ~/elsewhere"), "{text}");
}

#[test]
fn protected_paths_say_which_protection() {
    let env = Env::new();
    env.write_config("protect = [\"~/keep\"]\n");
    mkfile(&env.home().join("keep/target/x"), 10);
    mkfile(&env.home().join(".claude/projects/a"), 10);
    env.plan();
    let v = env.json("keep/target");
    assert_eq!(s(&v["status"]), "not_planned", "{v:#}");
    assert_eq!(s(&v["reason"]), "protected path (protect list: ~/keep)");
    let v = env.json(".claude/projects");
    assert_eq!(
        s(&v["reason"]),
        "protected path (built-in protect list: ~/.claude)"
    );
    let v = env.json("repo/.git/HEAD");
    assert_eq!(s(&v["reason"]), "inside .git");
}

#[test]
fn git_tracked_file_is_explained() {
    let env = Env::new();
    env.plan();
    let v = env.json("repo/notes.txt");
    assert_eq!(s(&v["status"]), "not_planned", "{v:#}");
    assert_eq!(s(&v["reason"]), "tracked in git");
}

#[test]
fn too_young_artifact_is_listed_but_not_preselected() {
    let env = Env::new();
    env.plan();
    let v = env.json("young/target");
    assert_eq!(s(&v["status"]), "planned", "{v:#}");
    assert_eq!(v["entry"]["selected"], false);
    assert_eq!(s(&v["reason"]), "untouched for 12 days; rule needs 30");
    let text = env.text("young/target");
    assert!(text.contains("pre-selected: no"), "{text}");
    assert!(
        text.contains("not pre-selected because: untouched for 12 days; rule needs 30"),
        "{text}"
    );
}

#[test]
fn too_young_download_is_not_planned() {
    let env = Env::new();
    env.plan();
    let v = env.json("Downloads/new.zip");
    assert_eq!(s(&v["status"]), "not_planned", "{v:#}");
    assert_eq!(s(&v["category"]), "user_rules");
    assert_eq!(s(&v["reason"]), "untouched for 12 days; rule needs 90");
    // Its store-rule row is in the plan, but the rule doesn't cover it now.
    assert_eq!(s(&v["entry"]["path"]), "~/Downloads");
    let text = env.text("Downloads/new.zip");
    assert!(
        text.contains("inside store-rule row ~/Downloads") && text.contains("rule needs 90"),
        "{text}"
    );
    // The old one is inside the planned store-rule row.
    let v = env.json("Downloads/old.zip");
    assert_eq!(s(&v["status"]), "inside_planned", "{v:#}");
    assert_eq!(s(&v["entry"]["path"]), "~/Downloads");
    assert!(
        s(&v["entry"]["rule"]).contains("older than 90 days"),
        "{v:#}"
    );
}

#[test]
fn disabled_category_is_named() {
    let env = Env::new();
    env.write_config("[artifacts]\nenabled = false\n");
    env.plan();
    let v = env.json("proj/target");
    assert_eq!(s(&v["status"]), "not_planned", "{v:#}");
    assert_eq!(s(&v["category"]), "artifacts");
    assert_eq!(s(&v["reason"]), "category artifacts disabled in config");
}

#[test]
fn symlink_not_in_plan_is_reported_not_followed() {
    let env = Env::new();
    std::os::unix::fs::symlink(
        env.home().join("proj/target"),
        env.home().join("Documents/build"),
    )
    .unwrap();
    env.plan();
    let v = env.json("Documents/build");
    assert_eq!(s(&v["status"]), "not_planned", "{v:#}");
    assert_eq!(s(&v["path"]), "~/Documents/build");
    assert_eq!(s(&v["symlink_target"]), "~/proj/target");
    assert_eq!(s(&v["reason"]), "symlink, not followed");
}

#[test]
fn nonexistent_path() {
    let env = Env::new();
    env.plan();
    let v = env.json("nope/never");
    assert_eq!(s(&v["status"]), "missing", "{v:#}");
    assert_eq!(v["exists"], false);
    assert_eq!(s(&v["path"]), "~/nope/never");
    assert_eq!(s(&v["reason"]), "does not exist");
    let text = env.text("nope/never");
    assert!(text.contains("not planned: does not exist"), "{text}");
}

#[test]
fn artifact_without_marker_and_no_rule() {
    let env = Env::new();
    env.plan();
    let v = env.json("nomarker/target");
    assert_eq!(s(&v["category"]), "artifacts", "{v:#}");
    assert_eq!(
        s(&v["reason"]),
        "artifact without its project marker (Cargo.toml or pom.xml not found)"
    );
    let v = env.json("Documents/report.txt");
    assert_eq!(s(&v["status"]), "not_planned", "{v:#}");
    assert_eq!(v["category"], Value::Null);
    assert_eq!(s(&v["reason"]), "no rule matches");
}

#[test]
fn created_after_the_plan_would_be_planned() {
    let env = Env::new();
    env.plan();
    for f in ["Cargo.toml", "target/x"] {
        let p = env.home().join("later").join(f);
        mkfile(&p, 10);
        set_age(&p, 50);
    }
    let v = env.json("later/target");
    assert_eq!(s(&v["status"]), "would_be_planned", "{v:#}");
    assert_eq!(s(&v["category"]), "artifacts");
}

#[test]
fn json_shape() {
    let env = Env::new();
    env.plan();
    let v = env.json("proj/target");
    let keys: Vec<&str> = v.as_object().unwrap().keys().map(String::as_str).collect();
    let mut want = vec![
        "path",
        "exists",
        "symlink_target",
        "plan",
        "status",
        "category",
        "reason",
        "entry",
        "skip",
    ];
    want.sort();
    let mut got = keys.clone();
    got.sort();
    assert_eq!(got, want);
    let mut entry: Vec<&str> = v["entry"]
        .as_object()
        .unwrap()
        .keys()
        .map(String::as_str)
        .collect();
    entry.sort();
    assert_eq!(
        entry,
        [
            "bytes", "category", "id", "path", "reason", "rule", "selected", "title"
        ]
    );
    assert!(s(&v["plan"]).ends_with('Z'), "{v:#}");
    assert!(s(&v["entry"]["id"]).starts_with("artifacts:"));
    assert_eq!(v["skip"], Value::Null);
    let skip = env.json("linkproj/target");
    let mut keys: Vec<&str> = skip["skip"]
        .as_object()
        .unwrap()
        .keys()
        .map(String::as_str)
        .collect();
    keys.sort();
    assert_eq!(keys, ["category", "reason", "title"]);
}

#[test]
fn explicit_plan_id_and_unknown_plan() {
    let env = Env::new();
    env.plan();
    let id = std::fs::read_dir(env.home().join(".local/state/cleaner/plans"))
        .unwrap()
        .map(|e| e.unwrap().file_name().into_string().unwrap())
        .find_map(|n| n.strip_suffix(".json").map(str::to_string))
        .unwrap();
    let out = env
        .cmd(&["explain", "proj/target", "--plan", &id, "--json"])
        .output()
        .unwrap();
    let v: Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(s(&v["plan"]), id);
    assert_eq!(s(&v["status"]), "planned");
    env.cmd(&["explain", "proj/target", "--plan", "19990101T000000Z"])
        .assert()
        .failure();
}

/// (type, size, mtime ns, mode) of every path under `root`, lstat only.
fn snapshot(root: &Path) -> BTreeMap<PathBuf, (u32, u64, i128, u32)> {
    let mut out = BTreeMap::new();
    let mut stack = vec![root.to_path_buf()];
    while let Some(p) = stack.pop() {
        let md = std::fs::symlink_metadata(&p).unwrap();
        let mtime = md.mtime() as i128 * 1_000_000_000 + md.mtime_nsec() as i128;
        out.insert(
            p.clone(),
            (md.mode() & 0o170000, md.size(), mtime, md.mode()),
        );
        if md.is_dir() {
            for e in std::fs::read_dir(&p).unwrap() {
                stack.push(e.unwrap().path());
            }
        }
    }
    out
}

#[test]
fn explain_changes_nothing_under_home() {
    let env = Env::new();
    let paths = [
        "proj/target",
        "proj/target/debug/app",
        "young/target",
        "linkproj/target",
        "repo/notes.txt",
        "Downloads/new.zip",
        "Documents/report.txt",
        "nope",
    ];
    // Without any plan: no config, plan, log or state may appear.
    let before = snapshot(&env.home());
    for p in paths {
        env.json(p);
        env.text(p);
    }
    assert_eq!(snapshot(&env.home()), before);
    assert!(!env.home().join(".local/state/cleaner").exists());
    assert!(!env.home().join(".config/cleaner").exists());

    // With a plan: still nothing written, nothing deleted.
    env.plan();
    let before = snapshot(&env.home());
    for p in paths {
        env.json(p);
        env.text(p);
    }
    assert_eq!(snapshot(&env.home()), before);
    assert!(env.home().join("proj/target/debug/app").exists());
    assert!(!env.home().join(".local/state/cleaner/log.jsonl").exists());
}
