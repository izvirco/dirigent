//! Left pane on the window canvas, top to bottom: the title row (Projects page folder, new
//! thread `+`); the pinned projects; the Inbox (threads waiting on the user, not snoozed)
//! with its cards hanging below it; the Workpool's cards (working threads) stacked up on
//! its row; the hide/show and settings buttons. Inbox and Workpool fold (the Workpool
//! starts folded: out of sight, out of mind) and leave the sidebar while they have no
//! thread at all.
//! Docked it has a drag handle on its right edge; hidden (`[` or the button) it slides in
//! as a floating panel while the mouse rests on the left edge.

use crate::app::{ArchiveAt, Dirigent, Pane, Resize, Section, Target, is_settings};
use crate::hosts;
use crate::icons::icon;
use crate::menu::{self, Side};
use crate::projects::{self, Group};
use crate::theme;
use crate::thread::{self, duration, elapsed};
use dirigent_protocol::ThreadSummary;
use gpui::{
    AnyElement, Context, Div, DragMoveEvent, FontWeight, MouseButton, Render, SharedString, Stateful, Window, div, prelude::*, px, rgba,
};

/// Default width; the user can drag it between `app::SIDEBAR_MIN` and `SIDEBAR_MAX`.
pub const WIDTH: f32 = 240.;
/// Width of the invisible strip along the window's left edge that reveals a hidden sidebar.
/// Narrower than the ruler's inset (`ruler::INSET`), so it never covers the ruler's marks.
const PEEK_ZONE: f32 = 6.;
/// A sidebar row's height and the gap between rows: the pitch a dragged project snaps to.
const ROW: f32 = 28.;
const GAP: f32 = 4.;
/// A thread card's height (`thread_card`): its lines (11 / 13 / 11px text at gpui's default
/// line height, φ × size rounded), `gap_0p5` between them and `py_1p5`. The branch line
/// only shows when there is a `vcs` label. Computed, not measured, so `cards` knows a
/// list's natural height; keep it in sync with `thread_card`.
fn card_height(vcs: &str) -> f32 {
    let two_lines = 18. + 21. + 2. + 2. * 6.;
    if vcs.is_empty() { two_lines } else { two_lines + 2. + 18. }
}

/// A project row being dragged (gpui drag payload): its group key.
struct DragProject(String);

/// What follows the cursor while a project is dragged: the row as it looks hovered, lifted
/// by a shadow. Painted as its own root, so it carries its own font and size.
struct Ghost {
    name: String,
    width: f32,
    font: SharedString,
}

impl Render for Ghost {
    fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
        div()
            .w(px(self.width))
            .h(px(ROW))
            .pl_2()
            .flex()
            .items_center()
            .rounded_md()
            .bg(rgba(theme::t().popup))
            .border_1()
            .border_color(rgba(theme::t().line))
            .shadow_md()
            .font_family(self.font.clone())
            .text_size(px(13.))
            .text_color(rgba(theme::t().fg))
            .child(div().min_w_0().truncate().child(self.name.clone()))
    }
}

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
    // A hairline on its right edge parts it from the main pane.
    div()
        .relative()
        .w(px(this.sidebar_width))
        .h_full()
        .flex_shrink_0()
        .border_r_1()
        .border_color(rgba(theme::t().line_soft))
        .child(content(this, cx))
        .child(handle)
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
    let (groups, _) = this.groups();
    let now = std::time::UNIX_EPOCH.elapsed().map(|d| d.as_millis() as i64).unwrap_or(0);
    let inbox = this.inbox_threads();
    let working = this.busy_threads();

    let title_row = div()
        .flex()
        .flex_row()
        .items_center()
        .h(px(40.)) // same as `lists::header`, so the title aligns with the main pane's
        .pl_4()
        .pr_2()
        .child(div().flex_1().font_weight(FontWeight::MEDIUM).text_color(rgba(theme::t().fg)).child("Dirigent"))
        // The folder opens the Projects page (lit while it's open); `+` a new thread.
        .child(
            theme::icon_button("projects", "folder", cx, |this, cx| this.show_projects(cx))
                .when(matches!(this.pane, Pane::Projects), |d| d.bg(rgba(theme::t().hover)))
                .tooltip(theme::tooltip("Projects")),
        )
        .child(theme::icon_button("new-thread", "plus", cx, |this, cx| this.new_draft(None, cx)));

    // The pinned projects, right under the title. They reorder by drag: the dragged row
    // leaves a gap that follows the cursor row by row (`move_project`), so the others
    // shuffle around it as it goes. A wider gap parts them from the Inbox.
    let dragging = this.dragging.clone().filter(|_| cx.has_active_drag());
    let pinned: Vec<AnyElement> = projects::pinned(&groups, &this.pinned_projects)
        .into_iter()
        .map(|g| project_row(g, dragging.as_deref() == Some(&g.key), this, cx))
        .collect();
    let pinned = (!pinned.is_empty()).then(|| {
        div().flex_shrink_0().flex().flex_col().gap(px(GAP)).pb_2().children(pinned).on_drag_move(cx.listener(
            |this, ev: &DragMoveEvent<DragProject>, _, cx| {
                let n = this.pinned_keys().len();
                let slot = (f32::from(ev.event.position.y - ev.bounds.top()) / (ROW + GAP)).floor().max(0.) as usize;
                let key = ev.drag(cx).0.clone();
                this.move_project(&key, slot.min(n.saturating_sub(1)), cx);
            },
        ))
    });

    // Inbox and Workpool rows are fixed; the Inbox's cards hang below its row, the
    // Workpool's stack up on its row. The two card lists share the room left (`share`);
    // what neither needs goes above the Workpool's cards. A row
    // with nothing at all behind it (no thread, snoozed ones included) is left out.
    let (n_inbox, n_working) = (inbox.len(), working.len());
    let show_inbox = !this.inbox_all().is_empty();
    let inbox_header =
        header("inbox", "Inbox", Section::Inbox, || Pane::Inbox, theme::t().warn, n_inbox, ["chevron-right", "chevron-down"], this, cx);
    let working_header = header(
        "workpool",
        "Workpool",
        Section::Workpool,
        || Pane::Workpool,
        theme::t().working,
        n_working,
        ["chevron-up", "chevron-down"],
        this,
        cx,
    );
    let inbox_cards = cards("inbox-cards", Section::Inbox, inbox, true, now, this, cx);
    let working_cards = cards("workpool-cards", Section::Workpool, working, false, now, this, cx).mt_auto();

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
        .child(
            theme::icon_button("settings", "settings", cx, |this, cx| this.show_config(cx))
                .when(is_settings(&this.pane), |d| d.bg(rgba(theme::t().hover))),
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
        .child(working_cards)
        .when(n_working > 0, |d| d.child(working_header))
        .child(bottom.flex_shrink_0())
}

