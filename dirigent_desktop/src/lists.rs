use crate::app::{ArchiveAt, Dirigent, DragProject, Menu, Pane, Renaming, Target};
use crate::icons::icon;
use crate::menu::{self, Side};
use crate::projects::Group;
use crate::theme::{self, Status, text, u};
use crate::{projects, sidebar, thread, titlebar};
use dirigent_protocol::ThreadSummary;
use gpui::{
    AnyElement, Context, Div, DragMoveEvent, ElementId, FontWeight, SharedString, Stateful, UniformList, div, prelude::*, rgba,
    uniform_list,
};
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
                theme::empty("No agents working", format!("Start one with {} n.", crate::input::MOD)).into_any_element()
            } else {
                thread_list("workpool", rows.into_iter().map(Item::row).collect(), true, cx).into_any_element()
            })
        }
        Pane::Project { group_key } => project_page(this, group_key, pane, cx),
        Pane::Projects => projects_page(this, pane, cx),
        Pane::Thread(_) | Pane::Draft { .. } => pane,
    }
}

fn projects_page(this: &Dirigent, pane: Div, cx: &mut Context<Dirigent>) -> Div {
    let (groups, orphans) = this.groups();
    let dragging = this.dragging.clone().filter(|d| !d.sidebar && cx.has_active_drag()).map(|d| d.key);
    let rows: Vec<AnyElement> = groups.iter().map(|g| project_entry(g, dragging.as_deref() == Some(&g.key), this, cx)).collect();
    let rows = div().flex().flex_col().gap(u(ENTRY_GAP)).children(rows).on_drag_move(cx.listener(
        |this, ev: &DragMoveEvent<DragProject>, _, cx| {
            let drag = ev.drag(cx);
            if drag.sidebar {
                return;
            }
            let (groups, _) = this.groups();
            let Some(dragged) = groups.iter().position(|g| g.key == drag.key) else { return };
            let heights: Vec<f32> =
                groups.iter().enumerate().map(|(i, g)| ENTRY_H + if i == dragged { 0. } else { details_height(g, this) }).collect();
            let y = (ev.event.position.y - ev.bounds.top()) / u(1.);
            let key = drag.key.clone();
            this.move_project(&key, &groups[projects::drop_slot(&heights, ENTRY_GAP, dragged, y)].key, cx);
        },
    ));
    let removed = (!orphans.is_empty()).then(|| {
        let what = if orphans.len() == 1 {
            "1 thread whose project was removed".to_owned()
        } else {
            format!("{} threads whose project was removed", orphans.len())
        };
        entry_row("proj/orphans", projects::ORPHANS, cx)
            .child(div().size(u(24.)).flex_shrink_0())
            .child(div().w(u(160.)).flex_shrink_0().truncate().text_color(rgba(theme::t().fg_1)).child("Removed projects"))
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .pr(u(CHEVRON_W))
                    .truncate()
                    .text_right()
                    .text_size(text::sm())
                    .text_color(rgba(theme::t().fg_2))
                    .child(what),
            )
            .child(div().size(u(24.)).flex_shrink_0())
    });

    let add = theme::button("add-project", "plus", "Add project").on_click(cx.listener(|this, _, _, cx| this.open_add_project(cx)));
    let pane = pane.child(header(div().child("Projects"), Some(add.into_any_element())));
    if groups.is_empty() && removed.is_none() {
        return pane.child(theme::empty("No projects yet", "Add a directory on one of your hosts with Add project."));
    }
    let column = div().w_full().max_w(u(ENTRY_MAX_W)).flex().flex_col().gap(u(ENTRY_GAP)).py_6().px_6().child(rows).children(removed);
    pane.child(scroll("projects", div().flex().flex_col().items_center().child(column)))
}

