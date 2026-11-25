//! Old toolchains and runtimes: rustup, nvm, pyenv, conda and Flatpak.
//! Every tool is a `FakeRunner` reply; nothing real is run.

mod common;

use cleaner::category::caches::Caches;
use cleaner::category::toolchains::{ToolEnv, Toolchains};
use cleaner::category::{Category, Outcome, ScanCtx, ScanOutput};
use cleaner::config::Config;
use cleaner::engine::registry;
use cleaner::fsutil::measure::InodeSet;
use cleaner::object::{Object, Operation};
use cleaner::plan::{Entry, EntryKind};
use cleaner::protect::Protector;
use common::{FakeRunner, err_out, mkfile, ok_out};
use std::ffi::OsString;
use std::fs;
use std::path::{Path, PathBuf};
use tempfile::TempDir;

const HOST: &str = "x86_64-unknown-linux-gnu";

struct Fx {
    tmp: TempDir,
    config: Config,
    env: ToolEnv,
}

impl Fx {
    fn new() -> Fx {
        Fx {
            tmp: tempfile::tempdir().unwrap(),
            config: Config::default(),
            env: ToolEnv::default(),
        }
    }

    fn home(&self) -> &Path {
        self.tmp.path()
    }

    fn with<R>(&self, tools: &FakeRunner, f: impl FnOnce(&ScanCtx, &Toolchains) -> R) -> R {
        let protector = Protector::new(self.home(), &self.config.protect);
        let ctx = ScanCtx {
            home: self.home(),
            now: common::now(),
            config: &self.config,
            protector: &protector,
            tools,
            euid: common::my_uid(),
            usage: None,
            hold: None,
        };
        // A fresh category per call, as the engine builds one per run.
        f(&ctx, &Toolchains::new(self.env.clone()))
    }

    fn scan(&self, tools: &FakeRunner) -> ScanOutput {
        self.with(tools, |ctx, c| {
            c.scan(ctx, &mut InodeSet::default()).unwrap()
        })
    }

    fn apply(&self, tools: &FakeRunner, e: &Entry) -> Outcome {
        self.with(tools, |ctx, c| c.apply(e, ctx))
    }
}

fn tc(name: &str) -> String {
    format!("{name}-{HOST}")
}

/// A toolchain dir with one file in it.
fn toolchain(rustup_home: &Path, name: &str) -> PathBuf {
    let dir = rustup_home.join("toolchains").join(name);
    mkfile(&dir.join("lib/librustc_driver.so"), 8192);
    dir
}

fn rustup_names(out: &ScanOutput) -> Vec<&str> {
    out.entries
        .iter()
        .filter_map(|e| match &e.kind {
            EntryKind::Object {
                obj: Object::RustupToolchain { name, .. },
            } => Some(name.as_str()),
            _ => None,
        })
        .collect()
}

fn item_paths(out: &ScanOutput) -> Vec<PathBuf> {
    out.entries
        .iter()
        .flat_map(|e| match &e.kind {
            EntryKind::Item { targets } => targets.iter().map(|t| t.path.clone()).collect(),
            _ => Vec::new(),
        })
        .collect()
}

fn flatpak_refs(out: &ScanOutput) -> Vec<&str> {
    out.entries
        .iter()
        .filter_map(|e| match &e.kind {
            EntryKind::Object {
                obj: Object::Flatpak { flatpak_ref },
            } => Some(flatpak_ref.as_str()),
            _ => None,
        })
        .collect()
}

fn is_conda_op(e: &Entry) -> bool {
    matches!(
        e.kind,
        EntryKind::Operation {
            op: Operation::CondaClean
        }
    )
}

fn entry_for(out: &ScanOutput, pred: impl Fn(&Entry) -> bool) -> &Entry {
    out.entries
        .iter()
        .find(|e| pred(e))
        .unwrap_or_else(|| panic!("no such entry in {:#?}", out.entries))
}

fn has_rustup(name: &str) -> impl Fn(&Entry) -> bool + '_ {
    move |e| matches!(&e.kind, EntryKind::Object { obj: Object::RustupToolchain { name: n, .. } } if n == name)
}

fn targets(e: &Entry) -> Vec<PathBuf> {
    match &e.kind {
        EntryKind::Item { targets } => targets.iter().map(|t| t.path.clone()).collect(),
        k => panic!("not an item: {k:?}"),
    }
}

// ---------------------------------------------------------------- general

#[test]
fn nothing_installed_is_silent() {
    let fx = Fx::new();
    let out = fx.scan(&FakeRunner::new());
    assert!(out.entries.is_empty(), "{:#?}", out.entries);
    assert!(out.skipped.is_empty(), "{:#?}", out.skipped);
    assert!(out.report_only.is_empty(), "{:#?}", out.report_only);
}

#[test]
fn env_vars_must_be_absolute() {
    let env = ToolEnv::from_vars(|name| {
        let v = match name {
            "RUSTUP_HOME" => "/opt/rustup",
            "CARGO_HOME" => "/opt/cargo",
            "NVM_DIR" => "relative/nvm",
            "PYENV_ROOT" => "",
            _ => return None,
        };
        Some(OsString::from(v))
    });
    assert_eq!(env.rustup_home, Some(PathBuf::from("/opt/rustup")));
    assert_eq!(env.cargo_home, Some(PathBuf::from("/opt/cargo")));
    assert_eq!(env.nvm_dir, None);
    assert_eq!(env.pyenv_root, None);
}