/// A selectable sidebar row (`ROW` tall). `selected` lifts the background. It has no right
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
        .h(px(ROW))
        .pl_2()
        .rounded_md()
        .cursor_pointer()
        .when(selected, |d| d.bg(rgba(theme::t().hover)))
        .hover(|d| d.bg(rgba(theme::t().hover)))
        .on_click(cx.listener(move |this, _, _, cx| on_click(this, cx)))
}

/// Inbox / Workpool row: opens the page. Label and count are in the section's `color`
/// (amber for the Inbox, the working blue for the Workpool); the count is the section's
/// cards. While the row is hovered a chevron replaces the count; clicking it folds the
/// cards. `chevrons` is its icon while folded / open: they point to where the cards go.
#[allow(clippy::too_many_arguments)]
fn header(
    id: &'static str,
    label: &'static str,
    section: Section,
    pane: fn() -> Pane,
    color: u32,
    n: usize,
    chevrons: [&'static str; 2],
    this: &Dirigent,
    cx: &mut Context<Dirigent>,
) -> Div {
    let open = !this.collapsed.contains(&section);
    let selected = std::mem::discriminant(&this.pane) == std::mem::discriminant(&pane());
    // Count and chevron are stacked in one box so swapping them doesn't shift layout; the
    // count keeps the text inset, the chevron is centered like the title's `+`.
    let fill = || div().absolute().inset_0().flex().items_center();
    let fold = trailing(SharedString::from(format!("fold/{id}")))
        .on_click(cx.listener(move |this, _, _, cx| {
            cx.stop_propagation();
            this.toggle_section(section, cx)
        }))
        .child(fill().justify_end().group_hover("row", |d| d.opacity(0.)).child(count(n, color)))
        .child(
            fill()
                .justify_center()
                .opacity(0.)
                .group_hover("row", |d| d.opacity(1.))
                .child(icon(chevrons[open as usize], theme::t().fg_2).size(px(14.))),
        );
    div().flex_shrink_0().px_2().child(
        row(id, selected, cx, move |this, cx| this.show(pane(), cx))
            .child(div().flex_1().text_color(rgba(color)).when(selected, |d| d.font_weight(FontWeight::MEDIUM)).child(label))
            .child(fold),
    )
}

/// A list that shares the sidebar's free height with the other lists: it grows (`flex_1`)
/// up to its `natural` height and scrolls past it. Lists in one column so split the room
/// fairly (all long: equal parts; a short one keeps its height, the others share the
/// rest) with nothing measured: `natural` comes from known row heights.
fn share(id: &'static str, natural: f32) -> Stateful<Div> {
    div().id(id).flex_1().min_h_0().max_h(px(natural)).overflow_y_scroll()
}

/// A section's thread cards (none while folded, `card_height` tall), each with how long its agent
/// has run (live) or ran (last run). `below` says the cards hang below their header
/// (Inbox) rather than stack up on it (Workpool), which side the gap to the header goes.
/// Stacked cards get a hairline on top, so they don't run into the Inbox's last (clipped)
/// card.
fn cards(
    id: &'static str,
    section: Section,
    rows: Vec<(&str, &ThreadSummary)>,
    below: bool,
    now: i64,
    this: &Dirigent,
    cx: &mut Context<Dirigent>,
) -> Stateful<Div> {
    let rows = if this.collapsed.contains(&section) { vec![] } else { rows };
    let n = rows.len();
    let mut natural = if !below && n > 0 { 1. + GAP } else { 0. };
    let cards: Vec<_> = rows
        .into_iter()
        .map(|(host, t)| {
            let elapsed = match (t.running_since, t.last_run_ms) {
                (Some(since), _) => elapsed(now - since),
                (None, Some(ms)) => duration(ms),
                (None, None) => String::new(),
            };
            let vcs = vcs_label(host, t, this);
            natural += card_height(&vcs) + GAP;
            thread_card(host, t, vcs, elapsed, this, cx)
        })
        .collect();
    share(id, natural)
        .flex()
        .flex_col()
        .gap(px(GAP))
        .px_2()
        .when(n > 0, |d| if below { d.pt(px(GAP)) } else { d.py(px(GAP)).border_t_1().border_color(rgba(theme::t().line_soft)) })
        .children(cards)
}

/// A thread under Inbox / Workpool: `project ... elapsed` / title / `branch ... [server]
/// logo`, the last line being its workspace name or branch (`vcs`, from `vcs_label`).
/// Outside version control there is no branch, so the card drops that line and the server
/// icon and logo end the title line instead (`card_height`). The title is amber while the
/// model waits on an answer, accent while the thread is unread, and led by an amber pin
/// while pinned. The server icon shows when the thread lives on another host, the logo is
/// the model's lab. Hovering a `snoozable` thread swaps
/// its run time for snooze (drops it from the sidebar until its next run) and archive
/// buttons (archiving a workspace thread asks under the button, `thread::archive_confirm`);
/// hovering any card opens `specs` to its right, unless a menu is open.
fn thread_card(host: &str, t: &ThreadSummary, vcs: String, elapsed: String, this: &Dirigent, cx: &mut Context<Dirigent>) -> Stateful<Div> {
    let (h, id) = (host.to_owned(), t.id.clone());
    let selected = this.open().is_some_and(|o| o.host == host && o.id == t.id);
    let hovered = this.menu.is_none() && this.hovered_card.as_ref().is_some_and(|(hh, i)| hh == host && *i == t.id);
    let unread = this.unread(host, t);
    let pinned = t.pinned;
    let title = t.name.clone().unwrap_or_else(|| "Untitled".into());
    let titling = this.titling(host, &t.id);
    let title_id = SharedString::from(format!("title/{h}/{id}"));
    let place = this.project_name(host, t);
    let marks = || {
        let server = (host != hosts::LOCAL).then(|| icon("server", theme::t().fg_2).size(px(12.)));
        server.into_iter().chain(t.model.as_deref().map(|m| icon(lab_logo(m), theme::t().fg_2).size(px(12.))))
    };
    let small = |text: String, color: u32| div().min_w_0().truncate().text_size(px(11.)).text_color(rgba(color)).child(text);
    let archive_confirm = thread::archive_confirm(host, &t.id, ArchiveAt::Sidebar, Side::Below(20.), this, cx);
    // The buttons stay (and the run time stays hidden) while the archive popup is open.
    let asking = archive_confirm.is_some();
    type Action = Box<dyn Fn(&mut Dirigent, &mut Context<Dirigent>)>;
    let button = |id: String, name: &'static str, on_click: Action| {
        div()
            .id(SharedString::from(id))
            .size(px(20.))
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
    // Run time and the buttons are stacked so swapping them on hover doesn't shift layout.
    let actions = this.snoozable(host, t).then(|| {
        let (h1, id1, h2, id2) = (h.clone(), id.clone(), h.clone(), id.clone());
        div()
            .absolute()
            .top(px(-4.))
            .right(px(-4.))
            .flex()
            .flex_row()
            .gap_0p5()
            .opacity(if asking { 1. } else { 0. })
            .group_hover("card", |d| d.opacity(1.))
            .child(button(format!("snooze/{h}/{id}"), "moon", Box::new(move |this, cx| this.set_snoozed(&h1, &id1, true, cx))))
            .child(
                button(
                    format!("archive/{h}/{id}"),
                    "archive",
                    Box::new(move |this, cx| this.set_archived(&h2, &id2, true, ArchiveAt::Sidebar, cx)),
                )
                .relative()
                .when(asking, |d| d.bg(rgba(theme::t().line)))
                .children(archive_confirm),
            )
    });
    let (h1, id1) = (h.clone(), id.clone());
    div()
        .id(SharedString::from(format!("card/{h}/{id}")))
        .group("card")
        .relative()
        .h(px(card_height(&vcs)))
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
                        small(elapsed.clone(), theme::t().fg_2)
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
                .child(div().flex_1().min_w_0().child(theme::title_text(title_id, title, titling)))
                .when(vcs.is_empty(), |d| d.children(marks().map(|m| m.ml_0p5()))),
        )
        .when(!vcs.is_empty(), |d| {
            d.child(div().flex().flex_row().items_center().gap_1p5().child(small(vcs.clone(), theme::t().fg_2).flex_1()).children(marks()))
        })
        .children(hovered.then(|| menu::popup(Side::Right, specs(host, t, vcs, elapsed, this), cx)))
}

