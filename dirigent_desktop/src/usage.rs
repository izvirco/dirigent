//! Settings › Usage: what the agents cost over time, across every host.
//!
//! The page is one big chart of cost over time: a smoothed line per series (a model, a
//! provider, or everything, see `Group`), the area under each faintly filled. Hovering
//! it puts a crosshair on the slot under the mouse with a card beside the cursor that
//! splits every line's cost into input / output tokens and the cached share. The legend
//! under the chart is a row per series with its totals for the window; click one to
//! hide its line.
//!
//! Data comes from every host's `ListUsage` for the window shown, already summed per
//! slot and model; `Model` folds the hosts together and groups the series. The window is
//! the last day / week / month ending now (`‹ ›` page it back and forth), or a custom
//! pair of dates. The slot width follows the window so a line has ~50 points.

use crate::app::{Dirigent, Menu, Pane};
use crate::input::TextInput;
use crate::menu::{self, Side};
use crate::theme::{self, dollars, kilo};
use chrono::{Local, NaiveDate, TimeZone};
use dirigent_protocol::UsageBucket;
use gpui::{
    Anchor, AnyElement, Bounds, Context, Div, Entity, FontWeight, Hsla, MouseMoveEvent, PathBuilder, Pixels, Point, SharedString, Stateful,
    TextAlign, TextRun, Window, anchored, canvas, deferred, div, fill, linear_color_stop, linear_gradient, point, prelude::*, px, rgba,
    size,
};
use std::cell::RefCell;
use std::collections::{HashMap, HashSet};
use std::rc::Rc;

const MINUTE: i64 = 60_000;
const HOUR: i64 = 60 * MINUTE;
const DAY: i64 = 24 * HOUR;
/// Slot widths a window can be cut into, smallest first.
const BUCKETS: [i64; 11] = [15 * MINUTE, 30 * MINUTE, HOUR, 2 * HOUR, 3 * HOUR, 6 * HOUR, 12 * HOUR, DAY, 2 * DAY, 7 * DAY, 30 * DAY];
/// A line gets at most this many points.
const MAX_SLOTS: i64 = 60;
/// Distances between time labels, smallest first.
const TICKS: [i64; 10] = [HOUR, 3 * HOUR, 6 * HOUR, 12 * HOUR, DAY, 2 * DAY, 7 * DAY, 14 * DAY, 30 * DAY, 90 * DAY];
const TICK_MIN_PX: f32 = 90.;
/// Series colours, assigned by rank (biggest spender first).
fn palette() -> [u32; 6] {
    let t = theme::t();
    [t.accent_ink, t.done, t.warn, 0xb48eadff, 0x6fb3c9ff, t.error]
}
/// Plot geometry: the room over it for the scale's top label, the time axis under it.
const BAND: f32 = 18.;
const AXIS: f32 = 22.;
const LINE_W: f32 = 1.5;
/// Horizontal gridlines, as fractions of the scale.
const GRID: [f64; 4] = [0.25, 0.5, 0.75, 1.];

#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub enum Range {
    Day,
    #[default]
    Week,
    Month,
    Custom,
}

impl Range {
    fn span(self) -> Option<i64> {
        match self {
            Range::Day => Some(DAY),
            Range::Week => Some(7 * DAY),
            Range::Month => Some(30 * DAY),
            Range::Custom => None,
        }
    }

    fn label(self) -> &'static str {
        match self {
            Range::Day => "1d",
            Range::Week => "7d",
            Range::Month => "30d",
            Range::Custom => "custom",
        }
    }
}

/// What a series is: one line per model, per provider, or one for everything.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub enum Group {
    #[default]
    Model,
    Provider,
    All,
}

impl Group {
    pub fn label(self) -> &'static str {
        match self {
            Group::Model => "by model",
            Group::Provider => "by provider",
            Group::All => "everything",
        }
    }
}

/// The page's state, on `Dirigent` (the custom range's date inputs live beside it there).
#[derive(Default)]
pub struct State {
    pub range: Range,
    /// The window shown, unix ms; `until` is exclusive.
    pub since: i64,
    pub until: i64,
    pub group: Group,
    /// Series keys the user clicked off in the legend.
    pub hidden: HashSet<String>,
    /// Each host's latest answer. Kept across the periodic refresh so the chart never
    /// blanks: a reply to `request` swaps in that host's new buckets. Dropped only when
    /// the window changes, since stale buckets would then be from a different window.
    pub buckets: HashMap<String, Vec<UsageBucket>>,
    pub request: Option<u64>,
    /// The mouse over the chart (window px) and the plot's bounds from its last paint.
    pub hover: Option<Point<Pixels>>,
    plot: Rc<RefCell<Bounds<Pixels>>>,
}

