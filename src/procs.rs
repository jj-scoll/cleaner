//! Running-app detection over `/proc` and app lock files (spec §5.2d).
//! `proc_root` is `/proc` in production and a temp dir in tests.

use std::path::Path;

use crate::config::AppCfg;

/// Whether the process named by an app's lock symlink is alive. Chromium
/// writes `host-<pid>` (the host may contain `-`), Firefox `<ip>:+<pid>`.
/// `None` if the lock is missing or its target isn't one of those forms.
pub fn lock_owner_alive(lock: &Path, proc_root: &Path) -> Option<bool> {
    let target = std::fs::read_link(lock).ok()?;
    let target = target.to_str()?;
    let pid = match target.rsplit_once(":+") {
        Some((_, pid)) => pid,
        None => target.rsplit_once('-')?.1,
    };
    let pid: u32 = pid.parse().ok()?;
    Some(proc_root.join(pid.to_string()).exists())
}

/// Whether any process's exe file name, or argv[0] file name, is in `names`.
/// `comm` is not used: it is truncated to 15 bytes (`.firefox-wrappe`).
pub fn exe_running(names: &[String], proc_root: &Path) -> bool {
    if names.is_empty() {
        return false;
    }
    let Ok(rd) = std::fs::read_dir(proc_root) else { return false };
    let named = |p: &Path| {
        p.file_name().is_some_and(|n| names.iter().any(|name| n == name.as_str()))
    };
    rd.flatten()
        .filter(|e| e.file_name().to_str().is_some_and(|s| s.bytes().all(|b| b.is_ascii_digit())))
        .any(|e| {
            let dir = e.path();
            // Another user's process has an unreadable `exe`; its cmdline still reads.
            if std::fs::read_link(dir.join("exe")).is_ok_and(|exe| named(&exe)) {
                return true;
            }
            let Ok(cmdline) = std::fs::read(dir.join("cmdline")) else { return false };
            let argv0 = cmdline.split(|&b| b == 0).next().unwrap_or_default();
            std::str::from_utf8(argv0).is_ok_and(|a| named(Path::new(a)))
        })
}

/// Any of the app's (expanded) locks has a live owner, or one of its exe
/// names is running.
pub fn app_running(app: &AppCfg, home: &Path, proc_root: &Path) -> bool {
    app.locks
        .iter()
        .flat_map(|l| crate::paths::expand(l, home))
        .any(|lock| lock_owner_alive(&lock, proc_root) == Some(true))
        || exe_running(&app.exe, proc_root)
}
