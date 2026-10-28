//! Main-pane pages that list threads: Inbox, Workpool, one project. Plus the shared
//! 40px header bar and the Projects page (every project, pins).

use crate::app::{ArchiveAt, Dirigent, Menu, Pane, Target};
use crate::icons::icon;
use crate::menu::{self, Side};
use crate::projects::Group;
use crate::theme::{self, Status};
use crate::{projects, thread};
use dirigent_protocol::ThreadSummary;
use gpui::{AnyElement, Context, Div, ElementId, FontWeight, SharedString, Stateful, UniformList, div, prelude::*, px, rgba, uniform_list};
use std::ops::Range;

pub fn render(this: &Dirigent, cx: &mut Context<Dirigent>) -> Div {
    let pane = div().flex_1().min_w_0().h_full().flex().flex_col();
    match &this.pane {
        Pane::Inbox => {
            let rows = this.inbox_all();
            pane.child(header(div().child("Inbox"), None)).child(if rows.is_empty() {
                theme::empty("Nothing waiting", "Threads land here when their agent finishes or asks you something.").into_any_element()
            } else {
                thread_list("inbox", rows.into_iter().map(Item::row).collect(), true, cx).into_any_element()
            })
        }
        Pane::Workpool => {
            let rows = this.busy_threads();
            pane.child(header(div().child("Workpool"), None)).child(if rows.is_empty() {
                theme::empty("No agents working", "Start one with ctrl n.").into_any_element()
            } else {
                thread_list("workpool", rows.into_iter().map(Item::row).collect(), true, cx).into_any_element()
            })
        }
        Pane::Project { group_key } => project_page(this, group_key, pane, cx),
        Pane::Projects => projects_page(this, pane, cx),
        Pane::Thread(_) | Pane::Draft { .. } => pane,
    }
}

/// The Projects page (the sidebar's folder): every project group as a row (pin toggle,
/// name, its hosts, thread count, `⋯` menu), "Removed projects" last; `Add project` in
/// the top bar opens the modal (`add_project.rs`). A row opens the project's page; a pinned one is also listed in the
/// sidebar.
fn projects_page(this: &Dirigent, pane: Div, cx: &mut Context<Dirigent>) -> Div {
    let (groups, orphans) = this.groups();
    let rows: Vec<AnyElement> = groups.iter().map(|g| project_entry(g, &groups, this, cx)).collect();
    let removed = (!orphans.is_empty()).then(|| {
        entry_row("proj/orphans", projects::ORPHANS, cx)
            .child(div().size(px(24.)).flex_shrink_0())
            .child(div().w(px(160.)).flex_shrink_0().truncate().text_color(rgba(theme::t().fg_1)).child("Removed projects"))
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .truncate()
                    .text_size(px(12.))
                    .text_color(rgba(theme::t().fg_2))
                    .child("Threads whose project was removed"),
            )
            .child(thread_count(orphans.len()))
            .child(div().size(px(24.)).flex_shrink_0())
    });

    let add = theme::button("add-project", "plus", "Add project").on_click(cx.listener(|this, _, _, cx| this.open_add_project(cx)));
    let pane = pane.child(header(div().child("Projects"), Some(add.into_any_element())));
    if rows.is_empty() && removed.is_none() {
        return pane.child(theme::empty("No projects yet", "Add a directory on one of your hosts with Add project."));
    }
    let column = div().w_full().max_w(px(760.)).flex().flex_col().gap_1().py_6().px_6().children(rows).children(removed);
    pane.child(scroll("projects", div().flex().flex_col().items_center().child(column)))
}

/// A Projects page row: opens the project's page (`key`) when clicked.
fn entry_row(id: impl Into<SharedString>, key: &str, cx: &mut Context<Dirigent>) -> Stateful<Div> {
    let key = key.to_owned();
    div()
        .id(ElementId::Name(id.into()))
        .group("row")
        .flex()
        .flex_row()
        .items_center()
        .gap_3()
        .h(px(36.))
        .px_2()
        .rounded_md()
        .cursor_pointer()
        .hover(|d| d.bg(rgba(theme::t().hover)))
        .on_click(cx.listener(move |this, _, _, cx| this.show(Pane::Project { group_key: key.clone() }, cx)))
}

fn thread_count(n: usize) -> Div {
    let text = if n == 1 { "1 thread".to_owned() } else { format!("{n} threads") };
    div().w(px(80.)).flex_shrink_0().text_right().text_size(px(12.)).text_color(rgba(theme::t().fg_2)).child(text)
}

