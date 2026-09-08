//! A stable entry point: resolve the selection, spawn the desktop, and exit.
#![cfg_attr(windows, windows_subsystem = "windows")]

use dirigent_launcher::{Selection, lock, show_error};
use std::{
    env,
    process::{Command, ExitCode},
};

fn launch() -> Result<(), String> {
    let executable = env::current_exe().map_err(|e| e.to_string())?;
    let root = executable
        .parent()
        .ok_or("launcher has no parent directory")?;
    // Keep cleanup/publication out until Windows has opened the selected executable.
    let _lock = lock(root, false)?;
    let selected = Selection::read(root)?;
    let child = Command::new(selected.executable(root))
        .args(env::args_os().skip(1))
        .spawn()
        .map_err(|e| {
            format!(
                "could not start Dirigent {} {}: {e}",
                selected.channel, selected.version
            )
        })?;
    // Pass Explorer's foreground permission through to the desktop, which may hand it on
    // to the existing window owner instead of creating a window of its own.
    #[cfg(windows)]
    unsafe {
        windows_sys::Win32::UI::WindowsAndMessaging::AllowSetForegroundWindow(child.id());
    }
    #[cfg(not(windows))]
    let _ = child;
    Ok(())
}

fn main() -> ExitCode {
    match launch() {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            show_error(&format!(
                "{error}\n\nTry reinstalling this channel of Dirigent."
            ));
            ExitCode::FAILURE
        }
    }
}
