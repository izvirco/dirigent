//! The background pattern (`[background]` in config.toml), after vir's ambient field: a
//! chart-like picture behind the window that looks still and, on a closer look, is moving.
//! Everything is drawn from a scalar field that creeps with the clock (`WAVES`: a few slow
//! plane waves), in the theme's `pattern` colour (its alpha: how strongly it shows). `Pattern`
//! picks the drawing, `Reach` how far it shows: behind the sidebar and the main pane, or behind the
//! sidebar only.
//!
//! It lies on the window's canvas, under everything: the docked sidebar has no fill of its
//! own, so it shows there at full strength, and the main pane's fill is made `PANE` opaque
//! over it, so it shows there dimmed. Behind the sidebar only (`Reach::Edge`) the docked
//! sidebar draws it inside itself instead, so it stops at its border even over a clear main
//! pane. The floating sidebar is solid and gets a copy.
//!
//! A picture is rasterized off the main thread (`draw`, into a `Mask`) for the window area
//! it covers, anchored at the window's top left; `Field::layer` paints the part of it under
//! an element. `Dirigent` redraws it every `CLOCK_SECS` and whenever what it is drawn for
//! changes (`Spec`). The waves cycle in one to three hours, so a redraw moves things by a
//! tenth of a pixel at most: nothing ever visibly steps, which also needs every drawing to
//! be continuous in the field (linear interpolation along grid edges is).

use gpui::{Bounds, Corners, IntoElement, Pixels, RenderImage, canvas, point, prelude::*, px, size};
use serde::{Deserialize, Serialize};
use std::f64::consts::TAU;
use std::sync::Arc;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Pattern {
    /// Nothing: the canvas stays plain.
    Off,
    /// A short stroke per cell along the field's contour direction: an engraving.
    Hatching,
    /// Iso-lines of the field: a terrain map.
    Contours,
    /// Iso-lines of the distance from a point off the bottom-left corner plus the field:
    /// rings spreading from a source.
    Spring,
    /// A dot per cell sized by the field.
    #[default]
    Halftone,
}

impl Pattern {
    pub const ALL: [Pattern; 5] = [Pattern::Off, Pattern::Hatching, Pattern::Contours, Pattern::Spring, Pattern::Halftone];

    /// Its name in config.toml and the Settings menu.
    pub fn name(self) -> &'static str {
        match self {
            Pattern::Off => "off",
            Pattern::Hatching => "hatching",
            Pattern::Contours => "contours",
            Pattern::Spring => "spring",
            Pattern::Halftone => "halftone",
        }
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Reach {
    /// Behind the sidebar only.
    Edge,
    /// Also behind the main pane, dimmed by its fill (`PANE`).
    #[default]
    Full,
}

/// How opaque the main pane's fill is over the pattern (its own alpha times this), so the
/// pattern shows there at `1 - PANE` of its strength. A clear pane (the `-transparent` themes)
/// stays clear: there the pattern is as strong as behind the sidebar.
pub const PANE: f32 = 0.7;

/// How often the picture is redrawn for the clock alone, in seconds.
pub const CLOCK_SECS: u64 = 2;

/// What a picture is drawn for: the pattern, the window area it covers from the top left
/// (logical px), the window's scale factor and the ink (its alpha: how strongly it shows).
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Spec {
    pub pattern: Pattern,
    pub width: u32,
    pub height: u32,
    pub scale: f32,
    pub color: u32,
}

impl Spec {
    /// Its area rounded up to `STEP`, so dragging the sidebar or the window edge only
    /// redraws every few dozen pixels (the picture may run past the window; nothing paints there).
    pub fn new(pattern: Pattern, width: f32, height: f32, scale: f32, color: u32) -> Self {
        const STEP: f32 = 64.;
        let round = |n: f32| ((n / STEP).ceil() * STEP) as u32;
        Self { pattern, width: round(width), height: round(height), scale, color }
    }
}

/// The background picture on screen, and what the window wants drawn.
#[derive(Default)]
pub struct Field {
    /// What should be on screen (set every render); `None`: nothing, the pattern is off.
    pub want: Option<Spec>,
    /// The newest picture and what it was drawn for.
    current: Option<(Spec, Arc<RenderImage>)>,
    /// The picture `current` replaced: the last frame drawn may still show it, so it
    /// leaves gpui's atlas only when the next one comes.
    previous: Option<Arc<RenderImage>>,
    /// A redraw is running.
    busy: bool,
}

impl Field {
    /// What to redraw now, if anything: the wanted picture when the one on screen was drawn
    /// for something else, or every time on the clock (`tick`). Never two at once.
    pub fn due(&mut self, tick: bool) -> Option<Spec> {
        let want = self.want?;
        let stale = tick || self.current.as_ref().is_none_or(|(spec, _)| *spec != want);
        (!self.busy && stale).then(|| {
            self.busy = true;
            want
        })
    }