#[test]
fn registered_after_ai_models_and_disable_able() {
    let ids = |c: &Config| -> Vec<&'static str> { registry(c).iter().map(|c| c.id()).collect() };
    let all = ids(&Config::default());
    let at = all
        .iter()
        .position(|id| *id == "toolchains")
        .expect("registered");
    assert_eq!(all[at - 1], "ai_models", "app_caches, ide_versions, ai_models, toolchains");
    let mut c = Config::default();
    c.toolchains.enabled = false;
    assert!(!ids(&c).contains(&"toolchains"));
    assert_eq!(
        cleaner::view::category_label("toolchains"),
        "Toolchains and runtimes"
    );
}

// ---------------------------------------------------------------- rustup

fn rustup_list(lines: &[&str]) -> FakeRunner {
    FakeRunner::new().respond("rustup toolchain list", ok_out(&(lines.join("\n") + "\n")))
}

#[test]
fn rustup_offers_unused_toolchains_only() {
    let fx = Fx::new();
    let rh = fx.home().join(".rustup");
    for n in ["stable", "beta", "nightly-2024-01-01", "1.70.0", "1.75.0"] {
        toolchain(&rh, &tc(n));
    }
    fs::create_dir_all(fx.home().join("linked")).unwrap();
    std::os::unix::fs::symlink(fx.home().join("linked"), rh.join("toolchains/foo")).unwrap();
    let tools = rustup_list(&[
        &format!("{} (active, default)", tc("stable")),
        &tc("beta"),
        &tc("nightly-2024-01-01"),
        &tc("1.70.0"),
        &tc("1.75.0"),
        "foo",
    ]);
    let out = fx.scan(&tools);
    let mut names = rustup_names(&out);
    names.sort();
    // Not the default, not the newest numbered stable, not a linked toolchain.
    assert_eq!(
        names,
        [tc("1.70.0"), tc("beta"), tc("nightly-2024-01-01")],
        "{:#?}",
        out.entries
    );
    let e = entry_for(&out, has_rustup(&tc("1.70.0")));
    assert!(!e.selected, "toolchains are never pre-selected");
    assert!(!e.permanent);
    assert!(e.bytes > 0);
    assert_eq!(e.category, "toolchains");
    match &e.kind {
        EntryKind::Object {
            obj: Object::RustupToolchain { path, .. },
        } => assert_eq!(path, &rh.join("toolchains").join(tc("1.70.0"))),
        k => panic!("{k:?}"),
    }
    assert!(out.skipped.is_empty(), "{:#?}", out.skipped);
}

#[test]
fn rustup_default_is_never_offered() {
    let fx = Fx::new();
    let rh = fx.home().join(".rustup");
    for n in ["nightly-2023-01-01", "1.60.0", "1.70.0"] {
        toolchain(&rh, &tc(n));
    }
    // Older rustup prints only "(default)".
    let tools = rustup_list(&[
        &format!("{} (default)", tc("nightly-2023-01-01")),
        &tc("1.60.0"),
        &tc("1.70.0"),
    ]);
    assert_eq!(rustup_names(&fx.scan(&tools)), [tc("1.60.0")]);
}

#[test]
fn rustup_active_override_is_kept() {
    let fx = Fx::new();
    let rh = fx.home().join(".rustup");
    for n in ["stable", "1.60.0", "1.65.0", "1.70.0"] {
        toolchain(&rh, &tc(n));
    }
    let tools = rustup_list(&[
        &format!("{} (default)", tc("stable")),
        &format!("{} (active)", tc("1.60.0")),
        &tc("1.65.0"),
        &tc("1.70.0"),
    ]);
    assert_eq!(rustup_names(&fx.scan(&tools)), [tc("1.65.0")]);
}

#[test]
fn rustup_single_toolchain_offers_nothing() {
    let fx = Fx::new();
    toolchain(&fx.home().join(".rustup"), &tc("stable"));
    let out = fx.scan(&rustup_list(&[&format!(
        "{} (active, default)",
        tc("stable")
    )]));
    assert!(out.entries.is_empty(), "{:#?}", out.entries);
    assert!(out.report_only.is_empty());
}

#[test]
fn rustup_no_installed_toolchains() {
    let fx = Fx::new();
    let out = fx.scan(&rustup_list(&["no installed toolchains"]));
    assert!(out.entries.is_empty());
    assert!(out.skipped.is_empty(), "{:#?}", out.skipped);
}

#[test]
fn rustup_honours_rustup_home() {
    let mut fx = Fx::new();
    let rh = fx.home().join("custom/rustup");
    fx.env.rustup_home = Some(rh.clone());
    toolchain(&rh, &tc("stable"));
    toolchain(&rh, &tc("1.60.0"));
    toolchain(&rh, &tc("1.70.0"));
    let tools = rustup_list(&[
        &format!("{} (default)", tc("stable")),
        &tc("1.60.0"),
        &tc("1.70.0"),
    ]);
    let out = fx.scan(&tools);
    let e = entry_for(&out, has_rustup(&tc("1.60.0")));
    match &e.kind {
        EntryKind::Object {
            obj: Object::RustupToolchain { path, .. },
        } => assert_eq!(path, &rh.join("toolchains").join(tc("1.60.0"))),
        k => panic!("{k:?}"),
    }
    assert!(e.bytes > 0, "sized from RUSTUP_HOME, not ~/.rustup");
}