/// One project group on the Projects page: the pin toggle (amber while pinned, faint
/// otherwise), its name, its hosts on one line, its thread count, and the `⋯`
/// menu: rename it (UI only), link it into another group, unlink it, remove it from a host
/// (its threads are kept, under Removed projects).
fn project_entry(g: &Group, all: &[Group], this: &Dirigent, cx: &mut Context<Dirigent>) -> AnyElement {
    let key = g.key.clone();
    let pinned = this.is_pinned_project(&key);
    let menu_open = this.menu == Some(Menu::Project { group_key: key.clone() });
    let link_open = this.menu == Some(Menu::Link { group_key: key.clone() });

    let mut actions: Vec<AnyElement> = vec![];
    if menu_open {
        let k = key.clone();
        actions.push(menu::item("rename", "Rename", "", false, move |this, cx| this.start_rename(&k, cx), cx));
        let k = key.clone();
        actions.push(menu::item(
            "link",
            "Link to another project",
            "",
            false,
            move |this, cx| this.toggle_menu(Menu::Link { group_key: k.clone() }, cx),
            cx,
        ));
        if g.manual {
            let k = key.clone();
            actions.push(menu::item("unlink", "Unlink", "", false, move |this, cx| this.link_group(&k, None, cx), cx));
        }
        for m in &g.members {
            let (h, pid) = (m.host.clone(), m.project.id.clone());
            let label = if g.members.len() > 1 { format!("Remove from {h}") } else { "Remove project".to_owned() };
            actions.push(menu::item(
                format!("rm/{h}/{pid}"),
                label,
                "threads are kept",
                false,
                move |this, cx| this.remove_project(&h, &pid, cx),
                cx,
            ));
        }
    }
    if link_open {
        actions.push(menu::header("Link into"));
        for other in all.iter().filter(|o| o.key != key) {
            let (k, target) = (key.clone(), other.key.clone());
            let hosts = other.members.iter().map(|m| m.host.as_str()).collect::<Vec<_>>().join(", ");
            actions.push(menu::item(
                format!("lnk/{}", other.key),
                other.name.clone(),
                hosts,
                false,
                move |this, cx| this.link_group(&k, Some(&target), cx),
                cx,
            ));
        }
        if all.len() < 2 {
            actions.push(menu::header("No other projects"));
        }
    }

    let small_button = |id: String| {
        div()
            .id(SharedString::from(id))
            .relative()
            .size(px(24.))
            .flex_shrink_0()
            .flex()
            .items_center()
            .justify_center()
            .rounded_md()
            .hover(|d| d.bg(rgba(theme::t().line)))
    };
    let k = key.clone();
    let pin = small_button(format!("pin/{key}"))
        .when(!pinned, |d| d.opacity(0.5).group_hover("row", |d| d.opacity(1.)))
        .on_click(cx.listener(move |this, _, _, cx| {
            cx.stop_propagation();
            this.set_project_pinned(&k, !pinned, cx)
        }))
        .child(icon("pin", if pinned { theme::t().warn } else { theme::t().fg_2 }).size(px(14.)));
    let open = menu_open || link_open;
    let k = key.clone();
    let more = small_button(format!("pm/{key}"))
        .opacity(if open { 1. } else { 0. })
        .group_hover("row", |d| d.opacity(1.))
        .when(open, |d| d.bg(rgba(theme::t().line)))
        .on_click(cx.listener(move |this, _, _, cx| {
            cx.stop_propagation();
            this.toggle_menu(Menu::Project { group_key: k.clone() }, cx)
        }))
        .child(icon("ellipsis", theme::t().fg_1))
        .children(open.then(|| menu::popup(Side::Below(24.), actions, cx)));
    let mut hosts: Vec<&str> = g.members.iter().map(|m| m.host.as_str()).collect();
    hosts.dedup(); // members come sorted by host
    let hosts = hosts.join(", ");
    // While renaming, the name is a field; presses in it don't open the project.
    let name = if this.renaming.as_deref() == Some(key.as_str()) {
        // Pulled left by its padding + border so the text stays where the name was.
        div()
            .ml(px(-7.))
            .mr(px(7.))
            .px_1p5()
            .py_0p5()
            .rounded_md()
            .border_1()
            .border_color(rgba(theme::t().accent_line))
            .on_mouse_down(gpui::MouseButton::Left, |_, _, cx| cx.stop_propagation())
            .child(this.rename_input.clone())
    } else {
        div().truncate().child(g.name.clone())
    };
    entry_row(format!("proj/{key}"), &key, cx)
        .child(pin)
        .child(name.w(px(160.)).flex_shrink_0().font_weight(FontWeight::MEDIUM))
        .child(div().flex_1().min_w_0().truncate().text_size(px(12.)).text_color(rgba(theme::t().fg_2)).child(hosts))
        .child(thread_count(g.threads.len()))
        .child(more)
        .into_any_element()
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
    this.sort_as_queue(&mut threads);
    let (active, archived): (Vec<_>, Vec<_>) = threads.into_iter().partition(|(_, t)| !t.archived);
    let new_thread = (group_key != projects::ORPHANS).then(|| {
        theme::button("project-new-thread", "plus", "New thread")
            .on_click(cx.listener(move |this, _, _, cx| this.new_draft(target.clone(), cx)))
            .into_any_element()
    });

    let title = breadcrumb(None, None, None, name, false, false, cx);
    let body = if active.is_empty() && archived.is_empty() {
        theme::empty("No threads yet", "Start one with New thread.").into_any_element()
    } else {
        let mut items: Vec<Item> = active.into_iter().map(Item::row).collect();
        if !archived.is_empty() {
            items.push(Item::Label(format!("Archived  {}", archived.len())));
            items.extend(archived.into_iter().map(Item::row));
        }
        thread_list(SharedString::from(format!("threads/{group_key}")), items, false, cx).into_any_element()
    };
    pane.child(header(title, new_thread)).child(body)
}

