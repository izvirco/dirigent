//! Captures Git and JJ checkpoints and turns them into per-turn file diffs.

use super::*;

#[derive(Clone, Debug)]
pub(crate) struct ActiveTurnDiff {
    pub(crate) id: u64,
    pub(crate) prompt: String,
    pub(crate) started_at: u64,
    pub(crate) status_override: Option<TurnDiffStatus>,
    baseline: VcsBaseline,
}

#[derive(Clone, Debug)]
enum VcsBaseline {
    Git(GitBaseline),
    Jj(JjBaseline),
}

#[derive(Clone, Debug)]
struct GitBaseline {
    repository_root: PathBuf,
    project_relative_path: PathBuf,
    revision: Option<String>,
    dirty_paths: HashSet<String>,
    dirty_files: WorkspaceSnapshot,
}

#[derive(Clone, Debug)]
struct JjBaseline {
    repository_root: PathBuf,
    project_relative_path: PathBuf,
    revision: String,
}

#[derive(Clone, Debug)]
struct SnapshotFile {
    hash: blake3::Hash,
    bytes: Option<Vec<u8>>,
    len: u64,
    mode: u32,
    binary: bool,
}

#[derive(Clone, Debug, Default)]
struct WorkspaceSnapshot {
    files: BTreeMap<String, SnapshotFile>,
}

/// Capture a VCS-native checkpoint for a turn. Projects outside Git or JJ deliberately
/// return `None`; turn diffs are not available without a repository.
pub(crate) fn begin_turn(
    id: u64,
    prompt: &str,
    root: &Path,
) -> Result<Option<ActiveTurnDiff>, String> {
    let Some((backend, repository_root, project_relative_path)) = diff_repository(root)? else {
        return Ok(None);
    };
    let baseline = match backend {
        DiffBackend::Git => VcsBaseline::Git(capture_git_baseline(
            repository_root,
            project_relative_path,
        )?),
        DiffBackend::Jj => VcsBaseline::Jj(JjBaseline {
            revision: jj_snapshot_revision(&repository_root)?,
            repository_root,
            project_relative_path,
        }),
    };
    Ok(Some(ActiveTurnDiff {
        id,
        prompt: prompt_excerpt(prompt),
        started_at: unix_timestamp(),
        status_override: None,
        baseline,
    }))
}

pub(crate) fn preview_turn(active: &ActiveTurnDiff, _root: &Path) -> TurnDiff {
    finish_turn_impl(
        active.id,
        active.prompt.clone(),
        active.started_at,
        active.status_override.unwrap_or(TurnDiffStatus::Completed),
        &active.baseline,
    )
}

pub(crate) fn finish_turn(
    active: ActiveTurnDiff,
    _root: &Path,
    status: TurnDiffStatus,
) -> TurnDiff {
    finish_turn_impl(
        active.id,
        active.prompt,
        active.started_at,
        status,
        &active.baseline,
    )
}

fn finish_turn_impl(
    id: u64,
    prompt: String,
    started_at: u64,
    status: TurnDiffStatus,
    baseline: &VcsBaseline,
) -> TurnDiff {
    let result = match baseline {
        VcsBaseline::Git(baseline) => {
            finish_git_turn(id, prompt.clone(), started_at, status, baseline)
        }
        VcsBaseline::Jj(baseline) => {
            finish_jj_turn(id, prompt.clone(), started_at, status, baseline)
        }
    };
    match result {
        Ok(turn) => turn,
        Err(error) => TurnDiff {
            id,
            prompt,
            started_at,
            finished_at: unix_timestamp(),
            status: TurnDiffStatus::Unavailable,
            files: Vec::new(),
            additions: 0,
            deletions: 0,
            error: Some(error),
        },
    }
}

pub(crate) fn prompt_excerpt(prompt: &str) -> String {
    let normalized = prompt.split_whitespace().collect::<Vec<_>>().join(" ");
    let mut end = normalized.len().min(96);
    while !normalized.is_char_boundary(end) {
        end -= 1;
    }
    if end < normalized.len() {
        format!("{}…", &normalized[..end])
    } else if normalized.is_empty() {
        "Agent turn".into()
    } else {
        normalized
    }
}

