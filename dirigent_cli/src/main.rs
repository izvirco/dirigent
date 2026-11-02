//! `dirigent`, the command line (a console program; `dirigent_desktop` is the app):
//!
//!   dirigent [dir]        start the desktop (which hands `dir` to a running one)
//!   dirigent pi [args…]   the bundled pi CLI on dirigent's agent dir (`pi install …`,
//!                         `pi config`, a terminal session)
//!
//! `--data-dir` / `--config-dir` (`dirigent_service::init_dirs`) go before the command and
//! apply to both. The desktop and the runtime are found next to this binary, as an install,
//! a nix package and a cargo target dir all have them.

use anyhow::{Context, Result};
use std::process::Command;

fn main() -> Result<()> {
    let operands = dirigent_service::init_dirs()?;
    if operands.first().map(String::as_str) == Some("pi") {
        let cmd = dirigent_service::runtime_cmd()?;
        let mut pi = Command::new(&cmd[0]);
        pi.args(&cmd[1..]).arg("pi").args(&operands[1..]).envs(dirigent_service::runtime_env());
        return exec(pi, true);
    }
    let exe = std::env::current_exe()?;
    let exe = if cfg!(windows) { exe } else { exe.canonicalize()? };
    let desktop = exe.with_file_name(format!("dirigent_desktop{}", std::env::consts::EXE_SUFFIX));
    let mut cmd = Command::new(&desktop);
    cmd.args(dirigent_service::dir_args()).args(&operands);
    exec(cmd, false).with_context(|| format!("start {}", desktop.display()))
}

/// Become `cmd`: replaced by it on unix, so a terminal's `dirigent` is the desktop (or pi)
/// itself, with its exit status. Windows can't replace a process: it runs `cmd` and, if
/// `wait`, exits with its status; the desktop (a GUI program) is left to run on its own.
fn exec(mut cmd: Command, wait: bool) -> Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        let _ = wait;
        Err(cmd.exec().into())
    }
    #[cfg(not(unix))]
    {
        let mut child = cmd.spawn()?;
        if !wait {
            return Ok(());
        }
        std::process::exit(child.wait()?.code().unwrap_or(1));
    }
}
