//! Coordinates asynchronous turn-diff capture, highlighting, and selection.

use super::*;

use crate::diff::{self, ActiveTurnDiff, DiffScope, DiffViewMode, TurnDiff, TurnDiffStatus};

pub(super) struct TurnHighlightTask {
    generation: u64,
    key: DiffDisplayKey,
    turn: TurnDiff,
}

pub(super) struct TurnHighlightResult {
    generation: u64,
    key: DiffDisplayKey,
    turn: TurnDiff,
}

/// Recomputes theme-dependent syntax spans for the visible diff away from the UI thread.
pub(super) fn run_turn_highlight_worker(
    tasks: async_channel::Receiver<TurnHighlightTask>,
    results: async_channel::Sender<TurnHighlightResult>,
    current_generation: Arc<AtomicU64>,
) {
    while let Ok(mut task) = tasks.recv_blocking() {
        if task.generation != current_generation.load(Ordering::Relaxed) {
            continue;
        }
        let started = Instant::now();
        task.turn.refresh_highlights();
        if task.generation != current_generation.load(Ordering::Relaxed) {
            continue;
        }
        let elapsed = started.elapsed();
        if elapsed >= Duration::from_millis(100) {
            tracing::info!(
                harness_id = task.key.0,
                turn_id = task.key.1,
                scope = ?task.key.2,
                elapsed_ms = elapsed.as_millis(),
                "asynchronous diff display highlighting completed"
            );
        }
        if results
            .send_blocking(TurnHighlightResult {
                generation: task.generation,
                key: task.key,
                turn: task.turn,
            })
            .is_err()
        {
            break;
        }
    }
}

pub(super) enum DiffTask {
    Begin {
        job_id: u64,
        harness_id: Id,
        process_generation: u64,
        turn_id: u64,
        prompt: String,
        root: PathBuf,
        awaiting_prompt: bool,
    },
    Preview {
        job_id: u64,
        harness_id: Id,
        turn: ActiveTurnDiff,
        root: PathBuf,
    },
    Finish {
        harness_id: Id,
        turn: ActiveTurnDiff,
        root: PathBuf,
        status: TurnDiffStatus,
    },
}

pub(super) enum DiffTaskResult {
    Began {
        job_id: u64,
        harness_id: Id,
        process_generation: u64,
        awaiting_prompt: bool,
        active: Result<Option<ActiveTurnDiff>, String>,
    },
    Previewed {
        job_id: u64,
        harness_id: Id,
        turn: TurnDiff,
    },
    Finished {
        harness_id: Id,
        turn: TurnDiff,
    },
}

/// Serializes repository checkpoint work on a dedicated worker thread.
pub(super) fn run_diff_worker(
    tasks: async_channel::Receiver<DiffTask>,
    results: async_channel::Sender<DiffTaskResult>,
) {
    while let Ok(task) = tasks.recv_blocking() {
        let result = match task {
            DiffTask::Begin {
                job_id,
                harness_id,
                process_generation,
                turn_id,
                prompt,
                root,
                awaiting_prompt,
            } => DiffTaskResult::Began {
                job_id,
                harness_id,
                process_generation,
                awaiting_prompt,
                active: diff::begin_turn(turn_id, &prompt, &root),
            },
            DiffTask::Preview {
                job_id,
                harness_id,
                turn,
                root,
            } => DiffTaskResult::Previewed {
                job_id,
                harness_id,
                turn: diff::preview_turn(&turn, &root),
            },
            DiffTask::Finish {
                harness_id,
                turn,
                root,
                status,
            } => DiffTaskResult::Finished {
                harness_id,
                turn: diff::finish_turn(turn, &root, status),
            },
        };
        if results.send_blocking(result).is_err() {
            break;
        }
    }
}

impl Dirigent {
    /// Invalidates only the disposable syntax cache. Persisted turn data remains untouched.
    pub(super) fn invalidate_diff_display_highlights(&mut self) {
        let generation = self
            .turn_highlight_generation
            .load(Ordering::Relaxed)
            .wrapping_add(1)
            .max(1);
        self.turn_highlight_generation
            .store(generation, Ordering::Relaxed);
        self.pending_turn_highlight = None;
        self.highlighted_diff_display = None;
    }