impl State {
    pub fn new() -> Self {
        let mut s = Self::default();
        s.set_range(Range::Week);
        s
    }

    /// The last `range` ending now (its live slot included).
    pub fn set_range(&mut self, range: Range) {
        let Some(span) = range.span() else {
            self.range = range;
            return;
        };
        self.range = range;
        self.until = align(now_ms(), bucket_for(span)) + bucket_for(span);
        self.since = self.until - span;
        self.buckets.clear();
    }

    /// Shift the window by its own length; forward stops at now.
    pub fn page(&mut self, dir: i64) {
        let span = self.until - self.since;
        let now = now_ms();
        if dir > 0 && self.until > now {
            return;
        }
        self.since += dir * span;
        self.until += dir * span;
        self.buckets.clear();
        if self.until > now {
            self.set_range(self.range);
        }
    }

    /// `from` and `to` are local calendar days, both inclusive.
    pub fn set_custom(&mut self, from: NaiveDate, to: NaiveDate) {
        let (from, to) = if from <= to { (from, to) } else { (to, from) };
        self.range = Range::Custom;
        self.since = midnight(from);
        self.until = midnight(to) + DAY;
        self.buckets.clear();
    }

    pub fn bucket(&self) -> i64 {
        bucket_for(self.until - self.since)
    }

    pub fn at_now(&self) -> bool {
        self.until > now_ms()
    }
}

/// The smallest slot width that keeps a window under `MAX_SLOTS` points.
fn bucket_for(span: i64) -> i64 {
    BUCKETS.into_iter().find(|b| span / b <= MAX_SLOTS).unwrap_or(BUCKETS[BUCKETS.len() - 1])
}

fn now_ms() -> i64 {
    std::time::UNIX_EPOCH.elapsed().map(|d| d.as_millis() as i64).unwrap_or(0)
}

fn local(ms: i64) -> chrono::DateTime<Local> {
    Local.timestamp_millis_opt(ms).single().unwrap_or_else(|| Local.timestamp_millis_opt(0).unwrap())
}

/// `t` rounded down to a multiple of `step` in local time, so hours start at :00 and
/// days at midnight.
fn align(t: i64, step: i64) -> i64 {
    let off = local(t).offset().local_minus_utc() as i64 * 1000;
    (t + off).div_euclid(step) * step - off
}

/// The local calendar day of `ms`, as the date fields show it.
pub fn day_of(ms: i64) -> String {
    local(ms).format("%Y-%m-%d").to_string()
}

pub fn parse_day(s: &str) -> Option<NaiveDate> {
    NaiveDate::parse_from_str(s.trim(), "%Y-%m-%d").ok()
}

fn midnight(d: NaiveDate) -> i64 {
    let dt = d.and_hms_opt(0, 0, 0).unwrap();
    Local.from_local_datetime(&dt).earliest().map(|t| t.timestamp_millis()).unwrap_or(0)
}

/// The sums shown for a series or a slot.
#[derive(Default, Clone, Copy, Debug, PartialEq)]
pub struct Totals {
    /// Prompt tokens: fresh input + cache read + cache write.
    pub input: u64,
    pub output: u64,
    pub cached: u64,
    pub cost: f64,
    pub messages: u64,
}

impl Totals {
    fn add(&mut self, b: &UsageBucket) {
        self.sum(&Totals {
            input: b.input + b.cache_read + b.cache_write,
            output: b.output,
            cached: b.cache_read,
            cost: b.cost,
            messages: b.messages,
        });
    }

    fn sum(&mut self, o: &Totals) {
        self.input += o.input;
        self.output += o.output;
        self.cached += o.cached;
        self.cost += o.cost;
        self.messages += o.messages;
    }

    /// Share of the prompt tokens that came from cache, `93%`.
    fn cached_pct(&self) -> String {
        if self.input == 0 { "–".into() } else { format!("{}%", (self.cached as f64 / self.input as f64 * 100.).round()) }
    }

    /// `1.7M in · 15k out · 98% cached`: the split the hover card shows per line.
    fn split(&self) -> String {
        format!("{} in · {} out · {} cached", kilo(self.input), kilo(self.output), self.cached_pct())
    }
}

