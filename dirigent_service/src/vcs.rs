//! Version control on the host: which system a directory is under (jj wins over git, so a
//! colocated repo is driven by jj), whether its working copy is dirty, and per-thread
//! isolated workspaces (a git worktree or a jj workspace) under
//! `$XDG_DATA_HOME/dirigent/workspaces/<name>`, where the name is also the git branch /
//! jj workspace name (`workspace_name`). Everything shells out to `git` / `jj`;
//! nothing else about the repo is managed. Branches are never deleted.

use anyhow::{Context, Result, anyhow};
use dirigent_protocol::Vcs;
use std::path::Path;
use std::process::Command;

/// Run a vcs command in `dir`, returning stdout on success and stderr as the error. A jj
/// workspace goes *stale* when its commit is rewritten from another workspace (a rebase in
/// the user's terminal) and every jj command in it refuses until `workspace update-stale`
/// brings its files to the new commit; that is done here, once, and the command retried,
/// so a thread's workspace follows what the user did to its history.
fn run(program: &str, dir: &Path, args: &[&str]) -> Result<String> {
    match run_once(program, dir, args) {
        Err(e) if program == "jj" && e.to_string().contains("working copy is stale") => {
            run_once("jj", dir, &["workspace", "update-stale"])?;
            run_once(program, dir, args)
        }
        r => r,
    }
}

fn run_once(program: &str, dir: &Path, args: &[&str]) -> Result<String> {
    let out = Command::new(program).current_dir(dir).args(args).output().with_context(|| format!("run {program}"))?;
    if !out.status.success() {
        let err = String::from_utf8_lossy(&out.stderr);
        return Err(anyhow!("{program} {}: {}", args.join(" "), err.trim()));
    }
    Ok(String::from_utf8_lossy(&out.stdout).into_owned())
}

/// Bring a jj workspace up to date with its rewritten commit before an agent works in it
/// (its own `jj` calls would fail otherwise). A no-op when it isn't stale, or not jj at all.
pub fn update_stale(path: &Path) {
    let _ = run("jj", path, &["log", "-r", "@", "--no-graph", "-T", "''"]);
}

/// Which system owns `path`: jj if it is inside a jj repo, else git, else none.
pub fn detect(path: &Path) -> Option<Vcs> {
    if !path.is_dir() {
        return None;
    }
    if run("jj", path, &["root", "--ignore-working-copy"]).is_ok() {
        return Some(Vcs::Jj);
    }
    if run("git", path, &["rev-parse", "--show-toplevel"]).is_ok() {
        return Some(Vcs::Git);
    }
    None
}

/// Whether `path` is a working copy its repo still knows. False when the directory is gone,
/// or when it outlived its registration: a forgotten jj workspace (`jj workspace forget`
/// keeps the files), a pruned git worktree (`.git` points nowhere). One `jj` / `git` call,
/// ~15 ms; `--ignore-working-copy` so a stale workspace still counts and nothing is snapshotted.
pub fn is_workspace(path: &Path) -> bool {
    if path.join(".jj").is_dir() {
        run_once("jj", path, &["log", "--ignore-working-copy", "-r", "@", "--no-graph", "-T", "''"]).is_ok()
    } else {
        path.join(".git").exists() && run_once("git", path, &["rev-parse", "--git-dir"]).is_ok()
    }
}

/// Uncommitted changes in the working copy at `path`. For jj the working copy is always a
/// commit, so "dirty" means `@` is not empty.
pub fn dirty(vcs: Vcs, path: &Path) -> Result<bool> {
    Ok(match vcs {
        Vcs::Git => !run("git", path, &["status", "--porcelain"])?.trim().is_empty(),
        Vcs::Jj => run("jj", path, &["log", "-r", "@", "--no-graph", "-T", "empty"])?.trim() != "true",
    })
}

/// What the working copy sits on: the git branch, or the jj change id of `@`.
pub fn branch(vcs: Vcs, path: &Path) -> Option<String> {
    let s = match vcs {
        Vcs::Git => run("git", path, &["branch", "--show-current"]).ok()?,
        Vcs::Jj => run("jj", path, &["log", "-r", "@", "--no-graph", "-T", "change_id.short()"]).ok()?,
    };
    let s = s.trim();
    (!s.is_empty()).then(|| s.to_owned())
}

/// Names a workspace can start from: git local branches and tags, jj bookmarks. Free text
/// is accepted as a base too; this is what the picker lists.
pub fn refs(vcs: Vcs, path: &Path) -> Vec<String> {
    let out = match vcs {
        Vcs::Git => run("git", path, &["for-each-ref", "--format=%(refname:short)", "refs/heads", "refs/tags"]),
        Vcs::Jj => run("jj", path, &["bookmark", "list", "--ignore-working-copy", "-T", "name ++ \"\\n\""]),
    };
    out.map(|s| s.lines().map(str::trim).filter(|l| !l.is_empty()).map(str::to_owned).collect()).unwrap_or_default()
}

