//! Main pane for one thread: header, the entries as a lazy `list()` in a centered reading
//! column, and the composer card with its model / effort chips. Also the new-thread page,
//! which is the same composer alone, centered in the pane.
//!
//! The transcript: user messages are raised, replies are markdown prose, and everything
//! the model did in between (thinking, interim prose, tool calls) is one *work* line per
//! turn (`model effort · duration · n tool calls · what it's doing now`). The transcript
//! never shows the steps themselves: clicking a work line (or `]`) opens `log_panel`, a
//! resizable panel on the pane's right with the full log, so the reading column only ever
//! grows. While a run goes on, the model's latest prose streams below its work line a line
//! at a time; the next prose, or the final reply, replaces it. The ruler (`ruler.rs`) lies
//! over the list's left edge: one mark per message, hover to preview, click to jump.
//! Messages can be pinned (the button next to `copy`); pins colour their marks.
//! Questions the model asked (`ask_user`) float above the composer (`ask_card`), out of the
//! transcript's flow, until they are answered or dismissed. Steers (typed, or the answers
//! to an ask) are user bubbles: `steer · pending` below the work line while pi holds them,
//! `steer` above it once the model has them.

use crate::app::{AskForm, Dirigent, Menu, OpenThread, Pane, Resize, Target};
use crate::icons::icon;
use crate::lists::{breadcrumb, header};
use crate::markdown;
use crate::menu::{self, Side};
use crate::picker;
use crate::projects;
use crate::ruler;
use crate::select::{Doc, Keys};
use crate::theme::{self, Fonts, Status, dollars, kilo};
use crate::view::{Item, Stats, Step, ToolCall, Work};
use dirigent_protocol::PromptMode;
use gpui::{
    Anchor, AnyElement, Context, Div, ElementId, Focusable, FontWeight, MouseButton, ObjectFit, SharedString, Stateful, anchored, deferred,
    div, img, list, point, prelude::*, px, relative, rgba,
};
use serde_json::Value;
use std::collections::HashMap;
use std::sync::Arc;

/// Work key for the run in progress while it has no persisted item yet.
pub const TAIL: usize = usize::MAX;
const MAX_RESULT_CHARS: usize = 20_000;
/// Tool rows are compact and table-like; everything inside a work shares this size.
const ROW_H: f32 = 22.;
const STEP_SIZE: f32 = 12.5;
/// Default log panel width; the user can drag it between `app::LOG_MIN` and `LOG_MAX`.
pub const LOG_WIDTH: f32 = 400.;

pub fn render(this: &mut Dirigent, cx: &mut Context<Dirigent>) -> Div {
    let pane = div().flex_1().min_w_0().h_full().flex().flex_col();
    let Pane::Thread(t) = &this.pane else {
        return match &this.pane {
            Pane::Draft { target } => draft_page(pane, target.clone(), this, cx),
            _ => pane,
        };
    };
    let list_state = t.list.clone();
    let (host, id) = (t.host.clone(), t.id.clone());
    let asks: Vec<Div> = t.asks.iter().map(|form| ask_card(form, cx)).collect();
    let summary = this.summary(&host, &id).cloned();
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
    let snoozed = status == Status::Snoozed;
    let pinned = this.pinned(&host, &id);
    let confirm_open = this.menu == Some(Menu::ConfirmDelete);
    let confirm = confirm_open.then(|| {
        let (h, i) = (host.clone(), id.clone());
        menu::popup(
            Side::BelowRight(26.),
            vec![
                menu::header("This removes the thread and its history."),
                menu::danger("del", "Delete thread", move |this, cx| this.delete_thread(&h, &i, cx), cx),
            ],
            cx,
        )
    });
    let actions = div()
        .flex()
        .flex_row()
        .items_center()
        .gap_1()
        .font_weight(FontWeight::NORMAL)
        .child(
            div()
                .flex()
                .flex_row()
                .items_center()
                .gap_1p5()
                .mr_2()
                .text_size(px(12.))
                .text_color(rgba(theme::t().fg_1))
                .child(status.glyph())
                .child(status.label()),
        )
        .when(matches!(status, Status::Asking | Status::Done | Status::Snoozed), |d| {
            // Snooze: out of the sidebar until the next run; amber while snoozed, click wakes it.
            d.child(theme::tinted_icon_button(
                "hdr-snooze",
                "moon",
                if snoozed { theme::t().warn } else { theme::t().fg_1 },
                cx,
                move |this, cx| this.set_snoozed(&h3, &i3, !snoozed, cx),
            ))
        })
        // Pin: on top of its list (an amber pin before the title shows it); amber while
        // pinned, click again unpins.
        .child(theme::tinted_icon_button("hdr-pin", "pin", if pinned { theme::t().warn } else { theme::t().fg_1 }, cx, move |this, cx| {
            this.set_pinned(&h4, &i4, !pinned, cx)
        }))
        .child(theme::icon_button("hdr-archive", if archived { "archive-restore" } else { "archive" }, cx, move |this, cx| {
            this.set_archived(&h1, &i1, !archived, cx)
        }))
        .child(
            theme::icon_button("hdr-delete", "trash-2", cx, |this, cx| this.toggle_menu(Menu::ConfirmDelete, cx))
                .when(confirm_open, |d| d.bg(rgba(theme::t().hover)))
                .children(confirm),
        )
        .into_any_element();

    let column = div()
        .flex_1()
        .min_w_0()
        .h_full()
        .flex()
        .flex_col()
        .child(
            div()
                .relative()
                .flex_1()
                .min_h_0()
                .w_full()
                .flex()
                .flex_col()
                .child(list(list_state, cx.processor(|this, ix, _window, cx| render_item(this, ix, cx))).flex_1().w_full())
                .child(ruler::render(this, cx)),
        )
        .child(
            // The ask stack is a zero-height layer at the composer's top edge that grows
            // upward (`justify_end`), so it covers the transcript instead of pushing it.
            div()
                .relative()
                .w_full()
                .flex()
                .flex_col()
                .items_center()
                .px_6()
                .pb_5()
                .pt_2()
                .child(
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
                        .gap_2()
                        .children(asks),
                )
                .child(composer(this, cx)),
        );
    let titling = this.titling(&host, &id);
    pane.child(header(breadcrumb(project, title, pinned, titling, cx), Some(actions)))
        .child(div().flex_1().min_h_0().flex().flex_row().child(column).children(log_panel(this, cx)))
}

