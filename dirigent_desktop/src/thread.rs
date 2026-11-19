use crate::app::{ArchiveAt, AskForm, Blob, Dirigent, Menu, OpenThread, PANE_MIN, Pane, PanelPart, Resize, Target};
use crate::icons::icon;
use crate::lists::{Via, breadcrumb, header};
use crate::markdown;
use crate::menu::{self, Side};
use crate::picker;
use crate::ruler;
use crate::select::{Doc, Keys};
use crate::theme::{self, Fonts, Status, dollars, kilo, text, u};
use crate::titlebar;
use crate::view::{Item, Stats, Step, ToolCall, Work, split_image_refs};
use crate::visual::{Picture, Visuals};
use crate::working;
use dirigent_protocol::transcript::{Body, Stop};
use dirigent_protocol::visuals::WIDTH;
use dirigent_protocol::{PromptMode, Setup, ThreadSummary};
use dirigent_service::vcs::default_base;
use gpui::{
    Anchor, Animation, AnimationExt, AnyElement, Bounds, ClipboardItem, Context, Div, ElementId, Focusable, FontWeight, MouseButton,
    ObjectFit, Pixels, SharedString, Stateful, Window, anchored, canvas, deferred, div, img, list, point, prelude::*, px, relative, rgba,
};
use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

pub const TAIL: usize = usize::MAX;
const MAX_RESULT_CHARS: usize = 20_000;
const ROW_H: f32 = 22.;
pub const LOG_WIDTH: f32 = 400.;

pub fn render(this: &mut Dirigent, window: &Window, cx: &mut Context<Dirigent>) -> Div {
    let pane = div().flex_1().min_w_0().h_full().flex().flex_col();
    if let Pane::Thread(t) = &mut this.pane
        && t.list.is_following_tail()
    {
        t.new_output = false;
    }
    let Pane::Thread(t) = &this.pane else {
        return match &this.pane {
            Pane::Draft { target } => draft_page(pane, target.clone(), this, window, cx),
            _ => pane,
        };
    };
    let list_state = t.list.clone();
    let (host, id) = (t.host.clone(), t.id.clone());
    let ask = t.asks.first().map(|form| ask_card(form, t.asks.len() - 1, cx));
    let jump = jump_button(t, cx);
    let summary = this.summary(&host, &id).cloned();
    let missing = summary.as_ref().is_some_and(|s| s.workspace_missing);
    let banner = summary
        .as_ref()
        .filter(|s| s.workspace_missing)
        .and_then(|s| s.workspace.as_deref())
        .map(|w| missing_banner(&host, &id, w, this.fonts.mono.clone(), cx));
    let title = summary.as_ref().and_then(|s| s.name.clone()).unwrap_or_else(|| "Untitled".into());
    let status = summary.as_ref().map(|s| this.status(&host, s)).unwrap_or(Status::Done);
    let archived = summary.as_ref().is_some_and(|s| s.archived);
    let project = summary.as_ref().and_then(|s| {
        let pid = s.project_id.as_deref()?;
        Some((this.group_key_of(&host, pid)?, this.project_name(&host, s)))
    });

    let (h1, i1) = (host.clone(), id.clone());
    let (h3, i3) = (host.clone(), id.clone());
    let (h4, i4) = (host.clone(), id.clone());
    let (thread_id, copy_btn) = (id.clone(), ElementId::from("hdr-copy-id"));
    let id_copied = t.copied.as_ref() == Some(&copy_btn);
    let snoozed = status == Status::Snoozed;
    let pinned = this.pinned(&host, &id);
    let panel_open = this.panel_open();
    let workspace = summary.as_ref().and_then(|s| s.workspace.clone());
    let confirm_open = this.menu == Some(Menu::ConfirmDelete);
    let confirm = confirm_open.then(|| {
        let (h, i) = (host.clone(), id.clone());
        let note = if workspace.is_some() {
            "This removes the thread, its history and its workspace."
        } else {
            "This removes the thread and its history."
        };
        menu::popup(
            Side::BelowRight { trigger_h: 26. },
            vec![menu::header(note), menu::danger("del", "Delete thread", move |this, cx| this.delete_thread(&h, &i, cx), cx)],
            cx,
        )
    });
    let archive_confirm = archive_confirm(&host, &id, ArchiveAt::Header, Side::BelowRight { trigger_h: 26. }, this, cx);
    let archive_open = archive_confirm.is_some();
    let fork_source = summary.as_ref().and_then(|s| s.forked_from.clone()).and_then(|from| {
        let name = this.summary(&host, &from.thread_id)?.name.clone().unwrap_or_else(|| "Untitled".into());
        let host = host.clone();
        Some(
            theme::icon_button("hdr-fork", "git-fork", cx, move |this, cx| this.open_fork_source(&host, &from, cx))
                .tooltip(theme::tooltip(format!("Forked from {name}"))),
        )
    });
    let actions = div()
        .flex()
        .flex_row()
        .items_center()
        .gap_1()
        .font_weight(FontWeight::NORMAL)
        .children(fork_source)
        .child(
            theme::icon_button(copy_btn.clone(), if id_copied { "check" } else { "copy" }, cx, move |this, cx| {
                this.copy_item(copy_btn.clone(), thread_id.clone(), cx)
            })
            .tooltip(theme::tooltip("Copy thread id")),
        )
        .when(summary.as_ref().is_some_and(|s| this.snoozable(&host, s)), |d| {
            d.child(theme::tinted_icon_button(
                "hdr-snooze",
                "moon",
                if snoozed { theme::t().warn } else { theme::t().fg_1 },
                cx,
                move |this, cx| this.set_snoozed(&h3, &i3, !snoozed, cx),
            ))
        })
        .child(theme::tinted_icon_button("hdr-pin", "pin", if pinned { theme::t().warn } else { theme::t().fg_1 }, cx, move |this, cx| {
            this.set_pinned(&h4, &i4, !pinned, cx)
        }))
        .when(archived || summary.as_ref().is_some_and(|s| this.at_rest(&host, s)), |d| {
            d.child(
                theme::icon_button("hdr-archive", if archived { "archive-restore" } else { "archive" }, cx, move |this, cx| {
                    this.set_archived(&h1, &i1, !archived, ArchiveAt::Header, cx)
                })
                .when(archive_open, |d| d.bg(rgba(theme::t().hover)))
                .children(archive_confirm),
            )
        })
        .child(
            theme::icon_button("hdr-delete", "trash-2", cx, |this, cx| this.toggle_menu(Menu::ConfirmDelete, cx))
                .when(confirm_open, |d| d.bg(rgba(theme::t().hover)))
                .children(confirm),
        )
        .child(
            theme::icon_button("hdr-panel", if panel_open { "panel-right-close" } else { "panel-right-open" }, cx, |this, cx| {
                this.toggle_panel(cx)
            })
            .ml_2(),
        )
        .into_any_element();

    let column = div()
        .relative()
        .flex_1()
        .min_w_0()
        .h_full()
        .flex()
        .flex_col()
        .child(list(list_state, cx.processor(|this, ix, _window, cx| this.render_with_visuals(ix, cx))).flex_1().min_h_0().w_full())
        .child(
            div()
                .relative()
                .w_full()
                .flex()
                .flex_col()
                .items_center()
                .px_6()
                .pb_5()
                .pt_2()
                .child(ask_layer(ask, cx))
                .children(jump)
                .children(banner)
                .child(composer(this, window, cx)),
        )
        .child(ruler::render(this, cx));
    let titling = this.titling(&host, &id);
    let branch = workspace.as_deref().map(|w| {
        let name = workspace_name(w);
        div()
            .flex()
            .flex_row()
            .items_center()
            .gap_1()
            .ml_2()
            .flex_shrink_0()
            .font_weight(FontWeight::NORMAL)
            .text_size(text::sm())
            .font_family(this.fonts.mono.clone())
            .text_color(rgba(if missing { theme::t().warn } else { theme::t().fg_1 }))
            .child(icon("git-branch", if missing { theme::t().warn } else { theme::t().fg_2 }).size(u(12.)))
            .child(name)
    });
    let parent = summary.as_ref().and_then(|s| s.parent_id.clone()).and_then(|pid| this.summary(&host, &pid)).map(|p| Via {
        host: host.clone(),
        id: p.id.clone(),
        name: p.name.clone().unwrap_or_else(|| "Untitled".into()).into(),
        status: this.status(&host, p),
    });
    let crumb = div()
        .flex()
        .flex_row()
        .items_center()
        .min_w_0()
        .child(breadcrumb(project, parent, Some(status), title, pinned, titling, cx))
        .children(branch);
    let (actions, panel) = if panel_open { (None, Some(side_panel(this, actions, cx))) } else { (Some(actions), None) };
    let main = div()
        .flex_1()
        .min_w(u(PANE_MIN))
        .h_full()
        .flex()
        .flex_col()
        .child(header(crumb, actions).when(panel_open, |d| d.pr_4()))
        .child(div().relative().flex_1().min_h_0().flex().flex_row().child(column));
    pane.child(div().flex_1().min_h_0().flex().flex_row().child(main).children(panel))
}

fn ask_layer(ask: Option<Div>, cx: &mut Context<Dirigent>) -> Div {
    div()
        .absolute()
        .top_0()
        .left_0()
        .right_0()
        .h_0()
        .px_6()
        .flex()
        .flex_col()
        .items_center()
        .justify_end()
        .on_children_prepainted(cx.processor(|this, cards: Vec<Bounds<Pixels>>, window, cx| {
            let top = cards.iter().map(|b| b.top()).min();
            let bottom = cards.iter().map(|b| b.bottom()).max();
            let h = top.zip(bottom).map_or(px(0.), |(top, bottom)| bottom - top);
            this.set_ask_height(h, window, cx);
        }))
        .children(ask)
}

