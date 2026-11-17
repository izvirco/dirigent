use std::ffi::OsStr;
use std::path::PathBuf;

pub fn config_dir() -> PathBuf {
    if cfg!(windows) { app_dir("APPDATA", r"AppData\Roaming") } else { app_dir("XDG_CONFIG_HOME", ".config") }
}

pub fn data_dir() -> PathBuf {
    if cfg!(windows) { app_dir("LOCALAPPDATA", r"AppData\Local") } else { app_dir("XDG_DATA_HOME", ".local/share") }
}

fn app_dir(var: &str, fallback: &str) -> PathBuf {
    std::env::var_os(var)
        .map(PathBuf::from)
        .or_else(|| std::env::home_dir().map(|h| h.join(fallback)))
        .expect("no home directory")
        .join(dirigent_protocol::release::app())
}

pub fn host_name() -> String {
    let name = if cfg!(windows) {
        std::env::var("COMPUTERNAME").ok()
    } else if cfg!(target_os = "macos") {
        command("scutil").args(["--get", "LocalHostName"]).output().ok().and_then(|out| String::from_utf8(out.stdout).ok())
    } else {
        std::fs::read_to_string("/etc/hostname").ok()
    };
    name.map(|n| n.trim().to_owned()).filter(|n| !n.is_empty()).unwrap_or_else(|| "local".into())
}

/// `Command::new(program)` for the programs the service runs (git, jj, ssh, tailscale). On
/// Windows without a console window: the desktop is a GUI app there, and every console
/// program it starts would flash one.
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