/// The new-thread page: `Create thread in <project>` above the composer, both centered in
/// the pane. Projects linked across hosts are one entry; `on <host>` then picks the host.
/// Leaving the page (sidebar, a thread) keeps whatever was typed.
fn draft_page(pane: Div, target: Option<Target>, this: &Dirigent, cx: &mut Context<Dirigent>) -> Div {
    let (groups, _) = projects::groups(&this.hosts, &this.links);
    let group = target.as_ref().and_then(|t| groups.iter().find(|g| g.has(&t.host, &t.project_id)));
    let project = group.map(|g| g.name.clone());
    let target_open = this.menu == Some(Menu::Target);
    let target_menu = target_open.then(|| {
        let mut items = Vec::new();
        for g in &groups {
            let selected = group.is_some_and(|s| s.key == g.key);
            // Keep the host when switching to a group that also lives there.
            let t: Target = g.member_for(target.as_ref().map(|t| t.host.as_str())).into();
            // Hosts are picked in the `on <host>` chip; linked projects' paths differ per host.
            let detail = if g.members.len() > 1 { String::new() } else { g.members[0].project.path.clone() };
            items.push(menu::item(
                format!("t/{}", g.key),
                g.name.clone(),
                detail,
                selected,
                move |this, cx| this.set_target(t.clone(), cx),
                cx,
            ));
        }
        if items.is_empty() {
            items.push(menu::header("No projects yet. Add one with + in the sidebar."));
        }
        menu::popup(Side::Below(26.), items, cx)
    });
    // `on <host>`: a plain chip when the project is on one host, a dropdown when it is linked
    // across several.
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
                        m.host.clone(),
                        m.project.path.clone(),
                        selected,
                        move |this, cx| this.set_target(mt.clone(), cx),
                        cx,
                    )
                })
                .collect();
            menu::popup(Side::Below(26.), items, cx)
        });
        if members.len() > 1 {
            theme::labeled_chip("host-chip", "on", t.host.clone(), false, host_open)
                .on_click(cx.listener(|this, _, _, cx| this.toggle_menu(Menu::TargetHost, cx)))
                .children(host_menu)
                .into_any_element()
        } else {
            div()
                .flex()
                .flex_row()
                .items_center()
                .gap_1()
                .h(px(26.))
                .px_2()
                .rounded_md()
                .bg(rgba(theme::t().raised))
                .border_1()
                .border_color(rgba(theme::t().line))
                .text_size(px(12.))
                .child(div().text_color(rgba(theme::t().fg_1)).child("on"))
                .child(div().font_weight(FontWeight::MEDIUM).child(t.host.clone()))
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
            .on_click(cx.listener(|this, _, _, cx| this.toggle_menu(Menu::Target, cx)))
            .children(target_menu),
        )
        .children(host_chip);

    let card = div().w_full().max_w(px(theme::COMPOSER)).flex().flex_col().gap_3().child(where_row).child(composer(this, cx));
    // Sits a little above true center so it reads as the page's subject, not its footer.
    pane.child(header(div().child("New thread"), None))
        .child(div().flex_1().flex().flex_col().items_center().justify_center().px_6().pb(px(80.)).child(card))
}

