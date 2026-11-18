use crate::ambient::Reach;
use crate::app::{ArchiveAt, Dirigent, DragProject, Menu, PANE_MIN, Pane, Renaming, Resize, Section, Target};
use crate::hosts;
use crate::icons::icon;
use crate::menu::{self, Side};
use crate::projects::{self, Group};
use crate::proxy;
use crate::theme::{self, Status, text, u};
use crate::thread::{self, duration, elapsed};
use crate::titlebar;
use crate::toasts;
use dirigent_protocol::{ThreadSummary, release};
use gpui::{
    AnyElement, App, Bounds, Context, Div, DragMoveEvent, FontWeight, Hsla, MouseButton, MouseDownEvent, Pixels, Render, SharedString,
    Stateful, Window, canvas, div, fill, linear_color_stop, linear_gradient, point, prelude::*, px, rgba, size,
};

// MUST KILL: rename: WIDTH → DEFAULT_WIDTH, the user drags it between app::SIDEBAR_MIN and the window minus app::PANE_MIN
pub const WIDTH: f32 = 240.;
// MUST KILL: extract: PEEK_ZONE → derived from ruler::INSET, so it stays narrower than the ruler's marks
const PEEK_ZONE: f32 = 6.;
const ROW: f32 = 28.;
const GAP: f32 = 4.;
// MUST KILL: extract: card_height's numbers → constants thread_card lays its lines out with
fn card_height(vcs: &str) -> f32 {
    let two_lines = 18. + 21. + 2. + 2. * 6.;
    if vcs.is_empty() { two_lines } else { two_lines + 2. + 18. }
}

#[derive(Clone)]
pub struct Ghost {
    pub name: String,
    pub width: f32,
    pub height: f32,
    pub indent: f32,
    pub weight: FontWeight,
    pub font: SharedString,
}

impl Render for Ghost {
    fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
        div()
            .w(u(self.width))
            .h(u(self.height))
            .pl(u(self.indent))
            .flex()
            .items_center()
            .rounded_md()
            .bg(rgba(theme::t().popup))
            .border_1()
            .border_color(rgba(theme::t().line))
            .shadow_md()
            .font_family(self.font.clone())
            .text_size(text::base())
            .text_color(rgba(theme::t().fg))
            .font_weight(self.weight)
            .child(div().min_w_0().truncate().child(self.name.clone()))
    }
}

pub fn docked(this: &Dirigent, cx: &mut Context<Dirigent>) -> Div {
    let handle = div()
        .id("sidebar-resize")
        .absolute()
        .top_0()
        .right(u(-3.))
        .w(u(6.))
        .h_full()
        .cursor_col_resize()
        .hover(|d| d.bg(rgba(theme::t().line)))
        .on_mouse_down(MouseButton::Left, cx.listener(|this, _, _, cx| this.start_resize(Resize::Sidebar, cx)));
    div()
        .relative()
        .w(u(this.sidebar_width))
        .min_w_0()
        .h_full()
        .border_r_1()
        .border_color(rgba(theme::t().line_soft))
        .when(this.config.background.reach == Reach::Edge, |d| d.children(this.field.layer(px(0.))))
        .child(content(this, cx))
        .child(handle)
}

pub fn peek(this: &Dirigent, cx: &mut Context<Dirigent>) -> Stateful<Div> {
    let held =
        matches!(this.menu, Some(Menu::ThreadActions { .. } | Menu::Toasts)) || matches!(this.renaming, Some(Renaming::Thread { .. }));
    let open = this.sidebar_peek || held;
    div()
        .id("sidebar-peek")
        .absolute()
        .left_0()
        .top_0()
        .h_full()
        .map(|d| if open { d.right(u(PANE_MIN)).max_w(u(this.sidebar_width + 2. * theme::GUTTER)) } else { d.w(u(PEEK_ZONE)) })
        .on_hover(cx.listener(|this, hovered, _, cx| this.set_peek(*hovered, cx)))
        // Only the whole copy can occlude: an occluding child would unhover it, and it would close.
        .when(open, |d| d.occlude())
        .when(open, |d| {
            let radius = u(8.) - px(1.);
            d.p(u(theme::GUTTER)).child(
                div()
                    .relative()
                    .size_full()
                    .rounded_lg()
                    .bg(rgba(theme::alpha(theme::t().canvas, 0xff)))
                    .border_1()
                    .border_color(rgba(theme::t().line))
                    .shadow_lg()
                    .overflow_hidden()
                    .children(this.field.layer(radius))
                    .child(content(this, cx)),
            )
        })
}

