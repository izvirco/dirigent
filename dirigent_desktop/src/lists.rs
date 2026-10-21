//! Main-pane pages that list threads: Inbox, Workpool, one project. Plus the shared
//! 40px header bar, the new-project form and the settings page.

use crate::app::{Dirigent, Pane, Target};
use crate::icons::icon;
use crate::net::HostStatus;
use crate::theme::{self, Status};
use crate::{hosts, projects};
use dirigent_protocol::ThreadSummary;
use gpui::{AnyElement, Context, Div, FontWeight, SharedString, Stateful, div, prelude::*, px, rgb};

pub fn render(this: &Dirigent, cx: &mut Context<Dirigent>) -> Div {
    let pane = div().flex_1().min_w_0().h_full().flex().flex_col();
    match &this.pane {
        Pane::Inbox => {
            let rows = this.threads_with(Status::Done);
            pane.child(header(div().child("Inbox"), None)).child(if rows.is_empty() {
                theme::empty("Nothing waiting", "Threads land here when their agent finishes.").into_any_element()
            } else {
                scroll("inbox", thread_list(&rows, true, this, cx)).into_any_element()
            })
        }
        Pane::Workpool => {
            let rows = this.threads_with(Status::Working);
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
                .child(div().text_color(rgb(theme::FG_1)).child("Absolute path of a directory on that host. Enter to add it."))
                .child(
                    div()
                        .w_full()
                        .px_3()
                        .py_2()
                        .rounded_md()
                        .bg(rgb(theme::RAISED))
                        .border_1()
                        .border_color(rgb(theme::LINE))
                        .child(this.path_input.clone()),
                ),
        ),
        Pane::Settings => settings_page(this, pane, cx),
        Pane::Thread(_) | Pane::Draft { .. } => pane,
    }
}

/// Settings: a narrow centered column of sections. For now just Hosts: every host with
/// its url and status (hover reveals remove), then the add form.
fn settings_page(this: &Dirigent, pane: Div, cx: &mut Context<Dirigent>) -> Div {
    let rows = this.hosts.iter().map(|h| {
        let (dot, status) = match h.status {
            HostStatus::Connected => (theme::DONE, "connected"),
            HostStatus::Connecting => (theme::WARN, "connecting"),
            HostStatus::Disconnected => (theme::ERROR, "disconnected"),
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
            .border_color(rgb(theme::LINE_SOFT))
            .child(div().size(px(6.)).rounded_full().bg(rgb(dot)))
            .child(div().w(px(110.)).truncate().font_weight(FontWeight::MEDIUM).child(name.clone()))
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .truncate()
                    .font_family(this.fonts.mono.clone())
                    .text_size(px(12.))
                    .text_color(rgb(theme::FG_1))
                    .child(h.url.clone()),
            )
            .child(div().w(px(90.)).flex_shrink_0().text_size(px(12.)).text_color(rgb(theme::FG_2)).child(status))
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
                    .hover(|d| d.bg(rgb(theme::HOVER)))
                    .on_click(cx.listener(move |this, _, _, cx| this.remove_host(&name, cx)))
                    .child(icon("x", theme::FG_1))
                    .into_any_element()
            })
    });
    let field = |input: gpui::Entity<crate::input::TextInput>| {
        div().px_3().py_2().rounded_md().bg(rgb(theme::RAISED)).border_1().border_color(rgb(theme::LINE)).child(input)
    };
    let form = div()
        .flex()
        .flex_col()
        .gap_3()
        .pt_4()
        .child(
            div().text_color(rgb(theme::FG_1)).child("Add a host running dirigent_service, reachable over your tailnet. Enter to add it."),
        )
        .child(
            div()
                .flex()
                .flex_row()
                .gap_2()
                .child(field(this.host_name_input.clone()).w(px(150.)))
                .child(field(this.host_url_input.clone()).flex_1()),
        )
        .child(div().text_size(px(12.)).text_color(rgb(theme::FG_2)).child(format!("Also editable by hand: {}", hosts::path().display())));

    let hosts = settings_section("Hosts", div().flex().flex_col().children(rows).child(form));
    let column = div().w_full().max_w(px(600.)).flex().flex_col().gap_8().py_8().px_6().child(hosts);
    pane.child(header(div().child("Settings"), None)).child(scroll("settings", div().flex().flex_col().items_center().child(column)))
}

/// A settings section: title over its content.
fn settings_section(title: &'static str, body: impl IntoElement) -> Div {
    div().flex().flex_col().gap_2().child(div().font_weight(FontWeight::MEDIUM).text_color(rgb(theme::FG)).child(title)).child(body)
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
        .border_color(rgb(theme::LINE_SOFT))
        .font_weight(FontWeight::MEDIUM)
        .child(div().flex_1().min_w_0().flex().flex_row().items_center().gap_1p5().child(title))
        .children(actions)
}

