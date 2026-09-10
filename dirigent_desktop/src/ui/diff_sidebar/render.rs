//! Renders diff-sidebar controls, files, hunks, and code rows.

use super::*;

impl Dirigent {
    pub(in crate::ui) fn render_changes_toggle(&self, cx: &mut Context<Self>) -> AnyElement {
        if self.diff_sidebar_open || !self.selected_harness_supports_turn_diffs() {
            return div().into_any_element();
        }
        div()
            .id("changes-toggle")
            .absolute()
            .top(px(4.0))
            .right(px(4.0))
            .size(px(28.0))
            .flex()
            .items_center()
            .justify_center()
            .text_color(rgb(muted()))
            .hover(|style| style.text_color(rgb(theme_text())))
            .on_click(cx.listener(|this, _, _, cx| {
                this.toggle_diff_sidebar();
                cx.notify();
            }))
            .child(
                svg()
                    .path("icon/panel-right.svg")
                    .size(px(18.0))
                    .text_color(rgb(theme_text())),
            )
            .into_any_element()
    }

    fn render_diff_mode_toggle(&self, cx: &mut Context<Self>) -> AnyElement {
        let (label, next) = match self.diff_view_mode {
            DiffViewMode::Unified => ("Unified", DiffViewMode::Split),
            DiffViewMode::Split => ("Split", DiffViewMode::Unified),
        };
        div()
            .id("diff-mode-toggle")
            .h(px(24.0))
            .px_2()
            .flex_none()
            .flex()
            .items_center()
            .rounded_md()
            .whitespace_nowrap()
            .text_xs()
            .text_color(rgb(theme_text()))
            .hover(|style| style.bg(rgb(surface_hover())))
            .on_click(cx.listener(move |this, _, _, cx| {
                this.set_diff_view_mode(next);
                cx.notify();
            }))
            .child(label)
            .into_any_element()
    }

    fn render_diff_scope_toggle(&self, cx: &mut Context<Self>) -> AnyElement {
        let (label, next) = match self.diff_scope {
            DiffScope::Cumulative => ("Cumulative", DiffScope::Turn),
            DiffScope::Turn => ("Turn only", DiffScope::Cumulative),
        };
        div()
            .id("diff-scope-toggle")
            .h(px(24.0))
            .px_2()
            .flex_none()
            .flex()
            .items_center()
            .rounded_md()
            .whitespace_nowrap()
            .text_xs()
            .text_color(rgb(theme_text()))
            .hover(|style| style.bg(rgb(surface_hover())))
            .on_click(cx.listener(move |this, _, _, cx| {
                this.set_diff_scope(next);
                cx.notify();
                cx.stop_propagation();
            }))
            .child(label)
            .into_any_element()
    }

    fn render_diff_turn_row(
        &self,
        turn: &TurnDiff,
        selected: bool,
        live: bool,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let turn_id = turn.id;
        div()
            .id(("diff-turn", turn.id))
            .w_full()
            .px_2()
            .py_1()
            .flex()
            .items_center()
            .gap_2()
            .rounded_md()
            .when(selected, |element| element.bg(rgb(surface_hover())))
            .hover(|style| style.bg(rgb(surface_hover())))
            .on_click(cx.listener(move |this, _, _, cx| {
                this.select_diff_turn(turn_id);
                cx.notify();
            }))
            .child(
                div()
                    .w(px(54.0))
                    .flex_none()
                    .text_xs()
                    .font_weight(gpui::FontWeight::SEMIBOLD)
                    .text_color(rgb(if selected { blue() } else { muted() }))
                    .child(format!("Turn {}", turn.id)),
            )
            .child(
                div()
                    .min_w_0()
                    .flex_1()
                    .flex()
                    .flex_col()
                    .child(
                        div()
                            .whitespace_nowrap()
                            .overflow_hidden()
                            .text_ellipsis()
                            .text_xs()
                            .text_color(rgb(theme_text()))
                            .child(turn.prompt.clone()),
                    )
                    .child(div().text_xs().text_color(rgb(muted())).child(if live {
                        format!(
                            "{} files · +{} −{}",
                            turn.files.len(),
                            turn.additions,
                            turn.deletions
                        )
                    } else {
                        format!(
                            "{} · {} files · +{} −{}",
                            status_label(turn.status),
                            turn.files.len(),
                            turn.additions,
                            turn.deletions
                        )
                    })),
            )
            .into_any_element()
    }

