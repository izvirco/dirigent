//! The move from pi, shown: the first start of a build with its own agent opens on this
//! window, not on the app. It makes the migration (`dirigent_service::migration`: the
//! dbs and the config dir copied, the threads converted, pi's settings brought over) and
//! shows how far it is, then what could not be brought over and where the backups are. It
//! stays until Continue, which starts the app as on any other day.
//!
//! Nothing else of the app runs meanwhile: the desktop's own db is one of those copied.

use crate::config::Config;
use crate::icons::icon;
use crate::lists::scroll;
use crate::theme::{self, text, u};
use dirigent_service::migration::{Event, Report, Step};
use futures::StreamExt;
use gpui::{
    App, AppContext, Bounds, Context, FontWeight, IntoElement, ParentElement, Render, SharedString, StatefulInteractiveElement, Styled,
    TitlebarOptions, Window, WindowBounds, WindowOptions, div, px, relative, rgba, size,
};
use std::time::Duration;

/// The window's size: a dialog's, with room for what could not be brought over. (Where
/// the window is made larger, tiled or maximized, what it shows stays a column this wide.)
const SIZE: (f32, f32) = (640., 620.);

/// The app is to start with the migration: this install is as pi left it, and no service
/// runs on it (one that does has made the migration, or is pi's still and has the data).
pub fn needed() -> bool {
    let local = std::net::SocketAddr::from(([127, 0, 0, 1], dirigent_service::port()));
    dirigent_service::migration::pending() && std::net::TcpStream::connect_timeout(&local, Duration::from_millis(300)).is_err()
}

/// Open the window and make the migration. `start` starts the app, once it is through and
/// Continue was clicked.
pub fn open(config: &Config, start: impl FnOnce(&mut App) + 'static, cx: &mut App) {
    // As the app will look (it sets both again when it starts).
    if let Ok(theme) = theme::load(&config.theme) {
        theme::set(theme);
    }
    theme::set_text_size(config.fonts.size());
    let font = config.fonts.resolve(cx).0.ui;
    let options = WindowOptions {
        window_bounds: Some(WindowBounds::Windowed(Bounds::centered(None, size(px(SIZE.0), px(SIZE.1)), cx))),
        app_id: Some(dirigent_protocol::release::app().into()),
        titlebar: Some(TitlebarOptions { title: Some("Dirigent".into()), ..Default::default() }),
        ..Default::default()
    };
    let opened = cx.open_window(options, |_, cx| cx.new(|cx| Migration::new(font, Box::new(start), cx)));
    if let Err(e) = opened {
        tracing::error!("the migration's window: {e:#}");
    }
    cx.activate(true);
}

/// What the migration has told so far.
#[derive(Default)]
struct Told {
    steps: Vec<Step>,
    /// How many of `steps` are done.
    done: usize,
    /// Threads converted, of how many.
    threads: (usize, usize),
    /// What it came to, once it is through.
    report: Option<Report>,
}

impl Told {
    fn event(&mut self, event: Event) {
        match event {
            Event::Steps(steps) => self.steps = steps,
            Event::Step(step) => self.done = self.steps.iter().position(|s| *s == step).unwrap_or(self.done),
            Event::Progress { done, total } => self.threads = (done, total),
            // In the report, which comes last.
            Event::Done(_) | Event::Left(_) => {}
        }
    }

    /// How far it is, 0 to 1 (and stays, where it stopped). The threads are most of it; a
    /// step is all or nothing but for them.
    fn fraction(&self) -> f32 {
        if self.report.as_ref().is_some_and(|report| report.error.is_none()) {
            return 1.;
        }
        let weight = |step: &Step| if *step == Step::Threads { 8. } else { 1. };
        let all: f32 = self.steps.iter().map(weight).sum();
        let before: f32 = self.steps.iter().take(self.done).map(weight).sum();
        let within = match self.steps.get(self.done) {
            Some(Step::Threads) if self.threads.1 > 0 => 8. * self.threads.0 as f32 / self.threads.1 as f32,
            _ => 0.,
        };
        if all > 0. { (before + within) / all } else { 0. }
    }
}

/// What starts the app.
type Start = Box<dyn FnOnce(&mut App)>;

