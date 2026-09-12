//! Usage from retained thread session entries, including cached branches.
use crate::{
    app::Dirigent,
    theme::{self, rgb},
};
use chrono::{DateTime, Local, Utc};
use gpui::{
    AnyElement, Context, IntoElement, PathBuilder, canvas, div, point, prelude::*, px, relative,
    svg,
};
use serde_json::Value;
use std::{
    cell::Cell,
    collections::{BTreeMap, HashSet},
    rc::Rc,
};

#[derive(Clone)]
struct Sample {
    at: i64,
    model: String,
    tokens: u64,
    cost: f64,
}
pub(crate) struct UsagePage {
    samples: Vec<Sample>,
    days: i64,
    loading: bool,
    unreadable: usize,
    request: std::time::Instant,
    hover: Option<(f32, f32)>,
}

struct UsageTooltip(String);
impl gpui::Render for UsageTooltip {
    fn render(&mut self, _: &mut gpui::Window, _: &mut Context<Self>) -> impl IntoElement {
        div()
            .p_3()
            .rounded_md()
            .bg(rgb(theme::menu_bg()))
            .text_sm()
            .text_color(rgb(theme::theme_text()))
            .child(self.0.clone())
    }
}

fn collect(entries: impl IntoIterator<Item = Value>) -> Vec<Sample> {
    let mut seen = HashSet::new();
    entries
        .into_iter()
        .filter_map(|entry| {
            let message = entry.get("message")?;
            if message.get("role")?.as_str()? != "assistant" {
                return None;
            }
            let usage = message.get("usage")?;
            let at = message
                .get("timestamp")
                .and_then(Value::as_i64)
                .map(|ms| ms / 1000)
                .or_else(|| {
                    entry
                        .get("timestamp")?
                        .as_str()
                        .and_then(|s| DateTime::parse_from_rfc3339(s).ok())
                        .map(|t| t.timestamp())
                })?;
            // Forks retain entry IDs: count the shared prefix only once.
            let key = entry
                .get("id")
                .map(Value::to_string)
                .unwrap_or_else(|| message.to_string());
            if !seen.insert(key) {
                return None;
            }
            let tokens = ["input", "output", "cacheRead", "cacheWrite"]
                .iter()
                .map(|key| usage.get(key).and_then(Value::as_u64).unwrap_or(0))
                .sum();
            Some(Sample {
                at,
                model: format!(
                    "{} / {}",
                    message
                        .get("provider")
                        .and_then(Value::as_str)
                        .unwrap_or("unknown"),
                    message
                        .get("model")
                        .and_then(Value::as_str)
                        .unwrap_or("unknown")
                ),
                tokens,
                cost: usage
                    .pointer("/cost/total")
                    .and_then(Value::as_f64)
                    .filter(|n| n.is_finite())
                    .unwrap_or(0.0),
            })
        })
        .collect()
}
fn number(n: u64) -> String {
    if n >= 1_000_000_000 {
        format!("{:.2}B", n as f64 / 1e9)
    } else if n >= 1_000_000 {
        format!("{:.2}M", n as f64 / 1e6)
    } else if n >= 1000 {
        format!("{:.1}K", n as f64 / 1000.)
    } else {
        n.to_string()
    }
}
fn date(at: i64, format: &str) -> String {
    DateTime::from_timestamp(at, 0)
        .map(|t| t.with_timezone(&Local).format(format).to_string())
        .unwrap_or_else(|| "Unknown".into())
}

/// Sum into evenly spaced intervals, including idle periods. Extend the first
/// and last interval values to the edges rather than inventing a drop to zero.
fn interval_points(
    usage: &BTreeMap<i64, u64>,
    start: i64,
    end: i64,
    interval: i64,
) -> Vec<(f32, u64)> {
    let count = ((end - start) / interval) as usize;
    let mut totals = vec![0; count];
    for (&at, &tokens) in usage.range(start..=end) {
        let index = (((at - start) / interval) as usize).min(count - 1);
        totals[index] += tokens;
    }
    let mut points = Vec::with_capacity(count + 2);
    points.push((0., totals[0]));
    points.extend(
        totals
            .iter()
            .enumerate()
            .map(|(index, &tokens)| ((index as f32 + 0.5) / count as f32, tokens)),
    );
    points.push((1., totals[count - 1]));
    points
}