    fn queue_diff_display_highlights(&mut self, key: DiffDisplayKey) {
        let generation = self.turn_highlight_generation.load(Ordering::Relaxed);
        let request = (generation, key);
        if self.pending_turn_highlight == Some(request)
            || self.highlighted_diff_display == Some(request)
        {
            return;
        }
        let Some(turn) = self.diff_display.clone() else {
            return;
        };
        if self
            .turn_highlight_tasks
            .try_send(TurnHighlightTask {
                generation,
                key,
                turn,
            })
            .is_err()
        {
            tracing::warn!("could not queue asynchronous diff display highlighting");
            return;
        }
        self.pending_turn_highlight = Some(request);
    }

    pub(super) fn handle_turn_highlight_result(&mut self, result: TurnHighlightResult) {
        let request = (result.generation, result.key);
        if result.generation != self.turn_highlight_generation.load(Ordering::Relaxed)
            || self.diff_display_key != Some(result.key)
            || self.pending_turn_highlight != Some(request)
        {
            return;
        }
        self.pending_turn_highlight = None;
        self.highlighted_diff_display = Some(request);
        self.diff_display = Some(result.turn);
        self.rebuild_diff_render_cache();
    }

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

    fn allocate_diff_job_id(&mut self) -> u64 {
        let id = self.next_diff_job_id;
        self.next_diff_job_id = self.next_diff_job_id.wrapping_add(1).max(1);
        id
    }

    /// Starts checkpoint capture and optionally holds the prompt until that checkpoint exists.
    pub(super) fn queue_turn_diff_baseline(
        &mut self,
        index: usize,
        prompt: &str,
        awaiting_prompt: bool,
    ) -> Option<u64> {
        let harness_id = self.harnesses[index].id;
        if self.harnesses[index].active_turn_diff.is_some()
            || self.pending_diff_prompts.contains_key(&harness_id)
        {
            return None;
        }
        let root = match self.working_directory_for_harness(harness_id) {
            Ok(root) => root,
            Err(error) => {
                tracing::warn!(error = %error, harness_id, "could not start turn diff");
                return None;
            }
        };
        let job_id = self.allocate_diff_job_id();
        let turn_id = self.harnesses[index].next_turn_diff_id;
        let task = DiffTask::Begin {
            job_id,
            harness_id,
            process_generation: self.harnesses[index].process_generation,
            turn_id,
            prompt: prompt.to_string(),
            root,
            awaiting_prompt,
        };
        if self.diff_tasks.try_send(task).is_err() {
            tracing::warn!(harness_id, "could not queue turn diff baseline");
            return None;
        }
        self.harnesses[index].next_turn_diff_id += 1;
        self.harnesses[index].active_turn_preview = None;
        Some(job_id)
    }

    pub(super) fn begin_turn_diff(&mut self, index: usize, prompt: &str) {
        if !self.harnesses[index].turn_diff_unavailable {
            let _ = self.queue_turn_diff_baseline(index, prompt, false);
        }
    }

    pub(super) fn refresh_active_turn_diff(&mut self, index: usize) {
        if !self.diff_sidebar_open {
            return;
        }
        let harness_id = self.harnesses[index].id;
        if self.pending_diff_previews.contains_key(&harness_id) {
            // At most one expensive preview runs per harness. One dirty bit is enough because
            // each preview captures the complete endpoint state rather than an incremental diff.
            self.dirty_diff_previews.insert(harness_id);
            return;
        }
        let Some(active) = self.harnesses[index].active_turn_diff.clone() else {
            return;
        };
        let root = match self.working_directory_for_harness(harness_id) {
            Ok(root) => root,
            Err(error) => {
                tracing::warn!(error = %error, harness_id, "could not refresh active turn diff");
                return;
            }
        };
        let job_id = self.allocate_diff_job_id();
        if self
            .diff_tasks
            .try_send(DiffTask::Preview {
                job_id,
                harness_id,
                turn: active,
                root,
            })
            .is_ok()
        {
            self.pending_diff_previews.insert(harness_id, job_id);
        }
    }