#[test]
fn rustup_pins_are_kept() {
    let fx = Fx::new();
    let rh = fx.home().join(".rustup");
    for n in ["stable", "beta", "nightly-2024-01-01", "1.60.0", "1.70.0"] {
        toolchain(&rh, &tc(n));
    }
    mkfile(&fx.home().join("src/p/Cargo.toml"), 10);
    fs::write(
        fx.home().join("src/p/rust-toolchain.toml"),
        "[toolchain]\nchannel = \"1.70.0\"\ncomponents = [\"clippy\"]\n",
    )
    .unwrap();
    mkfile(&fx.home().join("work/q/Cargo.toml"), 10);
    fs::write(
        fx.home().join("work/q/rust-toolchain"),
        "nightly-2024-01-01\n",
    )
    .unwrap();
    // `rustup override set beta` in some directory.
    fs::write(
        rh.join("settings.toml"),
        format!(
            "version = \"12\"\ndefault_toolchain = \"{}\"\n\n[overrides]\n\"/somewhere\" = \"{}\"\n",
            tc("stable"),
            tc("beta")
        ),
    )
    .unwrap();
    let tools = rustup_list(&[
        &format!("{} (default)", tc("stable")),
        &tc("beta"),
        &tc("nightly-2024-01-01"),
        &tc("1.60.0"),
        &tc("1.70.0"),
    ]);
    // 1.70.0 is pinned, so 1.60.0 is the only one left (1.70.0 would be
    // kept as the newest numbered stable anyway; 1.60.0 is not).
    assert_eq!(rustup_names(&fx.scan(&tools)), [tc("1.60.0")]);
}

#[test]
fn rustup_found_in_cargo_home_when_not_on_path() {
    let mut fx = Fx::new();
    let cargo = fx.home().join("cargo-home");
    fx.env.cargo_home = Some(cargo.clone());
    mkfile(&cargo.join("bin/rustup"), 10);
    let rh = fx.home().join(".rustup");
    toolchain(&rh, &tc("stable"));
    toolchain(&rh, &tc("1.60.0"));
    toolchain(&rh, &tc("1.70.0"));
    let prog = cargo.join("bin/rustup").display().to_string();
    let tools = FakeRunner::new().respond(
        &format!("{prog} toolchain list"),
        ok_out(&format!(
            "{} (default)\n{}\n{}\n",
            tc("stable"),
            tc("1.60.0"),
            tc("1.70.0")
        )),
    );
    assert_eq!(rustup_names(&fx.scan(&tools)), [tc("1.60.0")]);
}

#[test]
fn rustup_missing_reports_toolchain_dirs_only() {
    let fx = Fx::new();
    toolchain(&fx.home().join(".rustup"), &tc("stable"));
    let out = fx.scan(&FakeRunner::new());
    assert!(out.entries.is_empty());
    assert!(out.skipped.is_empty(), "{:#?}", out.skipped);
    assert_eq!(out.report_only.len(), 1, "{:#?}", out.report_only);
    assert!(
        out.report_only[0]
            .reason
            .contains("rustup is not installed")
    );
}

#[test]
fn rustup_list_failure_is_a_skip() {
    let fx = Fx::new();
    toolchain(&fx.home().join(".rustup"), &tc("stable"));
    let tools = FakeRunner::new().respond("rustup toolchain list", err_out(1, "error: boom\n"));
    let out = fx.scan(&tools);
    assert!(out.entries.is_empty());
    assert_eq!(out.skipped.len(), 1, "{:#?}", out.skipped);
    assert!(out.skipped[0].reason.contains("boom"), "{:?}", out.skipped);
}

#[test]
fn caches_leaves_rustup_dirs_to_this_category() {
    let fx = Fx::new();
    toolchain(&fx.home().join(".rustup"), &tc("stable"));
    let caches_report = |config: &Config| {
        let protector = Protector::new(fx.home(), &config.protect);
        let tools = FakeRunner::new();
        let ctx = ScanCtx {
            home: fx.home(),
            now: common::now(),
            config,
            protector: &protector,
            tools: &tools,
            euid: common::my_uid(),
            usage: None,
            hold: None,
        };
        Caches
            .scan(&ctx, &mut InodeSet::default())
            .unwrap()
            .report_only
            .len()
    };
    let mut c = Config::default();
    c.caches.uv_prune = false;
    c.caches.go_clean = false;
    assert_eq!(caches_report(&c), 0, "toolchains owns ~/.rustup");
    c.toolchains.rustup = false;
    assert_eq!(
        caches_report(&c),
        1,
        "old report-only row when rustup is off"
    );
}