    fn render_diff_turn_picker(
        &self,
        turns: &[Arc<TurnDiff>],
        active_preview: Option<&TurnDiff>,
        selected_turn_id: Option<u64>,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let open = self.diff_turn_dropdown_open;
        let selected = selected_turn_id.and_then(|id| {
            active_preview
                .filter(|preview| preview.id == id)
                .or_else(|| turns.iter().find(|turn| turn.id == id).map(Arc::as_ref))
        });
        let label = selected
            .map(|turn| format!("Turn {}", turn.id))
            .unwrap_or_else(|| "No turns".into());

        div()
            .relative()
            .flex_none()
            .child(
                div()
                    .id("diff-turn-picker")
                    .h(px(24.0))
                    .w_auto()
                    .px_2()
                    .flex()
                    .items_center()
                    .gap_1()
                    .rounded_md()
                    .whitespace_nowrap()
                    .text_xs()
                    .text_color(rgb(theme_text()))
                    .when(!turns.is_empty() || active_preview.is_some(), |element| {
                        element
                            .cursor_pointer()
                            .hover(|style| style.bg(rgb(surface_hover())))
                            .on_click(cx.listener(move |this, _, _, cx| {
                                this.diff_turn_dropdown_open = !open;
                                cx.notify();
                                cx.stop_propagation();
                            }))
                    })
                    .child(label)
                    .when(!turns.is_empty() || active_preview.is_some(), |element| {
                        element.child(dropdown_arrow(open))
                    }),
            )
            .when(open, |element| {
                element.child(
                    deferred(
                        div()
                            .id("diff-turn-dropdown")
                            .absolute()
                            .top(px(32.0))
                            .left_0()
                            .w(px(310.0))
                            .max_h(px(320.0))
                            .p_1()
                            .overflow_y_scroll()
                            .rounded_lg()
                            .border_1()
                            .border_color(rgb(border()))
                            .bg(rgb(crate::theme::menu_bg()))
                            .shadow_lg()
                            .occlude()
                            .on_mouse_down(
                                gpui::MouseButton::Left,
                                cx.listener(|_, _, _, cx| cx.stop_propagation()),
                            )
                            .on_click(cx.listener(|_, _, _, cx| cx.stop_propagation()))
                            .when_some(active_preview, |element, preview| {
                                element.child(self.render_diff_turn_row(
                                    preview,
                                    Some(preview.id) == selected_turn_id,
                                    true,
                                    cx,
                                ))
                            })
                            .children(turns.iter().rev().map(|turn| {
                                self.render_diff_turn_row(
                                    turn,
                                    Some(turn.id) == selected_turn_id,
                                    false,
                                    cx,
                                )
                            })),
                    )
                    .priority(3),
                )
            })
            .into_any_element()
    }

    fn render_diff_code(
        &self,
        id: String,
        block: &DiffBlock,
        reference: &DiffSelectionReference,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        self.render_styled_selectable_text_with_reference(
            id,
            block.text.clone(),
            &block.highlights,
            &[],
            &[],
            false,
            Some(reference.clone()),
            cx,
        )
    }

    fn diff_code_scroll(&self, key: &str) -> ScrollHandle {
        self.diff_code_scrolls
            .borrow_mut()
            .entry(key.to_string())
            .or_default()
            .clone()
    }

    pub(super) fn diff_file_code_width(file: &FileDiff) -> f32 {
        let columns = file
            .hunks
            .iter()
            .flat_map(|hunk| &hunk.rows)
            .flat_map(|row| [row.old_text.as_deref(), row.new_text.as_deref()])
            .flatten()
            .map(|line| line.chars().count())
            .max()
            .unwrap_or(0);
        (columns as f32 * 7.4 + 20.0).max(120.0)
    }

