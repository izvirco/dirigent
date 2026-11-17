use super::{Env, images, truncate};
use crate::tool::{Call, Content, Tool, ToolOutput, ToolSpec};
use anyhow::{Context, Result, anyhow, bail};
use async_trait::async_trait;
use serde::Deserialize;
use serde_json::{Value, json};
use std::collections::BTreeMap;
use std::io::{BufRead, Read as _};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, Weak};
use unicode_normalization::UnicodeNormalization;

fn resolve(path: &str, cwd: &Path) -> PathBuf {
    let spaces = ['\u{00A0}', '\u{202F}', '\u{205F}', '\u{3000}'];
    let path: String = path.chars().map(|c| if spaces.contains(&c) || ('\u{2000}'..='\u{200A}').contains(&c) { ' ' } else { c }).collect();
    let path = path.strip_prefix('@').unwrap_or(&path);
    let path = from_shell(path);
    let home = std::env::home_dir().unwrap_or_default();
    if path == "~" {
        return home;
    }
    match path.strip_prefix("~/").or_else(|| if cfg!(windows) { path.strip_prefix("~\\") } else { None }) {
        Some(rest) => home.join(rest),
        None => cwd.join(&*path),
    }
}

/// On Windows the model's shell is Git Bash, whose `/c/Users/...` is `C:\Users\...` to
/// everything else (MSYS, Cygwin and WSL spell it `/mnt/c`, `/cygdrive/c`).
fn from_shell(path: &str) -> std::borrow::Cow<'_, str> {
    if !cfg!(windows) || !path.starts_with('/') || path.starts_with("//") || path.contains('\\') {
        return path.into();
    }
    let rest = path.strip_prefix("/mnt/").or_else(|| path.strip_prefix("/cygdrive/")).unwrap_or(&path[1..]);
    let (drive, tail) = rest.split_once('/').unwrap_or((rest, ""));
    match drive.chars().next() {
        Some(letter) if drive.len() == 1 && letter.is_ascii_alphabetic() => {
            format!("{}:\\{}", letter.to_ascii_uppercase(), tail.replace('/', "\\")).into()
        }
        _ => path.into(),
    }
}

/// `resolve` for a file to read: when there is none by that name, the names macOS gives
/// files that a model types differently are tried (a narrow space before `AM` / `PM` and a
/// curly apostrophe in screenshot names, decomposed accents).
fn resolve_existing(path: &str, cwd: &Path) -> PathBuf {
    let resolved = resolve(path, cwd);
    if resolved.exists() {
        return resolved;
    }
    let text = resolved.to_string_lossy().into_owned();
    let narrow = text.replace(" AM.", "\u{202F}AM.").replace(" PM.", "\u{202F}PM.");
    let decomposed: String = text.nfd().collect();
    let variants = [narrow, decomposed.clone(), text.replace('\'', "\u{2019}"), decomposed.replace('\'', "\u{2019}")];
    variants.into_iter().map(PathBuf::from).find(|p| p.exists()).unwrap_or(resolved)
}

// MUST KILL: rename: `turn` → `lock_canonical_file` (one change at a time per file, across threads and path spellings)
async fn turn(path: &Path) -> tokio::sync::OwnedMutexGuard<()> {
    static TURNS: Mutex<BTreeMap<PathBuf, Weak<tokio::sync::Mutex<()>>>> = Mutex::new(BTreeMap::new());
    let file = match tokio::fs::canonicalize(path).await {
        Ok(file) => file,
        Err(_) => match (path.parent(), path.file_name()) {
            (Some(dir), Some(name)) => tokio::fs::canonicalize(dir).await.map_or_else(|_| path.to_owned(), |dir| dir.join(name)),
            _ => path.to_owned(),
        },
    };
    let turn = {
        let mut turns = TURNS.lock().unwrap();
        turns.retain(|_, turn| turn.strong_count() > 0);
        let waiting = turns.get(&file).and_then(Weak::upgrade);
        waiting.unwrap_or_else(|| {
            let turn = Arc::default();
            turns.insert(file, Arc::downgrade(&turn));
            turn
        })
    };
    turn.lock_owned().await
}