struct Migration {
    told: Told,
    font: SharedString,
    /// Continue's.
    start: Option<Start>,
}

impl Migration {
    fn new(font: SharedString, start: Start, cx: &mut Context<Self>) -> Self {
        enum Heard {
            Event(Event),
            Report(Report),
        }
        let (tx, mut rx) = futures::channel::mpsc::unbounded();
        // On a thread of its own: it copies and converts whole dbs.
        let spawned = std::thread::Builder::new().name("migration".into()).spawn(move || {
            let report = dirigent_service::migration::run(|event| drop(tx.unbounded_send(Heard::Event(event))));
            let _ = tx.unbounded_send(Heard::Report(report));
        });
        if let Err(e) = spawned {
            tracing::error!("the migration's thread: {e}");
        }
        cx.spawn(async move |this, cx| {
            while let Some(heard) = rx.next().await {
                let shown = this.update(cx, |this, cx| {
                    match heard {
                        Heard::Event(event) => this.told.event(event),
                        Heard::Report(report) => {
                            // For the log too: the window is gone with a click.
                            match &report.error {
                                Some(error) => tracing::error!("the migration from pi stopped: {error}"),
                                None => tracing::info!("the migration from pi is made"),
                            }
                            report.left.iter().for_each(|line| tracing::warn!("migration: {line}"));
                            this.told.report = Some(report);
                        }
                    }
                    cx.notify();
                });
                if shown.is_err() {
                    break;
                }
            }
        })
        .detach();
        Self { told: Told::default(), font, start: Some(start) }
    }

    /// Continue: the app starts, and this window goes once its own is there (the last
    /// window to close would take the app with it).
    fn go_on(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(start) = self.start.take() else { return };
        let this = window.window_handle();
        cx.defer(move |cx| {
            start(cx);
            let _ = this.update(cx, |_, window, _| window.remove_window());
        });
    }
}