    /// A redraw finished (`image` is `None` if it couldn't be drawn): put it on screen.
    /// Returns the picture to drop from gpui's atlas.
    pub fn show(&mut self, spec: Spec, image: Option<Arc<RenderImage>>) -> Option<Arc<RenderImage>> {
        self.busy = false;
        let image = image?;
        let replaced = self.current.replace((spec, image)).map(|(_, image)| image);
        std::mem::replace(&mut self.previous, replaced)
    }

    /// The pattern under an element: a fill for a `relative` parent, put before its
    /// content. It paints the part of the window-anchored picture it covers, with its
    /// corners rounded by `radius` (a rounded parent's `overflow_hidden` clips to its
    /// rectangle only). `None` while the pattern is off.
    pub fn layer(&self, radius: Pixels) -> Option<impl IntoElement + use<>> {
        self.want?;
        let (spec, image) = self.current.clone()?;
        let picture = Bounds::new(point(px(0.), px(0.)), size(px(spec.width as f32), px(spec.height as f32)));
        let paint = move |bounds, _, window: &mut gpui::Window, _: &mut gpui::App| {
            window.paint_image(bounds, picture, Corners::all(radius), image.clone(), 0, false).ok();
        };
        Some(canvas(|_, _, _| {}, paint).absolute().inset_0())
    }
}

/// Draw `spec` at `now` (ms since the epoch), in gpui's BGRA: the ink's colour throughout,
/// the drawing times the ink's alpha in the alpha. `None` with the pattern off or a zero area.
pub fn draw(spec: Spec, now: f64) -> Option<Arc<RenderImage>> {
    let (w, h) = (spec.width as f32, spec.height as f32);
    let mut mask = Mask::new(w, h, spec.scale);
    match spec.pattern {
        Pattern::Off => return None,
        Pattern::Hatching => hatching(&mut mask, w, h, now),
        Pattern::Contours => isolines(&mut mask, &TERRAIN, 0.2, w, h, now),
        Pattern::Spring => isolines(&mut mask, &SPRING, 1., w, h, now),
        Pattern::Halftone => halftone(&mut mask, w, h, now),
    }
    let [r, g, b, a] = spec.color.to_be_bytes();
    let k = a as f32 / 255.;
    let ramp: Vec<u8> = (0..=255).map(|c| (c as f32 * k).round() as u8).collect();
    let mut bgra = vec![0; mask.cover.len() * 4];
    for (px, &c) in bgra.as_chunks_mut::<4>().0.iter_mut().zip(&mask.cover) {
        *px = [b, g, r, ramp[c as usize]];
    }
    let frame = image::Frame::new(image::RgbaImage::from_raw(mask.w as u32, mask.h as u32, bgra)?);
    Some(Arc::new(RenderImage::new([frame])))
}

/// Coverage in device pixels, drawn in logical ones. Shapes are anti-aliased by each
/// pixel centre's distance to them and merged by max, so where they cross nothing paints
/// twice. Every shape is a few px across, so only its bounding box is visited.
struct Mask {
    w: usize,
    h: usize,
    scale: f32,
    cover: Vec<u8>,
}

impl Mask {
    fn new(w: f32, h: f32, scale: f32) -> Self {
        let (w, h) = ((w * scale).round() as usize, (h * scale).round() as usize);
        Self { w, h, scale, cover: vec![0; w * h] }
    }

