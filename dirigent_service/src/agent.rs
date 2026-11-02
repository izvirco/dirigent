//! Pi's agent directory: where the bundled pi keeps its auth, settings, models, installed
//! packages, extensions, skills and prompts. Dirigent gives it one of its own,
//! `<config dir>/agent` (`PI_CODING_AGENT_DIR` for the runtime and `dirigent pi`), apart
//! from `~/.pi/agent`, which a pi of another version may be using.
//!
//! The first time it's missing it is seeded by copying an existing agent dir: the platform's
//! default dirigent one (a `--config-dir` instance starts from the real install's) or else
//! pi's `~/.pi/agent`, so nobody has to sign in or install their packages again. Pi's
//! transcripts (`sessions/`, which can be big; threads live in `service.db`) and its caches
//! are left out; symlinks are copied as symlinks (home-manager keeps pointing them).

use std::path::{Path, PathBuf};
use tracing::{info, warn};

/// What a copy skips: pi's sessions, crash log and provider-catalog cache.
const SKIP: &[&str] = &["sessions", "crashes.json", "models-store.json"];

/// The agent dir, seeded from an existing one if it doesn't exist yet.
pub fn dir() -> PathBuf {
    let dir = crate::config_dir().join("agent");
    if dir.exists() {
        return dir;
    }
    let home = std::env::home_dir();
    let candidates = [Some(crate::platform::config_dir().join("agent")), home.map(|h| h.join(".pi").join("agent"))];
    let Some(from) = candidates.into_iter().flatten().find(|c| c != &dir && c.is_dir()) else {
        return dir;
    };
    match seed(&from, &dir) {
        Ok(()) => info!(from = %from.display(), to = %dir.display(), "agent dir seeded"),
        Err(e) => warn!(from = %from.display(), to = %dir.display(), "agent dir not seeded: {e}"),
    }
    dir
}

/// Copy `from` into `to` through a `<to>.new` that is renamed once complete: a copy that
/// fails halfway leaves nothing, and the next start tries again.
fn seed(from: &Path, to: &Path) -> std::io::Result<()> {
    let staging = to.with_extension("new");
    let _ = std::fs::remove_dir_all(&staging);
    if let Some(parent) = to.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::create_dir(&staging)?;
    let copied = copy_tree(from, &staging, true);
    if copied.is_err() {
        let _ = std::fs::remove_dir_all(&staging);
    }
    copied?;
    std::fs::rename(&staging, to)
}

/// Recursive copy, keeping symlinks as symlinks on unix (on Windows they're followed).
/// `top` applies `SKIP` to the top level only.
fn copy_tree(from: &Path, to: &Path, top: bool) -> std::io::Result<()> {
    for entry in std::fs::read_dir(from)? {
        let entry = entry?;
        let name = entry.file_name();
        if top && SKIP.iter().any(|s| name == *s) {
            continue;
        }
        let (src, dst) = (entry.path(), to.join(&name));
        // `DirEntry::metadata` doesn't follow symlinks.
        let meta = entry.metadata()?;
        if meta.is_symlink() && cfg!(unix) {
            #[cfg(unix)]
            std::os::unix::fs::symlink(std::fs::read_link(&src)?, &dst)?;
        } else if src.is_dir() {
            std::fs::create_dir(&dst)?;
            copy_tree(&src, &dst, false)?;
        } else {
            std::fs::copy(&src, &dst)?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn seeds_without_sessions_keeping_links() {
        let tmp = tempfile::tempdir().unwrap();
        let from = tmp.path().join("from");
        std::fs::create_dir_all(from.join("sessions/x")).unwrap();
        std::fs::create_dir_all(from.join("git/pkg")).unwrap();
        std::fs::write(from.join("auth.json"), "{}").unwrap();
        std::fs::write(from.join("crashes.json"), "[]").unwrap();
        std::fs::write(from.join("git/pkg/index.ts"), "").unwrap();
        #[cfg(unix)]
        std::os::unix::fs::symlink("/nowhere/AGENTS.md", from.join("AGENTS.md")).unwrap();

        let to = tmp.path().join("config/agent");
        seed(&from, &to).unwrap();
        assert!(to.join("auth.json").is_file());
        assert!(to.join("git/pkg/index.ts").is_file());
        assert!(!to.join("sessions").exists());
        assert!(!to.join("crashes.json").exists());
        assert!(!to.with_extension("new").exists());
        #[cfg(unix)]
        assert_eq!(std::fs::read_link(to.join("AGENTS.md")).unwrap(), Path::new("/nowhere/AGENTS.md"));
    }
}
