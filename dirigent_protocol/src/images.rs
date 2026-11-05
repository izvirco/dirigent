//! Images a reply shows. The model writes `![caption](/path/on/host.png)`; when the host
//! persists the reply it reads each such file into its blobs and records the link on the
//! text block as `blobs`: `{"<target as written>": "<blob id>"}`. The text itself stays as
//! the model wrote it, so a session replayed from the host's entries gets back exactly that.
//! (Given `blob:<id>` links in its history instead, a model starts writing such links
//! itself, with made-up ids the host never stored.) A client draws the images by looking their targets up in
//! `blobs` (the desktop's `markdown::link_blobs`). Both sides find the targets with
//! `image_targets`.
//!
//! Entries persisted before `blobs` existed have `blob:<id>` targets in their text already.

use std::ops::Range;

/// Byte ranges of the image targets in markdown `text`: of each `![alt](target)`, the
/// target's part before any title (`![a](p.png "title")`), trimmed. Empty ones are left out.
pub fn image_targets(text: &str) -> Vec<Range<usize>> {
    let mut out = vec![];
    let mut from = 0;
    while let Some(i) = text[from..].find("![") {
        let start = from + i;
        // Alt text up to `](`, not across a line; target up to `)`.
        let link = text[start..].find("](").map(|j| start + j + 2).filter(|&j| !text[start..j].contains('\n'));
        let Some((target_start, target_end)) = link.and_then(|j| text[j..].find(')').map(|k| (j, j + k))) else {
            from = start + 2;
            continue;
        };
        let target = &text[target_start..target_end];
        let lead = target.len() - target.trim_start().len();
        let path = target.trim().split(char::is_whitespace).next().unwrap_or("");
        if !path.is_empty() {
            out.push(target_start + lead..target_start + lead + path.len());
        }
        from = target_end;
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn targets() {
        let text = "Here:\n\n![home](shot.png)\n![again]( /abs/shot.png \"t\")\nnot ![a](missing.png) nor ![b]()\n![\nstray](x.png)";
        let targets: Vec<&str> = image_targets(text).into_iter().map(|r| &text[r]).collect();
        assert_eq!(targets, ["shot.png", "/abs/shot.png", "missing.png"]);
    }
}