fn planned_nightly(fx: &Fx) -> (Entry, String) {
    let rh = fx.home().join(".rustup");
    toolchain(&rh, &tc("stable"));
    toolchain(&rh, &tc("nightly-2024-01-01"));
    let list = format!("{} (default)\n{}\n", tc("stable"), tc("nightly-2024-01-01"));
    let out = fx.scan(&FakeRunner::new().respond("rustup toolchain list", ok_out(&list)));
    let e = entry_for(&out, has_rustup(&tc("nightly-2024-01-01"))).clone();
    (e, list)
}

#[test]
fn rustup_apply_uninstalls_by_name() {
    let fx = Fx::new();
    let (e, list) = planned_nightly(&fx);
    let tools = FakeRunner::new()
        .respond("rustup toolchain list", ok_out(&list))
        .respond("rustup toolchain uninstall", ok_out(""));
    match fx.apply(&tools, &e) {
        Outcome::Deleted { bytes, failures } => {
            assert!(bytes > 0);
            assert!(failures.is_empty(), "{failures:?}");
        }
        o => panic!("{o:?}"),
    }
    assert!(
        tools.calls().contains(&format!(
            "rustup toolchain uninstall {}",
            tc("nightly-2024-01-01")
        )),
        "{:?}",
        tools.calls()
    );
}

#[test]
fn rustup_apply_skips_a_toolchain_that_became_default() {
    let fx = Fx::new();
    let (e, _) = planned_nightly(&fx);
    let tools = FakeRunner::new()
        .respond(
            "rustup toolchain list",
            ok_out(&format!(
                "{}\n{} (active, default)\n",
                tc("stable"),
                tc("nightly-2024-01-01")
            )),
        )
        .respond("rustup toolchain uninstall", ok_out(""));
    match fx.apply(&tools, &e) {
        Outcome::Skipped { reason } => assert!(reason.contains("default"), "{reason}"),
        o => panic!("{o:?}"),
    }
    assert!(!tools.calls().iter().any(|c| c.contains("uninstall")));
}

#[test]
fn rustup_apply_skips_a_toolchain_pinned_since_plan() {
    let fx = Fx::new();
    let (e, list) = planned_nightly(&fx);
    mkfile(&fx.home().join("src/p/Cargo.toml"), 10);
    fs::write(
        fx.home().join("src/p/rust-toolchain.toml"),
        "[toolchain]\nchannel = \"nightly-2024-01-01\"\n",
    )
    .unwrap();
    let tools = FakeRunner::new()
        .respond("rustup toolchain list", ok_out(&list))
        .respond("rustup toolchain uninstall", ok_out(""));
    assert!(matches!(fx.apply(&tools, &e), Outcome::Skipped { .. }));
    assert!(!tools.calls().iter().any(|c| c.contains("uninstall")));
}

#[test]
fn rustup_apply_skips_a_toolchain_already_gone() {
    let fx = Fx::new();
    let (e, _) = planned_nightly(&fx);
    let tools = FakeRunner::new()
        .respond(
            "rustup toolchain list",
            ok_out(&format!("{} (default)\n", tc("stable"))),
        )
        .respond("rustup toolchain uninstall", ok_out(""));
    match fx.apply(&tools, &e) {
        Outcome::Skipped { reason } => assert!(reason.contains("no longer installed"), "{reason}"),
        o => panic!("{o:?}"),
    }
    assert!(!tools.calls().iter().any(|c| c.contains("uninstall")));
}

#[test]
fn rustup_apply_without_rustup_skips() {
    let fx = Fx::new();
    let (e, _) = planned_nightly(&fx);
    assert!(matches!(
        fx.apply(&FakeRunner::new(), &e),
        Outcome::Skipped { .. }
    ));
}

// ---------------------------------------------------------------- nvm

fn node(nvm: &Path, v: &str) -> PathBuf {
    let dir = nvm.join("versions/node").join(v);
    mkfile(&dir.join("bin/node"), 4096);
    dir
}

#[test]
fn nvm_keeps_default_alias_and_newest() {
    let fx = Fx::new();
    let nvm = fx.home().join(".nvm");
    for v in ["v16.20.2", "v18.19.0", "v18.20.4", "v20.11.0", "v22.1.0"] {
        node(&nvm, v);
    }
    mkfile(&nvm.join("alias/default"), 0);
    fs::write(nvm.join("alias/default"), "18\n").unwrap();
    let out = fx.scan(&FakeRunner::new());
    let mut paths = item_paths(&out);
    paths.sort();
    // "18" resolves to the newest v18; v22.1.0 is the newest overall.
    assert_eq!(
        paths,
        ["v16.20.2", "v18.19.0", "v20.11.0"].map(|v| nvm.join("versions/node").join(v))
    );
    for e in &out.entries {
        assert!(!e.selected, "nvm versions are never pre-selected");
        assert!(e.permanent, "global npm packages go with the version");
        assert!(e.bytes > 0);
    }
}

#[test]
fn nvm_follows_lts_alias_chain() {
    let fx = Fx::new();
    let nvm = fx.home().join(".nvm");
    for v in ["v18.0.0", "v20.11.0", "v21.0.0"] {
        node(&nvm, v);
    }
    mkfile(&nvm.join("alias/lts/iron"), 0);
    fs::write(nvm.join("alias/default"), "lts/*\n").unwrap();
    fs::write(nvm.join("alias/lts/*"), "lts/iron\n").unwrap();
    fs::write(nvm.join("alias/lts/iron"), "v20.11.0\n").unwrap();
    assert_eq!(
        item_paths(&fx.scan(&FakeRunner::new())),
        [nvm.join("versions/node/v18.0.0")]
    );
}