    pub(super) fn mark_turn_diff_status(&mut self, index: usize, status: TurnDiffStatus) {
        if let Some(active) = self.harnesses[index].active_turn_diff.as_mut() {
            active.status_override = Some(status);
        }
    }

    pub(super) fn finish_turn_diff(&mut self, index: usize, status: TurnDiffStatus) {
        let harness_id = self.harnesses[index].id;
        self.harnesses[index].turn_diff_unavailable = false;
        self.harnesses[index].active_turn_preview = None;
        self.pending_diff_previews.remove(&harness_id);
        self.dirty_diff_previews.remove(&harness_id);
        if self.selected_harness == Some(harness_id) {
            self.diff_display_key = None;
        }
        let Some(active) = self.harnesses[index].active_turn_diff.take() else {
            return;
        };
        let status = active.status_override.unwrap_or(status);
        let root = match self.working_directory_for_harness(harness_id) {
            Ok(root) => root,
            Err(error) => {
                tracing::warn!(error = %error, harness_id, "could not finish turn diff");
                return;
            }
        };
        if self
            .diff_tasks
            .try_send(DiffTask::Finish {
                harness_id,
                turn: active,
                root,
                status,
            })
            .is_err()
        {
            tracing::warn!(harness_id, "could not queue turn diff completion");
        }
    }

    fn apply_finished_turn_diff(&mut self, index: usize, turn: TurnDiff) {
        let harness_id = self.harnesses[index].id;
        let turn_id = turn.id;
        let turn = Arc::new(turn);
        if let Err(error) = self.state_database.save_turn_diff(harness_id, turn.clone()) {
            tracing::error!(error = %error, harness_id, turn_id, "could not queue turn diff persistence");
            self.banner = Some(error);
        }
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
        if self.selected_harness == Some(harness_id) {
            // Turn completion is infrequent, so rebuild the lightweight summary cache once and
            // let the exact diff replace its per-tool estimate. Keeping the rebuild point past
            // the message tail avoids remeasuring unchanged conversation messages.
            let rebuild_from_message = self.harnesses[index].messages.len();
            self.sync_conversation_render_cache(rebuild_from_message);
        }
    }