struct Series {
    key: String,
    label: String,
    /// The provider, when grouping by model.
    detail: String,
    color: u32,
    hidden: bool,
    slots: Vec<Totals>,
    total: Totals,
}

/// Every host's buckets folded into series for the current grouping, ranked by cost.
struct Model {
    slots: usize,
    series: Vec<Series>,
    /// Over the visible series only: the headline follows the filter.
    total: Totals,
}

impl Model {
    fn build(s: &State) -> Self {
        let bucket = s.bucket();
        let slots = ((s.until - s.since + bucket - 1) / bucket).max(1) as usize;
        let mut by_key: HashMap<String, Series> = HashMap::new();
        for b in s.buckets.values().flatten() {
            let (key, label, detail) = match s.group {
                Group::Model => (format!("{}/{}", b.provider, b.model), b.model.clone(), b.provider.clone()),
                Group::Provider => (b.provider.clone(), b.provider.clone(), String::new()),
                Group::All => ("all".to_owned(), "everything".to_owned(), String::new()),
            };
            let series = by_key.entry(key.clone()).or_insert_with(|| Series {
                hidden: s.hidden.contains(&key),
                key,
                label,
                detail,
                color: 0,
                slots: vec![Totals::default(); slots],
                total: Totals::default(),
            });
            let i = ((b.start - s.since) / bucket).clamp(0, slots as i64 - 1) as usize;
            series.slots[i].add(b);
            series.total.add(b);
        }
        let palette = palette();
        let mut series: Vec<Series> = by_key.into_values().collect();
        series.sort_by(|a, b| b.total.cost.total_cmp(&a.total.cost).then_with(|| a.key.cmp(&b.key)));
        for (i, s) in series.iter_mut().enumerate() {
            s.color = palette[i % palette.len()];
        }
        let mut total = Totals::default();
        for s in series.iter().filter(|s| !s.hidden) {
            total.sum(&s.total);
        }
        Self { slots, series, total }
    }

    fn visible(&self) -> impl Iterator<Item = &Series> {
        self.series.iter().filter(|s| !s.hidden)
    }

    /// Top of a strip's scale: the biggest visible slot value, rounded up to 1 / 2 / 5 × 10ⁿ.
    fn scale(&self) -> f64 {
        let max = self.visible().flat_map(|s| s.slots.iter().map(|t| t.cost)).fold(0., f64::max);
        if max <= 0. {
            return 1.;
        }
        let mag = 10f64.powf(max.log10().floor());
        let n = max / mag;
        mag * if n <= 1. {
            1.
        } else if n <= 2. {
            2.
        } else if n <= 5. {
            5.
        } else {
            10.
        }
    }
}

/// Where the plot lands inside the canvas bounds; shared by the paint and the hover hit test.
struct Layout {
    bounds: Bounds<Pixels>,
    /// The lines' region: under the scale label's band, over the time axis.
    plot: Bounds<Pixels>,
}

impl Layout {
    fn new(bounds: Bounds<Pixels>) -> Self {
        let h = (f32::from(bounds.size.height) - AXIS - BAND).max(0.);
        let plot = Bounds::new(point(bounds.left(), bounds.top() + px(BAND)), size(bounds.size.width, px(h)));
        Self { bounds, plot }
    }

    /// Centre x of slot `i` of `n`.
    fn x(&self, i: usize, n: usize) -> Pixels {
        self.bounds.left() + self.bounds.size.width * ((i as f32 + 0.5) / n as f32)
    }

    fn slot_at(&self, x: Pixels, n: usize) -> Option<usize> {
        let f = f32::from(x - self.bounds.left()) / f32::from(self.bounds.size.width);
        (0. ..1.).contains(&f).then(|| ((f * n as f32) as usize).min(n - 1))
    }
}

pub fn render(this: &Dirigent, pane: Div, cx: &mut Context<Dirigent>) -> Div {
    let s = &this.usage;
    let model = Rc::new(Model::build(s));
    let hover_slot = s.hover.and_then(|p| Layout::new(*s.plot.borrow()).slot_at(p.x, model.slots));
    let body = div()
        .flex_1()
        .min_h_0()
        .flex()
        .flex_col()
        .px_6()
        .pt_5()
        .pb_4()
        .child(hero(this, &model, cx))
        .child(chart(this, model.clone(), hover_slot, cx))
        .child(legend(&model, cx));
    pane.child(crate::lists::settings_header(Pane::Usage, cx)).child(body)
}