fn jump_button(t: &OpenThread, cx: &mut Context<Dirigent>) -> Option<Div> {
    if t.list.is_following_tail() {
        return None;
    }
    let th = theme::t();
    let list = t.list.clone();
    let rerender_when_following_tail = canvas(
        move |_, window, _| {
            if list.is_following_tail() {
                window.request_animation_frame();
            }
        },
        |_, _, _, _| {},
    );
    let (label, color) = if t.new_output { ("New output", th.accent_ink) } else { ("Latest", th.fg_1) };
    let button = div()
        .id("jump-latest")
        .relative()
        .h(u(22.))
        .flex_shrink_0()
        .flex()
        .flex_row()
        .items_center()
        .gap_1()
        .pl_1p5()
        .pr_2()
        .rounded_full()
        .cursor_pointer()
        .bg(rgba(th.raised))
        .border_1()
        .border_color(rgba(th.line))
        .shadow_md()
        .text_size(text::xs())
        .text_color(rgba(color))
        .hover(|d| d.bg(rgba(th.hover)))
        .block_mouse_except_scroll()
        .on_click(cx.listener(|this, _, _, cx| this.jump_to_end(cx)))
        .child(icon("arrow-down", color).size(u(12.)))
        .child(label)
        .child(rerender_when_following_tail.absolute().size_0())
        .with_animation("jump-latest-in", Animation::new(Duration::from_millis(150)), |d, v| d.opacity(v));
    let lift = if t.ask_h > px(0.) { t.ask_h + u(8.) } else { u(8.) };
    Some(div().absolute().top(-lift).left_0().right_0().h_0().flex().flex_col().items_center().justify_end().child(button))
}

fn side_panel(this: &Dirigent, actions: AnyElement, cx: &mut Context<Dirigent>) -> Div {
    let lineage = this.panel_shows(PanelPart::Lineage);
    let tree = if lineage { crate::orchestra::tree(this, cx) } else { None };
    let log = log_body(this, cx);
    let empty = (tree.is_none() && log.is_none()).then(|| {
        let what = if lineage { "Sub-agents this thread spawns show up here." } else { "Tool calls and thinking show up here." };
        div()
            .flex_1()
            .flex()
            .items_center()
            .justify_center()
            .px_6()
            .text_size(text::sm())
            .text_color(rgba(theme::t().fg_2))
            .child(div().w_full().text_center().child(what))
    });
    let tree = tree.map(|tree| {
        div()
            .id("orchestra")
            .overflow_y_scroll()
            .px_2()
            .pb_2()
            .when(log.is_some(), |d| d.flex_shrink_0().max_h(relative(0.5)).border_b_1().border_color(rgba(theme::t().line_soft)))
            .when(log.is_none(), |d| d.flex_1().min_h_0())
            .child(tree)
    });
    let toggle = |id: &'static str, part: PanelPart, glyph: &'static str, tip: &'static str| {
        let on = this.panel_shows(part);
        let color = if on { theme::t().fg } else { theme::t().fg_2 };
        theme::tinted_icon_button(id, glyph, color, cx, move |this, cx| this.toggle_panel_part(part, cx))
            .when(on, |d| d.bg(rgba(theme::t().hover)))
            .tooltip(theme::tooltip(tip))
    };
    let toggles = div()
        .flex()
        .flex_row()
        .items_center()
        .gap_1()
        .min_w_0()
        .overflow_hidden()
        .child(toggle("panel-lineage", PanelPart::Lineage, "network", "Agent lineage"))
        .child(toggle("panel-log", PanelPart::Log, "logs", "Tool calls and thinking"));
    let handle = div()
        .id("panel-resize")
        .absolute()
        .top(u(titlebar::HEIGHT))
        .bottom_0()
        .left(u(-3.))
        .w(u(6.))
        .cursor_col_resize()
        .hover(|d| d.bg(rgba(theme::t().line)))
        .on_mouse_down(MouseButton::Left, cx.listener(|this, _, _, cx| this.start_resize(Resize::Log, cx)));
    div()
        .relative()
        .w(u(this.log_width))
        .min_w_0()
        .h_full()
        .border_l_1()
        .border_color(rgba(theme::t().line_soft))
        .flex()
        .flex_col()
        .child(header(toggles, Some(actions)).justify_end())
        .children(crate::screen::section(this, cx))
        .children(tree)
        .children(log)
        .children(empty)
        .child(handle)
}

pub fn archive_confirm(host: &str, id: &str, at: ArchiveAt, side: Side, this: &Dirigent, cx: &mut Context<Dirigent>) -> Option<AnyElement> {
    if this.menu.as_ref() != Some(&Menu::ConfirmArchive { host: host.to_owned(), id: id.to_owned(), at }) {
        return None;
    }
    let workspace = this.summary(host, id)?.workspace.clone();
    let dirty = workspace.as_deref().and_then(|w| this.vcs_of(host, w)).is_some_and(|v| v.dirty);
    let note = if dirty { "The workspace has uncommitted changes." } else { "The workspace is clean." };
    let (h, i) = (host.to_owned(), id.to_owned());
    let (h2, i2) = (host.to_owned(), id.to_owned());
    Some(menu::popup(
        side,
        vec![
            menu::header(note),
            menu::danger("arch-rm", "Archive and remove workspace", move |this, cx| this.archive_with_workspace(&h, &i, false, cx), cx),
            menu::item(
                "arch-keep",
                "Archive, keep workspace on disk",
                "",
                false,
                move |this, cx| this.archive_with_workspace(&h2, &i2, true, cx),
                cx,
            ),
        ],
        cx,
    ))
}

fn workspace_name(dir: &str) -> String {
    crate::projects::basename(dir).to_owned()
}

fn missing_banner(host: &str, id: &str, workspace: &str, mono: SharedString, cx: &mut Context<Dirigent>) -> Div {
    let t = theme::t();
    let (h1, i1) = (host.to_owned(), id.to_owned());
    let (h2, i2) = (host.to_owned(), id.to_owned());
    div()
        .w_full()
        .max_w(u(theme::COMPOSER))
        .mb_2()
        .px_3()
        .py_2()
        .flex()
        .flex_row()
        .items_center()
        .gap_2()
        .rounded_md()
        .bg(rgba(t.raised))
        .border_1()
        .border_color(rgba(t.warn))
        .text_size(text::sm())
        .child(icon("triangle-alert", t.warn))
        .child(
            div()
                .flex_1()
                .min_w_0()
                .flex()
                .flex_col()
                .child(div().text_color(rgba(t.fg)).child("The thread's workspace was removed outside dirigent."))
                .child(div().text_size(text::xs()).text_color(rgba(t.fg_1)).font_family(mono).truncate().child(workspace.to_owned())),
        )
        .child(
            theme::button("ws-recreate", "git-branch", "Recreate")
                .on_click(cx.listener(move |this, _, _, cx| this.move_to_workspace(&h1, &i1, None, cx))),
        )
        .child(
            theme::button("ws-back", "archive-restore", "Back to project")
                .on_click(cx.listener(move |this, _, _, cx| this.remove_workspace(&h2, &i2, false, cx))),
        )
}

const TARGET_LIST_H: f32 = 270.;

const TARGET_ROW_CHARS: usize = 44;

fn draft_page(pane: Div, target: Option<Target>, this: &Dirigent, window: &Window, cx: &mut Context<Dirigent>) -> Div {
    let (groups, _) = this.groups();
    let group = target.as_ref().and_then(|t| groups.iter().find(|g| g.has(&t.host, &t.project_id)));
    let project = group.map(|g| g.name.clone());
    let target_open = this.menu == Some(Menu::Target);
    let target_menu = target_open.then(|| {
        if groups.is_empty() {
            return menu::popup(
                Side::Below { trigger_h: 26. },
                vec![menu::header("No projects yet. Add one with the folder in the sidebar.")],
                cx,
            );
        }
        let search = menu::search(this.target_search.clone());
        let multi_host = this.hosts.len() > 1;
        let list = menu::results("target-list", TARGET_LIST_H, cx, |cx| {
            let mut items = Vec::new();
            for g in &this.filtered_targets(cx) {
                let selected = group.is_some_and(|s| s.key == g.key);
                let t: Target = g.member_for(target.as_ref().map(|t| t.host.as_str())).into();
                let hosts = multi_host.then(|| {
                    let names = g.members.iter().map(|m| m.host.as_str()).collect::<Vec<_>>().join(", ");
                    let fits = g.name.chars().count() + names.chars().count() <= TARGET_ROW_CHARS;
                    if fits { names } else { format!("{} hosts", g.members.len()) }
                });
                let row = menu::stop(menu::row(format!("t/{}", g.key)), selected, move |this, _, cx| this.set_target(t.clone(), cx), cx)
                    .child(div().flex_shrink_0().child(g.name.clone()))
                    .children(
                        hosts
                            .map(|h| div().ml_auto().min_w_0().truncate().text_color(rgba(theme::t().fg_1)).text_size(text::sm()).child(h)),
                    );
                items.push(row.into_any());
            }
            if items.is_empty() {
                items.push(menu::header("No projects match"));
            }
            items
        });
        let body = div().w(u(360.)).flex().flex_col().child(search).child(menu::separator()).child(list).into_any();
        menu::popup(Side::Below { trigger_h: 26. }, vec![body], cx)
    });
    let host_chip = target.as_ref().filter(|_| this.hosts.len() > 1).map(|t| {
        let members = group.map(|g| g.members.as_slice()).unwrap_or_default();
        let host_open = this.menu == Some(Menu::TargetHost);
        let host_menu = host_open.then(|| {
            let items = members
                .iter()
                .map(|m| {
                    let mt: Target = m.into();
                    let selected = *t == mt;
                    menu::item(
                        format!("h/{}", m.host),
                        this.host_label(&m.host),
                        m.project.path.clone(),
                        selected,
                        move |this, cx| this.set_target(mt.clone(), cx),
                        cx,
                    )
                })
                .collect();
            menu::popup(Side::Below { trigger_h: 26. }, items, cx)
        });
        if members.len() > 1 {
            theme::labeled_chip("host-chip", "on", this.host_label(&t.host), false, host_open)
                .on_click(cx.listener(|this, _, _, cx| this.toggle_menu(Menu::TargetHost, cx)))
                .children(host_menu)
                .into_any_element()
        } else {
            div()
                .flex()
                .flex_row()
                .items_center()
                .gap_1()
                .h(u(26.))
                .px_2()
                .rounded_md()
                .bg(rgba(theme::t().raised))
                .border_1()
                .border_color(rgba(theme::t().line))
                .text_size(text::sm())
                .child(div().text_color(rgba(theme::t().fg_1)).child("on"))
                .child(div().font_weight(FontWeight::MEDIUM).child(this.host_label(&t.host)))
                .into_any_element()
        }
    });
    let where_row = div()
        .flex()
        .flex_row()
        .items_center()
        .gap_2()
        .child(
            theme::labeled_chip(
                "target-chip",
                "Create thread in",
                project.clone().unwrap_or_else(|| "a project".into()),
                project.is_none(),
                target_open,
            )
            .on_click(cx.listener(|this, _, window, cx| this.toggle_target_picker(window, cx)))
            .children(target_menu),
        )
        .children(host_chip);

    let card = div()
        .relative()
        .w_full()
        .max_w(u(theme::COMPOSER))
        .flex()
        .flex_col()
        .gap_3()
        .child(where_row)
        .child(composer(this, window, cx))
        .child(div().absolute().bottom_0().left_0().right_0().h_0().flex().flex_col().items_center().children(draft_warnings(this, cx)));
    pane.child(div().flex_1().flex().flex_col().items_center().justify_center().px_6().pb(u(40.)).child(card))
}