/// Smooth each interval without overshooting its measured values (or dipping below zero).
fn trace_series(path: &mut PathBuilder, points: &[gpui::Point<gpui::Pixels>]) {
    path.move_to(points[0]);
    for pair in points.windows(2) {
        let [a, b] = [pair[0], pair[1]];
        let mid = (a.x + b.x) / 2.;
        path.cubic_bezier_to(b, point(mid, a.y), point(mid, b.y));
    }
}

fn usage_chart(
    series: Vec<Vec<(f32, u64)>>,
    colors: [u32; 5],
    max: u64,
    chart_bounds: Rc<Cell<gpui::Bounds<gpui::Pixels>>>,
) -> impl IntoElement {
    canvas(
        move |bounds, _, _| {
            chart_bounds.set(bounds);
        },
        move |bounds, _, window, _| {
            let top = bounds.top() + px(10.);
            let bottom = bounds.bottom() - px(2.);
            let height = bottom - top;
            for step in 0..=4 {
                let y = top + height * (step as f32 / 4.);
                let mut grid = PathBuilder::stroke(px(1.));
                grid.move_to(point(bounds.left(), y));
                grid.line_to(point(bounds.right(), y));
                if let Ok(path) = grid.build() {
                    window.paint_path(path, rgb(theme::border()).opacity(0.55));
                }
            }
            for (i, values) in series.iter().enumerate() {
                if values.is_empty() {
                    continue;
                }
                let points = values
                    .iter()
                    .map(|(x, value)| {
                        point(
                            bounds.left() + bounds.size.width * *x,
                            bottom - height * (*value as f32 / max as f32),
                        )
                    })
                    .collect::<Vec<_>>();
                let color = rgb(colors[i % colors.len()]);
                let mut fill = PathBuilder::fill();
                trace_series(&mut fill, &points);
                fill.line_to(point(points.last().unwrap().x, bottom));
                fill.line_to(point(points[0].x, bottom));
                fill.close();
                if let Ok(path) = fill.build() {
                    window.paint_path(path, color.opacity(0.10));
                }
                let mut line = PathBuilder::stroke(px(2.));
                trace_series(&mut line, &points);
                if let Ok(path) = line.build() {
                    window.paint_path(path, color);
                }
            }
        },
    )
    .size_full()
}

// Invert the curve's x coordinate so the hover dot lies on the actual Bézier,
// rather than a straight interpolation between its endpoints.
fn curve_value(values: &[(f32, u64)], x: f32) -> f32 {
    if values.is_empty() {
        return 0.;
    }
    if values.len() == 1 {
        return values[0].1 as f32;
    }
    let index = values
        .partition_point(|p| p.0 < x)
        .saturating_sub(1)
        .min(values.len() - 2);
    let width = values[index + 1].0 - values[index].0;
    let fraction = if width > 0. {
        ((x - values[index].0) / width).clamp(0., 1.)
    } else {
        0.
    };
    let (mut low, mut high) = (0., 1.);
    for _ in 0..24 {
        let t = (low + high) / 2.;
        let curve_x = 1.5 * t - 1.5 * t * t + t * t * t;
        if curve_x < fraction {
            low = t;
        } else {
            high = t;
        }
    }
    let t = (low + high) / 2.;
    let weight = t * t * (3. - 2. * t);
    values[index].1 as f32 * (1. - weight) + values[index + 1].1 as f32 * weight
}