#[test]
fn nvm_unresolvable_default_offers_nothing() {
    let fx = Fx::new();
    let nvm = fx.home().join(".nvm");
    for v in ["v18.0.0", "v20.11.0", "v21.0.0"] {
        node(&nvm, v);
    }
    mkfile(&nvm.join("alias/default"), 0);
    fs::write(nvm.join("alias/default"), "lts/*\n").unwrap();
    let out = fx.scan(&FakeRunner::new());
    assert!(out.entries.is_empty(), "{:#?}", out.entries);
    assert_eq!(out.skipped.len(), 1, "{:#?}", out.skipped);
    assert!(out.skipped[0].reason.contains("lts/*"), "{:?}", out.skipped);
}

#[test]
fn nvm_single_version_offers_nothing() {
    let fx = Fx::new();
    node(&fx.home().join(".nvm"), "v20.11.0");
    let out = fx.scan(&FakeRunner::new());
    assert!(out.entries.is_empty(), "{:#?}", out.entries);
    assert!(out.skipped.is_empty());
}

#[test]
fn nvm_honours_nvm_dir() {
    let mut fx = Fx::new();
    let nvm = fx.home().join(".config/nvm");
    fx.env.nvm_dir = Some(nvm.clone());
    node(&nvm, "v18.0.0");
    node(&nvm, "v20.0.0");
    node(&fx.home().join(".nvm"), "v10.0.0");
    node(&fx.home().join(".nvm"), "v12.0.0");
    assert_eq!(
        item_paths(&fx.scan(&FakeRunner::new())),
        [nvm.join("versions/node/v18.0.0")]
    );
}

#[test]
fn nvm_apply_deletes_and_rechecks_default() {
    let fx = Fx::new();
    let nvm = fx.home().join(".nvm");
    for v in ["v16.0.0", "v18.0.0", "v20.0.0"] {
        node(&nvm, v);
    }
    let out = fx.scan(&FakeRunner::new());
    let pick = |v: &str| entry_for(&out, |e| targets(e) == [nvm.join("versions/node").join(v)]);
    let (v16, v18) = (pick("v16.0.0").clone(), pick("v18.0.0").clone());

    mkfile(&nvm.join("alias/default"), 0);
    fs::write(nvm.join("alias/default"), "v16.0.0\n").unwrap();
    match fx.apply(&FakeRunner::new(), &v16) {
        Outcome::Skipped { reason } => assert!(reason.contains("default"), "{reason}"),
        o => panic!("{o:?}"),
    }
    assert!(nvm.join("versions/node/v16.0.0").exists());

    assert!(matches!(
        fx.apply(&FakeRunner::new(), &v18),
        Outcome::Deleted { .. }
    ));
    assert!(!nvm.join("versions/node/v18.0.0").exists());
}

// ---------------------------------------------------------------- pyenv

fn python(pyenv: &Path, v: &str) -> PathBuf {
    let dir = pyenv.join("versions").join(v);
    mkfile(&dir.join("bin/python"), 4096);
    dir
}

#[test]
fn pyenv_keeps_global_newest_pins_and_virtualenv_bases() {
    let fx = Fx::new();
    let py = fx.home().join(".pyenv");
    for v in [
        "3.8.18",
        "3.9.19",
        "3.10.14",
        "3.11.8",
        "3.11.9",
        "3.12.4",
        "3.13.0rc1",
        "pypy3.10-7.3.15",
    ] {
        python(&py, v);
    }
    // pyenv-virtualenv: the env lives under its base, with a link beside it.
    mkfile(&py.join("versions/3.9.19/envs/myenv/bin/python"), 10);
    std::os::unix::fs::symlink(
        py.join("versions/3.9.19/envs/myenv"),
        py.join("versions/myenv"),
    )
    .unwrap();
    fs::write(py.join("version"), "3.10.14\n").unwrap();
    mkfile(&fx.home().join("src/p/pyproject.toml"), 10);
    fs::write(fx.home().join("src/p/.python-version"), "3.11\n").unwrap();

    let out = fx.scan(&FakeRunner::new());
    let mut paths = item_paths(&out);
    paths.sort();
    // 3.10.14 global, 3.11.9 is what "3.11" resolves to, 3.12.4 newest,
    // 3.9.19 has a virtualenv; pre-releases and other flavours are left alone.
    assert_eq!(
        paths,
        ["3.11.8", "3.8.18"].map(|v| py.join("versions").join(v))
    );
    for e in &out.entries {
        assert!(!e.selected, "pyenv versions are never pre-selected");
        assert!(e.permanent);
    }
    assert_eq!(out.skipped.len(), 1, "{:#?}", out.skipped);
    assert!(out.skipped[0].reason.contains("virtualenv"));
}

#[test]
fn pyenv_single_version_offers_nothing() {
    let fx = Fx::new();
    python(&fx.home().join(".pyenv"), "3.12.4");
    assert!(fx.scan(&FakeRunner::new()).entries.is_empty());
}

