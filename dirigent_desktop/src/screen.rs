//! The open thread's screen: what its agent is doing on a screen it opened (one of its own,
//! or its host's desktop), as its host shows it (`ThreadSummary::screen`). The right panel
//! has it on top (`section`): the window the agent works in, a cursor where it last
//! clicked, and what it just did under it, in amber and with `your desktop` before it when
//! that is where it happened. A click on the picture enlarges it over the window
//! (`preview`), where it goes on updating.
//!
//! The host tells of every step with a small `screen` event, which moves the cursor and
//! the caption at once. The picture is asked for (`GetScreen`) a small piece at a time, the
//! next only once the last is in, and after a rest as long as that one took (`Screen::rest`):
//! whatever the connection, at most one piece is on its way and pictures take half the
//! line at most. Over a slow one the cursor runs ahead of a picture that skips steps, and
//! everything else stays as quick as it was. Nothing is asked for while the screen isn't
//! on show (the panel closed). The last picture stays after the run, until the thread is
//! left: the host keeps none.

use crate::app::{Dirigent, Pane};
use crate::net::HostStatus;
use crate::theme::{self, text, u};
use base64::Engine;
use dirigent_protocol::ClientMessage;
use gpui::{
    Animation, AnimationExt, Context, Div, IntoElement, Pixels, RenderImage, SharedString, Size, Stateful, div, ease_out_quint, img,
    prelude::*, px, relative, rgba, svg,
};
use serde_json::Value;
use std::sync::Arc;
use std::time::{Duration, Instant};

/// The tallest the picture gets in the panel; a taller window is shown narrower.
const MAX_H: f32 = 320.;
/// How much of the window the enlarged picture may take, each way, and how far a small
/// one is blown up to get there.
const PREVIEW: f32 = 0.9;
const PREVIEW_ZOOM: f32 = 2.;
/// How long the cursor takes from one click to the next.
const GLIDE: f32 = 0.35;
/// How long a click's ring takes to spread and fade, once the cursor is there.
const RING: f32 = 0.6;
/// The picture is this late before the caption says so: reading one over a good connection
/// takes far less.
const LATE: Duration = Duration::from_millis(1500);
/// A piece that took this long or longer came over a slow connection: the next is asked
/// for after a rest as long, `MAX_REST` at most.
const SLOW: Duration = Duration::from_millis(50);
const MAX_REST: Duration = Duration::from_secs(3);

/// A picture as gpui draws it, and its size in pixels.
type Picture = (Arc<RenderImage>, (f32, f32));

/// A piece of a picture, as a `ServerMessage::Screen` brings it: the part at `offset` of
/// the base64 text, `total` long, of the picture `frame`.
pub struct Piece<'a> {
    pub frame: u64,
    pub total: usize,
    pub offset: usize,
    pub data: &'a str,
}

/// A thread's screen, as far as this client has it.
pub struct Screen {
    /// The picture on show, and its id on the host.
    picture: Option<(u64, Picture)>,
    /// The picture `picture` replaced: the last frame drawn may still show it, so it leaves
    /// gpui's atlas only when the next one comes (as `ambient::Field` does).
    previous: Option<Arc<RenderImage>>,
    /// The id of the host's picture, as its events last said: ids only grow, so a greater
    /// one than the picture's on show is one to ask for.
    latest: u64,
    /// Not asked yet: a screen is asked for once before any event names a picture.
    fresh: bool,
    /// Our `GetScreen` on its way, and when it left. One at a time: the next is asked for
    /// when it is in.
    request: Option<(u64, Instant)>,
    /// The picture being read: its id, its base64 text so far, and how long the whole is.
    reading: Option<(u64, String, usize)>,
    /// Nothing is asked for before then (`rest`), and a timer is set to ask then.
    rest_until: Option<Instant>,
    timer: bool,
    /// Since when the picture on show is not the host's.
    behind: Option<Instant>,
    /// What the agent just did, in the host's words.
    action: Option<SharedString>,
    /// It did that on the desktop of the host's user, not on a screen of the thread's own.
    desktop: bool,
    /// Where it last clicked, and where before that, as fractions of the picture.
    cursor: Option<(f32, f32)>,
    from: Option<(f32, f32)>,
    /// How many clicks so far: each one's animation is its own.
    clicks: usize,
}

impl Screen {
    /// A screen just heard of.
    fn new() -> Self {
        Self {
            picture: None,
            previous: None,
            latest: 0,
            fresh: true,
            request: None,
            reading: None,
            rest_until: None,
            timer: false,
            behind: None,
            action: None,
            desktop: false,
            cursor: None,
            from: None,
            clicks: 0,
        }
    }

