mod branching;
mod diff;
mod harness;
mod keyboard;
mod managed_workspace;
mod message_parsing;
mod path_completion;
mod runtime;
#[cfg(test)]
mod tests;
mod workspace;

use std::{
    collections::{HashMap, HashSet, VecDeque},
    fs,
    ops::Range,
    path::PathBuf,
    sync::Arc,
    time::{Duration, Instant},
};

use async_channel::Sender;
use base64::{Engine as _, engine::general_purpose::STANDARD as BASE64};
use fff_search::{
    FFFMode, FilePicker, FilePickerOptions, FuzzySearchOptions, PaginationArgs, QueryParser,
    SharedFilePicker, SharedFrecency,
};
use gpui::{
    ClipboardItem, Context, Entity, FocusHandle, Focusable, FollowMode, FontFeatures, Image,
    ImageFormat, IntoElement, KeyDownEvent, KeyUpEvent, ListAlignment, ListState, MouseButton,
    ObjectFit, ScrollHandle, SharedString, StyledImage, Window, WindowBackgroundAppearance,
    deferred, div, img, point, prelude::*, profiler, px,
};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use self::{harness::*, message_parsing::*, path_completion::*};
#[cfg(test)]
use branching::*;

use crate::{
    cache::SessionCache,
    diff::{DiffScope, DiffSelectionReference, DiffViewMode, TurnDiff, TurnDiffStatus},
    image_attachment::normalize_for_harness,
    model::{
        CodexUsage, CodexUsageWindow, ContextUsage, Harness, HarnessStatus, Id, ManagedWorkspace,
        Message, MessageRole, Project, RetryStatus, WorkspaceState,
    },
    platform,
    rpc::{PiProcess, RuntimeEvent, RuntimeTarget},
    storage,
    text_input::{AttachedImage, InputEvent, TextInput},
    theme::{self, bg, border, muted, rgb, theme_text},
    title_generator::{TitleGenerationEvent, TitleProcess},
    ui::{ConversationRenderCache, DiffRenderCache},
    vcs::RepositorySnapshot,
};

const STARTUP_MODEL_REQUEST_ID: &str = "dirigent-startup-model";
const STARTUP_THINKING_REQUEST_ID: &str = "dirigent-startup-thinking";

