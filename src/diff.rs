use std::{
    collections::{BTreeMap, HashMap, HashSet},
    fs::{self, File},
    io::Read as _,
    ops::Range,
    path::Path,
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use ignore::WalkBuilder;
use ropey::{Rope, RopeSlice};
use serde::{Deserialize, Serialize};
use similar::{DiffTag, TextDiff};
use tree_house::{
    Language, LanguageConfig, LanguageLoader, Syntax,
    highlighter::{Highlight, HighlightEvent, Highlighter},
    tree_sitter::Grammar,
};

use crate::theme::{blue, code_text, green, muted, orange, purple, yellow};

const MAX_TEXT_FILE_BYTES: u64 = 2 * 1024 * 1024;
const MAX_SNAPSHOT_BYTES: u64 = 48 * 1024 * 1024;
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
pub(crate) fn combine_turn_diffs(turns: &[TurnDiff]) -> Option<TurnDiff> {
    let last = turns.last()?;
    let mut combined = BTreeMap::<String, CombinedFile>::new();
    for turn in turns {
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
            .map_or(last.started_at, |turn| turn.started_at),
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

#[derive(Debug)]
pub(crate) struct ActiveTurnDiff {
    pub(crate) id: u64,
    pub(crate) prompt: String,
    pub(crate) started_at: u64,
    pub(crate) status_override: Option<TurnDiffStatus>,
    baseline: Result<WorkspaceSnapshot, String>,
}

#[derive(Debug)]
struct SnapshotFile {
    hash: blake3::Hash,
    bytes: Option<Vec<u8>>,
    len: u64,
    mode: u32,
    binary: bool,
}

#[derive(Debug, Default)]
struct WorkspaceSnapshot {
    files: BTreeMap<String, SnapshotFile>,
}

pub(crate) fn begin_turn(id: u64, prompt: &str, root: &Path) -> ActiveTurnDiff {
    ActiveTurnDiff {
        id,
        prompt: prompt_excerpt(prompt),
        started_at: unix_timestamp(),
        status_override: None,
        baseline: capture_workspace(root),
    }
}

pub(crate) fn finish_turn(active: ActiveTurnDiff, root: &Path, status: TurnDiffStatus) -> TurnDiff {
    let endpoint = capture_workspace(root);
    let mut turn = match (active.baseline, endpoint) {
        (Ok(old), Ok(new)) => build_turn(
            active.id,
            active.prompt,
            active.started_at,
            status,
            old,
            new,
        ),
        (Err(error), _) | (_, Err(error)) => TurnDiff {
            id: active.id,
            prompt: active.prompt,
            started_at: active.started_at,
            finished_at: unix_timestamp(),
            status: TurnDiffStatus::Unavailable,
            files: Vec::new(),
            additions: 0,
            deletions: 0,
            error: Some(error),
        },
    };
    turn.refresh_highlights();
    turn
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

fn ignored_directory(path: &Path) -> bool {
    path.file_name()
        .and_then(|name| name.to_str())
        .is_some_and(|name| {
            matches!(
                name,
                ".git"
                    | ".jj"
                    | "target"
                    | "node_modules"
                    | ".direnv"
                    | ".cache"
                    | "dist"
                    | "build"
            )
        })
}

fn capture_workspace(root: &Path) -> Result<WorkspaceSnapshot, String> {
    if !root.is_dir() {
        return Err(format!("diff workspace is missing: {}", root.display()));
    }
    let mut builder = WalkBuilder::new(root);
    builder
        .hidden(false)
        .parents(true)
        .git_ignore(true)
        .git_global(true)
        .git_exclude(true)
        .filter_entry(|entry| entry.depth() == 0 || !ignored_directory(entry.path()));

    let mut snapshot = WorkspaceSnapshot::default();
    let mut stored_bytes = 0_u64;
    for entry in builder.build() {
        let entry = entry.map_err(|error| format!("could not walk {}: {error}", root.display()))?;
        let Some(file_type) = entry.file_type() else {
            continue;
        };
        if !file_type.is_file() && !file_type.is_symlink() {
            continue;
        }
        let path = entry.path();
        let relative = path
            .strip_prefix(root)
            .unwrap_or(path)
            .to_string_lossy()
            .replace('\\', "/");
        let metadata = fs::symlink_metadata(path)
            .map_err(|error| format!("could not inspect {}: {error}", path.display()))?;
        let mode = file_mode(&metadata);
        if file_type.is_symlink() {
            let target = fs::read_link(path)
                .map_err(|error| format!("could not read link {}: {error}", path.display()))?;
            let bytes = target.to_string_lossy().into_owned().into_bytes();
            let hash = blake3::hash(&bytes);
            snapshot.files.insert(
                relative,
                SnapshotFile {
                    hash,
                    len: bytes.len() as u64,
                    bytes: Some(bytes),
                    mode,
                    binary: false,
                },
            );
            continue;
        }
        let len = metadata.len();
        let mut file = File::open(path)
            .map_err(|error| format!("could not read {}: {error}", path.display()))?;
        let mut hasher = blake3::Hasher::new();
        let can_store =
            len <= MAX_TEXT_FILE_BYTES && stored_bytes.saturating_add(len) <= MAX_SNAPSHOT_BYTES;
        let mut stored = can_store.then(Vec::new);
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
            stored_bytes = stored_bytes.saturating_add(len);
        }
        snapshot.files.insert(
            relative,
            SnapshotFile {
                hash: hasher.finalize(),
                bytes: stored,
                len,
                mode,
                binary,
            },
        );
    }
    Ok(snapshot)
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
                            #[cfg(test)]
                            eprintln!("{} query: {error}", names[0]);
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

const CAPTURE_KEYWORD: u32 = 0;
const CAPTURE_STRING: u32 = 1;
const CAPTURE_COMMENT: u32 = 2;
const CAPTURE_FUNCTION: u32 = 3;
const CAPTURE_TYPE: u32 = 4;
const CAPTURE_CONSTANT: u32 = 5;
const CAPTURE_VARIABLE: u32 = 6;
const CAPTURE_PUNCTUATION: u32 = 7;

fn highlight_for_capture(capture: &str) -> Option<Highlight> {
    let root = capture.split('.').next().unwrap_or(capture);
    let index = match root {
        "keyword" | "operator" | "attribute" | "tag" => CAPTURE_KEYWORD,
        "string" => CAPTURE_STRING,
        "comment" => CAPTURE_COMMENT,
        "function" | "constructor" | "method" => CAPTURE_FUNCTION,
        "type" | "namespace" | "module" => CAPTURE_TYPE,
        "constant" | "number" | "boolean" | "character" => CAPTURE_CONSTANT,
        "variable" | "property" | "label" => CAPTURE_VARIABLE,
        "punctuation" => CAPTURE_PUNCTUATION,
        _ => return None,
    };
    Some(Highlight::new(index))
}

fn capture_color(index: usize) -> u32 {
    match index as u32 {
        CAPTURE_KEYWORD => purple(),
        CAPTURE_STRING => green(),
        CAPTURE_COMMENT => muted(),
        CAPTURE_FUNCTION => blue(),
        CAPTURE_TYPE => yellow(),
        CAPTURE_CONSTANT => orange(),
        CAPTURE_VARIABLE => code_text(),
        CAPTURE_PUNCTUATION => code_text(),
        _ => code_text(),
    }
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn creates_structured_replacement_hunks() {
        let (hunks, additions, deletions) = diff_text("one\nold\nthree\n", "one\nnew\nthree\n");
        assert_eq!((additions, deletions), (1, 1));
        assert_eq!(hunks.len(), 1);
        assert!(
            hunks[0]
                .rows
                .iter()
                .any(|row| row.kind == DiffRowKind::Replaced)
        );
    }

    #[test]
    fn combines_turns_into_the_net_change() {
        let first = build_turn(
            1,
            "first".into(),
            1,
            TurnDiffStatus::Completed,
            WorkspaceSnapshot {
                files: BTreeMap::from([("file.txt".into(), snapshot_file("one\n"))]),
            },
            WorkspaceSnapshot {
                files: BTreeMap::from([("file.txt".into(), snapshot_file("two\n"))]),
            },
        );
        let second = build_turn(
            2,
            "second".into(),
            2,
            TurnDiffStatus::Completed,
            WorkspaceSnapshot {
                files: BTreeMap::from([("file.txt".into(), snapshot_file("two\n"))]),
            },
            WorkspaceSnapshot {
                files: BTreeMap::from([("file.txt".into(), snapshot_file("three\n"))]),
            },
        );

        let combined = combine_turn_diffs(&[first, second]).unwrap();
        assert_eq!(combined.id, 2);
        assert_eq!(combined.files.len(), 1);
        assert_eq!(combined.files[0].old_text.as_deref(), Some("one\n"));
        assert_eq!(combined.files[0].new_text.as_deref(), Some("three\n"));
        assert_eq!((combined.additions, combined.deletions), (1, 1));
    }

    #[test]
    fn cumulative_diff_omits_files_added_then_deleted() {
        let mut added = build_file_diff(
            "temporary.txt".into(),
            None,
            FileDiffKind::Added,
            None,
            Some(snapshot_file("temporary\n")),
        );
        added.refresh_highlights();
        let mut deleted = build_file_diff(
            "temporary.txt".into(),
            None,
            FileDiffKind::Deleted,
            Some(snapshot_file("temporary\n")),
            None,
        );
        deleted.refresh_highlights();
        let turn = |id, files| TurnDiff {
            id,
            prompt: format!("turn {id}"),
            started_at: id,
            finished_at: id,
            status: TurnDiffStatus::Completed,
            additions: 1,
            deletions: 1,
            files,
            error: None,
        };

        let combined = combine_turn_diffs(&[turn(1, vec![added]), turn(2, vec![deleted])]).unwrap();
        assert!(combined.files.is_empty());
    }

    fn snapshot_file(text: &str) -> SnapshotFile {
        SnapshotFile {
            hash: blake3::hash(text.as_bytes()),
            bytes: Some(text.as_bytes().to_vec()),
            len: text.len() as u64,
            mode: 0o100644,
            binary: false,
        }
    }

    #[test]
    fn highlights_rust_with_tree_house() {
        let spans = highlight_text("src/main.rs", "fn main() { let answer = 42; }");
        assert!(!spans.is_empty());
    }

    #[test]
    fn loads_the_bundled_language_set() {
        let loader = HighlightLoader::new();
        for name in [
            "rust",
            "javascript",
            "typescript",
            "tsx",
            "json",
            "toml",
            "python",
            "go",
            "bash",
            "c",
            "cpp",
            "html",
            "css",
            "yaml",
            "markdown",
        ] {
            assert!(loader.names.contains_key(name), "missing {name} grammar");
        }
    }

    #[test]
    fn truncates_prompt_on_a_character_boundary() {
        let prompt = "é".repeat(100);
        assert!(prompt_excerpt(&prompt).ends_with('…'));
    }

    #[test]
    fn freezes_added_modified_deleted_and_renamed_files() {
        let root = std::env::temp_dir().join(format!(
            "dirigent-diff-{}-{}",
            std::process::id(),
            fastrand::u64(..)
        ));
        fs::create_dir_all(root.join("target")).unwrap();
        fs::write(root.join("modified.rs"), "fn old() {}\n").unwrap();
        fs::write(root.join("deleted.txt"), "gone\n").unwrap();
        fs::write(root.join("rename.txt"), "same\n").unwrap();
        fs::write(root.join("target/ignored.txt"), "before\n").unwrap();

        let active = begin_turn(1, "change files", &root);
        fs::write(root.join("modified.rs"), "fn new() {}\n").unwrap();
        fs::remove_file(root.join("deleted.txt")).unwrap();
        fs::rename(root.join("rename.txt"), root.join("renamed.txt")).unwrap();
        fs::write(root.join("added.txt"), "hello\n").unwrap();
        fs::write(root.join("target/ignored.txt"), "after\n").unwrap();

        let turn = finish_turn(active, &root, TurnDiffStatus::Completed);
        let kinds = turn
            .files
            .iter()
            .map(|file| (file.path.as_str(), file.kind))
            .collect::<HashMap<_, _>>();
        assert_eq!(kinds["modified.rs"], FileDiffKind::Modified);
        assert_eq!(kinds["deleted.txt"], FileDiffKind::Deleted);
        assert_eq!(kinds["renamed.txt"], FileDiffKind::Renamed);
        assert_eq!(kinds["added.txt"], FileDiffKind::Added);
        assert!(!kinds.contains_key("target/ignored.txt"));

        fs::remove_dir_all(root).unwrap();
    }
}
