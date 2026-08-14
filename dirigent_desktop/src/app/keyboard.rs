//! Handles keyboard modes, menus, and application shortcuts.

use super::*;

impl Dirigent {
    pub(crate) fn close_keyboard_menu(&mut self) {
        self.keyboard_menu = None;
        self.keyboard_menu_activation = None;
    }
    pub(crate) fn enter_input_mode(&mut self, focus: bool) {
        self.keyboard_mode = KeyboardMode::Input;
        self.close_keyboard_menu();
        self.focus_normal_mode = false;
        self.focus_input = focus;
    }
    pub(crate) fn enter_normal_mode(&mut self) {
        self.keyboard_mode = KeyboardMode::Normal;
        self.close_keyboard_menu();
        self.composer_dropdown = None;
        self.path_completion = None;
        self.focus_input = false;
        self.focus_normal_mode = true;
    }
    pub(crate) fn copy_text(&self, text: String, cx: &mut Context<Self>) {
        cx.write_to_clipboard(ClipboardItem::new_string(text));
    }
    pub(crate) fn copy_text_with_feedback(
        &mut self,
        button_id: String,
        text: String,
        cx: &mut Context<Self>,
    ) {
        self.copy_text(text, cx);
        let copied_at = Instant::now();
        self.copied_button = Some((button_id.clone(), copied_at));
        cx.notify();
        cx.spawn(async move |this, cx| {
            cx.background_executor().timer(Duration::from_secs(1)).await;
            let _ = this.update(cx, |this, cx| {
                if this
                    .copied_button
                    .as_ref()
                    .is_some_and(|(id, at)| id == &button_id && *at == copied_at)
                {
                    this.copied_button = None;
                    cx.notify();
                }
            });
        })
        .detach();
    }
    pub(crate) fn open_image_preview(&mut self, image: Arc<Image>) {
        self.preview_image = Some(image);
    }
    pub(super) fn copy_thread_selection(&self, cx: &mut Context<Self>) -> bool {
        let Some(selection) = self
            .thread_text_selection
            .as_ref()
            .filter(|selection| !selection.range.is_empty())
        else {
            return false;
        };
        cx.write_to_clipboard(ClipboardItem::new_string(
            selection.text[selection.range.clone()].to_string(),
        ));
        true
    }
    /// Dispatches one key within the active prefix menu, preserving the menu on unknown keys.
    pub(super) fn perform_keyboard_menu_key(
        &mut self,
        key: &str,
        shift: bool,
        cx: &mut Context<Self>,
    ) {
        let Some(menu) = self.keyboard_menu.take() else {
            return;
        };
        self.keyboard_menu_activation = None;
        match (menu, key) {
            (KeyboardMenu::Space, "c") => self.start_new_selected_project(),
            (KeyboardMenu::Space, "i") => self.enter_input_mode(true),
            (KeyboardMenu::Space, "a") => self.begin_adding_project(),
            (KeyboardMenu::Space, "t") => self.keyboard_menu = Some(KeyboardMenu::Threads),
            (KeyboardMenu::Space, "p") => self.keyboard_menu = Some(KeyboardMenu::Projects),
            (KeyboardMenu::Space, "x") => self.abort_selected(),
            (KeyboardMenu::Space, "r") if shift => self.restart_selected(),
            (KeyboardMenu::Space, "r") => {
                if let Some(id) = self.selected_harness {
                    self.begin_renaming_harness(id, cx);
                }
            }
            (KeyboardMenu::Space, "n") => self.toggle_nix(),
            (KeyboardMenu::Space, "m") => self.toggle_composer_dropdown(ComposerDropdown::Model),
            (KeyboardMenu::Space, "e") => {
                self.toggle_composer_dropdown(ComposerDropdown::Reasoning)
            }
            (KeyboardMenu::Space, "b") => self.banner = None,
            (KeyboardMenu::Space, "d") if shift => {
                self.debug_panels_visible = !self.debug_panels_visible;
            }
            (KeyboardMenu::Space, "d") => self.toggle_diff_sidebar(),
            (KeyboardMenu::Space, "y") => {
                self.copy_thread_selection(cx);
            }
            (KeyboardMenu::Space, "o") if shift => self.set_all_work_groups_expanded(false),
            (KeyboardMenu::Space, "o") => self.set_all_work_groups_expanded(true),

            (KeyboardMenu::Threads, "c") => self.start_new_selected_project(),
            (KeyboardMenu::Threads, "j" | "n") => self.select_relative_harness(1),
            (KeyboardMenu::Threads, "k" | "p") => self.select_relative_harness(-1),
            (KeyboardMenu::Threads, "g") => self.select_edge_harness(true),
            (KeyboardMenu::Threads, "e") => self.select_edge_harness(false),
            (KeyboardMenu::Threads, "w") => {
                self.select_matching_harness(|harness| harness.status == HarnessStatus::Working)
            }
            (KeyboardMenu::Threads, "u") => {
                self.select_matching_harness(|harness| harness.has_unread_completion)
            }
            (KeyboardMenu::Threads, "x") => self.abort_selected(),
            (KeyboardMenu::Threads, "r") if shift => self.restart_selected(),
            (KeyboardMenu::Threads, "i") => self.enter_input_mode(true),

            (KeyboardMenu::Projects, "a") => self.begin_adding_project(),
            (KeyboardMenu::Projects, "c") => self.start_new_selected_project(),
            (KeyboardMenu::Projects, "j" | "n") => self.select_relative_project(1),
            (KeyboardMenu::Projects, "k" | "p") => self.select_relative_project(-1),
            (KeyboardMenu::Projects, "g") => self.select_edge_project(true),
            (KeyboardMenu::Projects, "e") => self.select_edge_project(false),
            (KeyboardMenu::Projects, "[") => self.resize_sidebar(self.sidebar_width - 24.0, cx),
            (KeyboardMenu::Projects, "]") => self.resize_sidebar(self.sidebar_width + 24.0, cx),

            (KeyboardMenu::Goto, "g") => self.scroll_conversation_to_fraction(0.0),
            (KeyboardMenu::Goto, "e") => self.scroll_conversation_to_fraction(1.0),
            (KeyboardMenu::Goto, "c") => self.enter_input_mode(true),
            (KeyboardMenu::Goto, "j" | "n") => self.select_relative_harness(1),
            (KeyboardMenu::Goto, "k" | "p") => self.select_relative_harness(-1),
            (KeyboardMenu::Goto, "h") => self.select_edge_harness(true),
            (KeyboardMenu::Goto, "l") => self.select_edge_harness(false),
            (KeyboardMenu::Goto, "w") => {
                self.select_matching_harness(|harness| harness.status == HarnessStatus::Working)
            }
            (KeyboardMenu::Goto, "u") => {
                self.select_matching_harness(|harness| harness.has_unread_completion)
            }
            (KeyboardMenu::Goto, "]") => self.select_relative_project(1),
            (KeyboardMenu::Goto, "[") => self.select_relative_project(-1),
            _ => self.keyboard_menu = Some(menu),
        }
    }
    pub(super) fn on_root_key_up(
        &mut self,
        event: &KeyUpEvent,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let activation_released = matches!(
            (self.keyboard_menu_activation, event.keystroke.key.as_str()),
            (Some(KeyboardMenu::Space), "space") | (Some(KeyboardMenu::Goto), "g")
        );
        if activation_released {
            self.keyboard_menu_activation = None;
            cx.stop_propagation();
        }
    }
    pub(super) fn on_root_key_down(
        &mut self,
        event: &KeyDownEvent,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let key = event.keystroke.key.as_str();
        let modifiers = event.keystroke.modifiers;
        let command = modifiers.control || modifiers.platform;

        if key == "escape" {
            self.preview_image = None;
            self.pending_workspace_deletion = None;
            self.enter_normal_mode();
            cx.stop_propagation();
            cx.notify();
            return;
        }

        if self.keyboard_menu_activation.is_some() {
            // Ignore key-repeat and chord input until the prefix key is released; otherwise the
            // activation key itself could immediately be interpreted as a menu command.
            cx.stop_propagation();
            return;
        }

        if self.keyboard_menu.is_some() && !command && !modifiers.alt {
            self.perform_keyboard_menu_key(key, modifiers.shift, cx);
            cx.stop_propagation();
            cx.notify();
            return;
        }

        if self.keyboard_mode == KeyboardMode::Normal {
            let handled = match key {
                "space" if !command && !modifiers.alt => {
                    self.keyboard_menu = Some(KeyboardMenu::Space);
                    self.keyboard_menu_activation = Some(KeyboardMenu::Space);
                    true
                }
                "g" if !command && !modifiers.alt && !modifiers.shift => {
                    self.keyboard_menu = Some(KeyboardMenu::Goto);
                    self.keyboard_menu_activation = Some(KeyboardMenu::Goto);
                    true
                }
                "g" if !command && modifiers.shift => {
                    self.scroll_conversation_to_fraction(1.0);
                    true
                }
                "i" if !command && !modifiers.alt => {
                    self.enter_input_mode(true);
                    true
                }
                "[" if !command && !modifiers.alt => {
                    self.resize_sidebar(self.sidebar_width - 24.0, cx);
                    true
                }
                "]" if !command && !modifiers.alt => {
                    self.resize_sidebar(self.sidebar_width + 24.0, cx);
                    true
                }
                "j" | "down" if !command && !modifiers.alt => {
                    self.scroll_conversation_by_fraction(0.06);
                    true
                }
                "k" | "up" if !command && !modifiers.alt => {
                    self.scroll_conversation_by_fraction(-0.06);
                    true
                }
                "d" if modifiers.control => {
                    self.scroll_conversation_by_fraction(0.5);
                    true
                }
                "u" if modifiers.control => {
                    self.scroll_conversation_by_fraction(-0.5);
                    true
                }
                _ => false,
            };
            if handled {
                cx.stop_propagation();
                cx.notify();
                return;
            }
        }

        if key == "c" && command && self.copy_thread_selection(cx) {
            cx.stop_propagation();
        }
    }
}