const ENTRY_H: f32 = 36.;
const ENTRY_GAP: f32 = 4.;
const ENTRY_MAX_W: f32 = 760.;
const MEMBER_H: f32 = 28.;
const MEMBERS_PAD: f32 = 6.;
// MUST KILL: extract: CHEVRON_W's summands (gap, icon, padding) as named consts
const CHEVRON_W: f32 = 6. + 12. + 4.;
// MUST KILL: extract: NAME_X's summands (padding, pin, gap) as named consts
const NAME_X: f32 = 8. + 24. + 12.;
const LINK_W: f32 = 440.;
const LINK_LIST_H: f32 = 216.;

fn details_height(g: &Group, this: &Dirigent) -> f32 {
    if this.expanded_projects.contains(&g.key) { g.members.len() as f32 * MEMBER_H + MEMBERS_PAD } else { 0. }
}

fn entry_row(id: impl Into<SharedString>, key: &str, cx: &mut Context<Dirigent>) -> Stateful<Div> {
    let key = key.to_owned();
    div()
        .id(ElementId::Name(id.into()))
        .group("row")
        .flex()
        .flex_row()
        .items_center()
        .gap_3()
        .h(u(ENTRY_H))
        .px_2()
        .rounded_md()
        .cursor_pointer()
        .hover(|d| d.bg(rgba(theme::t().hover)))
        .on_click(cx.listener(move |this, _, _, cx| this.show(Pane::Project { group_key: key.clone() }, cx)))
}

fn thread_count(n: usize) -> Div {
    let text = if n == 1 { "1 thread".to_owned() } else { format!("{n} threads") };
    div().flex_shrink_0().text_size(text::sm()).text_color(rgba(theme::t().fg_2)).child(text)
}

