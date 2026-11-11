//! `cleaner timer` and the unit/.desktop files. Every test uses a temp home
//! and a fake `systemctl` on PATH; nothing touches the real systemd user dir.

use assert_cmd::Command;
use cleaner::APP_ID;
use cleaner::config::Config;
use cleaner::paths::Paths;
use cleaner::timer::{
    parse_interval, parse_time, refresh_if_installed, render_service, render_space_service, render_space_timer, render_timer,
};
use predicates::str::contains;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::sync::Once;
use tempfile::TempDir;

/// Writes an executable `systemctl` into `dir` that appends its arguments
/// to `log`.
fn fake_systemctl(dir: &Path, log: &Path) {
    std::fs::create_dir_all(dir).unwrap();
    let script = dir.join("systemctl");
    std::fs::write(&script, format!("#!/bin/sh\necho \"$@\" >> '{}'\n", log.display())).unwrap();
    std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
}

/// In-process calls (`refresh_if_installed`) run `systemctl` from this
/// process's PATH, so put a fake one first, once, before any test uses it.
fn fake_systemctl_in_process() {
    static ONCE: Once = Once::new();
    ONCE.call_once(|| {
        let dir = std::env::temp_dir().join(format!("cleaner-timer-test-{}", std::process::id()));
        fake_systemctl(&dir, Path::new("/dev/null"));
        let path = format!("{}:{}", dir.display(), std::env::var("PATH").unwrap_or_default());
        // SAFETY: runs once, before this test reads the environment; the other
        // tests here pass PATH to their children explicitly.
        unsafe { std::env::set_var("PATH", path) };
    });
}

struct Env {
    tmp: TempDir,
}

impl Env {
    fn new() -> Env {
        let tmp = TempDir::new().unwrap();
        std::fs::create_dir_all(tmp.path().join("home")).unwrap();
        std::fs::create_dir_all(tmp.path().join("run")).unwrap();
        fake_systemctl(&tmp.path().join("bin"), &tmp.path().join("calls.txt"));
        Env { tmp }
    }

    fn home(&self) -> PathBuf {
        self.tmp.path().join("home")
    }

    fn paths(&self) -> Paths {
        Paths::for_home(&self.home())
    }

    fn calls_file(&self) -> PathBuf {
        self.tmp.path().join("calls.txt")
    }

    fn calls(&self) -> Vec<String> {
        std::fs::read_to_string(self.calls_file()).unwrap().lines().map(str::to_string).collect()
    }

    fn write_config(&self, text: &str) {
        let p = self.paths().config_file;
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        std::fs::write(p, text).unwrap();
    }

    fn unit_files(&self) -> [PathBuf; 3] {
        let p = self.paths();
        [
            p.systemd_user_dir.join("cleaner.service"),
            p.systemd_user_dir.join("cleaner.timer"),
            p.applications_dir.join(format!("{APP_ID}.desktop")),
        ]
    }

    /// `cleaner <args>` with HOME and every XDG dir inside the temp dir and
    /// the fake `systemctl` first on PATH.
    fn cmd(&self, args: &[&str]) -> Command {
        let home = self.home();
        let path = format!(
            "{}:{}",
            self.tmp.path().join("bin").display(),
            std::env::var("PATH").unwrap_or_default()
        );
        let mut c = Command::new(env!("CARGO_BIN_EXE_cleaner"));
        c.args(args)
            .env("HOME", &home)
            .env("XDG_CONFIG_HOME", home.join(".config"))
            .env("XDG_DATA_HOME", home.join(".local/share"))
            .env("XDG_STATE_HOME", home.join(".local/state"))
            .env("XDG_RUNTIME_DIR", self.tmp.path().join("run"))
            .env("PATH", path)
            .env("TZ", "UTC");
        c
    }
}

#[test]
fn parse_time_accepts() {
    assert_eq!(parse_time("03:00").unwrap(), (3, 0));
    assert_eq!(parse_time("23:59").unwrap(), (23, 59));
}

#[test]
fn parse_time_rejects_bad_values() {
    for bad in ["3am", "25:00", "7:5", "12:60", ""] {
        let err = parse_time(bad).expect_err(bad);
        assert!(format!("{err:#}").contains("HH:MM"), "{bad}: {err:#}");
    }
}

#[test]
fn render_exact() {
    assert_eq!(
        render_service(Path::new("/x/cleaner")),
        "[Unit]\n\
         Description=cleaner: build a cleanup plan (deletes nothing)\n\
         \n\
         [Service]\n\
         Type=oneshot\n\
         ExecStart=\"/x/cleaner\" plan --notify\n\
         Nice=10\n\
         IOSchedulingClass=idle\n"
    );
    assert_eq!(
        render_timer("03:00").unwrap(),
        "[Unit]\n\
         Description=cleaner nightly plan\n\
         \n\
         [Timer]\n\
         OnCalendar=*-*-* 03:00:00\n\
         Persistent=true\n\
         RandomizedDelaySec=5min\n\
         \n\
         [Install]\n\
         WantedBy=timers.target\n"
    );
}