pub struct Read {
    env: Arc<Env>,
}

impl Read {
    pub fn new(env: Arc<Env>) -> Self {
        Self { env }
    }
}

#[derive(Deserialize)]
struct ReadInput {
    path: String,
    offset: Option<f64>,
    limit: Option<f64>,
}

#[async_trait]
impl Tool for Read {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            snippet: Some("Read file contents".into()),
            guidelines: vec!["Use read to examine files instead of cat or sed.".into()],
            returns: Some("string | { type: \"image\"; data: string; mimeType: string; note: string }".into()),
            ..ToolSpec::new(
                "read",
                &format!(
                    "Read the contents of a file. Supports text files and images (jpg, png, gif, webp, bmp). Images are sent as \
                     attachments. For text files, output is truncated to {} lines or {}KB (whichever is hit first). Use offset/limit \
                     for large files. When you need the full file, continue with offset until complete.",
                    truncate::MAX_LINES,
                    truncate::MAX_BYTES / 1024
                ),
                json!({
                    "type": "object",
                    "properties": {
                        "path": { "type": "string", "description": "Path to the file to read (relative or absolute)" },
                        "offset": { "type": "number", "description": "Line number to start reading from (1-indexed)" },
                        "limit": { "type": "number", "description": "Maximum number of lines to read" },
                    },
                    "required": ["path"],
                }),
            )
        }
    }

    async fn call(&self, input: Value, _call: &Call) -> Result<ToolOutput> {
        let input: ReadInput = serde_json::from_value(input)?;
        let cwd = self.env.cwd.clone();
        tokio::task::spawn_blocking(move || read(&input, &cwd)).await?
    }
}

fn read(input: &ReadInput, cwd: &Path) -> Result<ToolOutput> {
    let path = resolve_existing(&input.path, cwd);
    let mut file = std::fs::File::open(&path).with_context(|| format!("{}", path.display()))?;
    let mut head = vec![];
    file.by_ref().take(4100).read_to_end(&mut head)?;
    if let Some(mime) = images::sniff(&head) {
        file.read_to_end(&mut head)?;
        return Ok(match images::process(&head, mime) {
            Ok(image) => {
                let note = std::iter::once(format!("Read image file [{}]", image.mime)).chain(image.notes).collect::<Vec<_>>().join("\n");
                ToolOutput {
                    content: vec![Content::Text(note), Content::Image { mime: image.mime, data: image.data }],
                    ..Default::default()
                }
            }
            Err(why) => ToolOutput::text(format!("Read image file [{mime}]\n{why}")),
        });
    }
    drop(file);
    let start = input.offset.map_or(0, |o| (o.max(1.) as usize) - 1);
    let limit = input.limit.map(|l| l.max(0.) as usize);
    let lines = Lines::select(&path, start, limit)?;
    if start >= lines.total {
        bail!("Offset {} is beyond end of file ({} lines total)", start + 1, lines.total);
    }
    let cut = truncate::head(&lines.kept.join("\n"));
    let limit_bytes = truncate::size(truncate::MAX_BYTES);
    let text = if cut.first_line_too_long {
        format!(
            "[Line {} is {}, exceeds {limit_bytes} limit. Use bash: sed -n '{}p' {} | head -c {}]",
            start + 1,
            truncate::size(lines.first_bytes),
            start + 1,
            input.path,
            truncate::MAX_BYTES
        )
    } else if let Some(by) = cut.by {
        let (last, total) = (start + cut.lines, lines.total);
        let limit = if by == truncate::By::Bytes { format!(" ({limit_bytes} limit)") } else { String::new() };
        format!("{}\n\n[Showing lines {}-{last} of {total}{limit}. Use offset={} to continue.]", cut.text, start + 1, last + 1)
    } else {
        match limit.map(|l| start + l).filter(|end| *end < lines.total) {
            Some(end) => format!("{}\n\n[{} more lines in file. Use offset={} to continue.]", cut.text, lines.total - end, end + 1),
            None => cut.text,
        }
    };
    Ok(ToolOutput::text(text))
}

