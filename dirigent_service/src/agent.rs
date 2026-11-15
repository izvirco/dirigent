//! The agent's files, before the service first uses them: `agent.toml`, the user's
//! `AGENTS.md` and `skills/` in the config dir (`dirigent_agent::config`).
//!
//! A config dir without an `agent.toml` gets its files from what came before, once:
//!
//! - pi's agent dir in it (`<config dir>/agent`, from when dirigent ran pi): imported
//!   (`dirigent_agent::import`, `dirigent_agent::mcp::import_pi`), so nobody sets their
//!   proxy up again or loses their instructions, skills, MCP servers and their sign-ins.
//!   That is a step of the migration (`migrate`; `migration.rs`, which has copied the
//!   config dir before, and removes pi's dir after if all of it is in that copy). It
//!   goes as far as it can: what it could not bring over is the user's to do, and the
//!   migration's to say.
//! - else, for a `--config-dir` instance, the real install's config dir (`prepare`): its
//!   files are copied (or its pi agent dir imported, which stays), so a test instance
//!   works like the real one. Symlinks are copied as symlinks (home-manager keeps pointing
//!   them), but for `agent.toml`, which the instance writes: it gets a file of its own.
//!
//! `agent.toml` is what says this was done, and is written last. A copy is whole or made
//! again by the next start.
//!
//! The proxy's management key is no part of this: it is in the data dir (`proxy.rs`), and
//! a test instance gets none. Nor are the real install's MCP sign-ins: a test instance
//! that refreshed a copy of one would sign the real install out (`import_pi`), so none
//! imports any.

use dirigent_agent::config::{Config, Dirs};
use dirigent_agent::import::Imported;
use std::path::Path;
use tracing::{info, warn};

/// What a real install's config dir hands a test instance. `agent.toml` last: a copy that
/// failed before it is made again by the next start.
const COPIED: [&str; 3] = ["AGENTS.md", "skills", "agent.toml"];

/// The install has a pi agent dir of its own that is yet to be brought over (`migrate`):
/// reason enough for a migration. (One that is made for pi's db brings the dir over too,
/// `agent.toml` or not.)
pub fn pending(dirs: &Dirs) -> bool {
    !dirs.config_file().exists() && dirs.config.join("agent").is_dir()
}

/// Bring the install's own pi agent dir over, as far as it goes. Returns what was done,
/// and what could not be brought over. An error is `agent.toml` not written: nothing
/// counts as imported then.
pub fn migrate(dirs: &Dirs) -> anyhow::Result<Imported> {
    // MCP sign-ins are the real install's alone. A test instance takes none from its pi
    // dir (a copy of the real one, when instances were given one).
    let sign_ins = dirs.config == crate::platform::config_dir();
    import(&dirs.config.join("agent"), dirs, sign_ins)
}

/// Give a test instance's `dirs.config` the agent's files if it has none yet: the real
/// install's. (An install's own pi dir is the migration's, `migrate`.)
pub fn prepare(dirs: &Dirs) {
    prepare_from(&crate::platform::config_dir(), dirs)
}

/// `prepare`, where the real install's config dir is `real`.
fn prepare_from(real: &Path, dirs: &Dirs) {
    if dirs.config_file().exists() || pending(dirs) || real == dirs.config {
        return;
    }
    if real.join("agent.toml").exists() {
        match copy(real, &dirs.config) {
            Ok(()) => info!(from = %real.display(), to = %dirs.config.display(), "the agent's files copied"),
            Err(e) => {
                warn!(from = %real.display(), to = %dirs.config.display(), "the agent's files not copied (the next start tries again): {e}")
            }
        }
        return;
    }
    // A real install that still is as pi left it.
    let pi = real.join("agent");
    if !pi.is_dir() {
        return;
    }
    match import(&pi, dirs, false) {
        Ok(Imported { done, left }) => {
            info!(from = %pi.display(), to = %dirs.config.display(), "pi's agent dir imported");
            done.iter().for_each(|note| info!("import: {note}"));
            left.iter().for_each(|note| warn!("import: {note}"));
        }
        Err(e) => warn!(from = %pi.display(), "pi's agent dir not imported (the next start tries again): {e:#}"),
    }
}

/// Bring the pi agent dir `pi` into `dirs`, as far as it goes, with its MCP `sign_ins` or
/// without. Returns what it did, and what it could not bring over. An error is
/// `agent.toml` not written: nothing counts as imported then.
fn import(pi: &Path, dirs: &Dirs, sign_ins: bool) -> anyhow::Result<Imported> {
    let mut imported = Imported::default();
    // Written even with nothing in it, and last: the file is what says the import was done.
    Config::update(dirs, |config| {
        imported = dirigent_agent::import::agent_dir(pi, dirs, config);
        let servers = dirigent_agent::mcp::import_pi(pi, dirs, sign_ins);
        imported.left.extend(servers.notes);
        for (name, server) in servers.servers {
            imported.done.push(format!("MCP server {name} (from mcp.json)"));
            config.mcp.entry(name).or_insert(server);
        }
        Ok(())
    })?;
    Ok(imported)
}

/// Copy the agent's files from one config dir to another.
fn copy(from: &Path, to: &Path) -> std::io::Result<()> {
    std::fs::create_dir_all(to)?;
    for name in COPIED {
        let (src, dst) = (from.join(name), to.join(name));
        // `symlink_metadata`: a link that leads nowhere is there too.
        if dst.symlink_metadata().is_ok() || src.symlink_metadata().is_err() {
            continue;
        }
        copy_entry(&src, &dst).inspect_err(|_| {
            // Half a copy would count as there.
            let _ = if dst.is_dir() { std::fs::remove_dir_all(&dst) } else { std::fs::remove_file(&dst) };
        })?;
    }
    Ok(())
}