fn project_entry(g: &Group, dragged: bool, this: &Dirigent, cx: &mut Context<Dirigent>) -> AnyElement {
    let key = g.key.clone();
    let pinned = this.is_pinned_project(&key);
    let expanded = this.expanded_projects.contains(&key);
    let menu_open = this.menu == Some(Menu::Project { group_key: key.clone() });
    let link_open = this.menu == Some(Menu::Link { group_key: key.clone() });

    let mut actions: Vec<AnyElement> = vec![];
    if menu_open {
        let k = key.clone();
        actions.push(menu::item("rename", "Rename", "", false, move |this, cx| this.start_rename(&k, cx), cx));
        let k = key.clone();
        actions.push(menu::item("link", "Link another project", "", false, move |this, cx| this.open_link_picker(&k, cx), cx));
        let linked: Vec<&str> = g.members.iter().filter(|m| m.linked).map(|m| m.host.as_str()).collect();
        if !linked.is_empty() {
            let k = key.clone();
            let (label, which) = if linked.len() > 1 { ("Unlink all", String::new()) } else { ("Unlink", linked[0].to_owned()) };
            actions.push(menu::item("unlink", label, which, false, move |this, cx| this.link_group(&k, None, cx), cx));
        }
        for m in &g.members {
            let (h, pid) = (m.host.clone(), m.project.id.clone());
            let label = if g.members.len() > 1 { format!("Remove from {h}") } else { "Remove project".to_owned() };
            let twice = g.members.iter().filter(|o| o.host == m.host).count() > 1;
            let detail = if twice { m.project.path.clone() } else { "threads are kept".to_owned() };
            let row = menu::stop(menu::row(format!("rm/{h}/{pid}")), false, move |this, _, cx| this.remove_project(&h, &pid, cx), cx)
                .child(div().flex_shrink_0().child(label))
                .child(
                    div()
                        .min_w_0()
                        .overflow_hidden()
                        .whitespace_nowrap()
                        .text_ellipsis_start()
                        .text_color(rgba(theme::t().fg_1))
                        .text_size(text::sm())
                        .child(detail),
                );
            actions.push(row.into_any());
        }
    }
    if link_open {
        actions.push(link_picker(g, this, cx));
    }

    let small_button = |id: String| {
        div()
            .id(SharedString::from(id))
            .relative()
            .size(u(24.))
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
        .child(icon("pin", if pinned { theme::t().warn } else { theme::t().fg_2 }).size(u(14.)));
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
        .children(open.then(|| menu::popup(Side::BelowRight { trigger_h: 24. }, actions, cx)));
    let mut hosts: Vec<&str> = g.members.iter().map(|m| m.host.as_str()).collect();
    // MUST KILL: rearchitect: hosts.dedup() relies on Group::members coming sorted by host; collect into a BTreeSet
    hosts.dedup();
    let hosts = hosts.into_iter().map(|h| this.host_label(h)).collect::<Vec<_>>().join(", ");
    let k = key.clone();
    let places = div()
        .id(SharedString::from(format!("exp/{key}")))
        .min_w_0()
        .h(u(24.))
        .flex()
        .flex_row()
        .items_center()
        .gap_1p5()
        .pl_1p5()
        .pr_1()
        .rounded_md()
        .text_size(text::sm())
        .text_color(rgba(theme::t().fg_2))
        .hover(|d| d.bg(rgba(theme::t().line)).text_color(rgba(theme::t().fg_1)))
        .on_click(cx.listener(move |this, _, _, cx| {
            cx.stop_propagation();
            this.toggle_project_expanded(&k, cx)
        }))
        .child(div().min_w_0().truncate().child(hosts))
        .child(icon(if expanded { "chevron-down" } else { "chevron-right" }, theme::t().fg_2).size(u(12.)));
    let members = (expanded && !dragged).then(|| member_lines(g, this, cx));
    let name = if this.renaming == Some(Renaming::Project(key.clone())) {
        // MUST KILL: extract: the -7 / 7 margins as a const from the rename field's padding and border
        div()
            .ml(u(-7.))
            .mr(u(7.))
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
    // MUST KILL: extract: the ghost width in project_entry from a shared pane_width() and the list padding const
    let sidebar_w = if this.sidebar_hidden { 0. } else { this.sidebar_width };
    let (ghost_name, font) = (g.name.clone(), this.fonts.ui.clone());
    let entity = cx.weak_entity();
    let row = entry_row(format!("proj/{key}"), &key, cx)
        .when(dragged, |d| d.opacity(0.))
        .on_drag(DragProject { key: key.clone(), sidebar: false }, move |drag, _, window, cx| {
            let drag = drag.clone();
            let _ = entity.update(cx, |this, cx| {
                this.dragging = Some(drag);
                cx.notify();
            });
            let pane = f32::from(window.viewport_size().width) / theme::zoom() - sidebar_w;
            let width = pane.min(ENTRY_MAX_W) - 2. * 24.;
            cx.new(|_| sidebar::Ghost {
                name: ghost_name.clone(),
                width,
                height: ENTRY_H,
                indent: NAME_X,
                weight: FontWeight::MEDIUM,
                font: font.clone(),
            })
        })
        .child(pin)
        .child(name.w(u(160.)).flex_shrink_0().font_weight(FontWeight::MEDIUM))
        .child(div().flex_1().min_w_0().flex().flex_row().justify_end().child(places))
        .child(more);
    div().flex().flex_col().child(row).children(members).into_any_element()
}

fn member_lines(g: &Group, this: &Dirigent, cx: &mut Context<Dirigent>) -> Div {
    let hosts = g.members.iter().map(|m| {
        let label = div().min_w_0().truncate().child(this.host_label(&m.host));
        div().h(u(MEMBER_H)).flex().flex_row().items_center().text_color(rgba(theme::t().fg_2)).child(label)
    });
    let lines: Vec<AnyElement> = g.members.iter().map(|m| member_line(g, m, this, cx)).collect();
    div()
        .flex()
        .flex_row()
        .gap_3()
        .pl(u(NAME_X))
        .pr_2()
        .pb(u(MEMBERS_PAD))
        .text_size(text::sm())
        .child(div().flex_shrink_0().max_w(u(160.)).flex().flex_col().children(hosts))
        .child(div().flex_1().min_w_0().flex().flex_col().children(lines))
}

fn member_line(g: &Group, m: &projects::Member, this: &Dirigent, cx: &mut Context<Dirigent>) -> AnyElement {
    let (host, pid, path) = (m.host.clone(), m.project.id.clone(), m.project.path.clone());
    let id = format!("{host}/{pid}");
    let (h, p) = (host.clone(), pid.clone());
    let linked = m.linked.then(|| {
        div()
            .id(SharedString::from(format!("unlink/{id}")))
            .flex_shrink_0()
            .flex()
            .flex_row()
            .items_center()
            .gap_1()
            .h(u(18.))
            .pl_1p5()
            .pr_1()
            .rounded_md()
            .border_1()
            .border_color(rgba(theme::t().line))
            .text_size(text::xs())
            .text_color(rgba(theme::t().fg_1))
            .cursor_pointer()
            .hover(|d| d.bg(rgba(theme::t().hover)).text_color(rgba(theme::t().fg)))
            .tooltip(theme::tooltip("Unlink: its own project again"))
            .on_click(cx.listener(move |this, _, _, cx| this.unlink_member(&h, &p, cx)))
            .child("linked")
            .child(icon("x", theme::t().fg_2).size(u(10.)))
    });
    let copy_id = ElementId::from(SharedString::from(format!("copy/{id}")));
    let done = this.copied.as_ref() == Some(&copy_id);
    let copy_path = path.to_string();
    let copy = div()
        .id(copy_id.clone())
        .size(u(24.))
        .flex_shrink_0()
        .flex()
        .items_center()
        .justify_center()
        .rounded_md()
        .cursor_pointer()
        .opacity(if done { 1. } else { 0. })
        .group_hover("member", |d| d.opacity(1.))
        .hover(|d| d.bg(rgba(theme::t().line)))
        .on_click(cx.listener(move |this, _, _, cx| this.copy_item(copy_id.clone(), copy_path.clone(), cx)))
        .child(icon(if done { "check" } else { "copy" }, theme::t().fg_1));
    div()
        .group("member")
        .flex()
        .flex_row()
        .items_center()
        .gap_2()
        .h(u(MEMBER_H))
        .child(
            div()
                .id(SharedString::from(format!("path/{id}")))
                .min_w_0()
                .overflow_hidden()
                .whitespace_nowrap()
                .text_ellipsis_start()
                .font_family(this.fonts.mono.clone())
                .text_color(rgba(theme::t().fg_1))
                .tooltip(theme::tooltip(path.clone()))
                .child(path),
        )
        .children(linked)
        .child(div().flex_1())
        .child(thread_count(g.threads_of(m)).pr_1())
        .child(copy)
        .into_any_element()
}

fn link_picker(g: &Group, this: &Dirigent, cx: &mut Context<Dirigent>) -> AnyElement {
    let key = g.key.clone();
    let list = menu::results("link-list", LINK_LIST_H, cx, |cx| {
        let mut items: Vec<AnyElement> = vec![];
        for other in this.link_candidates(&key, cx) {
            let (k, o) = (key.clone(), other.key.clone());
            let places = other.members.iter().map(|m| format!("{} {}", m.host, m.project.path)).collect::<Vec<_>>().join(", ");
            let id = format!("lnk/{}", other.key);
            items.push(menu::item(id, other.name, places, false, move |this, cx| this.link_into(&k, &o, cx), cx));
        }
        if items.is_empty() {
            let none = if this.link_search.read(cx).text().trim().is_empty() { "No other projects" } else { "No projects match" };
            items.push(menu::header(none));
        }
        items
    });
    div()
        .w(u(LINK_W))
        .flex()
        .flex_col()
        .child(menu::header(format!("Link into {}", g.name)))
        .child(menu::search(this.link_search.clone()))
        .child(menu::separator())
        .child(list)
        .into_any()
}

pub fn header(title: impl IntoElement, actions: Option<AnyElement>) -> Div {
    div()
        .flex()
        .flex_row()
        .items_center()
        .h(u(titlebar::HEIGHT))
        .flex_shrink_0()
        .px_4()
        .when(titlebar::CUSTOM, |d| d.pr(u(titlebar::CONTROLS_WIDTH)))
        .font_weight(FontWeight::MEDIUM)
        .child(div().flex_1().min_w_0().flex().flex_row().items_center().gap_1p5().child(title))
        .children(actions.map(|a| div().flex_shrink_0().occlude().child(a)))
}

pub struct Via {
    pub host: String,
    pub id: String,
    pub name: SharedString,
    pub status: Status,
}

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
    let chevron = || icon("chevron-right", theme::t().fg_2).size(u(12.)).flex_shrink_0();
    let row = || div().flex().flex_row().items_center().gap_1().min_w_0();
    let (via_glyph, via_name) = via
        .map(|v| {
            let open = |id: &'static str| {
                let (h, pid) = (v.host.clone(), v.id.clone());
                div().id(id).occlude().cursor_pointer().on_click(cx.listener(move |this, _, _, cx| this.select_thread(&h, &pid, cx)))
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
                .occlude()
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
        (g.name.clone(), g.threads_newest_first.iter().map(|(h, t)| (h.as_str(), t)).collect(), Some(g.member_for(None).into()))
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

fn section_label(text: String) -> Div {
    div()
        .w_full()
        .h(u(ROW_H))
        .px_4()
        .flex()
        .items_center()
        .text_size(text::sm())
        .text_color(rgba(theme::t().fg_2))
        .bg(rgba(theme::alpha(theme::t().canvas, 0x66)))
        .child(text)
}

pub fn scroll(id: &'static str, content: Div) -> Stateful<Div> {
    theme::stack().id(id).flex_1().overflow_y_scroll().child(content)
}

const ROW_H: f32 = 38.;

enum Item {
    Row { host: String, id: String },
    Label(String),
}

impl Item {
    fn row((host, t): (&str, &ThreadSummary)) -> Self {
        Item::Row { host: host.to_owned(), id: t.id.clone() }
    }
}

fn thread_list(id: impl Into<ElementId>, items: Vec<Item>, show_project: bool, cx: &mut Context<Dirigent>) -> UniformList {
    let count = items.len();
    uniform_list(
        id,
        count,
        cx.processor(move |this, range: Range<usize>, window, cx| {
            let now = std::time::UNIX_EPOCH.elapsed().map(|d| d.as_millis() as i64).unwrap_or(0);
            // MUST KILL: extract: the pane width (viewport over zoom minus the docked sidebar) into a Dirigent::pane_width()
            let width = f32::from(window.viewport_size().width) / theme::zoom() - if this.sidebar_hidden { 0. } else { this.sidebar_width };
            let cols = Columns::fit(width, this.hosts.len() > 1, show_project);
            items[range]
                .iter()
                .map(|item| match item {
                    Item::Label(text) => section_label(text.clone()).into_any_element(),
                    Item::Row { host, id } => match this.summary(host, id) {
                        Some(t) => thread_row(host, t, cols, now, this, cx).into_any_element(),
                        // MUST KILL: type: Item::Row resolved to its summary up front so this None arm goes
                        None => div().h(u(ROW_H)).into_any_element(),
                    },
                })
                .collect::<Vec<_>>()
        }),
    )
    .flex_1()
}

fn thread_row(host: &str, t: &ThreadSummary, cols: Columns, now: i64, this: &Dirigent, cx: &mut Context<Dirigent>) -> Stateful<Div> {
    let (h, id) = (host.to_owned(), t.id.clone());
    let title = t.name.clone().unwrap_or_else(|| "Untitled".into());
    let status = this.status(host, t);
    let project = if cols.project { this.project_name(host, t) } else { String::new() };
    let model = t.model.as_deref().map(theme::short_model).unwrap_or("").to_owned();
    let unread = this.unread(host, t);
    let (h2, id2, archived) = (h.clone(), id.clone(), t.archived);
    let (h3, id3, snoozed) = (h.clone(), id.clone(), status == Status::Snoozed);
    let pinned = t.pinned;
    let archive_icon = if archived { "archive-restore" } else { "archive" };
    let archive_confirm = thread::archive_confirm(host, &t.id, ArchiveAt::List, Side::BelowRight { trigger_h: 24. }, this, cx);

    div()
        .id(SharedString::from(format!("row/{h}/{id}")))
        .group("row")
        .w_full()
        .flex()
        .flex_row()
        .items_center()
        .gap_3()
        .h(u(ROW_H))
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
        .when(cols.host, |d| d.child(column(HOST_W).text_size(text::sm()).text_color(rgba(theme::t().fg_2)).child(host.to_owned())))
        .when(cols.project, |d| d.child(column(PROJECT_W).text_size(text::sm()).text_color(rgba(theme::t().fg_1)).child(project)))
        .when(cols.model, |d| d.child(column(MODEL_W).text_size(text::sm()).text_color(rgba(theme::t().fg_2)).child(model)))
        .child(column(32.).text_right().text_size(text::xs()).text_color(rgba(theme::t().fg_2)).child(theme::rel_time(now - t.updated_at)))
        .child(if this.snoozable(host, t) {
            let color = if snoozed { theme::t().warn } else { theme::t().fg_1 };
            row_action(format!("snooze/{h3}/{id3}"), "moon", color, snoozed, cx, move |this, cx| this.set_snoozed(&h3, &id3, !snoozed, cx))
                .into_any_element()
        } else {
            div().size(u(24.)).flex_shrink_0().into_any_element()
        })
        .child(if archived || this.at_rest(host, t) {
            row_action(format!("arch/{h2}/{id2}"), archive_icon, theme::t().fg_1, archive_confirm.is_some(), cx, move |this, cx| {
                this.set_archived(&h2, &id2, !archived, ArchiveAt::List, cx)
            })
            .relative()
            .children(archive_confirm)
            .into_any_element()
        } else {
            div().size(u(24.)).flex_shrink_0().into_any_element()
        })
}

const HOST_W: f32 = 90.;
const PROJECT_W: f32 = 140.;
const MODEL_W: f32 = 150.;
// MUST KILL: extract: GAP and the row's gap_3 from one const
const GAP: f32 = 12.;
// MUST KILL: extract: ROW_FIXED's summands as named consts
const ROW_FIXED: f32 = 2. * 16. + 16. + GAP + 3. * GAP + 32. + 2. * 24.;
const TITLE_MIN: f32 = 200.;

#[derive(Clone, Copy, Debug, PartialEq)]
struct Columns {
    host: bool,
    project: bool,
    model: bool,
}

impl Columns {
    fn fit(width: f32, host: bool, project: bool) -> Self {
        let mut room = width - ROW_FIXED - TITLE_MIN;
        let mut keep = |wanted: bool, w: f32| {
            if wanted {
                room -= w + GAP;
            }
            wanted && room >= 0.
        };
        let project = keep(project, PROJECT_W);
        let host = keep(host, HOST_W);
        let model = keep(true, MODEL_W);
        Columns { host, project, model }
    }
}

fn column(width: f32) -> Div {
    div().w(u(width)).flex_shrink_0().truncate()
}

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
        .size(u(24.))
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

#[cfg(test)]
mod tests {
    use super::Columns;

    #[test]
    fn columns_give_way_model_then_host_then_project() {
        let c = |host, project, model| Columns { host, project, model };
        assert_eq!(Columns::fit(800., true, true), c(true, true, true));
        assert_eq!(Columns::fit(700., true, true), c(true, true, false));
        assert_eq!(Columns::fit(600., true, true), c(false, true, false));
        assert_eq!(Columns::fit(500., true, true), c(false, false, false));
        assert_eq!(Columns::fit(700., false, true), c(false, true, true));
    }
}