#[test]
fn pyenv_honours_pyenv_root() {
    let mut fx = Fx::new();
    let py = fx.home().join("opt/pyenv");
    fx.env.pyenv_root = Some(py.clone());
    python(&py, "3.10.1");
    python(&py, "3.12.1");
    python(&fx.home().join(".pyenv"), "3.6.0");
    python(&fx.home().join(".pyenv"), "3.7.0");
    assert_eq!(
        item_paths(&fx.scan(&FakeRunner::new())),
        [py.join("versions/3.10.1")]
    );
}

#[test]
fn pyenv_apply_rechecks_global_and_pins() {
    let fx = Fx::new();
    let py = fx.home().join(".pyenv");
    for v in ["3.8.1", "3.9.1", "3.10.1", "3.12.1"] {
        python(&py, v);
    }
    let out = fx.scan(&FakeRunner::new());
    let pick = |v: &str| entry_for(&out, |e| targets(e) == [py.join("versions").join(v)]).clone();
    let (v38, v39, v310) = (pick("3.8.1"), pick("3.9.1"), pick("3.10.1"));

    fs::write(py.join("version"), "3.8.1\n").unwrap();
    assert!(matches!(
        fx.apply(&FakeRunner::new(), &v38),
        Outcome::Skipped { .. }
    ));
    mkfile(&fx.home().join("src/p/pyproject.toml"), 10);
    fs::write(fx.home().join("src/p/.python-version"), "3.9.1\n").unwrap();
    assert!(matches!(
        fx.apply(&FakeRunner::new(), &v39),
        Outcome::Skipped { .. }
    ));
    assert!(py.join("versions/3.8.1").exists());
    assert!(py.join("versions/3.9.1").exists());

    assert!(matches!(
        fx.apply(&FakeRunner::new(), &v310),
        Outcome::Deleted { .. }
    ));
    assert!(!py.join("versions/3.10.1").exists());
}

// ---------------------------------------------------------------- conda

fn conda_pkgs(pkgs: &Path) {
    mkfile(&pkgs.join("numpy-1.26.4-py312.conda"), 5000);
    mkfile(&pkgs.join("zlib-1.2.13-h5eee18b_0.tar.bz2"), 3000);
    mkfile(&pkgs.join("numpy-1.26.4-py312/info/index.json"), 2000);
    mkfile(&pkgs.join("urls.txt"), 100);
}

#[test]
fn conda_on_path_is_one_operation() {
    let fx = Fx::new();
    conda_pkgs(&fx.home().join("miniconda3/pkgs"));
    let tools = FakeRunner::new().installed("conda", "/usr/bin/conda");
    let out = fx.scan(&tools);
    assert_eq!(out.entries.len(), 1, "{:#?}", out.entries);
    let e = &out.entries[0];
    assert!(is_conda_op(e), "{e:?}");
    assert!(e.selected, "conda decides what is unused");
    assert!(!e.permanent);
    assert!(e.bytes >= 8000, "tarballs counted: {}", e.bytes);
    let conda_calls = |t: &FakeRunner| -> Vec<String> {
        t.calls()
            .into_iter()
            .filter(|c| c.starts_with("conda"))
            .collect()
    };
    assert!(conda_calls(&tools).is_empty(), "nothing runs at plan time");

    let tools = FakeRunner::new()
        .installed("conda", "/usr/bin/conda")
        .respond("conda clean", ok_out("Will remove 2 package(s).\n"));
    assert!(matches!(fx.apply(&tools, e), Outcome::Deleted { .. }));
    assert_eq!(
        conda_calls(&tools),
        ["conda clean --packages --tarballs --yes"]
    );
}

#[test]
fn conda_in_install_root_is_used_off_path() {
    let fx = Fx::new();
    let root = fx.home().join("anaconda3");
    conda_pkgs(&root.join("pkgs"));
    mkfile(&root.join("bin/conda"), 10);
    let out = fx.scan(&FakeRunner::new());
    assert_eq!(out.entries.len(), 1, "{:#?}", out.entries);
    assert!(is_conda_op(&out.entries[0]));
    let prog = root.join("bin/conda").display().to_string();
    let tools = FakeRunner::new().respond(&format!("{prog} clean"), ok_out(""));
    assert!(matches!(
        fx.apply(&tools, &out.entries[0]),
        Outcome::Deleted { .. }
    ));
    assert_eq!(
        tools.calls(),
        [format!("{prog} clean --packages --tarballs --yes")]
    );
}

#[test]
fn conda_apply_skips_when_conda_is_gone() {
    let fx = Fx::new();
    conda_pkgs(&fx.home().join("miniconda3/pkgs"));
    let out = fx.scan(&FakeRunner::new().installed("conda", "/usr/bin/conda"));
    assert!(matches!(
        fx.apply(&FakeRunner::new(), &out.entries[0]),
        Outcome::Skipped { .. }
    ));
}

#[test]
fn conda_failure_is_failed() {
    let fx = Fx::new();
    conda_pkgs(&fx.home().join("miniconda3/pkgs"));
    let tools = FakeRunner::new()
        .installed("conda", "/usr/bin/conda")
        .respond("conda clean", err_out(1, "CondaError: locked\n"));
    let out = fx.scan(&tools);
    match fx.apply(&tools, &out.entries[0]) {
        Outcome::Failed { error } => assert!(error.contains("locked"), "{error}"),
        o => panic!("{o:?}"),
    }
}