    fn render_diff_list_scrollbar(&self, cx: &mut Context<Self>) -> AnyElement {
        let max_offset = self.diff_list.max_offset_for_scrollbar().y.as_f32();
        let viewport = self.diff_list.viewport_bounds().size.height.as_f32();
        let thumb_fraction = if viewport > 0.0 && max_offset > 0.0 {
            let minimum = (10.0 / viewport).clamp(0.08, 1.0);
            (viewport / (viewport + max_offset)).clamp(minimum, 1.0)
        } else {
            1.0
        };
        let scroll_fraction = if max_offset > 0.0 {
            (-self.diff_list.scroll_px_offset_for_scrollbar().y.as_f32() / max_offset)
                .clamp(0.0, 1.0)
        } else {
            0.0
        };
        let thumb_top = (1.0 - thumb_fraction) * scroll_fraction;
        let drag = DiffListScrollbarDrag {
            list: self.diff_list.clone(),
            start_pointer: Rc::new(Cell::new(px(0.0))),
            start_offset: self.diff_list.scroll_px_offset_for_scrollbar().y.as_f32(),
            max_offset,
            thumb_travel: (viewport - 6.0).max(0.0) * (1.0 - thumb_fraction),
        };

        div()
            .id("diff-list-scrollbar")
            .absolute()
            .top(px(3.0))
            .bottom(px(3.0))
            .right(px(2.0))
            .w(px(2.0))
            .rounded_full()
            .when(max_offset > 0.0, |element| {
                element.bg(gpui::rgba(0xffffff16)).child(
                    div()
                        .id("diff-list-scrollbar-thumb")
                        .group("diff-list-scrollbar-thumb")
                        .absolute()
                        .top(gpui::relative(thumb_top))
                        .right(px(-3.0))
                        .h(gpui::relative(thumb_fraction))
                        .min_h(px(10.0))
                        .w(px(8.0))
                        .cursor(CursorStyle::Arrow)
                        .on_mouse_down(
                            MouseButton::Left,
                            cx.listener(|_, _, _, cx| cx.stop_propagation()),
                        )
                        .on_drag(drag, |drag, _, window, cx| {
                            drag.start_pointer.set(window.mouse_position().y);
                            cx.new(|_| DiffSidebarResizePreview)
                        })
                        .on_drag_move::<DiffListScrollbarDrag>(cx.listener(
                            |_, event: &DragMoveEvent<DiffListScrollbarDrag>, _, cx| {
                                let drag = event.drag(cx);
                                let offset = scrollbar_drag_offset(
                                    drag.start_offset,
                                    (event.event.position.y - drag.start_pointer.get()).as_f32(),
                                    drag.max_offset,
                                    drag.thumb_travel,
                                );
                                drag.list
                                    .set_offset_from_scrollbar(point(px(0.0), px(offset)));
                                cx.notify();
                                cx.stop_propagation();
                            },
                        ))
                        .child(
                            div()
                                .ml(px(3.0))
                                .h_full()
                                .w(px(2.0))
                                .rounded_full()
                                .bg(rgb(muted()))
                                .group_hover("diff-list-scrollbar-thumb", |style| {
                                    style.bg(rgb(orange()))
                                }),
                        ),
                )
            })
            .into_any_element()
    }

