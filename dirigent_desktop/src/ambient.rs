use gpui::{Bounds, Corners, IntoElement, Pixels, RenderImage, canvas, point, prelude::*, px, size};
use serde::{Deserialize, Serialize};
use std::f64::consts::TAU;
use std::sync::Arc;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Pattern {
    Off,
    Hatching,
    Contours,
    Spring,
    #[default]
    Halftone,
}

impl Pattern {
    pub const ALL: [Pattern; 5] = [Pattern::Off, Pattern::Hatching, Pattern::Contours, Pattern::Spring, Pattern::Halftone];

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
    Edge,
    #[default]
    Full,
}

pub const PANE_FILL_ALPHA: f32 = 0.7;

pub const CLOCK_SECS: u64 = 2;

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Spec {
    pub pattern: Pattern,
    pub width: u32,
    pub height: u32,
    pub scale: f32,
    pub color: u32,
}

impl Spec {
    pub fn rounded(pattern: Pattern, width: f32, height: f32, scale: f32, color: u32) -> Self {
        const STEP: f32 = 64.;
        let round = |n: f32| ((n / STEP).ceil() * STEP) as u32;
        Self { pattern, width: round(width), height: round(height), scale, color }
    }
}

#[derive(Default)]
pub struct Field {
    pub want: Option<Spec>,
    current: Option<(Spec, Arc<RenderImage>)>,
    on_last_frame: Option<Arc<RenderImage>>,
    busy: bool,
}

impl Field {
    pub fn due(&mut self, tick: bool) -> Option<Spec> {
        let want = self.want?;
        let stale = tick || self.current.as_ref().is_none_or(|(spec, _)| *spec != want);
        (!self.busy && stale).then(|| {
            self.busy = true;
            want
        })
    }

    pub fn show(&mut self, spec: Spec, image: Option<Arc<RenderImage>>) -> Option<Arc<RenderImage>> {
        self.busy = false;
        let image = image?;
        let replaced = self.current.replace((spec, image)).map(|(_, image)| image);
        std::mem::replace(&mut self.on_last_frame, replaced)
    }

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
                if d2 < reach * reach {
                    let c = ((reach - d2.sqrt()).min(1.) * 255.).round() as u8;
                    *cell = (*cell).max(c);
                }
            }
        }
    }

    fn line(&mut self, (x0, y0): (f32, f32), (x1, y1): (f32, f32)) {
        let s = self.scale;
        self.shade((x0 * s, y0 * s), (x1 * s, y1 * s), s / 2.);
    }

    fn dot(&mut self, (x, y): (f32, f32), r: f32) {
        let s = self.scale;
        self.shade((x * s, y * s), (x * s, y * s), r * s);
    }
}

struct Wave {
    len_px: f32,
    heading: f32,
    pace_minutes: f64,
    amp: f32,
}

const WAVES: [Wave; 4] = [
    Wave { len_px: 900., heading: 0.3, pace_minutes: 160., amp: 1. },
    Wave { len_px: 620., heading: 2.1, pace_minutes: 120., amp: 0.8 },
    Wave { len_px: 410., heading: 4.0, pace_minutes: 90., amp: 0.6 },
    Wave { len_px: 280., heading: 1.2, pace_minutes: 60., amp: 0.4 },
];

struct Scalar {
    k: f32,
    extra: Option<fn(x: f32, y: f32, height: f32) -> f32>,
}

const TERRAIN: Scalar = Scalar { k: 1., extra: None };

const SPRING: Scalar = Scalar { k: 0.5, extra: Some(rings) };

fn rings(x: f32, y: f32, height: f32) -> f32 {
    const SPACING_PX: f32 = 56.;
    let source = (-0.1 * height, 1.1 * height);
    (x - source.0).hypot(y - source.1) / SPACING_PX
}

struct Grid {
    cols: usize,
    rows: usize,
    cell: f32,
}

