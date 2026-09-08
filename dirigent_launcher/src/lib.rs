//! The installed layout shared by the launcher and desktop updater.
//! Executables in `versions/` are immutable; only `current.json` is replaced.

use std::{
    fs::{self, File, OpenOptions},
    io::Write,
    path::{Path, PathBuf},
};

// Windows uses named pipes; unit tests exercise the same handoff over Unix local sockets.
#[cfg(any(windows, test))]
pub mod instance;

use fs2::FileExt;
use serde::{Deserialize, Serialize};

pub const DESKTOP_EXE: &str = if cfg!(windows) {
    "dirigent_desktop.exe"
} else {
    "dirigent_desktop"
};
pub const LAUNCHER_EXE: &str = if cfg!(windows) {
    "dirigent.exe"
} else {
    "dirigent"
};

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
pub struct Selection {
    pub channel: String,
    pub version: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub previous_version: Option<String>,
}

/// Safe on Windows and Unix. Lowercase avoids aliases on case-insensitive filesystems.
pub fn validate_channel(channel: &str) -> Result<(), String> {
    let stem = channel.split('.').next().unwrap_or_default();
    let reserved = matches!(stem, "con" | "prn" | "aux" | "nul")
        || ((stem.starts_with("com") || stem.starts_with("lpt"))
            && stem.len() == 4
            && matches!(stem.as_bytes()[3], b'1'..=b'9'));
    if channel.is_empty()
        || channel.len() > 128
        || !channel.as_bytes()[0].is_ascii_alphanumeric()
        || !channel
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"-_.+".contains(&b))
        || channel.ends_with('.')
        || channel != channel.to_ascii_lowercase()
        || reserved
    {
        return Err("invalid release channel".into());
    }
    Ok(())
}

pub fn validate_version(channel: &str, version: &str) -> Result<(), String> {
    if channel == "stable" {
        let parsed = semver::Version::parse(version).map_err(|e| format!("invalid SemVer: {e}"))?;
        if !parsed.pre.is_empty() || !parsed.build.is_empty() {
            return Err("stable versions cannot contain prerelease or build metadata".into());
        }
    } else {
        let bytes = version.as_bytes();
        if bytes.len() != 15
            || bytes[8] != b'-'
            || !bytes[..8].iter().all(u8::is_ascii_digit)
            || !bytes[9..].iter().all(u8::is_ascii_digit)
        {
            return Err("branch versions must use YYYYMMDD-HHMMSS UTC".into());
        }
    }
    Ok(())
}

pub fn is_newer(channel: &str, next: &str, current: &str) -> Result<bool, String> {
    validate_version(channel, next)?;
    validate_version(channel, current)?;
    Ok(if channel == "stable" {
        semver::Version::parse(next).unwrap() > semver::Version::parse(current).unwrap()
    } else {
        next > current
    })
}

impl Selection {
    pub fn new(channel: &str, version: &str) -> Result<Self, String> {
        validate_channel(channel)?;
        validate_version(channel, version)?;
        Ok(Self {
            channel: channel.into(),
            version: version.into(),
            previous_version: None,
        })
    }

    pub fn directory_name(&self) -> String {
        format!("{}-{}", self.channel, self.version)
    }

    pub fn executable(&self, root: &Path) -> PathBuf {
        root.join("versions")
            .join(self.directory_name())
            .join(DESKTOP_EXE)
    }

    pub fn read(root: &Path) -> Result<Self, String> {
        let bytes = fs::read(root.join("current.json"))
            .map_err(|e| format!("could not read current.json: {e}"))?;
        let selection: Self =
            serde_json::from_slice(&bytes).map_err(|e| format!("invalid current.json: {e}"))?;
        Self::new(&selection.channel, &selection.version)?;
        if let Some(previous) = &selection.previous_version {
            validate_version(&selection.channel, previous)?;
        }
        Ok(selection)
    }

