use crate::tool::{Content, ToolOutput};
use base64::Engine;
use std::collections::HashMap;
use std::hash::{BuildHasher, Hasher};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::Duration;

const CHARS_PER_TOKEN: usize = 4;

#[derive(Debug, Clone, PartialEq)]
pub enum Item {
    Text(String),
    Console(String),
    Image { mime: String, data: String },
}

pub fn render(wall: Duration, items: Vec<Item>, error: Option<String>, max_tokens: u64, dir: &Path) -> ToolOutput {
    let is_error = error.is_some();
    let status = if is_error { "Script failed" } else { "Script completed" };
    let pieces = truncate(layout(items, error), max_tokens, dir);

    let mut out = Joined::default();
    out.text(&format!("{status}\nWall time {:.1} seconds\nOutput:\n", wall.as_secs_f64()));
    let mut saved: HashMap<&[u8], String> = HashMap::new();
    for piece in &pieces {
        match piece {
            Content::Text(text) => out.text(text),
            Content::Image { mime, data } => {
                out.text(saved.entry(data.as_slice()).or_insert_with(|| save_image(mime, data, dir)));
                out.0.push(piece.clone());
            }
        }
    }
    ToolOutput { content: out.0, is_error, value: None }
}

#[derive(Default)]
struct Joined(Vec<Content>);

impl Joined {
    fn text(&mut self, text: &str) {
        match self.0.last_mut() {
            Some(Content::Text(open)) => {
                if !open.is_empty() && !open.ends_with('\n') {
                    open.push('\n');
                }
                open.push_str(text);
            }
            // No piece of its own for nothing: providers refuse an empty text block.
            _ if text.is_empty() => {}
            _ => self.0.push(Content::Text(text.into())),
        }
    }
}

fn layout(items: Vec<Item>, error: Option<String>) -> Vec<Content> {
    let total = items.iter().filter(|item| matches!(item, Item::Text(_))).count();
    let mut out = Joined::default();
    let mut console = vec![];
    let mut index = 0;
    for item in items {
        match item {
            Item::Text(text) if total > 1 => {
                index += 1;
                out.text(&format!("==> text {index}/{total} <==\n{text}"));
            }
            Item::Text(text) => out.text(&text),
            Item::Console(line) => console.push(line),
            Item::Image { mime, data } => match BASE64.decode(&data) {
                Ok(data) => out.0.push(Content::Image { mime, data }),
                Err(error) => out.text(&format!("[Image ({mime}) could not be decoded: {error}]")),
            },
        }
    }
    if !console.is_empty() {
        out.text(&format!("<console_output>\n{}\n</console_output>", console.join("\n")));
    }
    if let Some(error) = error {
        out.text(&format!("Script error:\n{error}"));
    }
    out.0
}

fn truncate(pieces: Vec<Content>, max_tokens: u64, dir: &Path) -> Vec<Content> {
    let texts: Vec<&str> =
        pieces.iter().filter_map(|piece| if let Content::Text(text) = piece { Some(text.as_str()) } else { None }).collect();
    let combined = texts.join("\n");
    let budget = usize::try_from(max_tokens).unwrap_or(usize::MAX).saturating_mul(CHARS_PER_TOKEN);
    let chars = combined.chars().count();
    if texts.is_empty() || chars <= budget {
        return pieces;
    }
    let head_chars = budget / 2;
    let tail_chars = budget - head_chars;
    let at = |nth: usize| combined.char_indices().nth(nth).map_or(combined.len(), |(at, _)| at);
    let (head, tail) = (&combined[..at(head_chars)], &combined[at(chars - tail_chars)..]);
    let tokens = |chars: usize| chars.div_ceil(CHARS_PER_TOKEN);
    let saved = match write_file(dir, ".txt", combined.as_bytes()) {
        Ok(path) => format!("[Full output: {} (read with offset/limit)]", path.display()),
        Err(error) => format!("[Could not save the full output: {error}]"),
    };
    let text = format!(
        "Warning: truncated output (original token count: {})\nTotal output lines: {}\n\n{head}…{} tokens truncated…{tail}\n\n{saved}",
        tokens(chars),
        combined.split('\n').count(),
        tokens(chars - budget),
    );
    let images = pieces.into_iter().filter(|piece| matches!(piece, Content::Image { .. }));
    std::iter::once(Content::Text(text)).chain(images).collect()
}

fn save_image(mime: &str, data: &[u8], dir: &Path) -> String {
    let kind = format!("{mime}, {}", size(data.len()));
    let extension = match mime {
        "image/jpeg" => ".jpg",
        "image/gif" => ".gif",
        "image/webp" => ".webp",
        _ => ".png",
    };
    match write_file(dir, extension, data) {
        Ok(path) => format!("[Image saved to {} ({kind})]", path.display()),
        Err(error) => format!("[Image ({kind}) could not be saved: {error}]"),
    }
}

fn size(bytes: usize) -> String {
    match bytes {
        0..1024 => format!("{bytes}B"),
        1024..1_048_576 => format!("{:.1}KB", bytes as f64 / 1024.0),
        _ => format!("{:.1}MB", bytes as f64 / 1_048_576.0),
    }
}

