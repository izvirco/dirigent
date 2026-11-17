use crate::app::{Dirigent, Pane};
use crate::net::HostStatus;
use crate::theme::{self, text, u};
use base64::Engine;
use dirigent_protocol::ClientMessage;
use dirigent_protocol::transcript::Event;
use gpui::{
    Animation, AnimationExt, Context, Div, IntoElement, Pixels, RenderImage, SharedString, Size, Stateful, div, ease_out_quint, img,
    prelude::*, px, relative, rgba, svg,
};
use std::sync::Arc;
use std::time::{Duration, Instant};

const MAX_H: f32 = 320.;
const PREVIEW: f32 = 0.9;
const PREVIEW_ZOOM: f32 = 2.;
const GLIDE: f32 = 0.35;
const RING: f32 = 0.6;
const LATE: Duration = Duration::from_millis(1500);
const SLOW: Duration = Duration::from_millis(50);
const MAX_REST: Duration = Duration::from_secs(3);

type Picture = (Arc<RenderImage>, (f32, f32));

// MUST KILL: rename: Piece's offset / total → base64_offset / base64_len, into the frame's base64 text
pub struct Piece<'a> {
    pub frame: u64,
    pub total: usize,
    pub offset: usize,
    pub data: &'a str,
}

pub struct Screen {
    picture: Option<(u64, Picture)>,
    // MUST KILL: rename: previous → on_last_frame
    previous: Option<Arc<RenderImage>>,
    latest: u64,
    // MUST KILL: rename: fresh → never_asked
    fresh: bool,
    request: Option<(u64, Instant)>,
    reading: Option<(u64, String, usize)>,
    rest_until: Option<Instant>,
    timer: bool,
    behind: Option<Instant>,
    action: Option<SharedString>,
    desktop: bool,
    cursor: Option<(f32, f32)>,
    from: Option<(f32, f32)>,
    clicks: usize,
}

impl Screen {
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

    fn step(&mut self, frame: u64, action: Option<&str>, cursor: Option<[f32; 2]>, desktop: bool) {
        self.latest = self.latest.max(frame);
        if self.latest > self.shown() {
            self.behind.get_or_insert_with(Instant::now);
        }
        if let Some(action) = action {
            self.action = Some(action.to_owned().into());
        }
        self.desktop = desktop;
        if let Some([x, y]) = cursor {
            self.from = self.cursor.replace((x.clamp(0., 1.), y.clamp(0., 1.)));
            self.clicks += 1;
        }
    }

    // MUST KILL: type: next's (0, 0) for "the host's latest picture" → an enum (Latest, Piece { frame, offset })
    fn next(&self) -> Option<(u64, u64)> {
        if self.request.is_some() {
            return None;
        }
        match &self.reading {
            Some((frame, text, _)) => Some((*frame, text.len() as u64)),
            None => (self.fresh || self.latest > self.shown()).then_some((0, 0)),
        }
    }

    fn rest(&self, now: Instant) -> Option<Duration> {
        self.rest_until.and_then(|until| until.checked_duration_since(now)).filter(|left| !left.is_zero())
    }

    fn asked(&mut self, request: u64, now: Instant) {
        self.request = Some((request, now));
        self.fresh = false;
    }

    fn piece(&mut self, piece: &Piece, now: Instant) -> Option<(u64, String)> {
        let &Piece { frame, total, offset, data } = piece;
        let took = self.request.take().map_or(Duration::ZERO, |(_, at)| now.duration_since(at));
        self.rest_until = (took >= SLOW).then(|| now + took.min(MAX_REST));
        if total == 0 {
            self.failed();
            return None;
        }
        match &mut self.reading {
            Some((reading, text, _)) if *reading == frame && text.len() == offset => text.push_str(data),
            _ if offset == 0 => self.reading = Some((frame, data.to_owned(), total)),
            _ => self.reading = None,
        }
        let done = self.reading.as_ref().is_some_and(|(_, text, total)| text.len() >= *total);
        done.then(|| self.reading.take().map(|(frame, text, _)| (frame, text))).flatten()
    }

    fn show(&mut self, frame: u64, picture: Picture) -> Option<Arc<RenderImage>> {
        self.latest = self.latest.max(frame);
        if self.latest == frame {
            self.behind = None;
        }
        let replaced = self.picture.replace((frame, picture)).map(|(_, (image, _))| image);
        std::mem::replace(&mut self.previous, replaced)
    }

