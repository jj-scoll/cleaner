pub mod category;
pub mod config;
pub mod disk;
pub mod engine;
pub mod fmt;
pub mod fsutil;
pub mod git;
#[cfg(feature = "gui")]
pub mod gui;
pub mod lock;
pub mod log;
pub mod notify;
pub mod object;
pub mod paths;
pub mod plan;
pub mod procs;
pub mod protect;
pub mod settings;
pub mod space;
pub mod sys;
pub mod timer;
pub mod tool;
pub mod view;
pub mod walk;

/// GApplication id, also the `.desktop` file name and notification desktop-entry.
pub const APP_ID: &str = "io.github.jj_scoll.Cleaner";

/// The path units and GUI launches run: `CLEANER_EXE` when set, else this
/// binary. The Nix package's wrapper sets it to itself, so what it starts
/// keeps the wrapper's GTK environment.
pub fn self_exe() -> std::io::Result<std::path::PathBuf> {
    match std::env::var_os("CLEANER_EXE") {
        Some(p) => Ok(p.into()),
        None => std::env::current_exe(),
    }
}
