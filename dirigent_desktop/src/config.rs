//! `~/.config/dirigent/config.toml`: fonts, theme, telemetry, and how threads are named.
//! Edited by hand or from Settings › Config; the file is the truth. `Watch` notices hand
//! edits (to it or to the theme dir) so they apply without a restart.
//!
//! ```toml
//! theme = "dirigent"
//! telemetry = false
//!
//! [fonts]
//! ui = "Lilex"
//! prose = "Lilex"
//! code = "Lilex"
//!
//! [title]
//! model = "anthropic/claude-haiku-5-5"
//! reasoning = "low"
//! prompt = "..."
//! ```

use crate::theme::Fonts;
use anyhow::{Context, Result};
use dirigent_protocol::TitleGen;
use serde::{Deserialize, Serialize};
use std::path::PathBuf;
use std::time::SystemTime;

const HEADER: &str = "# Dirigent settings. Also edited by Settings > Config; changes apply as you save.\n\n";

pub const DEFAULT_TITLE_PROMPT: &str = "You name coding threads. Given the user's first request, reply with a title of 3 to 7 words \
that says what the work is about. Reply with the title only: no quotes, no trailing period, no explanation.";

#[derive(Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Config {
    /// A theme name: built-in or `~/.config/dirigent/theme/<name>.toml`.
    pub theme: String,
    /// Not wired to anything yet.
    pub telemetry: bool,
    pub fonts: FontConfig,
    pub title: TitleGen,
}

/// Font families as the user wrote them; `resolve` maps them to installed fonts.
#[derive(Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct FontConfig {
    pub ui: String,
    pub prose: String,
    pub code: String,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            theme: crate::theme::BUILTIN[0].0.to_owned(),
            telemetry: false,
            fonts: FontConfig::default(),
            title: TitleGen { model: "anthropic/claude-haiku-5-5".into(), reasoning: "low".into(), prompt: DEFAULT_TITLE_PROMPT.into() },
        }
    }
}

impl Default for FontConfig {
    fn default() -> Self {
        Self { ui: "Lilex".into(), prose: "Lilex".into(), code: "Lilex".into() }
    }
}

/// `~/.config/dirigent`.
pub fn dir() -> PathBuf {
    std::env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".config")))
        .expect("HOME not set")
        .join("dirigent")
}

pub fn path() -> PathBuf {
    dir().join("config.toml")
}

/// Read the file. Missing keys take their defaults; a missing file is written out with
/// every default so there is something to edit.
pub fn load() -> Result<Config> {
    let path = path();
    match std::fs::read_to_string(&path) {
        Ok(text) => toml::from_str(&text).with_context(|| format!("parse {}", path.display())),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            let config = Config::default();
            save(&config)?;
            Ok(config)
        }
        Err(e) => Err(e).with_context(|| format!("read {}", path.display())),
    }
}

pub fn save(config: &Config) -> Result<()> {
    let path = path();
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let body = toml::to_string(config)?;
    std::fs::write(&path, format!("{HEADER}{body}")).with_context(|| format!("write {}", path.display()))
}

impl FontConfig {
    /// Installed family names for the three roles. A family that isn't installed falls
    /// back to the system's generic font (sans for ui / prose, monospace for code) and is
    /// reported in the returned list so the UI can say so.
    pub fn resolve(&self) -> (Fonts, Vec<String>) {
        let mut missing: Vec<String> = vec![];
        let mut pick = |family: &str, generic: &str, fallback: &str| -> gpui::SharedString {
            match resolve_family(family) {
                Some(f) => f.into(),
                None => {
                    if !missing.iter().any(|m| m == family) {
                        missing.push(family.to_owned());
                    }
                    resolve_family(generic).unwrap_or_else(|| fallback.to_owned()).into()
                }
            }
        };
        let ui = pick(&self.ui, "sans-serif", "DejaVu Sans");
        let prose = pick(&self.prose, "sans-serif", "DejaVu Sans");
        let mono = pick(&self.code, "monospace", "DejaVu Sans Mono");
        (Fonts { ui, prose, mono }, missing)
    }
}

/// What fontconfig resolves `pattern` to, if that is a real match: the family itself, or
/// anything at all for a generic like `monospace` (fc-match always answers something, so a
/// concrete family only counts when it comes back as itself).
fn resolve_family(pattern: &str) -> Option<String> {
    let pattern = pattern.trim();
    if pattern.is_empty() {
        return None;
    }
    let out = std::process::Command::new("fc-match").args(["-f", "%{family[0]}", pattern]).output().ok()?;
    let family = String::from_utf8(out.stdout).ok()?.trim().to_owned();
    let generic = matches!(pattern, "sans-serif" | "serif" | "monospace");
    (!family.is_empty() && (generic || family.eq_ignore_ascii_case(pattern))).then_some(family)
}

/// Notices hand edits: polls the modification times of `config.toml` and the theme dir's
/// files once a second (from `Dirigent::tick`). Polling, not inotify, so editors that
/// write-and-rename or touch the dir are all handled the same way.
pub struct Watch {
    stamp: Vec<(PathBuf, Option<SystemTime>)>,
}

impl Watch {
    pub fn new() -> Self {
        Self { stamp: Self::stamp() }
    }

    /// True once per change to the watched files since the last call.
    pub fn changed(&mut self) -> bool {
        let now = Self::stamp();
        let changed = now != self.stamp;
        self.stamp = now;
        changed
    }

    fn stamp() -> Vec<(PathBuf, Option<SystemTime>)> {
        let mtime = |p: &PathBuf| std::fs::metadata(p).and_then(|m| m.modified()).ok();
        let mut files = vec![path()];
        if let Ok(entries) = std::fs::read_dir(crate::theme::dir()) {
            files.extend(entries.flatten().map(|e| e.path()).filter(|p| p.extension().is_some_and(|x| x == "toml")));
        }
        files.sort();
        files
            .into_iter()
            .map(|p| {
                let m = mtime(&p);
                (p, m)
            })
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn missing_keys_take_defaults() {
        let c: Config = toml::from_str("theme = \"kanagawa\"\n[fonts]\ncode = \"Fira Code\"\n").unwrap();
        assert_eq!(c.theme, "kanagawa");
        assert_eq!(c.fonts.code, "Fira Code");
        assert_eq!(c.fonts.ui, "Lilex");
        assert_eq!(c.title.reasoning, "low");
        let round: Config = toml::from_str(&toml::to_string(&Config::default()).unwrap()).unwrap();
        assert!(round == Config::default());
    }
}