#[test]
fn conda_missing_offers_only_tarballs() {
    let fx = Fx::new();
    let a = fx.home().join("miniconda3/pkgs");
    let b = fx.home().join(".conda/pkgs");
    conda_pkgs(&a);
    conda_pkgs(&b);
    let out = fx.scan(&FakeRunner::new());
    assert_eq!(
        out.entries.len(),
        2,
        "one row per pkgs dir: {:#?}",
        out.entries
    );
    for e in &out.entries {
        assert!(e.selected, "tarballs are only a download cache");
        assert!(!e.permanent);
        let mut t = targets(e);
        t.sort();
        let dir = t[0].parent().unwrap().to_path_buf();
        assert_eq!(
            t,
            [
                dir.join("numpy-1.26.4-py312.conda"),
                dir.join("zlib-1.2.13-h5eee18b_0.tar.bz2")
            ]
        );
    }
    let e = entry_for(&out, |e| targets(e)[0].starts_with(&a)).clone();
    assert!(matches!(
        fx.apply(&FakeRunner::new(), &e),
        Outcome::Deleted { .. }
    ));
    assert!(!a.join("zlib-1.2.13-h5eee18b_0.tar.bz2").exists());
    assert!(!a.join("numpy-1.26.4-py312.conda").exists());
    assert!(a.join("numpy-1.26.4-py312/info/index.json").exists());
    assert!(a.join("urls.txt").exists());
}

// ---------------------------------------------------------------- flatpak

const RUNTIMES: &str = "flatpak list --user --runtime --columns=ref,size";
const APPS: &str = "flatpak list --app --columns=ref,runtime";
const PINS: &str = "flatpak pin --user";

const USER_RUNTIMES: &str = "\
org.gnome.Platform/x86_64/45\t1.1 GB
org.gnome.Platform/x86_64/44\t900.0 MB
org.freedesktop.Platform/x86_64/23.08\t500.0 MB
org.freedesktop.Platform.GL.default/x86_64/23.08\t400.0 MB
org.freedesktop.Platform/x86_64/22.08\t450.0 MB
org.freedesktop.Platform.GL.default/x86_64/22.08\t300.0 MB
org.freedesktop.Platform.GL.nvidia-550-78/x86_64/1.4\t350.0 MB
org.gnome.Sdk/x86_64/45\t2.0 GB
org.kde.Platform/x86_64/5.15-23.08\t600.0 MB
com.example.App.Plugin.Foo/x86_64/stable\t10.0 MB
";

const ALL_APPS: &str = "\
org.gnome.Calculator/x86_64/stable\torg.gnome.Platform/x86_64/45
com.example.App/x86_64/stable\torg.freedesktop.Platform/x86_64/23.08
";

fn flatpak(apps: &str) -> FakeRunner {
    FakeRunner::new()
        .respond(RUNTIMES, ok_out(USER_RUNTIMES))
        .respond(APPS, ok_out(apps))
        .respond(PINS, ok_out("runtime/org.gnome.Sdk/x86_64/45\n"))
}

#[test]
fn flatpak_plans_unused_user_runtimes() {
    let fx = Fx::new();
    let out = fx.scan(&flatpak(ALL_APPS));
    let mut refs = flatpak_refs(&out);
    refs.sort();
    assert_eq!(
        refs,
        [
            "runtime/org.freedesktop.Platform.GL.default/x86_64/22.08",
            "runtime/org.freedesktop.Platform/x86_64/22.08",
            "runtime/org.gnome.Platform/x86_64/44",
            "runtime/org.kde.Platform/x86_64/5.15-23.08",
        ],
        "{:#?}",
        out.entries
    );
    let e = entry_for(
        &out,
        |e| matches!(&e.kind, EntryKind::Object { obj: Object::Flatpak { flatpak_ref } } if flatpak_ref.ends_with("Platform/x86_64/44")),
    );
    assert_eq!(e.bytes, 900_000_000);
    assert!(e.selected, "no installed app uses it");
    assert!(!e.permanent);
    assert!(out.skipped.is_empty(), "{:#?}", out.skipped);
}

#[test]
fn flatpak_missing_is_silent() {
    let fx = Fx::new();
    let out = fx.scan(&FakeRunner::new());
    assert!(out.entries.is_empty() && out.skipped.is_empty());
}

#[test]
fn flatpak_unreadable_app_list_offers_nothing() {
    let fx = Fx::new();
    let tools = FakeRunner::new()
        .respond(RUNTIMES, ok_out(USER_RUNTIMES))
        .respond(APPS, err_out(1, "error: oops\n"));
    let out = fx.scan(&tools);
    assert!(out.entries.is_empty(), "{:#?}", out.entries);
    assert_eq!(out.skipped.len(), 1, "{:#?}", out.skipped);
}

fn platform_44(out: &ScanOutput) -> Entry {
    entry_for(out, |e| {
        matches!(&e.kind, EntryKind::Object { obj: Object::Flatpak { flatpak_ref } } if flatpak_ref == "runtime/org.gnome.Platform/x86_64/44")
    })
    .clone()
}