    /// Call with the installation's exclusive lock held. A failed write leaves the old selection intact.
    pub fn write(&self, root: &Path) -> Result<(), String> {
        Self::new(&self.channel, &self.version)?;
        if let Some(previous) = &self.previous_version {
            validate_version(&self.channel, previous)?;
        }
        let mut temporary = tempfile::NamedTempFile::new_in(root).map_err(|e| e.to_string())?;
        serde_json::to_writer(&mut temporary, self).map_err(|e| e.to_string())?;
        temporary.write_all(b"\n").map_err(|e| e.to_string())?;
        temporary.as_file().sync_all().map_err(|e| e.to_string())?;
        temporary
            .persist(root.join("current.json"))
            .map_err(|e| format!("could not select version: {e}"))?;
        Ok(())
    }
}

/// Shared by launches; exclusive for publication and cleanup. Dropping the file releases it.
pub fn lock(root: &Path, exclusive: bool) -> Result<File, String> {
    let file = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(root.join("install.lock"))
        .map_err(|e| format!("could not open install lock: {e}"))?;
    if exclusive {
        file.lock_exclusive()
    } else {
        FileExt::lock_shared(&file)
    }
    .map_err(|e| format!("could not lock installation: {e}"))?;
    Ok(file)
}

/// Source/package-managed binaries are deliberately not self-updatable.
pub fn installation_root(
    executable: &Path,
    channel: &str,
    version: &str,
) -> Result<PathBuf, String> {
    let selection = Selection::new(channel, version)?;
    let root = executable
        .parent()
        .and_then(Path::parent)
        .and_then(Path::parent)
        .ok_or("executable is not in an installed version directory")?;
    if selection.executable(root) != executable || !root.join(LAUNCHER_EXE).is_file() {
        return Err("executable is not in an installed version directory".into());
    }
    if Selection::read(root)?.channel != channel {
        return Err("installation belongs to a different channel".into());
    }
    Ok(root.to_path_buf())
}

/// Publish an already verified directory on the same filesystem, then atomically select it.
/// The caller holds the exclusive install lock, including across any subsequent rollback.
pub fn publish(root: &Path, staged: &Path, mut next: Selection) -> Result<Selection, String> {
    let current = Selection::read(root)?;
    if next.channel != current.channel {
        return Err("update belongs to a different channel".into());
    }
    if !is_newer(&next.channel, &next.version, &current.version)? {
        return Err("this installation already has this version or a newer one".into());
    }
    if !staged.join(DESKTOP_EXE).is_file() {
        return Err("staged update has no desktop executable".into());
    }
    let destination = root.join("versions").join(next.directory_name());
    // An unselected version may remain after a failed selection write. Never overwrite it:
    // it could still be running after a manual rollback. Select the existing immutable build.
    if !destination.exists() {
        fs::rename(staged, &destination).map_err(|e| format!("could not publish update: {e}"))?;
    } else if !destination.join(DESKTOP_EXE).is_file() {
        return Err("existing version directory is incomplete".into());
    }
    next.previous_version = Some(current.version.clone());
    next.write(root)?;
    Ok(current)
}

/// Keep the selected, previous and calling versions. Locked old binaries are retried next time.
/// Call under the exclusive installation lock so a launcher cannot race selection and cleanup.
pub fn cleanup(root: &Path, running_version: &str) -> Result<(), String> {
    let current = Selection::read(root)?;
    let prefix = format!("{}-", current.channel);
    for entry in fs::read_dir(root.join("versions")).map_err(|e| e.to_string())? {
        let entry = entry.map_err(|e| e.to_string())?;
        let name = entry.file_name();
        let Some(version) = name.to_str().and_then(|name| name.strip_prefix(&prefix)) else {
            continue;
        };
        if validate_version(&current.channel, version).is_err()
            || version == current.version
            || version == running_version
            || current.previous_version.as_deref() == Some(version)
            || !entry.file_type().map_err(|e| e.to_string())?.is_dir()
        {
            continue;
        }
        let _ = fs::remove_dir_all(entry.path());
    }
    Ok(())
}

/// Reports launch failures even for Windows GUI-subsystem executables without a console.
#[cfg(not(windows))]
pub fn show_error(message: &str) {
    eprintln!("{message}");
}