fn content(this: &Dirigent, cx: &mut Context<Dirigent>) -> Stateful<Div> {
    let (groups, _) = this.groups();
    let inbox = this.inbox_threads();
    let working = this.busy_threads();

    let version = release::VERSION;
    let short = if release::nightly() { "nightly" } else { version };
    let title_row = div()
        .flex()
        .flex_row()
        .items_center()
        .h(u(titlebar::HEIGHT))
        .pl_4()
        .pr_2()
        .child(
            div()
                .h_full()
                .flex()
                .items_center()
                .font_weight(FontWeight::MEDIUM)
                .text_color(rgba(theme::t().fg))
                .child("Dirigent")
                .when(titlebar::CUSTOM, |d| titlebar::grab(d, cx)),
        )
        .child(
            div()
                .id("version")
                .h_full()
                .flex()
                .items_center()
                .ml_1p5()
                .cursor_pointer()
                .text_color(rgba(theme::t().fg_2))
                .hover(|d| d.text_color(rgba(theme::t().fg)))
                .on_click(cx.listener(|this, _, _, cx| this.open_changelog(cx)))
                .when(release::nightly(), |d| d.tooltip(theme::tooltip(version)))
                .child(short),
        )
        .child(div().flex_1().h_full().when(titlebar::CUSTOM, |d| titlebar::grab(d, cx)))
        .child(
            theme::icon_button("projects", "folder", cx, |this, cx| this.show_projects(cx))
                .when(matches!(this.pane, Pane::Projects), |d| d.bg(rgba(theme::t().hover))),
        )
        .child(theme::icon_button("new-thread", "plus", cx, |this, cx| this.new_draft(None, cx)));

    let dragging = this.dragging.clone().filter(|d| d.sidebar && cx.has_active_drag()).map(|d| d.key);
    let pinned: Vec<AnyElement> = projects::pinned(&groups, &this.pinned_projects)
        .into_iter()
        .map(|g| project_row(g, dragging.as_deref() == Some(&g.key), this, cx))
        .collect();
    let pinned = (!pinned.is_empty()).then(|| {
        div().flex_shrink_0().flex().flex_col().gap(u(GAP)).pb_2().children(pinned).on_drag_move(cx.listener(
            |this, ev: &DragMoveEvent<DragProject>, _, cx| {
                let drag = ev.drag(cx);
                if !drag.sidebar {
                    return;
                }
                let keys = this.pinned_keys();
                let slot = ((ev.event.position.y - ev.bounds.top()) / u(ROW + GAP)).floor().max(0.) as usize;
                if let Some(onto) = keys.get(slot.min(keys.len().saturating_sub(1))) {
                    let key = drag.key.clone();
                    this.move_project(&key, onto, cx);
                }
            },
        ))
    });

    let (n_inbox, n_working) = (inbox.len(), working.len());
    let show_inbox = !this.inbox_all().is_empty();
    let n_snoozed = this.inbox_all().len() - n_inbox;
    let inbox_header = header(
        "inbox",
        "Inbox",
        Section::Inbox,
        || Pane::Inbox,
        theme::t().warn,
        (n_inbox, n_snoozed),
        ["chevron-right", "chevron-down"],
        this,
        cx,
    );
    let working_header = header(
        "workpool",
        "Workpool",
        Section::Workpool,
        || Pane::Workpool,
        theme::t().working,
        (n_working, 0),
        ["chevron-right", "chevron-up"],
        this,
        cx,
    );
    let both = !this.collapsed.contains(&Section::Inbox) && !this.collapsed.contains(&Section::Workpool) && n_inbox > 0 && n_working > 0;
    let inbox_cards = cards("inbox-cards", Section::Inbox, inbox, true, this, cx).track_scroll(&this.inbox_scroll);
    let working_cards = cards("workpool-cards", Section::Workpool, working, false, this, cx).track_scroll(&this.workpool_scroll).mt_auto();

    let hidden = this.sidebar_hidden;
    let panel_icon = if hidden { "panel-left-open" } else { "panel-left-close" };
    let bottom = div()
        .flex()
        .flex_row()
        .items_center()
        .justify_between()
        .h(u(40.))
        .pl(u(10.))
        .pr_2()
        .child(theme::icon_button("sidebar-toggle", panel_icon, cx, move |this, cx| this.set_sidebar_hidden(!hidden, cx)))
        .child(
            div()
                .flex()
                .flex_row()
                .children(proxy_update_button(this, cx))
                .children(update_button(this, cx))
                .children(toasts::button(this, cx))
                .child(theme::icon_button("usage", "chart-no-axes-combined", cx, |this, cx| this.open_usage(cx)))
                .child(theme::icon_button("settings", "settings", cx, |this, cx| this.open_settings(cx))),
        );

    div()
        .id("sidebar")
        .size_full()
        .flex()
        .flex_col()
        .child(title_row.flex_shrink_0())
        .children(pinned)
        .when(show_inbox, |d| d.child(inbox_header))
        .child(inbox_cards)
        .when(both, |d| d.child(seam(this)))
        .child(working_cards)
        .when(n_working > 0, |d| d.child(working_header))
        .child(bottom.flex_shrink_0())
}

