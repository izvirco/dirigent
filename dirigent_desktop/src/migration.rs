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

const SIZE: (f32, f32) = (640., 620.);

// MUST KILL: extract: the port probe in needed() into a named service_running() check
pub fn needed() -> bool {
    let local = std::net::SocketAddr::from(([127, 0, 0, 1], dirigent_service::port()));
    dirigent_service::migration::pending() && std::net::TcpStream::connect_timeout(&local, Duration::from_millis(300)).is_err()
}

pub fn open(config: &Config, start: impl FnOnce(&mut App) + 'static, cx: &mut App) {
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

#[derive(Default)]
struct Told {
    steps: Vec<Step>,
    done: usize,
    // MUST KILL: type: threads as a struct (converted, total), not a tuple
    threads: (usize, usize),
    report: Option<Report>,
}

impl Told {
    fn event(&mut self, event: Event) {
        match event {
            Event::Steps(steps) => self.steps = steps,
            Event::Step(step) => self.done = self.steps.iter().position(|s| *s == step).unwrap_or(self.done),
            Event::Progress { done, total } => self.threads = (done, total),
            Event::Done(_) | Event::Left(_) => {}
        }
    }

    // MUST KILL: extract: the threads' weight 8 in fraction() as a named const
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

type Start = Box<dyn FnOnce(&mut App)>;

struct Migration {
    told: Told,
    font: SharedString,
    start: Option<Start>,
}

impl Migration {
    fn new(font: SharedString, start: Start, cx: &mut Context<Self>) -> Self {
        enum Heard {
            Event(Event),
            Report(Report),
        }
        let (tx, mut rx) = futures::channel::mpsc::unbounded();
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

    // MUST KILL: extract: go_on's start-then-remove order into a named fn: the app's window must open before this one closes, or the app quits
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
        told.event(Event::Step(Step::Threads));
        told.event(Event::Progress { done: 24, total: 96 });
        assert_eq!(told.fraction(), 0.3);
        told.event(Event::Step(Step::Settings));
        assert_eq!(told.fraction(), 0.9);
        told.report = Some(Report { error: Some("no room".into()), ..Default::default() });
        assert_eq!(told.fraction(), 0.9);
        told.report = Some(Report::default());
        assert_eq!(told.fraction(), 1.);
    }
}