    /// Paint the pixels around the segment `(x0, y0)..(x1, y1)` (device px; a point when
    /// both ends are one) that are within `r` + ½ of it, fading out over the last px.
    fn shade(&mut self, (x0, y0): (f32, f32), (x1, y1): (f32, f32), r: f32) {
        let (dx, dy) = (x1 - x0, y1 - y0);
        let len2 = (dx * dx + dy * dy).max(1e-6);
        let reach = r + 0.5;
        let clamp = |v: f32, max: usize| (v.max(0.) as usize).min(max);
        let (cx0, cx1) = (clamp((x0.min(x1) - reach).floor(), self.w), clamp((x0.max(x1) + reach).ceil(), self.w));
        let (cy0, cy1) = (clamp((y0.min(y1) - reach).floor(), self.h), clamp((y0.max(y1) + reach).ceil(), self.h));
        for y in cy0..cy1 {
            let py = y as f32 + 0.5 - y0;
            let row = &mut self.cover[y * self.w..(y + 1) * self.w];
            for (x, cell) in row.iter_mut().enumerate().take(cx1).skip(cx0) {
                let px = x as f32 + 0.5 - x0;
                let t = ((px * dx + py * dy) / len2).clamp(0., 1.);
                let d2 = (px - t * dx).powi(2) + (py - t * dy).powi(2);
                // Most of a slanted stroke's box is out of reach: no root for those.
                if d2 < reach * reach {
                    let c = ((reach - d2.sqrt()).min(1.) * 255.).round() as u8;
                    *cell = (*cell).max(c);
                }
            }
        }
    }

    /// A line one logical px wide with round ends.
    fn line(&mut self, (x0, y0): (f32, f32), (x1, y1): (f32, f32)) {
        let s = self.scale;
        self.shade((x0 * s, y0 * s), (x1 * s, y1 * s), s / 2.);
    }

    /// A disc of radius `r` (logical px).
    fn dot(&mut self, (x, y): (f32, f32), r: f32) {
        let s = self.scale;
        self.shade((x * s, y * s), (x * s, y * s), r * s);
    }
}

// ---- the field ----

/// A few plane waves of different lengths (px) and headings (radians); their sum is smooth
/// and irregular enough to read as terrain, and each creeps at its own pace (a cycle in
/// `pace` minutes: one to three hours). `amp` weighs them.
const WAVES: [(f32, f32, f64, f32); 4] = [(900., 0.3, 160., 1.), (620., 2.1, 120., 0.8), (410., 4.0, 90., 0.6), (280., 1.2, 60., 0.4)];

/// What's sampled: the waves scaled by `k` (about -1..1 at 1), plus `extra` of the
/// position and the area's height: the term that tells the drawings apart.
struct Scalar {
    k: f32,
    extra: Option<fn(f32, f32, f32) -> f32>,
}

const TERRAIN: Scalar = Scalar { k: 1., extra: None };

/// Rings every 56px from a source just off the bottom-left corner, wobbled by the field.
/// Placed by the height alone, so the rings stay put when the sidebar is resized.
const SPRING: Scalar = Scalar { k: 0.5, extra: Some(|x, y, h| (x + 0.1 * h).hypot(y - 1.1 * h) / 56.) };

/// A grid of `cell`-px squares over a `w` × `h` area, one sample per corner.
struct Grid {
    cols: usize,
    rows: usize,
    cell: f32,
}

impl Grid {
    fn new(w: f32, h: f32, cell: f32) -> Self {
        Self { cols: (w / cell).ceil() as usize + 1, rows: (h / cell).ceil() as usize + 1, cell }
    }