/// A band between list sections (`Archived`): the canvas colour at 40% over the panel, a
/// shade darker than the rows without going solid on a see-through theme. As tall as a
/// row: the virtualized list needs uniform items.
fn section_label(text: String) -> Div {
    div()
        .w_full()
        .h(px(ROW_H))
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

/// Every item of a thread list is this tall (rows and section labels alike).
const ROW_H: f32 = 38.;

/// One item of a thread list: a thread (by key, looked up when it's on screen) or a
/// section label.
enum Item {
    Row { host: String, id: String },
    Label(String),
}

impl Item {
    fn row((host, t): (&str, &ThreadSummary)) -> Self {
        Item::Row { host: host.to_owned(), id: t.id.clone() }
    }
}

/// A scrolling, virtualized thread list filling the rest of the pane: only the rows on
/// screen are built, so a project with thousands of archived threads stays cheap.
fn thread_list(id: impl Into<ElementId>, items: Vec<Item>, show_project: bool, cx: &mut Context<Dirigent>) -> UniformList {
    let count = items.len();
    uniform_list(
        id,
        count,
        cx.processor(move |this, range: Range<usize>, _, cx| {
            let now = std::time::UNIX_EPOCH.elapsed().map(|d| d.as_millis() as i64).unwrap_or(0);
            let show_host = this.hosts.len() > 1;
            items[range]
                .iter()
                .map(|item| match item {
                    Item::Label(text) => section_label(text.clone()).into_any_element(),
                    Item::Row { host, id } => match this.summary(host, id) {
                        Some(t) => thread_row(host, t, show_host, show_project, now, this, cx).into_any_element(),
                        // Unreachable (items come from this frame's `this`); one element per
                        // index keeps the vec aligned with `range`.
                        None => div().h(px(ROW_H)).into_any_element(),
                    },
                })
                .collect::<Vec<_>>()
        }),
    )
    .flex_1()
}

/// A thread row: status glyph, title (amber while asking, accent while unread, an amber pin before it while pinned),
/// project, model, age. Hover reveals snooze (finished threads; snoozed ones carry the
/// moon glyph) and archive.
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
    let archive_confirm = thread::archive_confirm(host, &t.id, ArchiveAt::List, Side::BelowRight(24.), this, cx);

    div()
        .id(SharedString::from(format!("row/{h}/{id}")))
        .group("row")
        .w_full()
        .flex()
        .flex_row()
        .items_center()
        .gap_3()
        .h(px(ROW_H))
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
        .when(show_host, |d| d.child(column(90.).text_size(px(12.)).text_color(rgba(theme::t().fg_2)).child(host.to_owned())))
        .when(show_project, |d| d.child(column(140.).text_size(px(12.)).text_color(rgba(theme::t().fg_1)).child(project)))
        .child(column(150.).text_size(px(12.)).text_color(rgba(theme::t().fg_2)).child(model))
        .child(column(32.).text_right().text_size(px(11.)).text_color(rgba(theme::t().fg_2)).child(theme::rel_time(now - t.updated_at)))
        // Rows that can't be snoozed keep an empty slot, so the columns stay aligned.
        .child(if this.snoozable(host, t) {
            let color = if snoozed { theme::t().warn } else { theme::t().fg_1 };
            row_action(format!("snooze/{h3}/{id3}"), "moon", color, snoozed, cx, move |this, cx| this.set_snoozed(&h3, &id3, !snoozed, cx))
                .into_any_element()
        } else {
            div().size(px(24.)).flex_shrink_0().into_any_element()
        })
        .child(
            row_action(format!("arch/{h2}/{id2}"), archive_icon, theme::t().fg_1, archive_confirm.is_some(), cx, move |this, cx| {
                this.set_archived(&h2, &id2, !archived, ArchiveAt::List, cx)
            })
            .relative()
            .children(archive_confirm),
        )
}

/// A fixed-width cell of a thread row: every row lays its columns out the same way, so
/// they line up like a table.
fn column(width: f32) -> Div {
    div().w(px(width)).flex_shrink_0().truncate()
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
        .flex_shrink_0()
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