struct Lines {
    // MUST KILL: rename: `total` → `editor_line_count` (a trailing newline counts a last, empty line, unlike truncate::lines)
    total: usize,
    kept: Vec<String>,
    first_bytes: usize,
}

impl Lines {
    fn select(path: &Path, start: usize, limit: Option<usize>) -> Result<Self> {
        let end = limit.map_or(usize::MAX, |l| start.saturating_add(l));
        let mut reader = std::io::BufReader::new(std::fs::File::open(path)?);
        let mut out = Lines { total: 0, kept: vec![], first_bytes: 0 };
        let (mut line, mut current, mut current_bytes, mut kept_bytes) = (0usize, vec![], 0usize, 0usize);
        let mut finish = |line: usize, current: &mut Vec<u8>, current_bytes: &mut usize, out: &mut Lines| {
            if line >= start && line < end {
                if line == start {
                    out.first_bytes = *current_bytes;
                }
                if out.kept.len() <= truncate::MAX_LINES && kept_bytes <= truncate::MAX_BYTES {
                    kept_bytes += current.len() + 1;
                    out.kept.push(String::from_utf8_lossy(current).into_owned());
                }
            }
            current.clear();
            *current_bytes = 0;
        };
        loop {
            let chunk = reader.fill_buf()?;
            if chunk.is_empty() {
                break;
            }
            let len = chunk.len();
            for piece in chunk.split_inclusive(|b| *b == b'\n') {
                let (text, ended) = match piece.split_last() {
                    Some((b'\n', text)) => (text, true),
                    _ => (piece, false),
                };
                if line >= start && line < end && current.len() <= truncate::MAX_BYTES {
                    current.extend_from_slice(text);
                }
                current_bytes += text.len();
                if ended {
                    finish(line, &mut current, &mut current_bytes, &mut out);
                    line += 1;
                }
            }
            reader.consume(len);
        }
        finish(line, &mut current, &mut current_bytes, &mut out);
        out.total = line + 1;
        Ok(out)
    }
}

pub struct Write {
    env: Arc<Env>,
}

impl Write {
    pub fn new(env: Arc<Env>) -> Self {
        Self { env }
    }
}

#[derive(Deserialize)]
struct WriteInput {
    path: String,
    content: String,
}

#[async_trait]
impl Tool for Write {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            snippet: Some("Create or overwrite files".into()),
            guidelines: vec!["Use write only for new files or complete rewrites.".into()],
            ..ToolSpec::new(
                "write",
                "Write content to a file. Creates the file if it doesn't exist, overwrites if it does. Automatically creates parent directories.",
                json!({
                    "type": "object",
                    "properties": {
                        "path": { "type": "string", "description": "Path to the file to write (relative or absolute)" },
                        "content": { "type": "string", "description": "Content to write to the file" },
                    },
                    "required": ["path", "content"],
                }),
            )
        }
    }

    async fn call(&self, input: Value, _call: &Call) -> Result<ToolOutput> {
        let input: WriteInput = serde_json::from_value(input)?;
        let path = resolve(&input.path, &self.env.cwd);
        if let Some(dir) = path.parent() {
            tokio::fs::create_dir_all(dir).await?;
        }
        let _one_at_a_time = turn(&path).await;
        tokio::fs::write(&path, input.content).await.with_context(|| format!("{}", path.display()))?;
        Ok(ToolOutput::text(format!("Successfully wrote to {}", input.path)))
    }
}

pub struct Edit {
    env: Arc<Env>,
}

impl Edit {
    pub fn new(env: Arc<Env>) -> Self {
        Self { env }
    }
}

