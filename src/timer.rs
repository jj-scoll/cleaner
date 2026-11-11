//! The systemd user timer that runs `cleaner plan --notify` nightly, and the
//! `.desktop` file that lets the notification and launchers open the GUI
//! (spec §7.1, §7.2).

use anyhow::{Context, bail};
use std::path::{Path, PathBuf};
use std::process::Command;

use crate::APP_ID;
use crate::config::Config;
use crate::paths::Paths;

const SERVICE: &str = "cleaner.service";
const TIMER: &str = "cleaner.timer";
const SPACE_SERVICE: &str = "cleaner-space.service";
const SPACE_TIMER: &str = "cleaner-space.timer";

/// Exactly `HH:MM`, 24-hour.
pub fn parse_time(s: &str) -> anyhow::Result<(u8, u8)> {
    let bad = || anyhow::anyhow!("timer_time must be HH:MM (24-hour), got \"{s}\"");
    let b = s.as_bytes();
    if b.len() != 5 || b[2] != b':' || ![0, 1, 3, 4].iter().all(|&i| b[i].is_ascii_digit()) {
        return Err(bad());
    }
    let (h, m) = ((b[0] - b'0') * 10 + b[1] - b'0', (b[3] - b'0') * 10 + b[4] - b'0');
    if h > 23 || m > 59 {
        return Err(bad());
    }
    Ok((h, m))
}

pub fn render_service(exe: &Path) -> String {
    format!(
        "[Unit]\n\
         Description=cleaner: build a cleanup plan (deletes nothing)\n\
         \n\
         [Service]\n\
         Type=oneshot\n\
         ExecStart=\"{}\" plan --notify\n\
         Nice=10\n\
         IOSchedulingClass=idle\n",
        exe.display()
    )
}

pub fn render_timer(time: &str) -> anyhow::Result<String> {
    let (h, m) = parse_time(time)?;
    Ok(format!(
        "[Unit]\n\
         Description=cleaner nightly plan\n\
         \n\
         [Timer]\n\
         OnCalendar=*-*-* {h:02}:{m:02}:00\n\
         Persistent=true\n\
         RandomizedDelaySec=5min\n\
         \n\
         [Install]\n\
         WantedBy=timers.target\n"
    ))
}

pub fn render_space_service(exe: &Path) -> String {
    format!(
        "[Unit]\n\
         Description=cleaner: check free space (deletes nothing)\n\
         \n\
         [Service]\n\
         Type=oneshot\n\
         ExecStart=\"{}\" plan --if-low\n\
         Nice=10\n\
         IOSchedulingClass=idle\n",
        exe.display()
    )
}

/// `^[1-9][0-9]*(min|h)$`.
pub fn parse_interval(s: &str) -> anyhow::Result<()> {
    let digits = s.strip_suffix("min").or_else(|| s.strip_suffix('h')).unwrap_or("");
    let ok = digits.bytes().all(|b| b.is_ascii_digit()) && !digits.is_empty() && !digits.starts_with('0');
    if ok { Ok(()) } else { bail!("invalid interval {s:?} (expected e.g. 10min or 1h)") }
}

pub fn render_space_timer(interval: &str) -> anyhow::Result<String> {
    parse_interval(interval)?;
    Ok(format!(
        "[Unit]\n\
         Description=cleaner low-space check\n\
         \n\
         [Timer]\n\
         OnStartupSec=2min\n\
         OnUnitActiveSec={interval}\n\
         AccuracySec=1min\n\
         \n\
         [Install]\n\
         WantedBy=timers.target\n"
    ))
}

pub fn render_desktop(exe: &Path) -> String {
    format!(
        "[Desktop Entry]\n\
         Type=Application\n\
         Name=Cleaner\n\
         Comment=Approve-first disk cleanup\n\
         Exec=\"{}\" gui\n\
         Icon=edit-clear-all-symbolic\n\
         Terminal=false\n\
         Categories=System;Utility;\n",
        exe.display()
    )
}

fn unit_files(paths: &Paths) -> [PathBuf; 3] {
    [
        paths.systemd_user_dir.join(SERVICE),
        paths.systemd_user_dir.join(TIMER),
        paths.applications_dir.join(format!("{APP_ID}.desktop")),
    ]
}

fn space_files(paths: &Paths) -> [PathBuf; 2] {
    [paths.systemd_user_dir.join(SPACE_SERVICE), paths.systemd_user_dir.join(SPACE_TIMER)]
}

fn remove_if_present(f: &Path) -> anyhow::Result<()> {
    match std::fs::remove_file(f) {
        Err(e) if e.kind() != std::io::ErrorKind::NotFound => {
            Err(e).with_context(|| format!("removing {}", f.display()))
        }
        _ => Ok(()),
    }
}

