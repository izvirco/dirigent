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

fn word_range(text: &str, index: usize) -> Range<usize> {
    if text.is_empty() {
        return 0..0;
    }

    let index = index.min(text.len());
    let character_start = if index == text.len() {
        text.char_indices()
            .next_back()
            .map_or(0, |(start, _)| start)
    } else if text.is_char_boundary(index) {
        index
    } else {
        (0..index)
            .rev()
            .find(|offset| text.is_char_boundary(*offset))
            .unwrap_or(0)
    };
    let character = text[character_start..]
        .chars()
        .next()
        .expect("non-empty text has a character at a valid boundary");
    let is_word = |candidate: char| candidate.is_alphanumeric() || candidate == '_';

    if !is_word(character) {
        return character_start..character_start + character.len_utf8();
    }

    let start = text[..character_start]
        .char_indices()
        .rev()
        .take_while(|(_, candidate)| is_word(*candidate))
        .last()
        .map_or(character_start, |(start, _)| start);
    let end = character_start
        + text[character_start..]
            .char_indices()
            .take_while(|(_, candidate)| is_word(*candidate))
            .map(|(_, candidate)| candidate.len_utf8())
            .sum::<usize>();
    start..end
}

fn local_selection_range(
    selection: &ThreadTextSelection,
    element_range: &Range<usize>,
) -> Option<Range<usize>> {
    let start = selection.range.start.max(element_range.start);
    let end = selection.range.end.min(element_range.end);
    (start < end).then(|| start - element_range.start..end - element_range.start)
}

