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
    ObjectFit, ScrollHandle, SharedString, StyledImage, Window, div, img, point, prelude::*,
    profiler, px, rgb,
};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::{
    cache::SessionCache,
    model::{ContextUsage, Harness, HarnessStatus, Id, Message, MessageRole, Project},
    platform,
    rpc::{PiProcess, RuntimeEvent, RuntimeTarget},
    storage,
    text_input::{AttachedImage, InputEvent, TextInput},
    theme::{self, bg, border, muted, theme_text},
};

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
    pub(crate) selected_project: Option<Id>,
    pub(crate) selected_harness: Option<Id>,
    pub(crate) last_used_harness: Option<Id>,
    pub(crate) adding_project: bool,
    pub(crate) creating_harness: bool,
    pub(crate) sidebar_width: f32,
    pub(crate) collapsed_projects: HashSet<Id>,
    pub(crate) sidebar_menu: Option<SidebarMenu>,
    pub(crate) renaming_harness: Option<Id>,
    pub(crate) project_input: Entity<TextInput>,
    pub(crate) harness_input: Entity<TextInput>,
    pub(crate) thread_rename_input: Entity<TextInput>,
    composer_inputs: HashMap<Id, Entity<TextInput>>,
    pub(crate) extension_input: Entity<TextInput>,
    pub(crate) pending_dialog: Option<PendingDialog>,
    pub(crate) banner: Option<String>,
    config_error: Option<String>,
    pub(crate) font: SharedString,
    pub(crate) conversation_list: ListState,
    conversation_list_message_count: usize,
    conversation_list_working: bool,
    pub(crate) conversation_scroll_dragging: bool,
    pub(crate) model_picker_scroll: ScrollHandle,
    pub(crate) composer_dropdown: Option<ComposerDropdown>,
    pub(crate) path_completion: Option<PathCompletion>,
    project_file_pickers: HashMap<Id, SharedFilePicker>,
    fuzzy_index_events: Sender<FuzzyIndexReady>,
    pub(crate) available_models: Vec<AvailableModel>,
    available_models_by_project: HashMap<Id, Vec<AvailableModel>>,
    available_thinking_levels: HashMap<(Id, String), Vec<String>>,
    session_cache: Option<SessionCache>,
    pub(crate) draft_model: Option<String>,
    pub(crate) draft_thinking_level: Option<String>,
    pub(crate) draft_nix_enabled: bool,
    // Loads project-local pi defaults before the project's first harness exists.
    project_probe: Option<(Id, PiProcess)>,
    pub(crate) thread_text_selection: Option<ThreadTextSelection>,
    pub(crate) hovered_copy_message: Option<(Id, usize)>,
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

fn start_fuzzy_index(
    path: &std::path::Path,
    watch: bool,
    ready: FuzzyIndexReady,
    ready_events: Sender<FuzzyIndexReady>,
) -> Result<SharedFilePicker, String> {
    let picker = SharedFilePicker::default();
    FilePicker::new_with_shared_state(
        picker.clone(),
        SharedFrecency::default(),
        FilePickerOptions {
            base_path: path.to_string_lossy().into_owned(),
            mode: FFFMode::Ai,
            watch,
            enable_fs_root_scanning: true,
            enable_home_dir_scanning: true,
            ..Default::default()
        },
    )
    .map_err(|error| format!("could not index {}: {error}", path.display()))?;

    let waiting_picker = picker.clone();
    std::thread::spawn(move || {
        waiting_picker.wait_for_scan(Duration::from_secs(60));
        let _ = ready_events.send_blocking(ready);
    });
    Ok(picker)
}

fn composer_path_query(text: &str, cursor: usize) -> Option<(Range<usize>, String)> {
    let prefix = text.get(..cursor)?;
    let at = prefix.rfind('@')?;
    if at > 0 {
        let preceding = prefix[..at].chars().next_back()?;
        if !preceding.is_whitespace() && !"([{<".contains(preceding) {
            return None;
        }
    }
    let query = &prefix[at + 1..];
    if query.chars().any(char::is_whitespace) {
        return None;
    }
    Some((at + 1..cursor, query.to_string()))
}

