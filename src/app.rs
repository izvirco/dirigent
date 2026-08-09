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
    ImageFormat, IntoElement, KeyDownEvent, KeyUpEvent, ListAlignment, ListOffset, ListState,
    MouseButton, ObjectFit, ScrollHandle, SharedString, StyledImage, Task, Window,
    WindowBackgroundAppearance, deferred, div, img, point, prelude::*, profiler, px,
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
    rpc::{PiProcess, RuntimeEvent, RuntimeEventKind, RuntimeTarget},
    storage,
    text_input::{AttachedImage, InputEvent, TextInput},
    theme::{self, bg, border, muted, rgb, theme_text},
    title_generator::{TitleGenerationEvent, TitleProcess},
    ui::{ConversationRenderCache, ConversationScrollAnchor, DiffRenderCache},
    vcs::RepositorySnapshot,
};

const STARTUP_MODEL_REQUEST_ID: &str = "dirigent-startup-model";
const STARTUP_THINKING_REQUEST_ID: &str = "dirigent-startup-thinking";
const UI_HEARTBEAT_INTERVAL: Duration = Duration::from_secs(1);
const UI_STALL_WARNING_THRESHOLD: Duration = Duration::from_secs(2);
const UI_PERFORMANCE_LOG_INTERVAL: Duration = Duration::from_secs(30);
const RUNTIME_EVENT_CHANNEL_CAPACITY: usize = 2_048;
const RUNTIME_EVENT_BATCH_LIMIT: usize = 64;
const RUNTIME_EVENT_QUEUE_WARNING_THRESHOLD: usize = 512;
const RUNTIME_EVENT_BATCH_WINDOW: Duration = Duration::from_millis(2);
const RUNTIME_EVENT_YIELD_INTERVAL: Duration = Duration::from_millis(16);
const SLOW_RUNTIME_EVENT_BATCH: Duration = Duration::from_millis(16);
const OLD_RUNTIME_EVENT_THRESHOLD: Duration = Duration::from_millis(100);

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

struct PendingDiffPrompt {
    job_id: u64,
    process_generation: u64,
    commands: Vec<Value>,
    started_at: Instant,
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

#[derive(Default)]
struct RuntimeEventLoopStats {
    batches: u64,
    received_events: u64,
    handled_events: u64,
    max_queue_depth: usize,
    max_queue_age: Duration,
    max_collect: Duration,
    max_handle: Duration,
    last_log_at: Option<Instant>,
}

impl RuntimeEventLoopStats {
    fn record(
        &mut self,
        raw_count: usize,
        handled_count: usize,
        queue_depth: usize,
        oldest_age: Duration,
        collect: Duration,
        handle: Duration,
    ) {
        self.batches += 1;
        self.received_events += raw_count as u64;
        self.handled_events += handled_count as u64;
        self.max_queue_depth = self.max_queue_depth.max(queue_depth);
        self.max_queue_age = self.max_queue_age.max(oldest_age);
        self.max_collect = self.max_collect.max(collect);
        self.max_handle = self.max_handle.max(handle);
    }