fn unix_timestamp() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

fn command_output(mut command: Command, description: &str) -> Result<Output, String> {
    platform::hide_command_window(&mut command);
    let mut child = command
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|error| format!("could not {description}: {error}"))?;
    let stdout = child.stdout.take().expect("piped VCS stdout");
    let stderr = child.stderr.take().expect("piped VCS stderr");
    // Drain both pipes concurrently: waiting for process exit first can deadlock when either pipe
    // fills. The surrounding worker remains responsible for the overall command timeout.
    let stdout = std::thread::spawn(move || {
        let mut bytes = Vec::new();
        let mut stdout = stdout;
        let _ = stdout.read_to_end(&mut bytes);
        bytes
    });
    let stderr = std::thread::spawn(move || {
        let mut bytes = Vec::new();
        let mut stderr = stderr;
        let _ = stderr.read_to_end(&mut bytes);
        bytes
    });
    let deadline = std::time::Instant::now() + VCS_COMMAND_TIMEOUT;
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) if std::time::Instant::now() < deadline => {
                std::thread::sleep(Duration::from_millis(5));
            }
            Ok(None) => {
                let _ = child.kill();
                let _ = child.wait();
                let _ = stdout.join();
                let _ = stderr.join();
                return Err(format!(
                    "could not {description}: command timed out after {} ms",
                    VCS_COMMAND_TIMEOUT.as_millis()
                ));
            }
            Err(error) => {
                let _ = child.kill();
                let _ = child.wait();
                let _ = stdout.join();
                let _ = stderr.join();
                return Err(format!("could not {description}: {error}"));
            }
        }
    };
    Ok(Output {
        status,
        stdout: stdout.join().unwrap_or_default(),
        stderr: stderr.join().unwrap_or_default(),
    })
}

fn successful_output(output: Output, description: &str) -> Result<Vec<u8>, String> {
    if output.status.success() {
        return Ok(output.stdout);
    }
    let error = String::from_utf8_lossy(&output.stderr).trim().to_string();
    Err(if error.is_empty() {
        format!("could not {description}")
    } else {
        format!("could not {description}: {error}")
    })
}

#[derive(Clone, Copy)]
enum DiffBackend {
    Git,
    Jj,
}

/// Chooses the innermost repository, preferring JJ when Git describes the same root.
fn diff_repository(root: &Path) -> Result<Option<(DiffBackend, PathBuf, PathBuf)>, String> {
    let root = fs::canonicalize(root).unwrap_or_else(|_| root.to_path_buf());
    let jj_root = if root.ancestors().any(|path| path.join(".jj").exists()) {
        let mut command = jj_command(&root, true);
        command.args(["workspace", "root"]);
        let output = successful_output(
            command_output(command, "inspect the JJ repository")?,
            "inspect the JJ repository",
        )?;
        Some(PathBuf::from(String::from_utf8_lossy(&output).trim()))
    } else {
        None
    };

    let mut command = Command::new("git");
    command
        .arg("-C")
        .arg(&root)
        .args(["rev-parse", "--show-toplevel"]);
    let output = command_output(command, "inspect the Git repository")?;
    let git_root = output
        .status
        .success()
        .then(|| PathBuf::from(String::from_utf8_lossy(&output.stdout).trim().to_string()));

    let selected = match (jj_root, git_root) {
        (Some(jj), Some(git)) if jj == git => Some((DiffBackend::Jj, jj)),
        (Some(jj), Some(git)) => {
            if git.components().count() > jj.components().count() {
                Some((DiffBackend::Git, git))
            } else {
                Some((DiffBackend::Jj, jj))
            }
        }
        (Some(jj), None) => Some((DiffBackend::Jj, jj)),
        (None, Some(git)) => Some((DiffBackend::Git, git)),
        (None, None) => None,
    };
    Ok(selected.map(|(backend, repository_root)| {
        let project_relative_path = root
            .strip_prefix(&repository_root)
            .unwrap_or(Path::new(""))
            .to_path_buf();
        (backend, repository_root, project_relative_path)
    }))
}

