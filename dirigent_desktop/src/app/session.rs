//! Rebuilds canonical session messages without monopolizing the GPUI thread.

use super::*;

const SESSION_MATERIALIZE_CHUNK_SIZE: usize = 128;
const SESSION_MATERIALIZE_YIELD: Duration = Duration::from_millis(1);

impl Dirigent {
    pub(super) fn request_cached_session_rebuild(&mut self, harness_index: usize) {
        let harness = &self.harnesses[harness_index];
        if harness.loaded_messages
            || !harness.messages.is_empty()
            || self.pending_session_rebuilds.contains_key(&harness.id)
            || self.requested_session_rebuilds.contains(&harness.id)
        {
            return;
        }
        let Some(entries) = harness.cached_entries.as_ref() else {
            return;
        };
        let request = CachedSessionRebuildRequest {
            harness_id: harness.id,
            entries: Arc::clone(entries),
            leaf_id: harness.cached_leaf_id.clone(),
        };
        if self.cached_session_rebuilds.try_send(request).is_ok() {
            self.requested_session_rebuilds.insert(harness.id);
        }
    }

    pub(super) fn request_delegated_session_rebuilds(&mut self, parent: Id) {
        let child_indices = self
            .harnesses
            .iter()
            .enumerate()
            .filter_map(|(index, harness)| {
                (harness.delegation.parent == Some(parent)).then_some(index)
            })
            .collect::<Vec<_>>();
        for index in child_indices {
            self.request_cached_session_rebuild(index);
        }
    }

    pub(super) fn queue_legacy_message_rebuild(
        &mut self,
        harness_index: usize,
        messages: Vec<Value>,
        cx: &mut Context<Self>,
    ) {
        let harness_id = self.harnesses[harness_index].id;
        let generation = self.harnesses[harness_index].process_generation;
        let entries_task = cx.background_spawn(async move {
            let mut parent_id: Option<String> = None;
            let mut entries = Vec::with_capacity(messages.len());
            for (index, message) in messages.into_iter().enumerate() {
                let id = format!("legacy:{index}");
                entries.push(json!({
                    "id": id,
                    "parentId": parent_id,
                    "type": "message",
                    "message": message,
                }));
                parent_id = Some(id);
            }
            (Arc::new(entries), parent_id)
        });
        cx.spawn(async move |this, cx| {
            let (entries, leaf_id) = entries_task.await;
            let _ = this.update(cx, |this, cx| {
                let Some(index) = this.harnesses.iter().position(|harness| {
                    harness.id == harness_id && harness.process_generation == generation
                }) else {
                    return;
                };
                this.queue_session_rebuild(
                    index,
                    entries,
                    leaf_id,
                    Some(generation),
                    "legacy_messages",
                    false,
                    false,
                    cx,
                );
            });
        })
        .detach();
    }