fn draft_warnings(this: &Dirigent, cx: &mut Context<Dirigent>) -> Option<Div> {
    let (target, path, info) = this.draft_vcs()?;
    let t = theme::t();
    let mut lines: Vec<(String, u32)> = vec![];
    let working = this.working_in(&target.host, path);
    if let Some(w) = working.first() {
        let name = w.name.clone().unwrap_or_else(|| "Another thread".into());
        let text = match working.len() {
            1 => format!("{name} is working in this checkout right now"),
            n => format!("{n} threads are working in this checkout right now"),
        };
        lines.push((text, t.warn));
    }
    if info.is_some_and(|v| v.dirty) {
        lines.push(("The working copy has uncommitted changes".into(), t.fg_1));
    }
    if lines.is_empty() || this.isolate || info.is_none_or(|v| v.vcs.is_none()) {
        return None;
    }
    Some(div().mt_3().flex().flex_col().items_center().gap_1().text_size(text::sm()).children(lines.into_iter().enumerate().map(
        |(i, (text, color))| {
            div()
                .flex()
                .flex_row()
                .items_center()
                .justify_center()
                .gap_1p5()
                .child(icon("triangle-alert", color).size(u(12.)))
                .child(div().text_color(rgba(color)).child(text))
                .child(
                    div()
                        .id(ElementId::Name(format!("isolate-hint/{i}").into()))
                        .ml_1()
                        .cursor_pointer()
                        .text_color(rgba(t.accent_ink))
                        .hover(|d| d.underline())
                        .on_click(cx.listener(|this, _, _, cx| this.set_isolate(true, cx)))
                        .child("isolate it"),
                )
        },
    )))
}

fn workspace_chip(this: &Dirigent, cx: &mut Context<Dirigent>) -> Option<AnyElement> {
    let t = theme::t();
    let picker_open = this.menu == Some(Menu::Base);
    let open_picker = |this: &mut Dirigent, window: &mut Window, cx: &mut Context<Dirigent>| this.toggle_base_picker(window, cx);
    match &this.pane {
        Pane::Draft { .. } => {
            let (_, _, info) = this.draft_vcs()?;
            let vcs = info?.vcs?;
            let label = match this.isolate {
                true => format!("workspace from {}", this.base.as_deref().unwrap_or(default_base(vcs))),
                false => "in project".to_owned(),
            };
            Some(
                theme::chip("ws-chip", label, false, picker_open)
                    .on_click(cx.listener(move |this, _, window, cx| open_picker(this, window, cx)))
                    .children(picker_open.then(|| base_picker(this, vcs, cx)))
                    .into_any_element(),
            )
        }
        Pane::Thread(open) => {
            let s = this.summary(&open.host, &open.id)?;
            let repo = this.project_path(&open.host, s)?;
            let info = this.vcs_of(&open.host, &repo)?;
            let vcs = info.vcs?;
            if s.workspace_missing {
                return None;
            }
            let running = s.running();
            let (host, id) = (open.host.clone(), open.id.clone());
            Some(match &s.workspace {
                None if s.setup == Some(Setup::Workspace) => {
                    theme::chip_in("ws-chip", dirigent_protocol::workspace_name(&id), t.accent_ink, theme::alpha(t.fg_2, 0x50), false)
                        .tooltip(theme::tooltip("Creating the workspace…"))
                        .into_any_element()
                }
                None if running || info.dirty => {
                    let why = match running {
                        true => "The thread is working. Stop it to move it into a workspace.",
                        false => "The working copy has uncommitted changes. Commit them to move the thread into a workspace.",
                    };
                    theme::chip_in("ws-chip", "in project", t.fg_1, theme::alpha(t.fg_2, 0x50), false)
                        .tooltip(theme::tooltip(why))
                        .into_any_element()
                }
                None => theme::chip("ws-chip", "in project", false, picker_open)
                    .on_click(cx.listener(move |this, _, window, cx| open_picker(this, window, cx)))
                    .children(picker_open.then(|| base_picker(this, vcs, cx)))
                    .into_any_element(),
                Some(w) => {
                    let kind = match vcs {
                        dirigent_protocol::Vcs::Git => "worktree",
                        dirigent_protocol::Vcs::Jj => "workspace",
                    };
                    let open_menu = this.menu == Some(Menu::Workspace);
                    let menu = open_menu.then(|| {
                        let dirty = this.vcs_of(&host, w).is_some_and(|v| v.dirty);
                        let (h, i) = (host.clone(), id.clone());
                        let (h2, i2) = (host.clone(), id.clone());
                        let path = w.clone();
                        let copy_path = move |this: &mut Dirigent, cx: &mut Context<Dirigent>| {
                            cx.write_to_clipboard(ClipboardItem::new_string(path.clone()));
                            this.menu = None;
                            this.toast("Path copied", cx);
                        };
                        let remove = format!("Back to project, remove the {kind}");
                        let detach = "Back to project, keep it on disk";
                        let header = match (running, dirty) {
                            (true, _) => "The thread is working. Stop it to leave the workspace.",
                            (false, true) => "The workspace has uncommitted changes.",
                            (false, false) => "The workspace is clean.",
                        };
                        let mut items =
                            vec![menu::header(header), menu::item("ws-copy", "Copy path", "", false, copy_path, cx), menu::separator()];
                        match running {
                            true => items.extend([menu::disabled("ws-rm", remove), menu::disabled("ws-detach", detach)]),
                            false => items.extend([
                                menu::danger("ws-rm", remove, move |this, cx| this.remove_workspace(&h, &i, false, cx), cx),
                                menu::item("ws-detach", detach, "", false, move |this, cx| this.remove_workspace(&h2, &i2, true, cx), cx),
                            ]),
                        }
                        menu::popup(Side::Above, items, cx)
                    });
                    theme::chip_in("ws-chip", workspace_name(w), t.accent_ink, t.fg_2, open_menu)
                        .on_click(cx.listener(|this, _, _, cx| this.toggle_menu(Menu::Workspace, cx)))
                        .children(menu)
                        .into_any_element()
                }
            })
        }
        _ => None,
    }
}

const BASE_LIST_H: f32 = 160.;

fn base_picker(this: &Dirigent, vcs: dirigent_protocol::Vcs, cx: &mut Context<Dirigent>) -> AnyElement {
    let search = menu::search(this.base_search.clone());
    let typed = this.base_search.read(cx).text().trim().to_owned();
    let refs = this.filtered_refs(cx);
    let drafting = matches!(this.pane, Pane::Draft { .. });
    let isolated = drafting && this.isolate;
    let current = this.base.clone().filter(|_| isolated);
    let in_project = menu::item(
        "base/none",
        "in project",
        "no workspace",
        !isolated,
        move |this, cx| {
            if drafting {
                this.set_isolate(false, cx);
            }
            this.close_menu(cx);
        },
        cx,
    );
    let list = menu::results("base-list", BASE_LIST_H, cx, |cx| {
        let mut items = vec![];
        if typed.is_empty() {
            let d = default_base(vcs);
            items.push(menu::item("base/default", d, "default", isolated && current.is_none(), |this, cx| this.pick_base(None, cx), cx));
        }
        for r in &refs {
            let base = r.clone();
            items.push(menu::item(
                format!("base/{r}"),
                r.clone(),
                "",
                current.as_deref() == Some(r),
                move |this, cx| this.pick_base(Some(base.clone()), cx),
                cx,
            ));
        }
        if !typed.is_empty() && !refs.contains(&typed) {
            let base = typed.clone();
            items.push(menu::item("base/typed", typed, "as typed", false, move |this, cx| this.pick_base(Some(base.clone()), cx), cx));
        }
        items
    });
    let body = div()
        .w(u(360.))
        .flex()
        .flex_col()
        .child(in_project)
        .child(menu::separator())
        .child(menu::header("New workspace from"))
        .child(search)
        .child(menu::separator())
        .child(list)
        .into_any();
    menu::popup(Side::Above, vec![body], cx)
}

fn model_chips(this: &Dirigent, below: bool, cx: &mut Context<Dirigent>) -> (Stateful<Div>, Stateful<Div>) {
    let side = || if below { Side::Below { trigger_h: 24. } } else { Side::Above };
    let no_target = matches!(this.pane, Pane::Draft { target: None });
    let model_label = this.model.as_deref().map(|m| theme::short_model(m).to_owned()).unwrap_or_else(|| "Model".into());
    let model_open = matches!(this.menu, Some(Menu::Model | Menu::AllModels));
    let model_menu = model_open.then(|| picker::menu(this, no_target, side(), cx));
    let effort_label = this.effort.clone().unwrap_or_else(|| "Effort".into());
    let effort_open = this.menu == Some(Menu::Effort);
    let effort_menu = effort_open.then(|| {
        let levels = this.current_model().map(|m| m.efforts.clone()).unwrap_or_default();
        let mut items = vec![];
        for l in levels {
            let selected = this.effort.as_deref() == Some(&l);
            let level = l.clone();
            items.push(menu::item(format!("e/{l}"), l, "", selected, move |this, cx| this.set_effort(level.clone(), cx), cx));
        }
        if items.is_empty() {
            items.push(menu::header("Pick a model first"));
        }
        menu::popup(side(), items, cx)
    });
    let model = theme::chip("model-chip", model_label, this.model.is_none(), model_open)
        .on_click(cx.listener(|this, _, _, cx| this.toggle_menu(Menu::Model, cx)))
        .children(model_menu);
    let effort = theme::chip("effort-chip", effort_label, this.effort.is_none(), effort_open)
        .flex_shrink_0()
        .on_click(cx.listener(|this, _, _, cx| this.toggle_menu(Menu::Effort, cx)))
        .children(effort_menu);
    (model, effort)
}