fn resolve_tilde_path(raw: &str, home: &std::path::Path) -> Option<PathBuf> {
    if raw == "~" {
        return Some(home.to_path_buf());
    }
    if let Some(relative) = raw.strip_prefix("~/").or_else(|| raw.strip_prefix(r"~\")) {
        return Some(home.join(relative));
    }
    (!raw.starts_with('~')).then(|| PathBuf::from(raw))
}

fn directory_path_query(raw: &str) -> Option<(PathBuf, String)> {
    if raw.is_empty() {
        return None;
    }
    let path = if raw.starts_with('~') {
        let home = platform::home_dir()?;
        resolve_tilde_path(raw, &home)?
    } else {
        PathBuf::from(raw)
    };
    if !path.is_absolute() {
        return None;
    }
    if path.is_dir() {
        path.parent()?;
        let has_trailing_separator = raw.ends_with('/') || raw.ends_with(std::path::MAIN_SEPARATOR);
        return has_trailing_separator.then(|| (path, String::new()));
    }
    let parent = path.parent()?.to_path_buf();
    parent.parent()?;
    if !parent.is_dir() {
        return None;
    }
    let query = path.file_name()?.to_string_lossy().into_owned();
    (!query.is_empty()).then_some((parent, query))
}

fn fuzzy_file_results(picker: &SharedFilePicker, query: &str) -> Vec<String> {
    let Ok(guard) = picker.read() else {
        return Vec::new();
    };
    let Some(picker) = guard.as_ref() else {
        return Vec::new();
    };
    let parser = QueryParser::default();
    let query = parser.parse(query);
    picker
        .fuzzy_search(
            &query,
            None,
            FuzzySearchOptions {
                pagination: PaginationArgs {
                    offset: 0,
                    limit: 8,
                },
                ..Default::default()
            },
        )
        .items
        .into_iter()
        .map(|item| item.relative_path(picker))
        .collect()
}

fn directory_child_results(root: &std::path::Path, query: &str) -> Vec<String> {
    let query = query.to_lowercase();
    let Ok(entries) = fs::read_dir(root) else {
        return Vec::new();
    };
    let mut children = entries
        .filter_map(Result::ok)
        .filter_map(|entry| {
            let path = entry.path();
            if !path.is_dir() {
                return None;
            }
            let name = entry.file_name().to_string_lossy().into_owned();
            let normalized = name.to_lowercase();
            (query.is_empty() || normalized.contains(&query)).then_some((
                !normalized.starts_with(&query),
                normalized,
                path.to_string_lossy().into_owned(),
            ))
        })
        .collect::<Vec<_>>();
    children
        .sort_unstable_by(|left, right| left.0.cmp(&right.0).then_with(|| left.1.cmp(&right.1)));
    children
        .into_iter()
        .take(8)
        .map(|(_, _, path)| path)
        .collect()
}

fn conversation_list_splice(
    old_message_count: usize,
    old_working: bool,
    new_message_count: usize,
    new_working: bool,
) -> (Range<usize>, usize) {
    let retained_message_count = old_message_count.min(new_message_count);
    let old_item_count = old_message_count + usize::from(old_working);
    let new_item_count = new_message_count + usize::from(new_working);
    (
        retained_message_count..old_item_count,
        new_item_count - retained_message_count,
    )
}

impl Dirigent {
    pub(crate) fn new(cx: &mut Context<Self>) -> Self {
        let (config_dir, appearance, mut config_error) = match theme::initialize() {
            Ok((config_dir, appearance)) => (Some(config_dir), appearance, None),
            Err(error) => (
                platform::config_dir().ok(),
                theme::default_appearance(),
                Some(error),
            ),
        };
        if let Some(config_dir) = config_dir {
            let (config_tx, config_rx) = async_channel::unbounded();
            if let Err(error) = theme::watch(config_dir.clone(), config_tx) {
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

        let project_input = cx.new(|cx| TextInput::new(project_path_placeholder(), cx));
        let harness_input = cx.new(|cx| {
            TextInput::new("Send a message to pi…", cx)
                .borderless()
                .multiline()
        });
        let extension_input = cx.new(|cx| TextInput::new("Enter a value…", cx));
        let thread_rename_input =
            cx.new(|cx| TextInput::new("Thread name", cx).borderless().compact());

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

        let (loaded, mut banner) = match storage::load() {
            Ok(state) => (state, None),
            Err(error) => (
                storage::LoadedState {
                    projects: Vec::new(),
                    harnesses: Vec::new(),
                    next_id: 1,
                    next_sidebar_order: 1,
                    last_used_harness: None,
                    collapsed_projects: HashSet::new(),
                    sidebar_width: storage::DEFAULT_SIDEBAR_WIDTH,
                },
                Some(error),
            ),
        };
        if banner.is_none() {
            banner = config_error.clone();
        }
        let storage::LoadedState {
            projects,
            mut harnesses,
            next_id,
            next_sidebar_order,
            last_used_harness,
            collapsed_projects,
            sidebar_width,
        } = loaded;
        let session_cache = match SessionCache::open() {
            Ok(cache) => Some(cache),
            Err(error) => {
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
                            if banner.is_none() {
                                banner = Some(format!("could not decode cached models: {error}"));
                            }
                        }
                    },
                    Ok(None) => {}
                    Err(error) => {
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
                Err(error) if banner.is_none() => banner = Some(error),
                Err(_) => {}
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
        let conversation_list_working =
            selected_conversation.is_some_and(|harness| harness.status == HarnessStatus::Working);
        let conversation_item_count =
            conversation_list_message_count + usize::from(conversation_list_working);
        let conversation_list =
            ListState::new(conversation_item_count, ListAlignment::Bottom, px(1_000.0))
                .with_uniform_item_height(px(48.0));
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
            selected_project,
            selected_harness,
            last_used_harness: selected_harness,
            adding_project,
            creating_harness: false,
            sidebar_width: sidebar_width.clamp(200.0, 520.0),
            collapsed_projects,
            sidebar_menu: None,
            renaming_harness: None,
            project_input,
            harness_input,
            thread_rename_input,
            composer_inputs,
            extension_input,
            pending_dialog: None,
            banner,
            config_error,
            font: appearance.font.into(),
            conversation_list,
            conversation_list_message_count,
            conversation_list_working,
            conversation_scroll_dragging: false,
            model_picker_scroll: ScrollHandle::new(),
            composer_dropdown: None,
            path_completion: None,
            project_file_pickers,
            fuzzy_index_events: fuzzy_index_tx,
            available_models,
            available_models_by_project,
            available_thinking_levels,
            session_cache,
            draft_model: None,
            draft_thinking_level: None,
            draft_nix_enabled: true,
            project_probe: None,
            thread_text_selection: None,
            hovered_copy_message: None,
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
        };
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

    fn persist(&mut self) {
        if let Err(error) = storage::save(
            &self.projects,
            &self.harnesses,
            self.next_id,
            self.next_sidebar_order,
            self.last_used_harness,
            &self.collapsed_projects,
            self.sidebar_width,
        ) {
            self.banner = Some(error);
        }
    }

    fn report_cache_error(&mut self, error: String) {
        if self.banner.is_none() {
            self.banner = Some(error);
        }
    }

    fn apply_appearance(&mut self, appearance: theme::Appearance, cx: &mut Context<Self>) {
        self.font = appearance.font.into();
        for message in self
            .harnesses
            .iter_mut()
            .flat_map(|harness| &mut harness.messages)
        {
            message.refresh_theme_colors();
        }
        let conversation_items =
            self.conversation_list_message_count + usize::from(self.conversation_list_working);
        self.conversation_list
            .remeasure_items(0..conversation_items);

        let inputs = self.composer_inputs.values().cloned().chain([
            self.project_input.clone(),
            self.harness_input.clone(),
            self.thread_rename_input.clone(),
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

    pub(crate) fn selected_composer_input(&self) -> Option<Entity<TextInput>> {
        self.selected_harness
            .and_then(|id| self.composer_inputs.get(&id).cloned())
    }

    fn path_completion_input(&self, target: PathCompletionTarget) -> Option<Entity<TextInput>> {
        match target {
            PathCompletionTarget::Project => Some(self.project_input.clone()),
            PathCompletionTarget::NewHarness => Some(self.harness_input.clone()),
            PathCompletionTarget::Harness(id) => self.composer_inputs.get(&id).cloned(),
        }
    }

    fn sync_path_completion_input(&self, cx: &mut Context<Self>) {
        let target = self
            .path_completion
            .as_ref()
            .map(|completion| completion.target);
        let visible = if self.adding_project {
            Some(PathCompletionTarget::Project)
        } else if self.creating_harness {
            Some(PathCompletionTarget::NewHarness)
        } else {
            self.selected_harness.map(PathCompletionTarget::Harness)
        };
        if let Some(visible) = visible
            && let Some(input) = self.path_completion_input(visible)
        {
            input.update(cx, |input, _| {
                input.set_completion_active(target == Some(visible))
            });
        }
    }

    fn refresh_composer_path_completion(
        &mut self,
        target: PathCompletionTarget,
        cx: &mut Context<Self>,
    ) {
        let Some(input) = self.path_completion_input(target) else {
            self.path_completion = None;
            return;
        };
        let query = {
            let input = input.read(cx);
            composer_path_query(input.text(), input.cursor())
        };
        let Some((replacement, query)) = query else {
            self.path_completion = None;
            input.update(cx, |input, _| input.set_completion_active(false));
            return;
        };
        let project_id = match target {
            PathCompletionTarget::NewHarness => self.selected_project,
            PathCompletionTarget::Harness(id) => self
                .harnesses
                .iter()
                .find(|harness| harness.id == id)
                .map(|harness| harness.project_id),
            PathCompletionTarget::Project => None,
        };
        let items = project_id
            .and_then(|id| self.project_file_pickers.get(&id))
            .map_or_else(Vec::new, |picker| fuzzy_file_results(picker, &query));
        let active = !items.is_empty();
        self.path_completion = active.then_some(PathCompletion {
            target,
            items,
            selected: 0,
            replacement,
        });
        input.update(cx, |input, _| input.set_completion_active(active));
        self.composer_dropdown = None;
        cx.notify();
    }

    fn refresh_project_path_completion(&mut self, cx: &mut Context<Self>) {
        let raw = self.project_input.read(cx).text().to_string();
        let Some((root, query)) = directory_path_query(raw.trim()) else {
            self.path_completion = None;
            self.project_input
                .update(cx, |input, _| input.set_completion_active(false));
            return;
        };
        let items = directory_child_results(&root, &query);
        let active = !items.is_empty();
        self.path_completion = active.then_some(PathCompletion {
            target: PathCompletionTarget::Project,
            items,
            selected: 0,
            replacement: 0..raw.len(),
        });
        self.project_input
            .update(cx, |input, _| input.set_completion_active(active));
        cx.notify();
    }

    fn handle_fuzzy_index_ready(&mut self, event: FuzzyIndexReady, cx: &mut Context<Self>) {
        match event {
            FuzzyIndexReady::Project(project_id) => {
                let target = if self.creating_harness && self.selected_project == Some(project_id) {
                    Some(PathCompletionTarget::NewHarness)
                } else {
                    self.selected_harness.and_then(|id| {
                        self.harnesses
                            .iter()
                            .find(|harness| harness.id == id && harness.project_id == project_id)
                            .map(|_| PathCompletionTarget::Harness(id))
                    })
                };
                if let Some(target) = target {
                    self.refresh_composer_path_completion(target, cx);
                }
            }
        }
    }

    fn move_path_completion(&mut self, delta: isize, cx: &mut Context<Self>) {
        let Some(completion) = self.path_completion.as_mut() else {
            return;
        };
        completion.selected = (completion.selected as isize + delta)
            .rem_euclid(completion.items.len() as isize) as usize;
        cx.notify();
    }

    pub(crate) fn choose_path_completion(&mut self, index: usize, cx: &mut Context<Self>) {
        let Some(completion) = self.path_completion.as_mut() else {
            return;
        };
        if index >= completion.items.len() {
            return;
        }
        completion.selected = index;
        self.accept_path_completion(cx);
    }

    fn accept_path_completion(&mut self, cx: &mut Context<Self>) {
        let Some(completion) = self.path_completion.take() else {
            return;
        };
        let Some(value) = completion.items.get(completion.selected).cloned() else {
            return;
        };
        let Some(input) = self.path_completion_input(completion.target) else {
            return;
        };
        let mut replacement = completion.replacement;
        let value = if completion.target == PathCompletionTarget::Project {
            value
        } else {
            if let Some(separator) = input.read(cx).text()[replacement.end..]
                .chars()
                .next()
                .filter(|character| matches!(character, ' ' | '\t'))
            {
                replacement.end += separator.len_utf8();
            }
            format!("{value} ")
        };
        input.update(cx, |input, cx| {
            input.set_completion_active(false);
            input.replace_range(replacement, &value, cx);
        });
        cx.notify();
    }

    fn add_composer_input(&mut self, harness_id: Id, cx: &mut Context<Self>) {
        let input = cx.new(|cx| {
            TextInput::new("Make no mistakes, or else...", cx)
                .borderless()
                .multiline()
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
        self.composer_inputs.insert(harness_id, input);
    }

    fn persist_composer_draft(&mut self, harness_id: Id, cx: &mut Context<Self>) {
        let Some(input) = self.composer_inputs.get(&harness_id) else {
            return;
        };
        let draft = input.read(cx).text().to_string();
        let images = input.read(cx).images();
        let Some(index) = self
            .harnesses
            .iter()
            .position(|harness| harness.id == harness_id)
        else {
            return;
        };
        let text_changed = self.harnesses[index].composer_draft != draft;
        let images_changed = self.harnesses[index].composer_draft_images.len() != images.len()
            || self.harnesses[index]
                .composer_draft_images
                .iter()
                .zip(&images)
                .any(|(cached, current)| {
                    cached.label != current.label || !Arc::ptr_eq(&cached.image, &current.image)
                });
        if !text_changed && !images_changed {
            return;
        }
        self.harnesses[index].composer_draft = draft;
        self.harnesses[index].composer_draft_images = images;
        self.cache_harness_draft(index, images_changed);
    }

    fn show_cached_models(&mut self, project_id: Id) {
        self.available_models = self
            .available_models_by_project
            .get(&project_id)
            .cloned()
            .unwrap_or_default();
    }

    fn cache_harness_entries(&mut self, index: usize) {
        let Some(cache) = self.session_cache.as_ref() else {
            return;
        };
        let harness = &self.harnesses[index];
        let (Some(session_file), Some(entries)) = (
            harness.session_file.as_deref(),
            harness.cached_entries.as_deref(),
        ) else {
            return;
        };
        if let Err(error) =
            cache.save_entries(session_file, entries, harness.cached_leaf_id.as_deref())
        {
            self.report_cache_error(error);
        }
    }

    fn cache_harness_state(&mut self, index: usize) {
        let Some(cache) = self.session_cache.as_ref() else {
            return;
        };
        let harness = &self.harnesses[index];
        let Some(session_file) = harness.session_file.as_deref() else {
            return;
        };
        if let Err(error) = cache.save_session_state(
            session_file,
            harness.model.as_deref(),
            harness.thinking_level.as_deref(),
        ) {
            self.report_cache_error(error);
        }
    }

    fn cache_harness_draft(&mut self, index: usize, include_images: bool) {
        let Some(cache) = self.session_cache.as_ref() else {
            return;
        };
        let harness = &self.harnesses[index];
        let Some(session_file) = harness.session_file.as_deref() else {
            return;
        };
        let result = if include_images {
            let images = harness
                .composer_draft_images
                .iter()
                .map(|attachment| CachedDraftImage {
                    label: attachment.label.clone(),
                    mime_type: attachment.image.format.mime_type().to_string(),
                    data: BASE64.encode(&attachment.image.bytes),
                })
                .collect::<Vec<_>>();
            serde_json::to_vec(&images)
                .map_err(|error| format!("could not encode composer images for caching: {error}"))
                .and_then(|images_json| {
                    cache.save_composer_draft(session_file, &harness.composer_draft, &images_json)
                })
        } else {
            cache.save_composer_text(session_file, &harness.composer_draft)
        };
        if let Err(error) = result {
            self.report_cache_error(error);
        }
    }

    fn cache_project_models(&mut self, project_id: Id) {
        let Some(cache) = self.session_cache.as_ref() else {
            return;
        };
        let Some(project_path) = self
            .projects
            .iter()
            .find(|project| project.id == project_id)
            .map(|project| project.path.clone())
        else {
            return;
        };
        let Some(models) = self.available_models_by_project.get(&project_id) else {
            return;
        };
        let result = serde_json::to_vec(models)
            .map_err(|error| format!("could not encode models for caching: {error}"))
            .and_then(|bytes| cache.save_models(&project_path, &bytes));
        if let Err(error) = result {
            self.report_cache_error(error);
        }
    }

    fn cache_model_thinking_levels(&mut self, project_id: Id, model: &str) {
        let Some(cache) = self.session_cache.as_ref() else {
            return;
        };
        let Some(project_path) = self
            .projects
            .iter()
            .find(|project| project.id == project_id)
            .map(|project| project.path.clone())
        else {
            return;
        };
        let Some(levels) = self
            .available_thinking_levels
            .get(&(project_id, model.to_string()))
        else {
            return;
        };
        if let Err(error) = cache.save_thinking_levels(&project_path, model, levels) {
            self.report_cache_error(error);
        }
    }

    fn reset_conversation_list(&mut self, harness_index: usize) {
        let harness = &self.harnesses[harness_index];
        self.conversation_list_message_count = harness.messages.len();
        self.conversation_list_working = harness.status == HarnessStatus::Working;
        let item_count =
            self.conversation_list_message_count + usize::from(self.conversation_list_working);
        self.conversation_list
            .reset_with_uniform_height(item_count, px(48.0));
        self.conversation_list.set_follow_mode(FollowMode::Tail);
    }

    fn sync_conversation_list(&mut self, harness_index: usize, changed_message: Option<usize>) {
        if self.selected_harness != Some(self.harnesses[harness_index].id) {
            return;
        }
        let harness = &self.harnesses[harness_index];
        let new_message_count = harness.messages.len();
        let new_working = harness.status == HarnessStatus::Working;
        if self.conversation_list_message_count != new_message_count
            || self.conversation_list_working != new_working
        {
            let (old_range, new_item_count) = conversation_list_splice(
                self.conversation_list_message_count,
                self.conversation_list_working,
                new_message_count,
                new_working,
            );
            self.conversation_list.splice(old_range, new_item_count);
            if new_message_count < self.conversation_list_message_count {
                // A completed run is replaced with canonical session entries, which can
                // contain fewer, differently grouped messages than the streamed view.
                self.conversation_list.remeasure_items(0..new_message_count);
            }
            self.conversation_list_message_count = new_message_count;
            self.conversation_list_working = new_working;
        }
        if let Some(index) = changed_message.filter(|index| *index < new_message_count) {
            self.conversation_list.remeasure_items(index..index + 1);
        }
    }

    pub(crate) fn scroll_conversation_to_fraction(&mut self, fraction: f32) {
        let max_offset = self.conversation_list.max_offset_for_scrollbar().y;
        self.conversation_list
            .set_offset_from_scrollbar(point(px(0.0), -max_offset * fraction.clamp(0.0, 1.0)));
    }

    fn scroll_conversation_by_fraction(&mut self, delta: f32) {
        let max_offset = self.conversation_list.max_offset_for_scrollbar().y.as_f32();
        let current = if max_offset > 0.0 {
            (-self
                .conversation_list
                .scroll_px_offset_for_scrollbar()
                .y
                .as_f32()
                / max_offset)
                .clamp(0.0, 1.0)
        } else {
            0.0
        };
        self.scroll_conversation_to_fraction(current + delta);
    }

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

    pub(crate) fn resize_sidebar(&mut self, width: f32) {
        let width = width.clamp(200.0, 520.0);
        if self.sidebar_width == width {
            return;
        }
        self.sidebar_width = width;
        self.persist();
    }

    pub(crate) fn toggle_project_collapsed(&mut self, project_id: Id) {
        if !self.collapsed_projects.remove(&project_id) {
            self.collapsed_projects.insert(project_id);
        }
        self.sidebar_menu = None;
        self.persist();
    }

    pub(crate) fn reorder_project(&mut self, source: Id, target: Id) {
        if source == target {
            return;
        }
        let Some(source_index) = self
            .projects
            .iter()
            .position(|project| project.id == source)
        else {
            return;
        };
        let Some(target_index) = self
            .projects
            .iter()
            .position(|project| project.id == target)
        else {
            return;
        };
        let project = self.projects.remove(source_index);
        self.projects
            .insert(target_index.min(self.projects.len()), project);
        self.persist();
    }

    pub(crate) fn toggle_thread_menu(&mut self, harness_id: Id) {
        let menu = SidebarMenu::Thread(harness_id);
        self.sidebar_menu = (self.sidebar_menu != Some(menu)).then_some(menu);
    }

    pub(crate) fn toggle_project_menu(&mut self, project_id: Id) {
        let menu = SidebarMenu::Project(project_id);
        self.sidebar_menu = (self.sidebar_menu != Some(menu)).then_some(menu);
    }

    pub(crate) fn toggle_bottom_sidebar_menu(&mut self) {
        self.sidebar_menu =
            (self.sidebar_menu != Some(SidebarMenu::Bottom)).then_some(SidebarMenu::Bottom);
    }

    pub(crate) fn begin_renaming_harness(&mut self, harness_id: Id, cx: &mut Context<Self>) {
        let Some(title) = self
            .harnesses
            .iter()
            .find(|harness| harness.id == harness_id)
            .map(|harness| {
                harness
                    .title
                    .split_whitespace()
                    .collect::<Vec<_>>()
                    .join(" ")
            })
        else {
            return;
        };
        self.thread_rename_input
            .update(cx, |input, cx| input.set_text(title, cx));
        self.renaming_harness = Some(harness_id);
        self.sidebar_menu = None;
        self.enter_input_mode(true);
    }

    fn finish_renaming_harness(&mut self, cx: &mut Context<Self>) {
        let Some(id) = self.renaming_harness else {
            return;
        };
        let title = self.thread_rename_input.read(cx).text().trim().to_string();
        if title.is_empty() {
            self.banner = Some("Thread names cannot be empty.".into());
            cx.notify();
            return;
        }
        if let Some(harness) = self.harnesses.iter_mut().find(|harness| harness.id == id) {
            harness.title = title;
            self.persist();
        }
        self.renaming_harness = None;
        self.enter_normal_mode();
        cx.notify();
    }

    fn select_after_harness_hidden(&mut self, project_id: Id) {
        let replacement = self.harness_navigation_ids().into_iter().next();
        if let Some(id) = replacement {
            self.select_harness(id);
        } else {
            self.selected_harness = None;
            self.last_used_harness = None;
            self.selected_project = self
                .projects
                .iter()
                .find(|project| project.id == project_id)
                .or_else(|| self.projects.first())
                .map(|project| project.id);
            self.creating_harness = false;
        }
    }

    pub(crate) fn can_archive_harness(&self, id: Id) -> bool {
        self.harnesses.iter().any(|harness| {
            harness.id == id
                && !matches!(
                    harness.status,
                    HarnessStatus::Starting | HarnessStatus::Working
                )
        })
    }

    pub(crate) fn set_harness_archived(&mut self, id: Id, archived: bool) {
        let Some(index) = self.harnesses.iter().position(|harness| harness.id == id) else {
            return;
        };
        if archived && !self.can_archive_harness(id) {
            return;
        }
        let project_id = self.harnesses[index].project_id;
        self.harnesses[index].archived = archived;
        self.harnesses[index].attention_required = false;
        self.harnesses[index].run_started_at = None;
        if archived {
            self.harnesses[index].has_unread_completion = false;
            self.harnesses[index].process.take();
            self.harnesses[index].status = HarnessStatus::Stopped;
        }
        self.refresh_harness_order(index);
        self.sidebar_menu = None;
        if archived && self.selected_harness == Some(id) {
            self.select_after_harness_hidden(project_id);
        }
        self.persist();
    }

    pub(crate) fn delete_harness(&mut self, id: Id) {
        let Some(index) = self.harnesses.iter().position(|harness| harness.id == id) else {
            return;
        };
        let project_id = self.harnesses[index].project_id;
        self.harnesses.remove(index);
        self.composer_inputs.remove(&id);
        self.sidebar_menu = None;
        if self.renaming_harness == Some(id) {
            self.renaming_harness = None;
        }
        if self.selected_harness == Some(id) {
            self.select_after_harness_hidden(project_id);
        }
        self.persist();
    }

    pub(crate) fn delete_project(&mut self, id: Id) {
        let Some(index) = self.projects.iter().position(|project| project.id == id) else {
            return;
        };
        let removed_harnesses = self
            .harnesses
            .iter()
            .filter(|harness| harness.project_id == id)
            .map(|harness| harness.id)
            .collect::<HashSet<_>>();
        let deleting_selection = self.selected_project == Some(id);
        let was_adding_project = self.adding_project;

        self.projects.remove(index);
        self.harnesses.retain(|harness| harness.project_id != id);
        self.composer_inputs
            .retain(|harness_id, _| !removed_harnesses.contains(harness_id));
        self.collapsed_projects.remove(&id);
        self.project_file_pickers.remove(&id);
        self.available_models_by_project.remove(&id);
        self.available_thinking_levels
            .retain(|(project_id, _), _| *project_id != id);
        if self
            .project_probe
            .as_ref()
            .is_some_and(|(project_id, _)| *project_id == id)
        {
            self.project_probe.take();
        }
        if self
            .renaming_harness
            .is_some_and(|harness_id| removed_harnesses.contains(&harness_id))
        {
            self.renaming_harness = None;
        }
        if self
            .pending_dialog
            .as_ref()
            .is_some_and(|dialog| removed_harnesses.contains(&dialog.harness_id))
        {
            self.pending_dialog = None;
        }
        if self
            .last_used_harness
            .is_some_and(|harness_id| removed_harnesses.contains(&harness_id))
        {
            self.last_used_harness = None;
        }
        self.sidebar_menu = None;

        if deleting_selection {
            self.selected_harness = None;
            self.thread_text_selection = None;
            self.creating_harness = false;
            self.composer_dropdown = None;
            self.path_completion = None;
            self.draft_model = None;
            self.draft_thinking_level = None;

            if !was_adding_project
                && let Some(harness_id) = self.harness_navigation_ids().into_iter().next()
            {
                self.select_harness(harness_id);
            } else {
                self.selected_project = self.projects.first().map(|project| project.id);
                self.adding_project = was_adding_project || self.projects.is_empty();
                if let Some(project_id) = self.selected_project {
                    self.show_cached_models(project_id);
                } else {
                    self.available_models.clear();
                }
            }
        }
        self.persist();
    }

    pub(crate) fn begin_adding_project(&mut self) {
        self.project_probe.take();
        self.path_completion = None;
        self.adding_project = true;
        self.creating_harness = false;
        self.sidebar_menu = None;
        self.banner = None;
    }

    fn start_new_selected_project(&mut self) {
        if let Some(project_id) = self
            .selected_project
            .or_else(|| self.projects.first().map(|p| p.id))
        {
            self.start_new_harness(project_id);
        } else {
            self.begin_adding_project();
        }
        self.enter_input_mode(true);
    }

    fn harness_navigation_ids(&self) -> Vec<Id> {
        let mut inbox = self
            .harnesses
            .iter()
            .filter(|harness| harness.is_in_inbox())
            .collect::<Vec<_>>();
        let mut workpool = self
            .harnesses
            .iter()
            .filter(|harness| harness.is_in_workpool())
            .collect::<Vec<_>>();
        inbox.sort_by_key(|harness| std::cmp::Reverse(harness.sidebar_order));
        workpool.sort_by_key(|harness| std::cmp::Reverse(harness.sidebar_order));
        inbox
            .into_iter()
            .chain(workpool)
            .map(|harness| harness.id)
            .collect()
    }

    fn select_relative_harness(&mut self, delta: isize) {
        let ids = self.harness_navigation_ids();
        if ids.is_empty() {
            return;
        }
        let current = self
            .selected_harness
            .and_then(|id| ids.iter().position(|candidate| *candidate == id))
            .unwrap_or(0) as isize;
        let target = (current + delta).rem_euclid(ids.len() as isize) as usize;
        self.select_harness(ids[target]);
    }

    fn select_edge_harness(&mut self, newest: bool) {
        let ids = self.harness_navigation_ids();
        let target = if newest { ids.first() } else { ids.last() };
        if let Some(id) = target {
            self.select_harness(*id);
        }
    }

    fn select_matching_harness(&mut self, matches: impl Fn(&Harness) -> bool) {
        let ids = self
            .harnesses
            .iter()
            .rev()
            .filter(|harness| !harness.archived && matches(harness))
            .map(|harness| harness.id)
            .collect::<Vec<_>>();
        if ids.is_empty() {
            return;
        }
        let target = self
            .selected_harness
            .and_then(|id| ids.iter().position(|candidate| *candidate == id))
            .map_or(0, |index| (index + 1) % ids.len());
        self.select_harness(ids[target]);
    }

    fn open_project(&mut self, project_id: Id) {
        if let Some(id) = self
            .harnesses
            .iter()
            .filter(|harness| harness.project_id == project_id && harness.archived)
            .max_by_key(|harness| harness.sidebar_order)
            .map(|harness| harness.id)
        {
            self.select_harness(id);
        } else {
            self.start_new_harness(project_id);
        }
    }

    fn select_relative_project(&mut self, delta: isize) {
        if self.projects.is_empty() {
            return;
        }
        let current = self
            .selected_project
            .and_then(|id| self.projects.iter().position(|project| project.id == id))
            .unwrap_or(0) as isize;
        let target = (current + delta).rem_euclid(self.projects.len() as isize) as usize;
        self.open_project(self.projects[target].id);
    }

    fn select_edge_project(&mut self, first: bool) {
        let project_id = if first {
            self.projects.first().map(|project| project.id)
        } else {
            self.projects.last().map(|project| project.id)
        };
        if let Some(project_id) = project_id {
            self.open_project(project_id);
        }
    }

    pub(crate) fn add_project(&mut self, cx: &mut Context<Self>) {
        let raw_path = self.project_input.read(cx).text().trim().to_string();
        if raw_path.is_empty() {
            self.banner = Some("Enter the full path to a project.".into());
            cx.notify();
            return;
        }
        let path = platform::home_dir()
            .and_then(|home| resolve_tilde_path(&raw_path, &home))
            .unwrap_or_else(|| PathBuf::from(&raw_path));
        if !path.is_absolute() {
            self.banner = Some("Project paths must be absolute or start with ~.".into());
            cx.notify();
            return;
        }
        let path = match fs::canonicalize(&path) {
            Ok(path) if path.is_dir() => path,
            Ok(_) => {
                self.banner = Some(format!("{} is not a directory.", path.display()));
                cx.notify();
                return;
            }
            Err(error) => {
                self.banner = Some(format!("Cannot open {}: {error}", path.display()));
                cx.notify();
                return;
            }
        };
        if let Some(project_id) = self
            .projects
            .iter()
            .find(|project| project.path == path)
            .map(|project| project.id)
        {
            self.start_new_harness(project_id);
            self.banner = Some("That project is already connected.".into());
            cx.notify();
            return;
        }

        let name = path
            .file_name()
            .and_then(|name| name.to_str())
            .filter(|name| !name.is_empty())
            .unwrap_or("project")
            .to_string();
        let id = self.allocate_id();
        self.projects.push(Project {
            id,
            name,
            path: path.clone(),
        });
        match start_fuzzy_index(
            &path,
            true,
            FuzzyIndexReady::Project(id),
            self.fuzzy_index_events.clone(),
        ) {
            Ok(picker) => {
                self.project_file_pickers.insert(id, picker);
            }
            Err(error) => self.banner = Some(error),
        }
        self.collapsed_projects.insert(id);
        self.selected_project = Some(id);
        self.show_cached_models(id);
        self.selected_harness = None;
        self.adding_project = false;
        self.creating_harness = true;
        self.draft_model = None;
        self.draft_thinking_level = None;
        self.draft_nix_enabled = true;
        self.banner = None;
        self.project_input.update(cx, |input, cx| input.clear(cx));
        self.persist();
        self.start_project_probe(id);
        cx.notify();
    }

    pub(crate) fn create_harness(&mut self, cx: &mut Context<Self>) {
        let prompt = self.harness_input.read(cx).text().trim().to_string();
        let images = self.harness_input.read(cx).images();
        let Some(project_id) = self.selected_project else {
            self.banner = Some("Connect a project before starting a harness.".into());
            cx.notify();
            return;
        };
        if prompt.is_empty() {
            self.banner = Some("Describe a task before starting pi.".into());
            cx.notify();
            return;
        }
        let title = prompt.chars().take(54).collect::<String>();
        self.project_probe.take();
        let id = self.allocate_id();
        let sidebar_order = self.allocate_sidebar_order();
        let mut harness = Harness::new(id, project_id, title, sidebar_order);
        harness.nix_enabled = self.draft_nix_enabled && self.project_has_devshell(project_id);
        harness.model = self.draft_model.take();
        harness.thinking_level = self.draft_thinking_level.take();
        harness.messages.push(Message::user_with_images(
            prompt.clone(),
            images.iter().map(|image| image.image.clone()).collect(),
        ));
        self.harnesses.push(harness);
        self.selected_harness = Some(id);
        self.last_used_harness = Some(id);
        self.add_composer_input(id, cx);
        self.reset_conversation_list(self.harnesses.len() - 1);
        self.composer_dropdown = None;
        self.path_completion = None;
        self.keyboard_mode = KeyboardMode::Input;
        self.focus_input = true;
        self.creating_harness = false;
        self.banner = None;
        self.harness_input.update(cx, |input, cx| input.clear(cx));
        self.persist();
        self.start_harness(id, Some((prompt, images)));
        cx.notify();
    }

    pub(crate) fn select_harness(&mut self, id: Id) {
        let Some(index) = self.harnesses.iter().position(|harness| harness.id == id) else {
            return;
        };
        self.project_probe.take();
        self.harnesses[index].has_unread_completion = false;
        self.selected_project = Some(self.harnesses[index].project_id);
        self.show_cached_models(self.harnesses[index].project_id);
        self.selected_harness = Some(id);
        self.last_used_harness = Some(id);
        self.thread_text_selection = None;
        self.adding_project = false;
        self.creating_harness = false;
        self.composer_dropdown = None;
        self.path_completion = None;
        self.draft_model = None;
        self.draft_thinking_level = None;
        self.reset_conversation_list(index);
        self.persist();
        self.start_harness(id, None);
    }

    pub(crate) fn start_new_harness(&mut self, project_id: Id) {
        let source_id = self
            .selected_harness
            .filter(|id| {
                self.harnesses.iter().any(|harness| {
                    harness.id == *id && harness.project_id == project_id && !harness.archived
                })
            })
            .or_else(|| {
                self.harnesses
                    .iter()
                    .find(|harness| harness.project_id == project_id && !harness.archived)
                    .map(|harness| harness.id)
            });
        let (model, thinking_level) = source_id
            .and_then(|id| self.harnesses.iter().find(|harness| harness.id == id))
            .map(|harness| (harness.model.clone(), harness.thinking_level.clone()))
            .unwrap_or_default();

        self.selected_project = Some(project_id);
        self.show_cached_models(project_id);
        self.selected_harness = None;
        self.adding_project = false;
        self.creating_harness = true;
        self.composer_dropdown = None;
        self.path_completion = None;
        self.draft_model = model;
        self.draft_thinking_level = thinking_level;
        self.draft_nix_enabled = true;
        self.banner = None;
        self.project_probe.take();

        if let Some(id) = source_id {
            if self.draft_model.is_none()
                || self.draft_thinking_level.is_none()
                || self.available_models.is_empty()
            {
                self.start_harness(id, None);
                if let Some(index) = self.harnesses.iter().position(|harness| harness.id == id) {
                    if self.draft_model.is_none() || self.draft_thinking_level.is_none() {
                        self.send_value(index, json!({"id":"dirigent-state","type":"get_state"}));
                    }
                    if self.available_models.is_empty() {
                        self.send_value(index, json!({"type":"get_available_models"}));
                    }
                }
            }
        } else {
            self.start_project_probe(project_id);
        }
    }

    fn start_project_probe(&mut self, project_id: Id) {
        if self
            .project_probe
            .as_ref()
            .is_some_and(|(active_project_id, _)| *active_project_id == project_id)
        {
            return;
        }
        self.project_probe.take();
        let Some(project) = self
            .projects
            .iter()
            .find(|project| project.id == project_id)
        else {
            return;
        };
        let project_path = project.path.clone();
        let session_name = format!("{} setup", project.name);
        let nix_enabled = self.draft_nix_enabled && project_has_devshell(&project_path);
        match PiProcess::spawn(
            RuntimeTarget::Project(project_id),
            &project_path,
            None,
            &session_name,
            nix_enabled,
            true,
            self.runtime_events.clone(),
        ) {
            Ok(process) => {
                self.project_probe = Some((project_id, process));
                self.send_project_value(
                    project_id,
                    json!({"id":"dirigent-project-state","type":"get_state"}),
                );
                self.send_project_value(project_id, json!({"type":"get_available_models"}));
            }
            Err(error) => self.banner = Some(error),
        }
    }

    fn send_project_value(&mut self, project_id: Id, value: Value) -> bool {
        let result = self
            .project_probe
            .as_ref()
            .filter(|(active_project_id, _)| *active_project_id == project_id)
            .ok_or_else(|| "pi project setup process is not running".to_string())
            .and_then(|(_, process)| process.send(value));
        if let Err(error) = result {
            self.banner = Some(error);
            false
        } else {
            true
        }
    }

    fn start_harness(&mut self, id: Id, initial_prompt: Option<(String, Vec<AttachedImage>)>) {
        let Some(index) = self.harnesses.iter().position(|harness| harness.id == id) else {
            return;
        };
        if self.harnesses[index].process.is_some() {
            if let Some((prompt, images)) = initial_prompt {
                self.send_prompt_command(index, prompt, images);
            }
            return;
        }
        let project_id = self.harnesses[index].project_id;
        let Some(project) = self
            .projects
            .iter()
            .find(|project| project.id == project_id)
        else {
            self.fail_harness(
                index,
                "The project for this harness no longer exists.".into(),
            );
            return;
        };
        let project_path = project.path.clone();
        let session_file = self.harnesses[index].session_file.clone();
        let title = self.harnesses[index].title.clone();
        if initial_prompt.is_some() {
            self.harnesses[index].status = HarnessStatus::Starting;
        }
        self.harnesses[index].error = None;
        self.harnesses[index].process_generation += 1;
        let process_generation = self.harnesses[index].process_generation;
        self.sync_conversation_list(index, None);
        match PiProcess::spawn(
            RuntimeTarget::Harness(id, process_generation),
            &project_path,
            session_file.as_deref(),
            &title,
            self.harnesses[index].nix_enabled && project_has_devshell(&project_path),
            false,
            self.runtime_events.clone(),
        ) {
            Ok(process) => {
                self.harnesses[index].process = Some(process);
                if self.harnesses[index].startup_settings_pending {
                    if let Some((provider, model_id)) =
                        self.harnesses[index].model.clone().and_then(|model| {
                            let (provider, model_id) = model.split_once('/')?;
                            Some((provider.to_string(), model_id.to_string()))
                        })
                    {
                        self.send_value(
                            index,
                            json!({"type":"set_model", "provider":provider, "modelId":model_id}),
                        );
                    }
                    if let Some(level) = self.harnesses[index].thinking_level.clone() {
                        self.send_value(index, json!({"type":"set_thinking_level", "level":level}));
                    }
                    self.harnesses[index].startup_settings_pending = false;
                }
                self.send_value(index, json!({"id":"dirigent-state","type":"get_state"}));
                self.request_context_usage(index);
                if !self.harnesses[index].loaded_messages {
                    self.request_entries(index);
                }
                if let Some((prompt, images)) = initial_prompt {
                    self.send_prompt_command(index, prompt, images);
                }
            }
            Err(error) => self.fail_harness(index, error),
        }
    }

    fn send_value(&mut self, index: usize, value: Value) -> bool {
        let result = self.harnesses[index]
            .process
            .as_ref()
            .ok_or_else(|| "pi is not running".to_string())
            .and_then(|process| process.send(value));
        if let Err(error) = result {
            self.fail_harness(index, error);
            false
        } else {
            true
        }
    }

    fn request_context_usage(&mut self, index: usize) {
        self.send_value(index, json!({"type":"get_session_stats"}));
    }

    fn request_entries(&mut self, index: usize) {
        let cursor = self.harnesses[index]
            .cached_entries
            .as_ref()
            .and_then(|entries| {
                entries
                    .iter()
                    .rev()
                    .find_map(|entry| entry.get("id")?.as_str())
            })
            .map(str::to_string);
        if let Some(cursor) = cursor {
            self.send_value(
                index,
                json!({
                    "id":"dirigent-entries-incremental",
                    "type":"get_entries",
                    "since":cursor,
                }),
            );
        } else {
            self.send_value(
                index,
                json!({"id":"dirigent-entries-full","type":"get_entries"}),
            );
        }
    }

    fn request_thinking_levels(&mut self, index: usize) {
        if self.harnesses[index].model.is_some() {
            self.send_value(index, json!({"type":"get_available_thinking_levels"}));
        }
    }

    fn send_prompt_command(&mut self, index: usize, prompt: String, images: Vec<AttachedImage>) {
        let images = images
            .into_iter()
            .map(|attachment| {
                json!({
                    "type":"image",
                    "data":BASE64.encode(&attachment.image.bytes),
                    "mimeType":attachment.image.format.mime_type(),
                })
            })
            .collect::<Vec<_>>();
        let mut command = json!({"type":"prompt","message":prompt});
        if !images.is_empty() {
            command["images"] = Value::Array(images);
        }
        if self.harnesses[index].status == HarnessStatus::Working {
            command["streamingBehavior"] = Value::String("steer".into());
        }
        if self.send_value(index, command) {
            let started_run = self.harnesses[index].run_started_at.is_none();
            self.harnesses[index].status = HarnessStatus::Working;
            self.harnesses[index].attention_required = false;
            if started_run {
                self.harnesses[index].run_started_at = Some(Instant::now());
                self.harnesses[index].last_run_duration = None;
                self.refresh_harness_order(index);
                self.persist();
            }
            self.sync_conversation_list(index, None);
        }
    }

    pub(crate) fn send_composer(&mut self, cx: &mut Context<Self>) {
        let Some(id) = self.selected_harness else {
            return;
        };
        let Some(input) = self.composer_inputs.get(&id).cloned() else {
            return;
        };
        let message = input.read(cx).text().trim().to_string();
        if message.is_empty() {
            return;
        }
        if self
            .harnesses
            .iter()
            .find(|harness| harness.id == id)
            .is_some_and(|harness| harness.archived)
        {
            self.set_harness_archived(id, false);
        }
        self.start_harness(id, None);
        let Some(index) = self.harnesses.iter().position(|harness| harness.id == id) else {
            return;
        };
        if self.harnesses[index].process.is_none() {
            return;
        }
        let images = input.read(cx).images();
        self.harnesses[index]
            .messages
            .push(Message::user_with_images(
                message.clone(),
                images.iter().map(|image| image.image.clone()).collect(),
            ));
        self.sync_conversation_list(index, None);
        self.conversation_list.scroll_to_end();
        self.composer_dropdown = None;
        self.send_prompt_command(index, message, images);
        input.update(cx, |input, cx| input.clear(cx));
        self.harnesses[index].composer_draft.clear();
        self.harnesses[index].composer_draft_images.clear();
        self.cache_harness_draft(index, true);
        cx.notify();
    }

    pub(crate) fn abort_selected(&mut self) {
        if let Some(index) = self
            .selected_harness
            .and_then(|id| self.harnesses.iter().position(|harness| harness.id == id))
            && self.send_value(index, json!({"type":"abort"}))
        {
            self.harnesses[index].status = HarnessStatus::Idle;
            self.harnesses[index].run_started_at = None;
            self.harnesses[index].attention_required = false;
            self.refresh_harness_order(index);
            let mut changed_message = None;
            for (message_index, message) in self.harnesses[index].messages.iter_mut().enumerate() {
                if message.running {
                    if message.role == MessageRole::Assistant {
                        changed_message = Some(message_index);
                    }
                    message.set_running(false);
                }
            }
            self.persist();
            self.sync_conversation_list(index, changed_message);
        }
    }

    fn restart_selected(&mut self) {
        if let Some(id) = self.selected_harness {
            self.restart_harness(id);
        }
    }

    fn composer_harness_id(&self) -> Option<Id> {
        if let Some(id) = self.selected_harness {
            return Some(id);
        }
        if !self.creating_harness {
            return None;
        }
        let project_id = self.selected_project?;
        self.harnesses
            .iter()
            .find(|harness| harness.project_id == project_id)
            .map(|harness| harness.id)
    }

    pub(crate) fn toggle_composer_dropdown(&mut self, dropdown: ComposerDropdown) {
        if self.composer_dropdown == Some(dropdown) {
            self.composer_dropdown = None;
            return;
        }
        self.composer_dropdown = Some(dropdown);
        if dropdown == ComposerDropdown::Project {
            return;
        }
        if self.creating_harness
            && let Some(project_id) = self.selected_project
            && self
                .project_probe
                .as_ref()
                .is_some_and(|(active_project_id, _)| *active_project_id == project_id)
        {
            match dropdown {
                ComposerDropdown::Model => {
                    self.send_project_value(project_id, json!({"type":"get_available_models"}));
                }
                ComposerDropdown::Reasoning => {
                    self.send_project_value(
                        project_id,
                        json!({"type":"get_available_thinking_levels"}),
                    );
                }
                ComposerDropdown::Project => return,
            }
            return;
        }
        let Some(id) = self.composer_harness_id() else {
            return;
        };
        self.start_harness(id, None);
        if let Some(index) = self.harnesses.iter().position(|harness| harness.id == id) {
            match dropdown {
                ComposerDropdown::Model => {
                    self.send_value(index, json!({"type":"get_available_models"}));
                }
                ComposerDropdown::Reasoning => self.request_thinking_levels(index),
                ComposerDropdown::Project => {}
            }
        }
    }

    pub(crate) fn select_model(&mut self, provider: String, model_id: String) {
        self.composer_dropdown = None;
        if self.creating_harness {
            self.draft_model = Some(format!("{provider}/{model_id}"));
            if let Some(project_id) = self.selected_project
                && self
                    .project_probe
                    .as_ref()
                    .is_some_and(|(active_project_id, _)| *active_project_id == project_id)
            {
                self.send_project_value(
                    project_id,
                    json!({"type":"set_model", "provider":provider, "modelId":model_id}),
                );
            }
            return;
        }
        let Some(id) = self.selected_harness else {
            return;
        };
        self.start_harness(id, None);
        if let Some(index) = self.harnesses.iter().position(|harness| harness.id == id) {
            self.harnesses[index].model = Some(format!("{provider}/{model_id}"));
            self.send_value(
                index,
                json!({"type":"set_model", "provider":provider, "modelId":model_id}),
            );
        }
    }

    pub(crate) fn select_thinking(&mut self, level: String) {
        self.composer_dropdown = None;
        if self.creating_harness {
            self.draft_thinking_level = Some(level);
            return;
        }
        let Some(id) = self.selected_harness else {
            return;
        };
        self.start_harness(id, None);
        if let Some(index) = self.harnesses.iter().position(|harness| harness.id == id) {
            self.harnesses[index].thinking_level = Some(level.clone());
            self.send_value(index, json!({"type":"set_thinking_level", "level":level}));
        }
    }

    pub(crate) fn reasoning_options(&self) -> Vec<String> {
        let current_model = if self.creating_harness {
            self.draft_model.as_deref()
        } else {
            self.selected_harness.and_then(|id| {
                self.harnesses
                    .iter()
                    .find(|harness| harness.id == id)
                    .and_then(|harness| harness.model.as_deref())
            })
        };
        if let (Some(project_id), Some(current_model)) = (self.selected_project, current_model)
            && let Some(levels) = self
                .available_thinking_levels
                .get(&(project_id, current_model.to_string()))
        {
            return levels.clone();
        }
        let model = current_model.and_then(|current| {
            self.available_models
                .iter()
                .find(|model| current == format!("{}/{}", model.provider, model.id))
        });
        if model.is_some_and(|model| !model.reasoning) {
            return vec!["off".into()];
        }
        let mut levels = vec![
            "off".into(),
            "minimal".into(),
            "low".into(),
            "medium".into(),
            "high".into(),
        ];
        if model.is_some_and(|model| model.supports_xhigh) {
            levels.push("xhigh".into());
        }
        if model.is_some_and(|model| model.supports_max) {
            levels.push("max".into());
        }
        levels
    }

    fn fail_harness(&mut self, index: usize, error: String) {
        self.harnesses[index].status = HarnessStatus::Failed;
        self.harnesses[index].run_started_at = None;
        self.harnesses[index].attention_required = false;
        self.harnesses[index].error = Some(error.clone());
        self.harnesses[index].messages.push(Message::error(error));
        self.refresh_harness_order(index);
        self.persist();
        self.sync_conversation_list(index, None);
    }

    fn handle_runtime_error(&mut self, target: RuntimeTarget, message: String) {
        match target {
            RuntimeTarget::Harness(harness_id, generation) => {
                if let Some(index) = self.harnesses.iter().position(|harness| {
                    harness.id == harness_id && harness.process_generation == generation
                }) {
                    self.harnesses[index].error = Some(message.clone());
                    self.harnesses[index].messages.push(Message::error(message));
                    if self.harnesses[index].status == HarnessStatus::Starting {
                        self.harnesses[index].status = HarnessStatus::Failed;
                        self.harnesses[index].run_started_at = None;
                        self.refresh_harness_order(index);
                        self.persist();
                    }
                    self.sync_conversation_list(index, None);
                }
            }
            RuntimeTarget::Project(project_id)
                if self
                    .project_probe
                    .as_ref()
                    .is_some_and(|(active_project_id, _)| *active_project_id == project_id) =>
            {
                self.banner = Some(message);
            }
            RuntimeTarget::Project(_) => {}
        }
    }

    fn handle_runtime_exit(&mut self, target: RuntimeTarget) {
        let RuntimeTarget::Harness(harness_id, generation) = target else {
            return;
        };
        let Some(index) = self.harnesses.iter().position(|harness| {
            harness.id == harness_id && harness.process_generation == generation
        }) else {
            return;
        };
        if self
            .pending_dialog
            .as_ref()
            .is_some_and(|dialog| dialog.harness_id == harness_id)
        {
            self.pending_dialog = None;
        }
        self.harnesses[index].process.take();
        let transitioned = self.harnesses[index].status != HarnessStatus::Stopped
            || self.harnesses[index].run_started_at.is_some()
            || self.harnesses[index].attention_required;
        if self.harnesses[index].status != HarnessStatus::Failed {
            self.harnesses[index].status = HarnessStatus::Stopped;
        }
        self.harnesses[index].run_started_at = None;
        self.harnesses[index].attention_required = false;
        if transitioned {
            self.refresh_harness_order(index);
            self.persist();
        }
    }

    fn handle_runtime_event(&mut self, event: RuntimeEvent, cx: &mut Context<Self>) {
        let (target, value) = match event {
            RuntimeEvent::Json { target, value } => (target, value),
            RuntimeEvent::Error { target, message } => {
                self.handle_runtime_error(target, message);
                return;
            }
            RuntimeEvent::Exited { target } => {
                self.handle_runtime_exit(target);
                return;
            }
        };
        let (harness_id, generation) = match target {
            RuntimeTarget::Harness(harness_id, generation) => (harness_id, generation),
            RuntimeTarget::Project(project_id) => {
                if value.get("type").and_then(Value::as_str) == Some("response") {
                    self.handle_project_response(project_id, &value);
                }
                return;
            }
        };
        let Some(index) = self.harnesses.iter().position(|harness| {
            harness.id == harness_id && harness.process_generation == generation
        }) else {
            return;
        };
        let event_type = value
            .get("type")
            .and_then(Value::as_str)
            .unwrap_or_default();
        let replaces_messages = event_type == "response"
            && value.get("success").and_then(Value::as_bool) != Some(false)
            && matches!(
                value.get("command").and_then(Value::as_str),
                Some("get_entries" | "get_messages")
            )
            && !self.harnesses[index].loaded_messages;
        let changed_message = match event_type {
            "agent_start" => {
                self.harnesses[index].status = HarnessStatus::Working;
                if self.harnesses[index].run_started_at.is_none() {
                    self.harnesses[index].run_started_at = Some(Instant::now());
                    self.harnesses[index].last_run_duration = None;
                    self.refresh_harness_order(index);
                    self.persist();
                }
                None
            }
            "agent_settled" => {
                let changed_message = self.settle_harness(index);
                self.request_context_usage(index);
                self.request_entries(index);
                changed_message
            }
            "compaction_start" => {
                self.handle_compaction_start(index, &value);
                None
            }
            "compaction_end" => {
                let changed_message = self.handle_compaction_end(index, &value);
                self.request_context_usage(index);
                changed_message
            }
            "message_end" => {
                self.request_context_usage(index);
                None
            }
            "message_update" => self.handle_message_update(index, &value),
            "tool_execution_start" => {
                self.handle_tool_start(index, &value);
                None
            }
            "tool_execution_update" => self.handle_tool_update(index, &value),
            "tool_execution_end" => self.handle_tool_end(index, &value),
            "response" => {
                self.handle_response(index, &value);
                None
            }
            "extension_error" => {
                let error = value
                    .get("error")
                    .and_then(Value::as_str)
                    .unwrap_or("A pi extension failed.");
                self.harnesses[index].messages.push(Message::error(error));
                None
            }
            "extension_ui_request" => {
                self.handle_extension_ui(index, &value, cx);
                None
            }
            _ => None,
        };
        if replaces_messages && self.selected_harness == Some(self.harnesses[index].id) {
            self.reset_conversation_list(index);
        } else {
            self.sync_conversation_list(index, changed_message);
        }
    }

    fn handle_compaction_start(&mut self, index: usize, value: &Value) {
        let reason = value.get("reason").and_then(Value::as_str);
        self.harnesses[index]
            .messages
            .push(Message::compaction(reason, None, true));
    }

    fn handle_compaction_end(&mut self, index: usize, value: &Value) -> Option<usize> {
        let reason = value.get("reason").and_then(Value::as_str);
        let summary = value
            .pointer("/result/summary")
            .and_then(Value::as_str)
            .map(truncate_output);
        let message_index = self.harnesses[index]
            .messages
            .iter()
            .rposition(|message| message.is_compaction() && message.running)
            .unwrap_or_else(|| {
                self.harnesses[index]
                    .messages
                    .push(Message::compaction(reason, None, true));
                self.harnesses[index].messages.len() - 1
            });
        let message = &mut self.harnesses[index].messages[message_index];
        message.set_running(false);
        if let Some(summary) = summary {
            message.set_detail(Some(summary));
        } else if value.get("aborted").and_then(Value::as_bool) == Some(true) {
            message.append_text(" · aborted");
        } else {
            message.append_text(" · failed");
            if let Some(error) = value.get("errorMessage").and_then(Value::as_str) {
                message.set_detail(Some(truncate_output(error)));
            }
        }
        Some(message_index)
    }

    fn handle_message_update(&mut self, index: usize, value: &Value) -> Option<usize> {
        let update = value.get("assistantMessageEvent").unwrap_or(&Value::Null);
        match update.get("type").and_then(Value::as_str) {
            Some("text_delta") | Some("thinking_delta") => {
                let role = if update.get("type").and_then(Value::as_str) == Some("thinking_delta") {
                    MessageRole::Thinking
                } else {
                    MessageRole::Assistant
                };
                let delta = update
                    .get("delta")
                    .and_then(Value::as_str)
                    .unwrap_or_default();
                if let Some(message) = self.harnesses[index]
                    .messages
                    .last_mut()
                    .filter(|message| message.role == role && message.running)
                {
                    message.append_text(delta);
                } else {
                    let mut message = Message::new(role, delta);
                    message.set_running(true);
                    self.harnesses[index].messages.push(message);
                }
                return self.harnesses[index].messages.len().checked_sub(1);
            }
            Some("text_end") | Some("thinking_end") => {
                let role = if update.get("type").and_then(Value::as_str) == Some("thinking_end") {
                    MessageRole::Thinking
                } else {
                    MessageRole::Assistant
                };
                if let Some(message_index) = self.harnesses[index]
                    .messages
                    .iter()
                    .rposition(|message| message.role == role && message.running)
                {
                    self.harnesses[index].messages[message_index].set_running(false);
                    return Some(message_index);
                }
            }
            Some("done") | Some("error") => {
                let mut changed_message = None;
                for (message_index, message) in self.harnesses[index]
                    .messages
                    .iter_mut()
                    .enumerate()
                    .filter(|(_, message)| {
                        matches!(message.role, MessageRole::Assistant | MessageRole::Thinking)
                            && message.running
                    })
                {
                    message.set_running(false);
                    changed_message = Some(message_index);
                }
                return changed_message;
            }
            _ => {}
        }
        None
    }

    fn settle_harness(&mut self, index: usize) -> Option<usize> {
        self.harnesses[index].status = HarnessStatus::Idle;
        let run_duration = self.harnesses[index]
            .run_started_at
            .take()
            .map(|started_at| started_at.elapsed());
        self.harnesses[index].last_run_duration = run_duration;
        self.harnesses[index].attention_required = false;
        if self.selected_harness != Some(self.harnesses[index].id) {
            self.harnesses[index].has_unread_completion = true;
        }
        self.refresh_harness_order(index);
        self.persist();
        let mut changed_message = None;
        for (message_index, message) in self.harnesses[index].messages.iter_mut().enumerate() {
            if message.running {
                if message.role == MessageRole::Assistant {
                    changed_message = Some(message_index);
                }
                message.set_running(false);
            }
        }
        if self.harnesses[index].nix_restart_pending {
            self.harnesses[index].nix_restart_pending = false;
            let id = self.harnesses[index].id;
            self.restart_harness(id);
        }
        changed_message
    }

    pub(crate) fn toggle_nix(&mut self) {
        if self.creating_harness {
            let Some(project_id) = self.selected_project else {
                return;
            };
            if self.project_has_devshell(project_id) {
                self.draft_nix_enabled = !self.draft_nix_enabled;
                if self
                    .project_probe
                    .as_ref()
                    .is_some_and(|(active_project_id, _)| *active_project_id == project_id)
                {
                    self.project_probe.take();
                    self.start_project_probe(project_id);
                }
            }
            return;
        }

        let Some(id) = self.selected_harness else {
            return;
        };
        let Some(index) = self.harnesses.iter().position(|harness| harness.id == id) else {
            return;
        };
        if !self.project_has_devshell(self.harnesses[index].project_id) {
            return;
        }
        self.harnesses[index].nix_enabled = !self.harnesses[index].nix_enabled;
        self.persist();
        if self.harnesses[index].status == HarnessStatus::Working {
            self.harnesses[index].nix_restart_pending = true;
        } else {
            self.restart_harness(id);
        }
    }

    fn restart_harness(&mut self, id: Id) {
        let Some(index) = self.harnesses.iter().position(|harness| harness.id == id) else {
            return;
        };
        if let Some(process) = self.harnesses[index].process.take() {
            process.stop();
        }
        if self.harnesses[index].run_started_at.take().is_some() {
            self.harnesses[index].attention_required = false;
            self.refresh_harness_order(index);
            self.persist();
        }
        self.start_harness(id, None);
    }

    pub(crate) fn project_has_devshell(&self, project_id: Id) -> bool {
        self.projects
            .iter()
            .find(|project| project.id == project_id)
            .is_some_and(|project| project_has_devshell(&project.path))
    }

    fn handle_tool_start(&mut self, index: usize, value: &Value) {
        let name = value
            .get("toolName")
            .and_then(Value::as_str)
            .unwrap_or("tool");
        let args = value.get("args").unwrap_or(&Value::Null);
        self.harnesses[index].messages.push(tool_message(
            name,
            args,
            value
                .get("toolCallId")
                .and_then(Value::as_str)
                .map(str::to_string),
            true,
        ));
    }

    fn handle_tool_update(&mut self, index: usize, value: &Value) -> Option<usize> {
        let id = value.get("toolCallId").and_then(Value::as_str);
        let detail = value
            .pointer("/partialResult/content/0/text")
            .and_then(Value::as_str)
            .map(truncate_output)?;
        let message_index = self.harnesses[index]
            .messages
            .iter()
            .rposition(|message| message.tool_call_id.as_deref() == id)?;
        self.harnesses[index].messages[message_index].set_detail(Some(detail));
        Some(message_index)
    }

    fn handle_tool_end(&mut self, index: usize, value: &Value) -> Option<usize> {
        let id = value.get("toolCallId").and_then(Value::as_str);
        let name = value
            .get("toolName")
            .and_then(Value::as_str)
            .unwrap_or("tool");
        let is_error = value.get("isError").and_then(Value::as_bool) == Some(true);
        let detail = value
            .get("result")
            .and_then(|result| tool_result_detail(name, result, is_error));
        let message_index = self.harnesses[index]
            .messages
            .iter()
            .rposition(|message| message.tool_call_id.as_deref() == id)?;
        let message = &mut self.harnesses[index].messages[message_index];
        message.set_running(false);
        if detail.is_some() && (name != "write" || is_error || message.detail.is_none()) {
            message.set_detail(detail);
        }
        if is_error {
            message.append_text(" · failed");
        }
        Some(message_index)
    }

    fn handle_project_response(&mut self, project_id: Id, value: &Value) {
        if !self
            .project_probe
            .as_ref()
            .is_some_and(|(active_project_id, _)| *active_project_id == project_id)
        {
            return;
        }
        if value.get("success").and_then(Value::as_bool) == Some(false) {
            self.banner = Some(
                value
                    .get("error")
                    .and_then(Value::as_str)
                    .unwrap_or("Pi could not load project settings.")
                    .to_string(),
            );
            return;
        }
        match value.get("command").and_then(Value::as_str) {
            Some("get_state") => {
                let data = value.get("data").unwrap_or(&Value::Null);
                let thinking_level = data
                    .get("thinkingLevel")
                    .and_then(Value::as_str)
                    .map(str::to_string);
                let model = data.get("model").and_then(|model| {
                    let provider = model.get("provider")?.as_str()?;
                    let id = model.get("id")?.as_str()?;
                    Some(format!("{provider}/{id}"))
                });
                if self.creating_harness && self.selected_project == Some(project_id) {
                    if self.draft_model.is_none() {
                        self.draft_model = model;
                    }
                    if self.draft_thinking_level.is_none() {
                        self.draft_thinking_level = thinking_level;
                    }
                }
                if self.draft_model.is_some() {
                    self.send_project_value(
                        project_id,
                        json!({"type":"get_available_thinking_levels"}),
                    );
                }
            }
            Some("get_available_models") => {
                let models = value
                    .pointer("/data/models")
                    .and_then(Value::as_array)
                    .into_iter()
                    .flatten()
                    .filter_map(parse_available_model)
                    .collect::<Vec<_>>();
                self.available_models_by_project
                    .insert(project_id, models.clone());
                if self.selected_project == Some(project_id) {
                    self.available_models = models;
                }
                self.cache_project_models(project_id);
            }
            Some("get_available_thinking_levels") => {
                let Some(model) = self.draft_model.clone() else {
                    return;
                };
                let levels = value
                    .pointer("/data/levels")
                    .and_then(Value::as_array)
                    .into_iter()
                    .flatten()
                    .filter_map(Value::as_str)
                    .map(str::to_string)
                    .collect::<Vec<_>>();
                self.available_thinking_levels
                    .insert((project_id, model.clone()), levels);
                self.cache_model_thinking_levels(project_id, &model);
            }
            Some("set_model") => {
                self.send_project_value(
                    project_id,
                    json!({"id":"dirigent-project-state","type":"get_state"}),
                );
            }
            _ => {}
        }
    }

    fn handle_response(&mut self, index: usize, value: &Value) {
        if value.get("success").and_then(Value::as_bool) == Some(false) {
            if value.get("command").and_then(Value::as_str) == Some("get_entries")
                && value.get("id").and_then(Value::as_str) == Some("dirigent-entries-incremental")
            {
                self.send_value(
                    index,
                    json!({"id":"dirigent-entries-full","type":"get_entries"}),
                );
                return;
            }
            let command = value.get("command").and_then(Value::as_str);
            if matches!(command, Some("set_model" | "set_thinking_level")) {
                self.send_value(index, json!({"id":"dirigent-state","type":"get_state"}));
            }
            let error = value
                .get("error")
                .and_then(Value::as_str)
                .unwrap_or("Pi rejected a command.");
            self.harnesses[index].messages.push(Message::error(error));
            return;
        }
        match value.get("command").and_then(Value::as_str) {
            Some("get_state") => {
                let data = value.get("data").unwrap_or(&Value::Null);
                self.harnesses[index].session_file = data
                    .get("sessionFile")
                    .and_then(Value::as_str)
                    .map(PathBuf::from);
                let thinking_level = data
                    .get("thinkingLevel")
                    .and_then(Value::as_str)
                    .map(str::to_string);
                let model = data.get("model").and_then(|model| {
                    let provider = model.get("provider")?.as_str()?;
                    let id = model.get("id")?.as_str()?;
                    Some(format!("{provider}/{id}"))
                });
                self.harnesses[index].thinking_level = thinking_level.clone();
                self.harnesses[index].model = model.clone();
                if self.creating_harness
                    && self.selected_project == Some(self.harnesses[index].project_id)
                {
                    if self.draft_thinking_level.is_none() {
                        self.draft_thinking_level = thinking_level;
                    }
                    if self.draft_model.is_none() {
                        self.draft_model = model;
                    }
                }
                let reported_status = if data
                    .get("isStreaming")
                    .and_then(Value::as_bool)
                    .unwrap_or(false)
                {
                    HarnessStatus::Working
                } else {
                    HarnessStatus::Idle
                };
                if self.harnesses[index].status != HarnessStatus::Working
                    || reported_status == HarnessStatus::Working
                {
                    self.harnesses[index].status = reported_status;
                }
                if reported_status == HarnessStatus::Working
                    && self.harnesses[index].run_started_at.is_none()
                {
                    self.harnesses[index].run_started_at = Some(Instant::now());
                    self.harnesses[index].last_run_duration = None;
                    self.refresh_harness_order(index);
                }
                self.persist();
                self.cache_harness_state(index);
                self.cache_harness_draft(index, true);
                self.request_thinking_levels(index);
            }
            Some("get_entries") => {
                let incoming = value
                    .pointer("/data/entries")
                    .and_then(Value::as_array)
                    .cloned()
                    .unwrap_or_default();
                let incremental =
                    value.get("id").and_then(Value::as_str) == Some("dirigent-entries-incremental");
                if incremental {
                    let entries = self.harnesses[index]
                        .cached_entries
                        .get_or_insert_with(Vec::new);
                    let mut known_ids = entries
                        .iter()
                        .filter_map(|entry| entry.get("id")?.as_str().map(str::to_string))
                        .collect::<HashSet<_>>();
                    entries.extend(incoming.into_iter().filter(|entry| {
                        entry
                            .get("id")
                            .and_then(Value::as_str)
                            .is_none_or(|id| known_ids.insert(id.to_string()))
                    }));
                } else {
                    self.harnesses[index].cached_entries = Some(incoming);
                }
                self.harnesses[index].cached_leaf_id = value
                    .pointer("/data/leafId")
                    .and_then(Value::as_str)
                    .map(str::to_string);
                self.harnesses[index].messages = parse_entries(
                    self.harnesses[index]
                        .cached_entries
                        .as_deref()
                        .unwrap_or_default(),
                    self.harnesses[index].cached_leaf_id.as_deref(),
                );
                self.harnesses[index].loaded_messages = true;
                self.cache_harness_entries(index);
            }
            Some("get_messages") if !self.harnesses[index].loaded_messages => {
                if let Some(messages) = value.pointer("/data/messages").and_then(Value::as_array) {
                    self.harnesses[index].messages = parse_messages(messages);
                }
                self.harnesses[index].loaded_messages = true;
            }
            Some("get_available_models") => {
                let models = value
                    .pointer("/data/models")
                    .and_then(Value::as_array)
                    .into_iter()
                    .flatten()
                    .filter_map(parse_available_model)
                    .collect::<Vec<_>>();
                let project_id = self.harnesses[index].project_id;
                self.available_models_by_project
                    .insert(project_id, models.clone());
                if self.selected_project == Some(project_id) {
                    self.available_models = models;
                }
                self.cache_project_models(project_id);
            }
            Some("get_available_thinking_levels") => {
                let Some(model) = self.harnesses[index].model.clone() else {
                    return;
                };
                let levels = value
                    .pointer("/data/levels")
                    .and_then(Value::as_array)
                    .into_iter()
                    .flatten()
                    .filter_map(Value::as_str)
                    .map(str::to_string)
                    .collect::<Vec<_>>();
                let project_id = self.harnesses[index].project_id;
                self.available_thinking_levels
                    .insert((project_id, model.clone()), levels);
                self.cache_model_thinking_levels(project_id, &model);
            }
            Some("get_session_stats") => {
                self.harnesses[index].context_usage = parse_context_usage(value);
            }
            Some("set_model") => {
                let model = value.get("data").unwrap_or(&Value::Null);
                if let (Some(provider), Some(id)) = (
                    model.get("provider").and_then(Value::as_str),
                    model.get("id").and_then(Value::as_str),
                ) {
                    self.harnesses[index].model = Some(format!("{provider}/{id}"));
                }
                self.cache_harness_state(index);
                self.request_context_usage(index);
                self.request_thinking_levels(index);
            }
            Some("set_thinking_level") => {
                self.cache_harness_state(index);
            }
            Some("cycle_model") => {
                if let Some(model) = value.pointer("/data/model") {
                    let provider = model.get("provider").and_then(Value::as_str);
                    let id = model.get("id").and_then(Value::as_str);
                    if let (Some(provider), Some(id)) = (provider, id) {
                        self.harnesses[index].model = Some(format!("{provider}/{id}"));
                    }
                }
                self.cache_harness_state(index);
                self.request_thinking_levels(index);
            }
            Some("cycle_thinking_level") => {
                self.harnesses[index].thinking_level = value
                    .pointer("/data/level")
                    .and_then(Value::as_str)
                    .map(str::to_string);
                self.cache_harness_state(index);
            }
            _ => {}
        }
    }

    fn clear_harness_attention(&mut self, index: usize) {
        if !self.harnesses[index].attention_required {
            return;
        }
        self.harnesses[index].attention_required = false;
        self.refresh_harness_order(index);
        self.persist();
    }

    pub(crate) fn respond_extension_value(&mut self, value: String) {
        let Some(dialog) = self.pending_dialog.take() else {
            return;
        };
        let Some(index) = self
            .harnesses
            .iter()
            .position(|harness| harness.id == dialog.harness_id)
        else {
            return;
        };
        self.send_value(
            index,
            json!({
                "type":"extension_ui_response",
                "id":dialog.request_id,
                "value":value
            }),
        );
        self.clear_harness_attention(index);
    }

    pub(crate) fn respond_extension_confirmation(&mut self, confirmed: bool) {
        let Some(dialog) = self.pending_dialog.take() else {
            return;
        };
        let Some(index) = self
            .harnesses
            .iter()
            .position(|harness| harness.id == dialog.harness_id)
        else {
            return;
        };
        self.send_value(
            index,
            json!({
                "type":"extension_ui_response",
                "id":dialog.request_id,
                "confirmed":confirmed
            }),
        );
        self.clear_harness_attention(index);
    }

    pub(crate) fn cancel_extension_dialog(&mut self) {
        let Some(dialog) = self.pending_dialog.take() else {
            return;
        };
        let Some(index) = self
            .harnesses
            .iter()
            .position(|harness| harness.id == dialog.harness_id)
        else {
            return;
        };
        self.send_value(
            index,
            json!({
                "type":"extension_ui_response",
                "id":dialog.request_id,
                "cancelled":true
            }),
        );
        self.clear_harness_attention(index);
    }

    pub(crate) fn submit_extension_dialog(&mut self, cx: &mut Context<Self>) {
        let Some(kind) = self.pending_dialog.as_ref().map(|dialog| dialog.kind) else {
            return;
        };
        if matches!(kind, DialogKind::Input | DialogKind::Editor) {
            let value = self.extension_input.read(cx).text().to_string();
            self.respond_extension_value(value);
            self.extension_input.update(cx, |input, cx| input.clear(cx));
            cx.notify();
        }
    }

    fn handle_extension_ui(&mut self, index: usize, value: &Value, cx: &mut Context<Self>) {
        let method = value
            .get("method")
            .and_then(Value::as_str)
            .unwrap_or_default();
        match method {
            "notify" => {
                if let Some(message) = value.get("message").and_then(Value::as_str) {
                    self.harnesses[index]
                        .messages
                        .push(Message::notice(message));
                    self.harnesses[index].has_unread_completion = true;
                    self.refresh_harness_order(index);
                    self.persist();
                }
            }
            "setStatus" => {
                if let Some(message) = value.get("statusText").and_then(Value::as_str) {
                    self.harnesses[index]
                        .messages
                        .push(Message::notice(message));
                }
            }
            "setTitle" => {
                if let Some(title) = value.get("title").and_then(Value::as_str) {
                    self.harnesses[index]
                        .messages
                        .push(Message::notice(format!("Pi title: {title}")));
                }
            }
            "set_editor_text" => {
                if let Some(text) = value.get("text").and_then(Value::as_str) {
                    self.harnesses[index].has_unread_completion = false;
                    self.harnesses[index].attention_required = true;
                    self.refresh_harness_order(index);
                    self.selected_project = Some(self.harnesses[index].project_id);
                    self.show_cached_models(self.harnesses[index].project_id);
                    self.selected_harness = Some(self.harnesses[index].id);
                    self.last_used_harness = Some(self.harnesses[index].id);
                    self.adding_project = false;
                    self.creating_harness = false;
                    self.persist();
                    if let Some(input) = self.composer_inputs.get(&self.harnesses[index].id) {
                        input.update(cx, |input, cx| input.set_text(text, cx));
                    }
                    self.persist_composer_draft(self.harnesses[index].id, cx);
                }
            }
            "setWidget" => {
                if let Some(lines) = value.get("widgetLines").and_then(Value::as_array) {
                    let text = lines
                        .iter()
                        .filter_map(Value::as_str)
                        .collect::<Vec<_>>()
                        .join("\n");
                    if !text.is_empty() {
                        self.harnesses[index].messages.push(Message::notice(text));
                    }
                }
            }
            "select" | "confirm" | "input" | "editor" => {
                let kind = match method {
                    "select" => DialogKind::Select,
                    "confirm" => DialogKind::Confirm,
                    "input" => DialogKind::Input,
                    _ => DialogKind::Editor,
                };
                if self.pending_dialog.is_some() {
                    let request_id = value.get("id").and_then(Value::as_str).unwrap_or_default();
                    self.send_value(
                        index,
                        json!({
                            "type":"extension_ui_response",
                            "id":request_id,
                            "cancelled":true
                        }),
                    );
                    self.harnesses[index].messages.push(Message::notice(
                        "An extension dialog was cancelled because another harness already needs input.",
                    ));
                    return;
                }
                self.harnesses[index].has_unread_completion = false;
                self.harnesses[index].attention_required = true;
                self.refresh_harness_order(index);
                self.selected_project = Some(self.harnesses[index].project_id);
                self.show_cached_models(self.harnesses[index].project_id);
                self.selected_harness = Some(self.harnesses[index].id);
                self.last_used_harness = Some(self.harnesses[index].id);
                self.adding_project = false;
                self.creating_harness = false;
                self.persist();
                let prefill = value
                    .get("prefill")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_string();
                self.extension_input
                    .update(cx, |input, cx| input.set_text(prefill, cx));
                self.pending_dialog = Some(PendingDialog {
                    harness_id: self.harnesses[index].id,
                    request_id: value
                        .get("id")
                        .and_then(Value::as_str)
                        .unwrap_or_default()
                        .to_string(),
                    kind,
                    title: value
                        .get("title")
                        .and_then(Value::as_str)
                        .unwrap_or("Pi needs input")
                        .to_string(),
                    message: value
                        .get("message")
                        .and_then(Value::as_str)
                        .map(str::to_string),
                    options: value
                        .get("options")
                        .and_then(Value::as_array)
                        .into_iter()
                        .flatten()
                        .filter_map(Value::as_str)
                        .map(str::to_string)
                        .collect(),
                });
            }
            _ => {}
        }
    }
}

impl Dirigent {
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

    fn copy_thread_selection(&self, cx: &mut Context<Self>) -> bool {
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

    fn perform_keyboard_menu_key(&mut self, key: &str, shift: bool, cx: &mut Context<Self>) {
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
            (KeyboardMenu::Space, "r") => self.restart_selected(),
            (KeyboardMenu::Space, "n") => self.toggle_nix(),
            (KeyboardMenu::Space, "m") => self.toggle_composer_dropdown(ComposerDropdown::Model),
            (KeyboardMenu::Space, "e") => {
                self.toggle_composer_dropdown(ComposerDropdown::Reasoning)
            }
            (KeyboardMenu::Space, "b") => self.banner = None,
            (KeyboardMenu::Space, "d") if shift => {
                self.debug_panels_visible = !self.debug_panels_visible;
            }
            (KeyboardMenu::Space, "y") => {
                self.copy_thread_selection(cx);
            }

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
            (KeyboardMenu::Threads, "r") => self.restart_selected(),
            (KeyboardMenu::Threads, "i") => self.enter_input_mode(true),

            (KeyboardMenu::Projects, "a") => self.begin_adding_project(),
            (KeyboardMenu::Projects, "c") => self.start_new_selected_project(),
            (KeyboardMenu::Projects, "j" | "n") => self.select_relative_project(1),
            (KeyboardMenu::Projects, "k" | "p") => self.select_relative_project(-1),
            (KeyboardMenu::Projects, "g") => self.select_edge_project(true),
            (KeyboardMenu::Projects, "e") => self.select_edge_project(false),
            (KeyboardMenu::Projects, "[") => self.resize_sidebar(self.sidebar_width - 24.0),
            (KeyboardMenu::Projects, "]") => self.resize_sidebar(self.sidebar_width + 24.0),

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

    fn on_root_key_up(&mut self, event: &KeyUpEvent, _: &mut Window, cx: &mut Context<Self>) {
        let activation_released = matches!(
            (self.keyboard_menu_activation, event.keystroke.key.as_str()),
            (Some(KeyboardMenu::Space), "space") | (Some(KeyboardMenu::Goto), "g")
        );
        if activation_released {
            self.keyboard_menu_activation = None;
            cx.stop_propagation();
        }
    }

    fn on_root_key_down(&mut self, event: &KeyDownEvent, _: &mut Window, cx: &mut Context<Self>) {
        let key = event.keystroke.key.as_str();
        let modifiers = event.keystroke.modifiers;
        let command = modifiers.control || modifiers.platform;

        if key == "escape" {
            self.preview_image = None;
            self.enter_normal_mode();
            cx.stop_propagation();
            cx.notify();
            return;
        }

        if self.keyboard_menu_activation.is_some() {
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
                    self.resize_sidebar(self.sidebar_width - 24.0);
                    true
                }
                "]" if !command && !modifiers.alt => {
                    self.resize_sidebar(self.sidebar_width + 24.0);
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

impl Render for Dirigent {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        self.frame_timing.collect_frames(Instant::now());
        let frame_timing_labels = self.frame_timing.labels();
        self.sync_path_completion_input(cx);
        let path_completion_anchor = self.path_completion.as_ref().and_then(|completion| {
            let at = completion.replacement.start.checked_sub(1)?;
            self.path_completion_input(completion.target)?
                .read(cx)
                .position_for_offset(at)
        });

        if self.focus_normal_mode {
            self.focus_normal_mode = false;
            window.focus(&self.thread_focus, cx);
        } else if self.focus_input {
            self.focus_input = false;
            let input = if self.renaming_harness.is_some() {
                Some(self.thread_rename_input.clone())
            } else if self.pending_dialog.is_some() {
                Some(self.extension_input.clone())
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
                if this.composer_dropdown.take().is_some() {
                    cx.notify();
                }
            }))
            .child(self.render_sidebar(window, cx))
            .child(self.render_center(window, cx))
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

fn parse_cached_draft_images(bytes: &[u8]) -> Result<Vec<AttachedImage>, String> {
    let images = serde_json::from_slice::<Vec<CachedDraftImage>>(bytes)
        .map_err(|error| format!("could not decode cached composer images: {error}"))?;
    images
        .into_iter()
        .map(|image| {
            let format = ImageFormat::from_mime_type(&image.mime_type).ok_or_else(|| {
                format!(
                    "cached composer image has unsupported type {}",
                    image.mime_type
                )
            })?;
            let bytes = BASE64
                .decode(image.data)
                .map_err(|error| format!("could not decode a cached composer image: {error}"))?;
            Ok(AttachedImage {
                label: image.label,
                image: Arc::new(Image::from_bytes(format, bytes)),
            })
        })
        .collect()
}

fn parse_available_model(value: &Value) -> Option<AvailableModel> {
    let thinking_map = value.get("thinkingLevelMap").and_then(Value::as_object);
    Some(AvailableModel {
        provider: value.get("provider")?.as_str()?.to_string(),
        id: value.get("id")?.as_str()?.to_string(),
        name: value
            .get("name")
            .and_then(Value::as_str)
            .unwrap_or_else(|| value.get("id").and_then(Value::as_str).unwrap_or("model"))
            .to_string(),
        reasoning: value
            .get("reasoning")
            .and_then(Value::as_bool)
            .unwrap_or(false),
        supports_xhigh: thinking_map.is_some_and(|map| map.contains_key("xhigh")),
        supports_max: thinking_map.is_some_and(|map| map.contains_key("max")),
    })
}

fn parse_context_usage(value: &Value) -> Option<ContextUsage> {
    let context_usage = value.pointer("/data/contextUsage")?;
    Some(ContextUsage {
        used_tokens: context_usage.get("tokens")?.as_u64()?,
        context_window: context_usage.get("contextWindow")?.as_u64()?,
    })
}

fn compact_json(value: &Value) -> String {
    truncate_output(&serde_json::to_string(value).unwrap_or_else(|_| "{}".into()))
}

fn one_line(value: &str) -> String {
    value.split_whitespace().collect::<Vec<_>>().join(" ")
}

fn tool_label(name: &str, args: &Value) -> String {
    let argument = match name {
        "bash" => args.get("command").and_then(Value::as_str),
        "read" | "write" | "edit" => args.get("path").and_then(Value::as_str),
        "find" | "grep" => args.get("pattern").and_then(Value::as_str),
        _ => None,
    };
    match argument {
        Some(argument) if name == "bash" => one_line(argument),
        Some(argument) => format!("{name} {}", one_line(argument)),
        _ if args.is_null() => name.to_string(),
        _ => format!("{name} {}", one_line(&compact_json(args))),
    }
}

fn tool_expanded(name: &str) -> bool {
    matches!(name, "edit" | "write")
}

fn write_detail(args: &Value) -> Option<String> {
    let content = args.get("content").and_then(Value::as_str)?;
    if content.is_empty() {
        return None;
    }
    let mut detail = String::with_capacity(content.len() + content.lines().count() * 2);
    for line in content.split_inclusive('\n') {
        detail.push_str("+ ");
        detail.push_str(line);
    }
    Some(truncate_output(&detail))
}

fn normalize_diff_spacing(diff: &str) -> String {
    let mut detail = String::with_capacity(diff.len() + diff.lines().count());
    for line in diff.split_inclusive('\n') {
        if matches!(line.as_bytes().first(), Some(b'+' | b'-' | b' ')) {
            detail.push_str(&line[..1]);
            detail.push(' ');
            detail.push_str(&line[1..]);
        } else {
            detail.push_str(line);
        }
    }
    detail
}

fn tool_message(name: &str, args: &Value, tool_call_id: Option<String>, running: bool) -> Message {
    let mut message = Message::tool(
        tool_label(name, args),
        tool_call_id,
        running,
        tool_expanded(name),
    );
    if name == "write" {
        message.set_detail(write_detail(args));
    }
    message
}

fn truncate_output(value: &str) -> String {
    const LIMIT: usize = 4_000;
    if value.len() <= LIMIT {
        value.to_string()
    } else {
        let mut end = LIMIT;
        while !value.is_char_boundary(end) {
            end -= 1;
        }
        format!("{}\n… output truncated by Dirigent", &value[..end])
    }
}

fn tool_result_detail(name: &str, result: &Value, is_error: bool) -> Option<String> {
    if name == "edit"
        && !is_error
        && let Some(diff) = result.pointer("/details/diff").and_then(Value::as_str)
    {
        return Some(truncate_output(&normalize_diff_spacing(diff)));
    }

    result
        .get("content")
        .map(content_text)
        .filter(|text| !text.is_empty())
        .map(|text| truncate_output(&text))
}

fn content_text(value: &Value) -> String {
    if let Some(text) = value.as_str() {
        return text.to_string();
    }
    value
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|block| {
            (block.get("type").and_then(Value::as_str) == Some("text"))
                .then(|| block.get("text").and_then(Value::as_str))
                .flatten()
        })
        .collect::<Vec<_>>()
        .join("\n")
}

fn content_images(value: &Value) -> Vec<Arc<Image>> {
    value
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|block| {
            if block.get("type").and_then(Value::as_str) != Some("image") {
                return None;
            }
            let data = block.get("data").and_then(Value::as_str)?;
            let format = ImageFormat::from_mime_type(
                block
                    .get("mimeType")
                    .and_then(Value::as_str)
                    .unwrap_or("image/png"),
            )?;
            let bytes = BASE64.decode(data).ok()?;
            (!bytes.is_empty()).then(|| Arc::new(Image::from_bytes(format, bytes)))
        })
        .collect()
}

fn push_assistant_block(messages: &mut Vec<Message>, role: MessageRole, text: &str) {
    if text.is_empty() {
        return;
    }
    if let Some(message) = messages.last_mut().filter(|message| message.role == role) {
        if !message.text.is_empty() {
            message.append_text("\n");
        }
        message.append_text(text);
        return;
    }
    messages.push(Message::new(role, text));
}

fn parse_entries(values: &[Value], leaf_id: Option<&str>) -> Vec<Message> {
    let entries_by_id = values
        .iter()
        .filter_map(|entry| Some((entry.get("id")?.as_str()?, entry)))
        .collect::<HashMap<_, _>>();
    let mut active_entries = Vec::new();
    let mut visited = HashSet::new();
    let mut current_id = leaf_id;
    while let Some(id) = current_id {
        if !visited.insert(id) {
            break;
        }
        let Some(entry) = entries_by_id.get(id).copied() else {
            break;
        };
        active_entries.push(entry);
        current_id = entry.get("parentId").and_then(Value::as_str);
    }
    active_entries.reverse();

    let mut messages = Vec::new();
    for entry in active_entries {
        match entry.get("type").and_then(Value::as_str) {
            Some("message") => {
                if let Some(message) = entry.get("message") {
                    push_parsed_message(&mut messages, message);
                }
            }
            Some("compaction") => {
                let summary = entry
                    .get("summary")
                    .and_then(Value::as_str)
                    .map(truncate_output);
                messages.push(Message::compaction(None, summary.as_deref(), false));
            }
            _ => {}
        }
    }
    messages
}

fn parse_messages(values: &[Value]) -> Vec<Message> {
    let mut messages = Vec::new();
    for value in values {
        push_parsed_message(&mut messages, value);
    }
    messages
}

fn push_parsed_message(messages: &mut Vec<Message>, value: &Value) {
    if value.get("role").and_then(Value::as_str) == Some("assistant") {
        if let Some(blocks) = value.get("content").and_then(Value::as_array) {
            for block in blocks {
                match block.get("type").and_then(Value::as_str) {
                    Some("text") => push_assistant_block(
                        messages,
                        MessageRole::Assistant,
                        block
                            .get("text")
                            .and_then(Value::as_str)
                            .unwrap_or_default(),
                    ),
                    Some("thinking") => push_assistant_block(
                        messages,
                        MessageRole::Thinking,
                        block
                            .get("thinking")
                            .and_then(Value::as_str)
                            .unwrap_or_default(),
                    ),
                    Some("toolCall") => {
                        let name = block.get("name").and_then(Value::as_str).unwrap_or("tool");
                        messages.push(tool_message(
                            name,
                            block.get("arguments").unwrap_or(&Value::Null),
                            block.get("id").and_then(Value::as_str).map(str::to_string),
                            false,
                        ));
                    }
                    _ => {}
                }
            }
        } else if let Some(message) = parse_message(value) {
            messages.push(message);
        }
    } else if value.get("role").and_then(Value::as_str) == Some("toolResult") {
        let tool_call_id = value.get("toolCallId").and_then(Value::as_str);
        if let Some(message) = messages
            .iter_mut()
            .rev()
            .find(|message| message.tool_call_id.as_deref() == tool_call_id)
        {
            let name = value
                .get("toolName")
                .and_then(Value::as_str)
                .unwrap_or("tool");
            let is_error = value.get("isError").and_then(Value::as_bool) == Some(true);
            let detail = tool_result_detail(name, value, is_error);
            if detail.is_some() && (name != "write" || is_error || message.detail.is_none()) {
                message.set_detail(detail);
            }
        } else if let Some(message) = parse_message(value) {
            messages.push(message);
        }
    } else if let Some(message) = parse_message(value) {
        messages.push(message);
    }
}

fn parse_message(value: &Value) -> Option<Message> {
    match value.get("role")?.as_str()? {
        "user" => {
            let content = value.get("content")?;
            Some(Message::user_with_images(
                content_text(content),
                content_images(content),
            ))
        }
        "assistant" => {
            let text = content_text(value.get("content")?);
            (!text.is_empty()).then(|| Message::new(MessageRole::Assistant, text))
        }
        "toolResult" => {
            let name = value
                .get("toolName")
                .and_then(Value::as_str)
                .unwrap_or("tool");
            let is_error = value.get("isError").and_then(Value::as_bool) == Some(true);
            let mut message = Message::tool(
                name,
                value
                    .get("toolCallId")
                    .and_then(Value::as_str)
                    .map(str::to_string),
                false,
                tool_expanded(name),
            );
            message.set_detail(tool_result_detail(name, value, is_error));
            Some(message)
        }
        "bashExecution" => {
            let mut message = Message::tool(
                value
                    .get("command")
                    .and_then(Value::as_str)
                    .unwrap_or("bash"),
                None,
                false,
                false,
            );
            message.set_detail(
                value
                    .get("output")
                    .and_then(Value::as_str)
                    .map(truncate_output),
            );
            Some(message)
        }
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::{
        FrameTiming, FrameTimingSample, composer_path_query, content_text,
        conversation_list_splice, directory_path_query, parse_available_model,
        parse_cached_draft_images, parse_context_usage, parse_entries, parse_message,
        parse_messages, resolve_tilde_path, tool_expanded, tool_label, tool_result_detail,
        truncate_output, write_detail,
    };
    use crate::model::MessageRole;
    use serde_json::json;
    use std::{collections::VecDeque, path::PathBuf, time::Duration};

    #[test]
    fn finds_the_active_composer_file_mention() {
        let text = "please inspect @src/ui/com";
        let (range, query) = composer_path_query(text, text.len()).unwrap();

        assert_eq!(&text[range], "src/ui/com");
        assert_eq!(query, "src/ui/com");
        assert!(composer_path_query("email@example.com", 17).is_none());
        assert!(composer_path_query("@src/app.rs then", 16).is_none());
    }

    #[test]
    fn searches_only_children_of_the_typed_directory() {
        let base = std::env::temp_dir().join(format!("dirigent-path-query-{}", std::process::id()));
        std::fs::create_dir_all(&base).unwrap();
        for name in ["alpha", "beta", "delta", "gamma", "izvir", "omega", "zeta"] {
            std::fs::create_dir_all(base.join(name)).unwrap();
        }
        std::fs::create_dir_all(base.join("alpha/nested")).unwrap();
        std::fs::write(base.join("not-a-directory"), "fixture").unwrap();
        let raw = base.join("pro");

        let (root, query) = directory_path_query(raw.to_str().unwrap()).unwrap();
        let children = super::directory_child_results(&base, "");

        assert_eq!(root, base);
        assert_eq!(query, "pro");
        assert_eq!(children.len(), 7);
        assert!(
            children
                .iter()
                .all(|path| { std::path::Path::new(path).parent() == Some(base.as_path()) })
        );
        assert_eq!(
            directory_path_query(&format!("{}/", base.display())),
            Some((base.clone(), String::new()))
        );
        assert!(directory_path_query(base.join("missing/pro").to_str().unwrap()).is_none());
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn resolves_tilde_against_the_home_directory() {
        assert_eq!(
            resolve_tilde_path("~/work/project", std::path::Path::new("/home/test")),
            Some(PathBuf::from("/home/test/work/project"))
        );
        assert_eq!(
            resolve_tilde_path(r"~\work\project", std::path::Path::new("/home/test")),
            Some(std::path::Path::new("/home/test").join(r"work\project"))
        );
        assert_eq!(
            resolve_tilde_path("~", std::path::Path::new("/home/test")),
            Some(PathBuf::from("/home/test"))
        );
        assert!(resolve_tilde_path("~other/project", std::path::Path::new("/home/test")).is_none());
    }

    #[test]
    fn fff_indexes_and_fuzzy_finds_project_files() {
        let base = std::env::temp_dir().join(format!("dirigent-fff-index-{}", std::process::id()));
        let source = base.join("src");
        std::fs::create_dir_all(source.join("ui")).unwrap();
        std::fs::write(source.join("project_setup.rs"), "// fixture").unwrap();
        std::fs::write(source.join("ui/composer.rs"), "// fixture").unwrap();
        let (events, _) = async_channel::unbounded();
        let picker =
            super::start_fuzzy_index(&base, false, super::FuzzyIndexReady::Project(0), events)
                .unwrap();
        assert!(picker.wait_for_scan(Duration::from_secs(5)));

        let initial_results = super::fuzzy_file_results(&picker, "");
        let results = super::fuzzy_file_results(&picker, "prjsetup");

        assert!(
            initial_results
                .iter()
                .any(|path| path == "src/project_setup.rs")
        );
        assert!(results.iter().any(|path| path == "src/project_setup.rs"));
        picker.cancel();
        let _ = std::fs::remove_dir_all(base);
    }

    #[test]
    fn summarizes_frame_performance_percentiles() {
        let samples = (1..=100)
            .map(|sample| FrameTimingSample {
                draw_ms: sample as f32,
                response_ms: Some(sample as f32 * 2.0),
                invalidations: 2,
            })
            .collect::<VecDeque<_>>();
        let summary = FrameTiming::summarize(&samples).unwrap();

        assert_eq!(summary.draw_average_ms, 50.5);
        assert_eq!(summary.draw_p99_ms, 99.0);
        assert_eq!(summary.draw_maximum_ms, 100.0);
        assert_eq!(summary.response_p99_ms, Some(198.0));
        assert_eq!(summary.invalidations_average, 2.0);
        assert_eq!(summary.sample_count, 100);
    }

    #[test]
    fn tolerates_frames_without_response_timing() {
        let samples = VecDeque::from([FrameTimingSample {
            draw_ms: 3.0,
            response_ms: None,
            invalidations: 1,
        }]);

        assert_eq!(
            FrameTiming::summarize(&samples).unwrap().response_p99_ms,
            None
        );
    }

    #[test]
    fn conversation_list_splice_removes_messages_missing_from_canonical_history() {
        let (old_range, new_item_count) = conversation_list_splice(30, false, 25, false);

        assert_eq!(old_range, 25..30);
        assert_eq!(new_item_count, 0);
    }

    #[test]
    fn restores_cached_composer_images() {
        let images = parse_cached_draft_images(
            br#"[{"label":"image-3","mime_type":"image/png","data":"AQID"}]"#,
        )
        .unwrap();

        assert_eq!(images.len(), 1);
        assert_eq!(images[0].label, "image-3");
        assert_eq!(images[0].image.bytes.as_slice(), &[1, 2, 3]);
    }

    #[test]
    fn extracts_text_blocks_from_pi_messages() {
        let value = json!([
            {"type":"thinking","thinking":"hidden"},
            {"type":"text","text":"hello"},
            {"type":"text","text":"world"}
        ]);
        assert_eq!(content_text(&value), "hello\nworld");
    }

    #[test]
    fn restores_user_messages() {
        let message = parse_message(&json!({"role":"user","content":"do it"})).unwrap();
        assert_eq!(message.role, MessageRole::User);
        assert_eq!(message.text, "do it");
    }

    #[test]
    fn restores_images_on_user_messages() {
        let message = parse_message(&json!({
            "role":"user",
            "content":[
                {"type":"text","text":"[image-1] inspect this"},
                {"type":"image","data":"AA==","mimeType":"image/png"}
            ]
        }))
        .unwrap();
        assert_eq!(message.text, "[image-1] inspect this");
        assert_eq!(message.images.len(), 1);
    }

    #[test]
    fn tool_output_is_bounded() {
        assert!(truncate_output(&"x".repeat(5_000)).len() < 5_000);
    }

    #[test]
    fn tool_labels_are_single_line_commands() {
        assert_eq!(
            tool_label("bash", &json!({"command":"cargo fmt\ncargo test"})),
            "cargo fmt cargo test"
        );
        assert_eq!(
            tool_label("read", &json!({"path":"src/app.rs"})),
            "read src/app.rs"
        );
    }

    #[test]
    fn formats_write_content_as_added_lines() {
        assert_eq!(
            write_detail(&json!({"content":"fn main() {\n    run();\n}\n"})).as_deref(),
            Some("+ fn main() {\n+     run();\n+ }\n")
        );
        assert!(tool_expanded("write"));
    }

    #[test]
    fn parses_context_usage() {
        let usage = parse_context_usage(&json!({
            "data": {
                "contextUsage": {
                    "tokens": 60_000,
                    "contextWindow": 200_000,
                    "percent": 30
                }
            }
        }))
        .unwrap();

        assert_eq!(usage.used_tokens, 60_000);
        assert_eq!(usage.context_window, 200_000);
    }

    #[test]
    fn parses_available_model_picker_options() {
        let model = parse_available_model(&json!({
            "provider":"openai-codex",
            "id":"gpt-5.6-sol",
            "name":"GPT-5.6 Sol",
            "reasoning":true,
            "thinkingLevelMap":{"xhigh":"xhigh","max":"max"}
        }))
        .unwrap();
        assert_eq!(model.provider, "openai-codex");
        assert_eq!(model.id, "gpt-5.6-sol");
        assert!(model.supports_xhigh);
        assert!(model.supports_max);
    }

    #[test]
    fn restores_thinking_blocks_in_content_order() {
        let messages = parse_messages(&[
            json!({
                "role":"assistant",
                "content":[
                    {"type":"thinking","thinking":"I should inspect the file."},
                    {"type":"toolCall","id":"call-1","name":"read","arguments":{"path":"src/app.rs"}},
                    {"type":"text","text":"Done."}
                ]
            }),
            json!({"role":"toolResult","toolCallId":"call-1","toolName":"read","content":"file"}),
        ]);

        assert_eq!(messages.len(), 3);
        assert_eq!(messages[0].role, MessageRole::Thinking);
        assert_eq!(messages[0].text, "I should inspect the file.");
        assert_eq!(messages[1].role, MessageRole::Tool);
        assert_eq!(messages[2].role, MessageRole::Assistant);
    }

    #[test]
    fn restores_tool_commands_with_collapsed_output() {
        let messages = parse_messages(&[
            json!({
                "role":"assistant",
                "content":[{"type":"toolCall","id":"call-1","name":"bash","arguments":{"command":"cargo fmt\ncargo test"}}]
            }),
            json!({"role":"toolResult","toolCallId":"call-1","toolName":"bash","content":"ok"}),
        ]);
        assert_eq!(messages.len(), 1);
        assert_eq!(messages[0].text, "cargo fmt cargo test");
        assert_eq!(messages[0].detail.as_deref(), Some("ok"));
        assert!(!messages[0].expanded);
    }

    #[test]
    fn restores_write_content_instead_of_success_message() {
        let messages = parse_messages(&[
            json!({
                "role":"assistant",
                "content":[{"type":"toolCall","id":"call-1","name":"write","arguments":{
                    "path":"src/main.rs",
                    "content":"fn main() {}\n"
                }}]
            }),
            json!({
                "role":"toolResult",
                "toolCallId":"call-1",
                "toolName":"write",
                "content":"Successfully wrote 13 bytes to src/main.rs."
            }),
        ]);

        assert_eq!(messages.len(), 1);
        assert_eq!(messages[0].text, "write src/main.rs");
        assert_eq!(messages[0].detail.as_deref(), Some("+ fn main() {}\n"));
        assert!(messages[0].expanded);
    }

    #[test]
    fn restores_edit_diff_instead_of_success_message() {
        let messages = parse_messages(&[
            json!({
                "role":"assistant",
                "content":[{"type":"toolCall","id":"call-1","name":"edit","arguments":{"path":"src/app.rs","edits":[]}}]
            }),
            json!({
                "role":"toolResult",
                "toolCallId":"call-1",
                "toolName":"edit",
                "content":"Successfully replaced 1 block(s) in src/app.rs.",
                "details":{"diff":" 9 before\n-10 old\n+10 new\n 11 after"}
            }),
        ]);

        assert_eq!(messages.len(), 1);
        assert_eq!(
            messages[0].detail.as_deref(),
            Some("  9 before\n- 10 old\n+ 10 new\n  11 after")
        );
        assert!(messages[0].expanded);
    }

    #[test]
    fn restores_complete_active_history_across_compaction() {
        let entries = vec![
            json!({
                "type":"message", "id":"user-old", "parentId":null,
                "message":{"role":"user","content":"old prompt"}
            }),
            json!({
                "type":"message", "id":"assistant-old", "parentId":"user-old",
                "message":{"role":"assistant","content":[{"type":"text","text":"old answer"}]}
            }),
            json!({
                "type":"compaction", "id":"compact-1", "parentId":"assistant-old",
                "summary":"summary of old work", "tokensBefore":100000
            }),
            json!({
                "type":"message", "id":"user-new", "parentId":"compact-1",
                "message":{"role":"user","content":"new prompt"}
            }),
            json!({
                "type":"message", "id":"other-branch", "parentId":"user-old",
                "message":{"role":"user","content":"abandoned prompt"}
            }),
        ];

        let messages = parse_entries(&entries, Some("user-new"));

        assert_eq!(messages.len(), 4);
        assert_eq!(messages[0].text, "old prompt");
        assert_eq!(messages[1].text, "old answer");
        assert!(messages[2].is_compaction());
        assert_eq!(messages[2].detail.as_deref(), Some("summary of old work"));
        assert_eq!(messages[3].text, "new prompt");
        assert!(
            messages
                .iter()
                .all(|message| message.text != "abandoned prompt")
        );
    }

    #[test]
    fn edit_errors_still_show_the_error_message() {
        let result = json!({
            "content":"oldText was not found",
            "details":{"diff":"-old\n+new"}
        });
        assert_eq!(
            tool_result_detail("edit", &result, true).as_deref(),
            Some("oldText was not found")
        );
    }
}