impl Render for Migration {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let t = theme::t();
        let told = &self.told;
        let report = told.report.as_ref();
        let failed = report.and_then(|report| report.error.clone());
        let heading = match (report, &failed) {
            (None, _) => "Bringing your threads and settings over",
            (Some(_), None) => "Your threads and settings are brought over",
            (Some(_), Some(_)) => "The migration stopped",
        };
        let steps = told.steps.iter().enumerate().map(|(ix, step)| {
            let state = match (report, &failed) {
                (Some(_), None) => State::Done,
                // The step it stopped in.
                (Some(_), Some(_)) if ix == told.done => State::Failed,
                _ if ix < told.done => State::Done,
                _ if ix == told.done && report.is_none() => State::Going,
                _ => State::Waiting,
            };
            let (label, detail) = match step {
                Step::Backup => ("A copy of what is there now", None),
                Step::Threads => ("Threads", (told.threads.1 > 0).then(|| format!("{} of {}", told.threads.0, told.threads.1))),
                Step::Settings => ("Settings: the proxy, instructions, skills, MCP servers", None),
            };
            step_row(state, label, detail)
        });
        let fill = if failed.is_some() { t.error } else { t.accent_line };
        let bar = div()
            .w_full()
            .h(u(6.))
            .rounded_full()
            .bg(rgba(t.line))
            .child(div().h_full().rounded_full().bg(rgba(fill)).w(relative(told.fraction().clamp(0., 1.))));
        let outcome = report.map(|report| {
            let backups = report.backups.iter().map(|backup| backup.display().to_string()).collect::<Vec<_>>();
            theme::stack()
                .gap(u(14.))
                .children(failed.clone().map(|error| {
                    theme::stack().gap_1().child(div().text_color(rgba(t.error)).child(error)).child(
                        div()
                            .text_color(rgba(t.fg_1))
                            .child("Nothing is lost: what was not converted is as it was. Start Dirigent again once this is fixed."),
                    )
                }))
                .children((!report.left.is_empty()).then(|| {
                    theme::stack().gap_1p5().child(div().text_color(rgba(t.fg_1)).child("Not brought over:")).children(
                        report.left.iter().map(|line| {
                            div()
                                .w_full()
                                .flex()
                                .flex_row()
                                .gap_2()
                                .child(div().mt(u(7.)).child(theme::dot(t.warn)))
                                .child(div().flex_1().min_w_0().child(line.clone()))
                        }),
                    )
                }))
                .children(
                    (failed.is_none() && report.left.is_empty()).then(|| div().text_color(rgba(t.fg_1)).child("Everything came over.")),
                )
                .children((!backups.is_empty()).then(|| {
                    theme::stack()
                        .gap_1()
                        .text_size(text::sm())
                        .text_color(rgba(t.fg_2))
                        .child("What was there before, and this report (migration.txt), is in:")
                        .children(backups.into_iter().map(|backup| div().text_color(rgba(t.fg_1)).child(backup)))
                        // (One that stopped goes on with them.)
                        .children(failed.is_none().then_some("Delete those once you have no use for them."))
                }))
        });
        let button = report.map(|_| {
            let (id, label) = if failed.is_some() { ("migration-quit", "Quit") } else { ("migration-continue", "Continue") };
            crate::onboarding::primary(id, label).on_click(cx.listener(move |this, _, window, cx| {
                if this.told.report.as_ref().is_some_and(|report| report.error.is_some()) {
                    cx.quit();
                } else {
                    this.go_on(window, cx);
                }
            }))
        });
        let column = div()
            .w_full()
            .max_w(px(SIZE.0))
            .h_full()
            .flex()
            .flex_col()
            .p(u(28.))
            .gap(u(18.))
            .child(
                theme::stack()
                    .gap_1()
                    .child(div().text_size(text::lg()).font_weight(FontWeight::MEDIUM).child(heading))
                    .child(div().text_color(rgba(t.fg_1)).child("Dirigent runs its own agent now. This happens once.")),
            )
            .child(bar)
            .child(theme::stack().gap_1p5().children(steps))
            .child(scroll("migration-outcome", theme::stack().children(outcome)))
            .child(div().w_full().flex().flex_row().justify_end().children(button));
        div()
            .size_full()
            .flex()
            .flex_col()
            .items_center()
            .bg(rgba(t.canvas))
            .text_color(rgba(t.fg))
            .font_family(self.font.clone())
            .text_size(text::base())
            .child(column)
    }
}

#[derive(Clone, Copy, PartialEq)]
enum State {
    Waiting,
    Going,
    Done,
    Failed,
}

/// A step: its state's mark, what it is, and how far it is.
fn step_row(state: State, label: &'static str, detail: Option<String>) -> impl IntoElement {
    let t = theme::t();
    let mark = match state {
        State::Waiting => theme::dot(t.line).into_any_element(),
        State::Going => theme::dot(t.working).into_any_element(),
        State::Done => icon("check", t.done).size(u(12.)).into_any_element(),
        State::Failed => icon("x", t.error).size(u(12.)).into_any_element(),
    };
    div()
        .w_full()
        .flex()
        .flex_row()
        .items_center()
        .gap_2()
        .text_color(rgba(if state == State::Waiting { t.fg_2 } else { t.fg }))
        .child(div().w(u(14.)).flex().justify_center().child(mark))
        .child(label)
        .children(detail.map(|detail| div().text_color(rgba(t.fg_1)).child(detail)))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_bar_follows_the_steps_and_the_threads() {
        let mut told = Told::default();
        assert_eq!(told.fraction(), 0.);
        told.event(Event::Steps(vec![Step::Backup, Step::Threads, Step::Settings]));
        told.event(Event::Step(Step::Backup));
        assert_eq!(told.fraction(), 0.);
        // The threads are eight of its ten parts.
        told.event(Event::Step(Step::Threads));
        told.event(Event::Progress { done: 24, total: 96 });
        assert_eq!(told.fraction(), 0.3);
        told.event(Event::Step(Step::Settings));
        assert_eq!(told.fraction(), 0.9);
        // One that stopped stays where it is; one that is through is all of it.
        told.report = Some(Report { error: Some("no room".into()), ..Default::default() });
        assert_eq!(told.fraction(), 0.9);
        told.report = Some(Report::default());
        assert_eq!(told.fraction(), 1.);
    }
}