#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum DialogKind {
    Select,
    Confirm,
    Input,
    Editor,
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum ComposerDropdown {
    Project,
    Model,
    Reasoning,
    EditModel,
    EditReasoning,
    Workspace,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum PathCompletionTarget {
    Project,
    NewHarness,
    Harness(Id),
}

pub(crate) struct PathCompletion {
    pub(crate) target: PathCompletionTarget,
    pub(crate) items: Vec<String>,
    pub(crate) selected: usize,
    replacement: Range<usize>,
}

#[derive(Clone, Debug)]
enum FuzzyIndexReady {
    Project(Id),
    Workspace(String),
}

pub(crate) enum WorkspaceEvent {
    Created(String),
    Failed(String, String),
    Removed {
        workspace_id: String,
        harness_id: Id,
    },
    RemoveFailed {
        harness_id: Id,
        error: String,
    },
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum SidebarMenu {
    Thread(Id),
    Project(Id),
    Bottom,
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum KeyboardMode {
    Normal,
    Input,
}

impl KeyboardMode {
    pub(crate) fn label(self) -> &'static str {
        match self {
            Self::Normal => "NORMAL",
            Self::Input => "INSERT",
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum KeyboardMenu {
    Space,
    Goto,
    Threads,
    Projects,
}

#[derive(Clone, Deserialize, Serialize)]
pub(crate) struct AvailableModel {
    pub(crate) provider: String,
    pub(crate) id: String,
    pub(crate) name: String,
    reasoning: bool,
    supports_xhigh: bool,
    supports_max: bool,
}

#[derive(Deserialize, Serialize)]
struct CachedDraftImage {
    label: String,
    mime_type: String,
    data: String,
}

pub(crate) struct ThreadTextSelection {
    pub(crate) id: String,
    pub(crate) text: SharedString,
    pub(crate) anchor: usize,
    pub(crate) head: usize,
    pub(crate) range: Range<usize>,
    pub(crate) selecting: bool,
    pub(crate) diff_reference: Option<DiffSelectionReference>,
}

pub(crate) struct MessageEdit {
    pub(crate) harness_id: Id,
    pub(crate) message_index: usize,
    pub(crate) entry_id: String,
    pub(crate) input: Entity<TextInput>,
    pub(crate) model: String,
    pub(crate) thinking: String,
    pub(crate) submitting: bool,
}

struct PendingEditSubmit {
    harness_id: Id,
    entry_id: String,
    text: String,
    images: Vec<AttachedImage>,
    model: String,
    thinking: String,
}

struct PendingFork {
    source_harness_id: Id,
    entry_id: String,
    position: &'static str,
}

pub(crate) struct PendingDialog {
    pub(crate) harness_id: Id,
    pub(crate) request_id: String,
    pub(crate) kind: DialogKind,
    pub(crate) title: String,
    pub(crate) message: Option<String>,
    pub(crate) options: Vec<String>,
}

#[derive(Clone, Copy, Debug, PartialEq)]
struct FrameTimingSample {
    draw_ms: f32,
    response_ms: Option<f32>,
    invalidations: u64,
}

#[derive(Clone, Copy, Debug, PartialEq)]
struct FrameTimingSummary {
    draw_average_ms: f32,
    draw_p99_ms: f32,
    draw_maximum_ms: f32,
    response_p99_ms: Option<f32>,
    invalidations_average: f32,
    sample_count: usize,
}

struct FrameTiming {
    collector: profiler::FrameTimingCollector,
    last_summary_at: Instant,
    samples: VecDeque<FrameTimingSample>,
    summary: Option<FrameTimingSummary>,
}

impl FrameTiming {
    const SUMMARY_INTERVAL: Duration = Duration::from_millis(250);
    const MAX_SAMPLES: usize = 240;

    fn new(now: Instant) -> Self {
        profiler::set_frame_trace_enabled(true);
        Self {
            collector: profiler::FrameTimingCollector::new(),
            last_summary_at: now,
            samples: VecDeque::with_capacity(Self::MAX_SAMPLES),
            summary: None,
        }
    }

    fn collect_frames(&mut self, now: Instant) {
        let frames = self.collector.collect_unseen();
        if frames.is_empty() {
            return;
        }
        for frame in frames {
            if self.samples.len() == Self::MAX_SAMPLES {
                self.samples.pop_front();
            }
            self.samples.push_back(FrameTimingSample {
                draw_ms: frame.draw_duration().as_secs_f32() * 1_000.0,
                response_ms: frame
                    .dirty_to_draw_duration()
                    .map(|duration| duration.as_secs_f32() * 1_000.0),
                invalidations: frame.invalidations,
            });
        }
        if self.summary.is_none()
            || now.saturating_duration_since(self.last_summary_at) >= Self::SUMMARY_INTERVAL
        {
            self.summary = Self::summarize(&self.samples);
            self.last_summary_at = now;
        }
    }

    fn p99(values: &mut [f32]) -> Option<f32> {
        if values.is_empty() {
            return None;
        }
        values.sort_by(f32::total_cmp);
        let index = ((values.len() as f32 * 0.99).ceil() as usize)
            .saturating_sub(1)
            .min(values.len() - 1);
        Some(values[index])
    }

    fn summarize(samples: &VecDeque<FrameTimingSample>) -> Option<FrameTimingSummary> {
        if samples.is_empty() {
            return None;
        }
        let sample_count = samples.len();
        let mut draw_times = samples
            .iter()
            .map(|sample| sample.draw_ms)
            .collect::<Vec<_>>();
        let draw_average_ms = draw_times.iter().sum::<f32>() / sample_count as f32;
        let draw_maximum_ms = draw_times.iter().copied().max_by(f32::total_cmp)?;
        let draw_p99_ms = Self::p99(&mut draw_times)?;
        let mut response_times = samples
            .iter()
            .filter_map(|sample| sample.response_ms)
            .collect::<Vec<_>>();
        let invalidations_average = samples
            .iter()
            .map(|sample| sample.invalidations as f32)
            .sum::<f32>()
            / sample_count as f32;
        Some(FrameTimingSummary {
            draw_average_ms,
            draw_p99_ms,
            draw_maximum_ms,
            response_p99_ms: Self::p99(&mut response_times),
            invalidations_average,
            sample_count,
        })
    }

    fn labels(&self) -> Vec<String> {
        let Some(summary) = self.summary else {
            return vec![
                "draw avg —".into(),
                "draw p99 —".into(),
                "draw max —".into(),
                "response p99 —".into(),
                "invalidations avg —".into(),
                "samples 0".into(),
            ];
        };
        vec![
            format!("draw avg {:.1} ms", summary.draw_average_ms),
            format!("draw p99 {:.1} ms", summary.draw_p99_ms),
            format!("draw max {:.1} ms", summary.draw_maximum_ms),
            summary.response_p99_ms.map_or_else(
                || "response p99 —".into(),
                |response| format!("response p99 {response:.1} ms"),
            ),
            format!(
                "invalidations avg {:.1}/frame",
                summary.invalidations_average
            ),
            format!("samples {}", summary.sample_count),
        ]
    }
}

pub(crate) struct Dirigent {
    pub(crate) projects: Vec<Project>,
    pub(crate) harnesses: Vec<Harness>,
    pub(crate) workspaces: Vec<ManagedWorkspace>,
    pub(crate) selected_project: Option<Id>,
    pub(crate) selected_harness: Option<Id>,
    pub(crate) last_used_harness: Option<Id>,
    pub(crate) adding_project: bool,
    pub(crate) creating_harness: bool,
    pub(crate) project_settings: Option<Id>,
    pub(crate) workspace_settings_editing: bool,
    pub(crate) sidebar_width: f32,
    pub(crate) diff_sidebar_open: bool,
    pub(crate) diff_sidebar_width: f32,
    pub(crate) diff_view_mode: DiffViewMode,
    pub(crate) diff_scope: DiffScope,
    pub(crate) selected_diff_turn: Option<(Id, u64)>,
    pub(crate) diff_turn_dropdown_open: bool,
    pub(crate) diff_list: ListState,
    pub(crate) diff_display_key: Option<(Id, u64, DiffScope)>,
    pub(crate) diff_display: Option<TurnDiff>,
    pub(crate) diff_render_cache: DiffRenderCache,
    pub(crate) diff_code_scrolls: std::cell::RefCell<HashMap<String, ScrollHandle>>,
    pub(crate) collapsed_projects: HashSet<Id>,
    pub(crate) expanded_archived_projects: HashSet<Id>,
    pub(crate) sidebar_menu: Option<SidebarMenu>,
    pub(crate) codex_usage: Option<CodexUsage>,
    pub(crate) renaming_harness: Option<Id>,
    pub(crate) project_input: Entity<TextInput>,
    pub(crate) harness_input: Entity<TextInput>,
    pub(crate) thread_rename_input: Entity<TextInput>,
    pub(crate) workspace_settings_input: Entity<TextInput>,
    composer_inputs: HashMap<Id, Entity<TextInput>>,
    pub(crate) extension_input: Entity<TextInput>,
    pub(crate) pending_dialog: Option<PendingDialog>,
    pub(crate) banner: Option<String>,
    config_error: Option<String>,
    pub(crate) font: SharedString,
    window_transparent: Option<bool>,
    pub(crate) conversation_list: ListState,
    pub(crate) conversation_render_cache: ConversationRenderCache,
    conversation_list_message_count: usize,
    conversation_list_queued_count: usize,
    conversation_list_working: bool,
    pub(crate) conversation_scroll_dragging: bool,
    pub(crate) model_picker_scroll: ScrollHandle,
    pub(crate) composer_dropdown: Option<ComposerDropdown>,
    pub(crate) editing_message: Option<MessageEdit>,
    pending_edit_submit: Option<PendingEditSubmit>,
    pending_forks: HashMap<Id, PendingFork>,
    pub(crate) path_completion: Option<PathCompletion>,
    project_file_pickers: HashMap<Id, SharedFilePicker>,
    workspace_file_pickers: HashMap<String, SharedFilePicker>,
    fuzzy_index_events: Sender<FuzzyIndexReady>,
    repository_snapshots: HashMap<Id, RepositorySnapshot>,
    pub(crate) draft_workspace_source: Option<RepositorySnapshot>,
    pending_workspace_sources: HashMap<Id, RepositorySnapshot>,
    pub(crate) pending_workspace_deletion: Option<Id>,
    deleting_workspace_harnesses: HashSet<Id>,
    pub(crate) available_models: Vec<AvailableModel>,
    available_models_by_project: HashMap<Id, Vec<AvailableModel>>,
    available_thinking_levels: HashMap<(Id, String), Vec<String>>,
    state_database: storage::StateDatabase,
    session_cache: Option<SessionCache>,
    pub(crate) draft_model: Option<String>,
    pub(crate) draft_thinking_level: Option<String>,
    pub(crate) draft_nix_enabled: bool,
    // Loads project-local pi defaults before the project's first harness exists.
    project_probe: Option<(Id, PiProcess)>,
    pi_bridge_extension: Option<PathBuf>,
    pub(crate) thread_text_selection: Option<ThreadTextSelection>,
    pub(crate) hovered_copy_message: Option<(Id, usize)>,
    pub(crate) hovered_action_message: Option<(Id, usize)>,
    pub(crate) hovered_tool_detail_message: Option<(Id, usize)>,
    pub(crate) copied_button: Option<(String, Instant)>,
    pub(crate) preview_image: Option<Arc<Image>>,
    pub(crate) thread_focus: FocusHandle,
    pub(crate) keyboard_mode: KeyboardMode,
    pub(crate) keyboard_menu: Option<KeyboardMenu>,
    pub(crate) keyboard_menu_activation: Option<KeyboardMenu>,
    focus_input: bool,
    focus_normal_mode: bool,
    debug_panels_visible: bool,
    frame_timing: FrameTiming,
    next_id: Id,
    next_sidebar_order: u64,
    runtime_events: Sender<RuntimeEvent>,
    workspace_events: Sender<WorkspaceEvent>,
    title_events: Sender<TitleGenerationEvent>,
    title_processes: HashMap<Id, TitleProcess>,
}

#[cfg(not(target_os = "windows"))]
fn project_has_devshell(path: &std::path::Path) -> bool {
    fs::read_to_string(path.join("flake.nix")).is_ok_and(|flake| flake.contains("devShell"))
}

#[cfg(target_os = "windows")]
fn project_has_devshell(_path: &std::path::Path) -> bool {
    false
}

#[cfg(not(target_os = "windows"))]
fn project_path_placeholder() -> &'static str {
    "/absolute/path/to/project"
}

#[cfg(target_os = "windows")]
fn project_path_placeholder() -> &'static str {
    r"C:\path\to\project"
}

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
            TextInput::new("Send a message to pi…", cx)
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

        let (event_tx, event_rx) = async_channel::unbounded();
        cx.spawn(async move |this, cx| {
            while let Ok(event) = event_rx.recv().await {
                if this
                    .update(cx, |this, cx| {
                        this.handle_runtime_event(event, cx);
                        cx.notify();
                    })
                    .is_err()
                {
                    break;
                }
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
        for harness in &mut harnesses {
            for turn in &mut harness.turn_diffs {
                turn.refresh_highlights();
            }
        }
        for workspace in &mut workspaces {
            workspace.state = match crate::vcs::validate_workspace(workspace) {
                Ok(()) => WorkspaceState::Ready,
                Err(error) => {
                    tracing::error!(error = %error, workspace_id = %workspace.id, "managed workspace validation failed");
                    WorkspaceState::Failed(error)
                }
            };
        }
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
                            harness.messages =
                                parse_entries(&entries, harness.cached_leaf_id.as_deref());
                            harness.cached_entries = Some(entries);
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
        .with_uniform_item_height(px(conversation_render_cache.item_height_hint()));
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
                let mut input = TextInput::new("Make no mistakes, or else...", cx)
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
            sidebar_width: sidebar_width.clamp(200.0, 520.0),
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
            config_error,
            font: appearance.font.into(),
            window_transparent: None,
            conversation_list,
            conversation_render_cache,
            conversation_list_message_count,
            conversation_list_queued_count,
            conversation_list_working,
            conversation_scroll_dragging: false,
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
            draft_workspace_source: None,
            pending_workspace_sources: HashMap::new(),
            pending_workspace_deletion: None,
            deleting_workspace_harnesses: HashSet::new(),
            available_models,
            available_models_by_project,
            available_thinking_levels,
            state_database,
            session_cache,
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
            runtime_events: event_tx,
            workspace_events: workspace_event_tx,
            title_events: title_event_tx,
            title_processes: HashMap::new(),
        };
        if let Some(project_id) = selected_project {
            this.refresh_repository(project_id);
        }
        if let Some(harness_id) = selected_harness {
            this.start_harness(harness_id, None);
        }
        this
    }

    fn allocate_id(&mut self) -> Id {
        let id = self.next_id;
        self.next_id += 1;
        id
    }

    fn allocate_sidebar_order(&mut self) -> u64 {
        let order = self.next_sidebar_order;
        self.next_sidebar_order += 1;
        order
    }

    fn refresh_harness_order(&mut self, index: usize) {
        self.harnesses[index].sidebar_order = self.allocate_sidebar_order();
    }

    pub(crate) fn persist(&mut self) {
        if let Err(error) = self.state_database.save(
            &self.projects,
            &self.harnesses,
            &self.workspaces,
            self.next_id,
            self.next_sidebar_order,
            self.last_used_harness,
            &self.collapsed_projects,
            self.sidebar_width,
            self.diff_sidebar_open,
            self.diff_sidebar_width,
            self.diff_view_mode,
        ) {
            tracing::error!(error = %error, "could not persist application state");
            self.banner = Some(error);
        }
    }

    fn report_cache_error(&mut self, error: String) {
        tracing::error!(error = %error, "session cache operation failed");
        if self.banner.is_none() {
            self.banner = Some(error);
        }
    }

    fn apply_appearance(&mut self, appearance: theme::Appearance, cx: &mut Context<Self>) {
        self.font = appearance.font.into();
        for harness in &mut self.harnesses {
            for turn in &mut harness.turn_diffs {
                turn.refresh_highlights();
            }
            if let Some(preview) = harness.active_turn_preview.as_mut() {
                preview.refresh_highlights();
            }
            for message in harness
                .messages
                .iter_mut()
                .chain(harness.queued_messages.iter_mut())
            {
                message.refresh_theme_colors();
            }
        }
        let conversation_items = self.conversation_list_message_count
            + self.conversation_list_queued_count
            + usize::from(self.conversation_list_working);
        self.conversation_list
            .remeasure_items(0..conversation_items);
        self.diff_display_key = None;

        let inputs = self.composer_inputs.values().cloned().chain([
            self.project_input.clone(),
            self.harness_input.clone(),
            self.thread_rename_input.clone(),
            self.workspace_settings_input.clone(),
            self.extension_input.clone(),
        ]);
        for input in inputs {
            input.update(cx, |_, cx| cx.notify());
        }

        if self.banner.as_ref() == self.config_error.as_ref() {
            self.banner = None;
        }
        self.config_error = None;
    }

    pub(crate) fn session_actions_available(&self) -> bool {
        self.pi_bridge_extension.is_some()
    }

    pub(crate) fn selected_composer_input(&self) -> Option<Entity<TextInput>> {
        self.selected_harness
            .and_then(|id| self.composer_inputs.get(&id).cloned())
    }
}

impl Render for Dirigent {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let window_transparent = bg() & 0xff != 0xff;
        if self.window_transparent != Some(window_transparent) {
            window.set_background_appearance(if window_transparent {
                WindowBackgroundAppearance::Transparent
            } else {
                WindowBackgroundAppearance::Opaque
            });
            self.window_transparent = Some(window_transparent);
        }

        self.frame_timing.collect_frames(Instant::now());
        let frame_timing_labels = self.frame_timing.labels();
        self.sync_path_completion_input(cx);
        let path_completion_anchor = self.path_completion.as_ref().and_then(|completion| {
            let at = completion.replacement.start.checked_sub(1)?;
            self.path_completion_input(completion.target)?
                .read(cx)
                .position_for_offset(at)
        });
        let diff_replaces_thread = self.diff_sidebar_replaces_thread(window);

        if self.focus_normal_mode {
            self.focus_normal_mode = false;
            window.focus(&self.thread_focus, cx);
        } else if self.focus_input {
            self.focus_input = false;
            let input = if self.renaming_harness.is_some() {
                Some(self.thread_rename_input.clone())
            } else if self.pending_dialog.is_some() {
                Some(self.extension_input.clone())
            } else if let Some(edit) = self.editing_message.as_ref() {
                Some(edit.input.clone())
            } else if self.project_settings.is_some() && self.workspace_settings_editing {
                Some(self.workspace_settings_input.clone())
            } else if self.adding_project {
                Some(self.project_input.clone())
            } else if self.creating_harness {
                Some(self.harness_input.clone())
            } else {
                self.selected_composer_input()
            };
            if let Some(input) = input {
                window.focus(&input.focus_handle(cx), cx);
            }
        }

        div()
            .id("app-root")
            .relative()
            .size_full()
            .flex()
            .overflow_hidden()
            .bg(rgb(bg()))
            .font_family(self.font.clone())
            .font_features(FontFeatures::disable_ligatures())
            .text_color(rgb(theme_text()))
            .track_focus(&self.thread_focus)
            .on_key_down(cx.listener(Self::on_root_key_down))
            .on_key_up(cx.listener(Self::on_root_key_up))
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(|this, _, _, cx| {
                    let mut changed = false;
                    if this.keyboard_mode == KeyboardMode::Input {
                        this.enter_normal_mode();
                        changed = true;
                    }
                    if this.keyboard_menu.is_some() {
                        this.close_keyboard_menu();
                        changed = true;
                    }
                    if this.sidebar_menu.take().is_some() {
                        changed = true;
                    }
                    if this.path_completion.take().is_some() {
                        changed = true;
                    }
                    if changed {
                        cx.notify();
                    }
                }),
            )
            .on_click(cx.listener(|this, _, _, cx| {
                let changed = this.composer_dropdown.take().is_some()
                    | std::mem::take(&mut this.diff_turn_dropdown_open);
                if changed {
                    cx.notify();
                }
            }))
            .when(
                self.composer_dropdown.is_some()
                    || self.sidebar_menu.is_some()
                    || self.path_completion.is_some(),
                |element| {
                    element.child(
                        deferred(
                            div()
                                .id("dropdown-dismiss-backdrop")
                                .absolute()
                                .inset_0()
                                .size_full()
                                .on_click(cx.listener(|this, _, _, cx| {
                                    let changed = this.composer_dropdown.take().is_some()
                                        | std::mem::take(&mut this.diff_turn_dropdown_open)
                                        | this.sidebar_menu.take().is_some()
                                        | this.path_completion.take().is_some();
                                    if changed {
                                        cx.notify();
                                    }
                                })),
                        )
                        .priority(1),
                    )
                },
            )
            .child(self.render_sidebar(window, cx))
            .when(!diff_replaces_thread, |element| {
                element.child(self.render_center(window, cx))
            })
            .child(self.render_diff_sidebar(window, cx))
            .when_some(self.keyboard_menu, |element, menu| {
                element.child(self.render_keyboard_menu(menu, cx))
            })
            .when_some(path_completion_anchor, |element, anchor| {
                let width = 520.0;
                let left = anchor.x.as_f32().clamp(
                    8.0,
                    (window.viewport_size().width.as_f32() - width - 8.0).max(8.0),
                );
                element.child(
                    div()
                        .absolute()
                        .left(px(left))
                        .bottom(window.viewport_size().height - anchor.y + px(2.0))
                        .w(px(width))
                        .child(self.render_path_completion_menu(cx)),
                )
            })
            .when_some(self.pending_workspace_deletion, |element, harness_id| {
                element.child(self.render_workspace_delete_dialog(harness_id, cx))
            })
            .when_some(self.preview_image.clone(), |element, image| {
                element.child(
                    div()
                        .id("image-preview-backdrop")
                        .absolute()
                        .inset_0()
                        .size_full()
                        .flex()
                        .items_center()
                        .justify_center()
                        .p_8()
                        .bg(gpui::rgba(0x000000cc))
                        .occlude()
                        .on_click(cx.listener(|this, _, _, cx| {
                            this.preview_image = None;
                            cx.notify();
                        }))
                        .child(
                            div()
                                .id("image-preview")
                                .w(window.viewport_size().width * 0.60)
                                .h(window.viewport_size().height * 0.60)
                                .flex()
                                .items_center()
                                .justify_center()
                                .on_click(cx.listener(|_, _, _, cx| cx.stop_propagation()))
                                .child(
                                    img(image)
                                        .size_full()
                                        .object_fit(ObjectFit::Contain)
                                        .rounded_lg(),
                                ),
                        ),
                )
            })
            .when(
                self.debug_panels_visible && self.keyboard_menu.is_none(),
                |element| {
                    element.child(
                        div()
                            .absolute()
                            .right(px(8.0))
                            .bottom(px(8.0))
                            .px_2()
                            .py_1()
                            .flex()
                            .flex_col()
                            .items_end()
                            .rounded_md()
                            .border_1()
                            .border_color(rgb(border()))
                            .bg(rgb(bg()).opacity(0.87))
                            .text_xs()
                            .text_right()
                            .text_color(rgb(muted()))
                            .children(
                                frame_timing_labels
                                    .into_iter()
                                    .map(|label| div().w_full().child(label)),
                            ),
                    )
                },
            )
    }
}