/// What a workspace starts from when nothing is asked for.
pub fn default_base(vcs: Vcs) -> &'static str {
    match vcs {
        Vcs::Git => "HEAD",
        Vcs::Jj => "@-",
    }
}

/// The one name a thread's workspace goes by: its directory, its git branch, its jj
/// workspace. `dirigent-` + the last 8 characters of the thread id (the random part of a
/// v7 uuid), short enough to read in a branch list.
pub fn workspace_name(thread_id: &str) -> String {
    let short: String = thread_id.chars().rev().take(8).collect::<Vec<_>>().into_iter().rev().collect();
    format!("dirigent-{short}")
}

/// Create the thread's workspace at `dest`, checked out at `base` (see `default_base`;
/// anything the vcs resolves, a bad one fails with its message and nothing is created).
/// Recreates it when the directory went missing but the branch / workspace name is still
/// known to the repo, resuming that branch instead of `base`.
pub fn add_workspace(vcs: Vcs, repo: &Path, dest: &Path, thread_id: &str, base: Option<&str>) -> Result<()> {
    anyhow::ensure!(!dest.exists(), "{} already exists (a workspace kept on disk?); remove it first", dest.display());
    if let Some(parent) = dest.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let name = workspace_name(thread_id);
    let dest_s = dest.to_string_lossy();
    let base = base.map(str::trim).filter(|b| !b.is_empty()).unwrap_or(default_base(vcs));
    // A worktree whose directory was deleted is still registered until pruned (a colocated
    // jj workspace is a git worktree too).
    let _ = run("git", repo, &["worktree", "prune"]);
    match vcs {
        Vcs::Git => {
            let exists = run("git", repo, &["rev-parse", "--verify", "--quiet", &format!("refs/heads/{name}")]).is_ok();
            if exists {
                run("git", repo, &["worktree", "add", &dest_s, &name])?;
            } else {
                run("git", repo, &["worktree", "add", "-b", &name, &dest_s, base])?;
            }
        }
        Vcs::Jj => {
            let _ = run("jj", repo, &["workspace", "forget", &name]);
            // jj makes the directory before it resolves `-r`: a bad base must not leave it behind.
            if let Err(e) = run("jj", repo, &["workspace", "add", "--name", &name, "-r", base, &dest_s]) {
                let _ = std::fs::remove_dir_all(dest);
                return Err(e);
            }
        }
    }
    Ok(())
}