fn send_button(id: &'static str, ready: bool, cx: &mut Context<Dirigent>) -> Stateful<Div> {
    div()
        .id(id)
        .size(u(28.))
        .flex()
        .items_center()
        .justify_center()
        .rounded_full()
        .cursor_pointer()
        .bg(rgba(if ready { theme::t().accent } else { theme::t().hover }))
        .hover(|d| d.bg(rgba(if ready { theme::t().accent_active } else { theme::t().line })))
        .on_click(cx.listener(|this, _, window, cx| {
            this.composer.update(cx, |_, cx| cx.emit(crate::input::Submitted::CtrlEnter));
            window.focus(&this.composer.focus_handle(cx), cx);
        }))
        .child(icon("arrow-up", if ready { 0x9fb6e8ff } else { theme::t().fg_2 }))
}

fn offline_note(this: &Dirigent) -> Option<Div> {
    this.composer_offline()
        .then(|| div().flex_shrink_0().px_1().text_size(text::sm()).text_color(rgba(theme::t().fg_2)).child("host offline"))
}

fn editing_note() -> Div {
    div()
        .w_full()
        .max_w(u(theme::COMPOSER))
        .h(u(28.))
        .flex()
        .items_center()
        .justify_center()
        .text_size(text::sm())
        .text_color(rgba(theme::t().fg_2))
        .child("Editing a message above \u{b7} Esc to cancel")
}

fn composer(this: &Dirigent, window: &Window, cx: &mut Context<Dirigent>) -> Div {
    if this.open().is_some_and(|t| t.editing.is_some()) {
        return editing_note();
    }
    let running = this.is_running();
    let has_text = !this.composer.read(cx).text().trim().is_empty();
    let drafting = matches!(this.pane, Pane::Draft { .. });
    let no_target = matches!(this.pane, Pane::Draft { target: None });
    let (model_chip, effort_chip) = model_chips(this, false, cx);

    let usage = (!drafting).then(|| this.open().and_then(|t| context_usage(t).map(|u| (t, u)))).flatten().map(|(t, used)| {
        let window = this.current_model().map(|m| m.context_window).unwrap_or(0);
        let label = if window > 0 { format!("{}/{}", kilo(used), kilo(window)) } else { kilo(used) };
        div()
            .id("ctx-usage")
            .relative()
            .h(u(24.))
            .px_1p5()
            .flex()
            .items_center()
            .rounded_md()
            .text_color(rgba(theme::t().fg_1))
            .hover(|d| d.bg(rgba(theme::t().hover)).text_color(rgba(theme::t().fg)))
            .on_hover(cx.listener(|this, hovered, _, cx| {
                if let Pane::Thread(t) = &mut this.pane {
                    t.stats_hover = *hovered;
                    cx.notify();
                }
            }))
            .child(label)
            .children(t.stats_hover.then(|| stats_card(t)))
    });

    let blocked = this.open().is_some_and(|t| this.not_ready(&t.host, &t.id).is_some()) || this.composer_offline();
    let action = if running && !blocked {
        div()
            .id("stop")
            .size(u(28.))
            .flex()
            .items_center()
            .justify_center()
            .rounded_full()
            .cursor_pointer()
            .bg(rgba(theme::t().danger))
            .hover(|d| d.bg(rgba(theme::t().danger_active)))
            .on_click(cx.listener(|this, _, _, cx| this.stop(cx)))
            .child(icon("square", theme::t().danger_ink).size(u(11.)))
    } else {
        send_button("send", has_text && this.model.is_some() && !no_target && !blocked, cx)
    };

    div()
        .relative()
        .w_full()
        .max_w(u(theme::COMPOSER))
        .flex()
        .flex_col()
        .rounded(u(CORNER))
        .bg(rgba(theme::t().raised))
        .border_1()
        .border_color(gpui::transparent_black())
        .shadow_md()
        .when(this.composer.focus_handle(cx).is_focused(window), |d| d.children(glow()))
        .child(
            div()
                .px_4()
                .pt_4()
                .pb_3()
                .text_size(text::lg())
                .line_height(u(22.))
                .on_mouse_down(MouseButton::Left, cx.listener(|this, _, _, cx| this.focus_composer(cx)))
                .child(this.composer.clone()),
        )
        .children(attachment_strip(this, cx))
        .child(
            div()
                .flex()
                .flex_row()
                .items_center()
                .gap_1()
                .px_2()
                .pb_2()
                .child(
                    div()
                        .flex_1()
                        .min_w_0()
                        .h(u(24.))
                        .overflow_hidden()
                        .flex()
                        .flex_row()
                        .flex_wrap()
                        .items_center()
                        .gap_1()
                        .child(
                            div()
                                .min_w_0()
                                .flex()
                                .flex_row()
                                .items_center()
                                .gap_1()
                                .children(workspace_chip(this, cx))
                                .child(model_chip)
                                .child(effort_chip),
                        )
                        .children(usage),
                )
                .children(offline_note(this))
                .child(action.flex_shrink_0()),
        )
}

fn ask_card(form: &AskForm, waiting: usize, cx: &mut Context<Dirigent>) -> Div {
    let t = theme::t();
    let ask_id = form.ask.id.clone();
    let (qi, q) = (form.current, &form.ask.questions[form.current]);
    let complete = form.complete(cx);
    let answers = form.answers(cx);

    let collapsed = |i: usize, depth: usize| {
        let question = &form.ask.questions[i];
        let answer = answers[i].join(", ");
        let done = !answer.is_empty();
        let ask_id = ask_id.clone();
        div()
            .id(ElementId::Name(format!("ask/{}/row/{i}", form.ask.id).into()))
            .h(u(24.))
            .ml(u(6. * depth as f32))
            .px_2()
            .flex()
            .flex_row()
            .items_center()
            .gap_2()
            .rounded_md()
            .cursor_pointer()
            .bg(rgba(t.hover))
            .opacity(1. - 0.15 * depth as f32)
            .text_size(text::sm())
            .hover(move |d| d.bg(rgba(t.line)).opacity(1.))
            .on_click(cx.listener(move |this, _, _, cx| this.show_ask_tab(&ask_id, i, cx)))
            .child(if done {
                icon("check", t.done).size(u(11.)).into_any_element()
            } else {
                div().size(u(11.)).flex_shrink_0().rounded_full().border_1().border_color(rgba(t.fg_2)).into_any_element()
            })
            .child(div().min_w_0().truncate().text_color(rgba(if done { t.fg_1 } else { t.fg })).child(question.text.clone()))
            .when(done, |d| {
                d.child(div().flex_1()).child(div().flex_shrink_0().max_w(relative(0.5)).truncate().text_color(rgba(t.fg)).child(answer))
            })
    };
    let above: Vec<_> = (0..qi).map(|i| collapsed(i, 0)).collect();
    let below: Vec<_> = (qi + 1..form.ask.questions.len()).enumerate().map(|(k, i)| collapsed(i, k)).collect();

    let option_row = |id: ElementId, on: bool, multi: bool, body: AnyElement| {
        let mark = div()
            .size(u(11.))
            .flex_shrink_0()
            .when(multi, |d| d.rounded(u(2.)))
            .when(!multi, |d| d.rounded_full())
            .border_1()
            .border_color(rgba(if on { t.accent_line } else { t.fg_2 }))
            .bg(rgba(if on { t.accent_line } else { 0 }));
        div()
            .id(id)
            .w_full()
            .min_h(u(26.))
            .px_2()
            .py_1()
            .flex()
            .flex_row()
            .items_center()
            .gap_2()
            .rounded_md()
            .cursor_pointer()
            .text_size(text::sm())
            .text_color(rgba(if on { t.fg } else { t.fg_1 }))
            .hover(move |d| d.bg(rgba(t.hover)))
            .child(mark)
            .child(body)
    };
    let options = q.options.iter().enumerate().map(|(oi, o)| {
        let ask_id = ask_id.clone();
        option_row(
            ElementId::Name(format!("ask/{}/{qi}/{oi}", form.ask.id).into()),
            form.picked[qi].contains(&oi),
            q.multi,
            div().flex_1().min_w_0().child(o.clone()).into_any_element(),
        )
        .on_click(cx.listener(move |this, _, _, cx| this.pick_option(&ask_id, qi, oi, cx)))
    });
    let custom = form.custom[qi].clone().map(|input| {
        let focus_id = ask_id.clone();
        let on = !input.read(cx).text().trim().is_empty();
        option_row(
            ElementId::Name(format!("ask/{}/{qi}/custom", form.ask.id).into()),
            on,
            q.multi,
            div().flex_1().min_w_0().child(input).into_any_element(),
        )
        .cursor_text()
        .on_click(cx.listener(move |this, _, _, cx| this.show_ask_tab(&focus_id, qi, cx)))
    });

    let next = form.next_unanswered(cx).is_some();
    let next_id = ask_id.clone();
    let action = (complete || next).then(|| {
        div()
            .id(ElementId::Name(format!("ask-next/{ask_id}").into()))
            .h(u(24.))
            .px_2p5()
            .flex()
            .items_center()
            .gap_1()
            .rounded_md()
            .cursor_pointer()
            .text_size(text::sm())
            .bg(rgba(if complete { t.accent } else { t.hover }))
            .text_color(rgba(t.fg))
            .font_weight(FontWeight::MEDIUM)
            .hover(move |d| d.bg(rgba(if complete { t.accent_active } else { t.line })))
            .on_click(cx.listener(move |this, _, _, cx| this.next_ask_tab(&next_id, cx)))
            .child(if complete { "Answer" } else { "Next" })
            .when(!complete, |d| d.child(icon("chevron-right", t.fg).size(u(11.))))
    });
    let card = div()
        .flex()
        .flex_col()
        .gap_1p5()
        .px_2()
        .pt_2()
        .pb_1p5()
        .rounded_md()
        .bg(rgba(t.popup))
        .border_1()
        .border_color(rgba(t.line))
        .shadow_sm()
        .child(div().px_2().text_size(text::base()).line_height(u(19.)).child(q.text.clone()))
        .child(div().flex().flex_col().gap_0p5().children(options).children(custom))
        .child(
            div()
                .min_h(u(24.))
                .flex()
                .flex_row()
                .items_center()
                .justify_end()
                .child(div().text_size(text::xs()).text_color(rgba(t.fg_2)).child(format!("{} of {}", qi + 1, form.ask.questions.len())))
                .child(div().flex_1())
                .children(action),
        );

    let insert_id = ask_id.clone();
    let insert = theme::button(format!("ask-insert/{ask_id}"), "corner-down-right", "Insert into composer")
        .tooltip(theme::tooltip("Put the questions in the composer to answer in your own message"))
        .on_click(cx.listener(move |this, _, window, cx| this.insert_ask(&insert_id, window, cx)));
    let dismiss_id = ask_id.clone();
    let dismiss = theme::button(format!("ask-dismiss/{ask_id}"), "x", "Dismiss")
        .on_click(cx.listener(move |this, _, _, cx| this.dismiss_ask(&dismiss_id, cx)));

    div()
        .w_full()
        .max_w(u(theme::COMPOSER * 0.9))
        .block_mouse_except_scroll()
        .flex()
        .flex_col()
        .gap_1()
        .px_3()
        .pt_2()
        .pb_2p5()
        .rounded(u(CORNER))
        .bg(rgba(t.raised))
        .shadow_md()
        .when(form.sent, |d| d.opacity(0.5))
        .child(
            div()
                .flex()
                .flex_row()
                .items_center()
                .gap_2()
                .mb_1()
                .text_size(text::sm())
                .text_color(rgba(t.fg_1))
                .child(icon("message-circle-question", t.warn))
                .child("The agent asks")
                .when(waiting > 0, |d| d.child(div().text_color(rgba(t.fg_2)).child(format!("· {waiting} more after this"))))
                .child(div().flex_1())
                .child(insert)
                .child(dismiss),
        )
        .children(above)
        .child(card)
        .children(below)
}

