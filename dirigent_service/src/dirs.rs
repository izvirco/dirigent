//! Paths the desktop's add-project browser sends: `~/…` expands to this host's home, and
//! a directory's subdirectories are listed for it to filter as the user types.

use anyhow::{Context, Result, ensure};
use dirigent_protocol::Dir;
use std::path::{Path, PathBuf};

/// `~` or `~/…` under `$HOME`; anything else must be absolute. Trailing slashes and `.`
/// segments are dropped (`/a/b/` → `/a/b`).
pub fn expand(path: &str) -> Result<PathBuf> {
    let path = path.trim();
    let path = match path.strip_prefix('~').filter(|rest| rest.is_empty() || rest.starts_with('/')) {
        Some(rest) => PathBuf::from(std::env::var_os("HOME").context("HOME not set")?).join(rest.trim_start_matches('/')),
        None => PathBuf::from(path),
    };
    ensure!(path.is_absolute(), "path must be absolute or start with ~/");
    Ok(path.components().collect())
}

/// The subdirectories of `dir` (symlinks to directories included), sorted by name.
pub fn list(dir: &Path) -> Result<Vec<Dir>> {
    let mut dirs: Vec<Dir> = std::fs::read_dir(dir)
        .with_context(|| format!("can't read {}", dir.display()))?
        .filter_map(|e| {
            let path = e.ok()?.path();
            let name = path.file_name()?.to_string_lossy().into_owned();
            path.is_dir().then(|| Dir { name, repo: path.join(".git").exists() || path.join(".jj").exists() })
        })
        .collect();
    dirs.sort_by(|a, b| a.name.cmp(&b.name));
    Ok(dirs)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn expands_home_and_normalizes() {
        let home = PathBuf::from(std::env::var_os("HOME").unwrap());
        assert_eq!(expand("~").unwrap(), home);
        assert_eq!(expand("~/work/").unwrap(), home.join("work"));
        assert_eq!(expand("/a/./b/").unwrap(), PathBuf::from("/a/b"));
        assert!(expand("~bob/x").is_err());
        assert!(expand("rel/path").is_err());
    }

    #[test]
    fn lists_subdirectories_marking_repos() {
        let root = std::env::temp_dir().join(format!("dirigent-dirs-{}", std::process::id()));
        std::fs::create_dir_all(root.join("b/.git")).unwrap();
        std::fs::create_dir_all(root.join("a")).unwrap();
        std::fs::write(root.join("file"), "").unwrap();
        let dirs = list(&root).unwrap();
        std::fs::remove_dir_all(&root).unwrap();
        assert_eq!(dirs, [Dir { name: "a".into(), repo: false }, Dir { name: "b".into(), repo: true }]);
    }
}
