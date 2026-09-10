//! Keeps the docked sidebar preference separate from its temporary hover reveal.

use super::*;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum SidebarState {
    Open,
    Hidden,
    Peeking,
}

impl SidebarState {
    pub(super) fn from_open(open: bool) -> Self {
        if open { Self::Open } else { Self::Hidden }
    }

    // Hover reveals do not change the saved open/hidden preference.
    pub(super) fn is_open(self) -> bool {
        self == Self::Open
    }

    fn toggled(self) -> Self {
        match self {
            Self::Open => Self::Hidden,
            Self::Hidden | Self::Peeking => Self::Open,
        }
    }

    fn with_hover(self, hovered: bool) -> Self {
        match self {
            Self::Open => Self::Open,
            _ if hovered => Self::Peeking,
            _ => Self::Hidden,
        }
    }

    fn layout_width(self, width: f32) -> f32 {
        if self.is_open() { width } else { 0.0 }
    }
}

impl Dirigent {
    pub(crate) fn toggle_sidebar(&mut self) {
        self.sidebar_state = self.sidebar_state.toggled();
        self.sidebar_menu = None;
        self.persist_sidebar_layout();
    }

    pub(crate) fn sidebar_layout_width(&self) -> f32 {
        self.sidebar_state.layout_width(self.sidebar_width)
    }

    pub(crate) fn set_sidebar_hovered(&mut self, hovered: bool, cx: &mut Context<Self>) {
        let state = self.sidebar_state.with_hover(hovered);
        if self.sidebar_state != state {
            self.sidebar_state = state;
            if state == SidebarState::Hidden {
                self.sidebar_menu = None;
                if self.renaming_harness.is_some() {
                    self.enter_normal_mode();
                }
            }
            cx.notify();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::SidebarState::{self, *};

    #[test]
    fn restoring_the_saved_preference_does_not_restore_hover_reveals() {
        assert_eq!(SidebarState::from_open(Open.is_open()), Open);
        assert_eq!(SidebarState::from_open(Hidden.is_open()), Hidden);
        assert_eq!(SidebarState::from_open(Peeking.is_open()), Hidden);
    }

    #[test]
    fn toggle_hides_the_docked_sidebar_and_pins_either_hidden_state() {
        assert_eq!(Open.toggled(), Hidden);
        assert_eq!(Hidden.toggled(), Open);
        assert_eq!(Peeking.toggled(), Open);
    }

    #[test]
    fn hover_only_reveals_a_hidden_sidebar_without_taking_layout_space() {
        assert_eq!(Open.with_hover(true), Open);
        assert_eq!(Open.with_hover(false), Open);
        assert_eq!(Hidden.with_hover(true), Peeking);
        assert_eq!(Peeking.with_hover(true), Peeking);
        assert_eq!(Peeking.with_hover(false), Hidden);
        assert_eq!(Hidden.with_hover(false), Hidden);
        assert_eq!(Open.layout_width(288.0), 288.0);
        assert_eq!(Hidden.layout_width(288.0), 0.0);
        assert_eq!(Peeking.layout_width(288.0), 0.0);
    }
}