fn update_button(this: &Dirigent, cx: &mut Context<Dirigent>) -> Option<AnyElement> {
    let Some(ready) = this.updater.ready.as_ref() else {
        let download = this.updater.downloading.as_ref()?;
        let menu = (this.menu == Some(Menu::Update)).then(|| {
            let mut rows = vec![menu::header(format!("Downloading Dirigent {}", download.version))];
            if let Some(t) = &download.transfer {
                let total = t.total.map(|total| format!("/{}", total >> 20)).unwrap_or_default();
                rows.push(menu::header(format!("{}{total} MiB, {:.1} MiB/s", t.received >> 20, t.per_second / f64::from(1 << 20))));
            }
            menu::popup(Side::Above, rows, cx)
        });
        let button = theme::tinted_icon_button("update", "circle-arrow-down", theme::t().accent_ink, cx, |this, cx| {
            this.toggle_menu(Menu::Update, cx)
        });
        return Some(button.children(menu).into_any_element());
    };
    let armed = this.updater.when_idle;
    let menu = (this.menu == Some(Menu::Update)).then(|| {
        let busy = this.local_busy();
        let action = if armed {
            menu::item("update-cancel", "Cancel restart", "waiting for agents", false, |this, cx| this.cancel_restart(cx), cx)
        } else if busy > 0 {
            let detail = if busy == 1 { "1 agent working".to_owned() } else { format!("{busy} agents working") };
            menu::item("update-idle", "Restart when idle", detail, false, |this, cx| this.restart_to_update(cx), cx)
        } else {
            menu::item("update-now", "Restart to update", "", false, |this, cx| this.restart_to_update(cx), cx)
        };
        menu::popup(Side::Above, vec![menu::header(format!("Dirigent {} is ready", ready.version)), action], cx)
    });
    let icon = if armed { "hourglass" } else { "circle-arrow-down" };
    let button = theme::tinted_icon_button("update", icon, theme::t().accent_ink, cx, |this, cx| this.toggle_menu(Menu::Update, cx));
    Some(button.children(menu).into_any_element())
}