    fn shown(&self) -> u64 {
        self.picture.as_ref().map_or(0, |(frame, _)| *frame)
    }

    /// A `screen` event (`ServerMessage::Event`): the picture to show now, and what the
    /// agent did.
    fn step(&mut self, event: &Value) {
        self.latest = self.latest.max(event.get("frame").and_then(Value::as_u64).unwrap_or(0));
        if self.latest > self.shown() {
            self.behind.get_or_insert_with(Instant::now);
        }
        if let Some(action) = event.get("action").and_then(Value::as_str) {
            self.action = Some(action.to_owned().into());
        }
        self.desktop = event.get("desktop").and_then(Value::as_bool).unwrap_or(false);
        let point = |i: usize| event.get("cursor")?.get(i)?.as_f64().map(|v| v.clamp(0., 1.) as f32);
        if let Some(to) = point(0).zip(point(1)) {
            self.from = self.cursor.replace(to);
            self.clicks += 1;
        }
    }

    /// What to ask the host for now, as `GetScreen`'s `frame` and `offset`: the next piece
    /// of the picture being read, else the first of its latest if that is one we don't have.
    /// `None` while a request is on its way, or there is nothing to ask for.
    fn next(&self) -> Option<(u64, u64)> {
        if self.request.is_some() {
            return None;
        }
        match &self.reading {
            Some((frame, text, _)) => Some((*frame, text.len() as u64)),
            None => (self.fresh || self.latest > self.shown()).then_some((0, 0)),
        }
    }

    /// How long to wait before asking: what is left of the rest after a slow piece.
    fn rest(&self, now: Instant) -> Option<Duration> {
        self.rest_until.and_then(|until| until.checked_duration_since(now)).filter(|left| !left.is_zero())
    }

    fn asked(&mut self, request: u64, now: Instant) {
        self.request = Some((request, now));
        self.fresh = false;
    }

    /// The answer to our request. Returns the picture's whole text once this was its last
    /// piece.
    fn piece(&mut self, piece: &Piece, now: Instant) -> Option<(u64, String)> {
        let &Piece { frame, total, offset, data } = piece;
        // A piece that was slow to come is followed by a rest as long: the line is someone
        // else's for at least half the time. (A picture's first piece is also slow when its
        // host had to make the picture first: a rest of some tens of milliseconds.)
        let took = self.request.take().map_or(Duration::ZERO, |(_, at)| now.duration_since(at));
        self.rest_until = (took >= SLOW).then(|| now + took.min(MAX_REST));
        // No picture: the thread has no screen any more.
        if total == 0 {
            self.failed();
            return None;
        }
        match &mut self.reading {
            Some((reading, text, _)) if *reading == frame && text.len() == offset => text.push_str(data),
            _ if offset == 0 => self.reading = Some((frame, data.to_owned(), total)),
            // Not the piece we are at: start over.
            _ => self.reading = None,
        }
        let done = self.reading.as_ref().is_some_and(|(_, text, total)| text.len() >= *total);
        done.then(|| self.reading.take().map(|(frame, text, _)| (frame, text))).flatten()
    }

    /// A picture read to its end, on show. Returns the one to drop from gpui's atlas, if any.
    fn show(&mut self, frame: u64, picture: Picture) -> Option<Arc<RenderImage>> {
        self.latest = self.latest.max(frame);
        if self.latest == frame {
            self.behind = None;
        }
        let replaced = self.picture.replace((frame, picture)).map(|(_, (image, _))| image);
        std::mem::replace(&mut self.previous, replaced)
    }

    /// The host has no picture (the run just ended), or what came is none. Not asked again
    /// until an event names a new one.
    fn failed(&mut self) {
        self.request = None;
        self.reading = None;
        self.latest = self.shown();
        self.behind = None;
    }

    /// The pictures to drop from gpui's atlas when the screen goes.
    fn images(self) -> impl Iterator<Item = Arc<RenderImage>> {
        self.picture.map(|(_, (image, _))| image).into_iter().chain(self.previous)
    }
}

/// A picture off the wire (the base64 text of an image file), in gpui's BGRA.
fn decode(text: &str) -> Option<Picture> {
    let bytes = base64::engine::general_purpose::STANDARD.decode(text).ok()?;
    let mut rgba = image::load_from_memory(&bytes).ok()?.into_rgba8();
    for px in rgba.as_chunks_mut::<4>().0 {
        px.swap(0, 2);
    }
    let size = (rgba.width() as f32, rgba.height() as f32);
    Some((Arc::new(RenderImage::new([image::Frame::new(rgba)])), size))
}