/// `project › title`, with the project clickable.
pub fn breadcrumb(project: Option<(String, String)>, title: impl Into<SharedString>, cx: &mut Context<Dirigent>) -> Div {
    div()
        .flex()
        .flex_row()
        .items_center()
        .gap_1()
        .min_w_0()
        .children(project.map(|(key, name)| {
            div()
                .flex()
                .flex_row()
                .items_center()
                .gap_1()
                .child(
                    div()
                        .id("crumb-project")
                        .text_color(rgb(theme::FG_1))
                        .cursor_pointer()
                        .hover(|d| d.text_color(rgb(theme::FG)))
                        .on_click(cx.listener(move |this, _, _, cx| this.show(Pane::Project { group_key: key.clone() }, cx)))
                        .child(name),
                )
                .child(icon("chevron-right", theme::FG_2).size(px(12.)))
        }))
        .child(div().min_w_0().truncate().child(title.into()))
}

fn project_page(this: &Dirigent, group_key: &str, pane: Div, cx: &mut Context<Dirigent>) -> Div {
    let (groups, orphans) = projects::groups(&this.hosts, &this.links);
    let (name, threads, target): (String, Vec<(&str, &ThreadSummary)>, Option<Target>) = if group_key == projects::ORPHANS {
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

    let (active, archived): (Vec<_>, Vec<_>) = threads.into_iter().partition(|(_, t)| !t.archived);
    let new_thread = (group_key != projects::ORPHANS).then(|| {
        theme::button("project-new-thread", "plus", "New thread")
            .on_click(cx.listener(move |this, _, _, cx| this.new_draft(target.clone(), cx)))
            .into_any_element()
    });

    let title = breadcrumb(None, name, cx);
    let mut body = div().id("project-page").flex_1().overflow_y_scroll().flex().flex_col();
    if !paths.is_empty() {
        body = body.child(
            div()
                .px_4()
                .py_2()
                .border_b_1()
                .border_color(rgb(theme::LINE_SOFT))
                .text_size(px(12.))
                .text_color(rgb(theme::FG_2))
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

fn section_label(text: String) -> Div {
    div().h(px(32.)).px_4().flex().items_center().text_size(px(12.)).text_color(rgb(theme::FG_2)).bg(rgb(theme::CANVAS)).child(text)
}

fn scroll(id: &'static str, content: Div) -> Stateful<Div> {
    div().id(id).flex_1().overflow_y_scroll().flex().flex_col().child(content)
}

/// Flat rows: status glyph, title (accent while unread), project, model, age. Hover reveals archive.
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
    let status = Status::of(t);
    let project = if show_project { this.project_name(host, t) } else { String::new() };
    let model = t.model.as_deref().map(theme::short_model).unwrap_or("").to_owned();
    let unread = this.unread(host, t);
    let (h2, id2, archived) = (h.clone(), id.clone(), t.archived);
    let archive_icon = if archived { "archive-restore" } else { "archive" };

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
        .border_color(rgb(theme::LINE_SOFT))
        .cursor_pointer()
        .hover(|d| d.bg(rgb(theme::RAISED)))
        .on_click(cx.listener(move |this, _, _, cx| this.select_thread(&h, &id, cx)))
        .child(status.glyph())
        .child(
            div()
                .flex_1()
                .min_w_0()
                .truncate()
                .font_weight(FontWeight::MEDIUM)
                .text_color(rgb(if unread {
                    theme::NAVY_INK
                } else if archived {
                    theme::FG_1
                } else {
                    theme::FG
                }))
                .child(title),
        )
        .when(show_host, |d| d.child(div().text_size(px(12.)).text_color(rgb(theme::FG_2)).child(host.to_owned())))
        .when(!project.is_empty(), |d| d.child(div().text_size(px(12.)).text_color(rgb(theme::FG_1)).child(project)))
        .child(div().w(px(150.)).truncate().text_size(px(12.)).text_color(rgb(theme::FG_2)).child(model))
        .child(div().w(px(32.)).text_size(px(11.)).text_color(rgb(theme::FG_2)).child(theme::rel_time(now - t.updated_at)))
        .child(
            div()
                .id(SharedString::from(format!("arch/{h2}/{id2}")))
                .size(px(24.))
                .flex()
                .items_center()
                .justify_center()
                .rounded_md()
                .opacity(0.)
                .group_hover("row", |d| d.opacity(1.))
                .hover(|d| d.bg(rgb(theme::HOVER)))
                .on_click(cx.listener(move |this, _, _, cx| {
                    cx.stop_propagation();
                    this.set_archived(&h2, &id2, !archived, cx)
                }))
                .child(icon(archive_icon, theme::FG_1)),
        )
}