fn merged_highlights(
    len: usize,
    highlights: &[(Range<usize>, HighlightStyle)],
    selection: Option<Range<usize>>,
) -> Vec<(Range<usize>, HighlightStyle)> {
    // Highlight producers emit ordered, non-overlapping ranges. Walk those ranges and the
    // single selection range together instead of searching all highlights at every boundary.
    let ordered = highlights
        .iter()
        .filter_map(|(range, style)| {
            let range = range.start.min(len)..range.end.min(len);
            (!range.is_empty()).then_some((range, *style))
        })
        .collect::<Vec<_>>();
    if !ordered
        .windows(2)
        .all(|pair| pair[0].0.end <= pair[1].0.start)
    {
        // Keep the original first-highlight-wins behavior for uncommon overlapping inputs.
        // Diff highlights use the linear path above.
        let mut boundaries = vec![0, len];
        for (range, _) in &ordered {
            boundaries.extend([range.start, range.end]);
        }
        if let Some(range) = selection.as_ref() {
            boundaries.extend([range.start.min(len), range.end.min(len)]);
        }
        boundaries.sort_unstable();
        boundaries.dedup();
        return boundaries
            .windows(2)
            .filter_map(|pair| {
                let range = pair[0]..pair[1];
                let mut style = ordered
                    .iter()
                    .find(|(highlight_range, _)| highlight_range.contains(&range.start))
                    .map(|(_, style)| *style)
                    .unwrap_or_default();
                if selection
                    .as_ref()
                    .is_some_and(|selected| selected.contains(&range.start))
                {
                    style.background_color =
                        Some(rgb(crate::theme::text_selection()).opacity(0.40).into());
                }
                (style != HighlightStyle::default()).then_some((range, style))
            })
            .collect();
    }

    let selection = selection.map(|range| range.start.min(len)..range.end.min(len));
    let selection_style = rgb(crate::theme::text_selection()).opacity(0.40).into();
    let mut result: Vec<(Range<usize>, HighlightStyle)> = Vec::with_capacity(
        ordered.len() + usize::from(selection.as_ref().is_some_and(|range| !range.is_empty())) * 2,
    );
    let mut highlight_index = 0;
    let mut position = 0;

    while position < len {
        while highlight_index < ordered.len() && ordered[highlight_index].0.end <= position {
            highlight_index += 1;
        }
        let highlight = ordered.get(highlight_index);
        let mut end = highlight
            .map_or(len, |(range, _)| {
                if position < range.start {
                    range.start
                } else {
                    range.end
                }
            })
            .min(len);
        let mut style = highlight
            .filter(|(range, _)| range.contains(&position))
            .map(|(_, style)| *style)
            .unwrap_or_default();

        if let Some(selected) = selection.as_ref() {
            if selected.contains(&position) {
                end = end.min(selected.end);
                style.background_color = Some(selection_style);
            } else if position < selected.start {
                end = end.min(selected.start);
            }
        }
        if end <= position {
            end = position + 1;
        }
        if style != HighlightStyle::default() {
            if let Some((previous_range, previous_style)) = result.last_mut()
                && *previous_style == style
                && previous_range.end == position
            {
                previous_range.end = end;
            } else {
                result.push((position..end, style));
            }
        }
        position = end;
    }
    result
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
    pub(crate) fn render_grouped_styled_selectable_text(
        &self,
        element_id: impl Into<String>,
        text: impl Into<SharedString>,
        highlights: &[(Range<usize>, HighlightStyle)],
        font_overrides: &[(Range<usize>, SharedString)],
        links: &[(Range<usize>, SharedString)],
        selection_id: impl Into<String>,
        selection_text: SharedString,
        selection_range: Range<usize>,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        self.render_styled_selectable_text_internal(
            element_id.into(),
            text.into(),
            highlights,
            font_overrides,
            links,
            false,
            selection_id.into(),
            selection_text,
            selection_range,
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
        let text_len = text.len();
        self.render_styled_selectable_text_internal(
            id.clone(),
            text.clone(),
            highlights,
            font_overrides,
            links,
            single_line,
            id,
            text,
            0..text_len,
            diff_reference,
            cx,
        )
    }

    #[allow(clippy::too_many_arguments)]
    fn render_styled_selectable_text_internal(
        &self,
        element_id: String,
        text: SharedString,
        highlights: &[(Range<usize>, HighlightStyle)],
        font_overrides: &[(Range<usize>, SharedString)],
        links: &[(Range<usize>, SharedString)],
        single_line: bool,
        selection_id: String,
        selection_text: SharedString,
        selection_range: Range<usize>,
        diff_reference: Option<DiffSelectionReference>,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        debug_assert_eq!(selection_range.len(), text.len());
        let selected_range = self
            .thread_text_selection
            .as_ref()
            .filter(|selection| selection.id == selection_id)
            .and_then(|selection| local_selection_range(selection, &selection_range));
        let mut styled = StyledText::new(text.clone());
        if !highlights.is_empty() || selected_range.is_some() {
            styled = if selected_range.is_none() && diff_reference.is_some() {
                // Prepared diff highlights are already clipped, ordered, and non-overlapping.
                // Avoid rebuilding the full run list on every frame unless a selection must
                // be merged into it.
                styled.with_highlights(highlights.iter().cloned())
            } else {
                styled.with_highlights(merged_highlights(text.len(), highlights, selected_range))
            };
        }
        if !font_overrides.is_empty() {
            styled = styled.with_font_family_overrides(font_overrides.iter().cloned());
        }
        let layout = styled.layout().clone();

        let down_id = selection_id.clone();
        let down_text = selection_text.clone();
        let down_element_text = text.clone();
        let down_range = selection_range.clone();
        let down_layout = layout.clone();
        let down_diff_reference = diff_reference.clone();
        let move_id = selection_id.clone();
        let move_range = selection_range.clone();
        let move_layout = layout.clone();
        let up_layout = layout.clone();
        let up_links = links.to_vec();
        div()
            .id(element_id)
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
                    let local_index =
                        text_index(&down_layout, event.position, down_element_text.len());
                    let index = down_range.start + local_index;
                    if event.click_count >= 2 {
                        let local_word = word_range(&down_element_text, local_index);
                        let word =
                            down_range.start + local_word.start..down_range.start + local_word.end;
                        this.thread_text_selection = Some(ThreadTextSelection {
                            id: down_id.clone(),
                            text: down_text.clone(),
                            anchor: word.start,
                            head: word.end,
                            range: word,
                            selecting: true,
                            diff_reference: down_diff_reference.clone(),
                        });
                    } else if event.modifiers.shift
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
                let local_index = text_index(&move_layout, event.position, move_range.len());
                let index = move_range.start + local_index;
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
