//! `dirigent`, the command line (a console program; `dirigent_desktop` is the app):
//!
//!   dirigent [dir]        start the desktop (which hands `dir` to a running one)
//!   dirigent key …        the public listener's API keys (`key add <name>`, `key list`,
//!                         `key revoke <name>`), for the service the desktop embeds
//!   dirigent login        sign this data dir in to the forge with a device code (`logout`,
//!                         `whoami`): the account forge-auth hosts and the leaderboard use
//!
//! `--data-dir` / `--config-dir` (`dirigent_service::init_dirs`) go before the command and
//! apply to both. The desktop is found next to this binary, as an install, a nix package and
//! a cargo target dir all have it.

use anyhow::{Context, Result};
use std::process::Command;

fn main() -> Result<()> {
    let operands = dirigent_service::init_dirs()?;
    if operands.first().map(String::as_str) == Some("key") {
        return dirigent_service::key_command(&operands[1..]);
    }
    if let [cmd] = &operands[..]
        && ["login", "logout", "whoami"].contains(&cmd.as_str())
    {
        return dirigent_service::forge::command(cmd);
    }
    let exe = std::env::current_exe()?;
    let exe = if cfg!(windows) { exe } else { exe.canonicalize()? };
    let desktop = exe.with_file_name(format!("dirigent_desktop{}", std::env::consts::EXE_SUFFIX));
    let mut cmd = Command::new(&desktop);
    cmd.args(dirigent_service::dir_args()).args(&operands);
    exec(cmd).with_context(|| format!("start {}", desktop.display()))
}

/// Become `cmd`: replaced by it on unix, so a terminal's `dirigent` is the desktop itself,
/// with its exit status. Windows can't replace a process: it starts the desktop (a GUI
/// program) and leaves it to run on its own.
fn exec(mut cmd: Command) -> Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        Err(cmd.exec().into())
    }
    #[cfg(not(unix))]
    {
        cmd.spawn()?;
        Ok(())
    }
}
