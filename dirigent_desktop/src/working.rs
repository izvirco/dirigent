use crate::theme::{self, mix};
use gpui::{Animation, AnimationExt, AnyElement, HighlightStyle, StyledText, div, prelude::*};
use std::time::Duration;

const PASS: f32 = 10.;
const WAIT: f32 = 1.;
const EDGE: f32 = 1.5;

pub fn line(key: usize, text: String) -> AnyElement {
    let t = theme::t();
    let n = text.chars().count() as f32;
    let cycle = Duration::from_secs_f32(2. * (PASS + WAIT));
    div()
        .min_w_0()
        .truncate()
        .with_animation(("working", key), Animation::new(cycle).repeat_synced(), move |d, v| {
            let s = v * 2. * (PASS + WAIT);
            let (back, s) = if s < PASS + WAIT { (false, s) } else { (true, s - PASS - WAIT) };
            let front = -EDGE + (n + 2. * EDGE) * (s / PASS).min(1.);
            let runs = text.char_indices().enumerate().map(|(i, (at, c))| {
                let passed = ease((front - i as f32 - 0.5) / EDGE + 0.5);
                let k = if back { 1. - passed } else { passed };
                (at..at + c.len_utf8(), HighlightStyle { color: Some(mix(t.fg_1, t.working, k)), ..Default::default() })
            });
            d.child(StyledText::new(text.clone()).with_highlights(runs))
        })
        .into_any_element()
}

fn ease(x: f32) -> f32 {
    let x = x.clamp(0., 1.);
    x * x * (3. - 2. * x)
}
