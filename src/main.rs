mod cli;

use clap::Parser;
use cli::{Cli, Cmd};

fn main() {
    if rustix::process::geteuid().is_root() {
        eprintln!("cleaner must not run as root");
        std::process::exit(2);
    }
    let cli = Cli::parse();
    if matches!(cli.command, Some(Cmd::Plan { .. } | Cmd::Apply { .. })) {
        cleaner::sys::lower_priority();
    }
    match cli::run(cli) {
        Ok(code) => std::process::exit(code),
        Err(e) => {
            eprintln!("error: {e:#}");
            std::process::exit(1);
        }
    }
}
