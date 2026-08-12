mod highlight;
mod repository;

pub(crate) use repository::{
    ActiveTurnDiff, begin_turn, finish_turn, preview_turn, prompt_excerpt,
};

use highlight::highlight_text;
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
