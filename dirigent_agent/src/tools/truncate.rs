//! What the model reads of a file or a command's output is cut to two limits, whichever is
//! hit first: `MAX_LINES` lines and `MAX_BYTES` bytes. A file is cut at its end (`head`: the
//! model reads on with an offset), output at its start (`tail`: errors and results are last).

pub const MAX_LINES: usize = 2000;
pub const MAX_BYTES: usize = 50 * 1024;

/// What a limit cut.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum By {
    Lines,
    Bytes,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Truncated {
    /// What is left: whole lines (but see `partial`).
    pub text: String,
    /// `None`: nothing was cut.
    pub by: Option<By>,
    /// Lines of the whole text, and of what is left.
    pub total_lines: usize,
    pub lines: usize,
    /// `tail`: what is left is the end of one line longer than the byte limit.
    pub partial: bool,
    /// `head`: the first line alone is over the byte limit, so nothing is left.
    pub first_line_too_long: bool,
}

/// The text's lines, as they are counted: a trailing newline ends the last line, it doesn't
/// start an empty one.
fn lines(text: &str) -> Vec<&str> {
    if text.is_empty() {
        return vec![];
    }
    let mut lines: Vec<&str> = text.split('\n').collect();
    if text.ends_with('\n') {
        lines.pop();
    }
    lines
}

fn whole(text: &str, total_lines: usize) -> Truncated {
    Truncated { text: text.to_owned(), by: None, total_lines, lines: total_lines, partial: false, first_line_too_long: false }
}

/// Keep the start of `text`. Never a partial line: if the first line alone is over the byte
/// limit, nothing is kept (`first_line_too_long`).
pub fn head(text: &str) -> Truncated {
    let all = lines(text);
    if all.len() <= MAX_LINES && text.len() <= MAX_BYTES {
        return whole(text, all.len());
    }
    if all[0].len() > MAX_BYTES {
        return Truncated {
            text: String::new(),
            by: Some(By::Bytes),
            total_lines: all.len(),
            lines: 0,
            partial: false,
            first_line_too_long: true,
        };
    }
    let mut kept = vec![];
    let mut bytes = 0;
    let mut by = By::Lines;
    for (i, line) in all.iter().enumerate().take(MAX_LINES) {
        // A newline joins it to the line before.
        let size = line.len() + usize::from(i > 0);
        if bytes + size > MAX_BYTES {
            by = By::Bytes;
            break;
        }
        kept.push(*line);
        bytes += size;
    }
    Truncated { text: kept.join("\n"), by: Some(by), total_lines: all.len(), lines: kept.len(), partial: false, first_line_too_long: false }
}

/// Keep the end of `text`. If its last line alone is over the byte limit, the end of that
/// line is kept (`partial`).
pub fn tail(text: &str) -> Truncated {
    let all = lines(text);
    if all.len() <= MAX_LINES && text.len() <= MAX_BYTES {
        return whole(text, all.len());
    }
    let mut kept: Vec<&str> = vec![];
    let mut bytes = 0;
    let mut by = By::Lines;
    let mut partial = false;
    for line in all.iter().rev() {
        if kept.len() >= MAX_LINES {
            break;
        }
        let size = line.len() + usize::from(!kept.is_empty());
        if bytes + size > MAX_BYTES {
            by = By::Bytes;
            if kept.is_empty() {
                kept.push(&line[line.ceil_char_boundary(line.len() - MAX_BYTES)..]);
                partial = true;
            }
            break;
        }
        kept.push(line);
        bytes += size;
    }
    kept.reverse();
    Truncated { text: kept.join("\n"), by: Some(by), total_lines: all.len(), lines: kept.len(), partial, first_line_too_long: false }
}

/// `512B`, `1.5KB`, `2.0MB`.
pub fn size(bytes: usize) -> String {
    match bytes {
        b if b < 1024 => format!("{b}B"),
        b if b < 1024 * 1024 => format!("{:.1}KB", b as f64 / 1024.),
        b => format!("{:.1}MB", b as f64 / (1024. * 1024.)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cuts_whole_lines_at_either_limit() {
        assert_eq!(head("a\nb\n").by, None);
        let many = "x\n".repeat(MAX_LINES + 5);
        let h = head(&many);
        assert_eq!((h.by, h.lines, h.total_lines), (Some(By::Lines), MAX_LINES, MAX_LINES + 5));
        let t = tail(&many);
        assert_eq!((t.by, t.lines), (Some(By::Lines), MAX_LINES));
        // Three lines of 20 KB: two fit in 50 KB.
        let wide = format!("{0}\n{0}\n{0}", "y".repeat(20 * 1024));
        let h = head(&wide);
        assert_eq!((h.by, h.lines), (Some(By::Bytes), 2));
        let t = tail(&wide);
        assert_eq!((t.by, t.lines, t.partial), (Some(By::Bytes), 2, false));
        // One line over the limit: nothing of a file, the end of an output (on a char boundary).
        let long = "é".repeat(MAX_BYTES);
        assert!(head(&long).first_line_too_long);
        let t = tail(&long);
        assert!(t.partial && t.text.len() <= MAX_BYTES && t.text.chars().all(|c| c == 'é'));
    }
}
