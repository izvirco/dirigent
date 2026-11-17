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
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Config {
    pub proxy: Proxy,
    pub compaction: Compaction,
    #[serde(skip_serializing_if = "BTreeMap::is_empty")]
    pub models: BTreeMap<String, ModelConfig>,
    #[serde(skip_serializing_if = "BTreeMap::is_empty")]
    pub mcp: BTreeMap<String, crate::mcp::Server>,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Proxy {
    /// Its address, without a trailing slash; empty: none set, nothing runs.
    pub url: String,
    pub key: String,
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

    /// Write the file as this. To change what it holds, `update`: a `load` and a `save`
    /// lose what was changed between them.
    pub fn save(&self, dirs: &Dirs) -> Result<()> {
        let _one_at_a_time = writing();
        self.write(dirs)
    }

    /// Change the file: `change` gets what it holds now, and what it leaves is saved.
    /// `change` runs with the file held (`WRITING`): it can't save or update itself.
    pub fn update(dirs: &Dirs, change: impl FnOnce(&mut Config) -> Result<()>) -> Result<()> {
        let _one_at_a_time = writing();
        let mut config = Self::load(dirs)?;
        change(&mut config)?;
        config.write(dirs)
    }

    // MUST KILL: extract: write_atomic(path, text) out of Config::write for the .part file + rename
    fn write(&self, dirs: &Dirs) -> Result<()> {
        let path = dirs.config_file();
        std::fs::create_dir_all(&dirs.config).with_context(|| format!("create {}", dirs.config.display()))?;
        let text = format!("# This host's agent. Also edited from Settings in the app.\n\n{}", toml::to_string(self)?);
        let file = linked(&path);
        let mut partial = file.clone().into_os_string();
        partial.push(".part");
        let written = write_private(Path::new(&partial), &text).and_then(|()| std::fs::rename(&partial, &file));
        written.with_context(|| format!("write {}", path.display()))
    }

    pub fn reserve(&self, model: &str) -> u64 {
        self.models.get(model).and_then(|m| m.reserve).unwrap_or(self.compaction.reserve)
    }
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
    options.open(path)?.write_all(text.as_bytes())
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
            c.proxy = Proxy { url: "http://127.0.0.1:8317".into(), key: "k".into() };
            c.models.insert("claude-opus-5-5".into(), ModelConfig { reserve: Some(350_000) });
            Ok(())
        })
        .unwrap();
        let config = Config::load(&dirs).unwrap();
        assert_eq!(config.proxy.key, "k");
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
        let proxy = Proxy { url: "http://127.0.0.1:8317".into(), key: "k".into() };
        Config { proxy: proxy.clone(), ..Config::default() }.save(&dirs).unwrap();
        let done = std::sync::atomic::AtomicBool::new(false);
        std::thread::scope(|scope| {
            let reading = scope.spawn(|| {
                while !done.load(std::sync::atomic::Ordering::Relaxed) {
                    assert_eq!(Config::load(&dirs).unwrap().proxy, proxy);
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
        for key in ["first", "second"] {
            Config::update(&dirs, |config| {
                config.proxy.key = key.into();
                Ok(())
            })
            .unwrap();
            assert!(dirs.config_file().is_symlink());
            assert!(std::fs::read_to_string(tmp.path().join("dotfiles/agent.toml")).unwrap().contains(key));
            assert_eq!(Config::load(&dirs).unwrap().proxy.key, key);
        }
    }
}