/// The thread's place in version control: its workspace name (also its git branch / jj
/// workspace, see the service's `workspace_name`), else the branch (git) or change id (jj)
/// of its directory as last probed (`Dirigent::request_sidebar_vcs`); "" until known.
fn vcs_label(host: &str, t: &ThreadSummary, this: &Dirigent) -> String {
    match &t.workspace {
        Some(w) => projects::basename(w).to_owned(),
        None => this.vcs_of(host, &t.cwd).and_then(|v| v.branch.clone()).unwrap_or_default(),
    }
}

/// Icon of the lab behind a `provider/model` key: by provider, else by model family (a
/// gateway like openrouter serves both), else a generic spark.
fn lab_logo(model: &str) -> &'static str {
    let (provider, name) = model.split_once('/').unwrap_or(("", model));
    if provider == "anthropic" || name.contains("claude") {
        "claude"
    } else if provider == "openai" || name.starts_with("gpt") || name.starts_with("o1") || name.starts_with("o3") || name.starts_with("o4")
    {
        "openai"
    } else {
        "sparkles"
    }
}

/// The hovered card's popup: the title, then one line each for what the card abbreviates:
/// `project, host` / model (effort) / workspace name or branch / run time.
fn specs(host: &str, t: &ThreadSummary, vcs: String, elapsed: String, this: &Dirigent) -> Vec<AnyElement> {
    let model = match (&t.model, &t.thinking_level) {
        (Some(m), Some(e)) => format!("{m} ({e})"),
        (Some(m), None) => m.clone(),
        (None, _) => String::new(),
    };
    let run = match (elapsed.is_empty(), t.running()) {
        (true, _) => String::new(),
        (false, true) => format!("running for {elapsed}"),
        (false, false) => format!("last run {elapsed}"),
    };
    let line = |value: String| div().px_2().py_0p5().max_w(px(360.)).truncate().text_size(px(12.)).child(value).into_any();
    let mut rows = vec![
        div()
            .px_2()
            .py_0p5()
            .max_w(px(360.))
            .truncate()
            .font_weight(FontWeight::MEDIUM)
            .child(t.name.clone().unwrap_or_else(|| "Untitled".into()))
            .into_any(),
        line(format!("{}, {host}", this.project_name(host, t))),
    ];
    rows.extend([model, vcs, run].into_iter().filter(|s| !s.is_empty()).map(line));
    rows
}

