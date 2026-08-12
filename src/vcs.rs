//! Probes repositories and creates or removes managed Git and JJ workspaces.

use std::{
    path::{Path, PathBuf},
    process::{Command, Output},
};

use crate::{
    model::{ManagedWorkspace, WorkspaceBackend},
    platform,
};

#[derive(Clone, Debug)]
pub(crate) struct RepositorySnapshot {
    pub(crate) backend: WorkspaceBackend,
    pub(crate) repository_root: PathBuf,
    pub(crate) project_relative_path: PathBuf,
    pub(crate) source_id: String,
    pub(crate) source_label: String,
    pub(crate) source_revision: String,
    pub(crate) jj_parent_revisions: Vec<String>,
    pub(crate) dirty: bool,
}

impl RepositorySnapshot {
    pub(crate) fn sidebar_label(&self) -> String {
        self.source_label.clone()
    }
}

fn background_command(program: &str) -> Command {
    let mut command = Command::new(program);
    platform::hide_command_window(&mut command);
    command
}

fn command_output(mut command: Command, description: &str) -> Result<Output, String> {
    command
        .output()
        .map_err(|error| format!("could not {description}: {error}"))
}

fn successful_text(output: Output, description: &str) -> Result<String, String> {
    if output.status.success() {
        return Ok(String::from_utf8_lossy(&output.stdout).trim().to_string());
    }
    let error = String::from_utf8_lossy(&output.stderr).trim().to_string();
    Err(if error.is_empty() {
        format!("could not {description}")
    } else {
        format!("could not {description}: {error}")
    })
}

fn jj_command(project_path: &Path) -> Command {
    let mut command = background_command("jj");
    command
        .arg("--no-pager")
        .arg("--color=never")
        .arg("-R")
        .arg(project_path);
    command
}

fn probe_jj(project_path: &Path) -> Result<Option<RepositorySnapshot>, String> {
    let mut root_command = jj_command(project_path);
    root_command.arg("workspace").arg("root");
    let root_output = match command_output(root_command, "inspect the JJ repository") {
        Ok(output) if output.status.success() => output,
        Ok(_) => return Ok(None),
        Err(error) => {
            if project_path
                .ancestors()
                .any(|path| path.join(".jj").exists())
            {
                return Err(error);
            }
            return Ok(None);
        }
    };
    let repository_root = PathBuf::from(
        String::from_utf8_lossy(&root_output.stdout)
            .trim()
            .to_string(),
    );
    let project_relative_path = project_path
        .strip_prefix(&repository_root)
        .unwrap_or(Path::new(""))
        .to_path_buf();

    let mut id_command = jj_command(project_path);
    id_command
        .arg("log")
        .arg("--no-graph")
        .arg("-r")
        .arg("@")
        .arg("-T")
        .arg("change_id.short(8) ++ \"\\n\" ++ commit_id ++ \"\\n\"");
    let metadata = successful_text(
        command_output(id_command, "read the current JJ change")?,
        "read the current JJ change",
    )?;
    let mut lines = metadata.lines();
    let source_id = lines
        .next()
        .filter(|id| !id.is_empty())
        .ok_or_else(|| "JJ did not report a current change ID".to_string())?
        .to_string();
    let source_revision = lines
        .next()
        .filter(|id| !id.is_empty())
        .ok_or_else(|| "JJ did not report a current commit ID".to_string())?
        .to_string();

    let mut parents_command = jj_command(project_path);
    parents_command
        .arg("log")
        .arg("--no-graph")
        .arg("-r")
        .arg("parents(@)")
        .arg("-T")
        .arg("commit_id ++ \"\\n\"");
    let parents = successful_text(
        command_output(parents_command, "read the current JJ parents")?,
        "read the current JJ parents",
    )?;

    Ok(Some(RepositorySnapshot {
        backend: WorkspaceBackend::Jj,
        repository_root,
        project_relative_path,
        source_label: source_id.clone(),
        source_id,
        source_revision,
        jj_parent_revisions: parents
            .lines()
            .filter(|parent| !parent.is_empty())
            .map(str::to_string)
            .collect(),
        dirty: false,
    }))
}

fn git_text(project_path: &Path, args: &[&str], description: &str) -> Result<String, String> {
    let mut command = background_command("git");
    command.arg("-C").arg(project_path).args(args);
    successful_text(command_output(command, description)?, description)
}

