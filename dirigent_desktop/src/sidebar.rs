//! Left pane on the window canvas: Inbox / Workpool, each listing its threads (finished
//! and not snoozed / working) as small cards, the project list, and at the bottom the hide/show and settings
//! buttons. Docked it has a drag handle on its right edge; hidden (`[` or the button) it
//! slides in as a floating panel while the mouse rests on the left edge.

use crate::app::{Dirigent, Menu, Pane, Resize, Section};
use crate::icons::icon;
use crate::menu::{self, Side};
use crate::projects::{self, Group};
use crate::theme;
use crate::thread::{duration, elapsed};
use dirigent_protocol::ThreadSummary;
use gpui::{AnyElement, Context, Div, FontWeight, MouseButton, SharedString, Stateful, div, prelude::*, px, rgba};

/// Default width; the user can drag it between `app::SIDEBAR_MIN` and `SIDEBAR_MAX`.
pub const WIDTH: f32 = 240.;
/// Width of the invisible strip along the window's left edge that reveals a hidden sidebar.
const PEEK_ZONE: f32 = 10.;

/// The sidebar in the layout, with its resize handle.
pub fn docked(this: &Dirigent, cx: &mut Context<Dirigent>) -> Div {
    let handle = div()
        .id("sidebar-resize")
        .absolute()
        .top_0()
        .right(px(-3.))
        .w(px(6.))
        .h_full()
        .cursor_col_resize()
        .hover(|d| d.bg(rgba(theme::t().line)))
        .on_mouse_down(MouseButton::Left, cx.listener(|this, _, _, cx| this.start_resize(Resize::Sidebar, cx)));
    // Inset by the main panel's gutter + border so the title row lines up with the panel's
    // header and the sidebar text sits at the same x as the panel's; same on the right so
    // selected rows have equal margins, and at the bottom so the bottom bar mirrors the title.
    let inset = px(theme::GUTTER + 1.);
    div().relative().w(px(this.sidebar_width)).h_full().flex_shrink_0().py(inset).px(inset).child(content(this, cx)).child(handle)
}

/// Hidden sidebar: a hot zone on the left edge that, while hovered, grows into a floating
/// copy of the sidebar. One element handles both so the hover never flickers between them.
pub fn peek(this: &Dirigent, cx: &mut Context<Dirigent>) -> Stateful<Div> {
    let open = this.sidebar_peek;
    div()
        .id("sidebar-peek")
        .absolute()
        .left_0()
        .top_0()
        .h_full()
        .w(px(if open { this.sidebar_width + 2. * theme::GUTTER } else { PEEK_ZONE }))
        .on_hover(cx.listener(|this, hovered, _, cx| this.set_peek(*hovered, cx)))
        .when(open, |d| {
            d.p(px(theme::GUTTER)).child(
                div()
                    .w(px(this.sidebar_width))
                    .h_full()
                    .rounded_lg()
                    .bg(rgba(theme::t().panel))
                    .border_1()
                    .border_color(rgba(theme::t().line))
                    .shadow_lg()
                    .overflow_hidden()
                    .child(content(this, cx)),
            )
        })
}