/// The headline — what the window cost — and, on the right, the window and grouping controls.
fn hero(this: &Dirigent, model: &Model, cx: &mut Context<Dirigent>) -> Div {
    let s = &this.usage;
    let total = dollars(model.total.cost);
    let (whole, cents) = total.split_once('.').map(|(w, c)| (w.to_owned(), format!(".{c}"))).unwrap_or((total.clone(), String::new()));
    let messages = match model.total.messages {
        0 => "no messages".to_owned(),
        1 => "1 message".to_owned(),
        n => format!("{n} messages"),
    };
    let headline = div()
        .flex()
        .flex_col()
        .gap_1()
        .child(
            div()
                .flex()
                .flex_row()
                .items_baseline()
                .text_size(px(34.))
                .line_height(px(40.))
                .font_weight(FontWeight::MEDIUM)
                .child(whole)
                .child(div().text_color(rgba(theme::t().fg_1)).child(cents)),
        )
        .child(
            div()
                .text_size(px(12.))
                .text_color(rgba(theme::t().fg_1))
                .child(format!("{}, {messages}", window_label(s.since, s.until, s.bucket()))),
        );

    let seg = |r: Range, cx: &mut Context<Dirigent>| {
        let active = s.range == r;
        div()
            .id(SharedString::from(format!("range/{}", r.label())))
            .h(px(24.))
            .px_2()
            .flex()
            .items_center()
            .rounded_md()
            .cursor_pointer()
            .text_size(px(12.))
            .text_color(rgba(if active { theme::t().fg } else { theme::t().fg_1 }))
            .when(active, |d| d.bg(rgba(theme::t().hover)))
            .hover(|d| d.text_color(rgba(theme::t().fg)))
            .on_click(cx.listener(move |this, _, _, cx| this.set_usage_range(r, cx)))
            .child(r.label())
    };
    let ranges = div()
        .flex()
        .flex_row()
        .items_center()
        .p(px(2.))
        .rounded_md()
        .bg(rgba(theme::t().raised))
        .border_1()
        .border_color(rgba(theme::t().line))
        .child(seg(Range::Day, cx))
        .child(seg(Range::Week, cx))
        .child(seg(Range::Month, cx))
        .child(seg(Range::Custom, cx));
    let at_now = s.at_now();
    let pager = div()
        .flex()
        .flex_row()
        .items_center()
        .child(theme::icon_button("usage-back", "chevron-left", cx, |this, cx| this.page_usage(-1, cx)))
        .child(theme::icon_button("usage-fwd", "chevron-right", cx, |this, cx| this.page_usage(1, cx)).when(at_now, |d| d.opacity(0.3)));

    let group_open = this.menu == Some(Menu::UsageGroup);
    let group_menu = group_open.then(|| {
        let items = [Group::Model, Group::Provider, Group::All]
            .into_iter()
            .map(|g| menu::item(format!("g/{}", g.label()), g.label(), "", s.group == g, move |this, cx| this.set_usage_group(g, cx), cx))
            .collect();
        menu::popup(Side::Below(24.), items, cx)
    });
    let group = theme::chip("usage-group", s.group.label(), false, group_open)
        .text_size(px(12.))
        .on_click(cx.listener(|this, _, _, cx| this.toggle_menu(Menu::UsageGroup, cx)))
        .children(group_menu);

    let field = |input: Entity<TextInput>| {
        div()
            .w(px(112.))
            .h(px(26.))
            .px_2()
            .flex()
            .items_center()
            .rounded_md()
            .bg(rgba(theme::t().raised))
            .border_1()
            .border_color(rgba(theme::t().line))
            .text_size(px(12.))
            .child(input)
    };
    let custom = (s.range == Range::Custom).then(|| {
        div()
            .flex()
            .flex_row()
            .items_center()
            .gap_2()
            .text_size(px(12.))
            .text_color(rgba(theme::t().fg_1))
            .child(field(this.usage_from.clone()))
            .child("to")
            .child(field(this.usage_to.clone()))
    });
    let controls = div()
        .flex()
        .flex_col()
        .items_end()
        .gap_2()
        .child(div().flex().flex_row().items_center().gap_2().child(group).child(ranges).child(pager))
        .children(custom);

    div().flex().flex_row().items_start().justify_between().pb_4().child(headline).child(controls)
}