/// The composer card: text on top, chips (model / effort / context) and the send or stop
/// button in the footer. Sized by its parent (reading column or draft page).
fn composer(this: &Dirigent, cx: &mut Context<Dirigent>) -> Div {
    let running = this.is_running();
    let has_text = !this.composer.read(cx).text().trim().is_empty();
    let drafting = matches!(this.pane, Pane::Draft { .. });
    let no_target = matches!(this.pane, Pane::Draft { target: None });

    let model_label = this.model.as_deref().map(|m| theme::short_model(m).to_owned()).unwrap_or_else(|| "Model".into());
    let model_open = matches!(this.menu, Some(Menu::Model | Menu::AllModels));
    let model_menu = model_open.then(|| picker::menu(this, no_target, cx));
    let effort_label = this.effort.clone().unwrap_or_else(|| "Effort".into());
    let effort_open = this.menu == Some(Menu::Effort);
    let effort_menu = effort_open.then(|| {
        let levels = this.current_model().map(|m| m.thinking_levels.clone()).unwrap_or_default();
        let mut items = vec![];
        for l in levels {
            let selected = this.effort.as_deref() == Some(&l);
            let level = l.clone();
            items.push(menu::item(format!("e/{l}"), l, "", selected, move |this, cx| this.set_effort(level.clone(), cx), cx));
        }
        if items.is_empty() {
            items.push(menu::header("Pick a model first"));
        }
        menu::popup(Side::Above, items, cx)
    });

    // The context chip: tokens in the model's window right now; hovering it shows what the
    // whole thread has cost so far (`stats_card`).
    let usage = (!drafting).then(|| this.open().and_then(|t| context_usage(t).map(|u| (t, u)))).flatten().map(|(t, used)| {
        let window = this.current_model().map(|m| m.context_window).unwrap_or(0);
        let label = if window > 0 { format!("{}/{}", kilo(used), kilo(window)) } else { kilo(used) };
        div()
            .id("ctx-usage")
            .relative()
            .h(px(24.))
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

    // One round button in the corner: send, or stop while running. While running, ctrl+Enter
    // steers and shift+Enter queues a follow-up (see `input::Submitted`); Esc also stops.
    let action = if running {
        div()
            .id("stop")
            .size(px(28.))
            .flex()
            .items_center()
            .justify_center()
            .rounded_full()
            .cursor_pointer()
            // Soft red, the same deep-fill / light-icon pairing as send.
            .bg(rgba(theme::t().danger))
            .hover(|d| d.bg(rgba(theme::t().danger_active)))
            .on_click(cx.listener(|this, _, _, cx| this.stop(cx)))
            .child(icon("square", theme::t().danger_ink).size(px(11.)))
    } else {
        let ready = has_text && this.model.is_some() && !no_target;
        div()
            .id("send")
            .size(px(28.))
            .flex()
            .items_center()
            .justify_center()
            .rounded_full()
            .cursor_pointer()
            .bg(rgba(if ready { theme::t().accent } else { theme::t().hover }))
            .hover(|d| d.bg(rgba(if ready { theme::t().accent_active } else { theme::t().line })))
            .on_click(cx.listener(|this, _, window, cx| {
                this.composer.update(cx, |_, cx| cx.emit(crate::input::Submitted { queued: false }));
                window.focus(&this.composer.focus_handle(cx), cx);
            }))
            .child(icon("arrow-up", if ready { 0x9fb6e8ff } else { theme::t().fg_2 }))
    };

    div()
        .relative()
        .w_full()
        .max_w(px(theme::COMPOSER))
        .flex()
        .flex_col()
        .rounded(px(CORNER))
        .bg(rgba(theme::t().raised))
        // No border of its own: the glow rings are the only edge, so it stays soft.
        .border_1()
        .border_color(gpui::transparent_black())
        .shadow_md()
        .children(glow())
        .child(div().px_4().pt_4().pb_3().text_size(px(14.)).line_height(px(22.)).child(this.composer.clone()))
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
                    theme::chip("model-chip", model_label, this.model.is_none(), model_open)
                        .on_click(cx.listener(|this, _, _, cx| this.toggle_menu(Menu::Model, cx)))
                        .children(model_menu),
                )
                .child(
                    theme::chip("effort-chip", effort_label, this.effort.is_none(), effort_open)
                        .on_click(cx.listener(|this, _, _, cx| this.toggle_menu(Menu::Effort, cx)))
                        .children(effort_menu),
                )
                .children(usage)
                .child(div().flex_1())
                .child(action),
        )
}