/// Trailing count on a `row`; its margin is the row's right inset, so it ends where the
/// cards' text does.
fn count(n: usize, color: u32) -> Div {
    div().mr_2().text_size(px(11.)).text_color(rgba(color)).child(if n > 0 { n.to_string() } else { String::new() })
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

/// A pinned project: its name, and on hover a `+` (flush right, under the title's) that
/// opens a draft for it. Grab it to reorder: a `Ghost` follows the cursor and the row
/// itself is a gap (`dragged`). Pinning, linking and removing are on the Projects page.
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
    // The ghost is as wide as the row: the sidebar minus its border and the list's padding.
    let (name, width, font) = (g.name.clone(), this.sidebar_width - 1. - 16., this.fonts.ui.clone());
    let entity = cx.weak_entity();
    div()
        .px_2()
        .when(dragged, |d| d.opacity(0.))
        .child(
            row(SharedString::from(format!("p/{key}")), selected, cx, move |this, cx| {
                this.show(Pane::Project { group_key: k.clone() }, cx)
            })
            .on_drag(DragProject(key.clone()), move |DragProject(key), _, _, cx| {
                let key = key.clone();
                let _ = entity.update(cx, |this, cx| {
                    this.dragging = Some(key);
                    cx.notify();
                });
                cx.new(|_| Ghost { name: name.clone(), width, font: font.clone() })
            })
            .child(div().flex_1().min_w_0().truncate().text_color(rgba(fg)).child(g.name.clone()))
            .child(new_thread),
        )
        .into_any()
}
