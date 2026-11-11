//! The notification after a scheduled plan (spec §5.1 step 2). Logic tests use
//! a fake notifier; the CLI test points D-Bus at a socket that doesn't exist.

mod common;

use assert_cmd::Command;
use cleaner::disk::FsUsage;
use cleaner::notify::{
    Notifier, NotifyOutcome, gui_launch_command, low_space_text, notify_if_worth_it, notify_low_space,
};
use cleaner::plan::{Entry, EntryKind, Plan};
use common::{mkfile, set_age};
use predicates::str::contains;
use std::cell::RefCell;
use std::time::Duration;
use tempfile::TempDir;

struct FakeNotifier {
    calls: RefCell<Vec<(String, String)>>,
    urgent: RefCell<Vec<(String, String)>>,
    waits: std::cell::Cell<u32>,
    click: bool,
    fail: bool,
}

impl FakeNotifier {
    fn new(click: bool) -> FakeNotifier {
        FakeNotifier {
            calls: RefCell::new(Vec::new()),
            urgent: RefCell::new(Vec::new()),
            waits: std::cell::Cell::new(0),
            click,
            fail: false,
        }
    }
}

impl Notifier for FakeNotifier {
    fn notify(&self, summary: &str, body: &str) -> anyhow::Result<u32> {
        if self.fail {
            anyhow::bail!("no notification daemon");
        }
        self.calls.borrow_mut().push((summary.to_string(), body.to_string()));
        Ok(7)
    }

    fn notify_urgent(&self, summary: &str, body: &str) -> anyhow::Result<u32> {
        self.urgent.borrow_mut().push((summary.to_string(), body.to_string()));
        Ok(7)
    }

    fn wait_for_click(&self, id: u32, _timeout: Duration) -> anyhow::Result<bool> {
        assert_eq!(id, 7);
        self.waits.set(self.waits.get() + 1);
        Ok(self.click)
    }
}

/// A plan with one selected entry of `bytes` bytes.
fn plan_with(bytes: u64) -> Plan {
    Plan {
        id: "20261110T030000Z".into(),
        created: 0,
        entries: vec![Entry {
            id: "e1".into(),
            category: "caches".into(),
            title: "pip".into(),
            kind: EntryKind::Item { targets: Vec::new() },
            bytes,
            reason: "old".into(),
            selected: true,
            permanent: false,
            note: None,
            last_active: None,
        }],
        skipped: Vec::new(),
        report_only: Vec::new(),
    }
}

const GIB: u64 = 1 << 30;

/// Runs `notify_if_worth_it` and returns the outcome and the launched ids.
fn run(plan: &Plan, n: &FakeNotifier) -> (NotifyOutcome, Vec<String>) {
    let mut launched = Vec::new();
    let out = notify_if_worth_it(plan, GIB, n, Some(&mut |id: &str| launched.push(id.to_string()))).unwrap();
    (out, launched)
}

#[test]
fn below_threshold_sends_nothing() {
    let n = FakeNotifier::new(true);
    let (out, launched) = run(&plan_with(500 << 20), &n);
    assert_eq!(out, NotifyOutcome::BelowThreshold);
    assert!(n.calls.borrow().is_empty());
    assert!(launched.is_empty());
}

#[test]
fn at_threshold_notifies_with_copy() {
    let n = FakeNotifier::new(false);
    run(&plan_with(GIB), &n);
    assert_eq!(
        *n.calls.borrow(),
        [(
            "1.0 GiB can be freed".to_string(),
            "cleaner built a plan; nothing has been deleted. Click to review it.".to_string()
        )]
    );
}

#[test]
fn click_launches_gui_for_plan() {
    let n = FakeNotifier::new(true);
    let plan = plan_with(2 * GIB);
    let (out, launched) = run(&plan, &n);
    assert_eq!(out, NotifyOutcome::Opened);
    assert_eq!(launched, [plan.id]);
}