fn content(this: &Dirigent, cx: &mut Context<Dirigent>) -> Stateful<Div> {
    let (groups, orphans) = projects::groups(&this.hosts, &this.links);
    let multi_host = this.hosts.len() > 1;
    let now = std::time::UNIX_EPOCH.elapsed().map(|d| d.as_millis() as i64).unwrap_or(0);
    let inbox = this.inbox_threads();
    let working = this.threads_with(theme::Status::Working);

    let title_row = div()
        .flex()
        .flex_row()
        .items_center()
        .h(px(40.)) // same as `lists::header`, so the title aligns with the main pane's
        .pl_4()
        .pr_2()
        .child(div().flex_1().font_weight(FontWeight::MEDIUM).text_color(rgba(theme::t().fg)).child("Dirigent"))
        .child(theme::icon_button("new-thread", "plus", cx, |this, cx| this.new_draft(None, cx)));

    // Every row and card in the sidebar can be highlighted (selected / hovered), so each
    // list keeps a 4px gap between neighbors so their backgrounds never touch.
    let nav = div()
        .flex()
        .flex_col()
        .gap_1()
        .px_2()
        .child(section("inbox", "Inbox", Section::Inbox, || Pane::Inbox, inbox, now, this, cx))
        .child(section("workpool", "Workpool", Section::Workpool, || Pane::Workpool, working, now, this, cx));

    let project_rows: Vec<AnyElement> = groups.iter().map(|g| project_row(g, &groups, this, cx)).collect();
    let orphan_row = (!orphans.is_empty()).then(|| {
        let selected = matches!(&this.pane, Pane::Project { group_key } if group_key == projects::ORPHANS);
        let n = orphans.len();
        div().px_2().child(
            row("orphans", selected, cx, |this, cx| this.show(Pane::Project { group_key: projects::ORPHANS.into() }, cx))
                .child(div().flex_1().min_w_0().truncate().text_color(rgba(theme::t().fg_1)).child("Removed projects"))
                .child(count(n)),
        )
    });

    // Section label with a `+` that appears on hover (or stays while its host menu is open).
    let add_open = this.menu == Some(Menu::NewProjectHost);
    let add = div()
        .id("new-project")
        .relative()
        .size(px(20.))
        .flex()
        .items_center()
        .justify_center()
        .rounded_sm()
        .cursor_pointer()
        .opacity(if add_open { 1. } else { 0. })
        .group_hover("section", |d| d.opacity(1.))
        .when(add_open, |d| d.bg(rgba(theme::t().hover)))
        .hover(|d| d.bg(rgba(theme::t().hover)))
        .on_click(cx.listener(
            move |this, _, _, cx| {
                if multi_host { this.toggle_menu(Menu::NewProjectHost, cx) } else { this.start_new_project(None, cx) }
            },
        ))
        .child(icon("plus", theme::t().fg_1))
        .children(add_open.then(|| {
            let mut items = vec![menu::header("New project on")];
            items.extend(this.hosts.iter().map(|h| {
                let name = h.name.clone();
                let status = format!("{:?}", h.status).to_lowercase();
                menu::item(
                    format!("nph/{name}"),
                    name.clone(),
                    status,
                    false,
                    move |this, cx| this.start_new_project(Some(name.clone()), cx),
                    cx,
                )
            }));
            menu::popup(Side::Below(20.), items, cx)
        }));
    let projects_section = div()
        .flex()
        .flex_col()
        .gap_1()
        .pt_5()
        .child(
            div()
                .group("section")
                .h(px(26.))
                .pl_4()
                .pr_3()
                .flex()
                .flex_row()
                .items_center()
                .child(div().flex_1().text_size(px(12.)).text_color(rgba(theme::t().fg_2)).child("Projects"))
                .child(add),
        )
        .children(project_rows)
        .children(orphan_row)
        .children(
            groups
                .is_empty()
                .then(|| div().px_4().py_1().text_size(px(12.)).text_color(rgba(theme::t().fg_2)).child("Add a project with +")),
        );

    // Bottom bar: hide / show the sidebar on the left, settings on the right.
    let hidden = this.sidebar_hidden;
    let panel_icon = if hidden { "panel-left-open" } else { "panel-left-close" };
    let bottom = div()
        .flex()
        .flex_row()
        .items_center()
        .justify_between()
        // Mirrors the title row: same height and right padding so the gear sits under the
        // `+`; on the left the button's 6px inner inset is taken off so the glyph starts
        // at the title's x.
        .h(px(40.))
        .pl(px(10.))
        .pr_2()
        .child(theme::icon_button("sidebar-toggle", panel_icon, cx, move |this, cx| this.set_sidebar_hidden(!hidden, cx)))
        .child(theme::icon_button("settings", "settings", cx, |this, cx| this.show_config(cx)));

    div()
        .id("sidebar")
        .size_full()
        .flex()
        .flex_col()
        .child(title_row)
        .child(div().id("nav").flex_1().overflow_y_scroll().child(nav).child(projects_section))
        .child(bottom)
}

/// A selectable sidebar row (28px). `selected` lifts the background. It has no right
/// padding: the trailing child supplies it (`count` with its margin, or a `trailing` box
/// that is flush with the nav padding like the title's `+`, so icons line up with it).
fn row(
    id: impl Into<SharedString>,
    selected: bool,
    cx: &mut Context<Dirigent>,
    on_click: impl Fn(&mut Dirigent, &mut Context<Dirigent>) + 'static,
) -> Stateful<Div> {
    div()
        .id(gpui::ElementId::Name(id.into()))
        .group("row")
        .flex()
        .flex_row()
        .items_center()
        .gap_2()
        .h(px(28.))
        .pl_2()
        .rounded_md()
        .cursor_pointer()
        .when(selected, |d| d.bg(rgba(theme::t().hover)))
        .hover(|d| d.bg(rgba(theme::t().hover)))
        .on_click(cx.listener(move |this, _, _, cx| on_click(this, cx)))
}

