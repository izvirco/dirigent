use crate::app::{Dirigent, Menu, now_ms};
use crate::icons::icon;
use crate::menu::{self, ROW_H, Side};
use crate::theme::{self, text, u};
use gpui::{AnyElement, Context, Div, ElementId, div, prelude::*, rgba};

const TTL_MS: i64 = 8000;
const MAX_IN_CORNER: usize = 3;
const CAP: usize = 100;
const WIDTH: f32 = 420.;
const LINE: f32 = 20.;
const COUNT_AND_AGE_MIN_WIDTH: f32 = 56.;

#[derive(Clone, Debug, PartialEq)]
pub struct ThreadRef {
    pub host: String,
    pub id: String,
}

pub struct Toast {
    pub text: String,
    pub thread: Option<ThreadRef>,
    pub count: u32,
    pub last_unix_ms: i64,
    pub dismissed: bool,
}

#[derive(Default)]
pub struct Toasts {
    list: Vec<Toast>,
    pub unseen: bool,
}

impl Toasts {
    pub fn push(&mut self, text: String, thread: Option<ThreadRef>, now_ms: i64) {
        let count = match self.list.iter().position(|toast| toast.text == text) {
            Some(i) => self.list.remove(i).count + 1,
            None => 1,
        };
        self.list.push(Toast { text, thread, count, last_unix_ms: now_ms, dismissed: false });
        if self.list.len() > CAP {
            self.list.remove(0);
        }
    }

    pub fn remove(&mut self, text: &str) {
        self.list.retain(|toast| toast.text != text);
    }

    pub fn dismiss(&mut self, text: &str) {
        if let Some(toast) = self.list.iter_mut().find(|toast| toast.text == text) {
            toast.dismissed = true;
        }
    }

    pub fn corner(&self, now_ms: i64) -> Vec<&Toast> {
        let fresh = |toast: &&Toast| !toast.dismissed && now_ms - toast.last_unix_ms < TTL_MS;
        let mut shown: Vec<&Toast> = self.list.iter().rev().filter(fresh).take(MAX_IN_CORNER).collect();
        shown.reverse();
        shown
    }

    pub fn history(&self) -> impl Iterator<Item = &Toast> {
        self.list.iter().rev()
    }

    pub fn is_empty(&self) -> bool {
        self.list.is_empty()
    }
}

impl Dirigent {
    pub fn toast(&mut self, message: impl Into<String>, cx: &mut Context<Self>) {
        self.push_toast(message.into(), None, cx);
    }

    pub fn thread_toast(&mut self, host: &str, thread_id: &str, message: &str, cx: &mut Context<Self>) {
        self.push_toast(format!("{host}: {message}"), Some(ThreadRef { host: host.to_owned(), id: thread_id.to_owned() }), cx);
    }

    fn push_toast(&mut self, text: String, thread: Option<ThreadRef>, cx: &mut Context<Self>) {
        tracing::info!("toast: {text}");
        self.toasts.push(text, thread, now_ms());
        self.toasts.unseen = self.menu != Some(Menu::Toasts);
        cx.notify();
    }

    fn toggle_toast_history(&mut self, cx: &mut Context<Self>) {
        self.toggle_menu(Menu::Toasts, cx);
        if self.menu == Some(Menu::Toasts) {
            self.toasts.unseen = false;
        }
    }

    fn dismiss_toast(&mut self, text: &str, cx: &mut Context<Self>) {
        self.toasts.dismiss(text);
        cx.notify();
    }

    fn remove_toast(&mut self, text: &str, cx: &mut Context<Self>) {
        self.toasts.remove(text);
        if self.toasts.is_empty() {
            self.close_menu(cx);
        }
        cx.notify();
    }

    fn clear_toasts(&mut self, cx: &mut Context<Self>) {
        self.toasts = Toasts::default();
        self.close_menu(cx);
    }
}

