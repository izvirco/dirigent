use anyhow::{Context, Result, anyhow};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, MutexGuard, PoisonError};

#[derive(Debug, Clone, PartialEq)]
pub struct Dirs {
    pub config: PathBuf,
    pub data: PathBuf,
}

impl Dirs {
    pub fn config_file(&self) -> PathBuf {
        self.config.join("agent.toml")
    }

    pub fn proxy_file(&self) -> PathBuf {
        self.data.join("proxy.json")
    }
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Config {
    pub compaction: Compaction,
    #[serde(skip_serializing_if = "BTreeMap::is_empty")]
    pub models: BTreeMap<String, ModelConfig>,
    #[serde(skip_serializing_if = "BTreeMap::is_empty")]
    pub mcp: BTreeMap<String, crate::mcp::Server>,
}

/// This host's CLIProxyAPI, in `proxy.json` in the data dir.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Proxy {
    /// Its address, without a trailing slash; empty: none set, nothing runs.
    pub url: String,
    #[serde(skip_serializing_if = "String::is_empty")]
    pub api_key: String,
    /// Opens its management API, which gives out the tokens of every account behind it:
    /// never copied to another instance.
    #[serde(alias = "key", skip_serializing_if = "String::is_empty")]
    pub management_key: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Compaction {
    pub reserve: u64,
    pub keep_recent: u64,
}

impl Default for Compaction {
    fn default() -> Self {
        Self { reserve: 16_384, keep_recent: 20_000 }
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct ModelConfig {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reserve: Option<u64>,
}

static WRITING: Mutex<()> = Mutex::new(());

// MUST KILL: rename: writing -> lock_ignoring_poison, so recovering a poisoned WRITING is in the name
fn writing() -> MutexGuard<'static, ()> {
    WRITING.lock().unwrap_or_else(PoisonError::into_inner)
}

impl Config {
    /// The file as it is now; the defaults without one. A file that doesn't parse is an
    /// error, not the defaults: saving over it would lose what it holds.
    pub fn load(dirs: &Dirs) -> Result<Self> {
        let path = dirs.config_file();
        match std::fs::read_to_string(&path) {
            Ok(text) => toml::from_str(&text).map_err(|e| anyhow!("{}: {}", path.display(), wrong(&e, &text))),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Self::default()),
            Err(e) => Err(e).with_context(|| format!("read {}", path.display())),
        }
    }

    /// Change the file: `change` gets what it holds now, and what it leaves is saved.
    /// `change` runs with the file held (`WRITING`): it can't save or update itself.
    pub fn update(dirs: &Dirs, change: impl FnOnce(&mut Config) -> Result<()>) -> Result<()> {
        let _one_at_a_time = writing();
        // The save writes what `Config` holds: a 0.0.10's `[proxy]` that is still there would go, its key with it.
        move_proxy_held(dirs)?;
        let mut config = Self::load(dirs)?;
        change(&mut config)?;
        config.write(dirs)
    }

    fn write(&self, dirs: &Dirs) -> Result<()> {
        let text = format!("# Dirigent's agent. Also edited from Settings in the app.\n\n{}", toml::to_string(self)?);
        write_whole(&dirs.config_file(), &text)
    }

    pub fn reserve(&self, model: &str) -> u64 {
        self.models.get(model).and_then(|m| m.reserve).unwrap_or(self.compaction.reserve)
    }
}

impl Proxy {
    /// The file as it is now; no proxy without one. A file that doesn't parse is an error,
    /// which says where and quotes nothing: the file holds keys.
    pub fn load(dirs: &Dirs) -> Result<Self> {
        let path = dirs.proxy_file();
        match std::fs::read_to_string(&path) {
            Ok(text) => {
                serde_json::from_str(&text).map_err(|e| anyhow!("{}: not read (line {}, column {})", path.display(), e.line(), e.column()))
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Self::default()),
            Err(e) => Err(e).with_context(|| format!("read {}", path.display())),
        }
    }

