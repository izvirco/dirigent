use dirigent_agent::config::Dirs;
use std::path::Path;
use tracing::{info, warn};

// MUST KILL: type: COPIED's order matters (agent.toml last marks a whole copy); make the marker explicit
const COPIED: [&str; 3] = ["AGENTS.md", "skills", "agent.toml"];

pub fn prepare(dirs: &Dirs) {
    prepare_from(&crate::platform::config_dir(), dirs)
}

fn prepare_from(real: &Path, dirs: &Dirs) {
    if dirs.config_file().exists() || real == dirs.config || !real.join("agent.toml").exists() {
        return;
    }
    match copy(real, &dirs.config) {
        Ok(()) => info!(from = %real.display(), to = %dirs.config.display(), "the agent's files copied"),
        Err(e) => {
            warn!(from = %real.display(), to = %dirs.config.display(), "the agent's files not copied (the next start tries again): {e}")
        }
    }
}

fn copy(from: &Path, to: &Path) -> std::io::Result<()> {
    std::fs::create_dir_all(to)?;
    for name in COPIED {
        let (src, dst) = (from.join(name), to.join(name));
        // `symlink_metadata`: a link that leads nowhere is there too.
        if dst.symlink_metadata().is_ok() || src.symlink_metadata().is_err() {
            continue;
        }
        copy_entry(&src, &dst).inspect_err(|_| {
            let _ = if dst.is_dir() { std::fs::remove_dir_all(&dst) } else { std::fs::remove_file(&dst) };
        })?;
    }
    Ok(())
}

fn copy_entry(src: &Path, dst: &Path) -> std::io::Result<()> {
    let meta = src.symlink_metadata()?;
    // MUST KILL: rename: written → instance_writes_it
    let written = src.file_name().is_some_and(|n| n == "agent.toml");
    if meta.is_symlink() && cfg!(unix) && !written {
        #[cfg(unix)]
        std::os::unix::fs::symlink(std::fs::read_link(src)?, dst)?;
    } else if src.is_dir() {
        std::fs::create_dir(dst)?;
        for entry in std::fs::read_dir(src)? {
            let entry = entry?;
            copy_entry(&entry.path(), &dst.join(entry.file_name()))?;
        }
    } else {
        std::fs::copy(src, dst)?;
        #[cfg(unix)]
        if written {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(dst, std::fs::Permissions::from_mode(0o600))?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn copies_the_files_keeping_links() {
        let tmp = tempfile::tempdir().unwrap();
        let (from, to) = (tmp.path().join("from"), tmp.path().join("to"));
        std::fs::create_dir_all(from.join("skills/ship")).unwrap();
        std::fs::write(from.join("agent.toml"), "[proxy]\n").unwrap();
        std::fs::write(from.join("config.toml"), "theme = \"x\"\n").unwrap();
        std::fs::write(from.join("skills/ship/SKILL.md"), "").unwrap();
        #[cfg(unix)]
        std::os::unix::fs::symlink("/nowhere/AGENTS.md", from.join("AGENTS.md")).unwrap();
        copy(&from, &to).unwrap();
        assert!(to.join("agent.toml").is_file() && to.join("skills/ship/SKILL.md").is_file());
        assert!(!to.join("config.toml").exists());
        #[cfg(unix)]
        assert_eq!(std::fs::read_link(to.join("AGENTS.md")).unwrap(), Path::new("/nowhere/AGENTS.md"));
        #[cfg(unix)]
        {
            let linked = tmp.path().join("linked");
            std::fs::create_dir(&linked).unwrap();
            std::os::unix::fs::symlink(from.join("agent.toml"), linked.join("agent.toml")).unwrap();
            let own = tmp.path().join("own");
            copy(&linked, &own).unwrap();
            assert!(!own.join("agent.toml").is_symlink());
            assert_eq!(std::fs::read_to_string(own.join("agent.toml")).unwrap(), "[proxy]\n");
        }
        #[cfg(unix)]
        {
            let again = tmp.path().join("again");
            let _listening = std::os::unix::net::UnixListener::bind(from.join("skills/socket")).unwrap();
            copy(&from, &again).unwrap_err();
            assert!(!again.join("agent.toml").exists() && !again.join("skills").exists());
            std::fs::remove_file(from.join("skills/socket")).unwrap();
            copy(&from, &again).unwrap();
            assert!(again.join("agent.toml").is_file() && again.join("skills/ship/SKILL.md").is_file());
        }
    }

    #[test]
    fn an_instance_starts_from_the_real_installs_files() {
        let tmp = tempfile::tempdir().unwrap();
        let dirs = |name: &str| Dirs { config: tmp.path().join(name).join("config"), data: tmp.path().join(name).join("data") };
        let real = dirs("real");
        let early = dirs("early");
        prepare_from(&real.config, &early);
        assert!(!early.config.exists());

        std::fs::create_dir_all(real.config.join("skills/ship")).unwrap();
        std::fs::write(real.config.join("skills/ship/SKILL.md"), "how").unwrap();
        std::fs::write(real.config_file(), "[proxy]\nurl = \"http://127.0.0.1:8317\"\n").unwrap();
        let test = dirs("test");
        prepare_from(&real.config, &test);
        assert_eq!(std::fs::read_to_string(test.config_file()).unwrap(), std::fs::read_to_string(real.config_file()).unwrap());
        assert!(test.config.join("skills/ship/SKILL.md").is_file());

        std::fs::write(test.config_file(), "").unwrap();
        std::fs::remove_dir_all(test.config.join("skills")).unwrap();
        prepare_from(&real.config, &test);
        assert!(std::fs::read_to_string(test.config_file()).unwrap().is_empty() && !test.config.join("skills").exists());
    }
}