    pub(super) fn handle_diff_task_result(&mut self, result: DiffTaskResult) {
        match result {
            DiffTaskResult::Began {
                job_id,
                harness_id,
                process_generation,
                awaiting_prompt,
                active,
            } => {
                let Some(index) = self.harnesses.iter().position(|harness| {
                    harness.id == harness_id && harness.process_generation == process_generation
                }) else {
                    if self
                        .pending_diff_prompts
                        .get(&harness_id)
                        .is_some_and(|pending| pending.job_id == job_id)
                    {
                        self.pending_diff_prompts.remove(&harness_id);
                    }
                    return;
                };
                let pending_job = self
                    .pending_diff_prompts
                    .get(&harness_id)
                    .map(|pending| (pending.job_id, pending.process_generation));
                // Prompt-holding captures must match exactly. Opportunistic captures are also
                // obsolete if a newer prompt capture has claimed the harness.
                let stale = if awaiting_prompt {
                    pending_job != Some((job_id, process_generation))
                } else {
                    pending_job.is_some_and(|pending| pending != (job_id, process_generation))
                };
                if stale {
                    return;
                }
                match active {
                    Ok(Some(active)) if self.harnesses[index].active_turn_diff.is_none() => {
                        self.harnesses[index].turn_diff_unavailable = false;
                        self.harnesses[index].active_turn_diff = Some(active);
                    }
                    Ok(Some(_)) => {}
                    Ok(None) => self.harnesses[index].turn_diff_unavailable = true,
                    Err(error) => {
                        self.harnesses[index].turn_diff_unavailable = true;
                        tracing::warn!(error = %error, harness_id, "could not capture turn diff baseline");
                    }
                }
                if !awaiting_prompt {
                    return;
                }
                let pending = self
                    .pending_diff_prompts
                    .remove(&harness_id)
                    .expect("matching pending diff prompt exists");
                let baseline_elapsed = pending.started_at.elapsed();
                let mut sent = false;
                for command in pending.commands {
                    if !self.send_value(index, command) {
                        break;
                    }
                    sent = true;
                }
                if sent {
                    self.mark_harness_working(index);
                }
                tracing::info!(
                    harness_id,
                    outcome = if sent { "sent" } else { "send_error" },
                    total_ms = baseline_elapsed.as_millis().min(u64::MAX as u128) as u64,
                    turn_diff_capture_ms =
                        baseline_elapsed.as_millis().min(u64::MAX as u128) as u64,
                    "asynchronous prompt baseline timing"
                );
            }
            DiffTaskResult::Previewed {
                job_id,
                harness_id,
                turn,
            } => {
                if self.pending_diff_previews.get(&harness_id) != Some(&job_id) {
                    return;
                }
                self.pending_diff_previews.remove(&harness_id);
                let Some(index) = self.harnesses.iter().position(|harness| {
                    harness.id == harness_id
                        && harness.active_turn_diff.as_ref().map(|active| active.id)
                            == Some(turn.id)
                }) else {
                    return;
                };
                let preview_id = turn.id;
                let latest_completed = self.harnesses[index].turn_diffs.last().map(|turn| turn.id);
                let follows_latest =
                    self.selected_diff_turn
                        .is_none_or(|(selected_harness, selected_turn)| {
                            selected_harness != harness_id
                                || Some(selected_turn) == latest_completed
                                || selected_turn == preview_id
                        });
                self.harnesses[index].active_turn_preview = Some(turn);
                if self.selected_harness == Some(harness_id) {
                    if follows_latest {
                        self.selected_diff_turn = Some((harness_id, preview_id));
                    }
                    self.diff_display_key = None;
                }
                if self.dirty_diff_previews.remove(&harness_id) {
                    self.refresh_active_turn_diff(index);
                }
            }
            DiffTaskResult::Finished { harness_id, turn } => {
                if let Some(index) = self
                    .harnesses
                    .iter()
                    .position(|harness| harness.id == harness_id)
                {
                    self.apply_finished_turn_diff(index, turn);
                }
            }
        }
    }

    pub(crate) fn selected_harness_supports_turn_diffs(&self) -> bool {
        let Some(harness) = self
            .selected_harness
            .and_then(|id| self.harnesses.iter().find(|harness| harness.id == id))
        else {
            return false;
        };
        harness.workspace_id.is_some()
            || self.repository_snapshots.contains_key(&harness.project_id)
            || harness.active_turn_diff.is_some()
            || !harness.turn_diffs.is_empty()
            || self.pending_diff_prompts.contains_key(&harness.id)
    }

    pub(crate) fn toggle_diff_sidebar(&mut self) {
        if !self.diff_sidebar_open && !self.selected_harness_supports_turn_diffs() {
            self.banner = Some("Turn diffs require a Git or JJ repository.".into());
            return;
        }
        self.diff_sidebar_open = !self.diff_sidebar_open;
        self.diff_turn_dropdown_open = false;
        if self.diff_sidebar_open {
            self.select_latest_diff_turn();
        } else {
            self.invalidate_diff_display_highlights();
        }
        self.persist_diff_sidebar();
    }

    pub(crate) fn close_diff_sidebar(&mut self) {
        if self.diff_sidebar_open {
            self.diff_sidebar_open = false;
            self.diff_turn_dropdown_open = false;
            self.invalidate_diff_display_highlights();
            self.persist_diff_sidebar();
        }
    }

    pub(crate) fn resize_diff_sidebar(&mut self, width: f32, cx: &mut Context<Self>) {
        let width = width.clamp(420.0, 1_600.0);
        if self.diff_sidebar_width != width {
            self.diff_sidebar_width = width;
            self.schedule_sidebar_layout_persist(cx);
        }
    }