    /// Change the file: `change` gets what it holds now, and what it leaves is saved, if it
    /// is something else. No proxy is no file. A file that can't be read counts as no proxy
    /// and is replaced: it holds nothing but what Settings › Proxy sets.
    pub fn update(dirs: &Dirs, change: impl FnOnce(&mut Proxy) -> Result<()>) -> Result<()> {
        let _one_at_a_time = writing();
        let held = Self::load(dirs).ok();
        let mut proxy = held.clone().unwrap_or_default();
        change(&mut proxy)?;
        if held.as_ref() == Some(&proxy) { Ok(()) } else { proxy.write(dirs) }
    }

    fn write(&self, dirs: &Dirs) -> Result<()> {
        let path = dirs.proxy_file();
        if *self != Self::default() {
            return write_whole(&path, &serde_json::to_string_pretty(self)?);
        }
        match std::fs::remove_file(&path) {
            Err(e) if e.kind() != std::io::ErrorKind::NotFound => Err(e).with_context(|| format!("remove {}", path.display())),
            _ => Ok(()),
        }
    }
}

/// 0.0.10 kept the proxy in `agent.toml` (`[proxy]`, with its `url` and API `key`): move it
/// to `proxy.json`. True if `agent.toml` had one. For a start to call: nothing reads the
/// proxy from `agent.toml` any more.
///
/// A `proxy.json` that has an API key is this host's proxy already, and stays as it is.
/// A management key is 0.0.10's own, and stays if it is for the same address.
pub fn move_proxy(dirs: &Dirs) -> Result<bool> {
    let _one_at_a_time = writing();
    move_proxy_held(dirs)
}

fn move_proxy_held(dirs: &Dirs) -> Result<bool> {
    #[derive(Deserialize)]
    struct Before {
        proxy: Option<Old>,
    }
    #[derive(Default, Deserialize)]
    #[serde(default)]
    struct Old {
        url: String,
        key: String,
    }
    let path = dirs.config_file();
    let text = match std::fs::read_to_string(&path) {
        Ok(text) => text,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(false),
        Err(e) => return Err(e).with_context(|| format!("read {}", path.display())),
    };
    let before: Before = toml::from_str(&text).map_err(|e| anyhow!("{}: {}", path.display(), wrong(&e, &text)))?;
    let Some(old) = before.proxy else { return Ok(false) };
    // One that can't be read has no API key: 0.0.10 used nothing of such a file.
    let held = Proxy::load(dirs).unwrap_or_default();
    if held.api_key.is_empty() {
        let management_key = if held.url == old.url { held.management_key } else { String::new() };
        Proxy { url: old.url, api_key: old.key, management_key }.write(dirs)?;
    }
    Config::load(dirs)?.write(dirs)?;
    Ok(true)
}

fn write_whole(path: &Path, text: &str) -> Result<()> {
    let file = linked(path);
    let mut partial = file.clone().into_os_string();
    partial.push(".part");
    let written = (|| {
        if let Some(dir) = file.parent() {
            std::fs::create_dir_all(dir)?;
        }
        write_private(Path::new(&partial), text)?;
        std::fs::rename(&partial, &file)
    })();
    written.with_context(|| format!("write {}", path.display()))
}

// MUST KILL: rename: wrong -> describe_without_quoting, it must never echo the file's text
fn wrong(error: &toml::de::Error, text: &str) -> String {
    // "invalid type: string \"…\", expected a map": what was expected names no value.
    let message = match error.message().rsplit_once(", expected ") {
        Some((_, expected)) => format!("expected {expected}"),
        None => error.message().replace('\n', ", "),
    };
    let Some(before) = error.span().and_then(|span| text.get(..span.start)) else { return message };
    let (line, column) = (before.matches('\n').count() + 1, before.chars().rev().take_while(|c| *c != '\n').count() + 1);
    format!("line {line}, column {column}: {message}")
}

// MUST KILL: rename: linked -> follow_symlinks, with its 40 as a named MAX_SYMLINK_HOPS const
fn linked(path: &Path) -> PathBuf {
    let mut file = path.to_owned();
    for _ in 0..40 {
        let Ok(target) = std::fs::read_link(&file) else { break };
        file = file.parent().unwrap_or(Path::new("")).join(target);
    }
    file
}

/// Write `text` to `path` readable by its owner only (unix): it holds keys.
pub fn write_private(path: &Path, text: &str) -> std::io::Result<()> {
    use std::io::Write;
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create(true).truncate(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
        options.mode(0o600);
        // `mode` only applies to a new file: tighten an existing one before writing to it.
        if path.exists() {
            std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))?;
        }
    }
    let mut file = options.open(path)?;
    file.write_all(text.as_bytes())?;
    // On disk before a rename puts it in the place of what was there.
    file.sync_all()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_file_round_trips_and_is_private() {
        let tmp = tempfile::tempdir().unwrap();
        let dirs = Dirs { config: tmp.path().join("config"), data: tmp.path().join("data") };
        assert_eq!(Config::load(&dirs).unwrap(), Config::default());
        Config::update(&dirs, |c| {
            c.models.insert("claude-opus-5-5".into(), ModelConfig { reserve: Some(350_000) });
            Ok(())
        })
        .unwrap();
        let config = Config::load(&dirs).unwrap();
        assert_eq!((config.reserve("claude-opus-5-5"), config.reserve("other")), (350_000, 16_384));
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(std::fs::metadata(dirs.config_file()).unwrap().permissions().mode() & 0o777, 0o600);
        }
        std::fs::write(dirs.config_file(), "[proxy").unwrap();
        assert!(Config::load(&dirs).is_err());
    }

    #[test]
    fn a_file_that_does_not_parse_is_not_quoted() {
        let tmp = tempfile::tempdir().unwrap();
        let dirs = Dirs { config: tmp.path().to_owned(), data: tmp.path().join("data") };
        let said = |text: &str| {
            std::fs::write(dirs.config_file(), text).unwrap();
            let said = format!("{:#}", Config::load(&dirs).unwrap_err());
            assert!(!said.contains("SECRET"), "{said}");
            said.strip_prefix(&format!("{}: ", dirs.config_file().display())).expect("the file's path first").to_owned()
        };
        assert_eq!(said("[proxy]\nurl = \"http://x\"\nkey = sk-SECRET\n"), "line 3, column 7: invalid string, expected `\"`, `'`");
        assert_eq!(said("[proxy]\nkey = \"sk-SECRET\n"), "line 2, column 17: invalid basic string");
        assert_eq!(said("[mcp.x]\ncommand = \"srv\"\nenv = \"TOKEN=sk-SECRET\"\n"), "line 3, column 7: expected a map");
        assert_eq!(said("[mcp.x]\nexposure = \"sk-SECRET, expected nothing\"\n"), "line 2, column 12: expected `codemode` or `direct`");
    }

    #[test]
    fn a_save_is_whole_and_loses_no_change() {
        let tmp = tempfile::tempdir().unwrap();
        let dirs = Dirs { config: tmp.path().join("config"), data: tmp.path().join("data") };
        let compaction = Compaction { reserve: 7, keep_recent: 9 };
        Config::update(&dirs, |config| {
            config.compaction = compaction;
            Ok(())
        })
        .unwrap();
        let done = std::sync::atomic::AtomicBool::new(false);
        std::thread::scope(|scope| {
            let reading = scope.spawn(|| {
                while !done.load(std::sync::atomic::Ordering::Relaxed) {
                    assert_eq!(Config::load(&dirs).unwrap().compaction, compaction);
                }
            });
            let dirs = &dirs;
            let change = |writer: u32| {
                move || {
                    for n in 0..25 {
                        Config::update(dirs, |config| {
                            config.models.insert(format!("m{writer}-{n}"), ModelConfig::default());
                            Ok(())
                        })
                        .unwrap();
                    }
                }
            };
            let writing: Vec<_> = (0..4).map(|writer| scope.spawn(change(writer))).collect();
            writing.into_iter().for_each(|writer| writer.join().unwrap());
            done.store(true, std::sync::atomic::Ordering::Relaxed);
            reading.join().unwrap();
        });
        assert_eq!(Config::load(&dirs).unwrap().models.len(), 100);
        assert!(!dirs.config.join("agent.toml.part").exists());
    }

    #[cfg(unix)]
    #[test]
    fn a_linked_file_is_written_where_it_leads() {
        let tmp = tempfile::tempdir().unwrap();
        let dirs = Dirs { config: tmp.path().join("config"), data: tmp.path().join("data") };
        std::fs::create_dir_all(tmp.path().join("dotfiles")).unwrap();
        std::fs::create_dir_all(&dirs.config).unwrap();
        std::os::unix::fs::symlink("../dotfiles/agent.toml", dirs.config_file()).unwrap();
        for reserve in [111, 222] {
            Config::update(&dirs, |config| {
                config.compaction.reserve = reserve;
                Ok(())
            })
            .unwrap();
            assert!(dirs.config_file().is_symlink());
            assert!(std::fs::read_to_string(tmp.path().join("dotfiles/agent.toml")).unwrap().contains(&reserve.to_string()));
            assert_eq!(Config::load(&dirs).unwrap().compaction.reserve, reserve);
        }
    }

    fn proxy(url: &str, api_key: &str, management_key: &str) -> Proxy {
        Proxy { url: url.into(), api_key: api_key.into(), management_key: management_key.into() }
    }

    #[test]
    fn the_proxy_is_a_private_file_of_the_data_dir() {
        let tmp = tempfile::tempdir().unwrap();
        let dirs = Dirs { config: tmp.path().join("config"), data: tmp.path().join("data") };
        assert_eq!(Proxy::load(&dirs).unwrap(), Proxy::default());
        let set = |to: Proxy| {
            Proxy::update(&dirs, |proxy| {
                *proxy = to;
                Ok(())
            })
            .unwrap()
        };
        set(proxy("http://p:1", "k", "m"));
        assert_eq!(Proxy::load(&dirs).unwrap(), proxy("http://p:1", "k", "m"));
        assert!(!dirs.config.exists());
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = || std::fs::metadata(dirs.proxy_file()).unwrap().permissions().mode() & 0o777;
            assert_eq!(mode(), 0o600);
            std::fs::set_permissions(dirs.proxy_file(), std::fs::Permissions::from_mode(0o644)).unwrap();
            set(proxy("http://p:1", "k", "m"));
            assert_eq!(mode(), 0o644, "nothing changed, nothing written");
            set(proxy("http://p:1", "k2", "m"));
            assert_eq!(mode(), 0o600);
            set(proxy("http://p:1", "k", "m"));
        }
        let refused = Proxy::update(&dirs, |proxy| {
            proxy.url.clear();
            Err(anyhow!("no"))
        });
        assert!(refused.is_err());
        assert_eq!(Proxy::load(&dirs).unwrap().url, "http://p:1");
        set(Proxy::default());
        assert!(!dirs.proxy_file().exists());
        set(Proxy::default());

        std::fs::write(dirs.proxy_file(), r#"{"url": "http://p:1", "key": "0.0.10's"}"#).unwrap();
        assert_eq!(Proxy::load(&dirs).unwrap(), proxy("http://p:1", "", "0.0.10's"));
        std::fs::write(dirs.proxy_file(), r#"{"url": "http://p:1", "api_key": sk-SECRET}"#).unwrap();
        let said = format!("{:#}", Proxy::load(&dirs).unwrap_err());
        assert_eq!(said, format!("{}: not read (line 1, column 34)", dirs.proxy_file().display()));
        set(proxy("http://q:1", "typed", ""));
        assert_eq!(Proxy::load(&dirs).unwrap(), proxy("http://q:1", "typed", ""));
        std::fs::write(dirs.proxy_file(), "").unwrap();
        set(Proxy::default());
        assert!(!dirs.proxy_file().exists());
    }

    #[test]
    fn a_proxy_in_agent_toml_moves_to_the_data_dir() {
        let tmp = tempfile::tempdir().unwrap();
        let dirs = Dirs { config: tmp.path().join("config"), data: tmp.path().join("data") };
        assert!(!move_proxy(&dirs).unwrap());
        std::fs::create_dir_all(&dirs.config).unwrap();
        std::fs::create_dir_all(&dirs.data).unwrap();
        let old = "[proxy]\nurl = \"http://p:1\"\nkey = \"sk-SECRET\"\n\n[compaction]\nreserve = 5\n\n[mcp.docs]\nurl = \"https://docs.dev/mcp\"\n";
        let before = |management: &str| {
            std::fs::write(dirs.config_file(), old).unwrap();
            std::fs::write(dirs.proxy_file(), management).unwrap();
        };
        let moved = || {
            assert!(move_proxy(&dirs).unwrap());
            let config = std::fs::read_to_string(dirs.config_file()).unwrap();
            assert!(!config.contains("proxy") && !config.contains("SECRET"), "{config}");
            let config = Config::load(&dirs).unwrap();
            assert!(config.compaction.reserve == 5 && config.mcp.contains_key("docs"));
            assert!(!move_proxy(&dirs).unwrap());
            Proxy::load(&dirs).unwrap()
        };

        before(r#"{"url": "http://p:1", "key": "m"}"#);
        assert_eq!(moved(), proxy("http://p:1", "sk-SECRET", "m"));
        before(r#"{"url": "http://other:1", "key": "m"}"#);
        assert_eq!(moved(), proxy("http://p:1", "sk-SECRET", ""));
        before(r#"{"url": "http://own:1", "api_key": "own", "management_key": "m"}"#);
        assert_eq!(moved(), proxy("http://own:1", "own", "m"));

        before("");
        assert_eq!(moved(), proxy("http://p:1", "sk-SECRET", ""));
        before("");
        std::fs::remove_file(dirs.proxy_file()).unwrap();
        assert_eq!(moved(), proxy("http://p:1", "sk-SECRET", ""));

        std::fs::remove_file(dirs.proxy_file()).unwrap();
        std::fs::write(dirs.config_file(), "[proxy]\nurl = \"\"\nkey = \"\"\n").unwrap();
        assert!(move_proxy(&dirs).unwrap() && !dirs.proxy_file().exists());
    }

    #[test]
    fn a_save_of_agent_toml_moves_the_proxy_first() {
        let tmp = tempfile::tempdir().unwrap();
        let dirs = Dirs { config: tmp.path().join("config"), data: tmp.path().join("data") };
        std::fs::create_dir_all(&dirs.config).unwrap();
        let reserve = |reserve: u64| {
            Config::update(&dirs, |config| {
                config.compaction.reserve = reserve;
                Ok(())
            })
        };
        // What a start could not move (the file did not parse then) is still there at a save.
        let old = "[proxy]\nurl = \"http://p:1\"\nkey = \"sk-SECRET\"\n";
        std::fs::write(dirs.config_file(), format!("{old}[compaction")).unwrap();
        assert!(move_proxy(&dirs).is_err() && reserve(5).is_err());
        assert!(std::fs::read_to_string(dirs.config_file()).unwrap().starts_with(old) && !dirs.proxy_file().exists());
        std::fs::write(dirs.config_file(), old).unwrap();
        reserve(5).unwrap();
        assert_eq!(Proxy::load(&dirs).unwrap(), proxy("http://p:1", "sk-SECRET", ""));
        assert_eq!(Config::load(&dirs).unwrap().compaction.reserve, 5);
        assert!(!std::fs::read_to_string(dirs.config_file()).unwrap().contains("proxy"));
    }
}