fn probe_git(project_path: &Path) -> Result<Option<RepositorySnapshot>, String> {
    let repository_root = match git_text(
        project_path,
        &["rev-parse", "--show-toplevel"],
        "inspect the Git repository",
    ) {
        Ok(root) => PathBuf::from(root),
        Err(_) => return Ok(None),
    };
    let source_revision = git_text(
        project_path,
        &["rev-parse", "HEAD"],
        "read the current Git revision",
    )?;
    let branch = git_text(
        project_path,
        &["symbolic-ref", "--quiet", "--short", "HEAD"],
        "read the current Git branch",
    )
    .ok();
    let source_label = branch.unwrap_or_else(|| {
        format!(
            "detached {}",
            source_revision.chars().take(8).collect::<String>()
        )
    });
    let status = git_text(
        project_path,
        &["status", "--porcelain=v1"],
        "read Git status",
    )
    .unwrap_or_default();
    let project_relative_path = project_path
        .strip_prefix(&repository_root)
        .unwrap_or(Path::new(""))
        .to_path_buf();

    Ok(Some(RepositorySnapshot {
        backend: WorkspaceBackend::Git,
        repository_root,
        project_relative_path,
        source_id: source_label.clone(),
        source_label,
        source_revision,
        jj_parent_revisions: Vec::new(),
        dirty: !status.is_empty(),
    }))
}

/// Selects the innermost repository, treating JJ as authoritative when roots coincide.
pub(crate) fn probe_repository(project_path: &Path) -> Result<Option<RepositorySnapshot>, String> {
    let jj = probe_jj(project_path)?;
    let git = match probe_git(project_path) {
        Ok(git) => git,
        Err(error) if jj.is_some() => {
            tracing::debug!(error = %error, path = %project_path.display(), "ignoring Git probe failure inside JJ repository");
            None
        }
        Err(error) => return Err(error),
    };
    Ok(match (jj, git) {
        (Some(jj), Some(git)) if jj.repository_root == git.repository_root => Some(jj),
        (Some(jj), Some(git)) => {
            if git.repository_root.components().count() > jj.repository_root.components().count() {
                Some(git)
            } else {
                Some(jj)
            }
        }
        (Some(jj), None) => Some(jj),
        (None, git) => git,
    })
}

pub(crate) fn validate_workspace(workspace: &ManagedWorkspace) -> Result<(), String> {
    if !workspace.working_directory.is_dir() {
        return Err(format!(
            "The managed workspace is missing: {}",
            workspace.working_directory.display()
        ));
    }
    let reported_root = match workspace.backend {
        WorkspaceBackend::Jj => {
            let mut command = jj_command(&workspace.root);
            command.arg("workspace").arg("root");
            PathBuf::from(successful_text(
                command_output(command, "validate the JJ workspace")?,
                "validate the JJ workspace",
            )?)
        }
        WorkspaceBackend::Git => {
            let root = PathBuf::from(git_text(
                &workspace.root,
                &["rev-parse", "--show-toplevel"],
                "validate the Git worktree",
            )?);
            if let Some(expected_branch) = workspace.git_branch.as_deref() {
                let branch = git_text(
                    &workspace.root,
                    &["symbolic-ref", "--quiet", "--short", "HEAD"],
                    "validate the Git worktree branch",
                )?;
                if branch != expected_branch {
                    return Err(format!(
                        "The managed worktree is on {branch}, expected {expected_branch}"
                    ));
                }
            }
            root
        }
    };
    let expected =
        std::fs::canonicalize(&workspace.root).unwrap_or_else(|_| workspace.root.clone());
    let reported = std::fs::canonicalize(&reported_root).unwrap_or(reported_root);
    if expected != reported {
        return Err(format!(
            "The managed workspace points to {}, expected {}",
            reported.display(),
            expected.display()
        ));
    }
    Ok(())
}