fn proxy_update_button(this: &Dirigent, cx: &mut Context<Dirigent>) -> Option<AnyElement> {
    let (host, update) = this.hosts.iter().find_map(|h| Some((h.name.clone(), h.proxy_update.as_ref()?)))?;
    let tip = proxy::outdated(update, (this.hosts.len() > 1).then_some(host.as_str()));
    let button = theme::tinted_icon_button("proxy-update", "triangle-alert", theme::t().warn, cx, move |this, cx| {
        this.open_proxy_settings(Some(host.clone()), cx)
    });
    Some(button.tooltip(theme::tooltip(tip)).into_any_element())
}

// MUST KILL: rearchitect: row leaves its right padding to the trailing box → a row that takes its trailing control
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
        .h(u(ROW))
        .pl_2()
        .rounded_md()
        .cursor_pointer()
        .when(selected, |d| d.bg(rgba(theme::t().hover)))
        .hover(|d| d.bg(rgba(theme::t().hover)))
        .on_click(cx.listener(move |this, _, _, cx| on_click(this, cx)))
}

// MUST KILL: type: header's (n, snoozed) and chevrons [folded, open] → named fields of a section spec
#[allow(clippy::too_many_arguments)]
fn header(
    id: &'static str,
    label: &'static str,
    section: Section,
    pane: fn() -> Pane,
    color: u32,
    (n, snoozed): (usize, usize),
    chevrons: [&'static str; 2],
    this: &Dirigent,
    cx: &mut Context<Dirigent>,
) -> Div {
    let open = !this.collapsed.contains(&section);
    let selected = std::mem::discriminant(&this.pane) == std::mem::discriminant(&pane());
    let small = |color: u32| div().flex().flex_row().items_center().gap_0p5().text_size(text::xs()).text_color(rgba(color));
    let numbers = div()
        .flex()
        .flex_row()
        .items_center()
        .gap_1p5()
        .when(n > 0, |d| d.child(small(color).child(n.to_string())))
        .when(snoozed > 0, |d| {
            d.child(small(theme::t().fg_2).child(icon("moon", theme::t().fg_2).size(u(11.))).child(snoozed.to_string()))
        });
    let fold = trailing(SharedString::from(format!("fold/{id}")))
        .on_click(cx.listener(move |this, _, _, cx| {
            cx.stop_propagation();
            this.toggle_section(section, cx)
        }))
        .child(icon(chevrons[open as usize], theme::t().fg_2).size(u(14.)));
    div().flex_shrink_0().px_2().child(
        row(id, selected, cx, move |this, cx| this.show(pane(), cx))
            .child(div().flex_1().text_color(rgba(color)).when(selected, |d| d.font_weight(FontWeight::MEDIUM)).child(label))
            .child(numbers)
            .child(fold),
    )
}

fn share(id: &'static str, natural: f32) -> Stateful<Div> {
    div().id(id).flex_1().min_h_0().max_h(u(natural)).overflow_y_scroll()
}

// MUST KILL: extract: seam's px(1.) threshold → a named slack for sub-pixel rounding
fn seam(this: &Dirigent) -> impl IntoElement {
    const FADE: f32 = 48.;
    let (inbox, workpool) = (this.inbox_scroll.clone(), this.workpool_scroll.clone());
    let paint = move |b: Bounds<Pixels>, _: (), window: &mut Window, _: &mut App| {
        if inbox.max_offset().y < px(1.) && workpool.max_offset().y < px(1.) {
            return;
        }
        let line: Hsla = rgba(theme::t().line).into();
        let clear = line.opacity(0.);
        let left = Bounds::new(b.origin, size(u(FADE), b.size.height));
        let right = Bounds::new(point(b.right() - u(FADE), b.top()), left.size);
        // As in CSS, a 90° gradient runs left to right.
        window.paint_quad(fill(left, linear_gradient(90., linear_color_stop(clear, 0.), linear_color_stop(line, 1.))));
        window.paint_quad(fill(Bounds::from_corners(left.top_right(), right.bottom_left()), line));
        window.paint_quad(fill(right, linear_gradient(90., linear_color_stop(line, 0.), linear_color_stop(clear, 1.))));
    };
    canvas(|_, _, _| {}, paint).flex_shrink_0().h(px(1.)).mx_4()
}

fn cards(
    id: &'static str,
    section: Section,
    rows: Vec<(&str, &ThreadSummary)>,
    below: bool,
    this: &Dirigent,
    cx: &mut Context<Dirigent>,
) -> Stateful<Div> {
    let rows = if this.collapsed.contains(&section) { vec![] } else { rows };
    let n = rows.len();
    let mut natural = if !below && n > 0 { GAP } else { 0. };
    let cards: Vec<_> = rows
        .into_iter()
        .map(|(host, t)| {
            let elapsed = match (t.running_since, t.last_run_ms) {
                (Some(since), _) => elapsed(this.clock - since),
                (None, Some(ms)) => duration(ms),
                (None, None) => String::new(),
            };
            let vcs = vcs_label(host, t, this);
            natural += card_height(&vcs) + GAP;
            thread_card(host, t, vcs, elapsed, this, cx)
        })
        .collect();
    share(id, natural).flex().flex_col().gap(u(GAP)).px_2().when(n > 0, |d| if below { d.pt(u(GAP)) } else { d.py(u(GAP)) }).children(cards)
}

fn thread_card(host: &str, t: &ThreadSummary, vcs: String, elapsed: String, this: &Dirigent, cx: &mut Context<Dirigent>) -> Stateful<Div> {
    let (h, id) = (host.to_owned(), t.id.clone());
    let selected = this.open().is_some_and(|o| o.host == host && o.id == t.id);
    let renaming = this.renaming == Some(Renaming::Thread { host: h.clone(), id: id.clone() });
    let hovered = this.menu.is_none() && !renaming && this.hovered_card.as_ref().is_some_and(|(hh, i)| hh == host && *i == t.id);
    let unread = this.unread(host, t);
    let pinned = t.pinned;
    let title = t.name.clone().unwrap_or_else(|| "Untitled".into());
    let titling = this.titling(host, &t.id);
    let title_id = SharedString::from(format!("title/{h}/{id}"));
    let place = this.project_name(host, t);
    let waiting = this.status(host, t) == Status::Waiting;
    let marks = || {
        let server = (host != hosts::LOCAL).then(|| icon("server", theme::t().fg_2).size(u(12.)));
        server.into_iter().chain(t.model.as_deref().map(|m| icon(lab_logo(m), theme::t().fg_2).size(u(12.))))
    };
    let small = |text: String, color: u32| div().min_w_0().truncate().text_size(text::xs()).text_color(rgba(color)).child(text);
    let archive_confirm = thread::archive_confirm(host, &t.id, ArchiveAt::Sidebar, Side::Below(20.), this, cx);
    let asking = archive_confirm.is_some();
    type Action = Box<dyn Fn(&mut Dirigent, &mut Context<Dirigent>)>;
    let button = |id: String, name: &'static str, on_click: Action| {
        div()
            .id(SharedString::from(id))
            .size(u(20.))
            .flex()
            .items_center()
            .justify_center()
            .rounded_sm()
            .hover(|d| d.bg(rgba(theme::t().line)))
            .on_click(cx.listener(move |this, _, _, cx| {
                cx.stop_propagation();
                on_click(this, cx)
            }))
            .child(icon(name, theme::t().fg_1))
    };
    let actions = this.snoozable(host, t).then(|| {
        let (h1, id1, h2, id2) = (h.clone(), id.clone(), h.clone(), id.clone());
        let at_rest = this.at_rest(host, t);
        div()
            .absolute()
            .top(u(-4.))
            .right(u(-4.))
            .flex()
            .flex_row()
            .gap_0p5()
            .opacity(if asking { 1. } else { 0. })
            .group_hover("card", |d| d.opacity(1.))
            .child(button(format!("snooze/{h}/{id}"), "moon", Box::new(move |this, cx| this.set_snoozed(&h1, &id1, true, cx))))
            .when(at_rest, |d| {
                d.child(
                    button(
                        format!("archive/{h}/{id}"),
                        "archive",
                        Box::new(move |this, cx| this.set_archived(&h2, &id2, true, ArchiveAt::Sidebar, cx)),
                    )
                    .relative()
                    .when(asking, |d| d.bg(rgba(theme::t().line)))
                    .children(archive_confirm),
                )
            })
    });
    let context_menu = match &this.menu {
        Some(Menu::ThreadActions { host: mh, id: mid, at }) if mh == host && *mid == t.id => {
            let (h1, id1, h2, id2) = (h.clone(), id.clone(), h.clone(), id.clone());
            let items = if this.retitling(host, &t.id) {
                vec![menu::header("Regenerating the title…")]
            } else {
                vec![
                    menu::item("rename", "Rename thread", "", false, move |this, cx| this.start_thread_rename(&h1, &id1, cx), cx),
                    menu::item("retitle", "Regenerate title", "", false, move |this, cx| this.retitle(&h2, &id2, cx), cx),
                ]
            };
            Some(menu::popup(Side::At(*at), items, cx))
        }
        _ => None,
    };
    let title_line = if renaming {
        div()
            .mx(u(-4.) - px(1.))
            .my(px(-1.))
            .px(u(4.))
            .rounded_sm()
            .border_1()
            .border_color(rgba(theme::t().accent_line))
            .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
            .child(this.rename_input.clone())
    } else {
        div().child(theme::title_text(title_id, title, titling))
    };
    let (h1, id1, h2, id2) = (h.clone(), id.clone(), h.clone(), id.clone());
    div()
        .id(SharedString::from(format!("card/{h}/{id}")))
        .group("card")
        .relative()
        .h(u(card_height(&vcs)))
        .flex_shrink_0()
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
        .on_mouse_down(
            MouseButton::Right,
            cx.listener(move |this, ev: &MouseDownEvent, _, cx| this.show_thread_menu(&h2, &id2, ev.position, cx)),
        )
        .on_hover(cx.listener(move |this, over, _, cx| {
            let me = (h1.clone(), id1.clone());
            if *over {
                this.hovered_card = Some(me);
            } else if this.hovered_card == Some(me) {
                this.hovered_card = None;
            }
            cx.notify();
        }))
        .child(
            div().flex().flex_row().items_center().gap_2().child(small(place, theme::t().fg_2).flex_1()).child(
                div()
                    .relative()
                    .flex_shrink_0()
                    .child(
                        div()
                            .flex()
                            .flex_row()
                            .items_center()
                            .gap_1()
                            .children(waiting.then(|| icon("hourglass", theme::t().working).size(u(8.))))
                            .child(small(elapsed.clone(), theme::t().fg_2))
                            .when(asking, |d| d.opacity(0.))
                            .when(actions.is_some(), |d| d.group_hover("card", |d| d.opacity(0.))),
                    )
                    .children(actions),
            ),
        )
        .child(
            div()
                .flex()
                .flex_row()
                .items_center()
                .gap_1()
                .min_w_0()
                .text_color(rgba(this.title_color(host, t)))
                .when(unread || t.asking(), |d| d.font_weight(FontWeight::MEDIUM))
                .children(pinned.then(theme::pin_mark))
                .child(title_line.flex_1().min_w_0())
                .when(vcs.is_empty(), |d| d.children(marks().map(|m| m.ml_0p5()))),
        )
        .when(!vcs.is_empty(), |d| {
            d.child(div().flex().flex_row().items_center().gap_1p5().child(small(vcs.clone(), theme::t().fg_2).flex_1()).children(marks()))
        })
        .children(hovered.then(|| menu::popup(Side::Right, specs(host, t, vcs, elapsed, this), cx)))
        .children(context_menu)
}