impl Grid {
    fn new(w: f32, h: f32, cell: f32) -> Self {
        Self { cols: (w / cell).ceil() as usize + 1, rows: (h / cell).ceil() as usize + 1, cell }
    }

    fn at(&self, v: &[f32], x: isize, y: isize) -> f32 {
        let x = x.clamp(0, self.cols as isize - 1) as usize;
        let y = y.clamp(0, self.rows as isize - 1) as usize;
        v[y * self.cols + x]
    }
}

fn sample(s: &Scalar, g: &Grid, h: f32, now: f64) -> Vec<f32> {
    let mut v = vec![0f32; g.cols * g.rows];
    for Wave { len_px, heading, pace_minutes, amp } in WAVES {
        let (kx, ky) = (heading.cos() * std::f32::consts::TAU / len_px, heading.sin() * std::f32::consts::TAU / len_px);
        let amp = s.k * amp / 2.8;
        let phase = (TAU / (pace_minutes * 60_000.) * now).rem_euclid(TAU) as f32;
        let (sin_x, cos_x): (Vec<f32>, Vec<f32>) =
            (0..g.cols).map(|x| kx * x as f32 * g.cell).map(|a| (amp * a.sin(), amp * a.cos())).unzip();
        for y in 0..g.rows {
            let (sin_y, cos_y) = (ky * y as f32 * g.cell + phase).sin_cos();
            for x in 0..g.cols {
                v[y * g.cols + x] += sin_x[x] * cos_y + cos_x[x] * sin_y;
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

#[derive(Clone, Copy)]
enum Edge {
    Top,
    Right,
    Bottom,
    Left,
}

const CASES: [&[(Edge, Edge)]; 16] = {
    use Edge::*;
    [
        &[],
        &[(Left, Bottom)],
        &[(Bottom, Right)],
        &[(Left, Right)],
        &[(Top, Right)],
        &[(Top, Right), (Bottom, Left)],
        &[(Top, Bottom)],
        &[(Top, Left)],
        &[(Top, Left)],
        &[(Top, Bottom)],
        &[(Top, Left), (Right, Bottom)],
        &[(Top, Right)],
        &[(Left, Right)],
        &[(Right, Bottom)],
        &[(Bottom, Left)],
        &[],
    ]
};

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
                let point = |e: Edge| match e {
                    Edge::Top => (px + cell * (iso - a) / (b - a), py),
                    Edge::Right => (px + cell, py + cell * (iso - b) / (c - b)),
                    Edge::Bottom => (px + cell * (iso - d) / (c - d), py + cell),
                    Edge::Left => (px, py + cell * (iso - a) / (d - a)),
                };
                for &(e1, e2) in CASES[idx] {
                    mask.line(point(e1), point(e2));
                }
            }
        }
        iso += step;
    }
}

fn hatching(mask: &mut Mask, w: f32, h: f32, now: f64) {
    let g = Grid::new(w, h, 12.);
    let v = sample(&TERRAIN, &g, h, now);
    for y in 0..g.rows as isize {
        for x in 0..g.cols as isize {
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
            let spec = Spec::rounded(pattern, 200., 150., 2., 0x25272d66);
            assert_eq!((spec.width, spec.height), (256, 192));
            let image = draw(spec, 1.7e12);
            assert_eq!(image.is_some(), pattern != Pattern::Off, "{pattern:?}");
            if let Some(image) = image {
                let size = image.size(0);
                assert_eq!((size.width.0, size.height.0), (512, 384));
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
        mask.line((1., 5.), (9., 5.));
        assert_eq!((mask.cover[4 * 10 + 5], mask.cover[5 * 10 + 5]), (128, 255));
    }

    #[test]
    fn redraws_one_at_a_time() {
        let mut field = Field::default();
        assert_eq!(field.due(true), None);
        let spec = Spec::rounded(Pattern::Hatching, 100., 100., 1., 0xffffffff);
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
