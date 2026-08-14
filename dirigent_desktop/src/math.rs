//! Renders TeX expressions to SVG away from the UI thread.

use std::{borrow::Cow, collections::HashSet};

use async_channel::{Receiver, Sender};
use mathjax_svg_rs::{HorizontalAlign, MathJax, Options};

use crate::model::Id;

const DISPLAY_MATH_FONT_SIZE: f32 = 16.0;
const INLINE_MATH_FONT_SIZE: f32 = 14.0;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub(crate) enum MathRenderMode {
    Display,
    Inline,
}

impl MathRenderMode {
    fn font_size(self) -> f32 {
        match self {
            Self::Display => DISPLAY_MATH_FONT_SIZE,
            Self::Inline => INLINE_MATH_FONT_SIZE,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub(crate) struct MathRenderKey {
    pub(crate) source: String,
    pub(crate) mode: MathRenderMode,
}

pub(crate) struct MathRenderTask {
    pub(crate) key: MathRenderKey,
}

pub(crate) struct MathRenderResult {
    pub(crate) key: MathRenderKey,
    pub(crate) rendered: Result<RenderedMath, String>,
}

pub(crate) struct RenderedMath {
    pub(crate) svg: String,
    pub(crate) width: f32,
    pub(crate) height: f32,
    /// Distance from the bottom of the SVG to MathJax's internal baseline.
    pub(crate) baseline_offset: f32,
}

#[derive(Clone)]
pub(crate) enum MathRenderState {
    Pending {
        messages: HashSet<(Id, usize)>,
    },
    Ready {
        asset_path: String,
        width: f32,
        height: f32,
        baseline_offset: f32,
    },
    Failed,
}

/// MathJax owns its own JavaScript worker. This serial coordinator keeps its blocking API off
/// GPUI's executors and creates the relatively expensive runtime only when math is first used.
pub(crate) fn run_math_render_worker(
    tasks: Receiver<MathRenderTask>,
    results: Sender<MathRenderResult>,
) {
    let mut renderer = None;
    while let Ok(task) = tasks.recv_blocking() {
        let renderer = renderer.get_or_insert_with(MathJax::new);
        let rendered = render_math(renderer, &task.key);
        if results
            .send_blocking(MathRenderResult {
                key: task.key,
                rendered,
            })
            .is_err()
        {
            break;
        }
    }
}

fn render_math(renderer: &MathJax, key: &MathRenderKey) -> Result<RenderedMath, String> {
    let source = normalize_siunitx(&key.source);
    let font_size = key.mode.font_size();
    let rendered = renderer.render_tex(
        &source,
        &Options {
            font_size: font_size.into(),
            horizontal_align: HorizontalAlign::Left,
        },
    )?;
    let svg = extract_svg(&rendered)?;
    let (width, height) = svg_dimensions(&svg, font_size)?;
    let baseline_offset = -svg_vertical_align(&svg, font_size).unwrap_or(0.0);
    Ok(RenderedMath {
        svg,
        width,
        height,
        baseline_offset,
    })
}

/// MathJax does not ship siunitx. Translate its common quantity command to core TeX while
/// leaving unsupported or malformed uses untouched for MathJax to report.
fn normalize_siunitx(source: &str) -> Cow<'_, str> {
    let mut output = String::new();
    let mut search_from = 0;
    let mut copied_until = 0;

    while let Some(relative_start) = source[search_from..].find(r"\qty") {
        let start = search_from + relative_start;
        let after_command = start + r"\qty".len();
        let preceding_backslashes = source.as_bytes()[..start]
            .iter()
            .rev()
            .take_while(|byte| **byte == b'\\')
            .count();
        if preceding_backslashes % 2 == 1 {
            search_from = after_command;
            continue;
        }
        if source[after_command..]
            .chars()
            .next()
            .is_some_and(|character| character.is_ascii_alphabetic())
        {
            search_from = after_command;
            continue;
        }

        let first_start = skip_ascii_whitespace(source, after_command);
        let Some((value, first_end)) = braced_group(source, first_start) else {
            search_from = after_command;
            continue;
        };
        let second_start = skip_ascii_whitespace(source, first_end);
        let Some((unit, second_end)) = braced_group(source, second_start) else {
            search_from = after_command;
            continue;
        };

        output.push_str(&source[copied_until..start]);
        output.push_str(value);
        output.push_str(r"\,\mathrm{");
        output.push_str(unit);
        output.push('}');
        copied_until = second_end;
        search_from = second_end;
    }

    if output.is_empty() {
        Cow::Borrowed(source)
    } else {
        output.push_str(&source[copied_until..]);
        Cow::Owned(output)
    }
}

fn skip_ascii_whitespace(source: &str, mut index: usize) -> usize {
    while source
        .as_bytes()
        .get(index)
        .is_some_and(u8::is_ascii_whitespace)
    {
        index += 1;
    }
    index
}

fn braced_group(source: &str, start: usize) -> Option<(&str, usize)> {
    if source.as_bytes().get(start) != Some(&b'{') {
        return None;
    }
    let mut depth = 1;
    let mut index = start + 1;
    while index < source.len() {
        match source.as_bytes()[index] {
            b'\\' if matches!(source.as_bytes().get(index + 1), Some(b'{' | b'}')) => index += 2,
            b'{' => {
                depth += 1;
                index += 1;
            }
            b'}' => {
                depth -= 1;
                if depth == 0 {
                    return Some((&source[start + 1..index], index + 1));
                }
                index += 1;
            }
            _ => index += 1,
        }
    }
    None
}

// MathJax may wrap its SVG in an mjx-container. GPUI's SVG parser needs the SVG itself as root.
fn extract_svg(rendered: &str) -> Result<String, String> {
    let start = rendered
        .find("<svg")
        .ok_or_else(|| "MathJax output did not contain an SVG".to_string())?;
    let relative_end = rendered[start..]
        .find("</svg>")
        .ok_or_else(|| "MathJax output contained an incomplete SVG".to_string())?;
    let end = start + relative_end + "</svg>".len();
    Ok(rendered[start..end].to_string())
}

fn svg_dimensions(svg: &str, font_size: f32) -> Result<(f32, f32), String> {
    let header_end = svg
        .find('>')
        .ok_or_else(|| "MathJax SVG had no opening tag".to_string())?;
    let header = &svg[..header_end];

    let explicit_size = attribute(header, "width")
        .and_then(|value| css_length(value, font_size))
        .zip(attribute(header, "height").and_then(|value| css_length(value, font_size)));
    let view_box_size = attribute(header, "viewBox").and_then(|view_box| {
        let values = view_box
            .split(|character: char| character.is_ascii_whitespace() || character == ',')
            .filter(|value| !value.is_empty())
            .map(str::parse::<f32>)
            .collect::<Result<Vec<_>, _>>()
            .ok()?;
        (values.len() == 4).then(|| {
            (
                values[2] / 1_000.0 * font_size,
                values[3] / 1_000.0 * font_size,
            )
        })
    });
    let (width, height) = explicit_size
        .or(view_box_size)
        .ok_or_else(|| "MathJax SVG had no usable dimensions".to_string())?;
    if !width.is_finite() || !height.is_finite() || width <= 0.0 || height <= 0.0 {
        return Err("MathJax SVG had invalid dimensions".to_string());
    }
    Ok((width.min(8_192.0), height.min(4_096.0)))
}

fn svg_vertical_align(svg: &str, font_size: f32) -> Option<f32> {
    let header = &svg[..svg.find('>')?];
    let style = attribute(header, "style")?;
    style.split(';').find_map(|declaration| {
        let (property, value) = declaration.split_once(':')?;
        (property.trim() == "vertical-align").then(|| css_length(value.trim(), font_size))?
    })
}

fn attribute<'a>(tag: &'a str, name: &str) -> Option<&'a str> {
    for quote in ['"', '\''] {
        let marker = format!(" {name}={quote}");
        let Some(start) = tag.find(&marker).map(|start| start + marker.len()) else {
            continue;
        };
        let Some(end) = tag[start..].find(quote).map(|end| start + end) else {
            continue;
        };
        return Some(&tag[start..end]);
    }
    None
}

