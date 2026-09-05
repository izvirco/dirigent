//! Restores persisted state and wires the application's background workers.

use super::*;

impl Dirigent {
    pub(crate) fn new(cx: &mut Context<Self>) -> Self {
        // Initialize persistent state before the rest of app startup reads from disk.
        let (state_database, loaded) = storage::StateDatabase::open()
            .unwrap_or_else(|error| panic!("could not initialize persistent state: {error}"));
        let (config_dir, appearance, mut config_error) = match theme::initialize() {
            Ok((config_dir, appearance)) => (Some(config_dir), appearance, None),
            Err(error) => {
                tracing::error!(error = %error, "could not initialize theme configuration");
                (
                    platform::config_dir().ok(),
                    theme::default_appearance(),
                    Some(error),
                )
            }
        };
        if let Some(config_dir) = config_dir {
            let (config_tx, config_rx) = async_channel::unbounded();
            if let Err(error) = theme::watch(config_dir.clone(), config_tx) {
                tracing::error!(error = %error, path = %config_dir.display(), "could not watch theme configuration");
                config_error = Some(error);
            } else {
                cx.spawn(async move |this, cx| {
                    while let Ok(event) = config_rx.recv().await {
                        let appearance = event.and_then(|()| theme::reload(&config_dir));
                        if this
                            .update(cx, |this, cx| {
                                match appearance {
                                    Ok(appearance) => this.apply_appearance(appearance, cx),
                                    Err(error) => {
                                        tracing::error!(error = %error, "could not reload theme configuration");
                                        this.config_error = Some(error.clone());
                                        this.banner = Some(error);
                                    }
                                }
                                cx.notify();
                            })
                            .is_err()
                        {
                            break;
                        }
                    }
                })
                .detach();
            }
        }

        let project_input = cx.new(|cx| {
            TextInput::new(project_path_placeholder(), cx)
                .borderless()
                .compact()
        });
        let harness_input = cx.new(|cx| {
            TextInput::new(COMPOSER_PLACEHOLDER, cx)
                .borderless()
                .multiline()
        });
        let extension_input = cx.new(|cx| TextInput::new("Enter a value…", cx));
        let thread_rename_input =
            cx.new(|cx| TextInput::new("Thread name", cx).borderless().compact());
        let workspace_settings_input = cx.new(|cx| {
            TextInput::new("/path/to/workspaces", cx)
                .borderless()
                .compact()
        });

        cx.subscribe(&project_input, |this, _, event, cx| match event {
            InputEvent::Submit => this.add_project(cx),
            InputEvent::Focused => {
                this.enter_input_mode(false);
                this.refresh_project_path_completion(cx);
                cx.notify();
            }
            InputEvent::Escape => {
                if this.path_completion.is_some() {
                    this.path_completion = None;
                } else {
                    this.enter_normal_mode();
                }
                cx.notify();
            }
            InputEvent::Changed => this.refresh_project_path_completion(cx),
            InputEvent::CompletionPrevious => this.move_path_completion(-1, cx),
            InputEvent::CompletionNext => this.move_path_completion(1, cx),
            InputEvent::CompletionAccepted => this.accept_path_completion(cx),
        })
        .detach();
        cx.subscribe(&harness_input, |this, _, event, cx| match event {
            InputEvent::Submit => this.create_harness(cx),
            InputEvent::Focused => {
                this.enter_input_mode(false);
                this.refresh_composer_path_completion(PathCompletionTarget::NewHarness, cx);
                cx.notify();
            }
            InputEvent::Escape => {
                if this.path_completion.is_some() {
                    this.path_completion = None;
                } else {
                    this.enter_normal_mode();
                }
                cx.notify();
            }
            InputEvent::Changed => {
                this.refresh_composer_path_completion(PathCompletionTarget::NewHarness, cx)
            }
            InputEvent::CompletionPrevious => this.move_path_completion(-1, cx),
            InputEvent::CompletionNext => this.move_path_completion(1, cx),
            InputEvent::CompletionAccepted => this.accept_path_completion(cx),
        })
        .detach();
        cx.subscribe(&thread_rename_input, |this, _, event, cx| match event {
            InputEvent::Submit => this.finish_renaming_harness(cx),
            InputEvent::Focused => {
                this.enter_input_mode(false);
                cx.notify();
            }
            InputEvent::Escape => {
                this.renaming_harness = None;
                this.enter_normal_mode();
                cx.notify();
            }
            InputEvent::Changed => {}
            _ => {}
        })
        .detach();
        cx.subscribe(
            &workspace_settings_input,
            |this, _, event, cx| match event {
                InputEvent::Submit => this.save_custom_workspace_root(cx),
                InputEvent::Focused => {
                    this.enter_input_mode(false);
                    cx.notify();
                }
                InputEvent::Escape => {
                    this.cancel_workspace_root_edit(cx);
                    cx.notify();
                }
                InputEvent::Changed => {}
                _ => {}
            },
        )
        .detach();
        cx.subscribe(&extension_input, |this, _, event, cx| match event {
            InputEvent::Submit => this.submit_extension_dialog(cx),
            InputEvent::Focused => {
                this.enter_input_mode(false);
                cx.notify();
            }
            InputEvent::Escape => {
                this.enter_normal_mode();
                cx.notify();
            }
            InputEvent::Changed => {}
            _ => {}
        })
        .detach();

        cx.spawn(async move |this, cx| {
            let mut last_performance_log = Instant::now();
            loop {
                let expected_at = Instant::now() + UI_HEARTBEAT_INTERVAL;
                cx.background_executor().timer(UI_HEARTBEAT_INTERVAL).await;
                let now = Instant::now();
                let delay = now.saturating_duration_since(expected_at);
                let periodic = now.saturating_duration_since(last_performance_log)
                    >= UI_PERFORMANCE_LOG_INTERVAL;
                if delay >= UI_STALL_WARNING_THRESHOLD || periodic {
                    if this
                        .update(cx, |this, _| {
                            this.log_ui_performance_snapshot(
                                (delay >= UI_STALL_WARNING_THRESHOLD).then_some(delay),
                            );
                        })
                        .is_err()
                    {
                        break;
                    }
                    if periodic {
                        last_performance_log = now;
                    }
                }
            }
        })
        .detach();

        let (fuzzy_index_tx, fuzzy_index_rx) = async_channel::unbounded();
        cx.spawn(async move |this, cx| {
            while let Ok(event) = fuzzy_index_rx.recv().await {
                if this
                    .update(cx, |this, cx| {
                        this.handle_fuzzy_index_ready(event, cx);
                        cx.notify();
                    })
                    .is_err()
                {
                    break;
                }
            }
        })
        .detach();

        let (cached_session_rebuild_tx, cached_session_rebuild_rx) =
            async_channel::unbounded::<CachedSessionRebuildRequest>();
        cx.spawn(async move |this, cx| {
            while let Ok(request) = cached_session_rebuild_rx.recv().await {
                if this
                    .update(cx, |this, cx| {
                        let Some(index) = this
                            .harnesses
                            .iter()
                            .position(|harness| harness.id == request.harness_id)
                        else {
                            this.requested_session_rebuilds.remove(&request.harness_id);
                            return;
                        };
                        this.requested_session_rebuilds.remove(&request.harness_id);
                        let still_current = this.harnesses[index]
                            .cached_entries
                            .as_ref()
                            .is_some_and(|entries| Arc::ptr_eq(entries, &request.entries));
                        if this.harnesses[index].loaded_messages || !still_current {
                            return;
                        }
                        this.queue_session_rebuild(
                            index,
                            request.entries,
                            request.leaf_id,
                            None,
                            "thread_selection_cache",
                            false,
                            false,
                            cx,
                        );
                    })
                    .is_err()
                {
                    break;
                }
            }
        })
        .detach();

        let (workspace_event_tx, workspace_event_rx) = async_channel::unbounded();
        cx.spawn(async move |this, cx| {
            while let Ok(event) = workspace_event_rx.recv().await {
                if this
                    .update(cx, |this, cx| {
                        this.handle_workspace_event(event, cx);
                        cx.notify();
                    })
                    .is_err()
                {
                    break;
                }
            }
        })
        .detach();

        // Repository and diff operations invoke external VCS commands, so each subsystem gets a
        // serial worker and returns plain results to the GPUI task.
        let (repository_task_tx, repository_task_rx) = async_channel::bounded(64);
        let (repository_result_tx, repository_result_rx) = async_channel::bounded(64);
        std::thread::Builder::new()
            .name("dirigent-repository".into())
            .spawn(move || {
                self::managed_workspace::run_repository_worker(
                    repository_task_rx,
                    repository_result_tx,
                )
            })
            .expect("could not start repository worker");
        cx.spawn(async move |this, cx| {
            while let Ok(result) = repository_result_rx.recv().await {
                if this
                    .update(cx, |this, cx| {
                        this.handle_repository_refresh(result);
                        cx.notify();
                    })
                    .is_err()
                {
                    break;
                }
            }
        })
        .detach();

        // Pi can produce token deltas much faster than the display should redraw. Bounded,
        // coalesced batches provide backpressure while preserving protocol boundaries.
        let (event_tx, event_rx) = async_channel::bounded(RUNTIME_EVENT_CHANNEL_CAPACITY);
        cx.spawn(async move |this, cx| {
            let mut stats = RuntimeEventLoopStats::default();
            let mut last_slow_log_at: Option<Instant> = None;
            while let Ok(first) = event_rx.recv().await {
                let collect_started = Instant::now();
                let queue_depth_at_start = event_rx.len() + 1;
                let mut events: Vec<RuntimeEvent> =
                    Vec::with_capacity(queue_depth_at_start.min(RUNTIME_EVENT_BATCH_LIMIT));
                events.push(first);
                while events.len() < RUNTIME_EVENT_BATCH_LIMIT
                    && collect_started.elapsed() < RUNTIME_EVENT_BATCH_WINDOW
                {
                    let Ok(event) = event_rx.try_recv() else {
                        break;
                    };
                    events.push(event);
                }
                let collect_elapsed = collect_started.elapsed();
                let raw_count = events.len();
                let oldest_age = events
                    .iter()
                    .map(|event| event.queued_at.elapsed())
                    .max()
                    .unwrap_or_default();
                let events = coalesce_runtime_events(events);
                let handled_count = events.len();
                let event_kinds = runtime_event_kinds(&events);
                let handle_started = Instant::now();
                if this
                    .update(cx, |this, cx| {
                        for event in events {
                            this.handle_runtime_event(event, cx);
                        }
                        cx.notify();
                    })
                    .is_err()
                {
                    break;
                }
                let handle_elapsed = handle_started.elapsed();
                let queue_depth_after = event_rx.len();
                stats.record(
                    raw_count,
                    handled_count,
                    queue_depth_at_start,
                    oldest_age,
                    collect_elapsed,
                    handle_elapsed,
                );
                let slow = handle_elapsed >= SLOW_RUNTIME_EVENT_BATCH
                    || oldest_age >= OLD_RUNTIME_EVENT_THRESHOLD
                    || queue_depth_at_start >= RUNTIME_EVENT_QUEUE_WARNING_THRESHOLD;
                let log_slow = slow
                    && last_slow_log_at.is_none_or(|last| last.elapsed() >= Duration::from_secs(1));
                if log_slow {
                    last_slow_log_at = Some(Instant::now());
                    tracing::warn!(
                        raw_events = raw_count,
                        handled_events = handled_count,
                        coalesced_events = raw_count.saturating_sub(handled_count),
                        event_kinds,
                        queue_depth_at_start,
                        queue_depth_after,
                        oldest_event_age_ms = duration_ms(oldest_age),
                        collect_us = duration_us(collect_elapsed),
                        handle_us = duration_us(handle_elapsed),
                        "slow runtime event batch"
                    );
                }
                stats.log_periodic(Instant::now(), queue_depth_after);
                // Give GPUI a chance to draw and process input, and allow nearby token
                // deltas to accumulate so the next batch can coalesce them.
                cx.background_executor()
                    .timer(RUNTIME_EVENT_YIELD_INTERVAL)
                    .await;
            }
        })
        .detach();

        let (title_event_tx, title_event_rx) = async_channel::unbounded();
        cx.spawn(async move |this, cx| {
            while let Ok(event) = title_event_rx.recv().await {
                if this
                    .update(cx, |this, cx| {
                        this.handle_title_generation_event(event);
                        cx.notify();
                    })
                    .is_err()
                {
                    break;
                }
            }
        })
        .detach();

        #[cfg(feature = "self-update")]
        let update_event_tx = {
            let (update_event_tx, update_event_rx) = async_channel::unbounded();
            crate::update::start_checker(update_event_tx.clone());
            cx.spawn(async move |this, cx| {
                while let Ok(event) = update_event_rx.recv().await {
                    if this
                        .update(cx, |this, cx| {
                            this.handle_update_event(event, cx);
                            cx.notify();
                        })
                        .is_err()
                    {
                        break;
                    }
                }
            })
            .detach();
            update_event_tx
        };

        let (math_render_task_tx, math_render_task_rx) = async_channel::unbounded();
        let (math_render_result_tx, math_render_result_rx) = async_channel::unbounded();
        std::thread::Builder::new()
            .name("dirigent-math-render".into())
            .spawn(move || {
                crate::math::run_math_render_worker(math_render_task_rx, math_render_result_tx)
            })
            .expect("could not start math render worker");
        cx.spawn(async move |this, cx| {
            while let Ok(result) = math_render_result_rx.recv().await {
                if this
                    .update(cx, |this, cx| {
                        this.handle_math_render_result(result);
                        cx.notify();
                    })
                    .is_err()
                {
                    break;
                }
            }
        })
        .detach();

        let (diff_task_tx, diff_task_rx) = async_channel::unbounded();
        let (diff_result_tx, diff_result_rx) = async_channel::unbounded();
        std::thread::Builder::new()
            .name("dirigent-diff".into())
            .spawn(move || self::diff::run_diff_worker(diff_task_rx, diff_result_tx))
            .expect("could not start diff worker");
        cx.spawn(async move |this, cx| {
            while let Ok(result) = diff_result_rx.recv().await {
                if this
                    .update(cx, |this, cx| {
                        this.handle_diff_task_result(result);
                        cx.notify();
                    })
                    .is_err()
                {
                    break;
                }
            }
        })
        .detach();

        let (turn_highlight_task_tx, turn_highlight_task_rx) = async_channel::unbounded();
        let (turn_highlight_result_tx, turn_highlight_result_rx) = async_channel::unbounded();
        let turn_highlight_generation = Arc::new(AtomicU64::new(1));
        let worker_highlight_generation = turn_highlight_generation.clone();
        std::thread::Builder::new()
            .name("dirigent-turn-highlights".into())
            .spawn(move || {
                self::diff::run_turn_highlight_worker(
                    turn_highlight_task_rx,
                    turn_highlight_result_tx,
                    worker_highlight_generation,
                )
            })
            .expect("could not start turn highlight worker");
        cx.spawn(async move |this, cx| {
            while let Ok(result) = turn_highlight_result_rx.recv().await {
                if this
                    .update(cx, |this, cx| {
                        this.handle_turn_highlight_result(result);
                        cx.notify();
                    })
                    .is_err()
                {
                    break;
                }
            }
        })
        .detach();

        let mut banner = config_error.clone();
        let pi_bridge_extension = match platform::materialize_pi_bridge() {
            Ok(path) => Some(path),
            Err(error) => {
                tracing::error!(error = %error, "could not materialize Pi bridge extension");
                if banner.is_none() {
                    banner = Some(error);
                }
                None
            }
        };
        let storage::LoadedState {
            projects,
            mut harnesses,
            mut workspaces,
            next_id,
            next_sidebar_order,
            last_used_harness,
            collapsed_projects,
            sidebar_width,
            diff_sidebar_open,
            diff_sidebar_width,
            diff_view_mode,
        } = loaded;
        for workspace in &mut workspaces {
            workspace.state = match crate::vcs::validate_workspace(workspace) {
                Ok(()) => WorkspaceState::Ready,
                Err(error) => {
                    tracing::error!(error = %error, workspace_id = %workspace.id, "managed workspace validation failed");
                    WorkspaceState::Failed(error)
                }
            };
        }
        // Durable metadata is already loaded above; this disposable cache restores conversation
        // content and composer state without making it part of the state database transaction.
        let session_cache = match SessionCache::open() {
            Ok(cache) => Some(cache),
            Err(error) => {
                tracing::error!(error = %error, "could not open session cache");
                if banner.is_none() {
                    banner = Some(error);
                }
                None
            }
        };
        let mut available_models_by_project = HashMap::new();
        let mut available_thinking_levels = HashMap::new();
        if let Some(cache) = session_cache.as_ref() {
            for harness in &mut harnesses {
                let Some(session_file) = harness.session_file.as_deref() else {
                    continue;
                };
                match cache.load_session(session_file) {
                    Ok(Some(cached)) => {
                        harness.model = cached.model;
                        harness.thinking_level = cached.thinking_level;
                        harness.composer_draft = cached.composer_draft;
                        match parse_cached_draft_images(&cached.composer_images_json) {
                            Ok(images) => harness.composer_draft_images = images,
                            Err(error) => {
                                tracing::error!(error = %error, harness_id = harness.id, "could not restore cached draft images");
                                if banner.is_none() {
                                    banner = Some(error);
                                }
                            }
                        }
                        harness.cached_leaf_id = cached.leaf_id;
                        if let Some(entries) = cached.entries {
                            harness.cached_entries = Some(Arc::new(entries));
                        }
                    }
                    Ok(None) => {}
                    Err(error) => {
                        tracing::error!(error = %error, harness_id = harness.id, "could not load cached session");
                        if banner.is_none() {
                            banner = Some(error);
                        }
                    }
                }
            }
            for project in &projects {
                match cache.load_models(&project.path) {
                    Ok(Some(bytes)) => match serde_json::from_slice(&bytes) {
                        Ok(models) => {
                            available_models_by_project.insert(project.id, models);
                        }
                        Err(error) => {
                            tracing::error!(error = %error, project_id = project.id, "could not decode cached models");
                            if banner.is_none() {
                                banner = Some(format!("could not decode cached models: {error}"));
                            }
                        }
                    },
                    Ok(None) => {}
                    Err(error) => {
                        tracing::error!(error = %error, project_id = project.id, "could not load cached models");
                        if banner.is_none() {
                            banner = Some(error);
                        }
                    }
                }
                match cache.load_thinking_levels(&project.path) {
                    Ok(levels) => {
                        available_thinking_levels.extend(
                            levels
                                .into_iter()
                                .map(|(model, levels)| ((project.id, model), levels)),
                        );
                    }
                    Err(error) => {
                        tracing::error!(error = %error, project_id = project.id, "could not load cached thinking levels");
                        if banner.is_none() {
                            banner = Some(error);
                        }
                    }
                }
            }
        }
        if let Some(cache) = session_cache.as_ref() {
            let cache_errors = cache.errors();
            cx.spawn(async move |this, cx| {
                while let Ok(error) = cache_errors.recv().await {
                    if this
                        .update(cx, |this, cx| {
                            this.report_cache_error(error);
                            cx.notify();
                        })
                        .is_err()
                    {
                        break;
                    }
                }
            })
            .detach();
        }
        let mut project_file_pickers = HashMap::new();
        for project in &projects {
            match start_fuzzy_index(
                &project.path,
                true,
                FuzzyIndexReady::Project(project.id),
                fuzzy_index_tx.clone(),
            ) {
                Ok(picker) => {
                    project_file_pickers.insert(project.id, picker);
                }
                Err(error) => {
                    tracing::error!(error = %error, project_id = project.id, "could not start project file index");
                    if banner.is_none() {
                        banner = Some(error);
                    }
                }
            }
        }
        let mut workspace_file_pickers = HashMap::new();
        for workspace in &workspaces {
            if workspace.state != WorkspaceState::Ready {
                continue;
            }
            match start_fuzzy_index(
                &workspace.working_directory,
                true,
                FuzzyIndexReady::Workspace(workspace.id.clone()),
                fuzzy_index_tx.clone(),
            ) {
                Ok(picker) => {
                    workspace_file_pickers.insert(workspace.id.clone(), picker);
                }
                Err(error) => {
                    tracing::error!(error = %error, workspace_id = %workspace.id, "could not start workspace file index");
                    if banner.is_none() {
                        banner = Some(error);
                    }
                }
            }
        }

        // Prefer the explicitly persisted thread, then the newest visible thread, and finally a
        // thread from the first project. This keeps startup deterministic after deletions.
        let fallback_project = projects.first().map(|project| project.id);
        let selected_harness = last_used_harness
            .filter(|id| harnesses.iter().any(|harness| harness.id == *id))
            .or_else(|| {
                harnesses
                    .iter()
                    .filter(|harness| !harness.archived)
                    .max_by_key(|harness| harness.sidebar_order)
                    .map(|harness| harness.id)
            })
            .or_else(|| {
                fallback_project.and_then(|project_id| {
                    harnesses
                        .iter()
                        .filter(|harness| harness.project_id == project_id)
                        .max_by_key(|harness| harness.sidebar_order)
                        .map(|harness| harness.id)
                })
            });
        let selected_project = selected_harness
            .and_then(|id| {
                harnesses
                    .iter()
                    .find(|harness| harness.id == id)
                    .map(|harness| harness.project_id)
            })
            .or(fallback_project);
        let adding_project = projects.is_empty();
        let available_models = selected_project
            .and_then(|project_id| available_models_by_project.get(&project_id).cloned())
            .unwrap_or_default();
        let selected_conversation =
            selected_harness.and_then(|id| harnesses.iter().find(|harness| harness.id == id));
        let conversation_list_message_count =
            selected_conversation.map_or(0, |harness| harness.messages.len());
        let conversation_list_queued_count =
            selected_conversation.map_or(0, |harness| harness.queued_messages.len());
        let conversation_list_working =
            selected_conversation.is_some_and(|harness| harness.status == HarnessStatus::Working);
        let conversation_render_cache = selected_conversation
            .map(ConversationRenderCache::build)
            .unwrap_or_default();
        let conversation_list = ListState::new(
            conversation_render_cache.len(),
            ListAlignment::Bottom,
            px(180.0),
        )
        .with_uniform_item_height(px(conversation_render_cache.item_height_hint()))
        .measure_all();
        conversation_list.set_follow_mode(FollowMode::Tail);
        let entity = cx.entity();
        conversation_list.set_scroll_handler(move |_, _, cx| {
            entity.update(cx, |_, cx| cx.notify());
        });

        let mut composer_inputs = HashMap::new();
        for harness in &harnesses {
            let harness_id = harness.id;
            let draft = harness.composer_draft.clone();
            let draft_images = harness.composer_draft_images.clone();
            let input = cx.new(move |cx| {
                let mut input = TextInput::new(COMPOSER_PLACEHOLDER, cx)
                    .borderless()
                    .multiline();
                if !draft.is_empty() || !draft_images.is_empty() {
                    input.restore_draft(draft, draft_images, cx);
                }
                input
            });
            cx.subscribe(&input, move |this, _, event, cx| match event {
                InputEvent::Submit if this.selected_harness == Some(harness_id) => {
                    this.send_composer(cx)
                }
                InputEvent::Focused if this.selected_harness == Some(harness_id) => {
                    this.enter_input_mode(false);
                    this.refresh_composer_path_completion(
                        PathCompletionTarget::Harness(harness_id),
                        cx,
                    );
                    cx.notify();
                }
                InputEvent::Escape if this.selected_harness == Some(harness_id) => {
                    if this.path_completion.is_some() {
                        this.path_completion = None;
                    } else {
                        this.enter_normal_mode();
                    }
                    cx.notify();
                }
                InputEvent::Changed => {
                    this.persist_composer_draft(harness_id, cx);
                    this.refresh_composer_path_completion(
                        PathCompletionTarget::Harness(harness_id),
                        cx,
                    );
                }
                InputEvent::CompletionPrevious => this.move_path_completion(-1, cx),
                InputEvent::CompletionNext => this.move_path_completion(1, cx),
                InputEvent::CompletionAccepted => this.accept_path_completion(cx),
                _ => {}
            })
            .detach();
            composer_inputs.insert(harness_id, input);
        }

        let mut this = Self {
            projects,
            harnesses,
            workspaces,
            selected_project,
            selected_harness,
            last_used_harness: selected_harness,
            adding_project,
            creating_harness: false,
            project_settings: None,
            workspace_settings_editing: false,
            about_open: false,
            pi_version: "Checking…".into(),
            sidebar_width: sidebar_width.clamp(200.0, 520.0),
            sidebar_layout_persist_task: None,
            diff_sidebar_open,
            diff_sidebar_width: diff_sidebar_width.clamp(420.0, 1_600.0),
            diff_view_mode,
            diff_scope: DiffScope::Cumulative,
            selected_diff_turn: None,
            diff_turn_dropdown_open: false,
            diff_list: ListState::new(0, ListAlignment::Top, px(180.0))
                .with_uniform_item_height(px(300.0)),
            diff_display_key: None,
            diff_display: None,
            diff_render_cache: DiffRenderCache::default(),
            diff_file_collapse_overrides: HashMap::new(),
            diff_code_scrolls: std::cell::RefCell::new(HashMap::new()),
            collapsed_projects,
            expanded_archived_projects: HashSet::new(),
            sidebar_menu: None,
            codex_usage: None,
            renaming_harness: None,
            project_input,
            harness_input,
            thread_rename_input,
            workspace_settings_input,
            composer_inputs,
            extension_input,
            pending_dialog: None,
            banner,
            #[cfg(feature = "self-update")]
            update_state: crate::update::UpdateState::Checking,
            #[cfg(feature = "self-update")]
            update_events: update_event_tx,
            config_error,
            font: appearance.font.into(),
            window_transparent: None,
            conversation_list,
            conversation_render_cache,
            conversation_list_message_count,
            conversation_list_queued_count,
            conversation_list_working,
            conversation_ruler_last_layout_at: Instant::now(),
            conversation_scroll_dragging: false,
            conversation_scroll_drag_offset: 0.0,
            model_picker_scroll: ScrollHandle::new(),
            composer_dropdown: None,
            editing_message: None,
            pending_edit_submit: None,
            pending_forks: HashMap::new(),
            path_completion: None,
            project_file_pickers,
            workspace_file_pickers,
            fuzzy_index_events: fuzzy_index_tx,
            repository_snapshots: HashMap::new(),
            repository_tasks: repository_task_tx,
            pending_repository_refreshes: HashSet::new(),
            dirty_repository_refreshes: HashSet::new(),
            repository_refreshed_at: HashMap::new(),
            draft_workspace_source: None,
            pending_workspace_sources: HashMap::new(),
            pending_workspace_deletion: None,
            deleting_workspace_harnesses: HashSet::new(),
            available_models,
            available_models_by_project,
            available_thinking_levels,
            state_database,
            session_cache,
            cached_session_rebuilds: cached_session_rebuild_tx,
            pending_session_rebuilds: HashMap::new(),
            requested_session_rebuilds: HashSet::new(),
            next_session_rebuild_job_id: 1,
            draft_model: None,
            draft_thinking_level: None,
            draft_nix_enabled: true,
            project_probe: None,
            pi_bridge_extension,
            thread_text_selection: None,
            hovered_copy_message: None,
            hovered_action_message: None,
            hovered_tool_detail_message: None,
            copied_button: None,
            preview_image: None,
            thread_focus: cx.focus_handle(),
            keyboard_mode: KeyboardMode::Normal,
            keyboard_menu: None,
            keyboard_menu_activation: None,
            focus_input: false,
            focus_normal_mode: true,
            debug_panels_visible: false,
            frame_timing: FrameTiming::new(Instant::now()),
            next_id,
            next_sidebar_order,
            math_renders: std::cell::RefCell::new(HashMap::new()),
            math_render_tasks: math_render_task_tx,
            runtime_events: event_tx,
            workspace_events: workspace_event_tx,
            title_events: title_event_tx,
            diff_tasks: diff_task_tx,
            turn_highlight_tasks: turn_highlight_task_tx,
            turn_highlight_generation,
            pending_turn_highlight: None,
            highlighted_diff_display: None,
            pending_diff_prompts: HashMap::new(),
            pending_diff_previews: HashMap::new(),
            dirty_diff_previews: HashSet::new(),
            next_diff_job_id: 1,
            title_processes: HashMap::new(),
        };
        let pi_version_task = cx.background_spawn(async { platform::pi_version() });
        cx.spawn(async move |this, cx| {
            let version = pi_version_task.await;
            let _ = this.update(cx, |this, cx| {
                this.pi_version = version.unwrap_or_else(|error| {
                    tracing::warn!(%error, "could not determine pi version");
                    "Unavailable".into()
                });
                cx.notify();
            });
        })
        .detach();

        if let Some((index, entries, leaf_id)) = selected_harness.and_then(|selected| {
            let index = this
                .harnesses
                .iter()
                .position(|harness| harness.id == selected)?;
            let harness = &this.harnesses[index];
            Some((
                index,
                Arc::clone(harness.cached_entries.as_ref()?),
                harness.cached_leaf_id.clone(),
            ))
        }) {
            this.queue_session_rebuild(
                index,
                entries,
                leaf_id,
                None,
                "startup_cache",
                false,
                false,
                cx,
            );
        }
        if let Some(project_id) = selected_project {
            this.refresh_repository(project_id);
        }
        if let Some(harness_id) = selected_harness {
            this.request_delegated_session_rebuilds(harness_id);
            this.start_harness(harness_id, None);
        }
        this
    }
}
