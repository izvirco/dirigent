use ropey::{Rope, RopeSlice};
use serde::{Deserialize, Serialize};
use similar::{DiffTag, TextDiff};
use std::{
    borrow::Borrow,
    collections::{BTreeMap, HashMap, HashSet},
    fs::{self, File},
    io::Read as _,
    ops::Range,
    path::{Path, PathBuf},
    process::{Command, Output, Stdio},
    time::{Duration, SystemTime, UNIX_EPOCH},
};
use tree_house::{
    Language, LanguageConfig, LanguageLoader, Syntax,
    highlighter::{Highlight, HighlightEvent, Highlighter},
    tree_sitter::Grammar,
};

use crate::{platform, theme};

const MAX_TEXT_FILE_BYTES: u64 = 2 * 1024 * 1024;
const MAX_SNAPSHOT_BYTES: u64 = 48 * 1024 * 1024;
const VCS_COMMAND_TIMEOUT: Duration = Duration::from_secs(2);
const DIFF_CONTEXT_LINES: usize = 3;

#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq)]
pub(crate) enum DiffViewMode {
    Unified,
    Split,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum DiffScope {
    Cumulative,
    Turn,
}

#[derive(Clone, Debug)]
pub(crate) struct DiffSelectionReference {
    pub(crate) turn_id: u64,
    pub(crate) path: String,
    pub(crate) side: &'static str,
    pub(crate) lines: String,
    pub(crate) language: String,
}

#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq)]
pub(crate) enum TurnDiffStatus {
    Completed,
    Aborted,
    Failed,
    Interrupted,
    Unavailable,
}

#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq)]
pub(crate) enum FileDiffKind {
    Added,
    Modified,
    Deleted,
    Renamed,
    Binary,
    Omitted,
    ModeChanged,
}

#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq)]
pub(crate) enum DiffRowKind {
    Context,
    Added,
    Removed,
    Replaced,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub(crate) struct DiffRow {
    pub(crate) old_number: Option<u32>,
    pub(crate) new_number: Option<u32>,
    pub(crate) old_text: Option<String>,
    pub(crate) new_text: Option<String>,
    pub(crate) kind: DiffRowKind,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub(crate) struct DiffHunk {
    pub(crate) old_start: u32,
    pub(crate) old_len: u32,
    pub(crate) new_start: u32,
    pub(crate) new_len: u32,
    pub(crate) rows: Vec<DiffRow>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub(crate) struct SyntaxSpan {
    pub(crate) range: Range<usize>,
    pub(crate) color: u32,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub(crate) struct FileDiff {
    pub(crate) path: String,
    pub(crate) old_path: Option<String>,
    pub(crate) kind: FileDiffKind,
    pub(crate) old_text: Option<String>,
    pub(crate) new_text: Option<String>,
    pub(crate) old_mode: u32,
    pub(crate) new_mode: u32,
    #[serde(default)]
    pub(crate) old_exists: Option<bool>,
    #[serde(default)]
    pub(crate) new_exists: Option<bool>,
    pub(crate) hunks: Vec<DiffHunk>,
    pub(crate) additions: usize,
    pub(crate) deletions: usize,
    pub(crate) message: Option<String>,
    #[serde(skip)]
    pub(crate) old_highlights: Vec<SyntaxSpan>,
    #[serde(skip)]
    pub(crate) new_highlights: Vec<SyntaxSpan>,
}

impl FileDiff {
    fn refresh_highlights(&mut self) {
        self.old_highlights = self
            .old_text
            .as_deref()
            .map(|text| highlight_text(self.old_path.as_deref().unwrap_or(&self.path), text))
            .unwrap_or_default();
        self.new_highlights = self
            .new_text
            .as_deref()
            .map(|text| highlight_text(&self.path, text))
            .unwrap_or_default();
    }
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub(crate) struct TurnDiff {
    pub(crate) id: u64,
    pub(crate) prompt: String,
    pub(crate) started_at: u64,
    pub(crate) finished_at: u64,
    pub(crate) status: TurnDiffStatus,
    pub(crate) files: Vec<FileDiff>,
    pub(crate) additions: usize,
    pub(crate) deletions: usize,
    pub(crate) error: Option<String>,
}

impl TurnDiff {
    pub(crate) fn refresh_highlights(&mut self) {
        for file in &mut self.files {
            file.refresh_highlights();
        }
    }
}

#[derive(Clone)]
struct CombinedFileVersion {
    exists: bool,
    text: Option<String>,
    mode: u32,
    highlights: Vec<SyntaxSpan>,
    opaque_kind: Option<FileDiffKind>,
    message: Option<String>,
}

#[derive(Clone)]
struct CombinedFile {
    original_path: String,
    current_path: String,
    old: CombinedFileVersion,
    new: CombinedFileVersion,
}

fn combined_version(file: &FileDiff, old: bool) -> CombinedFileVersion {
    let exists = if old {
        file.old_exists.unwrap_or(file.kind != FileDiffKind::Added)
    } else {
        file.new_exists
            .unwrap_or(file.kind != FileDiffKind::Deleted)
    };
    CombinedFileVersion {
        exists,
        text: if old {
            file.old_text.clone()
        } else {
            file.new_text.clone()
        },
        mode: if old { file.old_mode } else { file.new_mode },
        highlights: if old {
            file.old_highlights.clone()
        } else {
            file.new_highlights.clone()
        },
        opaque_kind: matches!(file.kind, FileDiffKind::Binary | FileDiffKind::Omitted)
            .then_some(file.kind),
        message: file.message.clone(),
    }
}

fn finish_combined_file(file: CombinedFile) -> Option<FileDiff> {
    if !file.old.exists && !file.new.exists {
        return None;
    }
    let renamed = file.old.exists && file.new.exists && file.original_path != file.current_path;
    let text_available = (!file.old.exists || file.old.text.is_some())
        && (!file.new.exists || file.new.text.is_some());
    let unchanged = file.old.exists
        && file.new.exists
        && !renamed
        && file.old.mode == file.new.mode
        && text_available
        && file.old.text == file.new.text;
    if unchanged {
        return None;
    }

    let requested_kind = match (file.old.exists, file.new.exists, renamed) {
        (false, true, _) => FileDiffKind::Added,
        (true, false, _) => FileDiffKind::Deleted,
        (true, true, true) => FileDiffKind::Renamed,
        (true, true, false) if file.old.text == file.new.text => FileDiffKind::ModeChanged,
        _ => FileDiffKind::Modified,
    };
    let opaque_kind = file.old.opaque_kind.or(file.new.opaque_kind);
    let (kind, hunks, additions, deletions, message) = if let Some(kind) = opaque_kind {
        (
            kind,
            Vec::new(),
            0,
            0,
            file.new.message.or(file.old.message),
        )
    } else if requested_kind == FileDiffKind::ModeChanged {
        (
            requested_kind,
            Vec::new(),
            0,
            0,
            Some(format!(
                "File mode changed {:o} → {:o}",
                file.old.mode, file.new.mode
            )),
        )
    } else {
        let (hunks, additions, deletions) = diff_text(
            file.old.text.as_deref().unwrap_or_default(),
            file.new.text.as_deref().unwrap_or_default(),
        );
        (requested_kind, hunks, additions, deletions, None)
    };

    Some(FileDiff {
        path: file.current_path.clone(),
        old_path: renamed.then_some(file.original_path),
        kind,
        old_text: file.old.exists.then_some(file.old.text).flatten(),
        new_text: file.new.exists.then_some(file.new.text).flatten(),
        old_mode: file.old.mode,
        new_mode: file.new.mode,
        old_exists: Some(file.old.exists),
        new_exists: Some(file.new.exists),
        hunks,
        additions,
        deletions,
        message,
        old_highlights: file.old.highlights,
        new_highlights: file.new.highlights,
    })
}

/// Collapse consecutive per-turn snapshots into the net workspace change through the last turn.
pub(crate) fn combine_turn_diffs<T>(turns: &[T]) -> Option<TurnDiff>
where
    T: Borrow<TurnDiff>,
{
    let last = turns.last()?.borrow();
    let mut combined = BTreeMap::<String, CombinedFile>::new();
    for turn in turns {
        let turn = turn.borrow();
        for file in &turn.files {
            let source_path = file.old_path.as_deref().unwrap_or(&file.path);
            let old = combined
                .remove(source_path)
                .unwrap_or_else(|| CombinedFile {
                    original_path: source_path.to_string(),
                    current_path: source_path.to_string(),
                    old: combined_version(file, true),
                    new: combined_version(file, true),
                });
            let mut updated = old;
            updated.current_path = file.path.clone();
            updated.new = combined_version(file, false);
            combined.insert(file.path.clone(), updated);
        }
    }

    let mut files = combined
        .into_values()
        .filter_map(finish_combined_file)
        .collect::<Vec<_>>();
    files.sort_by(|left, right| left.path.cmp(&right.path));
    let additions = files.iter().map(|file| file.additions).sum();
    let deletions = files.iter().map(|file| file.deletions).sum();
    Some(TurnDiff {
        id: last.id,
        prompt: last.prompt.clone(),
        started_at: turns
            .first()
            .map_or(last.started_at, |turn| turn.borrow().started_at),
        finished_at: last.finished_at,
        status: last.status,
        error: files
            .is_empty()
            .then(|| "No net file changes through this turn.".into()),
        files,
        additions,
        deletions,
    })
}

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

fn source_lines(text: &str) -> Vec<&str> {
    text.split_inclusive('\n')
        .map(|line| {
            let line = line.strip_suffix('\n').unwrap_or(line);
            line.strip_suffix('\r').unwrap_or(line)
        })
        .collect()
}

fn diff_text(old: &str, new: &str) -> (Vec<DiffHunk>, usize, usize) {
    let old_lines = source_lines(old);
    let new_lines = source_lines(new);
    let diff = TextDiff::from_lines(old, new);
    let mut hunks = Vec::new();
    let mut additions = 0;
    let mut deletions = 0;
    for group in diff.grouped_ops(DIFF_CONTEXT_LINES) {
        let old_start = group.first().map_or(0, |op| op.old_range().start) as u32 + 1;
        let new_start = group.first().map_or(0, |op| op.new_range().start) as u32 + 1;
        let mut rows = Vec::new();
        for op in &group {
            let old_range = op.old_range();
            let new_range = op.new_range();
            match op.tag() {
                DiffTag::Equal => {
                    for offset in 0..old_range.len() {
                        rows.push(DiffRow {
                            old_number: Some((old_range.start + offset + 1) as u32),
                            new_number: Some((new_range.start + offset + 1) as u32),
                            old_text: Some(old_lines[old_range.start + offset].to_string()),
                            new_text: Some(new_lines[new_range.start + offset].to_string()),
                            kind: DiffRowKind::Context,
                        });
                    }
                }
                DiffTag::Delete => {
                    deletions += old_range.len();
                    for index in old_range {
                        rows.push(DiffRow {
                            old_number: Some((index + 1) as u32),
                            new_number: None,
                            old_text: Some(old_lines[index].to_string()),
                            new_text: None,
                            kind: DiffRowKind::Removed,
                        });
                    }
                }
                DiffTag::Insert => {
                    additions += new_range.len();
                    for index in new_range {
                        rows.push(DiffRow {
                            old_number: None,
                            new_number: Some((index + 1) as u32),
                            old_text: None,
                            new_text: Some(new_lines[index].to_string()),
                            kind: DiffRowKind::Added,
                        });
                    }
                }
                DiffTag::Replace => {
                    additions += new_range.len();
                    deletions += old_range.len();
                    let count = old_range.len().max(new_range.len());
                    for offset in 0..count {
                        let old_index = old_range.start + offset;
                        let new_index = new_range.start + offset;
                        rows.push(DiffRow {
                            old_number: (old_index < old_range.end)
                                .then_some((old_index + 1) as u32),
                            new_number: (new_index < new_range.end)
                                .then_some((new_index + 1) as u32),
                            old_text: (old_index < old_range.end)
                                .then(|| old_lines[old_index].to_string()),
                            new_text: (new_index < new_range.end)
                                .then(|| new_lines[new_index].to_string()),
                            kind: DiffRowKind::Replaced,
                        });
                    }
                }
            }
        }
        let old_len = group.iter().map(|op| op.old_range().len()).sum::<usize>() as u32;
        let new_len = group.iter().map(|op| op.new_range().len()).sum::<usize>() as u32;
        hunks.push(DiffHunk {
            old_start,
            old_len,
            new_start,
            new_len,
            rows,
        });
    }
    (hunks, additions, deletions)
}

struct HighlightLoader {
    configs: Vec<LanguageConfig>,
    names: HashMap<&'static str, Language>,
}

impl HighlightLoader {
    fn new() -> Self {
        let mut loader = Self {
            configs: Vec::new(),
            names: HashMap::new(),
        };
        macro_rules! add {
            ($names:expr, $grammar:expr, $highlights:expr, $injections:expr, $locals:expr) => {{
                let names: &[&'static str] = &$names;
                match Grammar::try_from($grammar) {
                    Ok(grammar) => match LanguageConfig::new(grammar, $highlights, $injections, $locals) {
                        Ok(config) => {
                            let language = Language::new(loader.configs.len() as u32);
                            config.configure(|capture| highlight_for_capture(capture));
                            loader.configs.push(config);
                            for &name in names { loader.names.insert(name, language); }
                        }
                        Err(error) => {
                            tracing::warn!(language = names[0], error = %error, "could not load syntax query");
                        }
                    },
                    Err(error) => {
                        tracing::warn!(language = names[0], error = %error, "could not load syntax grammar");
                    }
                }
            }};
        }
        add!(
            ["rust", "rs"],
            tree_sitter_rust::LANGUAGE,
            tree_sitter_rust::HIGHLIGHTS_QUERY,
            tree_sitter_rust::INJECTIONS_QUERY,
            ""
        );
        add!(
            ["javascript", "js", "jsx"],
            tree_sitter_javascript::LANGUAGE,
            tree_sitter_javascript::HIGHLIGHT_QUERY,
            "",
            tree_sitter_javascript::LOCALS_QUERY
        );
        add!(
            ["typescript", "ts"],
            tree_sitter_typescript::LANGUAGE_TYPESCRIPT,
            tree_sitter_typescript::HIGHLIGHTS_QUERY,
            "",
            tree_sitter_typescript::LOCALS_QUERY
        );
        add!(
            ["tsx"],
            tree_sitter_typescript::LANGUAGE_TSX,
            tree_sitter_typescript::HIGHLIGHTS_QUERY,
            "",
            tree_sitter_typescript::LOCALS_QUERY
        );
        add!(
            ["json"],
            tree_sitter_json::LANGUAGE,
            tree_sitter_json::HIGHLIGHTS_QUERY,
            "",
            ""
        );
        add!(
            ["toml"],
            tree_sitter_toml_ng::LANGUAGE,
            tree_sitter_toml_ng::HIGHLIGHTS_QUERY,
            "",
            ""
        );
        add!(
            ["python", "py"],
            tree_sitter_python::LANGUAGE,
            tree_sitter_python::HIGHLIGHTS_QUERY,
            "",
            ""
        );
        add!(
            ["go"],
            tree_sitter_go::LANGUAGE,
            tree_sitter_go::HIGHLIGHTS_QUERY,
            "",
            ""
        );
        add!(
            ["bash", "sh"],
            tree_sitter_bash::LANGUAGE,
            tree_sitter_bash::HIGHLIGHT_QUERY,
            "",
            ""
        );
        add!(
            ["c"],
            tree_sitter_c::LANGUAGE,
            tree_sitter_c::HIGHLIGHT_QUERY,
            "",
            ""
        );
        add!(
            ["cpp", "c++"],
            tree_sitter_cpp::LANGUAGE,
            tree_sitter_cpp::HIGHLIGHT_QUERY,
            "",
            ""
        );
        add!(
            ["html"],
            tree_sitter_html::LANGUAGE,
            tree_sitter_html::HIGHLIGHTS_QUERY,
            tree_sitter_html::INJECTIONS_QUERY,
            ""
        );
        add!(
            ["css"],
            tree_sitter_css::LANGUAGE,
            tree_sitter_css::HIGHLIGHTS_QUERY,
            "",
            ""
        );
        add!(
            ["yaml", "yml"],
            tree_sitter_yaml::LANGUAGE,
            tree_sitter_yaml::HIGHLIGHTS_QUERY,
            "",
            ""
        );
        add!(
            ["markdown", "md"],
            tree_sitter_md::LANGUAGE,
            tree_sitter_md::HIGHLIGHT_QUERY_BLOCK,
            tree_sitter_md::INJECTION_QUERY_BLOCK,
            ""
        );
        add!(
            ["markdown_inline"],
            tree_sitter_md::INLINE_LANGUAGE,
            tree_sitter_md::HIGHLIGHT_QUERY_INLINE,
            tree_sitter_md::INJECTION_QUERY_INLINE,
            ""
        );
        loader
    }

    fn language_for_path(&self, path: &str) -> Option<Language> {
        let extension = Path::new(path).extension()?.to_str()?.to_ascii_lowercase();
        self.names.get(extension.as_str()).copied()
    }

    fn marker_name<'a>(&self, marker: tree_house::InjectionLanguageMarker<'a>) -> Option<String> {
        match marker {
            tree_house::InjectionLanguageMarker::Name(name) => Some(name.to_ascii_lowercase()),
            tree_house::InjectionLanguageMarker::Match(text)
            | tree_house::InjectionLanguageMarker::Filename(text)
            | tree_house::InjectionLanguageMarker::Shebang(text) => {
                Some(text.to_string().trim().to_ascii_lowercase())
            }
        }
    }
}

impl LanguageLoader for HighlightLoader {
    fn language_for_marker(
        &self,
        marker: tree_house::InjectionLanguageMarker<'_>,
    ) -> Option<Language> {
        let marker = self.marker_name(marker)?;
        self.names.get(marker.as_str()).copied().or_else(|| {
            Path::new(&marker)
                .extension()
                .and_then(|ext| ext.to_str())
                .and_then(|ext| self.names.get(ext).copied())
        })
    }

    fn get_config(&self, lang: Language) -> Option<&LanguageConfig> {
        self.configs.get(lang.idx())
    }
}

#[derive(Clone, Copy)]
#[repr(usize)]
enum SyntaxCapture {
    Annotation,
    Attribute,
    Comment,
    Constant,
    ConstantCharacter,
    ConstantCharacterEscape,
    ConstantMacro,
    Constructor,
    Function,
    FunctionBuiltin,
    FunctionMacro,
    Keyword,
    KeywordControlImport,
    Label,
    Module,
    Namespace,
    Operator,
    Punctuation,
    Special,
    String,
    StringRegexp,
    StringSpecial,
    StringSymbol,
    Tag,
    Type,
    Variable,
    VariableBuiltin,
    VariableOtherMember,
    VariableParameter,
}

fn capture_for_scope(scope: &str) -> Option<SyntaxCapture> {
    Some(match scope {
        "annotation" => SyntaxCapture::Annotation,
        "attribute" => SyntaxCapture::Attribute,
        "comment" => SyntaxCapture::Comment,
        "constant" => SyntaxCapture::Constant,
        "constant.character" | "character" => SyntaxCapture::ConstantCharacter,
        "constant.character.escape" | "escape" | "string.escape" => {
            SyntaxCapture::ConstantCharacterEscape
        }
        "constant.macro" => SyntaxCapture::ConstantMacro,
        "constructor" => SyntaxCapture::Constructor,
        "function" | "method" => SyntaxCapture::Function,
        "function.builtin" => SyntaxCapture::FunctionBuiltin,
        "function.macro" => SyntaxCapture::FunctionMacro,
        "keyword" => SyntaxCapture::Keyword,
        "keyword.control.import" | "import" | "include" => SyntaxCapture::KeywordControlImport,
        "label" => SyntaxCapture::Label,
        "module" => SyntaxCapture::Module,
        "namespace" => SyntaxCapture::Namespace,
        "operator" => SyntaxCapture::Operator,
        "punctuation" | "delimiter" => SyntaxCapture::Punctuation,
        "special" | "embedded" => SyntaxCapture::Special,
        "string" => SyntaxCapture::String,
        "string.regexp" => SyntaxCapture::StringRegexp,
        "string.special" => SyntaxCapture::StringSpecial,
        "string.symbol" => SyntaxCapture::StringSymbol,
        "tag" => SyntaxCapture::Tag,
        "type" => SyntaxCapture::Type,
        "variable" => SyntaxCapture::Variable,
        "variable.builtin" => SyntaxCapture::VariableBuiltin,
        "variable.other.member" | "property" => SyntaxCapture::VariableOtherMember,
        "variable.parameter" | "parameter" => SyntaxCapture::VariableParameter,
        "number" | "boolean" => SyntaxCapture::Constant,
        _ => return None,
    })
}

fn highlight_for_capture(capture: &str) -> Option<Highlight> {
    let mut scope = capture;
    loop {
        if let Some(capture) = capture_for_scope(scope) {
            return Some(Highlight::new(capture as u32));
        }
        let (parent, _) = scope.rsplit_once('.')?;
        scope = parent;
    }
}

fn capture_color(index: usize) -> u32 {
    const COLORS: [fn() -> u32; 29] = [
        theme::syntax_annotation,
        theme::syntax_attribute,
        theme::syntax_comment,
        theme::syntax_constant,
        theme::syntax_constant_character,
        theme::syntax_constant_character_escape,
        theme::syntax_constant_macro,
        theme::syntax_constructor,
        theme::syntax_function,
        theme::syntax_function_builtin,
        theme::syntax_function_macro,
        theme::syntax_keyword,
        theme::syntax_keyword_control_import,
        theme::syntax_label,
        theme::syntax_module,
        theme::syntax_namespace,
        theme::syntax_operator,
        theme::syntax_punctuation,
        theme::syntax_special,
        theme::syntax_string,
        theme::syntax_string_regexp,
        theme::syntax_string_special,
        theme::syntax_string_symbol,
        theme::syntax_tag,
        theme::syntax_type,
        theme::syntax_variable,
        theme::syntax_variable_builtin,
        theme::syntax_variable_other_member,
        theme::syntax_variable_parameter,
    ];
    COLORS
        .get(index)
        .map_or_else(theme::code_text, |color| color())
}

fn highlight_text(path: &str, text: &str) -> Vec<SyntaxSpan> {
    static LOADER: std::sync::OnceLock<HighlightLoader> = std::sync::OnceLock::new();
    let loader = LOADER.get_or_init(HighlightLoader::new);
    let Some(language) = loader.language_for_path(path) else {
        return Vec::new();
    };
    if text.is_empty() || text.len() > u32::MAX as usize {
        return Vec::new();
    }
    let rope = Rope::from_str(text);
    let source: RopeSlice<'_> = rope.slice(..);
    let Ok(syntax) = Syntax::new(source, language, Duration::from_millis(500), loader) else {
        return Vec::new();
    };
    let mut highlighter = Highlighter::new(&syntax, source, loader, 0..text.len() as u32);
    let mut position = highlighter.next_event_offset();
    let mut active = Vec::new();
    let mut spans = Vec::new();
    while position < text.len() as u32 {
        let (event, highlights) = highlighter.advance();
        if event == HighlightEvent::Refresh {
            active.clear();
        }
        active.extend(highlights);
        let start = position;
        position = highlighter.next_event_offset().min(text.len() as u32);
        if position <= start {
            break;
        }
        if let Some(highlight) = active.last() {
            spans.push(SyntaxSpan {
                range: start as usize..position as usize,
                color: capture_color(highlight.idx()),
            });
        }
    }
    spans
}