impl Dirigent {
    fn render_usage_chart(
        &self,
        series: Vec<Vec<(f32, u64)>>,
        names: &[String],
        colors: [u32; 5],
        max: u64,
        start: i64,
        end: i64,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let bounds = Rc::new(Cell::new(gpui::Bounds::default()));
        let mouse_bounds = bounds.clone();
        let hovered = self
            .usage
            .as_ref()
            .and_then(|page| page.hover)
            .and_then(|(x, mouse_y)| {
                series
                    .iter()
                    .enumerate()
                    .map(|(index, values)| {
                        let value = curve_value(values, x);
                        let y = (10. + 248. * (1. - value / max as f32)) / 260.;
                        (index, x, y, value)
                    })
                    .min_by(|a, b| (a.2 - mouse_y).abs().total_cmp(&(b.2 - mouse_y).abs()))
            });
        div()
            .id("usage-chart")
            .relative()
            .h(px(260.))
            .w_full()
            .on_mouse_move(
                cx.listener(move |this, event: &gpui::MouseMoveEvent, _, cx| {
                    let bounds = mouse_bounds.get();
                    if bounds.size.width <= px(0.) || bounds.size.height <= px(0.) {
                        return;
                    }
                    let hover = bounds.contains(&event.position).then(|| {
                        (
                            ((event.position.x - bounds.left()) / bounds.size.width).clamp(0., 1.),
                            ((event.position.y - bounds.top()) / bounds.size.height).clamp(0., 1.),
                        )
                    });
                    if let Some(page) = &mut this.usage {
                        if page.hover != hover {
                            page.hover = hover;
                            cx.notify();
                        }
                    }
                }),
            )
            .on_hover(cx.listener(|this, hovered, _, cx| {
                if !hovered {
                    if let Some(page) = &mut this.usage {
                        page.hover = None;
                    }
                    cx.notify();
                }
            }))
            .child(usage_chart(series, colors, max, bounds))
            .when_some(hovered, |el, (index, x, y, value)| {
                let at = start + ((end - start) as f32 * x) as i64;
                el.child(
                    div()
                        .absolute()
                        .left(relative(x))
                        .top(relative(y))
                        .child(
                            div()
                                .absolute()
                                .left(px(-4.))
                                .top(px(-4.))
                                .size(px(8.))
                                .rounded_full()
                                .border_2()
                                .border_color(rgb(theme::bg()))
                                .bg(rgb(colors[index % colors.len()])),
                        )
                        .child(
                            div()
                                .absolute()
                                .w(px(240.))
                                .p_3()
                                .rounded_md()
                                .shadow_lg()
                                .border_1()
                                .border_color(rgb(theme::border()))
                                .bg(rgb(theme::menu_bg()))
                                .text_xs()
                                .text_color(rgb(theme::theme_text()))
                                .when(x <= 0.5, |el| el.left(px(12.)))
                                .when(x > 0.5, |el| el.right(px(12.)))
                                .when(y < 0.5, |el| el.top(px(12.)))
                                .when(y >= 0.5, |el| el.bottom(px(12.)))
                                .child(
                                    div()
                                        .text_color(rgb(theme::muted()))
                                        .child(date(at, "%b %d, %H:%M")),
                                )
                                .child(div().child(names[index].clone()))
                                .child(
                                    div().child(format!(
                                        "≈ {} tokens",
                                        number(value.round() as u64)
                                    )),
                                ),
                        ),
                )
            })
            .into_any_element()
    }

    pub(crate) fn open_usage(&mut self, cx: &mut Context<Self>) {
        self.settings = None;
        self.about_open = false;
        self.sidebar_menu = None;
        self.enter_normal_mode();
        let days = self.usage.as_ref().map_or(7, |p| p.days);
        let request = std::time::Instant::now();
        self.usage = Some(UsagePage {
            samples: vec![],
            days,
            loading: true,
            unreadable: 0,
            request,
            hover: None,
        });
        self.refresh_subscription_usage();
        cx.spawn(async move |this, cx| {
            loop {
                cx.background_executor()
                    .timer(std::time::Duration::from_secs(10))
                    .await;
                let active = this
                    .update(cx, |this, _| {
                        // Closing or reopening the page invalidates this polling loop.
                        if !this
                            .usage
                            .as_ref()
                            .is_some_and(|page| page.request == request)
                        {
                            return false;
                        }
                        this.refresh_subscription_usage();
                        true
                    })
                    .unwrap_or(false);
                if !active {
                    break;
                }
            }
        })
        .detach();
        let sources = self
            .harnesses
            .iter()
            .map(|h| (h.session_file.clone(), h.cached_entries.clone()))
            .collect::<Vec<_>>();
        let task = cx.background_executor().spawn(async move {
            let mut entries = Vec::new();
            let mut unreadable = 0;
            for (path, cached) in sources {
                if let Some(cached) = cached {
                    entries.extend(cached.iter().cloned());
                }
                if let Some(path) = path {
                    match std::fs::read_to_string(path) {
                        Ok(text) => {
                            for line in text.lines().filter(|l| !l.trim().is_empty()) {
                                match serde_json::from_str(line) {
                                    Ok(v) => entries.push(v),
                                    Err(_) => unreadable += 1,
                                }
                            }
                        }
                        Err(_) => unreadable += 1,
                    }
                }
            }
            (collect(entries), unreadable)
        });
        cx.spawn(async move |this, cx| {
            let (samples, unreadable) = task.await;
            let _ = this.update(cx, |this, cx| {
                if let Some(page) = &mut this.usage {
                    if page.request != request {
                        return;
                    }
                    page.samples = samples;
                    page.unreadable = unreadable;
                    page.loading = false;
                    cx.notify();
                }
            });
        })
        .detach();
        cx.notify();
    }