#[derive(Deserialize)]
struct EditInput {
    path: String,
    edits: Vec<Replace>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Replace {
    old_text: String,
    new_text: String,
}

#[async_trait]
impl Tool for Edit {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            snippet: Some("Make precise file edits with exact text replacement, including multiple disjoint edits in one call".into()),
            guidelines: vec![
                "Use edit for targeted changes. Batch changes to one file in a single call.".into(),
                "Each edits[].oldText must exactly and uniquely match the original file. Keep matches short and non-overlapping; \
                 merge nearby changes without including large unchanged regions."
                    .into(),
            ],
            ..ToolSpec::new(
                "edit",
                "Edit a single file using exact text replacement. Every edits[].oldText must match a unique, non-overlapping region of \
                 the original file. If two changes affect the same block or nearby lines, merge them into one edit instead of emitting \
                 overlapping edits. Do not include large unchanged regions just to connect distant changes.",
                json!({
                    "type": "object",
                    "properties": {
                        "path": { "type": "string", "description": "Path to the file to edit (relative or absolute)" },
                        "edits": {
                            "type": "array",
                            "description": "One or more targeted replacements. Each edit is matched against the original file, not \
                                            incrementally. Do not include overlapping or nested edits. If two changes touch the same \
                                            block or nearby lines, merge them into one edit instead.",
                            "items": {
                                "type": "object",
                                "properties": {
                                    "oldText": {
                                        "type": "string",
                                        "description": "Exact text for one targeted replacement. It must be unique in the original file \
                                                        and must not overlap with any other edits[].oldText in the same call.",
                                    },
                                    "newText": { "type": "string", "description": "Replacement text for this targeted edit." },
                                },
                                "required": ["oldText", "newText"],
                            },
                        },
                    },
                    "required": ["path", "edits"],
                }),
            )
        }
    }

    async fn call(&self, input: Value, _call: &Call) -> Result<ToolOutput> {
        let input: EditInput = serde_json::from_value(lenient(input))?;
        if input.edits.is_empty() {
            bail!("Edit tool input is invalid. edits must contain at least one replacement.");
        }
        let path = resolve(&input.path, &self.env.cwd);
        let _one_at_a_time = turn(&path).await;
        let raw = tokio::fs::read(&path).await.map_err(|e| anyhow!("Could not edit file: {}. {e}.", input.path))?;
        let Ok(raw) = String::from_utf8(raw) else {
            bail!("Could not edit file: {}. It is not UTF-8 text: use bash to change it.", input.path);
        };
        // The model doesn't write the invisible byte order mark: matched without, kept in the file.
        let (bom, content) = match raw.strip_prefix('\u{FEFF}') {
            Some(rest) => ("\u{FEFF}", rest),
            None => ("", &*raw),
        };
        let unix = to_lf(content);
        let (new, replacements) = apply(&unix, &input.edits, &input.path)?;
        let new = if content == unix { new } else { splice(content, &replacements) };
        tokio::fs::write(&path, format!("{bom}{new}")).await.with_context(|| format!("{}", path.display()))?;
        Ok(ToolOutput::text(format!("Successfully replaced {} block(s) in {}.", input.edits.len(), input.path)))
    }
}

/// The shapes models get `edits` wrong in, put right: the array as a JSON string, one edit
/// in place of the array, and `oldText` / `newText` next to `path` (an older tool's input).
fn lenient(mut input: Value) -> Value {
    let Some(args) = input.as_object_mut() else { return input };
    let is_edit = |v: &Value| v.get("oldText").is_some_and(Value::is_string) && v.get("newText").is_some_and(Value::is_string);
    let mut edits = match args.remove("edits") {
        Some(Value::String(s)) => match serde_json::from_str(&s) {
            Ok(Value::Array(edits)) => edits,
            Ok(one) if is_edit(&one) => vec![one],
            _ => vec![Value::String(s)],
        },
        Some(Value::Array(edits)) => edits,
        Some(one) if is_edit(&one) => vec![one],
        Some(other) => vec![other],
        None => vec![],
    };
    if is_edit(&Value::Object(args.clone())) {
        edits.push(json!({ "oldText": args.remove("oldText"), "newText": args.remove("newText") }));
    }
    args.insert("edits".into(), Value::Array(edits));
    input
}