/// `Oct 14 – Oct 20` for day-wide slots, `Oct 20 09:00 – Oct 21 09:00` for finer ones
/// (the date once if both ends fall on the same day).
fn window_label(since: i64, until: i64, bucket: i64) -> String {
    let (a, b) = (local(since), local(until - 1));
    if bucket >= DAY {
        if a.date_naive() == b.date_naive() {
            return a.format("%b %-d").to_string();
        }
        return format!("{} – {}", a.format("%b %-d"), b.format("%b %-d"));
    }
    let b = local(until);
    if a.date_naive() == b.date_naive() {
        format!("{} {} – {}", a.format("%b %-d"), a.format("%H:%M"), b.format("%H:%M"))
    } else {
        format!("{} – {}", a.format("%b %-d %H:%M"), b.format("%b %-d %H:%M"))
    }
}

/// The cost chart in one canvas, with the hover card floated beside the cursor.
fn chart(this: &Dirigent, model: Rc<Model>, hover_slot: Option<usize>, cx: &mut Context<Dirigent>) -> Stateful<Div> {
    let s = &this.usage;
    let (since, bucket) = (s.since, s.bucket());
    let plot = s.plot.clone();
    let paint_model = model.clone();
    let strips = canvas(
        |_, _, _| {},
        move |bounds, _, window, cx| {
            *plot.borrow_mut() = bounds;
            paint(&paint_model, &Layout::new(bounds), since, bucket, hover_slot, window, cx);
        },
    )
    .size_full();
    let empty = (model.total.messages == 0).then(|| {
        let text = if s.request.is_some() && s.buckets.is_empty() { "Loading" } else { "Nothing spent in this window" };
        div().absolute().inset_0().flex().items_center().justify_center().pb(px(AXIS)).text_color(rgba(theme::t().fg_2)).child(text)
    });
    let card = hover_slot.zip(s.hover).map(|(i, p)| readout(&model, i, since, bucket, p, &Layout::new(*s.plot.borrow())));
    div()
        .id("usage-chart")
        .relative()
        .flex_1()
        .min_h(px(260.))
        .w_full()
        .on_mouse_move(cx.listener(|this, ev: &MouseMoveEvent, _, cx| {
            if this.usage.hover != Some(ev.position) {
                this.usage.hover = Some(ev.position);
                cx.notify();
            }
        }))
        .on_hover(cx.listener(|this, hovered, _, cx| {
            if !*hovered && this.usage.hover.take().is_some() {
                cx.notify();
            }
        }))
        .child(strips)
        .children(empty)
        .children(card)
}

