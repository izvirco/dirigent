//! Main-pane pages that list threads: Inbox, Workpool, one project. Plus the shared
//! 40px header bar, the new-project form and the settings pages (Hosts here, Config in
//! `settings.rs`, Usage in `usage.rs`; `settings_header` carries the tabs between them).

use crate::app::{Dirigent, Pane, Target};
use crate::icons::icon;
use crate::menu::Side;
use crate::net::HostStatus;
use crate::theme::{self, Status};
use crate::{hosts, projects, thread};
use dirigent_protocol::ThreadSummary;
use gpui::{AnyElement, Context, Div, FontWeight, SharedString, Stateful, div, prelude::*, px, rgba};

pub fn render(this: &Dirigent, cx: &mut Context<Dirigent>) -> Div {
    let pane = div().flex_1().min_w_0().h_full().flex().flex_col();
    match &this.pane {
        Pane::Inbox => {
            let rows = this.inbox_all();
            pane.child(header(div().child("Inbox"), None)).child(if rows.is_empty() {
                theme::empty("Nothing waiting", "Threads land here when their agent finishes or asks you something.").into_any_element()
            } else {
                scroll("inbox", thread_list(&rows, true, this, cx)).into_any_element()
            })
        }
        Pane::Workpool => {
            let rows = this.busy_threads();
            pane.child(header(div().child("Workpool"), None)).child(if rows.is_empty() {
                theme::empty("No agents working", "Start one with ctrl n.").into_any_element()
            } else {
                scroll("workpool", thread_list(&rows, true, this, cx)).into_any_element()
            })
        }
        Pane::Project { group_key } => project_page(this, group_key, pane, cx),
        Pane::NewProject { host } => pane.child(header(div().child(format!("New project on {host}")), None)).child(
            div()
                .flex()
                .flex_col()
                .gap_3()
                .p_6()
                .max_w(px(560.))
                .child(div().text_color(rgba(theme::t().fg_1)).child("Absolute path of a directory on that host. Enter to add it."))
                .child(
                    div()
                        .w_full()
                        .px_3()
                        .py_2()
                        .rounded_md()
                        .bg(rgba(theme::t().raised))
                        .border_1()
                        .border_color(rgba(theme::t().line))
                        .child(this.path_input.clone()),
                ),
        ),
        Pane::Config => crate::settings::render(this, pane, cx),
        Pane::Settings => settings_page(this, pane, cx),
        Pane::Usage => crate::usage::render(this, pane, cx),
        Pane::Thread(_) | Pane::Draft { .. } => pane,
    }
}

/// Settings: a narrow centered column of sections. For now just Hosts: every host with
/// its url and status (hover reveals remove), then the add form.
fn settings_page(this: &Dirigent, pane: Div, cx: &mut Context<Dirigent>) -> Div {
    let rows = this.hosts.iter().map(|h| {
        let (dot, status) = match h.status {
            HostStatus::Connected => (theme::t().done, "connected"),
            HostStatus::Connecting => (theme::t().warn, "connecting"),
            HostStatus::Disconnected => (theme::t().error, "disconnected"),
        };
        let name = h.name.clone();
        div()
            .id(SharedString::from(format!("host/{name}")))
            .group("row")
            .flex()
            .flex_row()
            .items_center()
            .gap_3()
            .h(px(38.))
            .px_2()
            .border_b_1()
            .border_color(rgba(theme::t().line_soft))
            .child(div().size(px(6.)).rounded_full().bg(rgba(dot)))
            .child(div().w(px(110.)).truncate().font_weight(FontWeight::MEDIUM).child(name.clone()))
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .truncate()
                    .font_family(this.fonts.mono.clone())
                    .text_size(px(12.))
                    .text_color(rgba(theme::t().fg_1))
                    .child(h.url.clone()),
            )
            .child(div().w(px(90.)).flex_shrink_0().text_size(px(12.)).text_color(rgba(theme::t().fg_2)).child(status))
            .child(if name == hosts::LOCAL {
                div().size(px(24.)).into_any_element()
            } else {
                div()
                    .id(SharedString::from(format!("rm-host/{name}")))
                    .size(px(24.))
                    .flex()
                    .items_center()
                    .justify_center()
                    .rounded_md()
                    .cursor_pointer()
                    .opacity(0.)
                    .group_hover("row", |d| d.opacity(1.))
                    .hover(|d| d.bg(rgba(theme::t().hover)))
                    .on_click(cx.listener(move |this, _, _, cx| this.remove_host(&name, cx)))
                    .child(icon("x", theme::t().fg_1))
                    .into_any_element()
            })
    });
    let field = |input: gpui::Entity<crate::input::TextInput>| {
        div().px_3().py_2().rounded_md().bg(rgba(theme::t().raised)).border_1().border_color(rgba(theme::t().line)).child(input)
    };
    let form = div()
        .flex()
        .flex_col()
        .gap_3()
        .pt_4()
        .child(
            div()
                .text_color(rgba(theme::t().fg_1))
                .child("Add a host running dirigent_service, reachable over your tailnet. Enter to add it."),
        )
        .child(
            div()
                .flex()
                .flex_row()
                .gap_2()
                .child(field(this.host_name_input.clone()).w(px(150.)))
                .child(field(this.host_url_input.clone()).flex_1()),
        )
        .child(
            div().text_size(px(12.)).text_color(rgba(theme::t().fg_2)).child(format!("Also editable by hand: {}", hosts::path().display())),
        );

    let hosts = settings_section("Hosts", div().flex().flex_col().children(rows).child(form));
    let column = div().w_full().max_w(px(600.)).flex().flex_col().gap_8().py_8().px_6().child(hosts);
    pane.child(settings_header(Pane::Settings, cx)).child(scroll("settings", div().flex().flex_col().items_center().child(column)))
}

