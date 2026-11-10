//! The Usage modal, behind the chart button left of the sidebar's gear, in three tabs: what
//! your agents cost over time, across every host (`Tab::Mine`), the leaderboard
//! (`Tab::Board`, see `leaderboard.rs`): everyone who joined, racing, and the accounts
//! behind the hosts' CLIProxyAPI with how much of their limits is used (`Tab::Proxy`).
//!
//! Every tab is one big chart over a list. The chart draws a smoothed line per series, the
//! area under each faintly filled where it is an amount: a model, a provider or everything
//! on Mine (`Group`), a player on the board, an account on Proxy. It shows one `Metric`,
//! per slot or as a running total that only climbs (`View::running`, the board's default:
//! there the order of the line ends is the ranking, each end labelled with its player).
//! Hovering puts a crosshair on the slot under the mouse with a card beside the cursor
//! that lists every line's value there, biggest first, and splits it into input / output
//! tokens and the cached share. The list under the chart is a row per series with its
//! totals for the window (the board's ranks players); click one to hide its line. Over the
//! chart are two lines at most: the tabs with the open one's window in their corner, then
//! Mine's and the board's headline with what the chart draws on its right (`hero_line`).
//!
//! Mine's data comes from every host's `ListUsage` for the window shown, already summed per
//! slot and model; `mine` folds the hosts together and groups the series. Its window is the
//! last day / week / month ending now (`‹ ›` page it back and forth), or a custom pair of
//! dates, with the slot width following the window so a line has ~50 points. The board's
//! comes from the api per player and slot, for the last 1d / 7d / 30d or all time.
//!
//! Proxy's comes from every host's `ListProxy` (a host's proxy is set up in Settings ›
//! Proxy, `proxy.rs`), for Mine's window. The list is the accounts as the proxy reports
//! them now: a bar per limit with when it starts over, and what else keeps the proxy from
//! using one. The chart is the history the host sampled: the share of one limit used
//! (`State::limit`, picked by its column's title in the list), on a scale that ends at the
//! limit. Several hosts' proxies fold into the one list, each account under its host.

use crate::app::{Dirigent, Menu};
use crate::icons::icon;
use crate::input::TextInput;
use crate::menu::{self, Side};
use crate::modal;
use crate::theme::{self, dollars, kilo, text, u};
use chrono::{Local, NaiveDate, TimeZone};
use dirigent_protocol::leaderboard::{Board, Range as BoardRange};
use dirigent_protocol::{Proxy, ProxyAccount, ProxyLimit, UsageBucket};
use gpui::{
    Anchor, AnyElement, Bounds, Context, Corners, Div, Entity, FontWeight, Hsla, Image, MouseMoveEvent, PathBuilder, Pixels, Point,
    SharedString, Stateful, TextAlign, TextRun, Window, anchored, canvas, deferred, div, fill, img, linear_color_stop, linear_gradient,
    point, prelude::*, px, relative, rgba, size,
};
use std::cell::RefCell;
use std::collections::{HashMap, HashSet};
use std::rc::Rc;
use std::sync::Arc;

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
/// Series colours, assigned by rank (biggest first).
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
/// The board's line-end labels: the room right of the plot they get, their avatar's size,
/// and the least distance between two (they're pushed apart when lines end close).
const LABELS_W: f32 = 116.;
const LABEL_AVATAR: f32 = 16.;
const LABEL_GAP: f32 = 18.;

#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub enum Tab {
    #[default]
    Mine,
    Board,
    Proxy,
}

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

/// What a series is on Mine: one line per model, per provider, or one for everything.
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

/// What the chart draws and the series are ranked by.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub enum Metric {
    #[default]
    Cost,
    /// Every token: the whole prompt (cache reads included) and the output.
    Tokens,
    Output,
    Messages,
    /// Proxy's: the share of a limit used, 0 to 1. Not one to pick (`ALL`).
    Used,
}

impl Metric {
    const ALL: [Metric; 4] = [Metric::Cost, Metric::Tokens, Metric::Output, Metric::Messages];

    pub fn label(self) -> &'static str {
        match self {
            Metric::Cost => "cost",
            Metric::Tokens => "tokens",
            Metric::Output => "output tokens",
            Metric::Messages => "messages",
            Metric::Used => "used",
        }
    }

    fn of(self, t: &Totals) -> f64 {
        match self {
            Metric::Cost => t.cost,
            Metric::Tokens => (t.input + t.output) as f64,
            Metric::Output => t.output as f64,
            Metric::Messages => t.messages as f64,
            Metric::Used => t.used,
        }
    }

    fn fmt(self, v: f64) -> String {
        match self {
            Metric::Cost => dollars(v),
            Metric::Used => percent(v),
            _ => kilo(v.round() as u64),
        }
    }
}

/// A share as whole percent: `42%`.
fn percent(v: f64) -> String {
    format!("{}%", (v * 100.).round())
}

/// How a tab's chart is drawn; each tab keeps its own.
#[derive(Default)]
pub struct View {
    pub metric: Metric,
    /// Running totals over the window rather than each slot's own.
    pub running: bool,
    /// Series keys the user clicked off in the list.
    pub hidden: HashSet<String>,
}

/// The modal's state, on `Dirigent` (the custom range's date inputs live beside it there,
/// the board itself in `Dirigent::leaderboard`).
#[derive(Default)]
pub struct State {
    pub tab: Tab,
    pub mine: View,
    pub board: View,
    pub proxy: View,
    /// Mine's window (and Proxy's), unix ms; `until` is exclusive.
    pub range: Range,
    pub since: i64,
    pub until: i64,
    pub group: Group,
    /// Each host's latest answer. Kept across the periodic refresh so the chart never
    /// blanks: a reply to `request` swaps in that host's new buckets. Dropped only when
    /// the window changes, since stale buckets would then be from a different window.
    pub buckets: HashMap<String, Vec<UsageBucket>>,
    pub request: Option<u64>,
    /// Each host's proxy as it last answered (`ListProxy`), kept across the refresh like
    /// `buckets`. A window change drops the samples (they are the old window's) but not
    /// the accounts: those are as of now, whatever the window.
    pub proxies: HashMap<String, Proxy>,
    pub proxy_request: Option<u64>,
    /// The limit Proxy's chart draws, a `ProxyLimit::name` (`limit` picks one the accounts have).
    pub limit: String,
    /// The mouse over the chart (window px) and the chart's bounds from its last paint.
    pub hover: Option<Point<Pixels>>,
    plot: Rc<RefCell<Bounds<Pixels>>>,
}

impl State {
    pub fn new() -> Self {
        let mut s = Self::default();
        s.set_range(Range::Week);
        s.board.running = true;
        s.limit = "7d".into();
        s
    }

    pub fn view_mut(&mut self) -> &mut View {
        match self.tab {
            Tab::Mine => &mut self.mine,
            Tab::Board => &mut self.board,
            Tab::Proxy => &mut self.proxy,
        }
    }

    /// The window moved: what the hosts answered for the old one goes.
    fn drop_window(&mut self) {
        self.buckets.clear();
        self.proxies.values_mut().for_each(|p| p.samples.clear());
    }

    /// The limits the proxies' accounts report, by name (`5h`, `7d`, `7d_oi`: in that order).
    fn limits(&self) -> Vec<&str> {
        let mut names: Vec<&str> =
            self.proxies.values().flat_map(|p| &p.accounts).flat_map(|a| &a.limits).map(|l| l.name.as_str()).collect();
        names.sort();
        names.dedup();
        names
    }