    #[allow(clippy::too_many_arguments)]
    fn render_diff_block(
        &self,
        id: String,
        block: &DiffBlock,
        gutter_width: f32,
        content_width: f32,
        reference: &DiffSelectionReference,
        scroll: &ScrollHandle,
        show_scrollbar: bool,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let scrollbar_id = format!("{id}-scrollbar");
        let scroll_id = format!("{id}-scroll");
        let border_column = div().w(px(4.0)).flex_none().overflow_hidden().child(
            StyledText::new(block.gutter_border.text.clone())
                .with_highlights(block.gutter_border.highlights.iter().cloned()),
        );
        let gutter_content = if let Some(columns) = block.gutter_columns.as_ref() {
            div()
                .w_full()
                .flex()
                .child(
                    div().w(px(38.0)).flex_none().pr_1().text_right().child(
                        StyledText::new(columns.old.text.clone())
                            .with_highlights(columns.old.highlights.iter().cloned()),
                    ),
                )
                .child(
                    div().w(px(38.0)).flex_none().pr_1().text_right().child(
                        StyledText::new(columns.new.text.clone())
                            .with_highlights(columns.new.highlights.iter().cloned()),
                    ),
                )
                .child(
                    div().w(px(14.0)).flex_none().text_center().child(
                        StyledText::new(columns.sign.text.clone())
                            .with_highlights(columns.sign.highlights.iter().cloned()),
                    ),
                )
                .child(border_column)
                .into_any_element()
        } else {
            div()
                .w_full()
                .flex()
                .child(
                    div().min_w_0().flex_1().pr_1().text_right().child(
                        StyledText::new(block.gutter_text.clone())
                            .with_highlights(block.gutter_highlights.iter().cloned()),
                    ),
                )
                .child(border_column)
                .into_any_element()
        };
        let gutter = div()
            .relative()
            .w(px(gutter_width))
            .flex_none()
            .whitespace_nowrap()
            .text_xs()
            .line_height(px(18.0))
            .child(gutter_content);
        let code_padding = div()
            .w(px(4.0))
            .flex_none()
            .overflow_hidden()
            .text_xs()
            .line_height(px(18.0))
            .child(
                StyledText::new(block.code_padding.text.clone())
                    .with_highlights(block.code_padding.highlights.iter().cloned()),
            );
        let mut code_scroll = div()
            .id(scroll_id)
            .w_full()
            .min_w_0()
            .overflow_x_scroll()
            .track_scroll(scroll)
            .child(
                div()
                    .min_w(px(content_width))
                    .whitespace_nowrap()
                    .line_height(px(18.0))
                    .child(self.render_diff_code(id, block, reference, cx)),
            );
        // Keep vertical wheel input on the virtualized diff list instead of converting it
        // into horizontal movement for this nested x-only scroll area.
        code_scroll.style().restrict_scroll_to_axis = Some(true);
        div()
            .w_full()
            .min_w_0()
            .flex()
            .items_start()
            .child(gutter)
            .child(code_padding)
            .child(div().relative().flex_1().min_w_0().child(code_scroll).when(
                show_scrollbar,
                |element| {
                    element.child(self.render_thin_horizontal_scrollbar(scrollbar_id, scroll, cx))
                },
            ))
            .into_any_element()
    }

    fn diff_file_collapsed(&self, file: &FileDiff) -> bool {
        self.diff_display_key
            .and_then(|key| self.diff_file_collapse_overrides.get(&key))
            .and_then(|overrides| overrides.get(&file.path))
            .copied()
            .unwrap_or_else(|| is_common_lock_file(&file.path))
    }

    fn toggle_diff_file(&mut self, file_index: usize) {
        let Some(key) = self.diff_display_key else {
            return;
        };
        let Some(file) = self
            .diff_display
            .as_ref()
            .and_then(|turn| turn.files.get(file_index))
        else {
            return;
        };
        let Some(old_header) = self.diff_render_cache.file_header_item_index(file_index) else {
            return;
        };
        let old_end = self
            .diff_render_cache
            .file_header_item_index(file_index + 1)
            .unwrap_or(self.diff_render_cache.items.len());
        let header_y = self
            .diff_list
            .bounds_for_item(old_header)
            .map(|bounds| {
                (bounds.top() - self.diff_list.viewport_bounds().top())
                    .as_f32()
                    .max(0.0)
            })
            .unwrap_or(0.0);
        let path = file.path.clone();
        let collapsed = self.diff_file_collapsed(file);

        // Anchor the list before changing its item count so the clicked header keeps its
        // viewport position. A sticky header has no item bounds, so it remains at the top.
        self.diff_list.scroll_to(ListOffset {
            item_ix: old_header,
            offset_in_item: px(0.0),
        });
        self.diff_list.scroll_by(px(-header_y));

        self.diff_file_collapse_overrides
            .entry(key)
            .or_default()
            .insert(path, !collapsed);
        self.thread_text_selection = None;

        let new_cache = self.build_diff_render_cache();
        let new_header = new_cache
            .file_header_item_index(file_index)
            .expect("rebuilt diff contains toggled file");
        let new_end = new_cache
            .file_header_item_index(file_index + 1)
            .unwrap_or(new_cache.items.len());
        self.diff_render_cache = new_cache;
        self.diff_list
            .splice(old_header + 1..old_end, new_end - new_header - 1);
        self.diff_list.remeasure_items(old_header..old_header + 1);
    }