impl Dirigent {
    /// Keep the open thread's screen in step with its host: it has one once its host says
    /// so (`ThreadSummary::screen`), and while it is on show the next piece of its picture
    /// is asked for when there is one to ask for and none on its way (`Screen::next`), once
    /// the rest after a slow piece is over. Called every frame, so whatever changes any of
    /// that (the thread opening, its host's lists and events, a piece coming in, the panel
    /// opening, a rest ending) is covered.
    pub fn sync_screen(&mut self, cx: &mut Context<Self>) {
        let Pane::Thread(t) = &self.pane else { return };
        let (host, id) = (t.host.clone(), t.id.clone());
        // A picture begun is read to its end, which its host keeps for us; a new one is
        // only there while the run is. Offline, the request would be dropped.
        let live = self.summary(&host, &id).is_some_and(|s| s.screen);
        if self.host(&host).is_none_or(|h| h.status != HostStatus::Connected) {
            return;
        }
        if live && let Pane::Thread(t) = &mut self.pane {
            t.screen.get_or_insert_with(Screen::new);
        }
        // Not while nobody looks: the panel closed by hand.
        if !(self.panel_open() || self.screen_preview) {
            return;
        }
        let Pane::Thread(t) = &mut self.pane else { return };
        let Some(screen) = &mut t.screen else { return };
        let Some((frame, offset)) = screen.next().filter(|(_, offset)| live || *offset > 0) else { return };
        let now = Instant::now();
        if let Some(rest) = screen.rest(now) {
            if !std::mem::replace(&mut screen.timer, true) {
                cx.spawn(async move |this, cx| {
                    cx.background_executor().timer(rest).await;
                    this.update(cx, |this, cx| {
                        if let Pane::Thread(t) = &mut this.pane
                            && let Some(screen) = &mut t.screen
                        {
                            screen.timer = false;
                        }
                        cx.notify();
                    })
                    .ok();
                })
                .detach();
            }
            return;
        }
        let request = self.request(&host, |id_| ClientMessage::GetScreen { id: id_, thread_id: id, frame, offset });
        if let Pane::Thread(t) = &mut self.pane
            && let Some(screen) = &mut t.screen
        {
            screen.asked(request, now);
        }
    }

    /// A `screen` event of a thread on `host`: a step of its agent's computer use.
    pub fn on_screen_event(&mut self, host: &str, thread_id: &str, event: &Value) {
        if let Pane::Thread(t) = &mut self.pane
            && t.host == host
            && t.id == thread_id
        {
            t.screen.get_or_insert_with(Screen::new).step(event);
        }
    }

    /// A `Screen` reply: the piece we asked for, if it is still the open thread's. The
    /// last piece of a picture puts it on show.
    pub fn on_screen(&mut self, host: &str, reply_to: u64, piece: &Piece, cx: &mut Context<Self>) {
        let Pane::Thread(t) = &mut self.pane else { return };
        let ours = |s: &&mut Screen| t.host == host && s.request.is_some_and(|(id, _)| id == reply_to);
        let Some(screen) = t.screen.as_mut().filter(ours) else { return };
        if let Some((frame, text)) = screen.piece(piece, Instant::now()) {
            match decode(&text) {
                Some(picture) => {
                    if let Some(old) = screen.show(frame, picture) {
                        cx.drop_image(old, None);
                    }
                }
                None => screen.failed(),
            }
        }
    }

    /// The open thread's screen goes (the thread is left, or its host went away), and its
    /// pictures leave gpui's atlas: deferred, as `drop_image` skips a window in the middle
    /// of an update, which a click that leaves the thread is.
    pub fn drop_screen(&mut self, cx: &mut Context<Self>) {
        self.screen_preview = false;
        let Pane::Thread(t) = &mut self.pane else { return };
        let images: Vec<_> = t.screen.take().into_iter().flat_map(Screen::images).collect();
        if !images.is_empty() {
            cx.defer(move |cx| images.into_iter().for_each(|image| cx.drop_image(image, None)));
        }
    }

    /// Enlarge the screen over the window, or put it back.
    pub fn set_screen_preview(&mut self, on: bool, cx: &mut Context<Self>) {
        self.screen_preview = on;
        if !on {
            self.focus_composer(cx);
        }
        cx.notify();
    }
}

