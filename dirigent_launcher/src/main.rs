//! A stable entry point: resolve the selection, spawn the desktop, and exit.
#![cfg_attr(windows, windows_subsystem = "windows")]

use dirigent_launcher::{Selection, lock};
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
    Command::new(selected.executable(root))
        .args(env::args_os().skip(1))
        .spawn()
        .map_err(|e| {
            format!(
                "could not start Dirigent {} {}: {e}",
                selected.channel, selected.version
            )
        })?;
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

#[cfg(not(windows))]
fn show_error(message: &str) {
    eprintln!("{message}");
}

#[cfg(windows)]
fn show_error(message: &str) {
    use windows_sys::Win32::UI::WindowsAndMessaging::{MB_ICONERROR, MB_OK, MessageBoxW};
    let text: Vec<u16> = message.encode_utf16().chain(Some(0)).collect();
    let title: Vec<u16> = "Dirigent".encode_utf16().chain(Some(0)).collect();
    // Both strings are NUL-terminated and remain alive for this synchronous call.
    unsafe {
        MessageBoxW(
            std::ptr::null_mut(),
            text.as_ptr(),
            title.as_ptr(),
            MB_OK | MB_ICONERROR,
        );
    }
}
