use std::ops::Range;

#[cfg(any(target_os = "linux", target_os = "freebsd"))]
use gpui::ClipboardItem;
use gpui::{
    AnyElement, Context, CursorStyle, HighlightStyle, IntoElement, MouseButton, MouseDownEvent,
    MouseMoveEvent, MouseUpEvent, SharedString, StyledText, TextLayout, div, prelude::*,
};

use crate::{
    app::{Dirigent, ThreadTextSelection},
    diff::DiffSelectionReference,
    theme::rgb,
};

fn text_index(layout: &TextLayout, position: gpui::Point<gpui::Pixels>, len: usize) -> usize {
    layout
        .index_for_position(position)
        .unwrap_or_else(|nearest| nearest)
        .min(len)
}

fn merged_highlights(
    len: usize,
    highlights: &[(Range<usize>, HighlightStyle)],
    selection: Option<Range<usize>>,
) -> Vec<(Range<usize>, HighlightStyle)> {
    let mut boundaries = vec![0, len];
    for (range, _) in highlights {
        boundaries.extend([range.start, range.end]);
    }
    if let Some(range) = selection.as_ref() {
        boundaries.extend([range.start, range.end]);
    }
    boundaries.sort_unstable();
    boundaries.dedup();
    boundaries
        .windows(2)
        .filter_map(|pair| {
            let range = pair[0]..pair[1];
            if range.is_empty() {
                return None;
            }
            let mut style = highlights
                .iter()
                .find(|(highlight_range, _)| highlight_range.contains(&range.start))
                .map(|(_, style)| *style)
                .unwrap_or_default();
            let selected = selection
                .as_ref()
                .is_some_and(|selected| selected.contains(&range.start));
            if selected {
                style.background_color =
                    Some(rgb(crate::theme::text_selection()).opacity(0.40).into());
            }
            (style != HighlightStyle::default()).then_some((range, style))
        })
        .collect()
}

impl Dirigent {
    pub(crate) fn render_selectable_text(
        &self,
        id: impl Into<String>,
        text: impl Into<SharedString>,
        colors: &[(Range<usize>, u32)],
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let highlights = colors
            .iter()
            .map(|(range, color)| {
                (
                    range.clone(),
                    HighlightStyle {
                        color: Some(rgb(*color).into()),
                        ..Default::default()
                    },
                )
            })
            .collect::<Vec<_>>();
        self.render_styled_selectable_text(id, text, &highlights, &[], &[], false, cx)
    }

    pub(crate) fn render_single_line_selectable_text(
        &self,
        id: impl Into<String>,
        text: impl Into<SharedString>,
        colors: &[(Range<usize>, u32)],
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let highlights = colors
            .iter()
            .map(|(range, color)| {
                (
                    range.clone(),
                    HighlightStyle {
                        color: Some(rgb(*color).into()),
                        ..Default::default()
                    },
                )
            })
            .collect::<Vec<_>>();
        self.render_styled_selectable_text(id, text, &highlights, &[], &[], true, cx)
    }

    #[allow(clippy::too_many_arguments)]
    pub(crate) fn render_styled_selectable_text(
        &self,
        id: impl Into<String>,
        text: impl Into<SharedString>,
        highlights: &[(Range<usize>, HighlightStyle)],
        font_overrides: &[(Range<usize>, SharedString)],
        links: &[(Range<usize>, SharedString)],
        single_line: bool,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        self.render_styled_selectable_text_with_reference(
            id,
            text,
            highlights,
            font_overrides,
            links,
            single_line,
            None,
            cx,
        )
    }

    #[allow(clippy::too_many_arguments)]
    pub(crate) fn render_styled_selectable_text_with_reference(
        &self,
        id: impl Into<String>,
        text: impl Into<SharedString>,
        highlights: &[(Range<usize>, HighlightStyle)],
        font_overrides: &[(Range<usize>, SharedString)],
        links: &[(Range<usize>, SharedString)],
        single_line: bool,
        diff_reference: Option<DiffSelectionReference>,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let id = id.into();
        let text = text.into();
        let selected_range = self
            .thread_text_selection
            .as_ref()
            .filter(|selection| selection.id == id)
            .map(|selection| selection.range.clone());
        let mut styled = StyledText::new(text.clone());
        if !highlights.is_empty() || selected_range.is_some() {
            styled =
                styled.with_highlights(merged_highlights(text.len(), highlights, selected_range));
        }
        if !font_overrides.is_empty() {
            styled = styled.with_font_family_overrides(font_overrides.iter().cloned());
        }
        let layout = styled.layout().clone();

        let down_id = id.clone();
        let down_text = text.clone();
        let down_layout = layout.clone();
        let down_diff_reference = diff_reference.clone();
        let move_id = id.clone();
        let move_layout = layout.clone();
        let up_layout = layout.clone();
        let up_links = links.to_vec();
        div()
            .id(id)
            .w_full()
            .min_w(gpui::px(0.0))
            .when(single_line, |element| {
                element
                    .whitespace_nowrap()
                    .overflow_hidden()
                    .text_ellipsis()
            })
            .cursor(CursorStyle::IBeam)
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(move |this, event: &MouseDownEvent, window, cx| {
                    this.enter_normal_mode();
                    window.focus(&this.thread_focus, cx);
                    let index = text_index(&down_layout, event.position, down_text.len());
                    if event.modifiers.shift
                        && let Some(selection) = this
                            .thread_text_selection
                            .as_mut()
                            .filter(|selection| selection.id == down_id)
                    {
                        selection.head = index;
                        selection.range = selection.anchor.min(index)..selection.anchor.max(index);
                        selection.selecting = true;
                    } else {
                        this.thread_text_selection = Some(ThreadTextSelection {
                            id: down_id.clone(),
                            text: down_text.clone(),
                            anchor: index,
                            head: index,
                            range: index..index,
                            selecting: true,
                            diff_reference: down_diff_reference.clone(),
                        });
                    }
                    cx.notify();
                }),
            )
            .on_mouse_move(cx.listener(move |this, event: &MouseMoveEvent, _, cx| {
                let Some(selection) = this
                    .thread_text_selection
                    .as_mut()
                    .filter(|selection| selection.id == move_id && selection.selecting)
                else {
                    return;
                };
                let index = text_index(&move_layout, event.position, selection.text.len());
                selection.head = index;
                selection.range = selection.anchor.min(index)..selection.anchor.max(index);
                cx.notify();
            }))
            .on_mouse_up(
                MouseButton::Left,
                cx.listener(move |this, event: &MouseUpEvent, _, cx| {
                    if let Some(selection) = this.thread_text_selection.as_mut() {
                        selection.selecting = false;
                        if !selection.range.is_empty() {
                            #[cfg(any(target_os = "linux", target_os = "freebsd"))]
                            cx.write_to_primary(ClipboardItem::new_string(
                                selection.text[selection.range.clone()].to_string(),
                            ));
                            return;
                        }
                    }
                    let index = text_index(&up_layout, event.position, text.len());
                    if let Some((_, url)) =
                        up_links.iter().find(|(range, _)| range.contains(&index))
                    {
                        cx.open_url(url);
                        cx.stop_propagation();
                    }
                }),
            )
            .on_mouse_up_out(
                MouseButton::Left,
                cx.listener(|this, _: &MouseUpEvent, _, _| {
                    if let Some(selection) = this.thread_text_selection.as_mut() {
                        selection.selecting = false;
                    }
                }),
            )
            .child(styled)
            .into_any_element()
    }
}
