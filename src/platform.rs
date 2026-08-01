use std::{
    env, fs,
    path::PathBuf,
    process::{Child, Command},
};

const PI_BRIDGE_EXTENSION: &str = include_str!("../asset/dirigent-bridge.ts");

#[cfg(not(target_os = "windows"))]
pub(crate) fn config_dir() -> Result<PathBuf, String> {
    if let Some(config_home) = env::var_os("XDG_CONFIG_HOME") {
        return Ok(PathBuf::from(config_home).join("dirigent"));
    }
    let home = home_dir().ok_or_else(|| {
        "HOME and XDG_CONFIG_HOME are unset; cannot load Dirigent configuration".to_string()
    })?;
    Ok(home.join(".config/dirigent"))
}

#[cfg(target_os = "windows")]
pub(crate) fn config_dir() -> Result<PathBuf, String> {
    let config_home = env::var_os("APPDATA")
        .map(PathBuf::from)
        .or_else(|| home_dir().map(|home| home.join("AppData/Roaming")))
        .ok_or_else(|| {
            "APPDATA and USERPROFILE are unset; cannot load Dirigent configuration".to_string()
        })?;
    Ok(config_home.join("dirigent"))
}

#[cfg(not(target_os = "windows"))]
fn state_directory() -> Result<PathBuf, String> {
    if let Some(data_home) = env::var_os("XDG_DATA_HOME") {
        return Ok(PathBuf::from(data_home).join("dirigent/v0"));
    }
    let home = home_dir().ok_or_else(|| {
        "HOME and XDG_DATA_HOME are unset; cannot persist Dirigent state".to_string()
    })?;
    Ok(home.join(".local/share/dirigent/v0"))
}

#[cfg(target_os = "windows")]
fn state_directory() -> Result<PathBuf, String> {
    let data_home = env::var_os("APPDATA")
        .map(PathBuf::from)
        .or_else(|| home_dir().map(|home| home.join("AppData/Roaming")))
        .ok_or_else(|| {
            "APPDATA and USERPROFILE are unset; cannot persist Dirigent state".to_string()
        })?;
    Ok(data_home.join("dirigent/v0"))
}

pub(crate) fn state_database_path() -> Result<PathBuf, String> {
    Ok(state_directory()?.join("state.sqlite3"))
}

pub(crate) fn logs_directory() -> Result<PathBuf, String> {
    Ok(state_directory()?.join("logs"))
}

// Legacy: Remove once all users have migrated
pub(crate) fn legacy_state_path() -> Result<PathBuf, String> {
    Ok(state_directory()?.join("state.json"))
}

#[cfg(not(target_os = "windows"))]
pub(crate) fn workspace_root() -> Result<PathBuf, String> {
    if let Some(data_home) = env::var_os("XDG_DATA_HOME") {
        return Ok(PathBuf::from(data_home).join("dirigent/workspace"));
    }
    let home = home_dir().ok_or_else(|| {
        "HOME and XDG_DATA_HOME are unset; cannot create managed workspaces".to_string()
    })?;
    Ok(home.join(".local/share/dirigent/workspace"))
}

#[cfg(target_os = "windows")]
pub(crate) fn workspace_root() -> Result<PathBuf, String> {
    let data_home = env::var_os("LOCALAPPDATA")
        .map(PathBuf::from)
        .or_else(|| home_dir().map(|home| home.join("AppData/Local")))
        .ok_or_else(|| {
            "LOCALAPPDATA and USERPROFILE are unset; cannot create managed workspaces".to_string()
        })?;
    Ok(data_home.join("dirigent/workspace"))
}

#[cfg(not(target_os = "windows"))]
pub(crate) fn cache_path() -> Result<PathBuf, String> {
    if let Some(cache_home) = env::var_os("XDG_CACHE_HOME") {
        return Ok(PathBuf::from(cache_home).join("dirigent/v0/cache.sqlite3"));
    }
    let home = home_dir().ok_or_else(|| {
        "HOME and XDG_CACHE_HOME are unset; cannot initialize the session cache".to_string()
    })?;
    Ok(home.join(".cache/dirigent/v0/cache.sqlite3"))
}