/// Paint the chart: gridlines with their dollar labels, time ticks, one smoothed line
/// (and its area) per visible series, and the crosshair with a dot per line on the
/// hovered slot.
fn paint(model: &Model, layout: &Layout, since: i64, bucket: i64, hover: Option<usize>, window: &mut Window, cx: &mut gpui::App) {
    let n = model.slots;
    let line: Hsla = rgba(theme::t().line).into();
    let soft: Hsla = rgba(theme::t().line_soft).into();
    let (bounds, plot) = (layout.bounds, layout.plot);

    // Time ticks: faint verticals through the plot, labels along the bottom.
    let span = bucket * n as i64;
    let px_per_ms = f32::from(bounds.size.width) / span as f32;
    let step = TICKS.into_iter().find(|t| *t as f32 * px_per_ms >= TICK_MIN_PX).unwrap_or(TICKS[TICKS.len() - 1]);
    let mut t = align(since, step);
    while t < since + span {
        if t >= since {
            let x = bounds.left() + px((t - since) as f32 * px_per_ms);
            let fmt = if step < DAY { "%H:%M" } else { "%b %-d" };
            let label = local(t).format(fmt).to_string();
            let w = text_width(&label, 11., window);
            if x - w / 2. >= bounds.left() && x + w / 2. <= bounds.right() {
                text(&label, 11., theme::t().fg_2, point(x - w / 2., bounds.bottom() - px(16.)), window, cx);
            }
            window.paint_quad(fill(Bounds::new(point(x, plot.top()), size(px(1.), plot.size.height)), soft));
        }
        t += step;
    }

    // The scale: a baseline, hairlines at every quarter with the amount at the right.
    let max = model.scale();
    let y_of = |v: f64| plot.bottom() - plot.size.height * (v / max) as f32;
    window.paint_quad(fill(Bounds::new(point(plot.left(), plot.bottom()), size(plot.size.width, px(1.))), line));
    for f in GRID {
        let y = y_of(max * f);
        window.paint_quad(fill(Bounds::new(point(plot.left(), y), size(plot.size.width, px(1.))), soft));
        let label = dollars(max * f);
        let w = text_width(&label, 11., window);
        text(&label, 11., theme::t().fg_2, point(plot.right() - w, y - px(16.)), window, cx);
    }

    // Biggest spender at the back.
    for series in model.visible().collect::<Vec<_>>().into_iter().rev() {
        let pts: Vec<Point<Pixels>> = (0..n).map(|i| point(layout.x(i, n), y_of(series.slots[i].cost))).collect();
        let color: Hsla = rgba(series.color).into();
        if let Some(area) = area_path(&pts, plot.bottom()) {
            let fade = linear_gradient(180., linear_color_stop(color.opacity(0.22), 0.), linear_color_stop(color.opacity(0.), 1.));
            window.paint_path(area, fade);
        }
        if let Some(stroke) = line_path(&pts) {
            window.paint_path(stroke, color);
        }
    }

    if let Some(i) = hover {
        let x = layout.x(i, n);
        window.paint_quad(fill(Bounds::new(point(x, plot.top()), size(px(1.), plot.size.height)), rgba(theme::t().fg_2)));
        for series in model.visible() {
            let c = point(x, y_of(series.slots[i].cost));
            let r = px(3.5);
            window.paint_quad(
                fill(Bounds::new(point(c.x - r, c.y - r), size(r * 2., r * 2.)), rgba(series.color))
                    .corner_radii(r)
                    .border_widths(px(1.5))
                    .border_color(rgba(theme::t().panel)),
            );
        }
    }
}

/// A smooth curve through `pts` (monotone cubic, so it never overshoots the data).
/// Returns the cubic segments as `(to, ctrl_a, ctrl_b)` for each step `i → i+1`.
fn smooth(pts: &[Point<Pixels>]) -> Vec<(Point<Pixels>, Point<Pixels>, Point<Pixels>)> {
    let n = pts.len();
    if n < 2 {
        return vec![];
    }
    let (xs, ys): (Vec<f32>, Vec<f32>) = pts.iter().map(|p| (f32::from(p.x), f32::from(p.y))).unzip();
    // Fritsch–Carlson tangents: secant slopes, zeroed at local extrema, limited elsewhere.
    let d: Vec<f32> = (0..n - 1).map(|i| (ys[i + 1] - ys[i]) / (xs[i + 1] - xs[i]).max(1e-6)).collect();
    let mut m = vec![0f32; n];
    m[0] = d[0];
    m[n - 1] = d[n - 2];
    for i in 1..n - 1 {
        m[i] = if d[i - 1] * d[i] <= 0. { 0. } else { (d[i - 1] + d[i]) / 2. };
    }
    for i in 0..n - 1 {
        if d[i] == 0. {
            m[i] = 0.;
            m[i + 1] = 0.;
        } else {
            let (a, b) = (m[i] / d[i], m[i + 1] / d[i]);
            let s = a * a + b * b;
            if s > 9. {
                let t = 3. / s.sqrt();
                m[i] = t * a * d[i];
                m[i + 1] = t * b * d[i];
            }
        }
    }
    (0..n - 1)
        .map(|i| {
            let dx = (xs[i + 1] - xs[i]) / 3.;
            (pts[i + 1], point(px(xs[i] + dx), px(ys[i] + m[i] * dx)), point(px(xs[i + 1] - dx), px(ys[i + 1] - m[i + 1] * dx)))
        })
        .collect()
}

fn line_path(pts: &[Point<Pixels>]) -> Option<gpui::Path<Pixels>> {
    let segs = smooth(pts);
    if segs.is_empty() {
        return None;
    }
    let mut p = PathBuilder::stroke(px(LINE_W));
    p.move_to(pts[0]);
    for (to, a, b) in segs {
        p.cubic_bezier_to(to, a, b);
    }
    p.build().ok()
}