fn git_output(root: &Path, args: &[&str], description: &str) -> Result<Vec<u8>, String> {
    let mut command = Command::new("git");
    command.arg("-C").arg(root).args(args);
    successful_output(command_output(command, description)?, description)
}

fn git_head(root: &Path) -> Result<Option<String>, String> {
    let mut command = Command::new("git");
    command
        .arg("-C")
        .arg(root)
        .args(["rev-parse", "--verify", "HEAD"]);
    let output = command_output(command, "read Git HEAD")?;
    if !output.status.success() {
        return Ok(None);
    }
    Ok(Some(
        String::from_utf8_lossy(&output.stdout).trim().to_string(),
    ))
}

fn git_status_paths(root: &Path, scope: &Path) -> Result<HashSet<String>, String> {
    let scope = repository_path(scope);
    let mut command = Command::new("git");
    command
        .arg("-C")
        .arg(root)
        .args([
            "status",
            "--porcelain=v1",
            "-z",
            "--untracked-files=all",
            "--ignored=no",
            "--",
        ])
        .arg(if scope.is_empty() { "." } else { &scope });
    let output = successful_output(
        command_output(command, "read Git status for turn diff")?,
        "read Git status for turn diff",
    )?;
    let records = output.split(|byte| *byte == 0).collect::<Vec<_>>();
    let mut paths = HashSet::new();
    let mut index = 0;
    while index < records.len() {
        let record = records[index];
        index += 1;
        if record.len() < 4 {
            continue;
        }
        let status = &record[..2];
        paths.insert(String::from_utf8_lossy(&record[3..]).into_owned());
        if (status.contains(&b'R') || status.contains(&b'C'))
            && let Some(source) = records.get(index).filter(|source| !source.is_empty())
        {
            paths.insert(String::from_utf8_lossy(source).into_owned());
            index += 1;
        }
    }
    Ok(paths)
}

/// Captures worktree bytes only for paths already dirty; clean paths can be recovered from HEAD.
fn capture_git_baseline(
    repository_root: PathBuf,
    project_relative_path: PathBuf,
) -> Result<GitBaseline, String> {
    let started = std::time::Instant::now();
    let revision = git_head(&repository_root)?;
    let dirty_paths = git_status_paths(&repository_root, &project_relative_path)?;
    let dirty_files = capture_worktree_files(&repository_root, dirty_paths.iter())?;
    tracing::debug!(
        elapsed_ms = started.elapsed().as_millis() as u64,
        dirty_path_count = dirty_paths.len(),
        captured_file_count = dirty_files.files.len(),
        root = %repository_root.display(),
        "captured Git turn baseline"
    );
    Ok(GitBaseline {
        repository_root,
        project_relative_path,
        revision,
        dirty_paths,
        dirty_files,
    })
}

fn git_changed_paths(
    root: &Path,
    scope: &Path,
    old: Option<&str>,
    new: Option<&str>,
) -> Result<HashSet<String>, String> {
    let scope = repository_path(scope);
    let mut command = Command::new("git");
    command.arg("-C").arg(root);
    match (old, new) {
        (Some(old), Some(new)) if old != new => {
            command.args(["diff", "--name-only", "-z", old, new, "--"]);
        }
        (None, Some(new)) => {
            command.args(["ls-tree", "-r", "--name-only", "-z", new, "--"]);
        }
        (Some(old), None) => {
            command.args(["ls-tree", "-r", "--name-only", "-z", old, "--"]);
        }
        _ => return Ok(HashSet::new()),
    }
    command.arg(if scope.is_empty() { "." } else { &scope });
    let bytes = successful_output(
        command_output(command, "compare Git revisions for turn diff")?,
        "compare Git revisions for turn diff",
    )?;
    Ok(bytes
        .split(|byte| *byte == 0)
        .filter(|path| !path.is_empty())
        .map(|path| String::from_utf8_lossy(path).into_owned())
        .collect())
}