#[cfg(windows)]
pub fn show_error(message: &str) {
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

/// Inno Setup uses this channel-scoped marker to require desktop processes to exit
/// before installer upgrades/uninstallation. It does not enforce a single app instance.
#[cfg(windows)]
pub fn installer_marker(channel: &str) -> Result<std::os::windows::io::OwnedHandle, String> {
    use std::os::windows::io::FromRawHandle;
    use windows_sys::Win32::System::Threading::CreateMutexW;
    validate_channel(channel)?;
    let name: Vec<u16> = format!("Dirigent-{channel}")
        .encode_utf16()
        .chain(Some(0))
        .collect();
    // The OS copies the name; OwnedHandle closes our reference at process shutdown.
    let handle = unsafe { CreateMutexW(std::ptr::null(), 0, name.as_ptr()) };
    if handle.is_null() {
        return Err(format!(
            "could not create installer marker: {}",
            std::io::Error::last_os_error()
        ));
    }
    Ok(unsafe { std::os::windows::io::OwnedHandle::from_raw_handle(handle) })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn install(root: &Path, channel: &str, version: &str) -> Selection {
        let selection = Selection::new(channel, version).unwrap();
        fs::create_dir_all(selection.executable(root).parent().unwrap()).unwrap();
        fs::write(selection.executable(root), b"old").unwrap();
        fs::write(root.join(LAUNCHER_EXE), b"launcher").unwrap();
        selection.write(root).unwrap();
        selection
    }

    #[test]
    fn channel_versions_and_paths() {
        assert!(is_newer("stable", "1.10.0", "1.9.0").unwrap());
        assert!(is_newer("unstable", "20260908-102911", "20260908-102910").unwrap());
        for (channel, version) in [
            ("..", "20260908-102910"),
            ("unstable", "../escape"),
            ("stable", "1.0.0/escape"),
            ("CON", "20260908-102910"),
            ("con", "20260908-102910"),
            ("Unstable", "20260908-102910"),
        ] {
            assert!(Selection::new(channel, version).is_err());
        }
    }

    #[test]
    fn publication_preserves_old_executable_and_prevents_stale_or_cross_channel_updates() {
        let root = tempfile::tempdir().unwrap();
        let old = install(root.path(), "unstable", "20260908-102910");
        let _lock = lock(root.path(), true).unwrap();
        let stage = tempfile::tempdir_in(root.path().join("versions")).unwrap();
        fs::write(stage.path().join(DESKTOP_EXE), b"new").unwrap();
        let next = Selection::new("unstable", "20260908-102911").unwrap();
        assert_eq!(
            publish(root.path(), stage.path(), next.clone()).unwrap(),
            old
        );
        assert_eq!(fs::read(old.executable(root.path())).unwrap(), b"old");
        assert_eq!(fs::read(next.executable(root.path())).unwrap(), b"new");
        let selected = Selection::read(root.path()).unwrap();
        assert_eq!(
            selected.previous_version.as_deref(),
            Some(old.version.as_str())
        );
        assert!(publish(root.path(), stage.path(), next).is_err());
        assert!(
            publish(
                root.path(),
                stage.path(),
                Selection::new("stable", "1.0.0").unwrap()
            )
            .is_err()
        );
        assert_eq!(Selection::read(root.path()).unwrap(), selected);
        assert_eq!(
            installation_root(&old.executable(root.path()), &old.channel, &old.version).unwrap(),
            root.path()
        );
        old.write(root.path()).unwrap(); // Explicit rollback needs no executable replacement.
        assert_eq!(Selection::read(root.path()).unwrap(), old);
    }

    #[test]
    fn cleanup_keeps_selected_previous_and_running_versions() {
        let root = tempfile::tempdir().unwrap();
        install(root.path(), "stable", "1.0.0");
        install(root.path(), "stable", "1.1.0");
        install(root.path(), "stable", "1.2.0");
        let mut current = install(root.path(), "stable", "1.3.0");
        current.previous_version = Some("1.2.0".into());
        current.write(root.path()).unwrap();
        cleanup(root.path(), "1.1.0").unwrap();
        assert!(!root.path().join("versions/stable-1.0.0").exists());
        for version in ["1.1.0", "1.2.0", "1.3.0"] {
            assert!(
                Selection::new("stable", version)
                    .unwrap()
                    .executable(root.path())
                    .exists()
            );
        }
    }
}