    /// The sample at `(x, y)`, clamped to the grid.
    fn at(&self, v: &[f32], x: isize, y: isize) -> f32 {
        let x = x.clamp(0, self.cols as isize - 1) as usize;
        let y = y.clamp(0, self.rows as isize - 1) as usize;
        v[y * self.cols + x]
    }
}

/// `s` on the grid at `now` (ms), for an area `h` tall. A plane wave sin(kx·x + ky·y + ωt)
/// splits into sin(kx·x)·cos(ky·y + ωt) + cos(kx·x)·sin(ky·y + ωt): one sin and cos per
/// column and per row, then two multiplies per sample.
fn sample(s: &Scalar, g: &Grid, h: f32, now: f64) -> Vec<f32> {
    let mut v = vec![0f32; g.cols * g.rows];
    for (len, dir, pace, amp) in WAVES {
        let (kx, ky) = (dir.cos() * std::f32::consts::TAU / len, dir.sin() * std::f32::consts::TAU / len);
        let amp = s.k * amp / 2.8;
        // The phase in f64: the clock in ms times a tiny rate loses everything in f32.
        let phase = (TAU / (pace * 60_000.) * now).rem_euclid(TAU) as f32;
        let (sx, cx): (Vec<f32>, Vec<f32>) = (0..g.cols).map(|x| kx * x as f32 * g.cell).map(|a| (amp * a.sin(), amp * a.cos())).unzip();
        for y in 0..g.rows {
            let (sy, cy) = (ky * y as f32 * g.cell + phase).sin_cos();
            for x in 0..g.cols {
                v[y * g.cols + x] += sx[x] * cy + cx[x] * sy;
            }
        }
    }
    if let Some(extra) = s.extra {
        for y in 0..g.rows {
            for x in 0..g.cols {
                v[y * g.cols + x] += extra(x as f32 * g.cell, y as f32 * g.cell, h);
            }
        }
    }
    v
}

// ---- the drawings ----

/// Marching squares: corner bits a (top-left) 8, b 4, c (bottom-right) 2, d 1; edges 0 top,
/// 1 right, 2 bottom, 3 left; each case is the edge pairs a line crosses.
const CASES: [&[(u8, u8)]; 16] = [
    &[],
    &[(3, 2)],
    &[(2, 1)],
    &[(3, 1)],
    &[(0, 1)],
    &[(0, 1), (2, 3)],
    &[(0, 2)],
    &[(0, 3)],
    &[(0, 3)],
    &[(0, 2)],
    &[(0, 3), (1, 2)],
    &[(0, 1)],
    &[(3, 1)],
    &[(1, 2)],
    &[(2, 3)],
    &[],
];

/// Iso-lines of `s` every `step`. Cells are fine enough that straight segments read as
/// curves, and the mask merges the loose segments' ends, so they read as whole lines
/// without being joined up.
fn isolines(mask: &mut Mask, s: &Scalar, step: f32, w: f32, h: f32, now: f64) {
    let g = Grid::new(w, h, 6.);
    let v = sample(s, &g, h, now);
    let (lo, hi) = v.iter().fold((f32::INFINITY, f32::NEG_INFINITY), |(lo, hi), &x| (lo.min(x), hi.max(x)));
    let mut iso = (lo / step).ceil() * step;
    while iso < hi {
        for y in 0..g.rows - 1 {
            for x in 0..g.cols - 1 {
                let a = v[y * g.cols + x];
                let b = v[y * g.cols + x + 1];
                let c = v[(y + 1) * g.cols + x + 1];
                let d = v[(y + 1) * g.cols + x];
                let idx = ((a > iso) as usize) << 3 | ((b > iso) as usize) << 2 | ((c > iso) as usize) << 1 | (d > iso) as usize;
                let (px, py, cell) = (x as f32 * g.cell, y as f32 * g.cell, g.cell);
                let point = |e: u8| match e {
                    0 => (px + cell * (iso - a) / (b - a), py),
                    1 => (px + cell, py + cell * (iso - b) / (c - b)),
                    2 => (px + cell * (iso - d) / (c - d), py + cell),
                    _ => (px, py + cell * (iso - a) / (d - a)),
                };
                for &(e1, e2) in CASES[idx] {
                    mask.line(point(e1), point(e2));
                }
            }
        }
        iso += step;
    }
}

/// One stroke per cell along the contour direction, longer where the field is steeper.
fn hatching(mask: &mut Mask, w: f32, h: f32, now: f64) {
    let g = Grid::new(w, h, 12.);
    let v = sample(&TERRAIN, &g, h, now);
    for y in 0..g.rows as isize {
        for x in 0..g.cols as isize {
            // The gradient by central differences, per px.
            let gx = (g.at(&v, x + 1, y) - g.at(&v, x - 1, y)) / (2. * g.cell);
            let gy = (g.at(&v, x, y + 1) - g.at(&v, x, y - 1)) / (2. * g.cell);
            let steep = gx.hypot(gy);
            if steep < 1e-6 {
                continue;
            }
            let len = 0.5 + 5. * (steep / 0.008).min(1.);
            let (dx, dy) = (-gy / steep * len, gx / steep * len);
            let (cx, cy) = (x as f32 * g.cell, y as f32 * g.cell);
            mask.line((cx - dx, cy - dy), (cx + dx, cy + dy));
        }
    }
}

/// A dot per cell, its size following the field.
fn halftone(mask: &mut Mask, w: f32, h: f32, now: f64) {
    let g = Grid::new(w, h, 14.);
    let v = sample(&TERRAIN, &g, h, now);
    for y in 0..g.rows {
        for x in 0..g.cols {
            let r = 0.4 + 2.2 * (0.5 + 0.5 * v[y * g.cols + x]);
            mask.dot((x as f32 * g.cell, y as f32 * g.cell), r);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn draws_every_pattern() {
        for pattern in Pattern::ALL {
            let spec = Spec::new(pattern, 200., 150., 2., 0x25272d66);
            assert_eq!((spec.width, spec.height), (256, 192));
            let image = draw(spec, 1.7e12);
            assert_eq!(image.is_some(), pattern != Pattern::Off, "{pattern:?}");
            if let Some(image) = image {
                let size = image.size(0);
                assert_eq!((size.width.0, size.height.0), (512, 384));
                // Some ink, never more than the pattern's alpha, in the theme's colour.
                let bytes = image.as_bytes(0).unwrap();
                let max = bytes.chunks(4).map(|p| p[3]).max().unwrap();
                assert!(max > 80 && max <= 0x66, "{pattern:?}: {max}");
                assert_eq!(&bytes[..3], &[0x2d, 0x27, 0x25]);
            }
        }
    }

    #[test]
    fn lines_cover_one_px_across() {
        let mut mask = Mask::new(10., 10., 1.);
        mask.line((1., 5.5), (9., 5.5));
        let column: Vec<u8> = (0..10).map(|y| mask.cover[y * 10 + 5]).collect();
        assert_eq!(column, [0, 0, 0, 0, 0, 255, 0, 0, 0, 0]);
        // Between two rows it splits across both; crossings don't add up.
        mask.line((1., 5.), (9., 5.));
        assert_eq!((mask.cover[4 * 10 + 5], mask.cover[5 * 10 + 5]), (128, 255));
    }

    #[test]
    fn redraws_one_at_a_time() {
        let mut field = Field::default();
        assert_eq!(field.due(true), None);
        let spec = Spec::new(Pattern::Hatching, 100., 100., 1., 0xffffffff);
        field.want = Some(spec);
        assert_eq!(field.due(false), Some(spec));
        assert_eq!(field.due(true), None, "one is running");
        assert!(field.show(spec, draw(spec, 0.)).is_none());
        assert_eq!(field.due(false), None, "up to date");
        assert_eq!(field.due(true), Some(spec), "the clock moved");
        assert!(field.show(spec, draw(spec, 1.)).is_none(), "the replaced one stays a round");
        assert!(field.due(true).is_some());
        assert!(field.show(spec, draw(spec, 2.)).is_some());
    }
}