fn finish_git_turn(
    id: u64,
    prompt: String,
    started_at: u64,
    status: TurnDiffStatus,
    baseline: &GitBaseline,
) -> Result<TurnDiff, String> {
    let endpoint_revision = git_head(&baseline.repository_root)?;
    let endpoint_dirty =
        git_status_paths(&baseline.repository_root, &baseline.project_relative_path)?;
    // A turn can edit pre-existing dirty files, create new dirt, or commit changes and leave a
    // clean worktree. The union covers all three without snapshotting the whole repository.
    let mut candidates = baseline.dirty_paths.clone();
    candidates.extend(endpoint_dirty);
    candidates.extend(git_changed_paths(
        &baseline.repository_root,
        &baseline.project_relative_path,
        baseline.revision.as_deref(),
        endpoint_revision.as_deref(),
    )?);

    let endpoint_files = capture_worktree_files(&baseline.repository_root, candidates.iter())?;
    let mut old = WorkspaceSnapshot::default();
    let mut new = WorkspaceSnapshot::default();
    for repository_path in candidates {
        let Some(display_path) =
            project_display_path(&repository_path, &baseline.project_relative_path)
        else {
            continue;
        };
        let old_file = if baseline.dirty_paths.contains(&repository_path) {
            baseline.dirty_files.files.get(&repository_path).cloned()
        } else if let Some(revision) = baseline.revision.as_deref() {
            git_tree_file(&baseline.repository_root, revision, &repository_path)?
        } else {
            None
        };
        if let Some(file) = old_file {
            old.files.insert(display_path.clone(), file);
        }
        if let Some(file) = endpoint_files.files.get(&repository_path).cloned() {
            new.files.insert(display_path, file);
        }
    }
    Ok(build_turn(id, prompt, started_at, status, old, new))
}

fn capture_worktree_files<'a>(
    repository_root: &Path,
    paths: impl Iterator<Item = &'a String>,
) -> Result<WorkspaceSnapshot, String> {
    let mut snapshot = WorkspaceSnapshot::default();
    let mut stored_bytes = 0_u64;
    for path in paths {
        if let Some(file) = snapshot_path(&repository_root.join(path), &mut stored_bytes)? {
            snapshot.files.insert(path.clone(), file);
        }
    }
    Ok(snapshot)
}

fn snapshot_path(path: &Path, stored_bytes: &mut u64) -> Result<Option<SnapshotFile>, String> {
    let metadata = match fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(format!("could not inspect {}: {error}", path.display())),
    };
    let mode = file_mode(&metadata);
    if metadata.file_type().is_symlink() {
        let target = fs::read_link(path)
            .map_err(|error| format!("could not read link {}: {error}", path.display()))?;
        let bytes = target.to_string_lossy().into_owned().into_bytes();
        return Ok(Some(snapshot_bytes(bytes, mode, stored_bytes)));
    }
    if !metadata.is_file() {
        return Ok(None);
    }
    let len = metadata.len();
    // Hash every file for change and rename detection, but cap retained bytes so one turn cannot
    // make the UI or state database grow without bound.
    let can_store =
        len <= MAX_TEXT_FILE_BYTES && stored_bytes.saturating_add(len) <= MAX_SNAPSHOT_BYTES;
    if !can_store {
        return Ok(Some(omitted_snapshot(&metadata, mode)));
    }
    let mut stored = Some(Vec::new());
    let mut hasher = blake3::Hasher::new();
    let mut file =
        File::open(path).map_err(|error| format!("could not read {}: {error}", path.display()))?;
    let mut buffer = [0_u8; 64 * 1024];
    loop {
        let read = file
            .read(&mut buffer)
            .map_err(|error| format!("could not read {}: {error}", path.display()))?;
        if read == 0 {
            break;
        }
        hasher.update(&buffer[..read]);
        if let Some(bytes) = stored.as_mut() {
            bytes.extend_from_slice(&buffer[..read]);
        }
    }
    let binary = stored
        .as_deref()
        .is_some_and(|bytes| bytes.contains(&0) || std::str::from_utf8(bytes).is_err());
    if stored.is_some() {
        *stored_bytes = stored_bytes.saturating_add(len);
    }
    Ok(Some(SnapshotFile {
        hash: hasher.finalize(),
        bytes: stored,
        len,
        mode,
        binary,
    }))
}

