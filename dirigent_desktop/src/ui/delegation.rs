//! Child-thread navigation. Delegated work itself is shown beside its launching work group.

use super::*;
use crate::theme::{blue, surface_hover};
use gpui::svg;

impl Dirigent {
    pub(super) fn render_delegation(&self, cx: &mut Context<Self>) -> AnyElement {
        let Some((id, parent)) = self
            .selected_harness
            .and_then(|id| self.harnesses.iter().find(|harness| harness.id == id))
            .and_then(|harness| harness.delegation.parent.map(|parent| (harness.id, parent)))
        else {
            return div().into_any_element();
        };
        let title = self
            .harnesses
            .iter()
            .find(|harness| harness.id == parent)
            .map(|harness| harness.title.clone())
            .unwrap_or_else(|| "Parent thread".into());

        div()
            .id(("delegation-parent", id as usize))
            .flex_none()
            .px_3()
            .py_2()
            .border_b_1()
            .border_color(rgb(border()))
            .flex()
            .items_center()
            .gap_2()
            .text_sm()
            .text_color(rgb(blue()))
            .cursor_pointer()
            .hover(|style| style.bg(rgb(surface_hover())))
            .on_click(cx.listener(move |this, _, _, cx| {
                this.select_harness(parent);
                cx.notify();
            }))
            .child(
                svg()
                    .path("icon/move-left.svg")
                    .size(px(14.0))
                    .text_color(rgb(blue()))
                    .flex_none(),
            )
            .child(
                div()
                    .min_w(px(0.0))
                    .overflow_hidden()
                    .text_ellipsis()
                    .child(title),
            )
            .into_any_element()
    }
}
