use gpui::{Context, IntoElement, deferred, div, prelude::*, px, rgb};

use crate::{
    app::{Dirigent, KeyboardMenu},
    theme::{border, faint, muted},
};

const SPACE_ITEMS: &[(&str, &str)] = &[
    ("c", "new thread"),
    ("i", "focus composer"),
    ("a", "add project"),
    ("t", "thread commands…"),
    ("p", "project commands…"),
    ("x", "stop agent"),
    ("r", "restart agent"),
    ("n", "toggle nix environment"),
    ("m", "choose model"),
    ("e", "choose reasoning level"),
    ("y", "copy thread selection"),
    ("b", "dismiss banner"),
    ("D", "toggle debug panels"),
];

const THREAD_ITEMS: &[(&str, &str)] = &[
    ("c", "new thread"),
    ("j / n", "next thread"),
    ("k / p", "previous thread"),
    ("g", "newest thread"),
    ("e", "oldest thread"),
    ("w", "next working thread"),
    ("u", "next unread thread"),
    ("i", "focus composer"),
    ("x", "stop agent"),
    ("r", "restart agent"),
];

const PROJECT_ITEMS: &[(&str, &str)] = &[
    ("a", "add project"),
    ("c", "new thread in project"),
    ("j / n", "next project"),
    ("k / p", "previous project"),
    ("g", "first project"),
    ("e", "last project"),
    ("[", "narrow sidebar"),
    ("]", "widen sidebar"),
];

const GOTO_ITEMS: &[(&str, &str)] = &[
    ("g", "top of conversation"),
    ("e", "bottom of conversation"),
    ("c", "composer"),
    ("j / n", "next thread"),
    ("k / p", "previous thread"),
    ("h", "newest thread"),
    ("l", "oldest thread"),
    ("w", "next working thread"),
    ("u", "next unread thread"),
    ("]", "next project"),
    ("[", "previous project"),
];

fn menu_items(menu: KeyboardMenu) -> &'static [(&'static str, &'static str)] {
    match menu {
        KeyboardMenu::Space => SPACE_ITEMS,
        KeyboardMenu::Goto => GOTO_ITEMS,
        KeyboardMenu::Threads => THREAD_ITEMS,
        KeyboardMenu::Projects => PROJECT_ITEMS,
    }
}

impl Dirigent {
    pub(crate) fn render_keyboard_menu(
        &self,
        menu: KeyboardMenu,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let items = menu_items(menu);
        div()
            .absolute()
            .right(px(10.0))
            .bottom(px(10.0))
            .w(px(286.0))
            .h(px(8.0 + items.len() as f32 * 23.0))
            .child(
                deferred(
                    div()
                        .id("keyboard-menu")
                        .size_full()
                        .px_3()
                        .py_1()
                        .flex()
                        .flex_col()
                        .gap_1()
                        .rounded_lg()
                        .border_1()
                        .border_color(rgb(border()))
                        .bg(rgb(crate::theme::sidebar_bg()))
                        .on_mouse_down_out(cx.listener(|this, _, _, cx| {
                            this.close_keyboard_menu();
                            cx.notify();
                        }))
                        .on_mouse_down(
                            gpui::MouseButton::Left,
                            cx.listener(|_, _, _, cx| cx.stop_propagation()),
                        )
                        .on_click(cx.listener(|_, _, _, cx| cx.stop_propagation()))
                        .children(items.iter().map(|(shortcut, description)| {
                            div()
                                .h(px(22.0))
                                .flex()
                                .items_center()
                                .text_xs()
                                .child(
                                    div()
                                        .w(px(38.0))
                                        .flex_none()
                                        .text_color(rgb(muted()))
                                        .child(*shortcut),
                                )
                                .child(div().text_color(rgb(faint())).child(*description))
                        })),
                )
                .priority(1),
            )
    }
}

#[cfg(test)]
mod tests {
    use super::{GOTO_ITEMS, PROJECT_ITEMS, SPACE_ITEMS, THREAD_ITEMS};

    #[test]
    fn keyboard_menus_offer_real_navigation_sets() {
        assert!(SPACE_ITEMS.len() >= 10);
        assert!(SPACE_ITEMS.contains(&("D", "toggle debug panels")));
        assert!(GOTO_ITEMS.len() >= 10);
        assert!(THREAD_ITEMS.len() >= 8);
        assert!(PROJECT_ITEMS.len() >= 5);
    }
}