/// One `ask_user` call as a stack of cards, nine tenths of the composer's width. The
/// question being answered is a card on top: each option on its own row (a circle for one
/// choice, a square for several) and, where the question takes typed text, a row holding
/// the text field. The other questions are collapsed to one-line rows in their order:
/// answered ones above the card with a check and their answer, the rest below it, each
/// a step further in and fainter, like a pile of notifications. Clicking a row opens it.
/// `Next` (Enter in the field too) opens the next unanswered question; once every one
/// has an answer `Answer` takes its place. `Dismiss` is always there, in the header.
/// Goes grey once sent, until the host confirms and the card leaves.
fn ask_card(form: &AskForm, cx: &mut Context<Dirigent>) -> Div {
    let t = theme::t();
    let ask_id = form.ask.id.clone();
    let (qi, q) = (form.current, &form.ask.questions[form.current]);
    let complete = form.complete(cx);
    let answers = form.answers(cx);

    // A collapsed question: its text, and its answer once it has one.
    let collapsed = |i: usize, depth: usize| {
        let question = &form.ask.questions[i];
        let answer = answers[i].join(", ");
        let done = !answer.is_empty();
        let ask_id = ask_id.clone();
        div()
            .id(ElementId::Name(format!("ask/{}/row/{i}", form.ask.id).into()))
            .h(px(24.))
            .ml(px(6. * depth as f32))
            .px_2()
            .flex()
            .flex_row()
            .items_center()
            .gap_2()
            .rounded_md()
            .cursor_pointer()
            .bg(rgba(t.hover))
            .opacity(1. - 0.15 * depth as f32)
            .text_size(px(12.))
            .hover(move |d| d.bg(rgba(t.line)).opacity(1.))
            .on_click(cx.listener(move |this, _, _, cx| this.show_ask_tab(&ask_id, i, cx)))
            .child(if done {
                icon("check", t.done).size(px(11.)).into_any_element()
            } else {
                div().size(px(11.)).flex_shrink_0().rounded_full().border_1().border_color(rgba(t.fg_2)).into_any_element()
            })
            .child(div().min_w_0().truncate().text_color(rgba(if done { t.fg_1 } else { t.fg })).child(question.text.clone()))
            .when(done, |d| {
                d.child(div().flex_1()).child(div().flex_shrink_0().max_w(relative(0.5)).truncate().text_color(rgba(t.fg)).child(answer))
            })
    };
    let above: Vec<_> = (0..qi).map(|i| collapsed(i, 0)).collect();
    let below: Vec<_> = (qi + 1..form.ask.questions.len()).enumerate().map(|(k, i)| collapsed(i, k)).collect();

    // An option row: the mark (filled while picked), then the label or the text field.
    let option_row = |id: ElementId, on: bool, multi: bool, body: AnyElement| {
        let mark = div()
            .size(px(11.))
            .flex_shrink_0()
            .when(multi, |d| d.rounded(px(2.)))
            .when(!multi, |d| d.rounded_full())
            .border_1()
            .border_color(rgba(if on { t.accent_line } else { t.fg_2 }))
            .bg(rgba(if on { t.accent_line } else { 0 }));
        div()
            .id(id)
            .w_full()
            .h(px(26.))
            .px_2()
            .flex()
            .flex_row()
            .items_center()
            .gap_2()
            .rounded_md()
            .cursor_pointer()
            .text_size(px(12.5))
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
            div().child(o.clone()).into_any_element(),
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

    // `Next` while another question waits for an answer; `Answer` once none does.
    let next = form.next_unanswered(cx).is_some();
    let next_id = ask_id.clone();
    let action = (complete || next).then(|| {
        div()
            .id(ElementId::Name(format!("ask-next/{ask_id}").into()))
            .h(px(24.))
            .px_2p5()
            .flex()
            .items_center()
            .gap_1()
            .rounded_md()
            .cursor_pointer()
            .text_size(px(12.))
            .bg(rgba(if complete { t.accent } else { t.hover }))
            .text_color(rgba(t.fg))
            .font_weight(FontWeight::MEDIUM)
            .hover(move |d| d.bg(rgba(if complete { t.accent_active } else { t.line })))
            .on_click(cx.listener(move |this, _, _, cx| this.next_ask_tab(&next_id, cx)))
            .child(if complete { "Answer" } else { "Next" })
            .when(!complete, |d| d.child(icon("chevron-right", t.fg).size(px(11.))))
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
        .child(div().px_2().text_size(px(13.)).line_height(px(19.)).child(q.text.clone()))
        .child(div().flex().flex_col().gap_0p5().children(options).children(custom))
        .child(
            div()
                .flex()
                .flex_row()
                .items_center()
                .justify_end()
                .child(div().text_size(px(11.5)).text_color(rgba(t.fg_2)).child(format!("{} of {}", qi + 1, form.ask.questions.len())))
                .child(div().flex_1())
                .children(action),
        );

    let dismiss_id = ask_id.clone();
    let dismiss = theme::button(format!("ask-dismiss/{ask_id}"), "x", "Dismiss")
        .on_click(cx.listener(move |this, _, _, cx| this.dismiss_ask(&dismiss_id, cx)));

    div()
        .w_full()
        .max_w(px(theme::COMPOSER * 0.9))
        .flex()
        .flex_col()
        .gap_1()
        .px_3()
        .pt_2()
        .pb_2p5()
        .rounded(px(CORNER))
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
                .text_size(px(12.))
                .text_color(rgba(t.fg_1))
                .child(icon("message-circle-question", t.warn))
                .child(div().flex_1().child("The agent asks"))
                .child(dismiss),
        )
        .children(above)
        .child(card)
        .children(below)
}

/// Thumbnails of the images referenced from the composer text, under it. Click one to see
/// it full-size; the \u2715 (on hover) drops it and its `[image-N]` token.
fn attachment_strip(this: &Dirigent, cx: &mut Context<Dirigent>) -> Option<Div> {
    let attachments = this.attachments(cx);
    if attachments.is_empty() {
        return None;
    }
    let thumbs = attachments.into_iter().map(|a| {
        let (n, image) = (a.n, a.image);
        let remove = div()
            .id(ElementId::NamedInteger("att-rm".into(), n as u64))
            .absolute()
            .top(px(-6.))
            .right(px(-6.))
            .size(px(18.))
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
            .child(icon("x", theme::t().fg).size(px(11.)));
        thumb_box()
            .id(ElementId::NamedInteger("att".into(), n as u64))
            .group("att")
            .border_1()
            .border_color(rgba(theme::t().line))
            .cursor_pointer()
            .hover(|d| d.border_color(rgba(theme::t().accent_line)))
            .on_click(cx.listener({
                let image = image.clone();
                move |this, _, _, cx| this.set_preview(Some(image.clone()), cx)
            }))
            .child(thumb(&image))
            .child(remove)
    });
    Some(div().flex().flex_row().flex_wrap().gap_3().px_4().pt_1().pb_3().children(thumbs))
}

/// Thumbnail height (boxes are 3:2) in the composer strip and under user bubbles.
const THUMB: f32 = 64.;

/// The 3:2 thumbnail box; `thumb` goes inside, centered.
fn thumb_box() -> Div {
    div().relative().flex_shrink_0().flex().items_center().justify_center().w(px(THUMB * 1.5)).h(px(THUMB)).rounded_md()
}

/// An image scaled down to fit the thumbnail box. Not `size_full`: `img` imposes the
/// bitmap's aspect ratio on its layout, which overflows a 3:2 box for portrait images.
/// Left at its natural size (both dimensions definite, so the ratio no longer applies)
/// and clamped to the box, `Contain` then fits the bitmap inside the clamped bounds.
fn thumb(image: &Arc<gpui::Image>) -> gpui::Img {
    img(image.clone()).max_w_full().max_h_full().rounded_md().object_fit(ObjectFit::Contain)
}

/// An image full-size over the whole window, on a dark scrim. Any click (or Esc) closes it.
pub fn preview(image: Arc<gpui::Image>, cx: &mut Context<Dirigent>) -> Stateful<Div> {
    div()
        .id("preview")
        .absolute()
        .inset_0()
        .flex()
        .items_center()
        .justify_center()
        .bg(rgba(0x000000d0))
        .cursor_pointer()
        .on_click(cx.listener(|this, _, _, cx| this.set_preview(None, cx)))
        .child(img(image).max_w(relative(0.9)).max_h(relative(0.9)).rounded_md().object_fit(ObjectFit::Contain))
}

/// Composer corner radius; the glow rings follow it.
const CORNER: f32 = 12.;
/// Horizontal padding of a user bubble; it overhangs the column by the same amount.
const BUBBLE_PAD: f32 = 16.;

/// A soft glow: three 1px navy rings stepping out from the card, each fainter than the
/// last, with a 1px gap between rings. Absolute, so they take no space and no clicks.
fn glow() -> impl Iterator<Item = Div> {
    [0x60, 0x30, 0x14].into_iter().enumerate().map(|(i, alpha)| {
        let out = 2. * (i + 1) as f32;
        div()
            .absolute()
            .inset(px(-out))
            .rounded(px(CORNER + out))
            .border_1()
            .border_color(rgba(theme::alpha(theme::t().accent_line, alpha)))
    })
}

/// Context tokens as pi computes them: usage of the last non-error assistant message.
fn context_usage(t: &OpenThread) -> Option<u64> {
    t.entries.iter().rev().find_map(|e| {
        let msg = e.json.get("message")?;
        if msg.get("role")?.as_str()? != "assistant" || matches!(msg.get("stopReason").and_then(Value::as_str), Some("error" | "aborted")) {
            return None;
        }
        let u = msg.get("usage")?;
        let n = |k: &str| u.get(k).and_then(Value::as_u64).unwrap_or(0);
        let total = n("totalTokens");
        let total = if total > 0 { total } else { n("input") + n("output") + n("cacheRead") + n("cacheWrite") };
        (total > 0).then_some(total)
    })
}

/// The thread's totals, floated above the context chip while it is hovered: tokens in
/// and out, how much of the input was cached, message counts, and the bill so far.
fn stats_card(t: &OpenThread) -> AnyElement {
    let s = Stats::of(t.entries.iter().map(|e| &e.json));
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
        .text_size(px(12.))
        .line_height(px(18.))
        .text_color(rgba(theme::t().fg))
        .child(format!("{} in / {} out", kilo(s.input), kilo(s.output)))
        .child(dim(format!("{} cached ({}%)", kilo(s.cached), s.cached_pct())))
        .child(dim(count(s.user_messages, "user message")))
        .child(dim(count(s.assistant_messages, "assistant message")))
        .child(dim(count(s.compactions, "compaction")))
        .child(div().pt_0p5().font_weight(FontWeight::MEDIUM).child(dollars(s.cost)));
    // Zero-size absolute wrapper so the anchored card sits at the chip's corner, not the
    // flex static position (see AGENTS.md).
    div()
        .absolute()
        .top_0()
        .left_0()
        .size_0()
        .child(deferred(
            anchored().anchor(Anchor::BottomLeft).offset(point(px(0.), px(-6.))).snap_to_window_with_margin(px(8.)).child(card),
        ))
        .into_any()
}

/// One list item: a timeline item, or (last index) the live tail: the optimistic echoes of
/// sent messages (prompts above the work in progress, follow-ups below it), the work and
/// its latest prose, then the steers pi still holds. Everything sits in the reading column.
/// Also what `Dirigent::copy_selection` calls to collect an item's text.
pub fn render_item(this: &mut Dirigent, ix: usize, cx: &mut Context<Dirigent>) -> AnyElement {
    let Pane::Thread(t) = &this.pane else { return div().into_any() };
    let live = live_work(this);
    let now = now_ms();
    let fonts = this.fonts.clone();
    let blobs = &this.blobs;
    let keys = &mut Keys::new(&this.selection, Doc::Thread, ix);
    let mut col = div().w_full().max_w(px(theme::COLUMN)).flex().flex_col();
    if ix < t.timeline.items.len() {
        if live.as_ref().is_none_or(|(k, _)| *k != ix) {
            col = col.child(render_timeline_item(&t.timeline.items[ix], ix, t, blobs, now, &fonts, keys, cx));
        }
    } else {
        // Echoes show their text only; the thumbnails come with the persisted entry.
        let echo = |mode| t.streaming.echo.iter().filter(move |(_, m)| *m == mode).map(|(text, _)| text.as_str()).collect::<Vec<_>>();
        for text in echo(PromptMode::Prompt) {
            col = col.child(user_bubble(text, &[], TAIL, t, &fonts, keys, cx));
        }
        if let Some((key, work)) = live {
            col = col.child(work_block(&work, key, true, t, now, &fonts, keys, cx));
            // Streamed text is already cut at its last completed line (`live_work`).
            if let Some(text) = work.latest_text().filter(|r| !r.trim().is_empty()) {
                col = col.child(reply_block(text, TAIL, t, &fonts, keys, cx));
            }
            // Pending steers last: the newest user input, closest to the composer.
            for (_, text, _) in work.steers().filter(|(_, _, queued)| *queued) {
                col = col.child(steer_bubble(text, true, t, &fonts, keys, cx));
            }
        }
        for text in echo(PromptMode::FollowUp) {
            col = col.child(user_bubble(text, &[], TAIL, t, &fonts, keys, cx));
        }
        col = col.pb_6();
    }
    if ix == 0 {
        col = col.pt_4();
    }
    div().w_full().flex().flex_col().items_center().px_6().child(col).into_any()
}

fn now_ms() -> i64 {
    std::time::UNIX_EPOCH.elapsed().map(|d| d.as_millis() as i64).unwrap_or(0)
}

/// When the current run started: the last user message, else now.
fn last_prompt_at(t: &OpenThread, now: i64) -> i64 {
    t.timeline.items.iter().rev().find_map(|i| if let Item::User { at, .. } = i { Some(*at) } else { None }).unwrap_or(now)
}

/// The run in progress, keyed for `expanded` / `log`: the open work's persisted steps plus
/// whatever is streaming right now (text only up to its last completed line). A just-sent
/// message (the echo) counts as running so the work line and the log panel appear at once
/// rather than when the host confirms the run.
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
            // An echoed message is the run's start until its persisted entry lands.
            let started = if t.streaming.echo.is_empty() { last_prompt_at(t, now_ms()) } else { now_ms() };
            (TAIL, Work { model: this.model.clone(), effort: this.effort.clone(), started, ..Default::default() })
        }
    };
    // Steers pi still holds (typed, or answers on their way) show as queued steps of the run.
    work.steps.extend(t.streaming.queued_steers().map(|text| Step::Steer { text: text.to_owned(), queued: true }));
    work.steps.extend(t.streaming.answers.iter().map(|(_, text)| Step::Steer { text: text.clone(), queued: true }));
    work.steps.extend(t.streaming.steps.iter().filter_map(|s| match s {
        Step::Text(x) => x.contains('\n').then(|| Step::Text(markdown::committed(x).to_owned())),
        Step::Thinking(x) => x.contains('\n').then(|| Step::Thinking(markdown::committed(x).to_owned())),
        tool => Some(tool.clone()),
    }));
    Some((key, work))
}

