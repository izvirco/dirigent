//! Centralizes platform paths and child-process behavior.

use std::{
    env, fs,
    path::PathBuf,
    process::{Child, Command},
};

const PI_BRIDGE_EXTENSION: &str = include_str!("../asset/dirigent-bridge.ts");

// Always use the running build's channel, never the launcher's mutable selection.
fn channel_directory(base: PathBuf) -> Result<PathBuf, String> {
    let channel = crate::build_info::channel();
    dirigent_launcher::validate_channel(channel)?;
    Ok(base.join("dirigent/channels").join(channel))
}

#[cfg(not(target_os = "windows"))]
pub(crate) fn config_dir() -> Result<PathBuf, String> {
    if let Some(config_home) = env::var_os("XDG_CONFIG_HOME") {
        return channel_directory(PathBuf::from(config_home));
    }
    let home = home_dir().ok_or_else(|| {
        "HOME and XDG_CONFIG_HOME are unset; cannot load Dirigent configuration".to_string()
    })?;
    channel_directory(home.join(".config"))
}

#[cfg(target_os = "windows")]
pub(crate) fn config_dir() -> Result<PathBuf, String> {
    let config_home = env::var_os("APPDATA")
        .map(PathBuf::from)
        .or_else(|| home_dir().map(|home| home.join("AppData/Roaming")))
        .ok_or_else(|| {
            "APPDATA and USERPROFILE are unset; cannot load Dirigent configuration".to_string()
        })?;
    channel_directory(config_home)
}

#[cfg(not(target_os = "windows"))]
fn state_directory() -> Result<PathBuf, String> {
    if let Some(data_home) = env::var_os("XDG_DATA_HOME") {
        return Ok(channel_directory(PathBuf::from(data_home))?.join("v0"));
    }
    let home = home_dir().ok_or_else(|| {
        "HOME and XDG_DATA_HOME are unset; cannot persist Dirigent state".to_string()
    })?;
    Ok(channel_directory(home.join(".local/share"))?.join("v0"))
}

#[cfg(target_os = "windows")]
fn state_directory() -> Result<PathBuf, String> {
    let data_home = env::var_os("APPDATA")
        .map(PathBuf::from)
        .or_else(|| home_dir().map(|home| home.join("AppData/Roaming")))
        .ok_or_else(|| {
            "APPDATA and USERPROFILE are unset; cannot persist Dirigent state".to_string()
        })?;
    Ok(channel_directory(data_home)?.join("v0"))
}

pub(crate) fn state_database_path() -> Result<PathBuf, String> {
    Ok(state_directory()?.join("state.sqlite3"))
}

pub(crate) fn logs_directory() -> Result<PathBuf, String> {
    Ok(state_directory()?.join("logs"))
}

#[cfg(not(target_os = "windows"))]
pub(crate) fn workspace_root() -> Result<PathBuf, String> {
    if let Some(data_home) = env::var_os("XDG_DATA_HOME") {
        return Ok(channel_directory(PathBuf::from(data_home))?.join("workspace"));
    }
    let home = home_dir().ok_or_else(|| {
        "HOME and XDG_DATA_HOME are unset; cannot create managed workspaces".to_string()
    })?;
    Ok(channel_directory(home.join(".local/share"))?.join("workspace"))
}

#[cfg(target_os = "windows")]
pub(crate) fn workspace_root() -> Result<PathBuf, String> {
    let data_home = env::var_os("LOCALAPPDATA")
        .map(PathBuf::from)
        .or_else(|| home_dir().map(|home| home.join("AppData/Local")))
        .ok_or_else(|| {
            "LOCALAPPDATA and USERPROFILE are unset; cannot create managed workspaces".to_string()
        })?;
    Ok(channel_directory(data_home)?.join("workspace"))
}