fn to_lf(text: &str) -> String {
    text.replace("\r\n", "\n").replace('\r', "\n")
}

#[derive(Debug, Clone, PartialEq)]
struct Replacement {
    at: usize,
    len: usize,
    new: String,
}

fn loosen(text: &str) -> String {
    let text: String = text.nfkc().collect();
    let lines: Vec<&str> = text.split('\n').map(str::trim_end).collect();
    lines
        .join("\n")
        .chars()
        .map(|c| match c {
            '\u{2018}' | '\u{2019}' | '\u{201A}' | '\u{201B}' => '\'',
            '\u{201C}' | '\u{201D}' | '\u{201E}' | '\u{201F}' => '"',
            '\u{2010}'..='\u{2015}' | '\u{2212}' => '-',
            '\u{00A0}' | '\u{2002}'..='\u{200A}' | '\u{202F}' | '\u{205F}' | '\u{3000}' => ' ',
            c => c,
        })
        .collect()
}

fn apply(content: &str, edits: &[Replace], path: &str) -> Result<(String, Vec<Replacement>)> {
    let one = edits.len() == 1;
    let edits: Vec<(String, String)> = edits.iter().map(|e| (to_lf(&e.old_text), to_lf(&e.new_text))).collect();
    for (i, (old, _)) in edits.iter().enumerate() {
        if old.is_empty() {
            bail!("{} must not be empty in {path}.", if one { "oldText".into() } else { format!("edits[{i}].oldText") });
        }
    }
    let loose = edits.iter().any(|(old, _)| !content.contains(old.as_str()));
    let base = if loose { loosen(content) } else { content.to_owned() };
    let mut matched: Vec<(usize, Replacement)> = vec![];
    for (i, (old, new)) in edits.iter().enumerate() {
        let (at, len) = match base.find(old.as_str()) {
            Some(at) => (at, old.len()),
            None => {
                let old = loosen(old);
                let Some(at) = base.find(&old) else {
                    bail!(match one {
                        true => format!(
                            "Could not find the exact text in {path}. The old text must match exactly including all whitespace and newlines."
                        ),
                        false => format!(
                            "Could not find edits[{i}] in {path}. The oldText must match exactly including all whitespace and newlines."
                        ),
                    });
                };
                (at, old.len())
            }
        };
        let occurrences = loosen(&base).matches(&loosen(old)).count();
        if occurrences > 1 {
            bail!(match one {
                true => format!(
                    "Found {occurrences} occurrences of the text in {path}. The text must be unique. Please provide more context to make it unique."
                ),
                false => format!(
                    "Found {occurrences} occurrences of edits[{i}] in {path}. Each oldText must be unique. Please provide more context to make it unique."
                ),
            });
        }
        matched.push((i, Replacement { at, len, new: new.clone() }));
    }
    matched.sort_by_key(|(_, r)| r.at);
    for pair in matched.windows(2) {
        if pair[0].1.at + pair[0].1.len > pair[1].1.at {
            bail!("edits[{}] and edits[{}] overlap in {path}. Merge them into one edit or target disjoint regions.", pair[0].0, pair[1].0);
        }
    }
    let matched: Vec<Replacement> = matched.into_iter().map(|(_, r)| r).collect();
    let replacements = if loose { widen(content, &base, &matched)? } else { matched };
    let new = replace(content, &replacements, 0);
    if new == content {
        bail!("No changes made to {path}. The replacement{} produced identical content.", if one { "" } else { "s" });
    }
    Ok((new, replacements))
}

fn replace(text: &str, replacements: &[Replacement], offset: usize) -> String {
    let mut out = String::with_capacity(text.len());
    let mut from = 0;
    for r in replacements {
        out.push_str(&text[from..r.at - offset]);
        out.push_str(&r.new);
        from = r.at - offset + r.len;
    }
    out.push_str(&text[from..]);
    out
}

fn line_spans(text: &str) -> Vec<(usize, usize)> {
    let mut at = 0;
    text.split_inclusive('\n')
        .map(|line| {
            at += line.len();
            (at - line.len(), at)
        })
        .collect()
}