#[test]
fn install_writes_units_and_enables() {
    let env = Env::new();
    env.cmd(&["timer", "install"]).assert().success();
    for f in env.unit_files() {
        assert!(f.is_file(), "{} missing", f.display());
    }
    let [service, timer, desktop] = env.unit_files().map(|f| std::fs::read_to_string(f).unwrap());
    assert!(timer.contains("OnCalendar=*-*-* 03:00:00\n"), "{timer}");
    let exe = Path::new(env!("CARGO_BIN_EXE_cleaner")).canonicalize().unwrap();
    assert!(service.contains(&format!("ExecStart=\"{}\" plan --notify\n", exe.display())), "{service}");
    assert!(desktop.contains(&format!("Exec=\"{}\" gui\n", exe.display())), "{desktop}");
    assert_eq!(
        env.calls(),
        ["--user daemon-reload", "--user enable --now cleaner.timer", "--user enable --now cleaner-space.timer"]
    );
}

#[test]
fn install_bad_time_writes_nothing() {
    let env = Env::new();
    env.write_config("timer_time = \"3am\"\n");
    env.cmd(&["timer", "install"]).assert().code(1).stderr(contains("HH:MM"));
    for f in env.unit_files() {
        assert!(!f.exists(), "{} written", f.display());
    }
    assert!(!env.calls_file().exists());
}

#[test]
fn uninstall_removes_units() {
    let env = Env::new();
    env.cmd(&["timer", "install"]).assert().success();
    env.cmd(&["timer", "uninstall"]).assert().success();
    for f in env.unit_files() {
        assert!(!f.exists(), "{} still there", f.display());
    }
    assert_eq!(env.calls().last().unwrap(), "--user daemon-reload");
}

#[test]
fn refresh_only_when_installed() {
    fake_systemctl_in_process();
    let env = Env::new();
    let exe = Path::new("/x/cleaner");
    let mut config = Config::default();
    assert!(!refresh_if_installed(&env.paths(), &config, exe).unwrap());
    for f in env.unit_files() {
        assert!(!f.exists(), "{} written", f.display());
    }

    env.cmd(&["timer", "install"]).assert().success();
    config.timer_time = "04:30".into();
    assert!(refresh_if_installed(&env.paths(), &config, exe).unwrap());
    let timer = std::fs::read_to_string(&env.unit_files()[1]).unwrap();
    assert!(timer.contains("OnCalendar=*-*-* 04:30:00\n"), "{timer}");
}

impl Env {
    fn space_files(&self) -> [PathBuf; 2] {
        let d = self.paths().systemd_user_dir;
        [d.join("cleaner-space.service"), d.join("cleaner-space.timer")]
    }
}

#[test]
fn render_space_exact() {
    assert_eq!(
        render_space_service(Path::new("/nix/store/x/bin/cleaner")),
        "[Unit]\nDescription=cleaner: check free space (deletes nothing)\n\n[Service]\nType=oneshot\nExecStart=\"/nix/store/x/bin/cleaner\" plan --if-low\nNice=10\nIOSchedulingClass=idle\n"
    );
    assert_eq!(
        render_space_timer("10min").unwrap(),
        "[Unit]\nDescription=cleaner low-space check\n\n[Timer]\nOnStartupSec=2min\nOnUnitActiveSec=10min\nAccuracySec=1min\n\n[Install]\nWantedBy=timers.target\n"
    );
}

#[test]
fn parse_interval_cases() {
    for ok in ["10min", "1h"] {
        parse_interval(ok).expect(ok);
    }
    for bad in ["0min", "10", "10s", "ten min", ""] {
        let err = parse_interval(bad).expect_err(bad);
        assert_eq!(err.to_string(), format!("invalid interval {bad:?} (expected e.g. 10min or 1h)"));
    }
}

#[test]
fn install_writes_space_units() {
    let env = Env::new();
    env.cmd(&["timer", "install"]).assert().success();
    for f in env.space_files() {
        assert!(f.is_file(), "{} missing", f.display());
    }
    assert!(env.calls().contains(&"--user enable --now cleaner-space.timer".to_string()), "{:?}", env.calls());
}

