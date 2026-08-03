use super::*;

use crate::diff::{self, DiffScope, DiffViewMode, TurnDiffStatus};

impl Dirigent {
    fn persist_diff_sidebar(&mut self) {
        if let Err(error) = self.state_database.save_diff_sidebar(
            self.diff_sidebar_open,
            self.diff_sidebar_width,
            self.diff_view_mode,
        ) {
            tracing::error!(error = %error, "could not persist diff sidebar state");
            self.banner = Some(error);
        }
    }

    pub(super) fn begin_turn_diff(&mut self, index: usize, prompt: &str) {
        if self.harnesses[index].active_turn_diff.is_some() {
            return;
        }
        let id = self.harnesses[index]
            .turn_diffs
            .iter()
            .map(|turn| turn.id)
            .max()
            .unwrap_or(0)
            + 1;
        let root = match self.working_directory_for_harness(self.harnesses[index].id) {
            Ok(root) => root,
            Err(error) => {
                tracing::warn!(error = %error, harness_id = self.harnesses[index].id, "could not start turn diff");
                return;
            }
        };
        self.harnesses[index].active_turn_diff = Some(diff::begin_turn(id, prompt, &root));
    }

    pub(super) fn mark_turn_diff_status(&mut self, index: usize, status: TurnDiffStatus) {
        if let Some(active) = self.harnesses[index].active_turn_diff.as_mut() {
            active.status_override = Some(status);
        }
    }

    pub(super) fn finish_turn_diff(&mut self, index: usize, status: TurnDiffStatus) {
        let Some(active) = self.harnesses[index].active_turn_diff.take() else {
            return;
        };
        let status = active.status_override.unwrap_or(status);
        let root = match self.working_directory_for_harness(self.harnesses[index].id) {
            Ok(root) => root,
            Err(error) => {
                tracing::warn!(error = %error, harness_id = self.harnesses[index].id, "could not finish turn diff");
                return;
            }
        };
        let turn = diff::finish_turn(active, &root, status);
        let turn_id = turn.id;
        self.harnesses[index].turn_diffs.push(turn);
        self.harnesses[index].turn_diffs.sort_by_key(|turn| turn.id);
        if self.diff_sidebar_open
            && self.selected_harness == Some(self.harnesses[index].id)
            && self
                .selected_diff_turn
                .is_none_or(|(harness_id, selected)| {
                    harness_id != self.harnesses[index].id || selected + 1 == turn_id
                })
        {
            self.selected_diff_turn = Some((self.harnesses[index].id, turn_id));
        }
        self.persist();
    }

    pub(crate) fn toggle_diff_sidebar(&mut self) {
        self.diff_sidebar_open = !self.diff_sidebar_open;
        self.diff_turn_dropdown_open = false;
        if self.diff_sidebar_open {
            self.select_latest_diff_turn();
        }
        self.persist_diff_sidebar();
    }

    pub(crate) fn close_diff_sidebar(&mut self) {
        if self.diff_sidebar_open {
            self.diff_sidebar_open = false;
            self.diff_turn_dropdown_open = false;
            self.persist_diff_sidebar();
        }
    }

    pub(crate) fn resize_diff_sidebar(&mut self, width: f32) {
        let width = width.clamp(420.0, 960.0);
        if self.diff_sidebar_width != width {
            self.diff_sidebar_width = width;
            self.persist_diff_sidebar();
        }
    }

    pub(crate) fn set_diff_view_mode(&mut self, mode: DiffViewMode) {
        if self.diff_view_mode != mode {
            self.diff_view_mode = mode;
            self.diff_list.remeasure();
            self.persist_diff_sidebar();
        }
    }

    pub(crate) fn set_diff_scope(&mut self, scope: DiffScope) {
        if self.diff_scope != scope {
            self.diff_scope = scope;
            self.diff_turn_dropdown_open = false;
            self.diff_display_key = None;
            self.thread_text_selection = None;
        }
    }

    pub(crate) fn toggle_diff_turn_dropdown(&mut self) {
        self.diff_turn_dropdown_open = !self.diff_turn_dropdown_open;
    }

    pub(crate) fn select_diff_turn(&mut self, turn_id: u64) {
        if let Some(harness_id) = self.selected_harness {
            self.selected_diff_turn = Some((harness_id, turn_id));
            self.diff_turn_dropdown_open = false;
            self.diff_display_key = None;
            self.thread_text_selection = None;
        }
    }