/// The click starts the GUI in its own transient user unit, so the timer's
/// oneshot service can finish (and the next night's run isn't blocked by an
/// open window), and a failed start is reported instead of swallowed.
#[test]
fn gui_launch_uses_own_transient_unit() {
    let cmd = gui_launch_command(std::path::Path::new("/nix/store/x/bin/cleaner"), Some("20261110T221007Z"));
    assert_eq!(cmd.get_program(), "systemd-run");
    let args: Vec<_> = cmd.get_args().map(|a| a.to_str().unwrap()).collect();
    assert_eq!(
        args,
        [
            "--user",
            "--collect",
            "--service-type=exec",
            "--unit=cleaner-gui-20261110T221007Z",
            "/nix/store/x/bin/cleaner",
            "gui",
            "20261110T221007Z",
        ]
    );
}

#[test]
fn gui_launch_without_plan() {
    let cmd = gui_launch_command(std::path::Path::new("/nix/store/x/bin/cleaner"), None);
    assert_eq!(cmd.get_program(), "systemd-run");
    let args: Vec<_> = cmd.get_args().map(|a| a.to_str().unwrap()).collect();
    assert_eq!(
        args,
        [
            "--user",
            "--collect",
            "--service-type=exec",
            "--unit=cleaner-gui-lowspace",
            "/nix/store/x/bin/cleaner",
            "gui",
        ]
    );
}

fn low_fs(label: &str, total_gib: u64, avail_gib: u64) -> FsUsage {
    FsUsage { label: label.into(), total: total_gib * GIB, avail: avail_gib * GIB }
}

#[test]
fn low_space_text_saved() {
    let plan = plan_with(12 * GIB);
    let home = low_fs("/home", 100, 3);
    assert_eq!(
        low_space_text(std::slice::from_ref(&home), Ok(&plan), true),
        (
            "/home is almost full: 3.0 GiB free (97% used)".to_string(),
            "A plan can free 12.0 GiB. Nothing has been deleted.".to_string()
        )
    );
    let (_, body) = low_space_text(&[home, low_fs("/data", 500, 40)], Ok(&plan), true);
    assert_eq!(
        body,
        "A plan can free 12.0 GiB. Nothing has been deleted. Also low: /data (40.0 GiB free)."
    );
}

#[test]
fn low_space_text_unsaved_disk_full() {
    let (_, body) = low_space_text(&[low_fs("/home", 100, 3)], Err("disk full".into()), true);
    assert_eq!(
        body,
        "The plan could not be saved: disk full. Nothing has been deleted. Open cleaner to free space."
    );
}

#[test]
fn low_space_notifies_urgent_regardless_of_threshold() {
    let low = [low_fs("/home", 100, 3)];
    let plan = plan_with(0);
    let n = FakeNotifier::new(true);
    let mut launched: Vec<Option<String>> = Vec::new();
    let out = notify_low_space(&low, Ok(&plan), &n, Some(&mut |id| launched.push(id.map(str::to_string)))).unwrap();
    assert_eq!(out, NotifyOutcome::Opened);
    assert_eq!(n.urgent.borrow().len(), 1);
    assert!(n.calls.borrow().is_empty());
    assert_eq!(launched, [Some(plan.id.clone())]);

    let n = FakeNotifier::new(true);
    let mut launched: Vec<Option<String>> = Vec::new();
    notify_low_space(&low, Err("disk full".into()), &n, Some(&mut |id| launched.push(id.map(str::to_string))))
        .unwrap();
    assert_eq!(launched, [None]);

    let n = FakeNotifier::new(false);
    let out = notify_low_space(&low, Ok(&plan), &n, Some(&mut |_| panic!("no click"))).unwrap();
    assert_eq!(out, NotifyOutcome::Dismissed);
}