    pub(crate) fn set_diff_view_mode(&mut self, mode: DiffViewMode) {
        if self.diff_view_mode != mode {
            self.diff_view_mode = mode;
            self.rebuild_diff_render_cache();
            self.thread_text_selection = None;
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
        self.selected_diff_turn = harness
            .active_turn_preview
            .as_ref()
            .or_else(|| harness.turn_diffs.last().map(Arc::as_ref))
            .map(|turn| (harness.id, turn.id));
        self.diff_display_key = None;
    }

    fn clear_diff_display(&mut self) {
        let had_display = self.diff_display.take().is_some();
        if self.diff_display_key.take().is_some()
            || self.pending_turn_highlight.is_some()
            || self.highlighted_diff_display.is_some()
        {
            self.invalidate_diff_display_highlights();
        }
        if had_display {
            self.rebuild_diff_render_cache();
        }
    }

    pub(crate) fn sync_diff_display(&mut self) {
        let Some(harness) = self
            .selected_harness
            .and_then(|id| self.harnesses.iter().find(|harness| harness.id == id))
        else {
            self.clear_diff_display();
            return;
        };
        let Some(turn_id) = self.selected_diff_turn_id() else {
            self.clear_diff_display();
            return;
        };
        let key = (harness.id, turn_id, self.diff_scope);
        if self.diff_display_key == Some(key) {
            self.queue_diff_display_highlights(key);
            return;
        }
        let display = if let Some(preview) = harness
            .active_turn_preview
            .as_ref()
            .filter(|preview| preview.id == turn_id)
        {
            match self.diff_scope {
                DiffScope::Cumulative => {
                    let mut turns = harness.turn_diffs.clone();
                    turns.push(Arc::new(preview.clone()));
                    diff::combine_turn_diffs(&turns)
                }
                DiffScope::Turn => Some(preview.clone()),
            }
        } else {
            let Some(index) = harness
                .turn_diffs
                .iter()
                .position(|turn| turn.id == turn_id)
            else {
                self.clear_diff_display();
                return;
            };
            match self.diff_scope {
                DiffScope::Cumulative => diff::combine_turn_diffs(&harness.turn_diffs[..=index]),
                DiffScope::Turn => Some(harness.turn_diffs[index].as_ref().clone()),
            }
        };
        self.invalidate_diff_display_highlights();
        self.diff_display = display;
        self.diff_display_key = Some(key);
        self.rebuild_diff_render_cache();
        self.queue_diff_display_highlights(key);
    }

    pub(crate) fn selected_diff_turn_id(&self) -> Option<u64> {
        let harness = self
            .selected_harness
            .and_then(|id| self.harnesses.iter().find(|harness| harness.id == id))?;
        self.selected_diff_turn
            .filter(|(harness_id, turn_id)| {
                *harness_id == harness.id
                    && (harness.turn_diffs.iter().any(|turn| turn.id == *turn_id)
                        || harness
                            .active_turn_preview
                            .as_ref()
                            .is_some_and(|preview| preview.id == *turn_id))
            })
            .map(|(_, turn_id)| turn_id)
            .or_else(|| {
                harness
                    .active_turn_preview
                    .as_ref()
                    .or_else(|| harness.turn_diffs.last().map(Arc::as_ref))
                    .map(|turn| turn.id)
            })
    }

    /// Drops persisted turns that no longer correspond to the active Pi session branch.
    pub(super) fn prune_turn_diffs_to_active_branch(&mut self, index: usize) {
        let entries = entries_through_leaf(
            self.harnesses[index]
                .cached_entries
                .as_deref()
                .unwrap_or_default(),
            self.harnesses[index].cached_leaf_id.as_deref(),
        );
        // Turn records predate direct entry linkage, so normalized prompt excerpts are the stable
        // key available for reconciling them after navigation or a fork.
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
        let previous_len = self.harnesses[index].turn_diffs.len();
        self.harnesses[index]
            .turn_diffs
            .retain(|turn| turn.prompt == "Agent turn" || prompts.contains(&turn.prompt));
        if self.harnesses[index].turn_diffs.len() != previous_len {
            let harness_id = self.harnesses[index].id;
            let retained = self.harnesses[index]
                .turn_diffs
                .iter()
                .map(|turn| turn.id)
                .collect();
            if let Err(error) = self.state_database.retain_turn_diffs(harness_id, retained) {
                tracing::error!(error = %error, harness_id, "could not queue turn diff pruning");
                self.banner = Some(error);
            }
        }
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