#[test]
fn install_low_space_off_removes_space_units() {
    let env = Env::new();
    for f in env.space_files() {
        std::fs::create_dir_all(f.parent().unwrap()).unwrap();
        std::fs::write(f, "x").unwrap();
    }
    env.write_config("[low_space]\nenabled = false\n");
    env.cmd(&["timer", "install"]).assert().success();
    for f in env.space_files() {
        assert!(!f.exists(), "{} still there", f.display());
    }
    assert!(env.calls().contains(&"--user disable --now cleaner-space.timer".to_string()), "{:?}", env.calls());
}

#[test]
fn install_bad_interval_writes_nothing() {
    let env = Env::new();
    env.write_config("[low_space]\ninterval = \"soon\"\n");
    env.cmd(&["timer", "install"]).assert().code(1).stderr(contains("invalid interval"));
    for f in env.unit_files().iter().chain(env.space_files().iter()) {
        assert!(!f.exists(), "{} written", f.display());
    }
    assert!(!env.calls_file().exists());
}

#[test]
fn uninstall_removes_space_units() {
    let env = Env::new();
    env.cmd(&["timer", "install"]).assert().success();
    env.cmd(&["timer", "uninstall"]).assert().success();
    for f in env.unit_files().iter().chain(env.space_files().iter()) {
        assert!(!f.exists(), "{} still there", f.display());
    }
    let calls = env.calls();
    for want in ["--user disable --now cleaner.timer", "--user disable --now cleaner-space.timer"] {
        assert!(calls.contains(&want.to_string()), "no {want:?} in {calls:?}");
    }
}

#[test]
fn refresh_syncs_space_timer() {
    fake_systemctl_in_process();
    let env = Env::new();
    let exe = Path::new("/x/cleaner");
    env.cmd(&["timer", "install"]).assert().success();
    for f in env.space_files() {
        assert!(f.is_file());
    }
    let mut config = Config::default();
    config.low_space.enabled = false;
    assert!(refresh_if_installed(&env.paths(), &config, exe).unwrap());
    for f in env.space_files() {
        assert!(!f.exists(), "{} still there", f.display());
    }
    config.low_space.enabled = true;
    config.low_space.interval = "1h".into();
    assert!(refresh_if_installed(&env.paths(), &config, exe).unwrap());
    let timer = std::fs::read_to_string(&env.space_files()[1]).unwrap();
    assert!(timer.contains("OnUnitActiveSec=1h\n"), "{timer}");
}

/// The nightly units as home-manager links them: symlinks into the Nix store
/// (dangling here, which `read_link` doesn't mind).
fn link_units_from_nix(env: &Env) -> Vec<PathBuf> {
    let dir = env.paths().systemd_user_dir;
    std::fs::create_dir_all(&dir).unwrap();
    ["cleaner.service", "cleaner.timer"]
        .iter()
        .map(|name| {
            let f = dir.join(name);
            let target = format!("/nix/store/0000-home-manager-files/.config/systemd/user/{name}");
            std::os::unix::fs::symlink(target, &f).unwrap();
            f
        })
        .collect()
}

#[test]
fn nix_managed_units_are_left_alone() {
    fake_systemctl_in_process();
    let env = Env::new();
    let links = link_units_from_nix(&env);
    for args in [["timer", "install"], ["timer", "uninstall"]] {
        env.cmd(&args).assert().code(1).stderr(contains("managed by Nix"));
    }
    env.cmd(&["timer", "status"]).assert().success().stdout(contains("units: managed by Nix\n"));
    let config = Config { timer_time: "04:30".into(), ..Default::default() };
    assert!(!refresh_if_installed(&env.paths(), &config, Path::new("/x/cleaner")).unwrap());
    for f in &links {
        assert!(std::fs::read_link(f).unwrap().starts_with("/nix/store"), "{} replaced", f.display());
    }
    for f in env.space_files().iter().chain(&env.unit_files()[2..]) {
        assert!(!f.exists(), "{} written", f.display());
    }
    assert_eq!(env.calls(), ["--user list-timers cleaner.timer cleaner-space.timer --no-pager"]);
}

/// The Nix package's wrapper sets CLEANER_EXE to itself, so units and the
/// notification's GUI launch get the wrapper's environment.
#[test]
fn install_uses_cleaner_exe_when_set() {
    let env = Env::new();
    let wrapper = env.tmp.path().join("wrapper/cleaner");
    std::fs::create_dir_all(wrapper.parent().unwrap()).unwrap();
    std::fs::write(&wrapper, "").unwrap();
    env.cmd(&["timer", "install"]).env("CLEANER_EXE", &wrapper).assert().success();
    let [service, _, desktop] = env.unit_files().map(|f| std::fs::read_to_string(f).unwrap());
    let want = wrapper.canonicalize().unwrap();
    assert!(service.contains(&format!("ExecStart=\"{}\" plan --notify\n", want.display())), "{service}");
    assert!(desktop.contains(&format!("Exec=\"{}\" gui\n", want.display())), "{desktop}");
}