    fn render_diff_file_header(
        &self,
        file_index: usize,
        last_in_file: bool,
        collapsed: bool,
        sticky: bool,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let Some(file) = self
            .diff_display
            .as_ref()
            .and_then(|turn| turn.files.get(file_index))
        else {
            return div().into_any_element();
        };
        let chevron = svg()
            .path("icon/chevron-down.svg")
            .size(px(12.0))
            .text_color(rgb(muted()))
            .flex_none();
        let chevron = if collapsed {
            chevron.with_transformation(Transformation::rotate(radians(
                -std::f32::consts::FRAC_PI_2,
            )))
        } else {
            chevron
        };

        div()
            .w_full()
            .when(last_in_file || sticky, |element| {
                element.border_b_1().border_color(rgb(border()))
            })
            .child(
                div()
                    .id((
                        if sticky {
                            "sticky-diff-file-header"
                        } else {
                            "diff-file-header"
                        },
                        file_index,
                    ))
                    .h(px(34.0))
                    .px_3()
                    .flex()
                    .items_center()
                    .gap_2()
                    .cursor_pointer()
                    .bg(rgb(surface()))
                    .text_xs()
                    .hover(|style| style.bg(rgb(surface_hover())))
                    .on_click(cx.listener(move |this, _, _, cx| {
                        this.toggle_diff_file(file_index);
                        cx.notify();
                        cx.stop_propagation();
                    }))
                    .child(chevron)
                    .child(
                        div()
                            .w(px(14.0))
                            .text_color(rgb(match file.kind {
                                FileDiffKind::Added => green(),
                                FileDiffKind::Deleted => red(),
                                _ => orange(),
                            }))
                            .child(file_kind_label(file.kind)),
                    )
                    .child(
                        div()
                            .min_w_0()
                            .flex_1()
                            .whitespace_nowrap()
                            .overflow_hidden()
                            .text_ellipsis()
                            .text_color(rgb(theme_text()))
                            .child(file.path.clone()),
                    )
                    .child(render_diff_stats(file.additions, file.deletions)),
            )
            .when(!collapsed && !sticky, |element| {
                element.when_some(file.message.clone(), |element, message| {
                    element.child(
                        div()
                            .px_3()
                            .py_3()
                            .text_xs()
                            .text_color(rgb(muted()))
                            .child(message),
                    )
                })
            })
            .into_any_element()
    }

    #[allow(clippy::too_many_arguments)]
    fn render_unified_diff_chunk(
        &self,
        file_index: usize,
        hunk_index: usize,
        chunk_index: usize,
        block: &DiffBlock,
        reference: &DiffSelectionReference,
        top_gap: bool,
        last_in_file: bool,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let prepared = &self.diff_render_cache.files[file_index];
        let scroll =
            self.diff_code_scroll(&format!("diff-{}-{file_index}-unified", reference.turn_id));
        div()
            .w_full()
            .overflow_hidden()
            .when(top_gap, |element| element.mt_2())
            .when(last_in_file, |element| {
                element.border_b_1().border_color(rgb(border()))
            })
            .child(self.render_diff_block(
                format!(
                    "diff-{}-{file_index}-{hunk_index}-{chunk_index}-unified",
                    reference.turn_id
                ),
                block,
                94.0,
                prepared.content_width,
                reference,
                &scroll,
                last_in_file,
                cx,
            ))
            .into_any_element()
    }