#[cfg(target_os = "windows")]
pub(crate) fn cache_path() -> Result<PathBuf, String> {
    let cache_home = env::var_os("LOCALAPPDATA")
        .map(PathBuf::from)
        .or_else(|| home_dir().map(|home| home.join("AppData/Local")))
        .ok_or_else(|| {
            "LOCALAPPDATA and USERPROFILE are unset; cannot initialize the session cache"
                .to_string()
        })?;
    Ok(cache_home.join("dirigent/v0/cache.sqlite3"))
}

pub(crate) fn materialize_pi_bridge() -> Result<PathBuf, String> {
    let database = state_database_path()?;
    let directory = database
        .parent()
        .ok_or_else(|| "Dirigent state database path has no parent directory".to_string())?;
    fs::create_dir_all(directory)
        .map_err(|error| format!("could not create {}: {error}", directory.display()))?;
    let path = directory.join("pi-bridge.ts");
    if fs::read_to_string(&path).ok().as_deref() != Some(PI_BRIDGE_EXTENSION) {
        let temporary = directory.join("pi-bridge.ts.tmp");
        fs::write(&temporary, PI_BRIDGE_EXTENSION)
            .map_err(|error| format!("could not write {}: {error}", temporary.display()))?;
        fs::rename(&temporary, &path)
            .map_err(|error| format!("could not replace {}: {error}", path.display()))?;
    }
    Ok(path)
}

#[cfg(not(target_os = "windows"))]
pub(crate) fn home_dir() -> Option<PathBuf> {
    env::var_os("HOME").map(PathBuf::from)
}

#[cfg(target_os = "windows")]
pub(crate) fn home_dir() -> Option<PathBuf> {
    env::var_os("USERPROFILE")
        .map(PathBuf::from)
        .or_else(|| {
            let drive = env::var_os("HOMEDRIVE")?;
            let path = env::var_os("HOMEPATH")?;
            Some(PathBuf::from(drive).join(path))
        })
        .or_else(|| env::var_os("HOME").map(PathBuf::from))
}

#[cfg(not(target_os = "windows"))]
pub(crate) fn pi_command(nix_enabled: bool) -> Result<Command, String> {
    let command = if nix_enabled {
        let mut command = Command::new("nix");
        command.args(["develop", "--no-write-lock-file", "--command", "pi"]);
        let nix_config = env::var("NIX_CONFIG").unwrap_or_default();
        command.env("NIX_CONFIG", format!("{nix_config}\nwarn-dirty = false"));
        command
    } else {
        Command::new("pi")
    };
    Ok(command)
}

#[cfg(target_os = "windows")]
pub(crate) fn pi_command(_nix_enabled: bool) -> Result<Command, String> {
    use std::os::windows::process::CommandExt as _;

    const CREATE_NO_WINDOW: u32 = 0x0800_0000;
    let path = env::var_os("PATH").and_then(|path| {
        env::split_paths(&path).find_map(|directory| {
            ["pi.exe", "pi.cmd"]
                .into_iter()
                .map(|name| directory.join(name))
                .find(|candidate| candidate.is_file())
        })
    });
    let path = path.ok_or_else(|| {
        "could not find pi.exe or pi.cmd on PATH; install pi before starting Dirigent".to_string()
    })?;
    let mut command = Command::new(path);
    command.creation_flags(CREATE_NO_WINDOW);
    Ok(command)
}

#[cfg(not(target_os = "windows"))]
pub(crate) fn stop_child(child: &mut Child) {
    let _ = child.kill();
    let _ = child.wait();
}

#[cfg(target_os = "windows")]
pub(crate) fn stop_child(child: &mut Child) {
    use std::os::windows::process::CommandExt as _;

    const CREATE_NO_WINDOW: u32 = 0x0800_0000;
    if child.try_wait().ok().flatten().is_none() {
        let _ = Command::new("taskkill.exe")
            .args(["/PID", &child.id().to_string(), "/T", "/F"])
            .creation_flags(CREATE_NO_WINDOW)
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status();
    }
    let _ = child.kill();
    let _ = child.wait();
}