/// Writes `path` via a temp file in the same dir and a rename.
pub(crate) fn write_atomic(path: &Path, text: &str) -> anyhow::Result<()> {
    let ctx = || format!("writing {}", path.display());
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).with_context(ctx)?;
    }
    let mut tmp = path.as_os_str().to_owned();
    tmp.push(".tmp");
    std::fs::write(&tmp, text).with_context(ctx)?;
    std::fs::rename(&tmp, path).with_context(ctx)
}

/// `systemctl --user <args>`; a non-zero exit is an error carrying its stderr.
fn systemctl(args: &[&str]) -> anyhow::Result<String> {
    let what = || format!("running systemctl --user {}", args.join(" "));
    let out = Command::new("systemctl").arg("--user").args(args).output().with_context(what)?;
    if !out.status.success() {
        bail!("{} failed: {}", what(), String::from_utf8_lossy(&out.stderr).trim());
    }
    Ok(String::from_utf8_lossy(&out.stdout).into_owned())
}

/// Renders everything first (a bad `timer_time` or `low_space.interval` writes
/// nothing), writes the nightly service, timer and `.desktop` file, plus the
/// space units when `low_space.enabled`, then reloads and enables the timers.
/// With low-space off the space timer is disabled and its files removed.
pub fn install(paths: &Paths, config: &Config, exe: &Path) -> anyhow::Result<()> {
    let timer = render_timer(&config.timer_time)?;
    let space_timer = render_space_timer(&config.low_space.interval)?;
    let [service_path, timer_path, desktop_path] = unit_files(paths);
    write_atomic(&service_path, &render_service(exe))?;
    write_atomic(&timer_path, &timer)?;
    write_atomic(&desktop_path, &render_desktop(exe))?;
    let [space_service_path, space_timer_path] = space_files(paths);
    if config.low_space.enabled {
        write_atomic(&space_service_path, &render_space_service(exe))?;
        write_atomic(&space_timer_path, &space_timer)?;
    } else {
        remove_space_units(paths)?;
    }
    systemctl(&["daemon-reload"])?;
    systemctl(&["enable", "--now", TIMER])?;
    if config.low_space.enabled {
        systemctl(&["enable", "--now", SPACE_TIMER])?;
    }
    Ok(())
}

/// Disables the space timer (a failure, e.g. not enabled, is ignored) and
/// removes its two files if present.
fn remove_space_units(paths: &Paths) -> anyhow::Result<()> {
    let _ = systemctl(&["disable", "--now", SPACE_TIMER]);
    space_files(paths).iter().try_for_each(|f| remove_if_present(f))
}

/// Disables both timers (a failure, e.g. not enabled, is ignored), removes the
/// five files if present, then reloads.
pub fn uninstall(paths: &Paths) -> anyhow::Result<()> {
    let _ = systemctl(&["disable", "--now", TIMER]);
    let _ = systemctl(&["disable", "--now", SPACE_TIMER]);
    for f in unit_files(paths).iter().chain(space_files(paths).iter()) {
        remove_if_present(f)?;
    }
    systemctl(&["daemon-reload"])?;
    Ok(())
}

/// `units: installed|not installed`, then what systemd says about both timers.
pub fn status(paths: &Paths) -> anyhow::Result<String> {
    let [service, timer, _] = unit_files(paths);
    let state = if service.is_file() && timer.is_file() { "installed" } else { "not installed" };
    let timers = systemctl(&["list-timers", TIMER, SPACE_TIMER, "--no-pager"])?;
    Ok(format!("units: {state}\n{timers}"))
}

/// After a settings change: if the nightly timer is installed, re-renders it,
/// syncs the space units to the config (write + enable, or disable + remove)
/// and reloads; returns whether it did. A bad time or interval writes nothing.
/// The services are left alone, so they keep the binary `install` was run from,
/// except a newly enabled space service, which takes `exe`.
pub fn refresh_if_installed(paths: &Paths, config: &Config, exe: &Path) -> anyhow::Result<bool> {
    let [_, timer_path, _] = unit_files(paths);
    if !timer_path.is_file() {
        return Ok(false);
    }
    let timer = render_timer(&config.timer_time)?;
    let space_timer = render_space_timer(&config.low_space.interval)?;
    write_atomic(&timer_path, &timer)?;
    let [space_service_path, space_timer_path] = space_files(paths);
    if config.low_space.enabled {
        if !space_service_path.is_file() {
            write_atomic(&space_service_path, &render_space_service(exe))?;
        }
        write_atomic(&space_timer_path, &space_timer)?;
    } else {
        remove_space_units(paths)?;
    }
    systemctl(&["daemon-reload"])?;
    if config.low_space.enabled {
        systemctl(&["enable", "--now", SPACE_TIMER])?;
    }
    Ok(true)
}