fn vcs_label(host: &str, t: &ThreadSummary, this: &Dirigent) -> String {
    match &t.workspace {
        Some(w) => projects::basename(w).to_owned(),
        None => this.vcs_of(host, &t.cwd).and_then(|v| v.branch.clone()).unwrap_or_default(),
    }
}

fn lab_logo(model: &str) -> &'static str {
    let (vendor, name) = model.split_once('/').unwrap_or(("", model));
    if vendor == "anthropic" || name.contains("claude") {
        "claude"
    } else if vendor == "openai" || name.starts_with("gpt") || name.starts_with("o1") || name.starts_with("o3") || name.starts_with("o4") {
        "openai"
    } else {
        "sparkles"
    }
}

fn specs(host: &str, t: &ThreadSummary, vcs: String, elapsed: String, this: &Dirigent) -> Vec<AnyElement> {
    let model = match (&t.model, &t.effort) {
        (Some(m), Some(e)) => format!("{m} ({e})"),
        (Some(m), None) => m.clone(),
        (None, _) => String::new(),
    };
    let run = match (elapsed.is_empty(), t.running()) {
        (true, _) => String::new(),
        (false, true) => format!("running for {elapsed}"),
        (false, false) => format!("last run {elapsed}"),
    };
    let free = match t.free_jobs {
        0 => String::new(),
        1 => "1 job left running".into(),
        n => format!("{n} jobs left running"),
    };
    let row = |value: String| div().px_2().py_0p5().max_w(u(360.)).child(value);
    let line = |value: String| row(value).text_size(text::sm()).into_any();
    let mut rows = vec![
        row(t.name.clone().unwrap_or_else(|| "Untitled".into())).font_weight(FontWeight::MEDIUM).into_any(),
        line(format!("{}, {host}", this.project_name(host, t))),
    ];
    rows.extend([model, vcs, run, free].into_iter().filter(|s| !s.is_empty()).map(line));
    rows
}