/// The settings pages' header: `Settings` and the tabs (Config, Hosts, Usage), the open one bright.
pub fn settings_header(active: Pane, cx: &mut Context<Dirigent>) -> Div {
    let title = div()
        .flex()
        .flex_row()
        .items_center()
        .gap_1()
        .child(div().mr_3().child("Settings"))
        .child(theme::tab("Config", matches!(active, Pane::Config), cx, |this, cx| this.show_config(cx)))
        .child(theme::tab("Hosts", matches!(active, Pane::Settings), cx, |this, cx| this.show_settings(cx)))
        .child(theme::tab("Usage", matches!(active, Pane::Usage), cx, |this, cx| this.show_usage(cx)));
    header(title, None)
}

/// A settings section: title over its content.
pub fn settings_section(title: &'static str, body: impl IntoElement) -> Div {
    div().flex().flex_col().gap_2().child(div().font_weight(FontWeight::MEDIUM).text_color(rgba(theme::t().fg)).child(title)).child(body)
}

/// The main pane's top bar: a title on the left, optional actions on the right.
pub fn header(title: impl IntoElement, actions: Option<AnyElement>) -> Div {
    div()
        .flex()
        .flex_row()
        .items_center()
        .h(px(40.))
        .flex_shrink_0()
        .px_4()
        .border_b_1()
        .border_color(rgba(theme::t().line_soft))
        .font_weight(FontWeight::MEDIUM)
        .child(div().flex_1().min_w_0().flex().flex_row().items_center().gap_1p5().child(title))
        .children(actions)
}

/// A sub-agent's parent, a segment of its breadcrumb.
pub struct Via {
    pub host: String,
    pub id: String,
    pub name: SharedString,
    pub status: Status,
}

/// `project > via > title`, with the project clickable, `via` (a sub-agent's parent, its
/// glyph and name opening it) an optional segment of its own, and before the title the
/// thread's `status` glyph and an amber pin while `pinned`. The title breathes while
/// `titling` (its name is being generated).
/// Nothing truncates while it all fits. When space runs out the parent's name gives way
/// first, down to nothing, then the title; the project, chevrons and glyphs keep their width.
pub fn breadcrumb(
    project: Option<(String, String)>,
    via: Option<Via>,
    status: Option<Status>,
    title: impl Into<SharedString>,
    pinned: bool,
    titling: bool,
    cx: &mut Context<Dirigent>,
) -> Div {
    let has_project = project.is_some();
    let chevron = || icon("chevron-right", theme::t().fg_2).size(px(12.)).flex_shrink_0();
    let row = || div().flex().flex_row().items_center().gap_1().min_w_0();
    // The title has priority: the parent's name and the title's part (`tail`) share a row,
    // and `tail` doesn't shrink but is capped at that row's width (its `min_w_0` matters:
    // taffy lets min override max), so the parent's name takes the squeeze until it's
    // gone, and only then does the title truncate.
    let (via_glyph, via_name) = via
        .map(|v| {
            let open = |id: &'static str| {
                let (h, pid) = (v.host.clone(), v.id.clone());
                div().id(id).cursor_pointer().on_click(cx.listener(move |this, _, _, cx| this.select_thread(&h, &pid, cx)))
            };
            (
                open("crumb-parent-glyph").flex_shrink_0().child(v.status.glyph()),
                open("crumb-parent")
                    .min_w_0()
                    .truncate()
                    .text_color(rgba(theme::t().fg_1))
                    .hover(|d| d.text_color(rgba(theme::t().fg)))
                    .child(v.name),
            )
        })
        .unzip();
    let tail = row()
        .flex_shrink_0()
        .max_w_full()
        .when(via_name.is_some(), |d| d.child(chevron()))
        .children(status.map(|s| div().flex_shrink_0().child(s.glyph())))
        .children(pinned.then(|| div().flex_shrink_0().child(theme::pin_mark())))
        .child(theme::title_text("crumb-title", title, titling));
    row()
        .children(project.map(|(key, name)| {
            div()
                .id("crumb-project")
                .flex_shrink_0()
                .text_color(rgba(theme::t().fg_1))
                .cursor_pointer()
                .hover(|d| d.text_color(rgba(theme::t().fg)))
                .on_click(cx.listener(move |this, _, _, cx| this.show(Pane::Project { group_key: key.clone() }, cx)))
                .child(name)
        }))
        .when(has_project, |d| d.child(chevron()))
        .children(via_glyph)
        .child(row().children(via_name).child(tail))
}