fn css_length(value: &str, font_size: f32) -> Option<f32> {
    let unit_start = value
        .find(|character: char| !(character.is_ascii_digit() || matches!(character, '.' | '-')))
        .unwrap_or(value.len());
    let number = value[..unit_start].parse::<f32>().ok()?;
    match value[unit_start..].trim() {
        "" | "px" => Some(number),
        "em" => Some(number * font_size),
        "ex" => Some(number * font_size / 2.0),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn translates_siunitx_quantities_to_core_tex() {
        assert_eq!(normalize_siunitx(r"\qty{3}{cm}"), r"3\,\mathrm{cm}");
        assert_eq!(
            normalize_siunitx(r"v=\qty {\frac{1}{2}} {m/s}"),
            r"v=\frac{1}{2}\,\mathrm{m/s}"
        );
        assert_eq!(
            normalize_siunitx(r"\qty{3}{m}+\qty{4}{s}"),
            r"3\,\mathrm{m}+4\,\mathrm{s}"
        );
    }

    #[test]
    fn leaves_other_qty_commands_untouched() {
        assert!(matches!(normalize_siunitx(r"\qty{x}"), Cow::Borrowed(_)));
        assert_eq!(
            normalize_siunitx(r"\qtyrange{1}{2}{m}"),
            r"\qtyrange{1}{2}{m}"
        );
        assert_eq!(normalize_siunitx(r"\\qty{3}{cm}"), r"\\qty{3}{cm}");
        assert_eq!(
            normalize_siunitx(r"\qty{x}+\qty{3}{cm}"),
            r"\qty{x}+3\,\mathrm{cm}"
        );
    }

    #[test]
    fn extracts_wrapped_svg_and_uses_ex_dimensions() {
        let rendered = r#"<mjx-container><svg width="5.5ex" height="2ex" viewBox="0 0 5500 2000"><path/></svg></mjx-container>"#;
        let svg = extract_svg(rendered).unwrap();
        assert_eq!(
            svg_dimensions(&svg, DISPLAY_MATH_FONT_SIZE).unwrap(),
            (44.0, 16.0)
        );
    }

    #[test]
    fn falls_back_to_mathjax_view_box_dimensions() {
        let svg = r#"<svg width="100%" viewBox="0 -800 2500 1250"></svg>"#;
        assert_eq!(
            svg_dimensions(svg, DISPLAY_MATH_FONT_SIZE).unwrap(),
            (40.0, 20.0)
        );
    }

    #[test]
    fn reads_mathjax_svg_baseline_at_the_requested_font_size() {
        let svg = r#"<svg style="vertical-align: -0.5ex;" width="2ex" height="2ex"></svg>"#;
        assert_eq!(svg_vertical_align(svg, INLINE_MATH_FONT_SIZE), Some(-3.5));
    }
}