    pub(super) fn select_latest_diff_turn(&mut self) {
        let Some(harness) = self
            .selected_harness
            .and_then(|id| self.harnesses.iter().find(|harness| harness.id == id))
        else {
            self.selected_diff_turn = None;
            self.diff_display_key = None;
            return;
        };
        self.selected_diff_turn = harness.turn_diffs.last().map(|turn| (harness.id, turn.id));
        self.diff_display_key = None;
    }

    pub(crate) fn sync_diff_display(&mut self) {
        let Some(harness) = self
            .selected_harness
            .and_then(|id| self.harnesses.iter().find(|harness| harness.id == id))
        else {
            if self.diff_display.take().is_some() {
                self.diff_list.reset(0);
            }
            self.diff_display_key = None;
            return;
        };
        let Some(turn_id) = self.selected_diff_turn_id() else {
            if self.diff_display.take().is_some() {
                self.diff_list.reset(0);
            }
            self.diff_display_key = None;
            return;
        };
        let key = (harness.id, turn_id, self.diff_scope);
        if self.diff_display_key == Some(key) {
            return;
        }
        let Some(index) = harness
            .turn_diffs
            .iter()
            .position(|turn| turn.id == turn_id)
        else {
            return;
        };
        self.diff_display = match self.diff_scope {
            DiffScope::Cumulative => diff::combine_turn_diffs(&harness.turn_diffs[..=index]),
            DiffScope::Turn => Some(harness.turn_diffs[index].clone()),
        };
        self.diff_display_key = Some(key);
        self.diff_list.reset_with_uniform_height(
            self.diff_display
                .as_ref()
                .map_or(0, |turn| turn.files.len()),
            px(300.0),
        );
    }

    pub(crate) fn selected_diff_turn_id(&self) -> Option<u64> {
        let harness = self
            .selected_harness
            .and_then(|id| self.harnesses.iter().find(|harness| harness.id == id))?;
        self.selected_diff_turn
            .filter(|(harness_id, turn_id)| {
                *harness_id == harness.id
                    && harness.turn_diffs.iter().any(|turn| turn.id == *turn_id)
            })
            .map(|(_, turn_id)| turn_id)
            .or_else(|| harness.turn_diffs.last().map(|turn| turn.id))
    }

    pub(super) fn prune_turn_diffs_to_active_branch(&mut self, index: usize) {
        let entries = entries_through_leaf(
            self.harnesses[index]
                .cached_entries
                .as_deref()
                .unwrap_or_default(),
            self.harnesses[index].cached_leaf_id.as_deref(),
        );
        let prompts = entries
            .iter()
            .filter_map(|entry| {
                let message = entry.get("message")?;
                (message.get("role").and_then(Value::as_str) == Some("user"))
                    .then(|| message.get("content"))
                    .flatten()
                    .map(content_text)
            })
            .map(|prompt| diff::prompt_excerpt(&prompt))
            .collect::<HashSet<_>>();
        if prompts.is_empty() {
            return;
        }
        self.harnesses[index]
            .turn_diffs
            .retain(|turn| turn.prompt == "Agent turn" || prompts.contains(&turn.prompt));
        if self.selected_harness == Some(self.harnesses[index].id) {
            self.diff_display_key = None;
        }
    }

    pub(crate) fn add_diff_selection_to_composer(&mut self, cx: &mut Context<Self>) {
        let Some(selection) = self
            .thread_text_selection
            .as_ref()
            .filter(|selection| !selection.range.is_empty())
        else {
            return;
        };
        let Some(reference) = selection.diff_reference.clone() else {
            return;
        };
        let selected = selection.text[selection.range.clone()].to_string();
        let block = format!(
            "\n[Diff: turn {} · {} · {} {}]\n```{}\n{}\n```\n",
            reference.turn_id,
            reference.path,
            reference.side,
            reference.lines,
            reference.language,
            selected
        );
        let Some(input) = self.selected_composer_input() else {
            return;
        };
        input.update(cx, |input, cx| input.insert_at_cursor(&block, cx));
        self.thread_text_selection = None;
        self.enter_input_mode(true);
        cx.notify();
    }
}