fn project_page(this: &Dirigent, group_key: &str, pane: Div, cx: &mut Context<Dirigent>) -> Div {
    let (groups, orphans) = this.groups();
    let (name, mut threads, target): (String, Vec<(&str, &ThreadSummary)>, Option<Target>) = if group_key == projects::ORPHANS {
        ("Removed projects".into(), orphans.iter().map(|(h, t)| (h.as_str(), t)).collect(), None)
    } else {
        let Some(g) = groups.iter().find(|g| g.key == group_key) else {
            return pane
                .child(header(div().child("Project"), None))
                .child(theme::empty("This project is gone", "It was removed on its host."));
        };
        (g.name.clone(), g.threads.iter().map(|(h, t)| (h.as_str(), t)).collect(), Some(g.member_for(None).into()))
    };
    let paths = groups
        .iter()
        .find(|g| g.key == group_key)
        .map(|g| g.members.iter().map(|m| format!("{}  {}", m.host, m.project.path)).collect::<Vec<_>>())
        .unwrap_or_default();

    this.sort_as_queue(&mut threads);
    let (active, archived): (Vec<_>, Vec<_>) = threads.into_iter().partition(|(_, t)| !t.archived);
    let new_thread = (group_key != projects::ORPHANS).then(|| {
        theme::button("project-new-thread", "plus", "New thread")
            .on_click(cx.listener(move |this, _, _, cx| this.new_draft(target.clone(), cx)))
            .into_any_element()
    });

    let title = breadcrumb(None, None, None, name, false, false, cx);
    let mut body = div().id("project-page").flex_1().overflow_y_scroll().flex().flex_col();
    if !paths.is_empty() {
        body = body.child(
            div()
                .px_4()
                .py_2()
                .border_b_1()
                .border_color(rgba(theme::t().line_soft))
                .text_size(px(12.))
                .text_color(rgba(theme::t().fg_2))
                .children(paths.into_iter().map(|p| div().child(p))),
        );
    }
    if active.is_empty() && archived.is_empty() {
        body = body.child(theme::empty("No threads yet", "Start one with New thread."));
    } else {
        body = body.child(thread_list(&active, false, this, cx));
        if !archived.is_empty() {
            body = body.child(section_label(format!("Archived  {}", archived.len()))).child(thread_list(&archived, false, this, cx));
        }
    }
    pane.child(header(title, new_thread)).child(body)
}

/// A band between list sections (`Archived`): the canvas colour at 40% over the panel, a
/// shade darker than the rows without going solid on a see-through theme.
pub fn section_label(text: String) -> Div {
    div()
        .h(px(32.))
        .px_4()
        .flex()
        .items_center()
        .text_size(px(12.))
        .text_color(rgba(theme::t().fg_2))
        .bg(rgba(theme::alpha(theme::t().canvas, 0x66)))
        .child(text)
}

pub fn scroll(id: &'static str, content: Div) -> Stateful<Div> {
    div().id(id).flex_1().overflow_y_scroll().flex().flex_col().child(content)
}

