//! Short, live-only entry fades and height reveals. Growing list rows lets GPUI's normal
//! tail following scroll smoothly, without taking scroll ownership away from the user.

use std::{
    cell::RefCell,
    rc::Rc,
    time::{Duration, Instant},
};

use gpui::{
    AnyElement, App, AvailableSpace, Bounds, ContentMask, Element, ElementId, GlobalElementId,
    InspectorElementId, IntoElement, LayoutId, Pixels, Style, WeakEntity, Window, div, prelude::*,
    px, size,
};

use crate::{app::Dirigent, model::Id};

const REVEAL_DURATION: Duration = Duration::from_millis(180);

#[derive(Default)]
struct RevealState {
    updated_at: Option<Instant>,
    started_at: Option<Instant>,
    width: Option<Pixels>,
    target_height: Pixels,
    from_height: Pixels,
    initial: bool,
    frame_pending: bool,
}

impl RevealState {
    fn progress(&self, now: Instant) -> f32 {
        let elapsed = self
            .started_at
            .map_or(1.0, |started| {
                now.saturating_duration_since(started).as_secs_f32() / REVEAL_DURATION.as_secs_f32()
            })
            .clamp(0.0, 1.0);
        1.0 - (1.0 - elapsed).powi(3)
    }

    fn height(&self, now: Instant) -> Pixels {
        self.from_height + (self.target_height - self.from_height) * self.progress(now)
    }

    fn update(&mut self, updated_at: Option<Instant>, animate: bool, now: Instant) {
        if self.updated_at != updated_at {
            self.from_height = self.height(now);
            self.initial = self.width.is_none();
            self.started_at = updated_at.filter(|updated| {
                animate && now.saturating_duration_since(*updated) < REVEAL_DURATION
            });
            self.updated_at = updated_at;
        }
        if !animate {
            self.started_at = None;
        }
    }
}

pub(super) struct MessageReveal {
    pub(super) child: Option<AnyElement>,
    pub(super) width: Pixels,
    pub(super) harness_id: Id,
    pub(super) message_index: usize,
    pub(super) updated_at: Option<Instant>,
    pub(super) follow_tail: bool,
    pub(super) grow: bool,
    pub(super) entity: WeakEntity<Dirigent>,
}

impl IntoElement for MessageReveal {
    type Element = Self;

    fn into_element(self) -> Self {
        self
    }
}

impl Element for MessageReveal {
    type RequestLayoutState = AnyElement;
    type PrepaintState = ();

    fn id(&self) -> Option<ElementId> {
        Some(format!("message-reveal-{}-{}", self.harness_id, self.message_index).into())
    }

    fn source_location(&self) -> Option<&'static core::panic::Location<'static>> {
        None
    }

    fn request_layout(
        &mut self,
        id: Option<&GlobalElementId>,
        _: Option<&InspectorElementId>,
        window: &mut Window,
        cx: &mut App,
    ) -> (LayoutId, Self::RequestLayoutState) {
        let state = window.with_element_state(id.unwrap(), |state, _| {
            let state = state.unwrap_or_else(|| Rc::new(RefCell::new(RevealState::default())));
            (state.clone(), state)
        });
        let now = Instant::now();
        // Never reflow history beneath someone reading above the tail. Reduced-motion
        // users get the final layout immediately, with no animation frame scheduling.
        let animate = self.follow_tail && !cx.reduce_motion();
        let mut reveal = state.borrow_mut();
        reveal.update(self.updated_at, animate, now);
        let progress = reveal.progress(now);
        let opacity = if reveal.initial { progress } else { 1.0 };
        if progress < 1.0 && !reveal.frame_pending {
            reveal.frame_pending = true;
            let entity = self.entity.clone();
            let harness_id = self.harness_id;
            let message_index = self.message_index;
            let state = state.clone();
            // measure_all also visits offscreen rows. Invalidate their intermediate heights
            // until settled, not just those that happen to reach prepaint in this frame.
            window.on_next_frame(move |_, cx| {
                state.borrow_mut().frame_pending = false;
                let _ = entity.update(cx, |this, cx| {
                    if this.selected_harness == Some(harness_id) {
                        if let Some(index) = this
                            .conversation_render_cache
                            .message_render_item_index(message_index)
                        {
                            this.conversation_list.remeasure_items(index..index + 1);
                        }
                        this.conversation_render_cache.invalidate_ruler_layout();
                        cx.notify();
                    }
                });
            });
        }
        drop(reveal);

        let mut child = div()
            .w_full()
            .opacity(opacity)
            .child(self.child.take().expect("reveal is laid out once"))
            .into_any_element();
        // Measure before requesting our own layout, not from a measured-layout callback:
        // GPUI's layout engine cannot be re-entered from one of its measure functions.
        let mut measured = child.layout_as_root(
            size(
                AvailableSpace::Definite(self.width),
                AvailableSpace::MinContent,
            ),
            window,
            cx,
        );
        let mut reveal = state.borrow_mut();
        if reveal.width.is_some_and(|width| width != measured.width) {
            reveal.started_at = None;
        }
        reveal.width = Some(measured.width);
        reveal.target_height = measured.height;
        // Reveal additions; shrink immediately for Markdown reinterpretation or closing
        // tool details rather than clipping content that is already gone.
        if self.grow && measured.height > reveal.from_height {
            measured.height = reveal.height(now).max(px(1.0));
        }
        let layout = window.request_layout(
            Style {
                size: measured.map(Into::into),
                ..Default::default()
            },
            [],
            cx,
        );
        (layout, child)
    }

    fn prepaint(
        &mut self,
        _: Option<&GlobalElementId>,
        _: Option<&InspectorElementId>,
        bounds: Bounds<Pixels>,
        child: &mut Self::RequestLayoutState,
        window: &mut Window,
        cx: &mut App,
    ) {
        window.with_content_mask(Some(ContentMask { bounds }), |window| {
            child.prepaint_at(bounds.origin, window, cx);
        });
    }

    fn paint(
        &mut self,
        _: Option<&GlobalElementId>,
        _: Option<&InspectorElementId>,
        bounds: Bounds<Pixels>,
        child: &mut Self::RequestLayoutState,
        _: &mut Self::PrepaintState,
        window: &mut Window,
        cx: &mut App,
    ) {
        window.with_content_mask(Some(ContentMask { bounds }), |window| {
            child.paint(window, cx);
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reveals_new_content_without_restarting_existing_text_fade() {
        let now = Instant::now();
        let mut state = RevealState::default();
        state.update(Some(now), true, now);
        state.width = Some(px(600.0));
        state.target_height = px(44.0);
        assert!(state.initial);
        assert_eq!(state.height(now), px(0.0));
        assert_eq!(state.height(now + REVEAL_DURATION), px(44.0));

        let next = now + REVEAL_DURATION;
        state.update(Some(next), true, next);
        state.target_height = px(66.0);
        assert!(!state.initial);
        assert_eq!(state.height(next), px(44.0));
        assert_eq!(state.height(next + REVEAL_DURATION), px(66.0));
        state.update(Some(next), false, next);
        assert_eq!(state.height(next), px(66.0));
    }

    #[test]
    fn history_does_not_animate_when_remounted() {
        let now = Instant::now();
        let mut state = RevealState::default();
        state.update(Some(now - REVEAL_DURATION), true, now);
        state.target_height = px(300.0);
        assert_eq!(state.height(now), px(300.0));
        assert_eq!(state.progress(now), 1.0);
    }
}