/// Remove the thread's workspace: unregister it from the repo and delete the directory
/// (uncommitted changes in it are lost; the caller confirms that with the user). The git
/// branch stays. Tolerates a workspace the repo already forgot and a directory already gone:
/// deleting the directory and pruning is all a worktree removal is.
pub fn remove_workspace(vcs: Vcs, repo: &Path, dest: &Path, thread_id: &str) -> Result<()> {
    let name = workspace_name(thread_id);
    match vcs {
        Vcs::Git => {
            let _ = run("git", repo, &["worktree", "remove", "--force", &dest.to_string_lossy()]);
        }
        Vcs::Jj => {
            let _ = run("jj", repo, &["workspace", "forget", &name]);
        }
    }
    if dest.exists() {
        std::fs::remove_dir_all(dest).with_context(|| format!("remove {}", dest.display()))?;
    }
    let _ = run("git", repo, &["worktree", "prune"]);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn git_repo() -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path();
        run("git", p, &["init", "-q", "-b", "main"]).unwrap();
        run("git", p, &["-c", "user.name=t", "-c", "user.email=t@t", "commit", "-q", "--allow-empty", "-m", "init"]).unwrap();
        dir
    }

    #[test]
    fn git_workspace_lifecycle() {
        if Command::new("git").arg("--version").output().is_err() {
            return;
        }
        let repo = git_repo();
        assert_eq!(detect(repo.path()), Some(Vcs::Git));
        assert!(!dirty(Vcs::Git, repo.path()).unwrap());
        std::fs::write(repo.path().join("x"), "1").unwrap();
        assert!(dirty(Vcs::Git, repo.path()).unwrap());

        let ws = tempfile::tempdir().unwrap();
        let dest = ws.path().join("t");
        // A bad base creates nothing.
        assert!(add_workspace(Vcs::Git, repo.path(), &dest, "0198-abcdef12", Some("nope")).is_err());
        assert!(!dest.exists());
        add_workspace(Vcs::Git, repo.path(), &dest, "0198-abcdef12", None).unwrap();
        assert_eq!(branch(Vcs::Git, &dest).as_deref(), Some("dirigent-abcdef12"));
        assert!(!dirty(Vcs::Git, &dest).unwrap());
        assert!(is_workspace(&dest));
        assert_eq!(refs(Vcs::Git, repo.path()), ["dirigent-abcdef12", "main"]);
        // Deleted behind our back: recreating finds the branch again.
        std::fs::remove_dir_all(&dest).unwrap();
        add_workspace(Vcs::Git, repo.path(), &dest, "0198-abcdef12", None).unwrap();
        assert!(dest.join(".git").exists());
        remove_workspace(Vcs::Git, repo.path(), &dest, "0198-abcdef12").unwrap();
        assert!(!dest.exists());
        // The branch outlives the workspace.
        assert!(run("git", repo.path(), &["rev-parse", "--verify", "refs/heads/dirigent-abcdef12"]).is_ok());
        // Removing again is fine.
        remove_workspace(Vcs::Git, repo.path(), &dest, "0198-abcdef12").unwrap();
        assert!(!is_workspace(&dest));
        // A directory git no longer knows as a worktree is not one, and removes cleanly.
        add_workspace(Vcs::Git, repo.path(), &dest, "0198-abcdef12", None).unwrap();
        std::fs::remove_file(dest.join(".git")).unwrap();
        assert!(!is_workspace(&dest));
        remove_workspace(Vcs::Git, repo.path(), &dest, "0198-abcdef12").unwrap();
        assert!(!dest.exists());
    }

    /// jj drives a colocated repo; its workspace is a change on top of `@`'s parents.
    #[test]
    fn jj_workspace_lifecycle() {
        if Command::new("jj").arg("--version").output().is_err() {
            return;
        }
        let repo = git_repo();
        run("jj", repo.path(), &["git", "init", "--colocate"]).unwrap();
        assert_eq!(detect(repo.path()), Some(Vcs::Jj));
        assert!(!dirty(Vcs::Jj, repo.path()).unwrap());
        std::fs::write(repo.path().join("x"), "1").unwrap();
        assert!(dirty(Vcs::Jj, repo.path()).unwrap());

        let ws = tempfile::tempdir().unwrap();
        let dest = ws.path().join("t");
        assert!(add_workspace(Vcs::Jj, repo.path(), &dest, "0198-abcdef12", Some("nope")).is_err());
        // A base of our own: the imported `main` bookmark.
        add_workspace(Vcs::Jj, repo.path(), &dest, "0198-abcdef12", Some("main")).unwrap();
        assert!(dest.join(".jj").exists());
        assert!(!dirty(Vcs::Jj, &dest).unwrap());
        assert!(branch(Vcs::Jj, &dest).is_some());
        assert_eq!(refs(Vcs::Jj, repo.path()), ["main"]);
        // Rebasing the workspace's commit from the main checkout makes it stale; our queries
        // update it and go on, and its files follow.
        std::fs::write(repo.path().join("README.md"), "moved").unwrap();
        run_once("jj", repo.path(), &["describe", "-m", "base"]).unwrap();
        run_once("jj", repo.path(), &["new"]).unwrap();
        run_once("jj", repo.path(), &["rebase", "-r", "dirigent-abcdef12@", "-d", "@-"]).unwrap();
        assert!(run_once("jj", &dest, &["log", "-r", "@", "-T", "''"]).unwrap_err().to_string().contains("stale"));
        assert!(!dirty(Vcs::Jj, &dest).unwrap());
        assert_eq!(std::fs::read_to_string(dest.join("README.md")).unwrap(), "moved");
        std::fs::remove_dir_all(&dest).unwrap();
        add_workspace(Vcs::Jj, repo.path(), &dest, "0198-abcdef12", None).unwrap();
        assert!(is_workspace(&dest));
        // Forgotten from the main checkout: the files stay, the workspace is gone.
        run_once("jj", repo.path(), &["workspace", "forget", "dirigent-abcdef12"]).unwrap();
        assert!(dest.is_dir());
        assert!(!is_workspace(&dest));
        remove_workspace(Vcs::Jj, repo.path(), &dest, "0198-abcdef12").unwrap();
        assert!(!dest.exists());
        assert!(!run("jj", repo.path(), &["workspace", "list"]).unwrap().contains("dirigent-abcdef12"));
    }

    #[test]
    fn not_a_repo() {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(detect(dir.path()), None);
        assert_eq!(detect(Path::new("/nonexistent/dir")), None);
        assert!(!is_workspace(dir.path()));
        assert!(!is_workspace(Path::new("/nonexistent/dir")));
    }
}