/// The cursor over a picture: an arrow that glides to where the agent last clicked, and a
/// ring spreading from its tip once it is there. A child for the picture's box (which is
/// `relative`). `ids` tell the panel's from the preview's.
fn pointer(screen: &Screen, ids: [&'static str; 2]) -> Option<impl IntoElement + use<>> {
    let to = screen.cursor?;
    let from = screen.from.unwrap_or(to);
    let n = screen.clicks;
    let t = theme::t();
    // White under the accent: one of them stands out on any window. The arrow's tip is
    // a few pixels into its box.
    let arrow = |name: &str, color: u32| {
        svg().path(format!("icons/{name}.svg")).absolute().left(u(-4.)).top(u(-3.)).size(u(20.)).text_color(rgba(color))
    };
    let ring =
        div().absolute().rounded_full().border_2().border_color(rgba(t.accent_ink)).bg(rgba(t.accent_ink & 0xffffff40)).with_animation(
            (ids[0], n),
            Animation::new(Duration::from_secs_f32(GLIDE + RING)),
            move |d, v| {
                // Nothing while the cursor is on its way.
                let k = ((v * (GLIDE + RING) - GLIDE) / RING).clamp(0., 1.);
                let r = 4. + 20. * k;
                d.left(u(-r)).top(u(-r)).size(u(2. * r)).opacity(if k > 0. { 1. - k } else { 0. })
            },
        );
    let cursor = div().absolute().size_0().child(ring).child(arrow("pointer-outline", 0xffffffff)).child(arrow("pointer", t.accent_ink));
    Some(cursor.with_animation((ids[1], n), Animation::new(Duration::from_secs_f32(GLIDE)).with_easing(ease_out_quint()), move |d, v| {
        d.left(relative(from.0 + (to.0 - from.0) * v)).top(relative(from.1 + (to.1 - from.1) * v))
    }))
}

/// The screen's part of the right panel, on top of it: the picture, as wide as the panel
/// (`MAX_H` tall at most), and a line under it with what the agent just did. `None` while
/// the open thread has no screen.
pub fn section(this: &Dirigent, cx: &mut Context<Dirigent>) -> Option<Div> {
    let open = this.open()?;
    let screen = open.screen.as_ref()?;
    let live = this.summary(&open.host, &open.id).is_some_and(|s| s.screen);
    let t = theme::t();
    let frame = |d: Div| d.rounded_md().border_1().border_color(rgba(t.line)).bg(rgba(t.canvas)).overflow_hidden();
    // In a plain block parent, its width capped so its height is: `thread::inline_image`
    // has why.
    let picture = match &screen.picture {
        // The cursor lies over the frame, not in it: at the picture's edge it isn't cut off.
        Some((_, (image, (w, h)))) => div()
            .id("screen")
            .relative()
            .w_full()
            .max_w(u(w * MAX_H / h))
            .mx_auto()
            .aspect_ratio(w / h)
            .cursor_pointer()
            .on_click(cx.listener(|this, _, _, cx| this.set_screen_preview(true, cx)))
            .child(frame(div()).size_full().child(img(image.clone()).size_full()))
            .children(pointer(screen, ["screen-ring", "screen-cursor"]))
            .into_any_element(),
        // Asked for, not in yet.
        None => frame(div()).w_full().aspect_ratio(16. / 10.).into_any_element(),
    };
    // A run that ended took its last pictures with it: nothing to catch up with.
    let late = live && screen.behind.is_some_and(|since| since.elapsed() > LATE);
    let caption = div()
        .h(u(22.))
        .mt_1()
        .flex()
        .flex_row()
        .items_center()
        .gap_1p5()
        .text_size(text::sm())
        .text_color(rgba(t.fg_1))
        // Lit while the agent is at it, amber on the user's own desktop; the picture of a run
        // that ended is its last.
        .child(div().size(u(6.)).flex_shrink_0().rounded_full().bg(rgba(match (live, screen.desktop) {
            (false, _) => t.fg_2,
            (true, true) => t.warn,
            (true, false) => t.working,
        })))
        .when(screen.desktop, |d| d.child(div().flex_shrink_0().text_color(rgba(t.warn)).child("your desktop")))
        .child(div().flex_1().min_w_0().truncate().child(screen.action.clone().unwrap_or_else(|| "Using the screen".into())))
        .when(late, |d| d.child(div().flex_shrink_0().text_color(rgba(t.fg_2)).child("catching up…")));
    Some(div().flex_shrink_0().px_3().pb_1().border_b_1().border_color(rgba(t.line_soft)).child(div().child(picture)).child(caption))
}

/// The screen enlarged over the whole window (`viewport`), on a dark scrim, as big as
/// `PREVIEW` and `PREVIEW_ZOOM` let it, and live like the panel's. Any click (or Esc)
/// closes it.
pub fn preview(this: &Dirigent, viewport: Size<Pixels>, cx: &mut Context<Dirigent>) -> Option<Stateful<Div>> {
    let screen = this.open()?.screen.as_ref().filter(|_| this.screen_preview)?;
    let (_, (image, (w, h))) = screen.picture.as_ref()?;
    let scale = (PREVIEW * f32::from(viewport.width) / w).min(PREVIEW * f32::from(viewport.height) / h).min(PREVIEW_ZOOM);
    let picture = div()
        .relative()
        .w(px(w * scale))
        .h(px(h * scale))
        .child(div().size_full().rounded_md().overflow_hidden().child(img(image.clone()).size_full()))
        .children(pointer(screen, ["preview-ring", "preview-cursor"]));
    Some(
        div()
            .id("screen-preview")
            .absolute()
            .inset_0()
            .flex()
            .items_center()
            .justify_center()
            .bg(rgba(0x000000d0))
            // Over the grab strip (`titlebar.rs`): a press at the top closes it too.
            .occlude()
            .cursor_pointer()
            .on_click(cx.listener(|this, _, _, cx| this.set_screen_preview(false, cx)))
            .child(picture),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn picture() -> Picture {
        (Arc::new(RenderImage::new([image::Frame::new(image::RgbaImage::new(1, 1))])), (1., 1.))
    }

    /// One request at a time: the pieces of a picture one after the other, then the next
    /// picture if events named a newer one meanwhile, and nothing when there is none.
    #[test]
    fn a_picture_is_read_a_piece_at_a_time() {
        let now = Instant::now();
        let mut s = Screen::new();
        // Just heard of: the host's latest is asked for, whatever it is.
        assert_eq!(s.next(), Some((0, 0)));
        s.asked(1, now);
        s.step(&json!({"type": "screen", "frame": 3}));
        s.step(&json!({"type": "screen", "frame": 4, "action": "click", "cursor": [0.25, 0.5]}));
        assert_eq!(s.next(), None);
        assert_eq!((s.action.as_deref(), s.cursor, s.clicks), (Some("click"), Some((0.25, 0.5)), 1));
        assert!(!s.desktop);
        // Picture 3 comes in two pieces; the second is asked for by its frame and offset.
        assert_eq!(s.piece(&Piece { frame: 3, total: 6, offset: 0, data: "abcd" }, now), None);
        assert_eq!(s.next(), Some((3, 4)));
        s.asked(2, now);
        assert_eq!(s.piece(&Piece { frame: 3, total: 6, offset: 4, data: "ef" }, now), Some((3, "abcdef".into())));
        s.show(3, picture());
        // An event named a newer one while it was read: that one next.
        assert!(s.behind.is_some());
        assert_eq!(s.next(), Some((0, 0)));
        s.asked(3, now);
        assert_eq!(s.piece(&Piece { frame: 4, total: 2, offset: 0, data: "gh" }, now), Some((4, "gh".into())));
        s.show(4, picture());
        assert!(s.next().is_none() && s.behind.is_none());
        // The same picture again (an action that changed nothing) is not asked for.
        s.step(&json!({"type": "screen", "frame": 4, "action": "key Return", "desktop": true}));
        assert_eq!(s.next(), None);
        assert!(s.desktop);
        // A request the host has no picture for (the run ended) isn't repeated.
        s.step(&json!({"type": "screen", "frame": 5}));
        s.asked(4, now);
        assert_eq!(s.piece(&Piece { frame: 0, total: 0, offset: 0, data: "" }, now), None);
        assert_eq!(s.next(), None);
    }

    /// A piece that was slow to come is followed by a rest as long (so pictures take half
    /// a slow line at most); a quick one by none.
    #[test]
    fn slow_pieces_are_spaced_out() {
        let now = Instant::now();
        let mut s = Screen::new();
        s.asked(1, now);
        s.piece(&Piece { frame: 1, total: 12, offset: 0, data: "abcd" }, now + Duration::from_millis(5));
        assert_eq!(s.rest(now + Duration::from_millis(5)), None);
        s.asked(3, now);
        let arrived = now + Duration::from_millis(800);
        s.piece(&Piece { frame: 1, total: 12, offset: 4, data: "efgh" }, arrived);
        assert_eq!(s.rest(arrived), Some(Duration::from_millis(800)));
        assert_eq!(s.rest(arrived + Duration::from_millis(800)), None);
    }
}