    /// The limit Proxy's chart draws: the one picked, or the first there is if no account has it.
    fn limit(&self) -> &str {
        let names = self.limits();
        if names.contains(&self.limit.as_str()) { &self.limit } else { names.first().copied().unwrap_or(&self.limit) }
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
        self.drop_window();
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
        self.drop_window();
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
        self.drop_window();
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

/// `3h`, `day`, `week`: a slot's width, for `per 3h`.
fn slot_label(ms: i64) -> String {
    match ms {
        DAY => "day".into(),
        ms if ms == 7 * DAY => "week".into(),
        ms if ms >= DAY => format!("{}d", ms / DAY),
        ms if ms >= HOUR => format!("{}h", ms / HOUR),
        ms => format!("{}m", ms / MINUTE),
    }
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
    /// Proxy's: the share of a limit used. Set per slot, never summed.
    pub used: f64,
}

impl Totals {
    fn of(input: u64, output: u64, cache_read: u64, cache_write: u64, cost: f64, messages: u64) -> Self {
        Totals { input: input + cache_read + cache_write, output, cached: cache_read, cost, messages, used: 0. }
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
    /// Per slot; running totals when the view is.
    slots: Vec<Totals>,
    total: Totals,
    /// The first slot the line is drawn from: a player's join.
    from: usize,
    /// A player's picture, and whether they're the signed-in one.
    avatar: Option<Arc<Image>>,
    you: bool,
}

impl Series {
    fn new(key: String, label: String, detail: String, slots: usize) -> Self {
        Series {
            key,
            label,
            detail,
            color: 0,
            hidden: false,
            slots: vec![Totals::default(); slots],
            total: Totals::default(),
            from: 0,
            avatar: None,
            you: false,
        }
    }

    fn add(&mut self, slot: usize, t: Totals) {
        self.slots[slot].sum(&t);
        self.total.sum(&t);
    }

    /// The value drawn at slot `i`.
    fn at(&self, i: usize, metric: Metric) -> f64 {
        metric.of(&self.slots[i])
    }
}

/// The chart and list's data: the series for the open tab, ranked by the metric.
struct Model {
    since: i64,
    /// Slot width (ms) and count.
    bucket: i64,
    slots: usize,
    metric: Metric,
    running: bool,
    series: Vec<Series>,
    /// Over the visible series only: the headline follows the filter.
    total: Totals,
    /// Label the line ends (the board).
    labels: bool,
}

impl Model {
    /// Rank `series` by `view`'s metric, colour them, hide the hidden ones and, for a running
    /// view, turn each slot into the total up to it.
    fn new(since: i64, bucket: i64, slots: usize, mut series: Vec<Series>, view: &View, labels: bool) -> Self {
        let metric = view.metric;
        series.sort_by(|a, b| metric.of(&b.total).total_cmp(&metric.of(&a.total)).then_with(|| a.key.cmp(&b.key)));
        let palette = palette();
        let mut total = Totals::default();
        for (i, s) in series.iter_mut().enumerate() {
            s.color = palette[i % palette.len()];
            s.hidden = view.hidden.contains(&s.key);
            if !s.hidden {
                total.sum(&s.total);
            }
            if view.running {
                for i in 1..s.slots.len() {
                    let before = s.slots[i - 1];
                    s.slots[i].sum(&before);
                }
            }
        }
        Self { since, bucket, slots, metric, running: view.running, series, total, labels }
    }

    fn visible(&self) -> impl Iterator<Item = &Series> {
        self.series.iter().filter(|s| !s.hidden)
    }

    /// Top of the scale: the biggest visible value, rounded up to 1 / 2 / 5 × 10ⁿ. A share's
    /// is the whole, so the lines show how far each is from its limit.
    fn scale(&self) -> f64 {
        if self.metric == Metric::Used {
            return 1.;
        }
        let max = self.visible().flat_map(|s| (s.from..self.slots).map(|i| s.at(i, self.metric))).fold(0., f64::max);
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

/// Mine: every host's buckets folded into series for the current grouping.
fn mine(s: &State) -> Model {
    let bucket = s.bucket();
    let slots = ((s.until - s.since + bucket - 1) / bucket).max(1) as usize;
    let mut by_key: HashMap<String, Series> = HashMap::new();
    for b in s.buckets.values().flatten() {
        let (key, label, detail) = match s.group {
            Group::Model => (format!("{}/{}", b.provider, b.model), b.model.clone(), b.provider.clone()),
            Group::Provider => (b.provider.clone(), b.provider.clone(), String::new()),
            Group::All => ("all".to_owned(), "everything".to_owned(), String::new()),
        };
        let i = ((b.start - s.since) / bucket).clamp(0, slots as i64 - 1) as usize;
        by_key
            .entry(key.clone())
            .or_insert_with(|| Series::new(key, label, detail, slots))
            .add(i, Totals::of(b.input, b.output, b.cache_read, b.cache_write, b.cost, b.messages));
    }
    Model::new(s.since, bucket, slots, by_key.into_values().collect(), &s.mine, false)
}

/// Proxy: a series per account behind every host's proxy, drawing the share of the limit
/// shown that was used. A line starts at the account's first sample in the window; a slot
/// the host didn't sample (its service was down) keeps the one before's.
fn proxy(s: &State) -> Model {
    let bucket = s.bucket();
    let slots = ((s.until - s.since + bucket - 1) / bucket).max(1) as usize;
    let limit = s.limit();
    // The host only tells accounts apart when several have a proxy.
    let many = s.proxies.values().filter(|p| !p.accounts.is_empty()).count() > 1;
    let mut series = vec![];
    for (host, p) in &s.proxies {
        for a in &p.accounts {
            let detail = if many { format!("{} · {host}", a.provider) } else { a.provider.clone() };
            let mut line = Series::new(account_key(host, &a.id), a.name.clone(), detail, slots);
            let mut sampled: Vec<Option<f64>> = vec![None; slots];
            for x in p.samples.iter().filter(|x| x.account == a.id && x.limit == limit) {
                let i = ((x.start - s.since) / bucket).clamp(0, slots as i64 - 1) as usize;
                sampled[i] = Some(sampled[i].map_or(x.used, |v| v.max(x.used)));
            }
            line.from = sampled.iter().position(Option::is_some).unwrap_or(slots);
            let mut last = 0.;
            for (slot, used) in line.slots.iter_mut().zip(sampled) {
                last = used.unwrap_or(last);
                // A request the provider let through at the limit can end a little over it.
                slot.used = last.min(1.);
            }
            series.push(line);
        }
    }
    // No totals to rank by: the accounts keep their order (by host and id), and so their colours.
    let mut model = Model::new(s.since, bucket, slots, series, &s.proxy, false);
    model.metric = Metric::Used;
    model
}

/// A proxy account's series key: accounts are per host.
fn account_key(host: &str, id: &str) -> String {
    format!("{host}/{id}")
}

/// Why the proxy can't use an account, when a limit isn't why (the bars say that).
fn trouble(a: &ProxyAccount) -> Option<&'static str> {
    match a {
        _ if a.disabled => Some("disabled"),
        _ if a.sign_in => Some("has to sign in again"),
        _ if a.error.is_some() => Some("failing"),
        _ => None,
    }
}

/// A limit's name as the tab says it. `7d_oi` is the part of Anthropic's weekly limit its
/// top model may use (the proxy calls it the Fable limit): a limit inside the `7d` one.
fn limit_label(name: &str) -> &str {
    match name {
        "7d_oi" => "7d Fable",
        name => name,
    }
}

/// `2h 05m`, `3d 4h`, `12m`: how long until `at`, for a limit's reset.
fn time_left(at: i64, now: i64) -> String {
    let m = (at - now).max(0) / MINUTE;
    match m {
        0..60 => format!("{m}m"),
        60..1440 => format!("{}h {:02}m", m / 60, m % 60),
        _ => format!("{}d {}h", m / 1440, m % 1440 / 60),
    }
}

/// `07:40` today, `Sat 07:40` on another day.
fn clock(at: i64, now: i64) -> String {
    let (t, today) = (local(at), local(now).date_naive());
    t.format(if t.date_naive() == today { "%H:%M" } else { "%a %H:%M" }).to_string()
}

/// The board: a series per player, drawn from when they joined. Before one arrives, an
/// empty day.
fn board(board: Option<&Board>, view: &View, you: Option<&str>, avatars: &HashMap<String, Option<Arc<Image>>>) -> Model {
    let Some(b) = board else {
        return Model::new(now_ms() - DAY, HOUR, 24, vec![], view, true);
    };
    let slot = b.slot.max(1);
    let slots = ((b.until - b.since) / slot).max(1) as usize;
    let at = |t: i64| (t - b.since).div_euclid(slot).clamp(0, slots as i64 - 1) as usize;
    let series = b
        .players
        .iter()
        .map(|p| {
            let mut s = Series::new(p.id.clone(), p.name.clone(), String::new(), slots);
            s.from = at(p.joined_at);
            s.avatar = avatars.get(&p.picture).cloned().flatten();
            s.you = you == Some(p.id.as_str());
            for x in &p.slots {
                s.add(at(x.start), Totals::of(x.input, x.output, x.cache_read, x.cache_write, x.cost, x.messages));
            }
            s
        })
        .collect();
    Model::new(b.since, slot, slots, series, view, true)
}

/// Where the chart lands inside the canvas bounds; shared by the paint and the hover hit test.
struct Layout {
    bounds: Bounds<Pixels>,
    /// The lines' region: under the scale label's band, over the time axis, left of the
    /// line-end labels.
    plot: Bounds<Pixels>,
}

impl Layout {
    fn new(bounds: Bounds<Pixels>, labels: bool) -> Self {
        let h = (bounds.size.height - u(AXIS + BAND)).max(px(0.));
        let w = (bounds.size.width - if labels { u(LABELS_W) } else { px(0.) }).max(px(0.));
        let plot = Bounds::new(point(bounds.left(), bounds.top() + u(BAND)), size(w, h));
        Self { bounds, plot }
    }

    /// Centre x of slot `i` of `n`.
    fn x(&self, i: usize, n: usize) -> Pixels {
        self.plot.left() + self.plot.size.width * ((i as f32 + 0.5) / n as f32)
    }

    fn slot_at(&self, x: Pixels, n: usize) -> Option<usize> {
        let f = f32::from(x - self.plot.left()) / f32::from(self.plot.size.width);
        (0. ..1.).contains(&f).then(|| ((f * n as f32) as usize).min(n - 1))
    }
}

pub fn render(this: &Dirigent, cx: &mut Context<Dirigent>) -> AnyElement {
    let s = &this.usage;
    let lb = &this.leaderboard;
    let model = Rc::new(match s.tab {
        Tab::Mine => mine(s),
        Tab::Board => board(lb.board.as_ref(), &s.board, lb.active(&this.forge.status).map(|a| a.id.as_str()), &lb.avatars),
        Tab::Proxy => proxy(s),
    });
    let hover_slot = s.hover.and_then(|p| Layout::new(*s.plot.borrow(), model.labels).slot_at(p.x, model.slots));
    // Two lines over the chart at most: the tabs with the open one's window in their
    // corner, then its headline with what the chart draws (Proxy has neither: its list's
    // column titles pick the limit; a notice stands there when something is off).
    let (when, hero, list) = match s.tab {
        Tab::Mine => {
            let [metric, mode] = view_controls(this, &model, cx);
            let what = vec![metric, group_control(this, cx), mode];
            (window_controls(this, cx), Some(hero(this, &model, what)), legend(&model, cx))
        }
        Tab::Board => {
            let what = view_controls(this, &model, cx).into();
            (board_window(this, cx), Some(board_hero(this, &model, what)), ranking(&model, cx))
        }
        Tab::Proxy => (window_controls(this, cx), proxy_notice(this, cx), accounts(this, &model, cx)),
    };
    let body = div()
        .flex()
        .flex_col()
        .px_6()
        .pt_4()
        .pb_4()
        .child(tabs(this, when, cx))
        .children(hero)
        .child(chart(this, model.clone(), hover_slot, cx))
        .child(list);
    modal::frame(modal::large(body), cx)
}

/// The modal's top line: the tab switch and, in its corner, the open tab's window (`when`).
/// Like every control here it is quiet text, so the chart stays the picture: a menu is a
/// chip (`cost ▾`), a choice between a few is its labels in a row with the chosen one
/// lit (`segments`), as the tabs themselves are.
fn tabs(this: &Dirigent, when: Vec<AnyElement>, cx: &mut Context<Dirigent>) -> Div {
    let open = this.usage.tab;
    let tab = |tab: Tab, label: &'static str, cx: &mut Context<Dirigent>| {
        let on = tab == open;
        div()
            .id(SharedString::from(format!("usage-tab/{label}")))
            .h(u(28.))
            .px_2()
            .flex()
            .items_center()
            .rounded_md()
            .cursor_pointer()
            .font_weight(FontWeight::MEDIUM)
            .text_color(rgba(if on { theme::t().fg } else { theme::t().fg_2 }))
            .when(on, |d| d.bg(rgba(theme::t().hover)))
            .hover(|d| d.text_color(rgba(theme::t().fg)))
            .on_click(cx.listener(move |this, _, _, cx| this.set_usage_tab(tab, cx)))
            .child(label)
    };
    // The sides hang out by a label's padding, so their text lines up with the chart's edges.
    let side = || div().flex().flex_row().items_center();
    div()
        .flex()
        .flex_row()
        .items_center()
        .justify_between()
        .gap_4()
        .h(u(30.))
        .mb_3()
        .child(side().gap_1().flex_shrink_0().ml(u(-8.)).children([
            tab(Tab::Mine, "Your usage", cx),
            tab(Tab::Board, "Leaderboard", cx),
            tab(Tab::Proxy, "Proxy", cx),
        ]))
        .child(side().gap_3().mr(u(-SEGMENT_PAD)).children(when))
}

/// The board's way in, beside its window while you aren't on it: Join (without the forge
/// sign-in, the way to Settings › Forge providers, where you sign in), or the join waiting
/// for the api. Nothing once you're on it: your row says so, and leaving is in Settings ›
/// Forge providers (`account.rs`).
fn join(this: &Dirigent, cx: &mut Context<Dirigent>) -> Option<AnyElement> {
    let lb = &this.leaderboard;
    if lb.active(&this.forge.status).is_some() {
        return None;
    }
    if lb.joining {
        return Some(div().text_size(text::sm()).text_color(rgba(theme::t().fg_1)).child("Joining…").into_any());
    }
    let (label, tip) = match this.forge.status.signed_in() {
        Some(_) => (
            "Join with your forge account",
            "Puts you on the public board and uploads your usage from now on (totals per hour, no prompts)",
        ),
        None => ("Sign in to join", "Opens Settings › Forge providers, where you sign in to the forge; then join here"),
    };
    Some(
        theme::button("board-join", "crown", label)
            .text_size(text::sm())
            .tooltip(theme::tooltip(tip))
            .on_click(cx.listener(|this, _, _, cx| this.join_board(cx)))
            .into_any(),
    )
}

/// A round picture, or the name's initial on `color` until it's loaded.
pub fn avatar(image: Option<Arc<Image>>, name: &str, color: u32, side: f32) -> AnyElement {
    match image {
        Some(image) => img(image).size(u(side)).rounded_full().flex_shrink_0().into_any_element(),
        None => div()
            .size(u(side))
            .rounded_full()
            .flex_shrink_0()
            .flex()
            .items_center()
            .justify_center()
            .bg(rgba(color))
            .text_size(u(side * 0.6))
            .font_weight(FontWeight::MEDIUM)
            .text_color(rgba(theme::t().panel))
            .child(name.chars().next().map(|c| c.to_uppercase().to_string()).unwrap_or_default())
            .into_any_element(),
    }
}

/// A headline number: dollars with their cents dimmed (`$24` `.66`), a count with its unit.
fn headline(metric: Metric, v: f64) -> Div {
    let (big, small) = match metric {
        Metric::Cost => {
            let total = dollars(v);
            total.split_once('.').map(|(w, c)| (w.to_owned(), format!(".{c}"))).unwrap_or((total, String::new()))
        }
        _ => (kilo(v.round() as u64), metric.label().to_owned()),
    };
    let unit = div().text_color(rgba(theme::t().fg_1)).when(metric != Metric::Cost, |d| d.ml_2().text_size(text::lg()));
    div()
        .flex()
        .flex_row()
        .items_baseline()
        .text_size(u(34.))
        .line_height(u(40.))
        .font_weight(FontWeight::MEDIUM)
        .child(big)
        .child(unit.child(small))
}

/// The line over Mine's and the board's chart: the headline `number`, its `caption` after
/// it on the same baseline, and on the right `what` the chart draws. The caption is cut
/// short when the three don't fit.
fn hero_line(number: Div, caption: Div, what: Vec<AnyElement>) -> Div {
    // The number's line box is taller than the small text's: these lift the caption and the
    // controls onto its baseline.
    let (caption_lift, what_lift) = (2., 0.);
    div()
        .flex()
        .flex_row()
        .items_end()
        .gap_3()
        .pb_2()
        .child(number.flex_shrink_0())
        .child(caption.flex_1().min_w_0().pb(u(caption_lift)).text_size(text::sm()).text_color(rgba(theme::t().fg_1)))
        .child(div().flex().flex_row().items_center().gap_3().flex_shrink_0().pb(u(what_lift)).mr(u(-SEGMENT_PAD)).children(what))
}

/// A control label's padding left and right (`theme::chip`'s).
const SEGMENT_PAD: f32 = 6.;

/// A choice between a few: its labels in a row (`segment`).
fn segments(children: impl IntoIterator<Item = Stateful<Div>>) -> Div {
    div().flex().flex_row().items_center().gap_0p5().children(children)
}

/// One of `segments`' labels: bright on a pill when it is the choice, dim until hovered
/// when it isn't.
fn segment(id: String, label: impl Into<SharedString>, active: bool) -> Stateful<Div> {
    let t = theme::t();
    div()
        .id(SharedString::from(id))
        .h(u(24.))
        .px(u(SEGMENT_PAD))
        .flex()
        .items_center()
        .rounded_md()
        .cursor_pointer()
        .text_size(text::sm())
        .text_color(rgba(if active { t.fg } else { t.fg_2 }))
        .when(active, |d| d.bg(rgba(t.hover)))
        .hover(move |d| d.text_color(rgba(t.fg)))
        .child(label.into())
}

/// What Mine's and the board's chart draws: the metric's menu, and the per slot / running
/// total choice.
fn view_controls(this: &Dirigent, model: &Model, cx: &mut Context<Dirigent>) -> [AnyElement; 2] {
    let open = this.menu == Some(Menu::UsageMetric);
    let menu = open.then(|| {
        let items = Metric::ALL
            .into_iter()
            .map(|m| {
                menu::item(format!("m/{}", m.label()), m.label(), "", model.metric == m, move |this, cx| this.set_usage_metric(m, cx), cx)
            })
            .collect();
        menu::popup(Side::Below(24.), items, cx)
    });
    let metric = theme::chip("usage-metric", model.metric.label(), false, open)
        .text_size(text::sm())
        .on_click(cx.listener(|this, _, _, cx| this.toggle_menu(Menu::UsageMetric, cx)))
        .children(menu);
    let per = segment("mode/per".into(), format!("per {}", slot_label(model.bucket)), !model.running)
        .on_click(cx.listener(|this, _, _, cx| this.set_usage_running(false, cx)));
    let total =
        segment("mode/total".into(), "total", model.running).on_click(cx.listener(|this, _, _, cx| this.set_usage_running(true, cx)));
    [metric.into_any_element(), segments([per, total]).into_any_element()]
}

/// What a line is on Mine: the grouping's menu.
fn group_control(this: &Dirigent, cx: &mut Context<Dirigent>) -> AnyElement {
    let s = &this.usage;
    let open = this.menu == Some(Menu::UsageGroup);
    let menu = open.then(|| {
        let items = [Group::Model, Group::Provider, Group::All]
            .into_iter()
            .map(|g| menu::item(format!("g/{}", g.label()), g.label(), "", s.group == g, move |this, cx| this.set_usage_group(g, cx), cx))
            .collect();
        menu::popup(Side::Below(24.), items, cx)
    });
    theme::chip("usage-group", s.group.label(), false, open)
        .text_size(text::sm())
        .on_click(cx.listener(|this, _, _, cx| this.toggle_menu(Menu::UsageGroup, cx)))
        .children(menu)
        .into_any_element()
}

/// The window, Mine's and Proxy's: the range choice, the custom range's date fields while
/// that is the range, and the pager.
fn window_controls(this: &Dirigent, cx: &mut Context<Dirigent>) -> Vec<AnyElement> {
    let s = &this.usage;
    let ranges = segments([Range::Day, Range::Week, Range::Month, Range::Custom].map(|r| {
        segment(format!("range/{}", r.label()), r.label(), s.range == r)
            .on_click(cx.listener(move |this, _, _, cx| this.set_usage_range(r, cx)))
    }));
    let at_now = s.at_now();
    let pager = div()
        .flex()
        .flex_row()
        .items_center()
        .child(theme::icon_button("usage-back", "chevron-left", cx, |this, cx| this.page_usage(-1, cx)))
        .child(theme::icon_button("usage-fwd", "chevron-right", cx, |this, cx| this.page_usage(1, cx)).when(at_now, |d| d.opacity(0.3)));
    let field = |input: Entity<TextInput>| {
        div()
            .w(u(100.))
            .h(u(24.))
            .px_2()
            .flex()
            .items_center()
            .rounded_md()
            .bg(rgba(theme::t().raised))
            .border_1()
            .border_color(rgba(theme::t().line))
            .text_size(text::sm())
            .child(input)
    };
    let custom = (s.range == Range::Custom).then(|| {
        div()
            .flex()
            .flex_row()
            .items_center()
            .gap_2()
            .text_size(text::sm())
            .text_color(rgba(theme::t().fg_1))
            .child(field(this.usage_from.clone()))
            .child("to")
            .child(field(this.usage_to.clone()))
    });
    [Some(ranges), custom, Some(pager)].into_iter().flatten().map(IntoElement::into_any_element).collect()
}

/// Mine's headline: what the window cost (or the metric's total), then the window and how
/// many messages it took.
fn hero(this: &Dirigent, model: &Model, what: Vec<AnyElement>) -> Div {
    let s = &this.usage;
    let messages = match model.total.messages {
        0 => "no messages".to_owned(),
        1 => "1 message".to_owned(),
        n => format!("{n} messages"),
    };
    let caption = div().truncate().child(format!("{}, {messages}", window_label(s.since, s.until, s.bucket())));
    hero_line(headline(model.metric, model.metric.of(&model.total)), caption, what)
}

/// The board's corner: the way onto it while you aren't (`join`), and its window: the last
/// day, week or month, or all time.
fn board_window(this: &Dirigent, cx: &mut Context<Dirigent>) -> Vec<AnyElement> {
    let range = this.leaderboard.range;
    let ranges = segments(BoardRange::ALL.map(|r| {
        segment(format!("board-range/{}", r.as_str()), r.as_str(), range == r)
            .on_click(cx.listener(move |this, _, _, cx| this.set_board_range(r, cx)))
    }));
    join(this, cx).into_iter().chain([ranges.into_any_element()]).collect()
}

/// The board's headline: the leader's number, then who they are (and where you stand).
fn board_hero(this: &Dirigent, model: &Model, what: Vec<AnyElement>) -> Div {
    let lb = &this.leaderboard;
    let t = theme::t();
    let metric = model.metric;
    let leader = model.series.first().filter(|s| metric.of(&s.total) > 0.);
    let span = match lb.range {
        BoardRange::Day => "the last day",
        BoardRange::Week => "the last 7 days",
        BoardRange::Month => "the last 30 days",
        BoardRange::All => "all time",
    };
    let sub = div().flex().flex_row().items_center().gap_1p5();
    let said = |text: String| div().min_w_0().truncate().child(text);
    let sub = match (&lb.board, leader) {
        (None, _) if lb.error.is_some() => sub.child(said("Couldn't reach the leaderboard".into())),
        (None, _) => sub.child(said("Loading the leaderboard".into())),
        (Some(_), None) => sub.child(said(format!("Nobody has burned anything in {span} yet"))),
        (Some(_), Some(l)) => {
            let rank = model.series.iter().position(|s| s.you);
            let you = match rank {
                Some(0) => " · that's you".to_owned(),
                Some(i) => format!(" · you're #{}", i + 1),
                None => format!(" · {} players", model.series.len()),
            };
            sub.child(icon("crown", t.warn).size(u(13.))).child(said(format!("{} leads {span}{you}", l.label)))
        }
    };
    let big = match leader {
        Some(l) => headline(metric, metric.of(&l.total)),
        None => div().text_size(u(34.)).line_height(u(40.)).font_weight(FontWeight::MEDIUM).text_color(rgba(t.fg_2)).child("–"),
    };
    hero_line(big, sub, what)
}

/// Proxy has no headline: its list says where the accounts stand. This is what is in the
/// way of that, if anything: why a host's look at its proxy failed (its accounts are then
/// as of the last one that worked), or that no host has a proxy, with the way to Settings
/// › Proxy, where one is set up.
fn proxy_notice(this: &Dirigent, cx: &mut Context<Dirigent>) -> Option<Div> {
    let s = &this.usage;
    let t = theme::t();
    let line = |color: u32| div().min_w_0().text_size(text::sm()).text_color(rgba(color));
    let mut errors: Vec<String> = s.proxies.iter().filter_map(|(host, p)| Some(format!("{host}: {}", p.error.as_ref()?))).collect();
    errors.sort();
    if !errors.is_empty() {
        return Some(div().flex().flex_col().gap_1().pb_3().children(errors.into_iter().map(|e| line(t.error).child(e))));
    }
    // Nothing to say while a host has one, or before any host has answered.
    if s.proxies.is_empty() || s.proxies.values().any(|p| !p.url.is_empty()) {
        return None;
    }
    let setup = theme::button("proxy-setup", "settings", "Set up in Settings")
        .text_size(text::sm())
        .on_click(cx.listener(|this, _, _, cx| this.open_proxy_settings(cx)));
    Some(div().flex().flex_row().items_center().gap_3().pb_3().child(line(t.fg_1).child("No host has a CLIProxyAPI set up")).child(setup))
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

/// What the chart says instead of lines, if anything.
fn empty_text(this: &Dirigent, model: &Model) -> Option<&'static str> {
    let s = &this.usage;
    let lb = &this.leaderboard;
    match s.tab {
        Tab::Mine if model.total.messages > 0 => None,
        Tab::Mine if s.request.is_some() && s.buckets.is_empty() => Some("Loading"),
        Tab::Mine => Some("Nothing spent in this window"),
        Tab::Board => match &lb.board {
            None if lb.error.is_some() => Some("Couldn't reach the leaderboard"),
            None => Some("Loading"),
            Some(b) if b.players.is_empty() => Some("Nobody has joined yet"),
            Some(_) if model.series.iter().all(|s| s.total.messages == 0) => Some("Nothing burned in this window yet"),
            Some(_) => None,
        },
        // The notice over the toolbar says why there are no accounts. A line takes two samples.
        Tab::Proxy if model.series.is_empty() => None,
        Tab::Proxy if model.series.iter().all(|s| s.from + 1 >= model.slots) => Some("No history in this window yet"),
        Tab::Proxy => None,
    }
}

/// The chart in one canvas, with the hover card floated beside the cursor.
fn chart(this: &Dirigent, model: Rc<Model>, hover_slot: Option<usize>, cx: &mut Context<Dirigent>) -> Stateful<Div> {
    let s = &this.usage;
    let plot = s.plot.clone();
    let paint_model = model.clone();
    let strips = canvas(
        |_, _, _| {},
        move |bounds, _, window, cx| {
            *plot.borrow_mut() = bounds;
            paint(&paint_model, &Layout::new(bounds, paint_model.labels), hover_slot, window, cx);
        },
    )
    .size_full();
    let empty = empty_text(this, &model).map(|text| {
        div().absolute().inset_0().flex().items_center().justify_center().pb(u(AXIS)).text_color(rgba(theme::t().fg_2)).child(text)
    });
    let card = hover_slot.zip(s.hover).map(|(i, p)| readout(&model, i, p, &Layout::new(*s.plot.borrow(), model.labels)));
    div()
        .id("usage-chart")
        .relative()
        .flex_1()
        .min_h(u(240.))
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

/// Paint the chart: horizontal gridlines with their labels, time labels, one smoothed line
/// (and its area, for amounts) per visible series, the board's line-end labels, and the crosshair with
/// a dot per line on the hovered slot.
fn paint(model: &Model, layout: &Layout, hover: Option<usize>, window: &mut Window, cx: &mut gpui::App) {
    let (n, metric) = (model.slots, model.metric);
    let line: Hsla = rgba(theme::t().line).into();
    let soft: Hsla = rgba(theme::t().line_soft).into();
    let (bounds, plot) = (layout.bounds, layout.plot);

    // Time ticks: labels along the bottom (no verticals: the hover crosshair is the only one).
    let span = model.bucket * n as i64;
    let px_per_ms = f32::from(plot.size.width) / span as f32;
    let step = TICKS.into_iter().find(|t| *t as f32 * px_per_ms >= TICK_MIN_PX * theme::zoom()).unwrap_or(TICKS[TICKS.len() - 1]);
    let mut t = align(model.since, step);
    while t < model.since + span {
        if t >= model.since {
            let x = plot.left() + px((t - model.since) as f32 * px_per_ms);
            let fmt = if step < DAY { "%H:%M" } else { "%b %-d" };
            let label = local(t).format(fmt).to_string();
            let w = text_width(&label, text::xs(), window);
            if x - w / 2. >= plot.left() && x + w / 2. <= plot.right() {
                paint_text(&label, text::xs(), theme::t().fg_2, point(x - w / 2., bounds.bottom() - u(16.)), window, cx);
            }
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
        let label = metric.fmt(max * f);
        let w = text_width(&label, text::xs(), window);
        paint_text(&label, text::xs(), theme::t().fg_2, point(plot.right() - w, y - u(16.)), window, cx);
    }

    // Biggest at the back.
    for series in model.visible().collect::<Vec<_>>().into_iter().rev() {
        let pts: Vec<Point<Pixels>> = (series.from..n).map(|i| point(layout.x(i, n), y_of(series.at(i, metric)))).collect();
        let color: Hsla = rgba(series.color).into();
        // No fill under a share: what's under the line is no amount, and accounts near
        // their limit would bury each other.
        if metric != Metric::Used
            && let Some(area) = area_path(&pts, plot.bottom())
        {
            let fade = linear_gradient(180., linear_color_stop(color.opacity(0.22), 0.), linear_color_stop(color.opacity(0.), 1.));
            window.paint_path(area, fade);
        }
        if let Some(stroke) = line_path(&pts) {
            window.paint_path(stroke, color);
        }
    }

    if model.labels {
        paint_labels(model, layout, &y_of, window, cx);
    }

    if let Some(i) = hover {
        let x = layout.x(i, n);
        window.paint_quad(fill(Bounds::new(point(x, plot.top()), size(px(1.), plot.size.height)), rgba(theme::t().fg_2)));
        for series in model.visible().filter(|s| i >= s.from) {
            dot(point(x, y_of(series.at(i, metric))), series.color, window);
        }
    }
}

/// A line's dot, ringed in the panel colour so it stands off the line.
fn dot(c: Point<Pixels>, color: u32, window: &mut Window) {
    let r = u(3.5);
    window.paint_quad(
        fill(Bounds::new(point(c.x - r, c.y - r), size(r * 2., r * 2.)), rgba(color))
            .corner_radii(r)
            .border_widths(u(1.5))
            .border_color(rgba(theme::t().panel)),
    );
}

/// Each line's end, dotted and labelled right of the plot with its player's picture and
/// name, the labels pushed apart where lines end close together.
fn paint_labels(model: &Model, layout: &Layout, y_of: &dyn Fn(f64) -> Pixels, window: &mut Window, cx: &mut gpui::App) {
    let n = model.slots;
    let ends: Vec<(&Series, Pixels)> = model.visible().map(|s| (s, y_of(s.at(n - 1, model.metric)))).collect();
    let half = u(LABEL_AVATAR) / 2.;
    let ys: Vec<f32> = ends.iter().map(|(_, y)| f32::from(*y)).collect();
    let placed = spread(&ys, f32::from(u(LABEL_GAP)), f32::from(layout.plot.top() + half), f32::from(layout.bounds.bottom() - half));
    let size_text = text::xs();
    for ((series, y_end), y) in ends.into_iter().zip(placed) {
        let y = px(y);
        dot(point(layout.x(n - 1, n), y_end), series.color, window);
        let left = layout.plot.right() + u(12.);
        let ring = Bounds::new(point(left - px(1.5), y - half - px(1.5)), size(half * 2. + px(3.), half * 2. + px(3.)));
        window.paint_quad(fill(ring, rgba(series.color)).corner_radii(half + px(1.5)));
        let pic = Bounds::new(point(left, y - half), size(half * 2., half * 2.));
        match series.avatar.clone().and_then(|image| image.use_render_image(window, cx)) {
            Some(data) => {
                let _ = window.paint_image(pic, pic, Corners::all(half), data, 0, false);
            }
            None => window.paint_quad(fill(pic, rgba(theme::t().panel)).corner_radii(half)),
        }
        let room = layout.bounds.right() - (left + half * 2. + u(6.));
        let name = fit(&series.label, room, size_text, window);
        paint_text(&name, size_text, series.color, point(left + half * 2. + u(6.), y - size_text * 0.7), window, cx);
    }
}

/// Positions for labels wanted at `ys`: each at least `gap` from the next, kept within
/// `top..=bottom` as far as they fit, in the same order.
fn spread(ys: &[f32], gap: f32, top: f32, bottom: f32) -> Vec<f32> {
    let mut order: Vec<usize> = (0..ys.len()).collect();
    order.sort_by(|a, b| ys[*a].total_cmp(&ys[*b]));
    let mut out = ys.to_vec();
    let mut prev = f32::NEG_INFINITY;
    for &i in &order {
        out[i] = out[i].max(prev + gap).max(top);
        prev = out[i];
    }
    let mut next = f32::INFINITY;
    for &i in order.iter().rev() {
        out[i] = out[i].min(next - gap).min(bottom);
        next = out[i];
    }
    out
}

/// `s`, cut short with `…` to fit `room`.
fn fit(s: &str, room: Pixels, size: Pixels, window: &mut Window) -> String {
    if text_width(s, size, window) <= room {
        return s.to_owned();
    }
    let mut chars: Vec<char> = s.chars().collect();
    while !chars.is_empty() {
        chars.pop();
        let cut = format!("{}…", chars.iter().collect::<String>());
        if text_width(&cut, size, window) <= room {
            return cut;
        }
    }
    String::new()
}

/// A smooth curve through `pts` (monotone cubic, so it never overshoots the data: a
/// running total never seems to dip). Returns the cubic segments as `(to, ctrl_a, ctrl_b)`
/// for each step `i → i+1`.
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
    let mut p = PathBuilder::stroke(u(LINE_W));
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

fn text_width(s: &str, size: Pixels, window: &mut Window) -> Pixels {
    shape(s, size, theme::t().fg, window).width
}

fn paint_text(s: &str, size: Pixels, color: u32, origin: Point<Pixels>, window: &mut Window, cx: &mut gpui::App) {
    let line = shape(s, size, color, window);
    let _ = line.paint(origin, size * 1.4, TextAlign::Left, None, window, cx);
}

fn shape(s: &str, size: Pixels, color: u32, window: &mut Window) -> gpui::ShapedLine {
    let run = TextRun { len: s.len(), font: window.text_style().font(), color: rgba(color).into(), ..Default::default() };
    window.text_system().shape_line(SharedString::from(s.to_owned()), size, &[run], None)
}

/// The hovered slot's time and, per visible line, its value there, biggest first, and the
/// split behind it (`1.7M in · 15k out · 98% cached`), floated beside the cursor: to its
/// right on the left half of the plot, to its left on the right half, so it never sits on
/// the line. Running totals are as of the slot's end.
fn readout(model: &Model, i: usize, mouse: Point<Pixels>, layout: &Layout) -> AnyElement {
    let (start, bucket) = (model.since + i as i64 * model.bucket, model.bucket);
    let (a, b) = (local(start), local(start + bucket));
    let when = match (bucket >= DAY, model.running) {
        (true, false) => a.format("%a, %b %-d").to_string(),
        (true, true) => format!("Through {}", a.format("%a, %b %-d")),
        (false, false) => format!("{} {} – {}", a.format("%b %-d"), a.format("%H:%M"), b.format("%H:%M")),
        (false, true) => format!("Through {}", b.format("%b %-d %H:%M")),
    };
    let mut lines: Vec<&Series> = model.visible().filter(|s| i >= s.from).collect();
    lines.sort_by(|x, y| y.at(i, model.metric).total_cmp(&x.at(i, model.metric)));
    // A grid, so each column is as wide as its widest cell: a value never wraps, and the
    // splits stay aligned however many digits it has.
    let rows = div().grid().grid_cols_max_content(4).gap_x_2().gap_y_1().items_center().children(lines.iter().flat_map(|s| {
        let t = s.slots[i];
        // A share has no tokens behind it.
        let split = if model.metric == Metric::Used { String::new() } else { t.split() };
        [
            div().size(u(7.)).rounded_full().bg(rgba(s.color)),
            div().w(u(140.)).truncate().child(s.label.clone()),
            div().min_w(u(56.)).whitespace_nowrap().text_color(rgba(theme::t().fg)).child(model.metric.fmt(model.metric.of(&t))),
            div().whitespace_nowrap().text_color(rgba(theme::t().fg_1)).child(split),
        ]
    }));
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
        .text_size(text::sm())
        .line_height(u(17.))
        .child(div().text_color(rgba(theme::t().fg_2)).child(when))
        .when(!lines.is_empty(), |d| d.child(rows));
    let right_half = mouse.x > layout.plot.left() + layout.plot.size.width / 2.;
    let (anchor, dx) = if right_half { (Anchor::TopRight, u(-16.)) } else { (Anchor::TopLeft, u(16.)) };
    // Zero-size absolute wrapper so the anchored card takes a window position, not the
    // flex static position (see AGENTS.md).
    div()
        .absolute()
        .top_0()
        .left_0()
        .size_0()
        .child(deferred(
            anchored().anchor(anchor).position(point(mouse.x + dx, mouse.y + u(16.))).snap_to_window_with_margin(u(8.)).child(card),
        ))
        .into_any()
}

/// A list column: fixed width, its own colour.
fn col(w: f32, text: impl Into<SharedString>, color: u32) -> Div {
    div().w(u(w)).flex_shrink_0().text_color(rgba(color)).child(text.into())
}

/// A list row, clicked to hide / show its series' line.
fn list_row(s: &Series, cx: &mut Context<Dirigent>) -> Stateful<Div> {
    let key = s.key.clone();
    div()
        .id(SharedString::from(format!("series/{}", s.key)))
        .flex()
        .flex_row()
        .items_center()
        .gap_3()
        .h(u(30.))
        .px_1()
        .mx(u(-4.))
        .rounded_md()
        .cursor_pointer()
        .border_t_1()
        .border_color(rgba(theme::t().line_soft))
        .hover(|d| d.bg(rgba(theme::t().raised)))
        .on_click(cx.listener(move |this, _, _, cx| this.toggle_usage_series(&key, cx)))
}

/// The list under the chart: its head and its rows, scrolling when there are many rather
/// than squeezing the chart.
fn list(head: Div, rows: impl IntoIterator<Item = Stateful<Div>>) -> Div {
    div()
        .flex()
        .flex_col()
        .pt_3()
        .text_size(text::sm())
        .child(head.flex().flex_row().items_center().gap_3().h(u(24.)).text_size(text::xs()).text_color(rgba(theme::t().fg_2)))
        .child(div().id("usage-legend").max_h(u(6.5 * 30.)).overflow_y_scroll().flex().flex_col().children(rows))
}

/// Mine's list: one row per series with its totals for the window.
fn legend(model: &Model, cx: &mut Context<Dirigent>) -> Div {
    let fg_2 = theme::t().fg_2;
    let head = div()
        .child(div().size(u(8.)))
        .child(div().flex_1())
        .child(col(80., "cost", fg_2))
        .child(col(80., "in", fg_2))
        .child(col(80., "out", fg_2))
        .child(col(80., "cached", fg_2))
        .child(col(70., "messages", fg_2));
    let rows: Vec<Stateful<Div>> = model
        .series
        .iter()
        .map(|s| {
            let fg = if s.hidden { theme::t().fg_2 } else { theme::t().fg };
            let num = if s.hidden { theme::t().fg_2 } else { theme::t().fg_1 };
            let swatch = div().size(u(8.)).rounded_full().flex_shrink_0();
            let swatch = if s.hidden { swatch.border_1().border_color(rgba(s.color)) } else { swatch.bg(rgba(s.color)) };
            let t = s.total;
            list_row(s, cx)
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
                            d.child(div().text_size(text::xs()).text_color(rgba(theme::t().fg_2)).child(s.detail.clone()))
                        }),
                )
                .child(col(80., dollars(t.cost), fg))
                .child(col(80., kilo(t.input), num))
                .child(col(80., kilo(t.output), num))
                .child(col(80., t.cached_pct(), num))
                .child(col(70., t.messages.to_string(), num))
        })
        .collect();
    list(head, rows)
}

/// Proxy's list: a row per account with where each of its limits stands: a bar, the share
/// used, and how long until it starts over. An account whose limits the proxy hasn't heard
/// of yet shows dashes. One the proxy can't use for another reason than a limit says so
/// after its name (`trouble`). The columns' titles pick the limit the chart draws.
fn accounts(this: &Dirigent, model: &Model, cx: &mut Context<Dirigent>) -> Div {
    let s = &this.usage;
    let t = theme::t();
    let now = now_ms();
    let limits = s.limits();
    let by_key: HashMap<String, &ProxyAccount> =
        s.proxies.iter().flat_map(|(host, p)| p.accounts.iter().map(move |a| (account_key(host, &a.id), a))).collect();
    // A limit's column title is also how it is picked for the chart: the one drawn is lit.
    let shown = s.limit();
    let head = div().child(div().size(u(8.))).child(div().flex_1()).children(limits.iter().map(|name| {
        let pick = (*name).to_owned();
        let title = segment(format!("limit/{name}"), limit_label(name).to_owned(), *name == shown)
            .ml(u(-SEGMENT_PAD))
            .tooltip(theme::tooltip("Draw this limit in the chart"))
            .on_click(cx.listener(move |this, _, _, cx| this.set_usage_limit(pick.clone(), cx)));
        div().w(u(LIMIT_W)).flex_shrink_0().flex().flex_row().child(title)
    }));
    let rows: Vec<Stateful<Div>> = model
        .series
        .iter()
        .filter_map(|series| {
            let a = by_key.get(&series.key)?;
            let fg = if series.hidden { t.fg_2 } else { t.fg };
            let swatch = div().size(u(8.)).rounded_full().flex_shrink_0();
            let swatch = if series.hidden { swatch.border_1().border_color(rgba(series.color)) } else { swatch.bg(rgba(series.color)) };
            let status = trouble(a).map(|what| {
                // Switched off on purpose is no alarm. Hovering says what the provider (or
                // the proxy) said, when it said something.
                div()
                    .id(SharedString::from(format!("status/{}", series.key)))
                    .flex_shrink_0()
                    .text_size(text::xs())
                    .text_color(rgba(if a.disabled { t.fg_2 } else { t.error }))
                    .child(what)
                    .when_some(a.error.clone(), |d, e| d.tooltip(theme::tooltip(e)))
            });
            let name = div()
                .flex_1()
                .min_w_0()
                .flex()
                .flex_row()
                .items_baseline()
                .gap_2()
                .child(div().truncate().text_color(rgba(fg)).child(series.label.clone()))
                .child(div().flex_shrink_0().text_size(text::xs()).text_color(rgba(t.fg_2)).child(series.detail.clone()))
                .children(status);
            let cells = limits.iter().map(|name| {
                let limit = a.limits.iter().find(|l| l.name == *name);
                limit_cell(format!("limit/{}/{name}", series.key), limit, !a.limits.is_empty(), now)
            });
            Some(list_row(series, cx).child(swatch).child(name).children(cells))
        })
        .collect();
    list(head, rows)
}

/// A limit's column in Proxy's list.
const LIMIT_W: f32 = 168.;

/// `▓▓▓░░░  42%  2h 05m`: a limit's bar, the share used and the time until it starts over
/// (when, on hover). The bar is plain until it matters: amber from three quarters, red
/// when the limit is reached. Without the limit: empty when the account has `others` (it
/// doesn't have this one: Codex has no Fable share, its bigger plans no 5 hours), a dash
/// when the proxy knows none of its limits yet.
fn limit_cell(id: String, limit: Option<&ProxyLimit>, others: bool, now: i64) -> Stateful<Div> {
    let t = theme::t();
    let cell = div().id(SharedString::from(id)).w(u(LIMIT_W)).flex_shrink_0().flex().flex_row().items_center().gap_2();
    let Some(limit) = limit else {
        if others {
            return cell;
        }
        let why = "The proxy learns an account's limits from its replies, and has had none from this one since it started";
        return cell.text_color(rgba(t.fg_2)).child("–").tooltip(theme::tooltip(why));
    };
    let used = limit.used_at(now).clamp(0., 1.);
    let color = match used {
        u if u >= 1. => t.error,
        u if u >= 0.75 => t.warn,
        _ => t.fg_1,
    };
    let bar = div()
        .w(u(56.))
        .h(u(5.))
        .flex_shrink_0()
        .rounded_full()
        .bg(rgba(t.line))
        .child(div().h_full().w(relative(used as f32)).min_w(u(if used > 0. { 2. } else { 0. })).rounded_full().bg(rgba(color)));
    // Started over already: nothing left to wait for.
    let reset = limit.resets_at.filter(|at| *at > now);
    cell.child(bar)
        .child(div().w(u(36.)).flex_shrink_0().text_align(TextAlign::Right).text_color(rgba(t.fg_1)).child(percent(used)))
        .child(div().text_size(text::xs()).text_color(rgba(t.fg_2)).child(reset.map(|at| time_left(at, now)).unwrap_or_default()))
        .when_some(reset, |d, at| d.tooltip(theme::tooltip(format!("Starts over {}", clock(at, now)))))
}

/// The board's list: the ranking by the metric shown, with every player's totals for the
/// window.
fn ranking(model: &Model, cx: &mut Context<Dirigent>) -> Div {
    let t = theme::t();
    let head = div()
        .child(col(18., "", t.fg_2))
        .child(div().size(u(22.)))
        .child(div().flex_1())
        .child(col(80., "cost", t.fg_2))
        .child(col(80., "tokens", t.fg_2))
        .child(col(70., "output", t.fg_2))
        .child(col(64., "cached", t.fg_2))
        .child(col(70., "messages", t.fg_2));
    let rows: Vec<Stateful<Div>> = model
        .series
        .iter()
        .enumerate()
        .map(|(rank, s)| {
            let (fg, num) = if s.hidden { (t.fg_2, t.fg_2) } else { (t.fg, t.fg_1) };
            let tot = s.total;
            let picture = div()
                .size(u(22.))
                .flex_shrink_0()
                .flex()
                .items_center()
                .justify_center()
                .rounded_full()
                .border_2()
                .border_color(rgba(s.color))
                .when(s.hidden, |d| d.opacity(0.4))
                .child(avatar(s.avatar.clone(), &s.label, s.color, 16.));
            let name = div()
                .flex_1()
                .min_w_0()
                .flex()
                .flex_row()
                .items_center()
                .gap_2()
                .child(div().truncate().text_color(rgba(fg)).child(s.label.clone()))
                .when(s.you, |d| {
                    d.child(div().px_1p5().rounded_md().bg(rgba(t.raised)).text_size(text::xs()).text_color(rgba(t.fg_1)).child("you"))
                });
            list_row(s, cx)
                .child(col(18., format!("{}", rank + 1), t.fg_2))
                .child(picture)
                .child(name)
                .child(col(80., dollars(tot.cost), fg))
                .child(col(80., kilo(tot.input + tot.output), num))
                .child(col(70., kilo(tot.output), num))
                .child(col(64., tot.cached_pct(), num))
                .child(col(70., tot.messages.to_string(), num))
        })
        .collect();
    list(head, rows)
}

#[cfg(test)]
mod tests {
    use super::*;
    use dirigent_protocol::ProxySample;
    use dirigent_protocol::leaderboard::{Player, Slot};

    #[test]
    fn buckets_keep_a_line_under_sixty_points() {
        assert_eq!(bucket_for(DAY), 30 * MINUTE);
        assert_eq!(bucket_for(7 * DAY), 3 * HOUR);
        assert_eq!(bucket_for(30 * DAY), 12 * HOUR);
        assert_eq!(bucket_for(365 * DAY), 7 * DAY);
        assert_eq!([30 * MINUTE, 3 * HOUR, DAY, 2 * DAY, 7 * DAY].map(slot_label), ["30m", "3h", "day", "2d", "week"]);
    }

    #[test]
    fn mine_folds_hosts_and_ranks_by_the_metric() {
        let (from, to) = (NaiveDate::from_ymd_opt(2026, 1, 1).unwrap(), NaiveDate::from_ymd_opt(2026, 1, 2).unwrap());
        let bucket = |start: i64, model: &str, cost: f64, messages: u64| UsageBucket {
            start,
            provider: "anthropic".into(),
            model: model.into(),
            input: 1,
            output: 2,
            cache_read: 10,
            cache_write: 3,
            cost,
            messages,
        };
        let mut s = State::default();
        s.mine.hidden.insert("anthropic/b".to_owned());
        s.set_custom(from, to);
        assert_eq!(s.until - s.since, 2 * DAY);
        assert_eq!(s.bucket(), HOUR);
        let since = s.since;
        s.buckets.insert("local".into(), vec![bucket(since, "a", 1., 1), bucket(since + HOUR, "b", 5., 1)]);
        s.buckets.insert("remote".into(), vec![bucket(since, "a", 2., 3)]);
        let m = mine(&s);
        assert_eq!(m.slots, 48);
        assert_eq!(
            m.series.iter().map(|x| (x.label.as_str(), x.total.cost, x.hidden)).collect::<Vec<_>>(),
            [("b", 5., true), ("a", 3., false)]
        );
        assert_eq!(m.series[1].slots[0], Totals { input: 28, output: 4, cached: 20, cost: 3., messages: 4, used: 0. });
        // The headline follows the filter: b is hidden.
        assert_eq!((m.total.messages, m.total.cost), (4, 3.));
        // Hidden series don't set the scale: a's biggest slot is $3, rounded up to $5.
        assert_eq!(m.scale(), 5.);
        // By messages, a leads.
        s.mine.metric = Metric::Messages;
        assert_eq!(mine(&s).series[0].label, "a");
    }

    #[test]
    fn proxy_lines_run_from_the_first_sample() {
        let limit = |name: &str, used: f64, resets_at: i64| ProxyLimit { name: name.into(), used, resets_at: Some(resets_at) };
        let account = |id: &str, limits: Vec<ProxyLimit>| ProxyAccount {
            id: id.into(),
            provider: "claude".into(),
            name: id.into(),
            limits,
            ..ProxyAccount::default()
        };
        let sample =
            |start: i64, account: &str, limit: &str, used: f64| ProxySample { start, account: account.into(), limit: limit.into(), used };
        let mut s = State { limit: "7d".into(), ..State::default() };
        s.set_custom(NaiveDate::from_ymd_opt(2026, 1, 1).unwrap(), NaiveDate::from_ymd_opt(2026, 1, 1).unwrap());
        let (since, step) = (s.since, s.bucket());
        s.proxies.insert(
            "local".into(),
            Proxy {
                url: "http://p".into(),
                accounts: vec![account("b", vec![limit("7d", 0.5, 0), limit("7d_oi", 0.9, 0)]), account("a", vec![])],
                samples: vec![
                    sample(since + step, "b", "7d", 0.2),
                    sample(since + 3 * step, "b", "7d", 1.02),
                    sample(since, "b", "7d_oi", 0.9),
                ],
                ..Proxy::default()
            },
        );
        assert_eq!((s.limits(), s.limit()), (vec!["7d", "7d_oi"], "7d"));
        let m = proxy(&s);
        let used = |x: &Series| x.slots.iter().take(5).map(|t| t.used).collect::<Vec<_>>();
        // By key, not by use. `a` was never sampled: no line. `b`'s runs from its first
        // sample, keeps the last one over a gap, and stops at the limit.
        assert_eq!((m.series[0].label.as_str(), m.series[0].from), ("a", m.slots));
        assert_eq!((m.series[1].label.as_str(), m.series[1].from, used(&m.series[1])), ("b", 1, vec![0., 0.2, 0.2, 1., 1.]));
        assert_eq!((m.metric, m.scale()), (Metric::Used, 1.));
        // A limit no account has gives way to the first there is; a window change keeps the accounts.
        s.limit = "5h".into();
        assert_eq!(s.limit(), "7d");
        s.page(-1);
        assert_eq!((s.proxies["local"].accounts.len(), s.proxies["local"].samples.len()), (2, 0));
        // Two hosts with a proxy: the same account behind both is a line each, told apart by host.
        let theirs = Proxy { url: "http://q".into(), accounts: vec![account("b", vec![limit("7d", 0.1, 0)])], ..Proxy::default() };
        s.proxies.insert("remote".into(), theirs);
        let m = proxy(&s);
        let rows = |m: &Model| m.series.iter().map(|x| (x.key.clone(), x.detail.clone())).collect::<Vec<_>>();
        let want = [("local/a", "claude · local"), ("local/b", "claude · local"), ("remote/b", "claude · remote")];
        assert_eq!(rows(&m), want.map(|(key, detail)| (key.to_owned(), detail.to_owned())));
        // Out of quota is the bars' to say; the row only names other trouble.
        let failing = ProxyAccount { error: Some("token expired".into()), ..ProxyAccount::default() };
        assert_eq!((trouble(&ProxyAccount::default()), trouble(&failing)), (None, Some("failing")));
        assert_eq!(trouble(&ProxyAccount { disabled: true, ..failing }), Some("disabled"));
        assert_eq!([time_left(59_000, 0), time_left(125 * MINUTE, 0), time_left(100 * HOUR, 0)], ["0m", "2h 05m", "4d 4h"]);
    }

    #[test]
    fn board_lines_run_from_joining() {
        let slot = |start: i64, cost: f64| Slot { start, cost, messages: 1, ..Slot::default() };
        let player = |id: &str, joined_at: i64, slots: Vec<Slot>| Player {
            id: id.into(),
            name: id.into(),
            picture: String::new(),
            joined_at,
            slots,
        };
        let b = Board {
            since: 0,
            until: 4 * HOUR,
            slot: HOUR,
            players: vec![
                player("bo", -DAY, vec![slot(0, 0.5)]),
                player("ana", 2 * HOUR + 5, vec![slot(2 * HOUR, 1.), slot(3 * HOUR, 2.)]),
            ],
        };
        let view = View { running: true, ..View::default() };
        let m = board(Some(&b), &view, Some("bo"), &HashMap::new());
        let costs = |s: &Series| s.slots.iter().map(|t| t.cost).collect::<Vec<_>>();
        assert_eq!((m.series[0].label.as_str(), m.series[0].from, costs(&m.series[0])), ("ana", 2, vec![0., 0., 1., 3.]));
        assert_eq!((m.series[1].label.as_str(), m.series[1].you, costs(&m.series[1])), ("bo", true, vec![0.5; 4]));
        assert_eq!(m.scale(), 5.);
    }

    #[test]
    fn labels_spread_apart() {
        assert_eq!(spread(&[12., 10., 50.], 10., 0., 100.), [20., 10., 50.]);
        assert_eq!(spread(&[95., 96.], 10., 0., 100.), [90., 100.]);
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