fn widen(original: &str, loose: &str, replacements: &[Replacement]) -> Result<Vec<Replacement>> {
    let (original_lines, loose_lines) = (line_spans(original), line_spans(loose));
    if original_lines.len() != loose_lines.len() {
        bail!("Cannot preserve unchanged lines because the base content has a different line count.");
    }
    // MUST KILL: type: a LineGroup struct (first line, end line, replacements) instead of this tuple
    let mut groups: Vec<(usize, usize, Vec<Replacement>)> = vec![];
    for r in replacements {
        let outside = || anyhow!("Replacement range is outside the base content.");
        let first = loose_lines.iter().position(|(start, end)| r.at >= *start && r.at < *end).ok_or_else(outside)?;
        let last = (first..loose_lines.len()).find(|i| loose_lines[*i].1 >= r.at + r.len).ok_or_else(outside)?;
        match groups.last_mut() {
            Some((_, end, group)) if first < *end => {
                *end = (*end).max(last + 1);
                group.push(r.clone());
            }
            _ => groups.push((first, last + 1, vec![r.clone()])),
        }
    }
    Ok(groups
        .into_iter()
        .map(|(first, end, group)| {
            let (from, to) = (loose_lines[first].0, loose_lines[end - 1].1);
            let at = original_lines[first].0;
            Replacement { at, len: original_lines[end - 1].1 - at, new: replace(&loose[from..to], &group, from) }
        })
        .collect())
}

fn splice(original: &str, replacements: &[Replacement]) -> String {
    let crlfs: Vec<usize> = original.match_indices("\r\n").map(|(i, _)| i).collect();
    let to_original = |offset: usize| offset + crlfs.iter().enumerate().take_while(|(n, at)| **at - n < offset).count();
    let mut out = String::with_capacity(original.len());
    // MUST KILL: extract: a join fn named for keeping a CR before a seam and an LF after it two line breaks
    let mut push = |part: &str| {
        if out.ends_with('\r') && part.starts_with('\n') {
            out.push('\n');
        }
        out.push_str(part);
    };
    let mut from = 0;
    for r in replacements {
        let start = to_original(r.at);
        push(&original[from..start]);
        match line_ending(original, start) {
            "\n" => push(&r.new),
            ending => push(&r.new.replace('\n', ending)),
        }
        from = to_original(r.at + r.len);
    }
    push(&original[from..]);
    out
}