/// Copy a file or a tree, keeping symlinks as symlinks on unix (on Windows they're followed).
/// Not `agent.toml`'s: Settings in the copy would write through it into the real install's.
fn copy_entry(src: &Path, dst: &Path) -> std::io::Result<()> {
    let meta = src.symlink_metadata()?;
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
        // `agent.toml` holds the proxy's key.
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
        // The desktop's own settings are not the agent's.
        assert!(!to.join("config.toml").exists());
        #[cfg(unix)]
        assert_eq!(std::fs::read_link(to.join("AGENTS.md")).unwrap(), Path::new("/nowhere/AGENTS.md"));
        // But for `agent.toml`, which the copy writes: a file of its own.
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
        // A copy that fails (a socket can't be copied) leaves nothing half there and no
        // `agent.toml`, so the next start copies again.
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

    /// A pi agent dir with a proxy, a skill and an MCP server that is signed in to.
    fn pi_dir(pi: &Path) {
        std::fs::create_dir_all(pi.join("skills/commit")).unwrap();
        std::fs::write(pi.join("skills/commit/SKILL.md"), "how").unwrap();
        std::fs::write(pi.join("cliproxyapi.json"), r#"{"baseUrl":"http://127.0.0.1:8317","apiKey":"k"}"#).unwrap();
        std::fs::write(pi.join("mcp.json"), r#"{"mcpServers":{"docs":{"url":"https://docs.dev/mcp","oauth":{"clientId":"mine"}}}}"#)
            .unwrap();
        std::fs::write(pi.join("mcp-auth.json"), r#"{"https://docs.dev/mcp":{"tokens":{"access_token":"a","refresh_token":"r"}}}"#)
            .unwrap();
    }

    /// What an import can't bring over is the user's to know: the rest is there, and so is
    /// `agent.toml`, with which the next start imports nothing.
    #[cfg(unix)]
    #[test]
    fn what_an_import_leaves_is_for_the_user() {
        let tmp = tempfile::tempdir().unwrap();
        let dirs = Dirs { config: tmp.path().join("config"), data: tmp.path().join("data") };
        let pi = dirs.config.join("agent");
        pi_dir(&pi);
        std::fs::write(
            pi.join("mcp.json"),
            r#"{"mcpServers":{"docs":{"url":"https://docs.dev/mcp"},"old":{"type":"sse","url":"https://old.dev/sse"}}}"#,
        )
        .unwrap();
        // A file that can't be copied: a socket.
        let _listening = std::os::unix::net::UnixListener::bind(pi.join("skills/commit/socket")).unwrap();
        let left = import(&pi, &dirs, true).unwrap().left;
        assert_eq!(left.len(), 3, "{left:?}");
        assert!(left[0].starts_with(&format!("{} was not copied (", pi.join("skills").display())), "{left:?}");
        assert!(left[1].starts_with("MCP server old is left out: "), "{left:?}");
        assert!(left[2].starts_with("MCP server docs: sign in again "), "{left:?}");
        let config = Config::load(&dirs).unwrap();
        assert_eq!((config.proxy.url.as_str(), config.mcp.contains_key("docs")), ("http://127.0.0.1:8317", true));
        assert!(dirs.config_file().exists() && !dirs.config.join("skills").exists());
    }

    /// Where an instance's files come from: its own pi dir (the migration's), else the real
    /// install's. A test instance takes no MCP sign-in of pi's.
    #[test]
    fn an_instance_starts_from_what_came_before() {
        let tmp = tempfile::tempdir().unwrap();
        let dirs = |name: &str| Dirs { config: tmp.path().join(name).join("config"), data: tmp.path().join(name).join("data") };
        let signed_in = |dirs: &Dirs| std::fs::read_to_string(dirs.data.join("mcp-auth.json")).is_ok_and(|stored| stored.contains("\"r\""));
        let proxy = |dirs: &Dirs| Config::load(dirs).unwrap().proxy.url;

        // A real install that is as pi left it hands a test instance what its pi dir has.
        let real = dirs("real");
        pi_dir(&real.config.join("agent"));
        let early = dirs("early");
        prepare_from(&real.config, &early);
        assert_eq!(proxy(&early), "http://127.0.0.1:8317");
        assert!(real.config.join("agent").is_dir() && !real.config_file().exists() && !signed_in(&early));

        // An instance with a pi dir of its own has that to migrate: nothing is prepared.
        let own = dirs("own");
        pi_dir(&own.config.join("agent"));
        std::fs::write(own.config.join("agent/cliproxyapi.json"), r#"{"baseUrl":"http://127.0.0.1:9999","apiKey":"k"}"#).unwrap();
        prepare_from(&real.config, &own);
        assert!(pending(&own) && !own.config_file().exists());
        // Migrated, it is pi's no more, and no sign-in came along (it is no real install).
        assert_eq!(migrate(&own).unwrap().left, Vec::<String>::new());
        assert_eq!(proxy(&own), "http://127.0.0.1:9999");
        assert!(!pending(&own));
        assert!(Config::load(&own).unwrap().mcp.contains_key("docs") && !signed_in(&own));

        // The real install's own import is signed in as pi was.
        import(&real.config.join("agent"), &real, true).unwrap();
        assert!(signed_in(&real));
        // From then on a test instance gets a copy of its files.
        let test = dirs("test");
        prepare_from(&real.config, &test);
        assert_eq!(proxy(&test), "http://127.0.0.1:8317");
        assert!(test.config.join("skills/commit/SKILL.md").is_file() && !signed_in(&test));
    }
}