/// The curve closed down to `base`, for the fill under a line.
fn area_path(pts: &[Point<Pixels>], base: Pixels) -> Option<gpui::Path<Pixels>> {
    let segs = smooth(pts);
    if segs.is_empty() {
        return None;
    }
    let mut p = PathBuilder::fill();
    p.move_to(point(pts[0].x, base));
    p.line_to(pts[0]);
    for (to, a, b) in segs {
        p.cubic_bezier_to(to, a, b);
    }
    p.line_to(point(pts[pts.len() - 1].x, base));
    p.close();
    p.build().ok()
}

fn text_width(s: &str, size: f32, window: &mut Window) -> Pixels {
    shape(s, size, theme::t().fg, window).width
}

fn text(s: &str, size: f32, color: u32, origin: Point<Pixels>, window: &mut Window, cx: &mut gpui::App) {
    let line = shape(s, size, color, window);
    let _ = line.paint(origin, px(size * 1.4), TextAlign::Left, None, window, cx);
}

fn shape(s: &str, size: f32, color: u32, window: &mut Window) -> gpui::ShapedLine {
    let run = TextRun { len: s.len(), font: window.text_style().font(), color: rgba(color).into(), ..Default::default() };
    window.text_system().shape_line(SharedString::from(s.to_owned()), px(size), &[run], None)
}

/// The hovered slot's time and, per visible line, its cost and the split behind it
/// (`1.7M in · 15k out · 98% cached`), floated beside the cursor: to its right on the
/// left half of the plot, to its left on the right half, so it never sits on the line.
fn readout(model: &Model, i: usize, since: i64, bucket: i64, mouse: Point<Pixels>, layout: &Layout) -> AnyElement {
    let start = since + i as i64 * bucket;
    let (a, b) = (local(start), local(start + bucket));
    let when = if bucket >= DAY {
        a.format("%a, %b %-d").to_string()
    } else {
        format!("{} {} – {}", a.format("%b %-d"), a.format("%H:%M"), b.format("%H:%M"))
    };
    let rows = model.visible().map(|s| {
        let t = s.slots[i];
        div()
            .flex()
            .flex_row()
            .items_center()
            .gap_2()
            .child(div().size(px(7.)).rounded_full().bg(rgba(s.color)).flex_shrink_0())
            .child(div().w(px(140.)).truncate().child(s.label.clone()))
            .child(div().w(px(56.)).flex_shrink_0().text_color(rgba(theme::t().fg)).child(dollars(t.cost)))
            .child(div().text_color(rgba(theme::t().fg_1)).child(t.split()))
    });
    let card = div()
        .flex()
        .flex_col()
        .gap_1()
        .px_3()
        .py_2()
        .rounded_md()
        .bg(rgba(theme::t().popup))
        .border_1()
        .border_color(rgba(theme::t().line))
        .shadow_md()
        .text_size(px(12.))
        .line_height(px(17.))
        .child(div().text_color(rgba(theme::t().fg_2)).child(when))
        .children(rows);
    let right_half = mouse.x > layout.plot.left() + layout.plot.size.width / 2.;
    let (anchor, dx) = if right_half { (Anchor::TopRight, px(-16.)) } else { (Anchor::TopLeft, px(16.)) };
    // Zero-size absolute wrapper so the anchored card takes a window position, not the
    // flex static position (see AGENTS.md).
    div()
        .absolute()
        .top_0()
        .left_0()
        .size_0()
        .child(deferred(
            anchored().anchor(anchor).position(point(mouse.x + dx, mouse.y + px(16.))).snap_to_window_with_margin(px(8.)).child(card),
        ))
        .into_any()
}