fn attachment_strip(this: &Dirigent, cx: &mut Context<Dirigent>) -> Option<Div> {
    let attachments = this.attachments(cx);
    if attachments.is_empty() {
        return None;
    }
    let thumbs = attachments.into_iter().map(|a| {
        let n = a.n;
        let remove = div()
            .id(ElementId::NamedInteger("att-rm".into(), n as u64))
            .absolute()
            .top(u(-6.))
            .right(u(-6.))
            .size(u(18.))
            .flex()
            .items_center()
            .justify_center()
            .rounded_full()
            .bg(rgba(theme::t().hover))
            .border_1()
            .border_color(rgba(theme::t().line))
            .cursor_pointer()
            .opacity(0.)
            .group_hover("att", |d| d.opacity(1.))
            .hover(|d| d.bg(rgba(theme::t().line)))
            .on_click(cx.listener(move |this, _, window, cx| {
                cx.stop_propagation();
                this.remove_attachment(n, window, cx);
            }))
            .child(icon("x", theme::t().fg).size(u(11.)));
        thumb_box(Some(&a.blob))
            .id(ElementId::NamedInteger("att".into(), n as u64))
            .group("att")
            .cursor_pointer()
            .on_click(cx.listener({
                let image = a.blob.image.clone();
                move |this, _, _, cx| this.set_preview(Some(image.clone()), cx)
            }))
            .child(thumb(&a.blob))
            .child(remove)
    });
    Some(div().flex().flex_row().flex_wrap().gap_3().px_4().pt_1().pb_3().children(thumbs))
}

const THUMB: f32 = 64.;
const INLINE_IMAGE_H: f32 = 420.;

fn inline_image<'a>(blobs: &'a HashMap<String, Blob>, seed: u64, cx: &'a Context<Dirigent>) -> impl FnMut(&str) -> AnyElement + 'a {
    let mut n = 0u64;
    move |id| {
        n += 1;
        let eid = ElementId::NamedInteger("md-img".into(), seed.wrapping_shl(8) | n);
        let Some(Blob { image, size: Some((w, h)) }) = blobs.get(id) else {
            return div().w(u(THUMB * 3.)).h(u(THUMB * 2.)).rounded_md().bg(rgba(theme::t().accent)).into_any();
        };
        // The box has the bitmap's shape: as wide as the column allows, up to the natural
        // size (real px: a big text size doesn't blur it) and INLINE_IMAGE_H tall; the img
        // fills it. (An `img` clamped by max sizes instead gets laid out taller than its
        // parent counts it and overlaps what follows: gpui makes both its sizes definite. And
        // the box needs its plain block parent: in a flex column, taffy takes an
        // aspect-ratio item's height from its unclamped width.)
        let box_ = div()
            .id(eid)
            .w_full()
            .max_w(px(*w).min(u(w * INLINE_IMAGE_H / h)))
            .aspect_ratio(w / h)
            .cursor_pointer()
            .on_click(cx.listener({
                let image = image.clone();
                move |this, _, _, cx| this.set_preview(Some(image.clone()), cx)
            }))
            .child(img(image.clone()).size_full().rounded_md());
        div().child(box_).into_any()
    }
}

fn inline_visual(visuals: &Visuals) -> impl FnMut(&str) -> Option<AnyElement> + '_ {
    move |html| {
        let Picture { image, aspect } = visuals.want(html)?;
        let picture = div().w_full().max_w(u(WIDTH as f32)).aspect_ratio(aspect).child(img(image).size_full());
        Some(div().child(picture).into_any())
    }
}

fn copy_code<'a>(copied: Option<&'a ElementId>, cx: &'a Context<Dirigent>) -> impl Fn(ElementId, SharedString) -> AnyElement + 'a {
    move |id, code| {
        let done = copied == Some(&id);
        let click = id.clone();
        raised(theme::icon_button(id, if done { "check" } else { "copy" }, cx, move |this, cx| {
            this.copy_item(click.clone(), code.to_string(), cx)
        }))
        .opacity(if done { 1. } else { 0. })
        .group_hover(markdown::CODE_GROUP, |d| d.opacity(1.))
        .into_any()
    }
}

fn raised(b: Stateful<Div>) -> Stateful<Div> {
    b.bg(rgba(theme::t().raised)).border_1().border_color(rgba(theme::t().line))
}

fn result_images(images: &[String], blobs: &HashMap<String, Blob>, seed: u64, cx: &mut Context<Dirigent>) -> Div {
    div().flex().flex_row().flex_wrap().gap_3().children(images.iter().enumerate().map(|(i, id)| {
        let blob = blobs.get(id);
        let box_ = thumb_box(blob)
            .id(ElementId::NamedInteger("result-img".into(), seed.wrapping_shl(8) | i as u64))
            .border_1()
            .border_color(rgba(theme::t().line));
        match blob {
            Some(blob) => box_
                .cursor_pointer()
                .hover(|d| d.border_color(rgba(theme::t().accent_line)))
                .on_click(cx.listener({
                    let image = blob.image.clone();
                    move |this, _, _, cx| this.set_preview(Some(image.clone()), cx)
                }))
                .child(thumb(blob)),
            None => box_.bg(rgba(theme::t().accent)),
        }
    }))
}

fn thumb_w(blob: Option<&Blob>) -> f32 {
    THUMB * blob.and_then(|b| b.size).map_or(1.5, |(w, h)| (w / h).clamp(0.5, 4.))
}

fn thumb_box(blob: Option<&Blob>) -> Div {
    div().relative().flex_shrink_0().w(u(thumb_w(blob))).h(u(THUMB)).rounded_md()
}

/// An image filling its `thumb_box`, cropped where the box's shape was clamped. With the
/// box's aspect ratio: `img` otherwise takes the bitmap's, and taffy lays it out at that
/// ratio even with both sizes set, overflowing a clamped box.
fn thumb(blob: &Blob) -> gpui::Img {
    img(blob.image.clone()).size_full().aspect_ratio(thumb_w(Some(blob)) / THUMB).rounded_md().object_fit(ObjectFit::Cover)
}

pub fn preview(image: Arc<gpui::Image>, cx: &mut Context<Dirigent>) -> Stateful<Div> {
    div()
        .id("preview")
        .absolute()
        .inset_0()
        .flex()
        .items_center()
        .justify_center()
        .bg(rgba(0x000000d0))
        .occlude()
        .cursor_pointer()
        .on_click(cx.listener(|this, _, _, cx| this.set_preview(None, cx)))
        .child(img(image).max_w(relative(0.9)).max_h(relative(0.9)).rounded_md().object_fit(ObjectFit::Contain))
}

const CORNER: f32 = 12.;
const BUBBLE_PAD: f32 = 16.;

fn glow() -> impl Iterator<Item = Div> {
    let color = theme::t().glow;
    [1., 0.5, 0.21].into_iter().enumerate().map(move |(i, k)| {
        let out = 2. * (i + 1) as f32;
        let alpha = ((color & 0xff) as f32 * k).round() as u32;
        div().absolute().inset(u(-out)).rounded(u(CORNER + out)).border_1().border_color(rgba(theme::alpha(color, alpha)))
    })
}

fn context_usage(t: &OpenThread) -> Option<u64> {
    let branch = t.entries.last().map(|last| dirigent_protocol::transcript::branch(&t.entries, &last.id)).unwrap_or_default();
    branch.into_iter().rev().find_map(|e| match &e.body {
        Body::Assistant(a) if !matches!(a.stop, Stop::Error | Stop::Aborted) => Some(a.usage.prompt() + a.usage.output).filter(|n| *n > 0),
        _ => None,
    })
}