#[allow(clippy::too_many_arguments)]
fn render_timeline_item(
    item: &Item,
    ix: usize,
    t: &OpenThread,
    blobs: &HashMap<String, Arc<gpui::Image>>,
    now: i64,
    fonts: &Fonts,
    keys: &mut Keys,
    cx: &mut Context<Dirigent>,
) -> AnyElement {
    match item {
        Item::User { text, images, .. } => {
            let images: Vec<_> = images.iter().map(|id| blobs.get(id).cloned()).collect();
            user_bubble(text, &images, ix, t, fonts, keys, cx).into_any()
        }
        Item::Work(w) => work_block(w, ix, false, t, now, fonts, keys, cx).into_any(),
        Item::Assistant(text) => reply_block(text, ix, t, fonts, keys, cx).into_any(),
        // Same voice as a tool row: one quiet line, just red.
        Item::Error(text) => div()
            .w_full()
            .py(px(2.))
            .text_size(px(STEP_SIZE))
            .line_height(px(19.))
            .text_color(rgba(theme::t().error))
            .child(keys.plain(text.clone()))
            .into_any(),
        Item::Compaction => notice_line(keys.plain("context compacted".to_owned())),
        Item::Notice(text) => notice_line(keys.plain(text.clone())),
    }
}

/// A quiet one-liner between messages (compaction, warnings, unknown entries).
fn notice_line(text: impl IntoElement) -> AnyElement {
    div().w_full().py_1().text_size(px(12.)).text_color(rgba(theme::t().fg_2)).child(text).into_any()
}