fn omitted_snapshot(metadata: &fs::Metadata, mode: u32) -> SnapshotFile {
    let len = metadata.len();
    let modified = metadata
        .modified()
        .ok()
        .and_then(|time| time.duration_since(UNIX_EPOCH).ok())
        .map_or(0, |duration| duration.as_nanos());
    let mut hasher = blake3::Hasher::new();
    hasher.update(&len.to_le_bytes());
    hasher.update(&mode.to_le_bytes());
    hasher.update(&modified.to_le_bytes());
    SnapshotFile {
        hash: hasher.finalize(),
        bytes: None,
        len,
        mode,
        binary: false,
    }
}

fn snapshot_bytes(bytes: Vec<u8>, mode: u32, stored_bytes: &mut u64) -> SnapshotFile {
    let len = bytes.len() as u64;
    let binary = bytes.contains(&0) || std::str::from_utf8(&bytes).is_err();
    let hash = blake3::hash(&bytes);
    let can_store =
        len <= MAX_TEXT_FILE_BYTES && stored_bytes.saturating_add(len) <= MAX_SNAPSHOT_BYTES;
    let bytes = can_store.then_some(bytes);
    if bytes.is_some() {
        *stored_bytes = stored_bytes.saturating_add(len);
    }
    SnapshotFile {
        hash,
        bytes,
        len,
        mode,
        binary,
    }
}

fn git_tree_file(root: &Path, revision: &str, path: &str) -> Result<Option<SnapshotFile>, String> {
    let mut command = Command::new("git");
    command
        .arg("-C")
        .arg(root)
        .args(["ls-tree", "-z", revision, "--", path]);
    let listing = successful_output(
        command_output(command, "inspect a Git tree entry")?,
        "inspect a Git tree entry",
    )?;
    let Some(header) = listing.split(|byte| *byte == b'\t').next() else {
        return Ok(None);
    };
    if header.is_empty() {
        return Ok(None);
    }
    let fields = header.split(|byte| *byte == b' ').collect::<Vec<_>>();
    if fields.len() < 3 {
        return Err(format!("Git returned an invalid tree entry for {path}"));
    }
    let mode = u32::from_str_radix(&String::from_utf8_lossy(fields[0]), 8).unwrap_or(0);
    if fields[1] != b"blob" {
        return Ok(Some(SnapshotFile {
            hash: blake3::hash(fields[2]),
            bytes: None,
            len: 0,
            mode,
            binary: true,
        }));
    }
    let object = String::from_utf8_lossy(fields[2]);
    let bytes = git_output(root, &["cat-file", "blob", &object], "read a Git blob")?;
    let mut stored_bytes = 0;
    Ok(Some(snapshot_bytes(bytes, mode, &mut stored_bytes)))
}

#[derive(Debug)]
struct JjDiffEntry {
    status: String,
    source_path: String,
    target_path: String,
    source_type: String,
    target_type: String,
    source_executable: bool,
    target_executable: bool,
}

fn jj_command(root: &Path, ignore_working_copy: bool) -> Command {
    let mut command = Command::new("jj");
    command.args(["--no-pager", "--color=never", "-R"]);
    command.arg(root).current_dir(root);
    if ignore_working_copy {
        command.arg("--ignore-working-copy");
    }
    command
}

fn jj_snapshot_revision(root: &Path) -> Result<String, String> {
    let mut command = jj_command(root, false);
    command.args(["log", "--no-graph", "-r", "@", "-T", "commit_id ++ \"\\n\""]);
    let bytes = successful_output(
        command_output(command, "snapshot the JJ working copy")?,
        "snapshot the JJ working copy",
    )?;
    let revision = String::from_utf8_lossy(&bytes).trim().to_string();
    if revision.is_empty() {
        Err("JJ did not report a working-copy commit".into())
    } else {
        Ok(revision)
    }
}

