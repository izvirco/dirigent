//! Lucide icons (ISC) and the lab logos from Simple Icons (CC0), see assets/icons/LICENSE,
//! embedded in the binary and served to gpui's `svg()` through an `AssetSource`. Add an
//! icon: drop the svg in assets/icons and list it here.

use crate::theme::u;
use gpui::{AssetSource, Result, SharedString, Svg, prelude::*, rgba, svg};
use std::borrow::Cow;

macro_rules! icons {
    ($($name:literal),* $(,)?) => {
        const ICONS: &[(&str, &[u8])] = &[$(($name, include_bytes!(concat!("../assets/icons/", $name, ".svg")))),*];
    };
}

icons!(
    "archive",
    "archive-restore",
    "arrow-up",
    "chart-no-axes-combined",
    "check",
    "chevron-down",
    "chevron-left",
    "chevron-right",
    "chevron-up",
    "circle-check",
    "circle-help",
    "circle-x",
    "claude",
    "copy",
    "corner-down-right",
    "ellipsis",
    "folder",
    "git-branch",
    "git-fork",
    "hourglass",
    "list",
    "message-circle-question",
    "minus",
    "moon",
    "openai",
    "panel-left-close",
    "panel-left-open",
    "panel-right-close",
    "panel-right-open",
    "pin",
    "play",
    "plus",
    "server",
    "settings",
    "sparkles",
    "square",
    "trash-2",
    "triangle-alert",
    "x",
);

pub struct Assets;

impl AssetSource for Assets {
    fn load(&self, path: &str) -> Result<Option<Cow<'static, [u8]>>> {
        let name = path.strip_prefix("icons/").and_then(|p| p.strip_suffix(".svg")).unwrap_or(path);
        Ok(ICONS.iter().find(|(n, _)| *n == name).map(|(_, bytes)| Cow::Borrowed(*bytes)))
    }

    fn list(&self, _path: &str) -> Result<Vec<SharedString>> {
        Ok(ICONS.iter().map(|(n, _)| SharedString::from(format!("{n}.svg"))).collect())
    }
}

/// A 14px icon (design pixels, `theme::u`). `Svg` ignores the inherited text color, so it takes one explicitly.
pub fn icon(name: &'static str, color: u32) -> Svg {
    svg().path(format!("icons/{name}.svg")).size(u(14.)).flex_shrink_0().text_color(rgba(color))
}