/// Flat rows: status glyph, title (amber while asking, accent while unread, an amber pin before it while pinned),
/// project, model, age. Hover reveals snooze (finished threads; snoozed ones carry the
/// moon glyph) and archive.
fn thread_list(rows: &[(&str, &ThreadSummary)], show_project: bool, this: &Dirigent, cx: &mut Context<Dirigent>) -> Div {
    let now = std::time::UNIX_EPOCH.elapsed().map(|d| d.as_millis() as i64).unwrap_or(0);
    let show_host = this.hosts.len() > 1;
    div().flex().flex_col().children(rows.iter().map(|(host, t)| thread_row(host, t, show_host, show_project, now, this, cx)))
}

fn thread_row(
    host: &str,
    t: &ThreadSummary,
    show_host: bool,
    show_project: bool,
    now: i64,
    this: &Dirigent,
    cx: &mut Context<Dirigent>,
) -> Stateful<Div> {
    let (h, id) = (host.to_owned(), t.id.clone());
    let title = t.name.clone().unwrap_or_else(|| "Untitled".into());
    let status = this.status(host, t);
    let project = if show_project { this.project_name(host, t) } else { String::new() };
    let model = t.model.as_deref().map(theme::short_model).unwrap_or("").to_owned();
    let unread = this.unread(host, t);
    let (h2, id2, archived) = (h.clone(), id.clone(), t.archived);
    let (h3, id3, snoozed) = (h.clone(), id.clone(), status == Status::Snoozed);
    let pinned = t.pinned;
    let archive_icon = if archived { "archive-restore" } else { "archive" };
    let archive_confirm = thread::archive_confirm(host, &t.id, Side::BelowRight(24.), this, cx);

    div()
        .id(SharedString::from(format!("row/{h}/{id}")))
        .group("row")
        .flex()
        .flex_row()
        .items_center()
        .gap_3()
        .h(px(38.))
        .px_4()
        .border_b_1()
        .border_color(rgba(theme::t().line_soft))
        .cursor_pointer()
        .hover(|d| d.bg(rgba(theme::t().raised)))
        .on_click(cx.listener(move |this, _, _, cx| this.select_thread(&h, &id, cx)))
        .child(status.glyph())
        .child(
            div().flex_1().min_w_0().flex().flex_row().items_center().gap_2().children(pinned.then(theme::pin_mark)).child(
                div()
                    .min_w_0()
                    .truncate()
                    .font_weight(FontWeight::MEDIUM)
                    .text_color(rgba(if archived && !unread { theme::t().fg_1 } else { this.title_color(host, t) }))
                    .child(title),
            ),
        )
        .when(show_host, |d| d.child(div().text_size(px(12.)).text_color(rgba(theme::t().fg_2)).child(host.to_owned())))
        .when(!project.is_empty(), |d| d.child(div().text_size(px(12.)).text_color(rgba(theme::t().fg_1)).child(project)))
        .child(div().w(px(150.)).truncate().text_size(px(12.)).text_color(rgba(theme::t().fg_2)).child(model))
        .child(div().w(px(32.)).text_size(px(11.)).text_color(rgba(theme::t().fg_2)).child(theme::rel_time(now - t.updated_at)))
        .when(matches!(status, Status::Asking | Status::Done | Status::Snoozed), |d| {
            let color = if snoozed { theme::t().warn } else { theme::t().fg_1 };
            d.child(row_action(format!("snooze/{h3}/{id3}"), "moon", color, snoozed, cx, move |this, cx| {
                this.set_snoozed(&h3, &id3, !snoozed, cx)
            }))
        })
        .child(
            row_action(format!("arch/{h2}/{id2}"), archive_icon, theme::t().fg_1, archive_confirm.is_some(), cx, move |this, cx| {
                this.set_archived(&h2, &id2, !archived, cx)
            })
            .relative()
            .children(archive_confirm),
        )
}

/// A thread row's trailing action: hidden until the row is hovered (or always shown while
/// `on`); its click doesn't open the row.
fn row_action(
    id: String,
    name: &'static str,
    color: u32,
    on: bool,
    cx: &mut Context<Dirigent>,
    on_click: impl Fn(&mut Dirigent, &mut Context<Dirigent>) + 'static,
) -> Stateful<Div> {
    div()
        .id(SharedString::from(id))
        .size(px(24.))
        .flex()
        .items_center()
        .justify_center()
        .rounded_md()
        .opacity(if on { 1. } else { 0. })
        .group_hover("row", |d| d.opacity(1.))
        .hover(|d| d.bg(rgba(theme::t().hover)))
        .on_click(cx.listener(move |this, _, _, cx| {
            cx.stop_propagation();
            on_click(this, cx)
        }))
        .child(icon(name, color))
}