fn line_ending(text: &str, offset: usize) -> &'static str {
    let lf = text[offset..].find('\n').map(|i| offset + i).or_else(|| text[..offset].rfind('\n'));
    match lf {
        Some(lf) if text[..lf].ends_with('\r') => "\r\n",
        Some(_) => "\n",
        None if text.contains('\r') => "\r",
        None => "\n",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio_util::sync::CancellationToken;

    fn env(dir: &Path) -> Arc<Env> {
        Arc::new(Env::test(dir))
    }

    fn call() -> Call {
        Call { id: "c".into(), cancel: CancellationToken::new(), script: false }
    }

    fn edit(old: &str, new: &str) -> Replace {
        Replace { old_text: old.into(), new_text: new.into() }
    }

    #[tokio::test]
    async fn reads_a_window_of_lines_and_says_where_to_go_on() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("f.txt"), "one\ntwo\nthree\nfour\n").unwrap();
        let read = Read::new(env(dir.path()));
        let text = |input: Value| async { read.call(input, &call()).await.map(|o| o.as_text()).map_err(|e| e.to_string()) };
        assert_eq!(text(json!({"path": "f.txt"})).await.unwrap(), "one\ntwo\nthree\nfour\n");
        assert_eq!(
            text(json!({"path": "f.txt", "offset": 2, "limit": 2})).await.unwrap(),
            "two\nthree\n\n[2 more lines in file. Use offset=4 to continue.]"
        );
        assert_eq!(text(json!({"path": "f.txt", "offset": 9})).await.unwrap_err(), "Offset 9 is beyond end of file (5 lines total)");
        std::fs::write(dir.path().join("long.txt"), "x\n".repeat(truncate::MAX_LINES + 10)).unwrap();
        let long = text(json!({"path": "long.txt", "offset": 3})).await.unwrap();
        assert!(long.ends_with("[Showing lines 3-2002 of 2011. Use offset=2003 to continue.]"), "{}", &long[long.len() - 80..]);
    }

    #[tokio::test]
    async fn edits_match_the_original_and_keep_its_line_endings() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("f.txt");
        std::fs::write(&path, "\u{FEFF}a = 1\r\nb = 2\r\nc = 3\r\n").unwrap();
        let tool = Edit::new(env(dir.path()));
        let input = json!({"path": "f.txt", "edits": {"oldText": "b = 2", "newText": "b = 20\nbb = 21"}});
        tool.call(input, &call()).await.unwrap();
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "\u{FEFF}a = 1\r\nb = 20\r\nbb = 21\r\nc = 3\r\n");
        let missing = tool.call(json!({"path": "f.txt", "edits": [{"oldText": "nope", "newText": "x"}]}), &call()).await;
        assert!(missing.unwrap_err().to_string().starts_with("Could not find the exact text in f.txt."));
    }

    #[tokio::test]
    async fn a_file_that_is_not_utf8_is_refused_and_left_as_it_is() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("latin1.txt");
        std::fs::write(&path, b"caf\xe9 = 1\n").unwrap();
        let input = json!({"path": "latin1.txt", "edits": [{"oldText": "= 1", "newText": "= 2"}]});
        let refused = Edit::new(env(dir.path())).call(input, &call()).await.unwrap_err();
        assert_eq!(refused.to_string(), "Could not edit file: latin1.txt. It is not UTF-8 text: use bash to change it.");
        assert_eq!(std::fs::read(&path).unwrap(), b"caf\xe9 = 1\n");
    }

    #[tokio::test]
    async fn edits_of_one_file_take_turns_whoever_makes_them() {
        let dir = tempfile::tempdir().unwrap();
        let dir = dir.path();
        std::fs::create_dir(dir.join("sub")).unwrap();
        let lines = |word: &str| (0..8).map(|n| format!("{word} {n}")).collect::<Vec<_>>().join("\n");
        std::fs::write(dir.join("f.txt"), lines("line")).unwrap();
        let edits = (0..8).map(|n| async move {
            let path = if n % 2 == 0 { "f.txt" } else { "sub/../f.txt" };
            let input = json!({"path": path, "edits": [{"oldText": format!("line {n}"), "newText": format!("edited {n}")}]});
            Edit::new(env(dir)).call(input, &call()).await.unwrap();
        });
        futures_util::future::join_all(edits).await;
        assert_eq!(std::fs::read_to_string(dir.join("f.txt")).unwrap(), lines("edited"));
    }

    #[test]
    fn edits_are_unique_disjoint_and_forgiving_of_typography() {
        let content = "let a = “x”;   \nlet b = 1;\nlet c = 1;\n";
        let (new, _) = apply(content, &[edit("let a = \"x\";\n", "let a = \"y\";\n")], "f").unwrap();
        assert_eq!(new, "let a = \"y\";\nlet b = 1;\nlet c = 1;\n");
        let (new, _) = apply(content, &[edit("b = 1", "b = 2"), edit("c = 1", "c = 3")], "f").unwrap();
        assert_eq!(new, "let a = “x”;   \nlet b = 2;\nlet c = 3;\n");
        let err = |edits: &[Replace]| apply(content, edits, "f").unwrap_err().to_string();
        assert!(err(&[edit(" = 1;", "")]).starts_with("Found 2 occurrences of the text in f."));
        assert!(err(&[edit("let b = 1;", "x"), edit("b = 1", "y")]).starts_with("edits[0] and edits[1] overlap in f."));
        assert!(err(&[edit("b = 1", "b = 1")]).starts_with("No changes made to f."));
        assert_eq!(err(&[edit("", "x")]), "oldText must not be empty in f.");
    }
}
