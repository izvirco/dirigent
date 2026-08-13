//! Renders TeX expressions to SVG away from the UI thread.

use std::collections::HashSet;

use async_channel::{Receiver, Sender};
use mathjax_svg_rs::{HorizontalAlign, MathJax, Options};

use crate::model::Id;

const MATH_FONT_SIZE: f64 = 16.0;

pub(crate) struct MathRenderTask {
    pub(crate) source: String,
}

pub(crate) struct MathRenderResult {
    pub(crate) source: String,
    pub(crate) rendered: Result<RenderedMath, String>,
}

pub(crate) struct RenderedMath {
    pub(crate) svg: String,
    pub(crate) width: f32,
    pub(crate) height: f32,
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
        let rendered = render_math(renderer, &task.source);
        if results
            .send_blocking(MathRenderResult {
                source: task.source,
                rendered,
            })
            .is_err()
        {
            break;
        }
    }
}

fn render_math(renderer: &MathJax, source: &str) -> Result<RenderedMath, String> {
    let rendered = renderer.render_tex(
        source,
        &Options {
            font_size: MATH_FONT_SIZE,
            horizontal_align: HorizontalAlign::Left,
        },
    )?;
    let svg = extract_svg(&rendered)?;
    let (width, height) = svg_dimensions(&svg)?;
    Ok(RenderedMath { svg, width, height })
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

fn svg_dimensions(svg: &str) -> Result<(f32, f32), String> {
    let header_end = svg
        .find('>')
        .ok_or_else(|| "MathJax SVG had no opening tag".to_string())?;
    let header = &svg[..header_end];

    let explicit_size = attribute(header, "width")
        .and_then(|value| css_length(value, MATH_FONT_SIZE as f32))
        .zip(
            attribute(header, "height").and_then(|value| css_length(value, MATH_FONT_SIZE as f32)),
        );
    let view_box_size = attribute(header, "viewBox").and_then(|view_box| {
        let values = view_box
            .split(|character: char| character.is_ascii_whitespace() || character == ',')
            .filter(|value| !value.is_empty())
            .map(str::parse::<f32>)
            .collect::<Result<Vec<_>, _>>()
            .ok()?;
        (values.len() == 4).then(|| {
            (
                values[2] / 1_000.0 * MATH_FONT_SIZE as f32,
                values[3] / 1_000.0 * MATH_FONT_SIZE as f32,
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
    fn extracts_wrapped_svg_and_uses_ex_dimensions() {
        let rendered = r#"<mjx-container><svg width="5.5ex" height="2ex" viewBox="0 0 5500 2000"><path/></svg></mjx-container>"#;
        let svg = extract_svg(rendered).unwrap();
        assert_eq!(svg_dimensions(&svg).unwrap(), (44.0, 16.0));
    }

    #[test]
    fn falls_back_to_mathjax_view_box_dimensions() {
        let svg = r#"<svg width="100%" viewBox="0 -800 2500 1250"></svg>"#;
        assert_eq!(svg_dimensions(svg).unwrap(), (40.0, 20.0));
    }
}