/// Inbox / Workpool: its nav row, then (unless collapsed) a card per thread (finished and
/// not snoozed under Inbox, working under Workpool), each with how long its agent has run
/// (live) or ran (last run).
#[allow(clippy::too_many_arguments)]
fn section(
    id: &'static str,
    label: &'static str,
    section: Section,
    pane: fn() -> Pane,
    rows: Vec<(&str, &ThreadSummary)>,
    now: i64,
    this: &Dirigent,
    cx: &mut Context<Dirigent>,
) -> Div {
    let open = !this.collapsed.contains(&section);
    let selected = std::mem::discriminant(&this.pane) == std::mem::discriminant(&pane());
    let cards: Vec<_> = rows
        .into_iter()
        .map(|(host, t)| {
            let elapsed = match (t.running_since, t.last_run_ms) {
                (Some(since), _) => elapsed(now - since),
                (None, Some(ms)) => duration(ms),
                (None, None) => String::new(),
            };
            thread_card(host, t, elapsed, this.hosts.len() > 1, this, cx)
        })
        .collect();
    // Count every thread the page lists (snoozed ones too), not just the cards.
    let n = match section {
        Section::Inbox => this.inbox_all().len(),
        Section::Workpool => this.threads_with(theme::Status::Working).len(),
    };
    div()
        .flex()
        .flex_col()
        .gap_1()
        .child(nav_row(id, label, section, open, selected, n, cx, move |this, cx| this.show(pane(), cx)))
        .when(open, |d| d.children(cards))
}

/// Inbox / Workpool row: opens the page. On its right sits the thread count, replaced by
/// a chevron while the row is hovered; clicking it folds the thread cards under the row.
#[allow(clippy::too_many_arguments)]
fn nav_row(
    id: &'static str,
    label: &'static str,
    section: Section,
    open: bool,
    selected: bool,
    n: usize,
    cx: &mut Context<Dirigent>,
    on_click: impl Fn(&mut Dirigent, &mut Context<Dirigent>) + 'static,
) -> Stateful<Div> {
    let fg = if selected { theme::t().fg } else { theme::t().fg_1 };
    // Count and chevron are stacked in one box so swapping them doesn't shift layout; the
    // count keeps the text inset, the chevron is centered like the title's `+`.
    let fill = || div().absolute().inset_0().flex().items_center();
    let fold = trailing(SharedString::from(format!("fold/{id}")))
        .on_click(cx.listener(move |this, _, _, cx| {
            cx.stop_propagation();
            this.toggle_section(section, cx)
        }))
        .child(fill().justify_end().group_hover("row", |d| d.opacity(0.)).child(count(n)))
        .child(
            fill()
                .justify_center()
                .opacity(0.)
                .group_hover("row", |d| d.opacity(1.))
                .child(icon(if open { "chevron-down" } else { "chevron-right" }, theme::t().fg_2).size(px(14.))),
        );
    row(id, selected, cx, on_click)
        .child(div().flex_1().text_color(rgba(fg)).when(selected, |d| d.font_weight(FontWeight::MEDIUM)).child(label))
        .child(fold)
}