/// A build without the GUI has nothing a click could open: the copy says how
/// to review from the terminal, and the timer's service exits at once
/// instead of waiting hours for a click.
#[test]
fn cli_build_points_at_show_and_does_not_wait() {
    let n = FakeNotifier::new(true);
    let out = notify_if_worth_it(&plan_with(2 * GIB), GIB, &n, None).unwrap();
    assert_eq!(out, NotifyOutcome::Sent);
    assert_eq!(
        *n.calls.borrow(),
        [(
            "2.0 GiB can be freed".to_string(),
            "cleaner built a plan; nothing has been deleted. Run `cleaner show` to review it.".to_string()
        )]
    );
    assert_eq!(n.waits.get(), 0);

    let n = FakeNotifier::new(true);
    let out = notify_if_worth_it(&plan_with(500 << 20), GIB, &n, None).unwrap();
    assert_eq!(out, NotifyOutcome::BelowThreshold);
    assert!(n.calls.borrow().is_empty());
}

#[test]
fn cli_build_low_space_points_at_terminal_and_does_not_wait() {
    let low = [low_fs("/home", 100, 3)];
    let plan = plan_with(12 * GIB);
    let n = FakeNotifier::new(true);
    let out = notify_low_space(&low, Ok(&plan), &n, None).unwrap();
    assert_eq!(out, NotifyOutcome::Sent);
    assert_eq!(n.urgent.borrow().len(), 1);
    assert_eq!(n.waits.get(), 0);

    let (_, body) = low_space_text(&[low_fs("/home", 100, 3), low_fs("/data", 500, 40)], Ok(&plan), false);
    assert_eq!(
        body,
        "A plan can free 12.0 GiB. Nothing has been deleted. Run `cleaner show` to review it. \
         Also low: /data (40.0 GiB free)."
    );
    let (_, body) = low_space_text(&low, Err("disk full".into()), false);
    assert_eq!(
        body,
        "The plan could not be saved: disk full. Nothing has been deleted. Free some space, then run `cleaner plan`."
    );
}

#[test]
fn dismiss_does_not_launch() {
    let n = FakeNotifier::new(false);
    let (out, launched) = run(&plan_with(2 * GIB), &n);
    assert_eq!(out, NotifyOutcome::Dismissed);
    assert!(launched.is_empty());
}

#[test]
fn notify_error_is_returned() {
    let n = FakeNotifier { fail: true, ..FakeNotifier::new(true) };
    let mut launched = 0;
    assert!(notify_if_worth_it(&plan_with(GIB), GIB, &n, Some(&mut |_: &str| launched += 1)).is_err());
    assert_eq!(launched, 0);
}

#[test]
fn notify_failure_is_not_fatal() {
    // Same layout as tests/cli.rs: HOME and every XDG dir in a temp dir.
    let tmp = TempDir::new().unwrap();
    let home = tmp.path().join("home");
    mkfile(&home.join(".cache/pip/http/a"), 20_000);
    set_age(&home.join(".cache/pip/http/a"), 40);
    mkfile(&home.join(".config/cleaner/config.toml"), 0);
    std::fs::write(home.join(".config/cleaner/config.toml"), "notify_threshold_bytes = 0\n").unwrap();
    std::fs::create_dir_all(tmp.path().join("run")).unwrap();

    Command::new(env!("CARGO_BIN_EXE_cleaner"))
        .args(["plan", "--notify"])
        .env("HOME", &home)
        .env("XDG_CONFIG_HOME", home.join(".config"))
        .env("XDG_STATE_HOME", home.join(".local/state"))
        .env("XDG_RUNTIME_DIR", tmp.path().join("run"))
        .env("DBUS_SESSION_BUS_ADDRESS", "unix:path=/nonexistent")
        .env("TZ", "UTC")
        .assert()
        .success()
        .stderr(contains("warning: notification failed"));

    let plans: Vec<_> = std::fs::read_dir(home.join(".local/state/cleaner/plans"))
        .unwrap()
        .filter_map(Result::ok)
        .filter(|e| e.file_name().to_string_lossy().ends_with(".json"))
        .collect();
    assert_eq!(plans.len(), 1);
}
