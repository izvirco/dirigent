//! The live work line (`claude-opus-5-5 low · Working for 12s · 1 tool call`) as one run
//! of gray text that a front consumes in the working colour over ten seconds, waits a
//! second, consumes back to gray over ten, waits a second, and repeats. The only motion is
//! the chars turning, one after the other. The phase is locked to gpui's shared clock
//! (`repeat_synced`), so the work line getting its persisted key mid-run doesn't restart
//! a pass; under reduced motion gpui renders it still.

use crate::theme::{self, mix};
use gpui::{Animation, AnimationExt, AnyElement, HighlightStyle, StyledText, div, prelude::*};
use std::time::Duration;

/// One pass of the front along the line takes this long...
const PASS: f32 = 10.;
/// ...and it waits this long before the next; a cycle is two of each.
const WAIT: f32 = 1.;
/// Over how many chars a char turns as the front crosses it.
const EDGE: f32 = 1.5;

/// `text` animated; `key` tells the work lines apart.
pub fn line(key: usize, text: String) -> AnyElement {
    let t = theme::t();
    let n = text.chars().count() as f32;
    let cycle = Duration::from_secs_f32(2. * (PASS + WAIT));
    div()
        .min_w_0()
        .truncate()
        .with_animation(("working", key), Animation::new(cycle).repeat_synced(), move |d, v| {
            // Seconds into the cycle: the first pass, its wait, the pass back, its wait.
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

/// Smoothstep: 0..1 eased at both ends, clamped.
fn ease(x: f32) -> f32 {
    let x = x.clamp(0., 1.);
    x * x * (3. - 2. * x)
}