// MUST KILL: extract: trailing's mr(u(3.)) → derived from the title's 26px button around this 20px box
fn trailing(id: SharedString) -> Stateful<Div> {
    div()
        .id(id)
        .relative()
        .size(u(20.))
        .mr(u(3.))
        .flex()
        .items_center()
        .justify_center()
        .rounded_sm()
        .cursor_pointer()
        .hover(|d| d.bg(rgba(theme::t().line)))
}

fn project_row(g: &Group, dragged: bool, this: &Dirigent, cx: &mut Context<Dirigent>) -> AnyElement {
    let key = g.key.clone();
    let selected = matches!(&this.pane, Pane::Project { group_key } if *group_key == key);
    let fg = if selected { theme::t().fg } else { theme::t().fg_1 };
    let k = key.clone();
    let target: Target = g.member_for(None).into();
    let new_thread = trailing(SharedString::from(format!("gp/{key}")))
        .opacity(0.)
        .group_hover("row", |d| d.opacity(1.))
        .on_click(cx.listener(move |this, _, _, cx| {
            cx.stop_propagation();
            this.new_draft(Some(target.clone()), cx)
        }))
        .child(icon("plus", theme::t().fg_1));
    // MUST KILL: extract: the ghost's `- 1. - 16.` → the sidebar's named border and list padding
    let ghost = Ghost {
        name: g.name.clone(),
        width: this.sidebar_width - 1. - 16.,
        height: ROW,
        indent: 8.,
        weight: FontWeight::NORMAL,
        font: this.fonts.ui.clone(),
    };
    let entity = cx.weak_entity();
    div()
        .px_2()
        .when(dragged, |d| d.opacity(0.))
        .child(
            row(SharedString::from(format!("p/{key}")), selected, cx, move |this, cx| {
                this.show(Pane::Project { group_key: k.clone() }, cx)
            })
            .on_drag(DragProject { key: key.clone(), sidebar: true }, move |drag, _, _, cx| {
                let drag = drag.clone();
                let _ = entity.update(cx, |this, cx| {
                    this.dragging = Some(drag);
                    cx.notify();
                });
                cx.new(|_| ghost.clone())
            })
            .child(div().flex_1().min_w_0().truncate().text_color(rgba(fg)).child(g.name.clone()))
            .child(new_thread),
        )
        .into_any()
}