fn reply_block(text: &str, ix: usize, t: &OpenThread, fonts: &Fonts, keys: &mut Keys, cx: &mut Context<Dirigent>) -> Div {
    div()
        .group("msg")
        .relative()
        .w_full()
        .py_1()
        .font_family(fonts.prose.clone())
        .text_size(px(14.))
        .line_height(px(22.))
        .child(markdown::render(text, &fonts.mono, keys))
        .children(pin_bar(ix, t, BUBBLE_PAD + PIN_GAP))
        .children(corner_buttons(ix, text, t, cx))
}

/// How far left of a message's box its pin bar sits.
const PIN_GAP: f32 = 6.;

/// A pinned message's amber bar in the left margin, `left` px before the block's edge
/// (so bubbles and replies line up despite the bubble's overhang).
fn pin_bar(ix: usize, t: &OpenThread, left: f32) -> Option<Div> {
    t.pins.contains(&ix).then(|| div().absolute().left(px(-left)).top_1().bottom_1().w(px(2.)).rounded_full().bg(rgba(theme::t().warn)))
}

/// `pin` and `copy` in a message's top right corner, revealed on hover. Copy puts the
/// message's markdown on the clipboard and shows a check for a moment; pin marks the
/// message in the ruler and with a bar in the margin (the pin is amber while pinned).
/// Not for the live tail (`TAIL`).
fn corner_buttons(ix: usize, text: &str, t: &OpenThread, cx: &mut Context<Dirigent>) -> Option<Div> {
    if ix == TAIL {
        return None;
    }
    let (copied, pinned) = (t.copied == Some(ix), t.pins.contains(&ix));
    let text = text.to_owned();
    let button = |b: Stateful<Div>| b.bg(rgba(theme::t().raised)).border_1().border_color(rgba(theme::t().line));
    let tint = if pinned { theme::t().warn } else { theme::t().fg_1 };
    let pin = button(theme::tinted_icon_button(("pin", ix), "pin", tint, cx, move |this, cx| this.toggle_pin(ix, cx)));
    let copy = button(theme::icon_button(("copy", ix), if copied { "check" } else { "copy" }, cx, move |this, cx| {
        this.copy_item(ix, text.clone(), cx)
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
            .child(pin)
            .child(copy),
    )
}

/// The bubble bleeds past the column by its own padding, so its text lines up with
/// replies and work lines instead of being inset. Attached images go under the text as
/// thumbnails (a blank box while the bytes are on their way); click one to see it.
/// `ix` is the bubble's timeline index: thumbnail ids must be unique across the thread,
/// since gpui shares click state between same-id elements and drops clicks on the others.
#[allow(clippy::too_many_arguments)]
fn user_bubble(
    text: &str,
    images: &[Option<Arc<gpui::Image>>],
    ix: usize,
    t: &OpenThread,
    fonts: &Fonts,
    keys: &mut Keys,
    cx: &mut Context<Dirigent>,
) -> Div {
    let thumbs = (!images.is_empty()).then(|| {
        div().flex().flex_row().flex_wrap().gap_3().pt_3().children(images.iter().enumerate().map(|(i, image)| {
            let id = ElementId::NamedInteger("bubble-img".into(), (ix as u64) << 8 | i as u64);
            let box_ = thumb_box().id(id).border_1().border_color(rgba(theme::t().accent_line));
            match image {
                Some(image) => box_
                    .cursor_pointer()
                    .hover(|d| d.border_color(rgba(theme::t().accent_ink)))
                    .on_click(cx.listener({
                        let image = image.clone();
                        move |this, _, _, cx| this.set_preview(Some(image.clone()), cx)
                    }))
                    .child(thumb(image)),
                None => box_.bg(rgba(theme::t().accent)),
            }
        }))
    });
    div()
        .group("msg")
        .relative()
        .mx(px(-BUBBLE_PAD))
        .mt_4()
        .mb_3()
        .px(px(BUBBLE_PAD))
        .py_3()
        .rounded_lg()
        // A shade under the send button's accent, so bubbles sit back from the chrome.
        .bg(rgba(theme::t().bubble))
        .font_family(fonts.prose.clone())
        .text_size(px(14.))
        .line_height(px(22.))
        .child(markdown::render(text, &fonts.mono, keys))
        .children(thumbs)
        .children(pin_bar(ix, t, PIN_GAP))
        .children(corner_buttons(ix, text, t, cx).map(|b| b.top_2().right_2()))
}

/// A turn's work in the transcript: the steers the model has already taken (user bubbles,
/// kept after the run) and then the work line. Pending steers are rendered by the tail.
#[allow(clippy::too_many_arguments)]
fn work_block(
    w: &Work,
    key: usize,
    live: bool,
    t: &OpenThread,
    now: i64,
    fonts: &Fonts,
    keys: &mut Keys,
    cx: &mut Context<Dirigent>,
) -> Div {
    let taken: Vec<_> =
        w.steers().filter(|(_, _, queued)| !queued).map(|(_, text, _)| steer_bubble(text, false, t, fonts, keys, cx)).collect();
    div().w_full().flex().flex_col().children(taken).child(work_line(w, key, live, t, now, cx))
}

/// A steer as a user bubble with a `steer` label; `pending` (and `cancel` on hover, for a
/// typed steer pi still holds) at the right until the model has it. Answers to an ask are
/// steers too, but can't be cancelled: the host already has them.
fn steer_bubble(text: &str, pending: bool, t: &OpenThread, fonts: &Fonts, keys: &mut Keys, cx: &mut Context<Dirigent>) -> Div {
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
        .text_size(px(11.5))
        .line_height(px(14.))
        .child(div().text_color(rgba(theme::t().accent_ink)).child("steer"))
        .child(div().flex_1())
        .children(cancel)
        .when(pending, |d| d.child(div().text_color(rgba(theme::t().fg_2)).child("pending")));
    div()
        .group("msg")
        .relative()
        .mx(px(-BUBBLE_PAD))
        .my_2()
        .px(px(BUBBLE_PAD))
        .py_2p5()
        .rounded_lg()
        .bg(rgba(theme::t().bubble))
        .when(pending, |d| d.opacity(0.7))
        .font_family(fonts.prose.clone())
        .text_size(px(14.))
        .line_height(px(22.))
        .child(label)
        .child(markdown::render(text, &fonts.mono, keys))
}

/// A steer in the work log: `steer` tinted, the message, and `queued` at the right (where a
/// tool's time sits) until pi hands it to the model, with `cancel` revealed on hover just
/// before it (typed steers only; see `steer_bubble`).
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
        .py(px(2.))
        .text_size(px(STEP_SIZE))
        .line_height(px(19.))
        .child(div().flex_shrink_0().text_color(rgba(theme::t().accent_ink)).child("steer"))
        .child(div().flex_1().min_w_0().text_color(rgba(theme::t().fg_1)).child(keys.plain(text.to_owned())))
        // `cancel` sits left of `queued` so `queued` stays flush right while it's invisible.
        .children(cancel)
        .when(queued, |d| d.child(div().flex_shrink_0().text_color(rgba(theme::t().fg_2)).child("queued")))
}

/// A turn's work as one line: `› model effort · 1m 23s · 4 tool calls`, plus what it is
/// doing right now while `live` (`· bash 3.2s`). Click to show its log in the panel;
/// tinted while the panel shows it.
fn work_line(w: &Work, key: usize, live: bool, t: &OpenThread, now: i64, cx: &mut Context<Dirigent>) -> AnyElement {
    let shown = t.log == Some(key);
    let tools = match w.tool_count() {
        0 => "no tool calls".to_owned(),
        1 => "1 tool call".to_owned(),
        n => format!("{n} tool calls"),
    };
    let activity = live.then(|| t.streaming.activity(&w.steps, now).unwrap_or_else(|| "waiting".into()));
    let dot = || div().text_color(rgba(theme::t().fg_2)).child("·");
    row(step_id(key, usize::MAX))
        .my_1()
        .gap_2()
        .text_size(px(STEP_SIZE))
        .when(shown, |d| d.bg(rgba(theme::t().raised)))
        .on_click(cx.listener(move |this, _, _, cx| this.toggle_log(key, cx)))
        .child(div().text_color(rgba(theme::t().fg)).child(w.model.as_deref().map(theme::short_model).unwrap_or("model").to_owned()))
        .children(w.effort.clone().map(|e| div().text_color(rgba(theme::t().accent_ink)).child(e)))
        .child(dot())
        .child(div().text_color(rgba(theme::t().fg_1)).child(if live { elapsed(now - w.started) } else { duration(w.duration(now)) }))
        .child(dot())
        .child(div().text_color(rgba(theme::t().fg_1)).child(tools))
        .children(activity.map(|a| {
            div()
                .flex()
                .flex_row()
                .gap_2()
                .min_w_0()
                .child(dot())
                .child(div().min_w_0().truncate().text_color(rgba(theme::t().working)).child(a))
        }))
        .into_any()
}

/// The work the log panel shows (`t.log`) and whether it is the run in progress.
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

/// The panel on the pane's right with the full log of the work `t.log` points at, split
/// off by a hairline with a drag handle: a lazy `list()` of its steps (`render_step`),
/// one mono size; tool rows open to their arguments and output. Showing the run in
/// progress it follows the latest step unless the user scrolled up (`FollowMode::Tail`).
/// The list's item count is reconciled here, so every path that changes the steps
/// (events, entries, a new prompt) is covered; those paths remeasure the items.
fn log_panel(this: &Dirigent, cx: &mut Context<Dirigent>) -> Option<Div> {
    let Pane::Thread(t) = &this.pane else { return None };
    let key = t.log?;
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
    let title = div()
        .flex()
        .flex_row()
        .items_center()
        .h(px(32.))
        .pl_4()
        .pr_2()
        .flex_shrink_0()
        .text_size(px(12.))
        .text_color(rgba(theme::t().fg_2))
        .child(div().flex_1().child("Work log"))
        .child(theme::icon_button("log-close", "x", cx, move |this, cx| this.toggle_log(key, cx)));
    let handle = div()
        .id("log-resize")
        .absolute()
        .top_0()
        .left(px(-3.))
        .w(px(6.))
        .h_full()
        .cursor_col_resize()
        .hover(|d| d.bg(rgba(theme::t().line)))
        .on_mouse_down(MouseButton::Left, cx.listener(|this, _, _, cx| this.start_resize(Resize::Log, cx)));
    Some(
        div()
            .relative()
            .w(px(this.log_width))
            .flex_shrink_0()
            .h_full()
            .border_l_1()
            .border_color(rgba(theme::t().line_soft))
            .flex()
            .flex_col()
            .child(title)
            .child(steps.flex_1().w_full().text_size(px(STEP_SIZE)).line_height(px(19.)))
            .child(handle),
    )
}

/// One step of the shown work as a log list item; a placeholder while it has none.
/// Also what `Dirigent::copy_selection` calls to collect a step's text.
pub fn render_step(this: &mut Dirigent, work: &Work, live: bool, six: usize, cx: &mut Context<Dirigent>) -> AnyElement {
    let Pane::Thread(t) = &this.pane else { return div().into_any() };
    let Some(key) = t.log else { return div().into_any() };
    let Some(step) = work.steps.get(six) else {
        let text = if live { "Waiting for the model" } else { "No steps" };
        return div().h(px(ROW_H)).flex().items_center().text_color(rgba(theme::t().fg_2)).child(text).into_any();
    };
    let mono = &this.fonts.mono;
    let keys = &mut Keys::new(&this.selection, Doc::Log, six);
    match step {
        Step::Thinking(text) => div().py_0p5().text_color(rgba(theme::t().fg_1)).child(markdown::render(text, mono, keys)).into_any(),
        Step::Text(text) => div().py_0p5().text_color(rgba(theme::t().fg)).child(markdown::render(text, mono, keys)).into_any(),
        Step::Tool(call) => render_tool(call, (key, six), t, now_ms(), keys, cx),
        Step::Steer { text, queued } => steer_row((key, six), text, *queued, t, keys, cx).into_any(),
        // A job notice in the row voice: `job` where a tool's name goes, red when it failed.
        Step::Notice(text) => {
            let failed = text.contains("exit ") && !text.contains("exit 0");
            div()
                .h(px(ROW_H))
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

/// One tool call on one line: tinted name, primary argument, and its time at the right —
/// `12s`, `1.3s / 30s` against the timeout while running, red when it failed. Click for
/// all arguments and the output.
fn render_tool(call: &ToolCall, key: (usize, usize), t: &OpenThread, now: i64, keys: &mut Keys, cx: &mut Context<Dirigent>) -> AnyElement {
    let expanded = t.expanded.contains(&key);
    let failed = call.result.as_ref().is_some_and(|r| r.is_error);
    let (time, color) = match (t.streaming.running_since(&call.id), call.started.zip(call.ended)) {
        (Some(since), _) => {
            let so_far = elapsed(now - since);
            (Some(call.timeout.map(|s| format!("{so_far} / {}", duration((s * 1000.) as i64))).unwrap_or(so_far)), theme::t().working)
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
        }
        b
    });
    div().w_full().flex().flex_col().child(line).children(body).into_any()
}

/// A clickable one-liner in the work log. Bleeds 4px past the column so the hover
/// highlight frames the text without shifting it.
fn row(id: ElementId) -> gpui::Stateful<Div> {
    div()
        .id(id)
        .h(px(ROW_H))
        .mx(px(-4.))
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

/// A finished span: `34ms` under a tenth of a second, `0.34s` / `0.1s` under a second,
/// else whole seconds (`12s`, `1m 23s`, `1h 05m`).
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

/// A span still running: whole seconds only (`0s`, `12s`, `1m 23s`, `1h 05m`), so it
/// advances exactly once per `tick` instead of jittering with every event.
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