    #[allow(clippy::too_many_arguments)]
    fn render_split_diff_chunk(
        &self,
        file_index: usize,
        hunk_index: usize,
        chunk_index: usize,
        old_block: &DiffBlock,
        new_block: &DiffBlock,
        old_reference: &DiffSelectionReference,
        new_reference: &DiffSelectionReference,
        top_gap: bool,
        last_in_file: bool,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let prepared = &self.diff_render_cache.files[file_index];
        let scroll = self.diff_code_scroll(&format!(
            "diff-{}-{file_index}-split",
            new_reference.turn_id
        ));
        div()
            .w_full()
            .min_w_0()
            .flex()
            .overflow_hidden()
            .when(top_gap, |element| element.mt_2())
            .when(last_in_file, |element| {
                element.border_b_1().border_color(rgb(border()))
            })
            .child(
                div()
                    .w_1_2()
                    .min_w_0()
                    .overflow_hidden()
                    .border_r_1()
                    .border_color(rgb(border()))
                    .child(self.render_diff_block(
                        format!(
                            "diff-{}-{file_index}-{hunk_index}-{chunk_index}-old",
                            old_reference.turn_id
                        ),
                        old_block,
                        48.0,
                        prepared.content_width,
                        old_reference,
                        &scroll,
                        false,
                        cx,
                    )),
            )
            .child(
                div()
                    .w_1_2()
                    .min_w_0()
                    .overflow_hidden()
                    .child(self.render_diff_block(
                        format!(
                            "diff-{}-{file_index}-{hunk_index}-{chunk_index}-new",
                            new_reference.turn_id
                        ),
                        new_block,
                        48.0,
                        prepared.content_width,
                        new_reference,
                        &scroll,
                        last_in_file,
                        cx,
                    )),
            )
            .into_any_element()
    }

    fn render_diff_list_item(
        &mut self,
        index: usize,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let Some(item) = self.diff_render_cache.items.get(index) else {
            return div().into_any_element();
        };
        match item {
            DiffRenderItem::FileHeader {
                file_index,
                collapsed,
                last_in_file,
            } => self.render_diff_file_header(*file_index, *last_in_file, *collapsed, false, cx),
            DiffRenderItem::UnifiedChunk {
                file_index,
                hunk_index,
                chunk_index,
                block,
                reference,
                top_gap,
                last_in_file,
            } => self.render_unified_diff_chunk(
                *file_index,
                *hunk_index,
                *chunk_index,
                block,
                reference,
                *top_gap,
                *last_in_file,
                cx,
            ),
            DiffRenderItem::SplitChunk {
                file_index,
                hunk_index,
                chunk_index,
                old_block,
                new_block,
                old_reference,
                new_reference,
                top_gap,
                last_in_file,
            } => self.render_split_diff_chunk(
                *file_index,
                *hunk_index,
                *chunk_index,
                old_block,
                new_block,
                old_reference,
                new_reference,
                *top_gap,
                *last_in_file,
                cx,
            ),
        }
    }

    fn render_sticky_diff_file_header(&self, cx: &mut Context<Self>) -> AnyElement {
        let Some(item_index) = self
            .diff_render_cache
            .items
            .len()
            .checked_sub(1)
            .map(|last| self.diff_list.logical_scroll_top().item_ix.min(last))
        else {
            return div().into_any_element();
        };
        let file_index = self.diff_render_cache.items[item_index].file_index();
        let collapsed = self
            .diff_display
            .as_ref()
            .and_then(|turn| turn.files.get(file_index))
            .is_some_and(|file| self.diff_file_collapsed(file));
        let next_header = self
            .diff_render_cache
            .file_header_item_index(file_index + 1);
        let viewport_top = self.diff_list.viewport_bounds().top();
        let top = next_header
            .and_then(|index| self.diff_list.bounds_for_item(index))
            .map(|bounds| {
                (bounds.top() - viewport_top - px(34.0))
                    .as_f32()
                    .clamp(-34.0, 0.0)
            })
            .unwrap_or(0.0);

        div()
            .absolute()
            .top(px(top))
            .left_0()
            .right_0()
            .occlude()
            .child(self.render_diff_file_header(file_index, false, collapsed, true, cx))
            .into_any_element()
    }