fn write_file(dir: &Path, extension: &str, bytes: &[u8]) -> std::io::Result<PathBuf> {
    // std's source of randomness: a `RandomState`'s keys.
    let random = std::hash::RandomState::new().build_hasher().finish();
    let path = dir.join(format!("dirigent-codemode-{random:016x}{extension}"));
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    std::os::unix::fs::OpenOptionsExt::mode(&mut options, 0o600);
    options.open(&path)?.write_all(bytes)?;
    Ok(path)
}

const BASE64: base64::engine::GeneralPurpose = base64::engine::GeneralPurpose::new(
    &base64::alphabet::STANDARD,
    base64::engine::GeneralPurposeConfig::new()
        .with_decode_allow_trailing_bits(true)
        .with_decode_padding_mode(base64::engine::DecodePaddingMode::Indifferent),
);

#[cfg(test)]
mod tests {
    use super::*;

    fn text(items: Vec<Item>, error: Option<&str>, max_tokens: u64, dir: &Path) -> String {
        render(Duration::from_millis(1234), items, error.map(String::from), max_tokens, dir).as_text()
    }

    #[test]
    fn items_are_laid_out_in_order() {
        let dir = tempfile::tempdir().unwrap();
        let one = text(vec![Item::Text("only".into())], None, 100, dir.path());
        assert_eq!(one, "Script completed\nWall time 1.2 seconds\nOutput:\nonly");

        let items = vec![
            Item::Console("first line".into()),
            Item::Text("a\n".into()),
            Item::Text(String::new()),
            Item::Console("second".into()),
            Item::Text("c".into()),
        ];
        let many = text(items, Some("Error: boom"), 100, dir.path());
        assert_eq!(
            many,
            "Script failed\nWall time 1.2 seconds\nOutput:\n==> text 1/3 <==\na\n==> text 2/3 <==\n==> text 3/3 <==\nc\n\
             <console_output>\nfirst line\nsecond\n</console_output>\nScript error:\nError: boom"
        );
        assert_eq!(text(vec![], None, 100, dir.path()), "Script completed\nWall time 1.2 seconds\nOutput:\n");
    }

    #[test]
    fn long_text_keeps_its_start_and_end_and_goes_to_a_file() {
        let dir = tempfile::tempdir().unwrap();
        let long: String = (0..100).map(|i| format!("line {i:02} ✓\n")).collect();
        let out = text(vec![Item::Text(long.clone())], None, 10, dir.path());
        let body = out.strip_prefix("Script completed\nWall time 1.2 seconds\nOutput:\n").unwrap();
        let (shown, saved) = body.rsplit_once("\n\n").unwrap();
        assert_eq!(
            shown,
            "Warning: truncated output (original token count: 250)\nTotal output lines: 101\n\nline 00 ✓\nline 01 ✓\n…240 tokens truncated…line 98 ✓\nline 99 ✓\n"
        );
        let path = saved.strip_prefix("[Full output: ").unwrap().strip_suffix(" (read with offset/limit)]").unwrap();
        assert_eq!(std::fs::read_to_string(path).unwrap(), long);
        assert!(Path::new(path).starts_with(dir.path()));
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(std::fs::metadata(path).unwrap().permissions().mode() & 0o777, 0o600);
        }
        assert!(text(vec![Item::Text(long.clone())], None, 250, dir.path()).ends_with(&long));
    }

    #[test]
    fn images_are_shown_and_saved_once() {
        let dir = tempfile::tempdir().unwrap();
        let png = b"\x89PNG\r\n\x1a\n rest of the file".to_vec();
        let image = || Item::Image { mime: "image/png".into(), data: base64::engine::general_purpose::STANDARD.encode(&png) };
        let out =
            render(Duration::ZERO, vec![Item::Text("before".into()), image(), image(), Item::Text("after".into())], None, 100, dir.path());
        let [Content::Text(before), Content::Image { mime, data }, Content::Text(again), Content::Image { .. }, Content::Text(after)] =
            out.content.as_slice()
        else {
            panic!("{:?}", out.content);
        };
        assert_eq!((mime.as_str(), data), ("image/png", &png));
        let label = before.strip_prefix("Script completed\nWall time 0.0 seconds\nOutput:\n==> text 1/2 <==\nbefore\n").unwrap();
        let path = label.strip_prefix("[Image saved to ").unwrap().strip_suffix(" (image/png, 25B)]").unwrap();
        assert!(path.ends_with(".png"));
        assert_eq!(std::fs::read(path).unwrap(), png);
        assert_eq!(again, label);
        assert_eq!(after, "==> text 2/2 <==\nafter");
        assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 1);

        let missing = dir.path().join("missing");
        let out = render(Duration::ZERO, vec![image(), Item::Text("x".repeat(100))], None, 5, &missing);
        let [Content::Text(cut), Content::Image { .. }] = out.content.as_slice() else { panic!("{:?}", out.content) };
        assert!(cut.contains("xxxxxxxxxx…20 tokens truncated…xxxxxxxxxx\n\n[Could not save the full output: "), "{cut}");
        assert!(cut.contains("]\n[Image (image/png, 25B) could not be saved: "), "{cut}");

        let out = render(Duration::ZERO, vec![image(), Item::Text(String::new())], None, 100, dir.path());
        assert!(matches!(out.content.as_slice(), [Content::Text(_), Content::Image { .. }]), "{:?}", out.content);
    }
}
