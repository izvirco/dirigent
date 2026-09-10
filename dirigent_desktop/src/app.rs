//! Defines the central application state and shared application-level helpers.

mod branching;
mod delegation;
mod diff;
mod harness;
mod keyboard;
mod managed_workspace;
mod message_parsing;
mod path_completion;
mod render;
mod runtime;
mod session;
mod sidebar;
mod startup;
#[cfg(feature = "self-update")]
mod update;
mod workspace;

use std::{
    collections::{HashMap, HashSet, VecDeque},
    fs,
    ops::Range,
    path::PathBuf,
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    },
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

pub(crate) use self::sidebar::SidebarState;
use self::{harness::*, message_parsing::*, path_completion::*};

use crate::{
    cache::SessionCache,
    diff::{DiffScope, DiffSelectionReference, DiffViewMode, TurnDiff, TurnDiffStatus},
    image_attachment::normalize_for_harness,
    math::{MathRenderKey, MathRenderState, MathRenderTask},
    model::{
        CodexUsage, CodexUsageWindow, ContextUsage, Harness, HarnessStatus, Id, ManagedWorkspace,
        Message, MessageRole, PiProcessState, Project, RetryStatus, SessionStats, WorkspaceState,
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

const COMPOSER_PLACEHOLDER: &str = "Little Dragon - Little Man (Marcus Intalex Remix)";
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

struct CachedSessionRebuildRequest {
    harness_id: Id,
    entries: Arc<Vec<Value>>,
    leaf_id: Option<String>,
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
            Self::Normal => "NOR",
            Self::Input => "INS",
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

/// Merges protocol updates only when dropping the boundary between them is lossless.
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
    // Partial tool results are snapshots, so only the newest update is useful.
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
    if !matches!(previous_kind, Some("text_delta" | "thinking_delta"))
        || previous_kind != next_kind
        || previous_value.pointer("/assistantMessageEvent/contentIndex")
            != next_value.pointer("/assistantMessageEvent/contentIndex")
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
    // Text and thinking updates are deltas rather than snapshots and must be concatenated.
    previous_delta.push_str(delta);
    true
}

/// Reduces high-frequency streaming traffic before it reaches GPUI's update loop.
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

pub(crate) type DiffDisplayKey = (Id, u64, DiffScope);

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
    pub(crate) about_open: bool,
    pub(crate) settings: Option<crate::ui::settings::Settings>,
    pub(crate) pi_version: String,
    pub(crate) sidebar_state: SidebarState,
    pub(crate) sidebar_width: f32,
    pub(crate) sidebar_scroll: ScrollHandle,
    sidebar_layout_persist_task: Option<Task<()>>,
    pub(crate) diff_sidebar_open: bool,
    pub(crate) diff_sidebar_width: f32,
    pub(crate) diff_view_mode: DiffViewMode,
    pub(crate) diff_scope: DiffScope,
    pub(crate) selected_diff_turn: Option<(Id, u64)>,
    pub(crate) diff_turn_dropdown_open: bool,
    pub(crate) diff_list: ListState,
    pub(crate) diff_display_key: Option<DiffDisplayKey>,
    pub(crate) diff_display: Option<TurnDiff>,
    pub(crate) diff_render_cache: DiffRenderCache,
    pub(crate) diff_file_collapse_overrides: HashMap<DiffDisplayKey, HashMap<String, bool>>,
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
    #[cfg(feature = "self-update")]
    pub(crate) update_state: crate::update::UpdateState,
    #[cfg(feature = "self-update")]
    update_events: Sender<crate::update::UpdateEvent>,
    config_error: Option<String>,
    pub(crate) font: SharedString,
    pub(crate) onboarding: Option<crate::ui::onboarding::Onboarding>,
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
    cached_session_rebuilds: Sender<CachedSessionRebuildRequest>,
    pending_session_rebuilds: HashMap<Id, u64>,
    requested_session_rebuilds: HashSet<Id>,
    next_session_rebuild_job_id: u64,
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
    pub(crate) math_renders: std::cell::RefCell<HashMap<MathRenderKey, MathRenderState>>,
    pub(crate) math_render_tasks: Sender<MathRenderTask>,
    runtime_events: Sender<RuntimeEvent>,
    workspace_events: Sender<WorkspaceEvent>,
    title_events: Sender<TitleGenerationEvent>,
    diff_tasks: Sender<self::diff::DiffTask>,
    turn_highlight_tasks: Sender<self::diff::TurnHighlightTask>,
    turn_highlight_generation: Arc<AtomicU64>,
    pending_turn_highlight: Option<(u64, DiffDisplayKey)>,
    highlighted_diff_display: Option<(u64, DiffDisplayKey)>,
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

    /// Debounces resize writes so dragging a sidebar does not flood the storage worker.
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
        let result = self.state_database.save_sidebar_layout(
            self.sidebar_state.is_open(),
            self.sidebar_width,
            self.diff_sidebar_width,
        );
        let elapsed = started.elapsed();
        if elapsed >= Duration::from_millis(16) {
            tracing::warn!(
                elapsed_ms = duration_ms(elapsed),
                sidebar_open = self.sidebar_state.is_open(),
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
            self.sidebar_state.is_open(),
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
        // Color values are resolved into cached messages and the visible diff, so a theme reload
        // must invalidate more than the top-level GPUI view.
        self.invalidate_diff_display_highlights();
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