fn stats_card(t: &OpenThread) -> AnyElement {
    let s = Stats::of(&t.entries);
    let count = |n: usize, what: &str| format!("{n} {what}{}", if n == 1 { "" } else { "s" });
    let dim = |text: String| div().text_color(rgba(theme::t().fg_1)).child(text);
    let card = div()
        .flex()
        .flex_col()
        .gap_0p5()
        .px_3()
        .py_2()
        .rounded_md()
        .bg(rgba(theme::t().popup))
        .border_1()
        .border_color(rgba(theme::t().line))
        .shadow_md()
        .text_size(text::sm())
        .line_height(u(18.))
        .text_color(rgba(theme::t().fg))
        .child(format!("{} in / {} out", kilo(s.prompt), kilo(s.output)))
        .child(dim(format!("{} cached ({}%)", kilo(s.cached), s.cached_pct())))
        .child(dim(count(s.user_messages, "user message")))
        .child(dim(count(s.assistant_messages, "assistant message")))
        .child(dim(count(s.compactions, "compaction")))
        .child(div().pt_0p5().font_weight(FontWeight::MEDIUM).child(dollars(s.cost)));
    // Zero-size absolute wrapper so the anchored card sits at the chip's corner, not the
    // flex static position.
    div()
        .absolute()
        .top_0()
        .left_0()
        .size_0()
        .child(deferred(anchored().anchor(Anchor::BottomLeft).offset(point(px(0.), u(-6.))).snap_to_window_with_margin(u(8.)).child(card)))
        .into_any()
}

pub fn render_item(this: &mut Dirigent, ix: usize, cx: &mut Context<Dirigent>) -> AnyElement {
    let Pane::Thread(t) = &this.pane else { return div().into_any() };
    let live = live_work(this);
    let now = this.clock;
    let fonts = this.fonts.clone();
    let blobs = &this.blobs;
    let keys = &mut Keys::new(&this.selection, Doc::Thread, ix);
    let archived = this.summary(&t.host, &t.id).is_some_and(|s| s.archived);
    let busy = this.summary(&t.host, &t.id).and_then(|s| busy(s, now));
    let bubble = if archived { theme::t().hover } else { theme::t().bubble };
    let idle = !this.is_running() && t.rewound.is_none();
    let mut col = div().w_full().max_w(u(theme::COLUMN)).flex().flex_col();
    if ix < t.timeline.items.len() {
        if live.as_ref().is_none_or(|(k, _)| *k != ix) {
            let editing = t.editing.as_ref().is_some_and(|e| t.timeline.entry_id(ix) == Some(e.entry_id.as_str()));
            col = col.child(if editing {
                edit_bubble(this, bubble, &fonts, cx).into_any()
            } else {
                render_timeline_item(&t.timeline.items[ix], ix, t, bubble, idle, blobs, now, &fonts, keys, cx)
            });
        }
    } else {
        let echo = |mode| t.streaming.echo.iter().filter(move |(_, m)| *m == mode).map(|(text, _)| text.as_str()).collect::<Vec<_>>();
        for text in echo(PromptMode::Prompt) {
            col = col.child(user_bubble(text, &[], blobs, TAIL, t, bubble, false, &fonts, keys, cx));
        }
        if let Some((key, work)) = live {
            col = col.child(work_block(&work, key, true, busy, t, bubble, blobs, now, &fonts, keys, cx));
            if let Some(text) = work.latest_text().filter(|r| !r.trim().is_empty()) {
                col = col.child(reply_block(text, TAIL, t, blobs, &fonts, keys, cx));
            }
            for (six, step) in work.steps.iter().enumerate() {
                if let Step::Steer { text, images, queued: true } = step {
                    col = col.child(steer_bubble(text, images, (key, six), true, t, bubble, blobs, &fonts, keys, cx));
                }
            }
        }
        for text in echo(PromptMode::FollowUp) {
            col = col.child(user_bubble(text, &[], blobs, TAIL, t, bubble, false, &fonts, keys, cx));
        }
        col = col.pb_6();
    }
    if ix == 0 {
        col = col.pt_4();
    }
    let overscroll = if ix == t.timeline.items.len() { t.ask_h } else { px(0.) };
    div().w_full().flex().flex_col().items_center().px_6().pb(overscroll).child(col).into_any()
}

fn work_started(this: &Dirigent, t: &OpenThread) -> i64 {
    if let Some(prompted) = t.timeline.prompted_at() {
        return prompted;
    }
    let Some(began) = this.summary(&t.host, &t.id).and_then(|thread| thread.running_since) else { return this.clock };
    let cut = t.timeline.items.iter().rev().find_map(|i| if let Item::Work(w) = i { w.ended } else { None });
    began.max(cut.unwrap_or(0))
}

fn live_work(this: &Dirigent) -> Option<(usize, Work)> {
    let Pane::Thread(t) = &this.pane else { return None };
    if !this.is_running() && t.streaming.echo.is_empty() && t.streaming.answers.is_empty() {
        return None;
    }
    let (key, mut work) = match t.timeline.open_work() {
        Some(i) => match &t.timeline.items[i] {
            Item::Work(w) => (i, w.clone()),
            _ => (TAIL, Work::default()),
        },
        None => {
            let started = if t.streaming.echo.is_empty() { work_started(this, t) } else { this.clock };
            (TAIL, Work { model: this.model.clone(), effort: this.effort.clone(), started, ..Default::default() })
        }
    };
    work.steps.extend(t.streaming.live_steps());
    Some((key, work))
}

#[allow(clippy::too_many_arguments)]
fn render_timeline_item(
    item: &Item,
    ix: usize,
    t: &OpenThread,
    bubble: u32,
    idle: bool,
    blobs: &HashMap<String, Blob>,
    now: i64,
    fonts: &Fonts,
    keys: &mut Keys,
    cx: &mut Context<Dirigent>,
) -> AnyElement {
    match item {
        Item::User { text, images, .. } => user_bubble(text, images, blobs, ix, t, bubble, idle, fonts, keys, cx).into_any(),
        Item::Work(w) => work_block(w, ix, false, None, t, bubble, blobs, now, fonts, keys, cx).into_any(),
        Item::Assistant(text) => reply_block(text, ix, t, blobs, fonts, keys, cx).into_any(),
        Item::Error(text) => div()
            .w_full()
            .py(u(2.))
            .text_size(text::sm())
            .line_height(u(19.))
            .text_color(rgba(theme::t().error))
            .child(keys.plain(text.clone()))
            .into_any(),
        Item::Compaction => notice_line(keys.plain("context compacted".to_owned())),
        Item::Notice(text) => notice_line(keys.plain(text.clone())),
        Item::Child { thread_id, text } => child_line(format!("child/{ix}"), &t.host, thread_id, text, cx),
    }
}

fn child_line(id: String, host: &str, thread_id: &str, text: &str, cx: &mut Context<Dirigent>) -> AnyElement {
    let (host, thread_id) = (host.to_owned(), thread_id.to_owned());
    div()
        .id(ElementId::Name(id.into()))
        .w_full()
        .py_1()
        .flex()
        .flex_row()
        .items_center()
        .gap_1p5()
        .text_size(text::sm())
        .text_color(rgba(theme::t().fg_2))
        .cursor_pointer()
        .hover(|d| d.text_color(rgba(theme::t().fg)))
        .on_click(cx.listener(move |this, _, _, cx| this.select_thread(&host, &thread_id, cx)))
        .child(icon("corner-down-right", theme::t().fg_2).size(u(12.)))
        .child(text.to_owned())
        .into_any()
}

fn notice_line(text: impl IntoElement) -> AnyElement {
    div().w_full().py_1().text_size(text::sm()).text_color(rgba(theme::t().fg_2)).child(text).into_any()
}

#[allow(clippy::too_many_arguments)]
fn reply_block(
    text: &str,
    ix: usize,
    t: &OpenThread,
    blobs: &HashMap<String, Blob>,
    fonts: &Fonts,
    keys: &mut Keys,
    cx: &mut Context<Dirigent>,
) -> Div {
    let body = markdown::render(
        text,
        &fonts.mono,
        keys,
        Some(&mut inline_image(blobs, ix as u64, cx)),
        Some(&mut inline_visual(&t.visuals)),
        Some(&copy_code(t.copied.as_ref(), cx)),
        &[],
    );
    let top = if markdown::starts_with_code(text) { 4. + CORNER_BUTTONS_H } else { 4. };
    div()
        .group("msg")
        .relative()
        .w_full()
        .pt(u(top))
        .pb_1()
        .font_family(fonts.prose.clone())
        .text_size(text::lg())
        .line_height(u(22.))
        .child(body)
        .children(pin_bar(ix, t, BUBBLE_PAD + PIN_GAP).map(|b| b.top(u(top))))
        .children(corner_buttons(ix, text, t, true, false, cx))
}

const PIN_GAP: f32 = 6.;
const CORNER_BUTTONS_H: f32 = 26.;

fn pin_bar(ix: usize, t: &OpenThread, left: f32) -> Option<Div> {
    t.is_pinned(ix).then(|| div().absolute().left(u(-left)).top_1().bottom_1().w(u(2.)).rounded_full().bg(rgba(theme::t().warn)))
}

