pub const MAX_LINES: usize = 2000;
pub const MAX_BYTES: usize = 50 * 1024;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum By {
    Lines,
    Bytes,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Truncated {
    pub text: String,
    pub by: Option<By>,
    pub total_lines: usize,
    pub lines: usize,
    pub partial: bool,
    pub first_line_too_long: bool,
}

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
        let wide = format!("{0}\n{0}\n{0}", "y".repeat(20 * 1024));
        let h = head(&wide);
        assert_eq!((h.by, h.lines), (Some(By::Bytes), 2));
        let t = tail(&wide);
        assert_eq!((t.by, t.lines, t.partial), (Some(By::Bytes), 2, false));
        let long = "é".repeat(MAX_BYTES);
        assert!(head(&long).first_line_too_long);
        let t = tail(&long);
        assert!(t.partial && t.text.len() <= MAX_BYTES && t.text.chars().all(|c| c == 'é'));
    }
}