fn thread_link(thread: &Option<ThreadRef>, this: &Dirigent, cx: &mut Context<Dirigent>) -> Option<AnyElement> {
    let thread = thread.clone()?;
    let name = this.summary(&thread.host, &thread.id)?.name.clone().unwrap_or_else(|| "Untitled".into());
    let t = theme::t();
    let link = div()
        .id("thread")
        .min_w_0()
        .truncate()
        .text_size(text::sm())
        .text_color(rgba(t.fg_2))
        .cursor_pointer()
        .hover(move |d| d.text_color(rgba(t.fg)))
        .on_click(cx.listener(move |this, _, _, cx| {
            cx.stop_propagation();
            this.show_thread(&thread.host, &thread.id, cx)
        }))
        .child(name);
    Some(div().flex().flex_row().child(link).into_any_element())
}

fn head(toast: &Toast, trailing: Option<Div>, overlay: Div) -> Div {
    div()
        .relative()
        .flex()
        .flex_row()
        .items_start()
        .gap_2()
        .line_height(u(LINE))
        .child(div().flex_1().min_w_0().child(toast.text.clone()))
        .children(trailing)
        .child(overlay)
}

fn count(toast: &Toast) -> Option<Div> {
    (toast.count > 1).then(|| small(format!("×{}", toast.count)))
}

fn small(text: String) -> Div {
    div().flex_shrink_0().text_size(text::sm()).text_color(rgba(theme::t().fg_2)).child(text)
}

struct Actions {
    group: &'static str,
    copy_id: ElementId,
    text: String,
    copied: bool,
    button_hover: u32,
}

impl Actions {
    fn render(self, remove: impl Fn(&mut Dirigent, &mut Context<Dirigent>) + 'static, cx: &mut Context<Dirigent>) -> Div {
        let Actions { group, copy_id, text, copied, button_hover } = self;
        let button = |id: &'static str, name: &'static str| {
            div()
                .id(id)
                .size(u(20.))
                .flex()
                .items_center()
                .justify_center()
                .rounded_sm()
                .cursor_pointer()
                .hover(move |d| d.bg(rgba(button_hover)))
                .child(icon(name, theme::t().fg_1))
        };
        let copy = button("copy", if copied { "check" } else { "copy" }).on_click(cx.listener(move |this, _, _, cx| {
            cx.stop_propagation();
            this.copy_item(copy_id.clone(), text.clone(), cx)
        }));
        let remove = button("remove", "x").on_click(cx.listener(move |this, _, _, cx| {
            cx.stop_propagation();
            remove(this, cx)
        }));
        div()
            .absolute()
            .top_0()
            .right(u(-3.))
            .h(u(LINE))
            .flex()
            .flex_row()
            .items_center()
            .gap_0p5()
            .opacity(if copied { 1. } else { 0. })
            .group_hover(group, |d| d.opacity(1.))
            .child(copy)
            .child(remove)
    }

    fn cover(&self, trailing: Div) -> Div {
        trailing.when(self.copied, |d| d.opacity(0.)).group_hover(self.group, |d| d.opacity(0.))
    }
}

pub fn corner(this: &Dirigent, cx: &mut Context<Dirigent>) -> Option<AnyElement> {
    let shown = this.toasts.corner(now_ms());
    if shown.is_empty() {
        return None;
    }
    let t = theme::t();
    let toasts: Vec<AnyElement> = shown
        .into_iter()
        .map(|toast| {
            let key = toast.text.clone();
            let copy_id = ElementId::Name(format!("toast-copy/{}", toast.text).into());
            let actions = Actions {
                group: "toast",
                copied: this.copied() == Some(&copy_id),
                copy_id,
                text: toast.text.clone(),
                button_hover: t.line,
            };
            let trailing = count(toast).map(|count| actions.cover(count));
            let pair = actions.render(
                {
                    let key = key.clone();
                    move |this, cx| this.dismiss_toast(&key, cx)
                },
                cx,
            );
            div()
                .id(ElementId::Name(format!("toast/{}", toast.text).into()))
                .group("toast")
                .block_mouse_except_scroll()
                .max_w(u(WIDTH))
                .flex()
                .flex_col()
                .gap_0p5()
                .px_3()
                .py_2()
                .rounded_md()
                .bg(rgba(t.popup))
                .border_1()
                .border_color(rgba(t.line))
                .shadow_md()
                .cursor_pointer()
                .on_click(cx.listener(move |this, _, _, cx| this.dismiss_toast(&key, cx)))
                .child(head(toast, trailing, pair.pl_1().bg(rgba(t.popup))))
                .children(thread_link(&toast.thread, this, cx))
                .into_any_element()
        })
        .collect();
    Some(div().absolute().bottom_4().right_4().flex().flex_col().gap_1().children(toasts).into_any_element())
}