fn corner_buttons(ix: usize, text: &str, t: &OpenThread, fork: bool, edit: bool, cx: &mut Context<Dirigent>) -> Option<Div> {
    if ix == TAIL {
        return None;
    }
    let copy_id = ElementId::from(("copy", ix));
    let (copied, pinned) = (t.copied.as_ref() == Some(&copy_id), t.is_pinned(ix));
    let text = text.to_owned();
    let tint = if pinned { theme::t().warn } else { theme::t().fg_1 };
    let edit = edit.then(|| {
        raised(theme::icon_button(("edit", ix), "pencil", cx, move |this, cx| this.start_edit(ix, cx)))
            .tooltip(theme::tooltip("Edit and send again from here"))
    });
    let fork = fork.then(|| {
        raised(theme::icon_button(("fork", ix), "git-fork", cx, move |this, _| this.fork_at(ix))).tooltip(theme::tooltip("Fork from here"))
    });
    let pin = raised(theme::tinted_icon_button(("pin", ix), "pin", tint, cx, move |this, cx| this.toggle_pin(ix, cx)));
    let copy = raised(theme::icon_button(copy_id.clone(), if copied { "check" } else { "copy" }, cx, move |this, cx| {
        this.copy_item(copy_id.clone(), text.clone(), cx)
    }));
    Some(
        div()
            .absolute()
            .top_0()
            .right_0()
            .flex()
            .flex_row()
            .gap_1()
            .opacity(if copied { 1. } else { 0. })
            .group_hover("msg", |d| d.opacity(1.))
            .children(edit)
            .children(fork)
            .child(pin)
            .child(copy),
    )
}

// MUST KILL: rearchitect: pair images by stored refs only (backfill older messages) instead of guessing from token order
fn image_links(text: &str, refs: &[u32], images: &[String]) -> Vec<(String, SharedString)> {
    let ns = if refs.is_empty() { crate::input::image_numbers(text) } else { refs.to_vec() };
    ns.into_iter().zip(images).map(|(n, id)| (crate::input::image_token(n), format!("dirigent://image/{id}").into())).collect()
}

/// A message's attached images as thumbnails (a blank box while the bytes are on their
/// way); click one to see it. `id` names the i-th: thumbnail ids must be unique across the
/// thread, since gpui shares click state between same-id elements and drops clicks on the
/// others. `None` without images.
fn thumbs(images: &[String], blobs: &HashMap<String, Blob>, id: impl Fn(usize) -> ElementId, cx: &mut Context<Dirigent>) -> Option<Div> {
    (!images.is_empty()).then(|| {
        div().flex().flex_row().flex_wrap().gap_3().pt_3().children(images.iter().enumerate().map(|(i, blob_id)| {
            let blob = blobs.get(blob_id);
            let box_ = thumb_box(blob).id(id(i));
            match blob {
                Some(blob) => box_
                    .cursor_pointer()
                    .on_click(cx.listener({
                        let image = blob.image.clone();
                        move |this, _, _, cx| this.set_preview(Some(image.clone()), cx)
                    }))
                    .child(thumb(blob)),
                None => box_.bg(rgba(theme::t().accent)),
            }
        }))
    })
}

#[allow(clippy::too_many_arguments)]
fn user_bubble(
    text: &str,
    images: &[String],
    blobs: &HashMap<String, Blob>,
    ix: usize,
    t: &OpenThread,
    bubble: u32,
    idle: bool,
    fonts: &Fonts,
    keys: &mut Keys,
    cx: &mut Context<Dirigent>,
) -> Div {
    let typed = if ix == TAIL { None } else { t.typed_message(ix) };
    let (edit, fork) = (typed.is_some() && idle, typed.is_some_and(|e| e.parent_id.is_some()));
    let (body, refs) = split_image_refs(text);
    let thumbs = thumbs(images, blobs, |i| ElementId::NamedInteger("bubble-img".into(), (ix as u64) << 8 | i as u64), cx);
    div()
        .group("msg")
        .relative()
        .mx(u(-BUBBLE_PAD))
        .mt_4()
        .mb_3()
        .px(u(BUBBLE_PAD))
        .py_3()
        .rounded_lg()
        .bg(rgba(bubble))
        .font_family(fonts.prose.clone())
        .text_size(text::lg())
        .line_height(u(22.))
        .child(markdown::render(body, &fonts.mono, keys, None, None, None, &image_links(body, &refs, images)))
        .children(thumbs)
        .children(pin_bar(ix, t, PIN_GAP))
        .children(corner_buttons(ix, body, t, fork, edit, cx).map(|b| b.top_2().right_2()))
}

fn edit_bubble(this: &Dirigent, bubble: u32, fonts: &Fonts, cx: &mut Context<Dirigent>) -> Div {
    let (model, effort) = model_chips(this, true, cx);
    let blocked = this.open().is_some_and(|t| this.not_ready(&t.host, &t.id).is_some()) || this.composer_offline();
    let ready = !this.composer.read(cx).text().trim().is_empty() && this.model.is_some() && !this.is_running() && !blocked;
    let cancel = div()
        .id("edit-cancel")
        .h(u(24.))
        .px_2()
        .flex()
        .items_center()
        .rounded_md()
        .cursor_pointer()
        .text_color(rgba(theme::t().fg_1))
        .hover(|d| d.bg(rgba(theme::t().hover)).text_color(rgba(theme::t().fg)))
        .on_click(cx.listener(|this, _, _, cx| this.cancel_edit(cx)))
        .child("Cancel");
    div()
        .mx(u(-BUBBLE_PAD))
        .mt_4()
        .mb_3()
        .flex()
        .flex_col()
        .rounded_lg()
        .bg(rgba(bubble))
        .border_1()
        .border_color(rgba(theme::t().accent))
        .child(
            div()
                .px(u(BUBBLE_PAD))
                .pt_3()
                .pb_2()
                .font_family(fonts.prose.clone())
                .text_size(text::lg())
                .line_height(u(22.))
                .child(this.composer.clone()),
        )
        .children(attachment_strip(this, cx))
        .child(
            div()
                .flex()
                .flex_row()
                .items_center()
                .gap_1()
                .px_2()
                .pb_2()
                .child(div().flex_1().min_w_0().flex().flex_row().items_center().gap_1().child(model).child(effort))
                .children(offline_note(this))
                .child(cancel.flex_shrink_0())
                .child(send_button("edit-send", ready, cx).flex_shrink_0()),
        )
}

fn busy(s: &ThreadSummary, now: i64) -> Option<String> {
    match s.setup {
        Some(Setup::Workspace) => Some("Creating a workspace…".into()),
        Some(Setup::NixShell) => Some("Activating nix dev shell…".into()),
        None => s.compacting_since.map(|since| format!("Compacting context… · {}", elapsed(now - since))),
    }
}

#[allow(clippy::too_many_arguments)]
fn work_block(
    w: &Work,
    key: usize,
    live: bool,
    busy: Option<String>,
    t: &OpenThread,
    bubble: u32,
    blobs: &HashMap<String, Blob>,
    now: i64,
    fonts: &Fonts,
    keys: &mut Keys,
    cx: &mut Context<Dirigent>,
) -> Div {
    let taken: Vec<_> = w
        .steps
        .iter()
        .enumerate()
        .filter_map(|(six, s)| match s {
            Step::Steer { text, images, queued: false } => {
                Some(steer_bubble(text, images, (key, six), false, t, bubble, blobs, fonts, keys, cx))
            }
            _ => None,
        })
        .collect();
    let children: Vec<_> = w
        .steps
        .iter()
        .enumerate()
        .filter_map(|(i, s)| match s {
            Step::Child { thread_id, text } => Some(child_line(format!("child/{key}/{i}"), &t.host, thread_id, text, cx)),
            _ => None,
        })
        .collect();
    div().w_full().flex().flex_col().children(taken).child(work_line(w, key, live, busy, t, now, cx)).children(children)
}

#[allow(clippy::too_many_arguments)]
fn steer_bubble(
    text: &str,
    images: &[String],
    key: (usize, usize),
    pending: bool,
    t: &OpenThread,
    bubble: u32,
    blobs: &HashMap<String, Blob>,
    fonts: &Fonts,
    keys: &mut Keys,
    cx: &mut Context<Dirigent>,
) -> Div {
    let (body, refs) = split_image_refs(text);
    let cancelable = pending && t.streaming.queued_steers().any(|s| s == text);
    let cancel = cancelable.then(|| {
        let text = text.to_owned();
        div()
            .id(ElementId::Name(format!("steer-cancel/{}", gpui::hash(&text)).into()))
            .cursor_pointer()
            .text_color(rgba(theme::t().fg_2))
            .hover(|d| d.text_color(rgba(theme::t().error)))
            .opacity(0.)
            .group_hover("msg", |d| d.opacity(1.))
            .on_click(cx.listener(move |this, _, _, cx| {
                cx.stop_propagation();
                this.cancel_queued(text.clone(), cx);
            }))
            .child("cancel")
    });
    let label = div()
        .flex()
        .flex_row()
        .items_center()
        .gap_2()
        .mb_1()
        .text_size(text::xs())
        .line_height(u(14.))
        .child(div().text_color(rgba(theme::t().accent_ink)).child("steer"))
        .child(div().flex_1())
        .children(cancel)
        .when(pending, |d| d.child(div().text_color(rgba(theme::t().fg_2)).child("pending")));
    div()
        .group("msg")
        .relative()
        .mx(u(-BUBBLE_PAD))
        .my_2()
        .px(u(BUBBLE_PAD))
        .py_2p5()
        .rounded_lg()
        .bg(rgba(bubble))
        .when(pending, |d| d.opacity(0.7))
        .font_family(fonts.prose.clone())
        .text_size(text::lg())
        .line_height(u(22.))
        .child(label)
        .child(markdown::render(body, &fonts.mono, keys, None, None, None, &image_links(body, &refs, images)))
        .children(thumbs(images, blobs, |i| ElementId::NamedInteger("steer-img".into(), step_id_n(key) << 8 | i as u64), cx))
}

fn steer_row(key: (usize, usize), text: &str, queued: bool, t: &OpenThread, keys: &mut Keys, cx: &mut Context<Dirigent>) -> Div {
    let cancel = (queued && t.streaming.queued_steers().any(|s| s == text)).then(|| {
        let text = text.to_owned();
        div()
            .id(ElementId::NamedInteger(SharedString::from("steer-cancel"), step_id_n(key)))
            .flex_shrink_0()
            .cursor_pointer()
            .text_color(rgba(theme::t().fg_2))
            .hover(|d| d.text_color(rgba(theme::t().error)))
            .opacity(0.)
            .group_hover("steer", |d| d.opacity(1.))
            .on_click(cx.listener(move |this, _, _, cx| {
                cx.stop_propagation();
                this.cancel_queued(text.clone(), cx);
            }))
            .child("cancel")
    });
    div()
        .group("steer")
        .flex()
        .flex_row()
        .gap_2()
        .py(u(2.))
        .text_size(text::sm())
        .line_height(u(19.))
        .child(div().flex_shrink_0().text_color(rgba(theme::t().accent_ink)).child("steer"))
        .child(div().flex_1().min_w_0().text_color(rgba(theme::t().fg_1)).child(keys.plain(split_image_refs(text).0.to_owned())))
        .children(cancel)
        .when(queued, |d| d.child(div().flex_shrink_0().text_color(rgba(theme::t().fg_2)).child("queued")))
}

