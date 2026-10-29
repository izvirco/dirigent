//! Git origin lookup for projects. Only used so the desktop can auto-link the same
//! repo across hosts; no git management happens here.

use std::path::Path;

/// `git remote get-url origin`, normalized. `None` if not a git repo or no origin.
pub fn origin(path: &Path) -> Option<String> {
    let out = crate::platform::command("git").args(["-C", &path.to_string_lossy(), "remote", "get-url", "origin"]).output().ok()?;
    if !out.status.success() {
        return None;
    }
    let url = String::from_utf8(out.stdout).ok()?;
    Some(normalize(url.trim()))
}

/// `git@github.com:a/b.git`, `ssh://git@github.com/a/b`, `https://github.com/a/b/` → `github.com/a/b`.
pub fn normalize(url: &str) -> String {
    let rest = match url.split_once("://") {
        Some((_, rest)) => rest,
        // scp-like: user@host:path
        None => &url.replacen(':', "/", 1),
    };
    let rest = rest.rsplit_once('@').map(|(_, r)| r).unwrap_or(rest);
    let rest = rest.trim_end_matches('/').trim_end_matches(".git");
    match rest.split_once('/') {
        Some((host, path)) => format!("{}/{}", host.to_lowercase(), path),
        None => rest.to_lowercase(),
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn normalizes_common_forms() {
        for u in [
            "git@github.com:izvir/Dirigent.git",
            "ssh://git@GitHub.com/izvir/Dirigent",
            "https://github.com/izvir/Dirigent/",
            "https://user:tok@github.com/izvir/Dirigent.git",
        ] {
            assert_eq!(super::normalize(u), "github.com/izvir/Dirigent", "{u}");
        }
    }
}