/// Creates a JJ workspace or Git worktree from the snapshot recorded at selection time.
pub(crate) fn create_workspace(workspace: &ManagedWorkspace) -> Result<(), String> {
    if workspace.root.exists() {
        return Err(format!(
            "workspace destination already exists: {}",
            workspace.root.display()
        ));
    }
    if let Some(parent) = workspace.root.parent() {
        std::fs::create_dir_all(parent)
            .map_err(|error| format!("could not create {}: {error}", parent.display()))?;
    }

    let mut command = match workspace.backend {
        WorkspaceBackend::Jj => {
            let mut command = background_command("jj");
            command
                .arg("--no-pager")
                .arg("--color=never")
                .arg("-R")
                .arg(&workspace.source_repository)
                .arg("workspace")
                .arg("add")
                .arg("--name")
                .arg(&workspace.id);
            for parent in &workspace.jj_parent_revisions {
                command.arg("-r").arg(parent);
            }
            command.arg(&workspace.root);
            command
        }
        WorkspaceBackend::Git => {
            let branch = workspace
                .git_branch
                .as_deref()
                .ok_or_else(|| "managed Git workspace has no branch".to_string())?;
            let mut command = background_command("git");
            command
                .arg("-C")
                .arg(&workspace.source_repository)
                .arg("worktree")
                .arg("add")
                .arg("-b")
                .arg(branch)
                .arg(&workspace.root)
                .arg(&workspace.source_revision);
            command
        }
    };
    let output = command
        .output()
        .map_err(|error| format!("could not create workspace: {error}"))?;
    if !output.status.success() {
        let error = String::from_utf8_lossy(&output.stderr).trim().to_string();
        return Err(if error.is_empty() {
            "workspace creation failed".into()
        } else {
            format!("workspace creation failed: {error}")
        });
    }
    validate_workspace(workspace)
}

pub(crate) fn remove_workspace(workspace: &ManagedWorkspace) -> Result<(), String> {
    // The random workspace ID must remain the final path component. Refuse destructive VCS or
    // filesystem operations if persisted metadata points anywhere less constrained.
    if workspace.root.file_name().and_then(|name| name.to_str()) != Some(workspace.id.as_str()) {
        return Err(format!(
            "refusing to remove unexpected workspace path: {}",
            workspace.root.display()
        ));
    }

    match workspace.backend {
        WorkspaceBackend::Git => {
            if !workspace.root.exists() {
                if workspace.source_repository.is_dir() {
                    let mut command = background_command("git");
                    command
                        .arg("-C")
                        .arg(&workspace.source_repository)
                        .arg("worktree")
                        .arg("prune");
                    successful_text(
                        command_output(command, "prune the missing Git worktree")?,
                        "prune the missing Git worktree",
                    )?;
                }
                return Ok(());
            }
            let command_root = if workspace.source_repository.is_dir() {
                &workspace.source_repository
            } else {
                &workspace.root
            };
            let mut command = background_command("git");
            command
                .arg("-C")
                .arg(command_root)
                .arg("worktree")
                .arg("remove")
                .arg(&workspace.root);
            successful_text(
                command_output(command, "remove the Git worktree")?,
                "remove the Git worktree",
            )?;
        }
        WorkspaceBackend::Jj => {
            if !workspace.root.exists() {
                return Ok(());
            }
            // Force JJ to snapshot outstanding changes before forgetting its workspace record.
            // `workspace forget` alone does not remove the checkout directory.
            let mut snapshot = jj_command(&workspace.root);
            snapshot.arg("status");
            successful_text(
                command_output(snapshot, "snapshot the JJ workspace")?,
                "snapshot the JJ workspace",
            )?;

            let mut forget = jj_command(&workspace.root);
            forget.arg("workspace").arg("forget").arg(&workspace.id);
            successful_text(
                command_output(forget, "forget the JJ workspace")?,
                "forget the JJ workspace",
            )?;
            std::fs::remove_dir_all(&workspace.root).map_err(|error| {
                format!("could not remove {}: {error}", workspace.root.display())
            })?;
        }
    }
    Ok(())
}

pub(crate) fn project_slug(name: &str) -> String {
    let mut slug = String::new();
    let mut separated = false;
    for character in name.chars().flat_map(char::to_lowercase) {
        if character.is_ascii_alphanumeric() {
            slug.push(character);
            separated = false;
        } else if !separated && !slug.is_empty() {
            slug.push('-');
            separated = true;
        }
    }
    while slug.ends_with('-') {
        slug.pop();
    }
    if slug.is_empty() {
        "project".into()
    } else {
        slug
    }
}