fn work_line(w: &Work, key: usize, live: bool, busy: Option<String>, t: &OpenThread, now: i64, cx: &mut Context<Dirigent>) -> AnyElement {
    let shown = t.log == Some(key);
    let tools = match w.tool_count() {
        0 => "no tool calls".to_owned(),
        1 => "1 tool call".to_owned(),
        n => format!("{n} tool calls"),
    };
    let model = w.model.as_deref().map(theme::short_model).unwrap_or("model").to_owned();
    let status = match (live, busy) {
        (true, Some(busy)) => vec![busy],
        (true, None) => vec![format!("Working for {}", elapsed(now - w.started)), "·".into(), tools],
        (false, _) => vec![duration(w.duration_ms()), "·".into(), tools],
    };
    let parts: Vec<String> = [Some(model), w.effort.clone(), Some("·".into())].into_iter().flatten().chain(status).collect();
    let row = row(step_id(key, usize::MAX))
        .my_1()
        .gap_2()
        .text_size(text::sm())
        .text_color(rgba(theme::t().fg_1))
        .when(shown, |d| d.bg(rgba(theme::t().raised)))
        .on_click(cx.listener(move |this, _, _, cx| this.toggle_log(key, cx)));
    if live {
        return row.child(working::line(key, parts.join(" "))).into_any();
    }
    row.children(parts.into_iter().map(|p| div().when(p == "·", |d| d.text_color(rgba(theme::t().fg_2))).child(p))).into_any()
}

pub fn shown_work(this: &Dirigent) -> Option<(Work, bool)> {
    let Pane::Thread(t) = &this.pane else { return None };
    let key = t.log?;
    if let Some((_, w)) = live_work(this).filter(|(k, _)| *k == key) {
        return Some((w, true));
    }
    match t.timeline.items.get(key) {
        Some(Item::Work(w)) => Some((w.clone(), false)),
        _ => None,
    }
}

fn log_body(this: &Dirigent, cx: &mut Context<Dirigent>) -> Option<AnyElement> {
    let Pane::Thread(t) = &this.pane else { return None };
    t.log?;
    let (work, live) = shown_work(this)?;
    let n = work.steps.len().max(1);
    let state = &t.log_list;
    let had = state.item_count();
    if n > had {
        state.splice(had..had, n - had);
    } else if n < had {
        state.splice(n..had, 0);
    }
    let work = Arc::new(work);
    let steps = list(
        state.clone(),
        cx.processor(move |this, six, _window, cx| {
            div()
                .px_4()
                .when(six == 0, |d| d.pt_1())
                .when(six + 1 == work.steps.len().max(1), |d| d.pb_3())
                .child(render_step(this, &work, live, six, cx))
                .into_any()
        }),
    );
    Some(steps.flex_1().min_h_0().w_full().text_size(text::sm()).line_height(u(19.)).into_any_element())
}

pub fn render_step(this: &mut Dirigent, work: &Work, live: bool, six: usize, cx: &mut Context<Dirigent>) -> AnyElement {
    let Pane::Thread(t) = &this.pane else { return div().into_any() };
    let Some(key) = t.log else { return div().into_any() };
    let Some(step) = work.steps.get(six) else {
        let text = if live { "Waiting for the model" } else { "No steps" };
        return div().h(u(ROW_H)).flex().items_center().text_color(rgba(theme::t().fg_2)).child(text).into_any();
    };
    let mono = &this.fonts.mono;
    let blobs = &this.blobs;
    let keys = &mut Keys::new(&this.selection, Doc::Log, six);
    match step {
        Step::Thinking(text) => div()
            .py_0p5()
            .text_color(rgba(theme::t().fg_1))
            .child(markdown::render(text, mono, keys, None, None, Some(&copy_code(t.copied.as_ref(), cx)), &[]))
            .into_any(),
        Step::Text(text) => {
            let body = markdown::render(
                text,
                mono,
                keys,
                Some(&mut inline_image(blobs, step_id_n((key, six)), cx)),
                None,
                Some(&copy_code(t.copied.as_ref(), cx)),
                &[],
            );
            div().py_0p5().text_color(rgba(theme::t().fg)).child(body).into_any()
        }
        Step::Tool(call) => render_tool(call, (key, six), t, blobs, this.clock, keys, cx),
        Step::Steer { text, queued, .. } => steer_row((key, six), text, *queued, t, keys, cx).into_any(),
        Step::Child { thread_id, text } => child_line(format!("log-child/{key}/{six}"), &t.host, thread_id, text, cx),
        Step::CacheMiss(text) => div()
            .h(u(ROW_H))
            .flex()
            .items_center()
            .child(div().min_w_0().truncate().text_color(rgba(theme::t().warn)).child(keys.plain(text.clone())))
            .into_any(),
        Step::Notice(text) => {
            let failed = text.contains("exit ") && !text.contains("exit 0");
            div()
                .h(u(ROW_H))
                .flex()
                .flex_row()
                .items_center()
                .gap_2()
                .child(div().flex_shrink_0().text_color(rgba(if failed { theme::t().error } else { theme::t().done })).child("job"))
                .child(
                    div()
                        .flex_1()
                        .min_w_0()
                        .truncate()
                        .text_color(rgba(theme::t().fg_1))
                        .child(keys.plain(text.trim_start_matches("job ").to_owned())),
                )
                .into_any()
        }
    }
}

fn render_tool(
    call: &ToolCall,
    key: (usize, usize),
    t: &OpenThread,
    blobs: &HashMap<String, Blob>,
    now: i64,
    keys: &mut Keys,
    cx: &mut Context<Dirigent>,
) -> AnyElement {
    let expanded = t.expanded.contains(&key);
    let failed = call.result.as_ref().is_some_and(|r| r.is_error);
    let (time, color) = match (t.streaming.running_since(&call.id), call.started.zip(call.ended)) {
        (Some(since), _) => {
            let so_far = elapsed(now - since);
            (Some(call.timeout_secs.map(|s| format!("{so_far} / {}", duration((s * 1000.) as i64))).unwrap_or(so_far)), theme::t().working)
        }
        (None, Some((s, e))) => (Some(duration(e - s)), if failed { theme::t().error } else { theme::t().fg_2 }),
        (None, None) => (failed.then(|| "failed".to_owned()), theme::t().error),
    };
    let line = row(step_id(key.0, key.1))
        .gap_2()
        .on_click(cx.listener(move |this, _, _, cx| this.toggle_expanded(key, cx)))
        .child(div().flex_shrink_0().text_color(rgba(theme::t().done)).child(call.name.clone()))
        .child(div().flex_1().min_w_0().truncate().text_color(rgba(theme::t().fg_1)).child(call.primary.clone()))
        .children(time.map(|s| div().flex_shrink_0().text_color(rgba(color)).child(s)));
    let body = expanded.then(|| {
        let mut b = div().my_1().px_3().py_2().rounded_md().bg(rgba(theme::t().raised)).flex().flex_col().gap_2();
        if !call.args.is_empty() {
            b = b.child(div().text_color(rgba(theme::t().fg_1)).child(keys.plain(call.args.clone())));
        }
        if let Some(r) = &call.result {
            let mut s: String = r.text.chars().take(MAX_RESULT_CHARS).collect();
            if s.len() < r.text.len() {
                s.push_str("\n…");
            }
            let color = if r.is_error { theme::t().error } else { theme::t().fg_1 };
            b = b.child(div().pt_2().border_t_1().border_color(rgba(theme::t().line)).text_color(rgba(color)).child(keys.plain(s)));
            if !r.images.is_empty() {
                b = b.child(result_images(&r.images, blobs, step_id_n(key), cx));
            }
        }
        b
    });
    div().w_full().flex().flex_col().child(line).children(body).into_any()
}

fn row(id: ElementId) -> gpui::Stateful<Div> {
    div()
        .id(id)
        .h(u(ROW_H))
        .mx(u(-4.))
        .px_1()
        .rounded_sm()
        .flex()
        .flex_row()
        .items_center()
        .cursor_pointer()
        .hover(|d| d.bg(rgba(theme::t().raised)))
}

fn step_id(item: usize, step: usize) -> ElementId {
    ElementId::NamedInteger(SharedString::from("step"), step_id_n((item, step)))
}

fn step_id_n((item, step): (usize, usize)) -> u64 {
    (item as u64).wrapping_mul(100_000) ^ step as u64
}

pub fn duration(ms: i64) -> String {
    let ms = ms.max(0);
    if ms < 100 {
        format!("{ms}ms")
    } else if ms < 1000 {
        format!("0.{}s", format!("{:02}", ms / 10).trim_end_matches('0'))
    } else {
        elapsed(ms)
    }
}

pub fn elapsed(ms: i64) -> String {
    let s = ms.max(0) / 1000;
    if s < 60 {
        format!("{s}s")
    } else if s < 3600 {
        format!("{}m {:02}s", s / 60, s % 60)
    } else {
        format!("{}h {:02}m", s / 3600, (s % 3600) / 60)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn durations() {
        assert_eq!(duration(34), "34ms");
        assert_eq!(duration(100), "0.1s");
        assert_eq!(duration(340), "0.34s");
        assert_eq!(duration(999), "0.99s");
        assert_eq!(duration(1000), "1s");
        assert_eq!(duration(3200), "3s");
        assert_eq!(duration(83_000), "1m 23s");
        assert_eq!(duration(3_900_000), "1h 05m");
        assert_eq!(elapsed(340), "0s");
        assert_eq!(elapsed(12_900), "12s");
    }
}