#[cfg(not(target_os = "windows"))]
pub(crate) fn cache_path() -> Result<PathBuf, String> {
    if let Some(cache_home) = env::var_os("XDG_CACHE_HOME") {
        return Ok(channel_directory(PathBuf::from(cache_home))?.join("v0/cache.sqlite3"));
    }
    let home = home_dir().ok_or_else(|| {
        "HOME and XDG_CACHE_HOME are unset; cannot initialize the session cache".to_string()
    })?;
    Ok(channel_directory(home.join(".cache"))?.join("v0/cache.sqlite3"))
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
    Ok(channel_directory(cache_home)?.join("v0/cache.sqlite3"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn owned_paths_use_the_build_channel() {
        let channel = crate::build_info::channel();
        for path in [
            config_dir(),
            state_database_path(),
            cache_path(),
            logs_directory(),
            workspace_root(),
        ] {
            let path = path.unwrap();
            let components: Vec<_> = path.iter().map(|part| part.to_string_lossy()).collect();
            assert!(
                components
                    .windows(3)
                    .any(|parts| parts == ["dirigent", "channels", channel]),
                "unscoped path: {}",
                path.display()
            );
        }
        assert_eq!(
            state_database_path().unwrap().parent(),
            logs_directory().unwrap().parent()
        );
    }
}

/// Materializes a versioned private bundle so a running Pi never loads half an update.
pub(crate) fn materialize_pi_bridge() -> Result<PathBuf, String> {
    let files = [
        ("pi-bridge.ts", PI_BRIDGE_EXTENSION),
        (
            "dirigent-agents.ts",
            include_str!("../asset/dirigent-agents.ts"),
        ),
        (
            "dirigent-agent-worker.mjs",
            include_str!("../asset/dirigent-agent-worker.mjs"),
        ),
    ];
    let mut hash = blake3::Hasher::new();
    for (_, content) in &files {
        hash.update(content.as_bytes());
    }
    let database = state_database_path()?;
    let directory = database
        .parent()
        .ok_or_else(|| "Dirigent state database path has no parent directory".to_string())?
        .join("bridges")
        .join(hash.finalize().to_hex().as_str());
    fs::create_dir_all(&directory)
        .map_err(|error| format!("could not create {}: {error}", directory.display()))?;
    for (name, content) in files {
        let path = directory.join(name);
        if fs::read_to_string(&path).ok().as_deref() != Some(content) {
            let temporary = path.with_extension("tmp");
            fs::write(&temporary, content)
                .map_err(|error| format!("could not write {}: {error}", temporary.display()))?;
            fs::rename(&temporary, &path)
                .map_err(|error| format!("could not replace {}: {error}", path.display()))?;
        }
    }
    Ok(directory.join("pi-bridge.ts"))
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
pub(crate) fn hide_command_window(_command: &mut Command) {}

#[cfg(target_os = "windows")]
pub(crate) fn hide_command_window(command: &mut Command) {
    use std::os::windows::process::CommandExt as _;

    const CREATE_NO_WINDOW: u32 = 0x0800_0000;
    const SW_HIDE: u16 = 0;
    command
        .creation_flags(CREATE_NO_WINDOW)
        .show_window(SW_HIDE);
}

#[cfg(not(target_os = "windows"))]
pub(crate) fn pi_command(nix_enabled: bool) -> Result<Command, String> {
    // `nix develop --command pi` resolves the project dev shell without mutating its lock file.
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
    hide_command_window(&mut command);
    Ok(command)
}

pub(crate) fn pi_version() -> Result<String, String> {
    let output = pi_command(false)?
        .arg("--version")
        .output()
        .map_err(|error| format!("could not read pi version: {error}"))?;
    if !output.status.success() {
        return Err(format!("pi --version exited with {}", output.status));
    }
    let version = String::from_utf8_lossy(&output.stdout).trim().to_string();
    if version.is_empty() {
        return Err("pi --version returned no version".into());
    }
    Ok(version)
}

#[cfg(not(target_os = "windows"))]
pub(crate) fn stop_child(child: &mut Child) {
    let _ = child.kill();
    let _ = child.wait();
}

#[cfg(target_os = "windows")]
pub(crate) fn stop_child(child: &mut Child) {
    // Pi may be launched through a cmd/npm shim; taskkill is needed to stop that entire tree.
    if child.try_wait().ok().flatten().is_none() {
        let mut command = Command::new("taskkill.exe");
        hide_command_window(&mut command);
        let _ = command
            .args(["/PID", &child.id().to_string(), "/T", "/F"])
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status();
    }
    let _ = child.kill();
    let _ = child.wait();
}
