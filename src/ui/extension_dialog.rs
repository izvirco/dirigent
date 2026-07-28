use gpui::{AnyElement, Context, IntoElement, div, prelude::*, px, rgb};

use crate::{
    app::{DialogKind, Dirigent},
    theme::{ACCENT, BG, BORDER, MUTED, SURFACE, SURFACE_HOVER, TEXT},
};

impl Dirigent {
    pub(super) fn render_extension_dialog(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let dialog = self.pending_dialog.as_ref().expect("dialog must exist");
        let kind = dialog.kind;
        let title = dialog.title.clone();
        let message = dialog.message.clone();
        let options = dialog.options.clone();

        div()
            .absolute()
            .inset_0()
            .size_full()
            .flex()
            .items_center()
            .justify_center()
            .p_8()
            .bg(gpui::rgba(0x000000aa))
            .child(
                div()
                    .w_full()
                    .max_w(px(520.0))
                    .p_5()
                    .flex()
                    .flex_col()
                    .gap_4()
                    .rounded_xl()
                    .border_1()
                    .border_color(rgb(BORDER))
                    .bg(rgb(SURFACE))
                    .child(
                        div()
                            .text_lg()
                            .font_weight(gpui::FontWeight::SEMIBOLD)
                            .text_color(rgb(TEXT))
                            .child(title),
                    )
                    .when_some(message, |element, message| {
                        element.child(
                            div()
                                .text_sm()
                                .line_height(px(20.0))
                                .text_color(rgb(MUTED))
                                .child(message),
                        )
                    })
                    .when(kind == DialogKind::Select, |element| {
                        element.child(div().flex().flex_col().gap_2().children(
                            options.into_iter().enumerate().map(|(index, option)| {
                                let response = option.clone();
                                div()
                                    .id(("extension-option", index))
                                    .h(px(38.0))
                                    .px_3()
                                    .flex()
                                    .items_center()
                                    .rounded_lg()
                                    .border_1()
                                    .border_color(rgb(BORDER))
                                    .text_sm()
                                    .text_color(rgb(TEXT))
                                    .hover(|style| style.bg(rgb(SURFACE_HOVER)))
                                    .on_click(cx.listener(move |this, _, _, cx| {
                                        this.respond_extension_value(response.clone());
                                        cx.notify();
                                    }))
                                    .child(option)
                                    .into_any_element()
                            }),
                        ))
                    })
                    .when(
                        matches!(kind, DialogKind::Input | DialogKind::Editor),
                        |element| element.child(self.extension_input.clone()),
                    )
                    .child(
                        div()
                            .flex()
                            .justify_end()
                            .gap_2()
                            .child(
                                div()
                                    .id("cancel-extension-dialog")
                                    .h(px(34.0))
                                    .px_3()
                                    .flex()
                                    .items_center()
                                    .rounded_lg()
                                    .border_1()
                                    .border_color(rgb(BORDER))
                                    .text_xs()
                                    .text_color(rgb(MUTED))
                                    .hover(|style| style.bg(rgb(SURFACE_HOVER)))
                                    .on_click(cx.listener(|this, _, _, cx| {
                                        this.cancel_extension_dialog();
                                        cx.notify();
                                    }))
                                    .child("Cancel"),
                            )
                            .when(kind == DialogKind::Confirm, |element| {
                                element
                                    .child(confirm_button("No", false, cx))
                                    .child(confirm_button("Yes", true, cx))
                            })
                            .when(
                                matches!(kind, DialogKind::Input | DialogKind::Editor),
                                |element| {
                                    element.child(
                                        div()
                                            .id("submit-extension-dialog")
                                            .h(px(34.0))
                                            .px_3()
                                            .flex()
                                            .items_center()
                                            .rounded_lg()
                                            .bg(rgb(ACCENT))
                                            .font_weight(gpui::FontWeight::SEMIBOLD)
                                            .text_xs()
                                            .text_color(rgb(BG))
                                            .on_click(cx.listener(|this, _, _, cx| {
                                                this.submit_extension_dialog(cx)
                                            }))
                                            .child("Submit"),
                                    )
                                },
                            ),
                    ),
            )
    }
}

fn confirm_button(label: &'static str, confirmed: bool, cx: &mut Context<Dirigent>) -> AnyElement {
    div()
        .id(("extension-confirm", usize::from(confirmed)))
        .h(px(34.0))
        .px_3()
        .flex()
        .items_center()
        .rounded_lg()
        .bg(rgb(if confirmed { ACCENT } else { SURFACE_HOVER }))
        .font_weight(gpui::FontWeight::SEMIBOLD)
        .text_xs()
        .text_color(rgb(if confirmed { BG } else { TEXT }))
        .on_click(cx.listener(move |this, _, _, cx| {
            this.respond_extension_confirmation(confirmed);
            cx.notify();
        }))
        .child(label)
        .into_any_element()
}