#[test]
fn flatpak_apply_uninstalls_from_user_installation() {
    let fx = Fx::new();
    let e = platform_44(&fx.scan(&flatpak(ALL_APPS)));
    let tools = flatpak(ALL_APPS).respond("flatpak uninstall", ok_out(""));
    assert_eq!(
        fx.apply(&tools, &e),
        Outcome::Deleted {
            bytes: 900_000_000,
            failures: vec![]
        }
    );
    assert!(
        tools.calls().contains(
            &"flatpak uninstall --user --noninteractive runtime/org.gnome.Platform/x86_64/44"
                .to_string()
        ),
        "{:?}",
        tools.calls()
    );
}

#[test]
fn flatpak_apply_skips_a_runtime_now_in_use() {
    let fx = Fx::new();
    let e = platform_44(&fx.scan(&flatpak(ALL_APPS)));
    let apps = format!("{ALL_APPS}org.gnome.Old/x86_64/stable\torg.gnome.Platform/x86_64/44\n");
    let tools = flatpak(&apps).respond("flatpak uninstall", ok_out(""));
    assert!(matches!(fx.apply(&tools, &e), Outcome::Skipped { .. }));
    assert!(!tools.calls().iter().any(|c| c.contains("uninstall")));
}

#[test]
fn flatpak_apply_skips_a_runtime_already_gone() {
    let fx = Fx::new();
    let e = platform_44(&fx.scan(&flatpak(ALL_APPS)));
    let tools = FakeRunner::new()
        .respond(RUNTIMES, ok_out("org.gnome.Platform/x86_64/45\t1.1 GB\n"))
        .respond(APPS, ok_out(ALL_APPS))
        .respond("flatpak uninstall", ok_out(""));
    match fx.apply(&tools, &e) {
        Outcome::Skipped { reason } => assert!(reason.contains("no longer installed"), "{reason}"),
        o => panic!("{o:?}"),
    }
    assert!(!tools.calls().iter().any(|c| c.contains("uninstall")));
}

// ---------------------------------------------------------------- switches

/// Everything installed at once: one rustup, nvm, pyenv, conda and flatpak row.
fn everything(fx: &Fx) -> FakeRunner {
    let rh = fx.home().join(".rustup");
    toolchain(&rh, &tc("stable"));
    toolchain(&rh, &tc("1.60.0"));
    toolchain(&rh, &tc("1.70.0"));
    node(&fx.home().join(".nvm"), "v18.0.0");
    node(&fx.home().join(".nvm"), "v20.0.0");
    python(&fx.home().join(".pyenv"), "3.10.1");
    python(&fx.home().join(".pyenv"), "3.12.1");
    conda_pkgs(&fx.home().join("miniconda3/pkgs"));
    flatpak(ALL_APPS)
        .respond(
            "rustup toolchain list",
            ok_out(&format!(
                "{} (default)\n{}\n{}\n",
                tc("stable"),
                tc("1.60.0"),
                tc("1.70.0")
            )),
        )
        .installed("conda", "/usr/bin/conda")
}

fn tool_of(e: &Entry) -> &'static str {
    match &e.kind {
        EntryKind::Object {
            obj: Object::RustupToolchain { .. },
        } => "rustup",
        EntryKind::Object {
            obj: Object::Flatpak { .. },
        } => "flatpak",
        EntryKind::Operation {
            op: Operation::CondaClean,
        } => "conda",
        EntryKind::Item { targets } if targets[0].path.to_string_lossy().contains("/.nvm/") => {
            "nvm"
        }
        EntryKind::Item { targets } if targets[0].path.to_string_lossy().contains("/.pyenv/") => {
            "pyenv"
        }
        k => panic!("unexpected entry {k:?}"),
    }
}

#[test]
fn each_tool_can_be_switched_off() {
    let mut fx = Fx::new();
    let tools = everything(&fx);
    let mut all: Vec<&str> = fx.scan(&tools).entries.iter().map(tool_of).collect();
    all.sort();
    all.dedup();
    assert_eq!(all, ["conda", "flatpak", "nvm", "pyenv", "rustup"]);

    for off in ["rustup", "nvm", "pyenv", "conda", "flatpak"] {
        fx.config.toolchains = Default::default();
        let t = &mut fx.config.toolchains;
        match off {
            "rustup" => t.rustup = false,
            "nvm" => t.nvm = false,
            "pyenv" => t.pyenv = false,
            "conda" => t.conda = false,
            _ => t.flatpak = false,
        }
        let tools = everything(&fx);
        let out = fx.scan(&tools);
        let mut seen: Vec<&str> = out.entries.iter().map(tool_of).collect();
        seen.sort();
        seen.dedup();
        let rest: Vec<&str> = all.iter().copied().filter(|t| *t != off).collect();
        assert_eq!(seen, rest, "{off} switched off");
        assert!(
            !tools.calls().iter().any(|c| c.starts_with(off)),
            "{off} still queried: {:?}",
            tools.calls()
        );
        assert!(out.report_only.is_empty(), "{off}: {:#?}", out.report_only);
    }

    fx.config.toolchains = Default::default();
    fx.config.toolchains.enabled = false;
    let tools = everything(&fx);
    assert!(fx.scan(&tools).entries.is_empty());
    assert!(tools.calls().is_empty(), "{:?}", tools.calls());
}