    fn failed(&mut self) {
        self.request = None;
        self.reading = None;
        self.latest = self.shown();
        self.behind = None;
    }

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
    // MUST KILL: rearchitect: sync_screen polls every frame → ask from the events that change what to ask
    pub fn sync_screen(&mut self, cx: &mut Context<Self>) {
        let Pane::Thread(t) = &self.pane else { return };
        let (host, id) = (t.host.clone(), t.id.clone());
        let live = self.summary(&host, &id).is_some_and(|s| s.screen);
        if self.host(&host).is_none_or(|h| h.status != HostStatus::Connected) {
            return;
        }
        if live && let Pane::Thread(t) = &mut self.pane {
            t.screen.get_or_insert_with(Screen::new);
        }
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

    pub fn on_screen_event(&mut self, host: &str, thread_id: &str, event: &Event) {
        if let (Pane::Thread(t), Event::Screen { frame, action, cursor, desktop }) = (&mut self.pane, event)
            && t.host == host
            && t.id == thread_id
        {
            t.screen.get_or_insert_with(Screen::new).step(*frame, action.as_deref(), *cursor, *desktop);
        }
    }

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

    pub fn set_screen_preview(&mut self, on: bool, cx: &mut Context<Self>) {
        self.screen_preview = on;
        if !on {
            self.focus_composer(cx);
        }
        cx.notify();
    }
}

fn pointer(screen: &Screen, ids: [&'static str; 2]) -> Option<impl IntoElement + use<>> {
    let to = screen.cursor?;
    let from = screen.from.unwrap_or(to);
    let n = screen.clicks;
    let t = theme::t();
    let arrow = |name: &str, color: u32| {
        svg().path(format!("icons/{name}.svg")).absolute().left(u(-4.)).top(u(-3.)).size(u(20.)).text_color(rgba(color))
    };
    let ring =
        div().absolute().rounded_full().border_2().border_color(rgba(t.accent_ink)).bg(rgba(t.accent_ink & 0xffffff40)).with_animation(
            (ids[0], n),
            Animation::new(Duration::from_secs_f32(GLIDE + RING)),
            move |d, v| {
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

pub fn section(this: &Dirigent, cx: &mut Context<Dirigent>) -> Option<Div> {
    let open = this.open()?;
    let screen = open.screen.as_ref()?;
    let live = this.summary(&open.host, &open.id).is_some_and(|s| s.screen);
    let t = theme::t();
    let frame = |d: Div| d.rounded_md().border_1().border_color(rgba(t.line)).bg(rgba(t.canvas)).overflow_hidden();
    let picture = match &screen.picture {
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
        None => frame(div()).w_full().aspect_ratio(16. / 10.).into_any_element(),
    };
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
            .occlude()
            .cursor_pointer()
            .on_click(cx.listener(|this, _, _, cx| this.set_screen_preview(false, cx)))
            .child(picture),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn picture() -> Picture {
        (Arc::new(RenderImage::new([image::Frame::new(image::RgbaImage::new(1, 1))])), (1., 1.))
    }

    #[test]
    fn a_picture_is_read_a_piece_at_a_time() {
        let now = Instant::now();
        let mut s = Screen::new();
        assert_eq!(s.next(), Some((0, 0)));
        s.asked(1, now);
        s.step(3, None, None, false);
        s.step(4, Some("click"), Some([0.25, 0.5]), false);
        assert_eq!(s.next(), None);
        assert_eq!((s.action.as_deref(), s.cursor, s.clicks), (Some("click"), Some((0.25, 0.5)), 1));
        assert!(!s.desktop);
        assert_eq!(s.piece(&Piece { frame: 3, total: 6, offset: 0, data: "abcd" }, now), None);
        assert_eq!(s.next(), Some((3, 4)));
        s.asked(2, now);
        assert_eq!(s.piece(&Piece { frame: 3, total: 6, offset: 4, data: "ef" }, now), Some((3, "abcdef".into())));
        s.show(3, picture());
        assert!(s.behind.is_some());
        assert_eq!(s.next(), Some((0, 0)));
        s.asked(3, now);
        assert_eq!(s.piece(&Piece { frame: 4, total: 2, offset: 0, data: "gh" }, now), Some((4, "gh".into())));
        s.show(4, picture());
        assert!(s.next().is_none() && s.behind.is_none());
        s.step(4, Some("key Return"), None, true);
        assert_eq!(s.next(), None);
        assert!(s.desktop);
        s.step(5, None, None, false);
        s.asked(4, now);
        assert_eq!(s.piece(&Piece { frame: 0, total: 0, offset: 0, data: "" }, now), None);
        assert_eq!(s.next(), None);
    }

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