    #[allow(clippy::too_many_arguments)]
    pub(super) fn queue_session_rebuild(
        &mut self,
        harness_index: usize,
        entries: Arc<Vec<Value>>,
        leaf_id: Option<String>,
        expected_generation: Option<u64>,
        reason: &'static str,
        trim_to_active_branch: bool,
        cache_after_rebuild: bool,
        cx: &mut Context<Self>,
    ) {
        let harness_id = self.harnesses[harness_index].id;
        let job_id = self.next_session_rebuild_job_id;
        self.next_session_rebuild_job_id += 1;
        self.pending_session_rebuilds.insert(harness_id, job_id);
        let scroll_anchor = (self.selected_harness == Some(harness_id))
            .then(|| self.conversation_scroll_anchor(harness_index))
            .flatten();
        let background_leaf_id = leaf_id.clone();
        let traversal_task = cx.background_spawn(async move {
            let started = Instant::now();
            let active_indices = active_entry_indices(&entries, background_leaf_id.as_deref());
            // Turn records predate direct entry linkage, so prompt excerpts are the stable key
            // available for reconciling them after navigation or a fork.
            let prompts = active_indices
                .iter()
                .filter_map(|&entry_index| {
                    let message = entries[entry_index].get("message")?;
                    (message.get("role").and_then(Value::as_str) == Some("user"))
                        .then(|| message.get("content"))
                        .flatten()
                        .map(content_text)
                })
                .map(|prompt| crate::diff::prompt_excerpt(&prompt))
                .collect::<HashSet<_>>();
            let (entries, active_indices) = if trim_to_active_branch {
                let active_entries = active_indices
                    .into_iter()
                    .map(|index| entries[index].clone())
                    .collect::<Vec<_>>();
                let entries = Arc::new(active_entries);
                let active_indices = (0..entries.len()).collect();
                (entries, active_indices)
            } else {
                (entries, active_indices)
            };
            (entries, active_indices, prompts, started.elapsed())
        });

        cx.spawn(async move |this, cx| {
            let (entries, active_indices, prompts, traversal_elapsed) = traversal_task.await;
            let current = this
                .update(cx, |this, _| {
                    this.pending_session_rebuilds.get(&harness_id) == Some(&job_id)
                        && expected_generation.is_none_or(|generation| {
                            this.harnesses.iter().any(|harness| {
                                harness.id == harness_id && harness.process_generation == generation
                            })
                        })
                })
                .unwrap_or(false);
            if !current {
                return;
            }

            let materialize_started = Instant::now();
            let mut parser = EntryMessageParser::new(None, None);
            let mut chunks = 0_usize;
            for chunk in active_indices.chunks(SESSION_MATERIALIZE_CHUNK_SIZE) {
                let current = this
                    .update(cx, |this, _| {
                        this.pending_session_rebuilds.get(&harness_id) == Some(&job_id)
                    })
                    .unwrap_or(false);
                if !current {
                    return;
                }
                for &entry_index in chunk {
                    parser.push(&entries[entry_index]);
                }
                chunks += 1;
                cx.background_executor()
                    .timer(SESSION_MATERIALIZE_YIELD)
                    .await;
            }
            let mut parsed = parser.finish();
            let materialize_elapsed = materialize_started.elapsed();

            let _ = this.update(cx, |this, cx| {
                if this.pending_session_rebuilds.get(&harness_id) != Some(&job_id) {
                    return;
                }
                let Some(index) = this.harnesses.iter().position(|harness| {
                    harness.id == harness_id
                        && expected_generation
                            .is_none_or(|generation| harness.process_generation == generation)
                }) else {
                    return;
                };
                this.pending_session_rebuilds.remove(&harness_id);
                let apply_started = Instant::now();
                let previous_messages = std::mem::take(&mut this.harnesses[index].messages);
                let expansion_changed = reconcile_work_group_expansion(
                    &previous_messages,
                    &parsed.messages,
                    &mut this.harnesses[index].work_group_expansion,
                );
                preserve_streamed_at(&previous_messages, &mut parsed.messages);
                this.harnesses[index].messages = parsed.messages;
                this.harnesses[index].canonical_message_count =
                    this.harnesses[index].messages.len();
                this.harnesses[index].canonical_model = parsed.model;
                this.harnesses[index].canonical_thinking_level = parsed.thinking_level;
                this.harnesses[index].cached_entries = Some(entries);
                this.harnesses[index].cached_leaf_id = leaf_id;
                this.harnesses[index].loaded_messages = true;
                this.prune_turn_diffs_to_prompts(index, &prompts);
                if expansion_changed {
                    this.persist();
                }
                let cache_started = Instant::now();
                if cache_after_rebuild {
                    this.cache_harness_entries(index);
                }
                let cache_elapsed = cache_started.elapsed();
                let conversation_started = Instant::now();
                if this.selected_harness == Some(harness_id) {
                    this.reset_conversation_list_preserving_scroll(index, scroll_anchor);
                }
                let conversation_elapsed = conversation_started.elapsed();
                tracing::info!(
                    harness_id,
                    job_id,
                    reason,
                    entry_count = this.harnesses[index]
                        .cached_entries
                        .as_ref()
                        .map_or(0, |entries| entries.len()),
                    message_count = this.harnesses[index].messages.len(),
                    chunks,
                    traversal_us = duration_us(traversal_elapsed),
                    materialize_us = duration_us(materialize_elapsed),
                    apply_us = duration_us(apply_started.elapsed()),
                    cache_queue_us = duration_us(cache_elapsed),
                    conversation_sync_us = duration_us(conversation_elapsed),
                    "session rebuild timing"
                );
                cx.notify();
            });
        })
        .detach();
    }
}