    pub(super) fn render_usage(&self, cx: &mut Context<Self>) -> AnyElement {
        let page = self.usage.as_ref().unwrap();
        let end = Utc::now().timestamp();
        let start = end - page.days * 86400;
        let mut models: BTreeMap<String, (BTreeMap<i64, u64>, u64, f64)> = BTreeMap::new();
        for s in page.samples.iter().filter(|s| s.at >= start && s.at <= end) {
            let row = models
                .entry(s.model.clone())
                .or_insert_with(|| (BTreeMap::new(), 0, 0.));
            *row.0.entry(s.at).or_default() += s.tokens;
            row.1 += s.tokens;
            row.2 += s.cost;
        }
        let colors = [
            theme::blue(),
            theme::orange(),
            theme::accent(),
            theme::yellow(),
            theme::muted(),
        ];
        let (interval, interval_label) = match page.days {
            1 => (20 * 60, "20 minutes"),
            7 => (60 * 60, "hour"),
            _ => (24 * 60 * 60, "day"),
        };
        let series = models
            .values()
            .map(|row| interval_points(&row.0, start, end, interval))
            .collect::<Vec<_>>();
        let max = series
            .iter()
            .flatten()
            .map(|point| point.1)
            .max()
            .unwrap_or(0)
            .max(1);
        let total_tokens = models.values().map(|r| r.1).sum();
        let cost: f64 = models.values().map(|r| r.2).sum();
        let control_height = px(28.);
        let chart = self.render_usage_chart(
            series,
            &models.keys().cloned().collect::<Vec<_>>(),
            colors,
            max,
            start,
            end,
            cx,
        );
        div().id("usage-scroll").flex_1().h_full().overflow_y_scroll().p_8().bg(rgb(theme::bg()))
            .child(div().max_w(px(1100.)).mx_auto().flex().flex_col().gap_6()
                .child(div().flex().items_center().gap_4()
                    .child(div().flex_1().text_xl().child("Usage"))
                    .child(div().flex().items_center().gap_2().text_xs().text_color(rgb(theme::muted()))
                        .child(div().h(control_height).flex().p(px(2.)).border_1().border_color(rgb(theme::border())).rounded_md()
                            .children([1, 7, 30].into_iter().map(|days| div().id(("usage-range", days as usize)).h_full().flex().items_center().justify_center().px_2().rounded_sm().cursor_pointer()
                                .bg(rgb(if days == page.days { theme::surface_hover() } else { theme::bg() }))
                                .hover(|s| s.bg(rgb(theme::surface_hover())))
                                .on_click(cx.listener(move |this, _, _, cx| { if let Some(p) = &mut this.usage { p.days = days; p.hover = None; } cx.notify(); }))
                                .child(format!("{days}d")))))
                        .child(div().id("usage-refresh").size(control_height).flex().items_center().justify_center().rounded_md()
                            .border_1().border_color(rgb(theme::border())).cursor_pointer()
                            .hover(|s| s.bg(rgb(theme::surface_hover())))
                            .tooltip(|_, cx| cx.new(|_| UsageTooltip("Refresh usage".into())).into())
                            .child(svg().path("icon/refresh-cw.svg").size(px(14.)).text_color(rgb(theme::muted())))
                            .on_click(cx.listener(|this, _, _, cx| this.open_usage(cx))))
                        .child(div().id("usage-close").h(control_height).flex().items_center().justify_center().px_2().rounded_md().cursor_pointer()
                            .hover(|s| s.bg(rgb(theme::surface_hover())))
                            .child("Close").on_click(cx.listener(|this, _, _, cx| { this.usage = None; cx.notify(); })))))
                .child(div().text_sm().text_color(rgb(theme::muted())).child(format!("{} — {}", date(start, "%b %d, %H:%M"), date(end, "%b %d, %H:%M"))))
                .child(div().flex().gap_4().children([("5-hour limit", self.codex_usage.and_then(|u| u.five_hour)), ("Weekly limit", self.codex_usage.and_then(|u| u.weekly))].into_iter().map(|(label, limit)| {
                    div().flex_1().p_4().border_1().border_color(rgb(theme::border())).rounded_lg().flex().flex_col().gap_2()
                        .child(label)
                        .child(limit.map_or_else(|| "Unavailable".into(), |l| format!("{:.0}% used", l.used_percent)))
                        .child(div().h(px(5.)).w_full().rounded_full().bg(rgb(theme::border())).child(div().h_full().w(relative(limit.map_or(0., |l| (l.used_percent / 100.).clamp(0., 1.) as f32))).bg(rgb(theme::accent())).rounded_full()))
                        .child(div().text_xs().text_color(rgb(theme::muted())).child(limit.and_then(|l| l.resets_at).map_or_else(|| "Reset time unavailable".into(), |t| format!("Resets {}", date(t as i64, "%b %d, %H:%M %Z")))))
                })))
                .child(div().text_xs().text_color(rgb(theme::muted())).child(format!("Codex subscription · ${cost:.2} · {} tokens", number(total_tokens))))
                .when(page.loading, |el| el.child("Loading history…"))
                .when(page.unreadable > 0, |el| el.child("Some session files or entries could not be read; history may be incomplete."))
                .when(!page.loading && models.is_empty(), |el| el.child("No recorded token usage in this range."))
                .child(div().text_sm().child(format!("Tokens per {interval_label} · peak {}", number(max))))
                .child(chart)
                .child(div().flex().justify_between().text_xs().text_color(rgb(theme::muted())).child(date(start, "%b %d %H:%M")).child(date(end, "%b %d %H:%M")))
                .child(div().flex().justify_between().text_sm().child("Model").child("Tokens / Cost"))
                .children(models.iter().enumerate().map(|(i, (name, row))| div().flex().justify_between().py_2().border_b_1().border_color(rgb(theme::border()))
                    .child(div().flex().gap_2().items_center().child(div().size(px(8.)).rounded_full().bg(rgb(colors[i % colors.len()]))).child(name.clone()))
                    .child(format!("{} / ${:.2}", number(row.1), row.2))))
            ).into_any_element()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn intervals_sum_usage_preserve_idle_periods_and_extend_to_edges() {
        let usage = BTreeMap::from([(0, 10), (1199, 20), (1200, 40), (3600, 50)]);
        let points = interval_points(&usage, 0, 4800, 1200);
        assert_eq!(
            points,
            vec![
                (0., 30),
                (0.125, 30),
                (0.375, 40),
                (0.625, 0),
                (0.875, 50),
                (1., 50)
            ]
        );
        assert_eq!(
            points[1..points.len() - 1].iter().map(|p| p.1).sum::<u64>(),
            120
        );
        let at_end = interval_points(&BTreeMap::from([(4800, 7)]), 0, 4800, 1200);
        assert_eq!(at_end.last(), Some(&(1., 7)));
    }

    #[test]
    fn hover_follows_timestamped_curve_without_overshooting() {
        let points = [(0., 0), (0.2, 100), (1., 20)];
        assert!((curve_value(&points, 0.) - 0.).abs() < 0.001);
        assert!((curve_value(&points, 0.2) - 100.).abs() < 0.001);
        assert!((curve_value(&points, 1.) - 20.).abs() < 0.001);
        assert!((curve_value(&points, 0.1) - 50.).abs() < 0.001);
        assert!((curve_value(&points, 0.6) - 60.).abs() < 0.001);
        for step in 0..=100 {
            assert!((0. ..=100.).contains(&curve_value(&points, step as f32 / 100.)));
        }
    }

    #[test]
    fn counts_cached_tokens_and_deduplicates_forks() {
        let entry = serde_json::json!({"id":"a", "timestamp":"2026-01-01T00:00:00Z", "message":{"role":"assistant", "provider":"p", "model":"m", "usage":{"input":10,"output":5,"cacheRead":20,"cacheWrite":3,"cost":{"total":0.25}}}});
        let samples = collect([entry.clone(), entry]);
        assert_eq!(samples.len(), 1);
        assert_eq!(samples[0].tokens, 38);
        assert_eq!(samples[0].cost, 0.25);
        assert_eq!(samples[0].at, 1767225600);
    }
}