    fn build_diff_render_cache(&self) -> DiffRenderCache {
        let collapsed_files = self
            .diff_display
            .as_ref()
            .map(|turn| {
                turn.files
                    .iter()
                    .map(|file| self.diff_file_collapsed(file))
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default();
        self.diff_display
            .as_ref()
            .map(|turn| {
                DiffRenderCache::build(turn, self.diff_view_mode, collapsed_files.as_slice())
            })
            .unwrap_or_default()
    }

    pub(crate) fn rebuild_diff_render_cache(&mut self) {
        self.diff_render_cache = self.build_diff_render_cache();
        self.diff_list.reset_with_uniform_height(
            self.diff_render_cache.items.len(),
            px(self.diff_render_cache.item_height_hint()),
        );
    }

    pub(crate) fn diff_sidebar_replaces_thread(&self, window: &Window) -> bool {
        self.diff_sidebar_open
            && self.selected_harness.is_some()
            && diff_sidebar_replaces_thread(
                window.viewport_size().width.as_f32(),
                self.sidebar_layout_width(),
                self.diff_sidebar_width,
            )
    }

    pub(crate) fn render_diff_sidebar(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        if !self.diff_sidebar_open {
            return div().into_any_element();
        }
        self.sync_diff_display();
        if self.selected_harness.is_none() {
            return div().into_any_element();
        }
        let replaces_thread = self.diff_sidebar_replaces_thread(window);
        let harness = self
            .selected_harness
            .and_then(|id| self.harnesses.iter().find(|harness| harness.id == id))
            .expect("selected harness exists");
        let selected_turn_id = self.selected_diff_turn_id();
        let display_empty = self
            .diff_display
            .as_ref()
            .is_none_or(|turn| turn.files.is_empty());
        let empty_message = self.diff_display.as_ref().and_then(|turn| {
            display_empty.then(|| {
                turn.error.clone().unwrap_or_else(|| match self.diff_scope {
                    DiffScope::Cumulative => "No net file changes through this turn.".into(),
                    DiffScope::Turn => "No file changes in this turn.".into(),
                })
            })
        });
        let combined_stats = self
            .diff_display
            .as_ref()
            .map(|turn| (turn.additions, turn.deletions));
        let no_turns = harness.turn_diffs.is_empty() && harness.active_turn_preview.is_none();
        let turn_picker = self.render_diff_turn_picker(
            &harness.turn_diffs,
            harness.active_turn_preview.as_ref(),
            selected_turn_id,
            cx,
        );
        let resize_drag = DiffSidebarResizeDrag {
            width: self.diff_sidebar_width,
            mouse_x: window.mouse_position().x,
        };
        let viewport_width = window.viewport_size().width.as_f32();
        let rendered_width = if replaces_thread {
            (viewport_width - self.sidebar_layout_width()).max(0.0)
        } else {
            self.diff_sidebar_width
        };
        let minimum_width = if replaces_thread {
            0.0
        } else {
            MIN_DIFF_SIDEBAR_WIDTH
        };
        let maximum_width = if replaces_thread {
            rendered_width
        } else {
            viewport_width * 0.85
        };
        let has_diff_selection = self
            .thread_text_selection
            .as_ref()
            .is_some_and(|selection| {
                !selection.range.is_empty() && selection.diff_reference.is_some()
            });

        div()
            .relative()
            .w(px(rendered_width))
            .min_w(px(minimum_width))
            .max_w(px(maximum_width))
            .h_full()
            .flex_none()
            .flex()
            .flex_col()
            .border_l_1()
            .border_color(rgb(border()))
            .bg(rgb(bg()))
            .child(
                div()
                    .h(px(36.0))
                    .px_1()
                    .flex_none()
                    .flex()
                    .items_center()
                    .gap_1()
                    .border_b_1()
                    .border_color(rgb(border()))
                    .child(turn_picker)
                    .child(self.render_diff_scope_toggle(cx))
                    .child(self.render_diff_mode_toggle(cx))
                    .child(div().flex_1())
                    .when_some(combined_stats, |element, (additions, deletions)| {
                        element.child(render_diff_stats(additions, deletions))
                    })
                    .child(
                        div()
                            .id("close-diff-sidebar")
                            .size(px(28.0))
                            .flex_none()
                            .flex()
                            .items_center()
                            .justify_center()
                            .text_color(rgb(muted()))
                            .hover(|style| style.text_color(rgb(theme_text())))
                            .on_click(cx.listener(|this, _, _, cx| {
                                this.close_diff_sidebar();
                                cx.notify();
                            }))
                            .child(
                                svg()
                                    .path("icon/panel-right.svg")
                                    .size(px(18.0))
                                    .text_color(rgb(theme_text())),
                            ),
                    ),
            )
            .child(
                div()
                    .id("diff-content-list")
                    .relative()
                    .flex_1()
                    .min_h_0()
                    .overflow_hidden()
                    .whitespace_nowrap()
                    .text_xs()
                    .text_color(rgb(code_text()))
                    .when(!display_empty, |element| {
                        element.child(
                            list(
                                self.diff_list.clone(),
                                cx.processor(Self::render_diff_list_item),
                            )
                            .size_full(),
                        )
                    })
                    .when(display_empty, |element| {
                        let message = empty_message.unwrap_or_else(|| {
                            if no_turns {
                                "Completed turn diffs will appear here.".into()
                            } else {
                                "No file changes to display.".into()
                            }
                        });
                        element.child(
                            div()
                                .absolute()
                                .inset_0()
                                .p_6()
                                .text_center()
                                .whitespace_normal()
                                .text_color(rgb(muted()))
                                .child(message),
                        )
                    })
                    .when(!display_empty, |element| {
                        element.child(self.render_sticky_diff_file_header(cx))
                    })
                    .child(self.render_diff_list_scrollbar(cx)),
            )
            .when(has_diff_selection, |element| {
                element.child(
                    div()
                        .h(px(42.0))
                        .px_3()
                        .flex_none()
                        .flex()
                        .items_center()
                        .gap_2()
                        .border_t_1()
                        .border_color(rgb(border()))
                        .bg(rgb(surface()))
                        .child(
                            div()
                                .flex_1()
                                .text_xs()
                                .text_color(rgb(muted()))
                                .child("Diff text selected"),
                        )
                        .child(
                            div()
                                .id("add-diff-reference")
                                .h(px(28.0))
                                .px_3()
                                .flex()
                                .items_center()
                                .rounded_md()
                                .bg(rgb(blue()))
                                .text_xs()
                                .text_color(rgb(bg()))
                                .hover(|style| style.bg(rgb(crate::theme::accent_hover())))
                                .on_click(cx.listener(|this, _, _, cx| {
                                    this.add_diff_selection_to_composer(cx);
                                    cx.stop_propagation();
                                }))
                                .child("Add reference to composer"),
                        ),
                )
            })
            .when(!replaces_thread, |element| {
                element.child(
                    div()
                        .id("diff-sidebar-resize-handle")
                        .absolute()
                        .top_0()
                        .bottom_0()
                        .left(px(-3.0))
                        .w(px(7.0))
                        .cursor(CursorStyle::ResizeColumn)
                        .hover(|style| style.bg(rgb(blue())))
                        .on_mouse_down(
                            gpui::MouseButton::Left,
                            cx.listener(|_, _, _, cx| cx.stop_propagation()),
                        )
                        .on_drag(resize_drag, |_, _, _, cx| {
                            cx.new(|_| DiffSidebarResizePreview)
                        })
                        .on_drag_move::<DiffSidebarResizeDrag>(cx.listener(
                            |this, event: &DragMoveEvent<DiffSidebarResizeDrag>, _, cx| {
                                let drag = event.drag(cx);
                                let delta = drag.mouse_x - event.event.position.x;
                                this.resize_diff_sidebar(drag.width + delta.as_f32(), cx);
                                cx.notify();
                            },
                        )),
                )
            })
            .into_any_element()
    }
}
