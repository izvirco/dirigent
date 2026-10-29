//! What differs on Windows: where config and data live by default, this machine's name, and
//! how helper programs start. Everything else is the same code on every OS (Windows takes
//! `/` in paths too, and `std::env::home_dir` is `%USERPROFILE%` there).

use std::ffi::OsStr;
use std::path::PathBuf;

/// `$XDG_CONFIG_HOME/dirigent` (else `~/.config/dirigent`); `%APPDATA%\dirigent` on Windows.
pub fn config_dir() -> PathBuf {
    if cfg!(windows) { app_dir("APPDATA", r"AppData\Roaming") } else { app_dir("XDG_CONFIG_HOME", ".config") }
}

/// `$XDG_DATA_HOME/dirigent` (else `~/.local/share/dirigent`); `%LOCALAPPDATA%\dirigent` on
/// Windows.
pub fn data_dir() -> PathBuf {
    if cfg!(windows) { app_dir("LOCALAPPDATA", r"AppData\Local") } else { app_dir("XDG_DATA_HOME", ".local/share") }
}

/// `$var/dirigent`, else `<home>/<fallback>/dirigent`.
fn app_dir(var: &str, fallback: &str) -> PathBuf {
    std::env::var_os(var)
        .map(PathBuf::from)
        .or_else(|| std::env::home_dir().map(|h| h.join(fallback)))
        .expect("no home directory")
        .join("dirigent")
}

/// This machine's name, which the service introduces itself with (`Hello`): `/etc/hostname`,
/// `%COMPUTERNAME%` on Windows, else `local`.
pub fn host_name() -> String {
    let name = if cfg!(windows) { std::env::var("COMPUTERNAME").ok() } else { std::fs::read_to_string("/etc/hostname").ok() };
    name.map(|n| n.trim().to_owned()).filter(|n| !n.is_empty()).unwrap_or_else(|| "local".into())
}

/// `Command::new(program)` for the programs the service runs (git, jj, tailscale, the
/// runtime). On Windows without a console window: the desktop is a GUI app there, and every
/// console program it starts would flash one. The runtime's own children inherit its
/// hidden console.
pub fn command(program: impl AsRef<OsStr>) -> std::process::Command {
    #[cfg_attr(not(windows), allow(unused_mut))]
    let mut cmd = std::process::Command::new(program);
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        cmd.creation_flags(CREATE_NO_WINDOW);
    }
    cmd
}