fn jj_diff_entries(baseline: &JjBaseline, endpoint: &str) -> Result<Vec<JjDiffEntry>, String> {
    let template = concat!(
        "status ++ \"\\0\" ++ source.path() ++ \"\\0\" ++ ",
        "target.path() ++ \"\\0\" ++ source.file_type() ++ \"\\0\" ++ ",
        "target.file_type() ++ \"\\0\" ++ source.executable() ++ \"\\0\" ++ ",
        "target.executable() ++ \"\\0\""
    );
    let mut command = jj_command(&baseline.repository_root, true);
    command.args([
        "diff",
        "--from",
        &baseline.revision,
        "--to",
        endpoint,
        "-T",
        template,
    ]);
    let bytes = successful_output(
        command_output(command, "compare JJ checkpoints for turn diff")?,
        "compare JJ checkpoints for turn diff",
    )?;
    let fields = bytes.split(|byte| *byte == 0).collect::<Vec<_>>();
    let mut entries = Vec::new();
    for record in fields.chunks(7) {
        if record.len() < 7 || record[0].is_empty() {
            continue;
        }
        entries.push(JjDiffEntry {
            status: String::from_utf8_lossy(record[0]).into_owned(),
            source_path: String::from_utf8_lossy(record[1]).into_owned(),
            target_path: String::from_utf8_lossy(record[2]).into_owned(),
            source_type: String::from_utf8_lossy(record[3]).into_owned(),
            target_type: String::from_utf8_lossy(record[4]).into_owned(),
            source_executable: record[5] == b"true",
            target_executable: record[6] == b"true",
        });
    }
    Ok(entries)
}

fn finish_jj_turn(
    id: u64,
    prompt: String,
    started_at: u64,
    status: TurnDiffStatus,
    baseline: &JjBaseline,
) -> Result<TurnDiff, String> {
    let endpoint = jj_snapshot_revision(&baseline.repository_root)?;
    let entries = jj_diff_entries(baseline, &endpoint)?;
    let mut files = Vec::new();
    for entry in entries {
        let source_display =
            project_display_path(&entry.source_path, &baseline.project_relative_path);
        let target_display =
            project_display_path(&entry.target_path, &baseline.project_relative_path);
        let (old_path, new_path, requested_kind) = match entry.status.as_str() {
            "modified" if source_display.is_some() => (
                source_display.clone(),
                target_display.clone(),
                FileDiffKind::Modified,
            ),
            "added" if target_display.is_some() => {
                (None, target_display.clone(), FileDiffKind::Added)
            }
            "removed" if source_display.is_some() => {
                (source_display.clone(), None, FileDiffKind::Deleted)
            }
            "renamed" if source_display.is_some() && target_display.is_some() => (
                source_display.clone(),
                target_display.clone(),
                FileDiffKind::Renamed,
            ),
            "renamed" if source_display.is_some() => {
                (source_display.clone(), None, FileDiffKind::Deleted)
            }
            "renamed" if target_display.is_some() => {
                (None, target_display.clone(), FileDiffKind::Added)
            }
            "copied" if target_display.is_some() => {
                (None, target_display.clone(), FileDiffKind::Added)
            }
            _ => continue,
        };
        let old = if old_path.is_some() {
            Some(jj_file(
                &baseline.repository_root,
                &baseline.revision,
                &entry.source_path,
                &entry.source_type,
                entry.source_executable,
            )?)
        } else {
            None
        };
        let new = if new_path.is_some() {
            Some(jj_file(
                &baseline.repository_root,
                &endpoint,
                &entry.target_path,
                &entry.target_type,
                entry.target_executable,
            )?)
        } else {
            None
        };
        let path = new_path
            .clone()
            .or_else(|| old_path.clone())
            .expect("scoped JJ diff has a path");
        let rename_source = (requested_kind == FileDiffKind::Renamed)
            .then(|| old_path.expect("JJ rename has a source"));
        files.push(build_file_diff(
            path,
            rename_source,
            requested_kind,
            old,
            new,
        ));
    }
    files.sort_by(|left, right| left.path.cmp(&right.path));
    let additions = files.iter().map(|file| file.additions).sum();
    let deletions = files.iter().map(|file| file.deletions).sum();
    Ok(TurnDiff {
        id,
        prompt,
        started_at,
        finished_at: unix_timestamp(),
        status,
        files,
        additions,
        deletions,
        error: None,
    })
}