    fn log_periodic(&mut self, now: Instant, queue_depth: usize) {
        let last_log_at = self.last_log_at.get_or_insert(now);
        if now.saturating_duration_since(*last_log_at) < UI_PERFORMANCE_LOG_INTERVAL {
            return;
        }
        tracing::debug!(
            batches = self.batches,
            received_events = self.received_events,
            handled_events = self.handled_events,
            coalesced_events = self.received_events.saturating_sub(self.handled_events),
            queue_depth,
            max_queue_depth = self.max_queue_depth,
            max_queue_age_ms = duration_ms(self.max_queue_age),
            max_collect_us = duration_us(self.max_collect),
            max_handle_us = duration_us(self.max_handle),
            "runtime event loop performance"
        );
        self.batches = 0;
        self.received_events = 0;
        self.handled_events = 0;
        self.max_queue_depth = queue_depth;
        self.max_queue_age = Duration::ZERO;
        self.max_collect = Duration::ZERO;
        self.max_handle = Duration::ZERO;
        *last_log_at = now;
    }
}

fn duration_ms(duration: Duration) -> u64 {
    duration.as_millis().min(u64::MAX as u128) as u64
}

fn duration_us(duration: Duration) -> u64 {
    duration.as_micros().min(u64::MAX as u128) as u64
}

fn try_coalesce_runtime_delta(previous: &mut RuntimeEvent, next: &RuntimeEvent) -> bool {
    if previous.target() != next.target() {
        return false;
    }
    let (
        RuntimeEventKind::Json {
            value: previous_value,
            ..
        },
        RuntimeEventKind::Json {
            value: next_value, ..
        },
    ) = (&mut previous.kind, &next.kind)
    else {
        return false;
    };
    let previous_event_type = previous_value.get("type").and_then(Value::as_str);
    let next_event_type = next_value.get("type").and_then(Value::as_str);
    if previous_event_type == Some("tool_execution_update")
        && next_event_type == previous_event_type
        && previous_value.get("toolCallId") == next_value.get("toolCallId")
    {
        *previous_value = next_value.clone();
        return true;
    }

    if previous_event_type != Some("message_update") || next_event_type != previous_event_type {
        return false;
    }
    let previous_kind = previous_value
        .pointer("/assistantMessageEvent/type")
        .and_then(Value::as_str);
    let next_kind = next_value
        .pointer("/assistantMessageEvent/type")
        .and_then(Value::as_str);
    if !matches!(previous_kind, Some("text_delta" | "thinking_delta")) || previous_kind != next_kind
    {
        return false;
    }
    let Some(delta) = next_value
        .pointer("/assistantMessageEvent/delta")
        .and_then(Value::as_str)
    else {
        return false;
    };
    let Some(Value::String(previous_delta)) =
        previous_value.pointer_mut("/assistantMessageEvent/delta")
    else {
        return false;
    };
    previous_delta.push_str(delta);
    true
}

fn coalesce_runtime_events(events: Vec<RuntimeEvent>) -> Vec<RuntimeEvent> {
    let mut coalesced: Vec<RuntimeEvent> = Vec::with_capacity(events.len());
    for event in events {
        // Streams from separate harnesses may be interleaved. Cross-harness ordering has no
        // semantic meaning, so merge with the latest event for this target until a same-target
        // protocol boundary is encountered.
        let mut merged = false;
        for previous in coalesced.iter_mut().rev() {
            if previous.target() != event.target() {
                continue;
            }
            merged = try_coalesce_runtime_delta(previous, &event);
            break;
        }
        if !merged {
            coalesced.push(event);
        }
    }
    coalesced
}

fn runtime_event_kinds(events: &[RuntimeEvent]) -> String {
    let mut counts = HashMap::<&str, usize>::new();
    for event in events {
        *counts.entry(event.diagnostic_kind()).or_default() += 1;
    }
    let mut counts = counts.into_iter().collect::<Vec<_>>();
    counts.sort_unstable_by_key(|(kind, _)| *kind);
    counts
        .into_iter()
        .map(|(kind, count)| format!("{kind}:{count}"))
        .collect::<Vec<_>>()
        .join(",")
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
    sidebar_layout_persist_task: Option<Task<()>>,
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
    pub(crate) conversation_ruler_last_layout_at: Instant,
    pub(crate) conversation_scroll_dragging: bool,
    pub(crate) conversation_scroll_drag_offset: f32,
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
    repository_tasks: Sender<self::managed_workspace::RepositoryRefreshTask>,
    pending_repository_refreshes: HashSet<Id>,
    dirty_repository_refreshes: HashSet<Id>,
    repository_refreshed_at: HashMap<Id, Instant>,
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
    diff_tasks: Sender<self::diff::DiffTask>,
    turn_highlight_tasks: Sender<self::diff::TurnHighlightTask>,
    turn_highlight_generation: u64,
    pending_diff_prompts: HashMap<Id, PendingDiffPrompt>,
    pending_diff_previews: HashMap<Id, u64>,
    dirty_diff_previews: HashSet<Id>,
    next_diff_job_id: u64,
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
        std::thread::Builder::new()
            .name("dirigent-turn-highlights".into())
            .spawn(move || {
                self::diff::run_turn_highlight_worker(
                    turn_highlight_task_rx,
                    turn_highlight_result_tx,
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
            diff_tasks: diff_task_tx,
            turn_highlight_tasks: turn_highlight_task_tx,
            turn_highlight_generation: 0,
            pending_diff_prompts: HashMap::new(),
            pending_diff_previews: HashMap::new(),
            dirty_diff_previews: HashSet::new(),
            next_diff_job_id: 1,
            title_processes: HashMap::new(),
        };
        this.queue_turn_diff_highlights();
        if let Some(project_id) = selected_project {
            this.refresh_repository(project_id);
        }
        if let Some(harness_id) = selected_harness {
            this.start_harness(harness_id, None);
        }
        this
    }

    fn log_ui_performance_snapshot(&mut self, stall_delay: Option<Duration>) {
        self.frame_timing.collect_frames(Instant::now());
        let frame = self.frame_timing.summary;
        let selected = self.selected_harness.and_then(|id| {
            self.harnesses
                .iter()
                .find(|harness| harness.id == id)
                .map(|harness| {
                    let status = match harness.status {
                        HarnessStatus::Starting => "starting",
                        HarnessStatus::Idle => "idle",
                        HarnessStatus::Working => "working",
                        HarnessStatus::Failed => "failed",
                        HarnessStatus::Stopped => "stopped",
                    };
                    (
                        harness.id,
                        status,
                        harness.messages.len(),
                        harness.queued_messages.len(),
                    )
                })
        });
        let (selected_harness_id, selected_status, message_count, queued_message_count) = selected
            .map(|(id, status, messages, queued)| (Some(id), Some(status), messages, queued))
            .unwrap_or((None, None, 0, 0));
        let working_harnesses = self
            .harnesses
            .iter()
            .filter(|harness| harness.status == HarnessStatus::Working)
            .count();
        let draw_average_ms = frame.map(|summary| summary.draw_average_ms);
        let draw_p99_ms = frame.map(|summary| summary.draw_p99_ms);
        let draw_maximum_ms = frame.map(|summary| summary.draw_maximum_ms);
        let response_p99_ms = frame.and_then(|summary| summary.response_p99_ms);
        let invalidations_average = frame.map(|summary| summary.invalidations_average);
        let frame_samples = frame.map_or(0, |summary| summary.sample_count);
        let runtime_queue_depth = self.runtime_events.len();
        let workspace_queue_depth = self.workspace_events.len();
        let title_queue_depth = self.title_events.len();
        let diff_queue_depth = self.diff_tasks.len();
        let render_item_count = self.conversation_render_cache.len();
        let pending_diff_prompts = self.pending_diff_prompts.len();
        let pending_diff_previews = self.pending_diff_previews.len();
        let repository_queue_depth = self.repository_tasks.len();
        let pending_repository_refreshes = self.pending_repository_refreshes.len();

        if let Some(delay) = stall_delay {
            tracing::warn!(
                delay_ms = duration_ms(delay),
                interval_ms = duration_ms(UI_HEARTBEAT_INTERVAL),
                ?selected_harness_id,
                ?selected_status,
                message_count,
                queued_message_count,
                render_item_count,
                harness_count = self.harnesses.len(),
                working_harnesses,
                runtime_queue_depth,
                workspace_queue_depth,
                title_queue_depth,
                diff_queue_depth,
                pending_diff_prompts,
                pending_diff_previews,
                repository_queue_depth,
                pending_repository_refreshes,
                ?draw_average_ms,
                ?draw_p99_ms,
                ?draw_maximum_ms,
                ?response_p99_ms,
                ?invalidations_average,
                frame_samples,
                "UI event loop heartbeat delayed"
            );
        } else {
            tracing::debug!(
                ?selected_harness_id,
                ?selected_status,
                message_count,
                queued_message_count,
                render_item_count,
                harness_count = self.harnesses.len(),
                working_harnesses,
                runtime_queue_depth,
                workspace_queue_depth,
                title_queue_depth,
                diff_queue_depth,
                pending_diff_prompts,
                pending_diff_previews,
                repository_queue_depth,
                pending_repository_refreshes,
                ?draw_average_ms,
                ?draw_p99_ms,
                ?draw_maximum_ms,
                ?response_p99_ms,
                ?invalidations_average,
                frame_samples,
                "UI performance snapshot"
            );
        }
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
        // Starting an archived thread to display it can emit runtime lifecycle events.
        // Those events are not new activity and must not reorder the archive.
        if self.harnesses[index].archived {
            return;
        }
        self.harnesses[index].sidebar_order = self.allocate_sidebar_order();
    }

    pub(crate) fn schedule_sidebar_layout_persist(&mut self, cx: &mut Context<Self>) {
        self.sidebar_layout_persist_task = Some(cx.spawn(async move |this, cx| {
            cx.background_executor()
                .timer(Duration::from_millis(200))
                .await;
            let _ = this.update(cx, |this, _| this.persist_sidebar_layout());
        }));
    }

    fn persist_sidebar_layout(&mut self) {
        let started = Instant::now();
        let result = self
            .state_database
            .save_sidebar_layout(self.sidebar_width, self.diff_sidebar_width);
        let elapsed = started.elapsed();
        if elapsed >= Duration::from_millis(16) {
            tracing::warn!(
                elapsed_ms = duration_ms(elapsed),
                sidebar_width = self.sidebar_width,
                diff_sidebar_width = self.diff_sidebar_width,
                "slow sidebar layout persistence"
            );
        }
        if let Err(error) = result {
            tracing::error!(error = %error, "could not persist sidebar layout");
            self.banner = Some(error);
        }
    }

    pub(crate) fn persist_last_used_harness(&mut self) {
        let started = Instant::now();
        let result = self
            .state_database
            .save_last_used_harness(self.last_used_harness);
        let elapsed = started.elapsed();
        if elapsed >= Duration::from_millis(16) {
            tracing::warn!(
                elapsed_ms = duration_ms(elapsed),
                last_used_harness = ?self.last_used_harness,
                "slow selected thread persistence"
            );
        }
        if let Err(error) = result {
            tracing::error!(error = %error, "could not persist selected thread");
            self.banner = Some(error);
        }
    }

    pub(crate) fn persist_harness_session_file(&mut self, index: usize) {
        let harness = &self.harnesses[index];
        let started = Instant::now();
        let result = self
            .state_database
            .save_harness_session_file(harness.id, harness.session_file.as_deref());
        let elapsed = started.elapsed();
        if elapsed >= Duration::from_millis(16) {
            tracing::warn!(
                elapsed_ms = duration_ms(elapsed),
                harness_id = harness.id,
                "slow thread session path persistence"
            );
        }
        if let Err(error) = result {
            tracing::error!(error = %error, harness_id = harness.id, "could not persist thread session path");
            self.banner = Some(error);
        }
    }

    pub(crate) fn persist(&mut self) {
        let started = Instant::now();
        let result = self.state_database.save(
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
        );
        let elapsed = started.elapsed();
        if elapsed >= Duration::from_millis(16) {
            tracing::warn!(
                elapsed_ms = duration_ms(elapsed),
                project_count = self.projects.len(),
                harness_count = self.harnesses.len(),
                workspace_count = self.workspaces.len(),
                "slow application state persistence"
            );
        }
        if let Err(error) = result {
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
        self.queue_turn_diff_highlights();
        for harness in &mut self.harnesses {
            for message in harness
                .messages
                .iter_mut()
                .chain(harness.queued_messages.iter_mut())
            {
                message.refresh_theme_colors();
            }
        }
        self.conversation_list
            .remeasure_items(0..self.conversation_render_cache.len());
        self.conversation_render_cache.invalidate_ruler_layout();
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
