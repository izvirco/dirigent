use dirigent_agent::config::{self, Dirs, Proxy};
use std::path::Path;
use tracing::{info, warn};

const COPIED: [&str; 3] = ["AGENTS.md", "skills", "agent.toml"];

pub fn prepare(dirs: &Dirs) {
    prepare_from(&Dirs { config: crate::platform::config_dir(), data: crate::platform::data_dir() }, dirs)
}

fn prepare_from(real: &Dirs, dirs: &Dirs) {
    if !dirs.config_file().exists() && real.config_file().exists() {
        if let Err(e) = copy_proxy(real, dirs) {
            warn!(from = %real.data.display(), "the proxy not copied: {e:#}");
        }
        let (from, to) = (&real.config, &dirs.config);
        match copy(from, to) {
            Ok(()) => info!(from = %from.display(), to = %to.display(), "the agent's files copied"),
            Err(e) => warn!(from = %from.display(), to = %to.display(), "the agent's files not copied (the next start tries again): {e}"),
        }
    }
    match config::move_proxy(dirs) {
        Ok(true) => info!(to = %dirs.proxy_file().display(), "the proxy moved out of agent.toml"),
        Ok(false) => {}
        Err(e) => warn!("agent.toml still has the proxy (the next start, or a save of it, tries again): {e:#}"),
    }
}

fn copy_proxy(real: &Dirs, dirs: &Dirs) -> anyhow::Result<()> {
    let theirs = Proxy::load(real)?;
    Proxy::update(dirs, |own| {
        if own.url.is_empty() {
            *own = Proxy { management_key: String::new(), ..theirs };
        }
        Ok(())
    })
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
    let instance_writes_it = src.file_name().is_some_and(|n| n == "agent.toml");
    if meta.is_symlink() && cfg!(unix) && !instance_writes_it {
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
        if instance_writes_it {
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
        let proxy = |url: &str, api_key: &str, management_key: &str| Proxy {
            url: url.into(),
            api_key: api_key.into(),
            management_key: management_key.into(),
        };
        let set = |dirs: &Dirs, to: Proxy| {
            Proxy::update(dirs, |proxy| {
                *proxy = to;
                Ok(())
            })
            .unwrap()
        };
        let real = dirs("real");
        let early = dirs("early");
        prepare_from(&real, &early);
        assert!(!early.config.exists() && !early.data.exists());

        std::fs::create_dir_all(real.config.join("skills/ship")).unwrap();
        std::fs::write(real.config.join("skills/ship/SKILL.md"), "how").unwrap();
        std::fs::write(real.config_file(), "[compaction]\nreserve = 5\n").unwrap();
        set(&real, proxy("http://127.0.0.1:8317", "k", "m"));
        let test = dirs("test");
        prepare_from(&real, &test);
        assert_eq!(std::fs::read_to_string(test.config_file()).unwrap(), std::fs::read_to_string(real.config_file()).unwrap());
        assert!(test.config.join("skills/ship/SKILL.md").is_file());
        assert_eq!(Proxy::load(&test).unwrap(), proxy("http://127.0.0.1:8317", "k", ""));
        assert_eq!(Proxy::load(&real).unwrap().management_key, "m");

        std::fs::write(test.config_file(), "").unwrap();
        std::fs::remove_dir_all(test.config.join("skills")).unwrap();
        set(&test, proxy("http://own:1", "own", ""));
        prepare_from(&real, &test);
        assert!(std::fs::read_to_string(test.config_file()).unwrap().is_empty() && !test.config.join("skills").exists());
        assert_eq!(Proxy::load(&test).unwrap().url, "http://own:1");

        let beside = Dirs { config: tmp.path().join("beside"), data: real.data.clone() };
        prepare_from(&real, &beside);
        assert_eq!(Proxy::load(&real).unwrap(), proxy("http://127.0.0.1:8317", "k", "m"));
    }

    #[test]
    fn an_instance_starts_from_a_real_install_that_is_a_0_0_10() {
        let tmp = tempfile::tempdir().unwrap();
        let dirs = |name: &str| Dirs { config: tmp.path().join(name).join("config"), data: tmp.path().join(name).join("data") };
        let (real, test) = (dirs("real"), dirs("test"));
        std::fs::create_dir_all(&real.config).unwrap();
        std::fs::create_dir_all(&real.data).unwrap();
        let theirs = "[proxy]\nurl = \"http://127.0.0.1:8317\"\nkey = \"k\"\n";
        std::fs::write(real.config_file(), theirs).unwrap();
        std::fs::write(real.proxy_file(), r#"{"url": "http://127.0.0.1:8317", "key": "m"}"#).unwrap();
        prepare_from(&real, &test);
        let own = Proxy::load(&test).unwrap();
        assert_eq!((own.url.as_str(), own.api_key.as_str(), own.management_key.as_str()), ("http://127.0.0.1:8317", "k", ""));
        assert!(!std::fs::read_to_string(test.config_file()).unwrap().contains("proxy"));
        assert_eq!(std::fs::read_to_string(real.config_file()).unwrap(), theirs);
    }
}