fn jj_file(
    root: &Path,
    revision: &str,
    path: &str,
    file_type: &str,
    executable: bool,
) -> Result<SnapshotFile, String> {
    if matches!(file_type, "git-submodule" | "conflict" | "tree") {
        return Ok(SnapshotFile {
            hash: blake3::hash(path.as_bytes()),
            bytes: None,
            len: 0,
            mode: if file_type == "git-submodule" {
                0o160000
            } else {
                0
            },
            binary: true,
        });
    }
    let mut command = jj_command(root, true);
    command.args(["file", "show", "-r", revision, "--", path]);
    let bytes = successful_output(
        command_output(command, "read a file from a JJ checkpoint")?,
        "read a file from a JJ checkpoint",
    )?;
    let mode = if file_type == "symlink" {
        0o120000
    } else if executable {
        0o100755
    } else {
        0o100644
    };
    let mut stored_bytes = 0;
    Ok(snapshot_bytes(bytes, mode, &mut stored_bytes))
}

fn repository_path(path: &Path) -> String {
    path.to_string_lossy().replace('\\', "/")
}

fn project_display_path(path_in_repository: &str, scope: &Path) -> Option<String> {
    let path = Path::new(path_in_repository);
    let relative = if scope.as_os_str().is_empty() {
        path
    } else {
        path.strip_prefix(scope).ok()?
    };
    let display = repository_path(relative);
    (!display.is_empty()).then_some(display)
}

#[cfg(unix)]
fn file_mode(metadata: &fs::Metadata) -> u32 {
    use std::os::unix::fs::MetadataExt as _;
    metadata.mode()
}

#[cfg(not(unix))]
fn file_mode(metadata: &fs::Metadata) -> u32 {
    u32::from(metadata.permissions().readonly())
}

/// Compares snapshots and recognizes exact-content renames before constructing file diffs.
fn build_turn(
    id: u64,
    prompt: String,
    started_at: u64,
    status: TurnDiffStatus,
    mut old: WorkspaceSnapshot,
    mut new: WorkspaceSnapshot,
) -> TurnDiff {
    let mut renamed = HashMap::new();
    let deleted = old
        .files
        .iter()
        .filter(|(path, _)| !new.files.contains_key(*path))
        .map(|(path, file)| (path.clone(), file.hash))
        .collect::<Vec<_>>();
    let added = new
        .files
        .iter()
        .filter(|(path, _)| !old.files.contains_key(*path))
        .map(|(path, file)| (path.clone(), file.hash))
        .collect::<Vec<_>>();
    let mut used_additions = HashSet::new();
    // Rename detection is intentionally conservative: only exact hashes match, and each added
    // destination can satisfy at most one deletion.
    for (old_path, old_hash) in &deleted {
        if let Some((new_path, _)) = added
            .iter()
            .find(|(new_path, hash)| *hash == *old_hash && !used_additions.contains(new_path))
        {
            renamed.insert(old_path.clone(), new_path.clone());
            used_additions.insert(new_path.clone());
        }
    }

    let mut files = Vec::new();
    let paths = old
        .files
        .keys()
        .chain(new.files.keys())
        .cloned()
        .collect::<HashSet<_>>();
    let mut paths = paths.into_iter().collect::<Vec<_>>();
    paths.sort();
    let renamed_destinations = renamed.values().cloned().collect::<HashSet<_>>();
    for path in paths {
        if renamed_destinations.contains(&path) {
            continue;
        }
        if let Some(new_path) = renamed.get(&path) {
            let old_file = old.files.remove(&path).expect("rename source exists");
            let new_file = new
                .files
                .remove(new_path)
                .expect("rename destination exists");
            files.push(build_file_diff(
                new_path.clone(),
                Some(path),
                FileDiffKind::Renamed,
                Some(old_file),
                Some(new_file),
            ));
            continue;
        }
        let old_file = old.files.remove(&path);
        let new_file = new.files.remove(&path);
        if old_file
            .as_ref()
            .zip(new_file.as_ref())
            .is_some_and(|(old, new)| old.hash == new.hash && old.mode == new.mode)
        {
            continue;
        }
        let kind = match (&old_file, &new_file) {
            (None, Some(_)) => FileDiffKind::Added,
            (Some(_), None) => FileDiffKind::Deleted,
            (Some(old), Some(new)) if old.hash == new.hash => FileDiffKind::ModeChanged,
            _ => FileDiffKind::Modified,
        };
        files.push(build_file_diff(path, None, kind, old_file, new_file));
    }
    files.sort_by(|left, right| left.path.cmp(&right.path));
    let additions = files.iter().map(|file| file.additions).sum();
    let deletions = files.iter().map(|file| file.deletions).sum();
    TurnDiff {
        id,
        prompt,
        started_at,
        finished_at: unix_timestamp(),
        status,
        files,
        additions,
        deletions,
        error: None,
    }
}