/// A thread under Inbox / Workpool: `project, host … elapsed` / title / model. The title
/// is accent-colored while the thread is unread and led by an amber pin while pinned. Hovering a finished thread swaps its run
/// time for a snooze button, which drops it from the sidebar until its next run.
fn thread_card(
    host: &str,
    t: &ThreadSummary,
    elapsed: String,
    show_host: bool,
    this: &Dirigent,
    cx: &mut Context<Dirigent>,
) -> Stateful<Div> {
    let (h, id) = (host.to_owned(), t.id.clone());
    let selected = this.open().is_some_and(|o| o.host == host && o.id == t.id);
    let unread = this.unread(host, t);
    let pinned = this.pinned(host, &t.id);
    let title = t.name.clone().unwrap_or_else(|| "Untitled".into());
    let titling = this.titling(host, &t.id);
    let title_id = SharedString::from(format!("title/{h}/{id}"));
    let mut place = this.project_name(host, t);
    if show_host {
        place = if place.is_empty() { host.to_owned() } else { format!("{place}, {host}") };
    }
    let model = t.model.as_deref().map(theme::short_model).unwrap_or("").to_owned();
    let small = |text: String, color: u32| div().min_w_0().truncate().text_size(px(11.)).text_color(rgba(color)).child(text);
    // Run time and snooze button are stacked so swapping them on hover doesn't shift layout.
    let snooze = (theme::Status::of(t) == theme::Status::Done).then(|| {
        let (h, id) = (h.clone(), id.clone());
        div()
            .id(SharedString::from(format!("snooze/{h}/{id}")))
            .absolute()
            .top(px(-4.))
            .right(px(-4.))
            .size(px(20.))
            .flex()
            .items_center()
            .justify_center()
            .rounded_sm()
            .opacity(0.)
            .group_hover("card", |d| d.opacity(1.))
            .hover(|d| d.bg(rgba(theme::t().line)))
            .on_click(cx.listener(move |this, _, _, cx| {
                cx.stop_propagation();
                this.set_snoozed(&h, &id, true, cx)
            }))
            .child(icon("moon", theme::t().fg_1))
    });
    div()
        .id(SharedString::from(format!("card/{h}/{id}")))
        .group("card")
        .flex()
        .flex_col()
        .gap_0p5()
        .px_2()
        .py_1p5()
        .rounded_md()
        .cursor_pointer()
        .when(selected, |d| d.bg(rgba(theme::t().hover)))
        .hover(|d| d.bg(rgba(theme::t().hover)))
        .on_click(cx.listener(move |this, _, _, cx| this.select_thread(&h, &id, cx)))
        .child(
            div().flex().flex_row().items_center().gap_2().child(small(place, theme::t().fg_2).flex_1()).child(
                div()
                    .relative()
                    .flex_shrink_0()
                    .child(small(elapsed, theme::t().fg_2).when(snooze.is_some(), |d| d.group_hover("card", |d| d.opacity(0.))))
                    .children(snooze),
            ),
        )
        .child(
            div()
                .flex()
                .flex_row()
                .items_center()
                .gap_1()
                .min_w_0()
                .text_color(rgba(if unread { theme::t().accent_ink } else { theme::t().fg }))
                .when(unread, |d| d.font_weight(FontWeight::MEDIUM))
                .children(pinned.then(theme::pin_mark))
                .child(theme::title_text(title_id, title, titling)),
        )
        .when(!model.is_empty(), |d| d.child(small(model, theme::t().fg_2)))
}

/// Trailing count on a `row`; its margin is the row's right inset, so it ends where the
/// cards' text does.
fn count(n: usize) -> Div {
    div().mr_2().text_size(px(11.)).text_color(rgba(theme::t().fg_2)).child(if n > 0 { n.to_string() } else { String::new() })
}

/// Trailing control box on a `row`: as wide as the title's `+` button and flush with the
/// nav padding, so a centered icon lines up with it. Highlights on hover.
fn trailing(id: SharedString) -> Stateful<Div> {
    div()
        .id(id)
        .relative()
        .w(px(26.))
        .h(px(20.))
        .flex()
        .items_center()
        .justify_center()
        .rounded_sm()
        .cursor_pointer()
        .hover(|d| d.bg(rgba(theme::t().line)))
}

/// Project name, which hosts it spans, and a `⋯` menu that appears on hover.
fn project_row(g: &Group, all: &[Group], this: &Dirigent, cx: &mut Context<Dirigent>) -> AnyElement {
    let key = g.key.clone();
    let selected = matches!(&this.pane, Pane::Project { group_key } if *group_key == key);
    let menu_open = this.menu == Some(Menu::Project { group_key: key.clone() });
    let link_open = this.menu == Some(Menu::Link { group_key: key.clone() });

    let mut actions: Vec<AnyElement> = vec![];
    if menu_open {
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

    let fg = if selected { theme::t().fg } else { theme::t().fg_1 };
    let k = key.clone();
    div()
        .px_2()
        .child(
            row(SharedString::from(format!("p/{key}")), selected, cx, move |this, cx| {
                this.show(Pane::Project { group_key: k.clone() }, cx)
            })
            .child(div().flex_1().min_w_0().truncate().text_color(rgba(fg)).child(g.name.clone()))
            .child(
                trailing(SharedString::from(format!("gm/{key}")))
                    .opacity(if menu_open || link_open { 1. } else { 0. })
                    .group_hover("row", |d| d.opacity(1.))
                    .when(menu_open || link_open, |d| d.bg(rgba(theme::t().line)))
                    .on_click(cx.listener(move |this, _, _, cx| {
                        cx.stop_propagation();
                        this.toggle_menu(Menu::Project { group_key: key.clone() }, cx)
                    }))
                    .child(icon("ellipsis", theme::t().fg_1))
                    .children((menu_open || link_open).then(|| menu::popup(Side::Below(20.), actions, cx))),
            ),
        )
        .into_any()
}