/// One row per series with its totals for the window; click a row to hide / show its line.
fn legend(model: &Model, cx: &mut Context<Dirigent>) -> Div {
    let col = |w: f32, text: String, color: u32| div().w(px(w)).flex_shrink_0().text_color(rgba(color)).child(text);
    let head = div()
        .flex()
        .flex_row()
        .items_center()
        .gap_3()
        .h(px(24.))
        .text_size(px(11.))
        .text_color(rgba(theme::t().fg_2))
        .child(div().size(px(8.)))
        .child(div().flex_1())
        .child(col(80., "cost".into(), theme::t().fg_2))
        .child(col(80., "in".into(), theme::t().fg_2))
        .child(col(80., "out".into(), theme::t().fg_2))
        .child(col(80., "cached".into(), theme::t().fg_2))
        .child(col(70., "messages".into(), theme::t().fg_2));
    let rows = model.series.iter().map(|s| {
        let key = s.key.clone();
        let fg = if s.hidden { theme::t().fg_2 } else { theme::t().fg };
        let num = if s.hidden { theme::t().fg_2 } else { theme::t().fg_1 };
        let swatch = div().size(px(8.)).rounded_full().flex_shrink_0();
        let swatch = if s.hidden { swatch.border_1().border_color(rgba(s.color)) } else { swatch.bg(rgba(s.color)) };
        let t = s.total;
        div()
            .id(SharedString::from(format!("series/{}", s.key)))
            .flex()
            .flex_row()
            .items_center()
            .gap_3()
            .h(px(30.))
            .px_1()
            .mx(px(-4.))
            .rounded_md()
            .cursor_pointer()
            .border_t_1()
            .border_color(rgba(theme::t().line_soft))
            .hover(|d| d.bg(rgba(theme::t().raised)))
            .on_click(cx.listener(move |this, _, _, cx| this.toggle_usage_series(&key, cx)))
            .child(swatch)
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .flex()
                    .flex_row()
                    .items_baseline()
                    .gap_2()
                    .child(div().truncate().text_color(rgba(fg)).child(s.label.clone()))
                    .when(!s.detail.is_empty(), |d| {
                        d.child(div().text_size(px(11.)).text_color(rgba(theme::t().fg_2)).child(s.detail.clone()))
                    }),
            )
            .child(col(80., dollars(t.cost), fg))
            .child(col(80., kilo(t.input), num))
            .child(col(80., kilo(t.output), num))
            .child(col(80., t.cached_pct(), num))
            .child(col(70., t.messages.to_string(), num))
    });
    // Many models: the list scrolls rather than squeezing the chart.
    div()
        .flex()
        .flex_col()
        .pt_3()
        .text_size(px(12.))
        .child(head)
        .child(div().id("usage-legend").max_h(px(6.5 * 30.)).overflow_y_scroll().flex().flex_col().children(rows))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn buckets_keep_a_line_under_sixty_points() {
        assert_eq!(bucket_for(DAY), 30 * MINUTE);
        assert_eq!(bucket_for(7 * DAY), 3 * HOUR);
        assert_eq!(bucket_for(30 * DAY), 12 * HOUR);
        assert_eq!(bucket_for(365 * DAY), 7 * DAY);
    }

    #[test]
    fn model_folds_hosts_and_ranks_by_cost() {
        let (from, to) = (NaiveDate::from_ymd_opt(2026, 1, 1).unwrap(), NaiveDate::from_ymd_opt(2026, 1, 2).unwrap());
        let bucket = |start: i64, model: &str, cost: f64| UsageBucket {
            start,
            provider: "anthropic".into(),
            model: model.into(),
            input: 1,
            output: 2,
            cache_read: 10,
            cache_write: 3,
            cost,
            messages: 1,
        };
        let mut s = State { hidden: HashSet::from(["anthropic/b".to_owned()]), ..Default::default() };
        s.set_custom(from, to);
        assert_eq!(s.until - s.since, 2 * DAY);
        assert_eq!(s.bucket(), HOUR);
        let since = s.since;
        s.buckets.insert("local".into(), vec![bucket(since, "a", 1.), bucket(since + HOUR, "b", 5.)]);
        s.buckets.insert("remote".into(), vec![bucket(since, "a", 2.)]);
        let m = Model::build(&s);
        assert_eq!(m.slots, 48);
        assert_eq!(
            m.series.iter().map(|x| (x.label.as_str(), x.total.cost, x.hidden)).collect::<Vec<_>>(),
            [("b", 5., true), ("a", 3., false)]
        );
        assert_eq!(m.series[1].slots[0], Totals { input: 28, output: 4, cached: 20, cost: 3., messages: 2 });
        // The headline follows the filter: b is hidden.
        assert_eq!((m.total.messages, m.total.cost), (2, 3.));
        // Hidden series don't set the scale: a's biggest slot is $3, rounded up to $5.
        assert_eq!(m.scale(), 5.);
    }

    #[test]
    fn smoothing_never_overshoots() {
        let pts: Vec<Point<Pixels>> =
            [(0., 100.), (10., 100.), (20., 0.), (30., 0.), (40., 50.)].iter().map(|&(x, y)| point(px(x), px(y))).collect();
        for (to, a, b) in smooth(&pts) {
            for p in [to, a, b] {
                assert!((0. ..=100.).contains(&f32::from(p.y)), "{p:?}");
            }
        }
    }
}