fn build_file_diff(
    path: String,
    old_path: Option<String>,
    requested_kind: FileDiffKind,
    old: Option<SnapshotFile>,
    new: Option<SnapshotFile>,
) -> FileDiff {
    let old_exists = old.is_some();
    let new_exists = new.is_some();
    let old_mode = old.as_ref().map_or(0, |file| file.mode);
    let new_mode = new.as_ref().map_or(0, |file| file.mode);
    let binary = old.as_ref().is_some_and(|file| file.binary)
        || new.as_ref().is_some_and(|file| file.binary);
    let omitted = old.as_ref().is_some_and(|file| file.bytes.is_none())
        || new.as_ref().is_some_and(|file| file.bytes.is_none());
    let old_text = old
        .as_ref()
        .and_then(|file| file.bytes.as_deref())
        .and_then(|bytes| std::str::from_utf8(bytes).ok())
        .map(str::to_string);
    let new_text = new
        .as_ref()
        .and_then(|file| file.bytes.as_deref())
        .and_then(|bytes| std::str::from_utf8(bytes).ok())
        .map(str::to_string);
    let (kind, hunks, additions, deletions, message) = if omitted {
        let old_size = old.as_ref().map_or(0, |file| file.len);
        let new_size = new.as_ref().map_or(0, |file| file.len);
        (
            FileDiffKind::Omitted,
            Vec::new(),
            0,
            0,
            Some(format!(
                "Large file omitted ({old_size} → {new_size} bytes)"
            )),
        )
    } else if binary {
        (
            FileDiffKind::Binary,
            Vec::new(),
            0,
            0,
            Some("Binary file changed".into()),
        )
    } else if requested_kind == FileDiffKind::ModeChanged {
        (
            requested_kind,
            Vec::new(),
            0,
            0,
            Some(format!("File mode changed {old_mode:o} → {new_mode:o}")),
        )
    } else {
        let (hunks, additions, deletions) = diff_text(
            old_text.as_deref().unwrap_or_default(),
            new_text.as_deref().unwrap_or_default(),
        );
        (requested_kind, hunks, additions, deletions, None)
    };
    let mut file = FileDiff {
        path,
        old_path,
        kind,
        old_text,
        new_text,
        old_mode,
        new_mode,
        old_exists: Some(old_exists),
        new_exists: Some(new_exists),
        hunks,
        additions,
        deletions,
        message,
        old_highlights: Vec::new(),
        new_highlights: Vec::new(),
    };
    file.refresh_highlights();
    file
}