pub fn button(this: &Dirigent, cx: &mut Context<Dirigent>) -> Option<AnyElement> {
    if this.toasts.is_empty() {
        return None;
    }
    let t = theme::t();
    let open = this.menu == Some(Menu::Toasts);
    let popup = open.then(|| {
        let now = now_ms();
        let clear =
            menu::header_button("toast-clear", "Clear all", None, t.fg_1).on_click(cx.listener(|this, _, _, cx| this.clear_toasts(cx)));
        let header = div().flex().flex_row().justify_end().items_center().h(u(ROW_H)).flex_shrink_0().px_0p5().child(clear);
        let mut rows = vec![header.into_any_element(), menu::separator()];
        rows.extend(this.toasts.history().map(|toast| row(toast, now, this, cx)));
        menu::popup(Side::Above, rows, cx)
    });
    let dot = this.toasts.unseen.then(|| div().absolute().top(u(4.)).right(u(4.)).size(u(6.)).rounded_full().bg(rgba(t.accent_ink)));
    let button = theme::icon_button("toast-history", "logs", cx, |this, cx| this.toggle_toast_history(cx))
        .when(open, |d| d.bg(rgba(t.hover)))
        .children(dot)
        .children(popup);
    Some(button.into_any_element())
}

fn row(toast: &Toast, now: i64, this: &Dirigent, cx: &mut Context<Dirigent>) -> AnyElement {
    let id = ElementId::Name(format!("toast-history/{}", toast.text).into());
    let actions = Actions {
        group: "toast-history-row",
        copied: this.copied() == Some(&id),
        copy_id: id.clone(),
        text: toast.text.clone(),
        button_hover: theme::t().hover,
    };
    let (copy_id, text, key) = (id.clone(), toast.text.clone(), toast.text.clone());
    let el = div()
        .id(id)
        .group("toast-history-row")
        .w(u(WIDTH))
        .flex_shrink_0()
        .flex()
        .flex_col()
        .gap_0p5()
        .px_2()
        .py_1p5()
        .rounded_md()
        .cursor_pointer();
    let count_and_age = div()
        .min_w(u(COUNT_AND_AGE_MIN_WIDTH))
        .flex()
        .flex_row()
        .justify_end()
        .gap_1p5()
        .children(count(toast))
        .child(small(theme::rel_time(now - toast.last_unix_ms)));
    let trailing = actions.cover(count_and_age);
    menu::stop(el, false, move |this, _, cx| this.copy_item(copy_id.clone(), text.clone(), cx), cx)
        .child(head(toast, Some(trailing), actions.render(move |this, cx| this.remove_toast(&key, cx), cx)))
        .children(thread_link(&toast.thread, this, cx))
        .into_any_element()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn texts<'a>(toasts: impl IntoIterator<Item = &'a Toast>) -> Vec<&'a str> {
        toasts.into_iter().map(|toast| toast.text.as_str()).collect()
    }

    fn thread(id: &str) -> Option<ThreadRef> {
        Some(ThreadRef { host: "local".into(), id: id.into() })
    }

    #[test]
    fn a_repeat_counts_up_and_moves_to_the_end_with_a_fresh_expiry() {
        let mut toasts = Toasts::default();
        toasts.push("a".into(), None, 0);
        toasts.push("b".into(), None, 0);
        toasts.push("a".into(), None, 5000);
        assert_eq!(texts(toasts.history()), ["a", "b"]);
        assert_eq!(texts(toasts.corner(5000)), ["b", "a"]);
        let a = toasts.history().next().unwrap();
        assert_eq!((a.count, a.last_unix_ms), (2, 5000));
        assert_eq!(texts(toasts.corner(9000)), ["a"]);
        assert!(toasts.corner(13000).is_empty());
        assert_eq!(texts(toasts.history()), ["a", "b"]);
    }

    #[test]
    fn dismissed_and_expired_toasts_leave_the_corner_and_stay_in_the_history() {
        let mut toasts = Toasts::default();
        toasts.push("old".into(), None, 0);
        toasts.push("new".into(), None, 4000);
        toasts.dismiss("new");
        assert_eq!(texts(toasts.corner(4000)), ["old"]);
        assert!(toasts.corner(8000).is_empty());
        assert_eq!(texts(toasts.history()), ["new", "old"]);
        toasts.push("old".into(), None, 9000);
        assert_eq!(texts(toasts.corner(9000)), ["old"]);
    }

    #[test]
    fn a_dismissed_toast_comes_back_on_a_repeat() {
        let mut toasts = Toasts::default();
        toasts.push("a".into(), None, 0);
        toasts.dismiss("a");
        assert!(toasts.corner(1000).is_empty());
        toasts.push("a".into(), None, 2000);
        assert_eq!(texts(toasts.corner(2000)), ["a"]);
    }

    #[test]
    fn a_removed_toast_is_forgotten() {
        let mut toasts = Toasts::default();
        toasts.push("a".into(), None, 0);
        toasts.push("a".into(), None, 0);
        toasts.push("b".into(), None, 0);
        toasts.remove("a");
        assert_eq!(texts(toasts.history()), ["b"]);
        assert_eq!(texts(toasts.corner(0)), ["b"]);
        toasts.push("a".into(), None, 1);
        assert_eq!(toasts.history().next().map(|toast| (toast.text.as_str(), toast.count)), Some(("a", 1)));
    }

    #[test]
    fn the_corner_shows_the_three_newest() {
        let mut toasts = Toasts::default();
        for text in ["1", "2", "3", "4", "5"] {
            toasts.push(text.into(), None, 0);
        }
        assert_eq!(texts(toasts.corner(0)), ["3", "4", "5"]);
        toasts.dismiss("5");
        assert_eq!(texts(toasts.corner(0)), ["2", "3", "4"]);
        assert_eq!(toasts.history().count(), 5);
    }

    #[test]
    fn the_cap_drops_the_oldest() {
        let mut toasts = Toasts::default();
        for i in 0..=CAP {
            toasts.push(i.to_string(), None, i as i64);
        }
        let history = texts(toasts.history());
        assert_eq!(history.len(), CAP);
        assert_eq!(history.first(), Some(&CAP.to_string().as_str()));
        assert_eq!(history.last(), Some(&"1"));
    }

    #[test]
    fn a_repeat_at_the_cap_drops_nothing() {
        let mut toasts = Toasts::default();
        for i in 0..CAP {
            toasts.push(i.to_string(), None, i as i64);
        }
        toasts.push("0".into(), None, CAP as i64);
        let history = texts(toasts.history());
        assert_eq!(history.len(), CAP);
        assert_eq!(history.first(), Some(&"0"));
        assert_eq!(history.last(), Some(&"1"));
    }

    #[test]
    fn a_repeat_points_at_where_it_last_came_from() {
        let mut toasts = Toasts::default();
        toasts.push("local: mcp down".into(), thread("t1"), 0);
        toasts.push("local: mcp down".into(), thread("t2"), 1);
        let toast = toasts.history().next().unwrap();
        assert_eq!((toast.count, &toast.thread), (2, &thread("t2")));
        toasts.push("local: mcp down".into(), None, 2);
        assert_eq!(toasts.history().next().unwrap().thread, None);
        assert_eq!(toasts.history().count(), 1);
    }
}
