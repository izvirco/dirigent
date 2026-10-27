//! Root view: all UI state, incoming host messages, and user actions.
//! Rendering lives in `sidebar.rs` (navigation), `lists.rs` (inbox / workpool / project
//! pages) and `thread.rs` (one thread + composer).

use crate::cache::Cache;
use crate::input::{self, ImagePasted, Submitted, TextInput, image_token};
use crate::net::{HostStatus, Incoming, Net};
use crate::orchestra::{self, LineageNode};
use crate::projects::{self, HostState, Links};
use crate::select::{Doc, OpenAppLink, Selection};
use crate::theme::{self, Status};
use crate::view::{self, Item, Streaming, Timeline};
use crate::{config, hosts, settings};
use crate::{lists, ruler, sidebar, thread, usage};
use base64::Engine;
use dirigent_protocol::{
    Ask, ClientMessage, Entry, ImageAttachment, ModelInfo, PromptMode, ServerMessage, ThreadSummary, TitleGen, Vcs, prompt_title,
};
use futures::StreamExt;
use gpui::EntityInputHandler;
use gpui::{
    AnyWindowHandle, App, ClipboardItem, Context, Entity, FocusHandle, Focusable, FollowMode, FontFeatures, ListAlignment, ListOffset,
    ListState, MouseButton, MouseMoveEvent, MouseUpEvent, Pixels, Point, Window, actions, div, prelude::*, px, rgba,
};
use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

actions!(dirigent, [NewThread, Abort, ToggleSidebar, ToggleLog, Quit]);

pub const KEY_CONTEXT: &str = "Dirigent";
const TOAST_TTL: Duration = Duration::from_secs(8);
/// How often the draft page / an open workspace menu re-asks the host about the repo.
const VCS_REFRESH_SECS: u64 = 2;
const LAST_MODEL_KEY: &str = "last_model";
const LAST_EFFORT_KEY: &str = "last_effort";
const LAST_TARGET_KEY: &str = "last_target";
/// Whether the last draft asked for an isolated workspace; the next one starts the same way.
const LAST_ISOLATE_KEY: &str = "last_isolate";
const PRESETS_KEY: &str = "presets";
const SIDEBAR_WIDTH_KEY: &str = "sidebar_width";
/// The sidebar's project order as the user dragged it: a JSON list of group keys.
const PROJECT_ORDER_KEY: &str = "project_order";
/// Where the user was when the app last closed (see `remember_pane`).
const PANE_KEY: &str = "pane";
const SIDEBAR_MIN: f32 = 180.;
const SIDEBAR_MAX: f32 = 420.;
const LOG_WIDTH_KEY: &str = "log_width";
const LOG_MIN: f32 = 280.;
const LOG_MAX: f32 = 800.;
/// How long a message's copy button shows its check.
const COPIED_TTL: Duration = Duration::from_millis(1200);
/// How often the usage page re-asks the hosts while it shows a window that ends now.
const USAGE_REFRESH_SECS: u64 = 30;
/// A thread's title shows as generating for at most this long after its first prompt.
const TITLING_TTL: Duration = Duration::from_secs(60);

/// Which edge the mouse is dragging.
#[derive(Clone, Copy, PartialEq)]
pub enum Resize {
    Sidebar,
    Log,
}

/// The thread shown in the main pane.
pub struct OpenThread {
    pub host: String,
    pub id: String,
    pub entries: Vec<Entry>,
    /// What the pane renders, folded from `entries` as they arrive.
    pub timeline: Timeline,
    pub streaming: Streaming,
    /// `timeline.items.len() + 1` items: the last one is the live tail (echoed messages, work in progress).
    pub list: ListState,
    /// (work item index, step index) of the tool calls whose details are open in the log
    /// panel. `thread::TAIL` as item index addresses the streaming tail.
    pub expanded: HashSet<(usize, usize)>,
    /// The work whose full log the right panel's log section shows (item index,
    /// `thread::TAIL` for a live work that has no persisted item yet); `None` = no log.
    pub log: Option<usize>,
    /// The log panel's steps, one item each; follows the tail while it shows a live work.
    pub log_list: ListState,
    /// The item whose copy button was just clicked (it shows a check for a moment).
    pub copied: Option<usize>,
    /// Entry ids of the pinned messages (amber in the ruler; the host keeps them, see
    /// `ThreadSummary::pinned_entries`), kept in step on every `Threads` push.
    pub pins: HashSet<String>,
    /// The ruler's animation and hover state (see `ruler.rs`).
    pub ruler: ruler::State,
    /// The composer's context chip is hovered: show the thread's totals (`thread::stats_card`).
    pub stats_hover: bool,
    /// A `jump_to` in progress: the item to center and the frames left to get it there.
    center_on: Option<(usize, u8)>,
    /// A form per open `ask_user` call (`ThreadSummary::asks`), kept in step by `sync_asks`.
    pub asks: Vec<AskForm>,
    /// The right panel (lineage over the work log): the user's choice from the header
    /// button, or `None` for the default (open while there is something to show; see
    /// `panel_open`). Clicking a work line resets it to the default; closing it from a
    /// work line or Esc sets `Some(false)`.
    pub panel: Option<bool>,
}

impl OpenThread {
    /// Timeline item `ix` is a pinned message.
    pub fn is_pinned(&self, ix: usize) -> bool {
        self.timeline.entry_id(ix).is_some_and(|id| self.pins.contains(id))
    }
}

/// The user's answers in progress to one `ask_user` call: the picked option indices and
/// the text field (where the question allows one) per question, one question shown at a
/// time (`current`).
pub struct AskForm {
    pub ask: Ask,
    pub picked: Vec<HashSet<usize>>,
    pub custom: Vec<Option<Entity<TextInput>>>,
    pub current: usize,
    /// Answered or dismissed; waiting for the host to drop the ask.
    pub sent: bool,
}

impl AskForm {
    /// The typed text for question `i`, if any.
    fn typed(&self, i: usize, cx: &App) -> Option<String> {
        self.custom[i].as_ref().map(|input| input.read(cx).text().trim().to_owned()).filter(|t| !t.is_empty())
    }

    /// Question `i` has an option picked or text typed.
    pub fn answered(&self, i: usize, cx: &App) -> bool {
        !self.picked[i].is_empty() || self.typed(i, cx).is_some()
    }

    /// Every question is answered: the form can be sent.
    pub fn complete(&self, cx: &App) -> bool {
        (0..self.ask.questions.len()).all(|i| self.answered(i, cx))
    }

    /// The next question after the shown one (wrapping) that still has no answer.
    pub fn next_unanswered(&self, cx: &App) -> Option<usize> {
        let n = self.ask.questions.len();
        (1..n).map(|k| (self.current + k) % n).find(|&i| !self.answered(i, cx))
    }

    /// One list per question: the picked options in order, then the typed text.
    pub fn answers(&self, cx: &App) -> Vec<Vec<String>> {
        self.ask
            .questions
            .iter()
            .enumerate()
            .map(|(i, q)| {
                let mut a: Vec<String> =
                    q.options.iter().enumerate().filter(|(j, _)| self.picked[i].contains(j)).map(|(_, o)| o.clone()).collect();
                a.extend(self.typed(i, cx));
                a
            })
            .collect()
    }
}

/// A host's answer about a directory (`ServerMessage::VcsStatus`).
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct VcsInfo {
    /// `None`: not under git or jj (or gone).
    pub vcs: Option<Vcs>,
    pub dirty: bool,
    pub branch: Option<String>,
    /// Branches / tags / bookmarks a workspace can start from.
    pub refs: Vec<String>,
}

/// What a workspace starts from when nothing is picked (mirrors the service's default).
pub fn default_base(vcs: Vcs) -> &'static str {
    match vcs {
        Vcs::Git => "HEAD",
        Vcs::Jj => "@-",
    }
}

/// Where a new thread will run: a project on a host.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct Target {
    pub host: String,
    pub project_id: String,
}

impl From<&projects::Member> for Target {
    fn from(m: &projects::Member) -> Self {
        Target { host: m.host.clone(), project_id: m.project.id.clone() }
    }
}

pub enum Pane {
    /// Finished threads, newest first, across every host.
    Inbox,
    /// Threads with an agent currently working.
    Workpool,
    /// One project group: all its threads, archived ones last.
    Project {
        group_key: String,
    },
    /// Path input for a new project on `host`.
    NewProject {
        host: String,
    },
    /// Settings › Config: `config.toml` (fonts, theme, thread titles, telemetry; `settings.rs`).
    Config,
    /// Settings › Hosts: remote hosts (`hosts.toml`) with their status, and the form to add one.
    Settings,
    /// Settings › Usage: tokens and cost over time, across hosts (`usage.rs`).
    Usage,
    /// New-thread page: the composer alone, centered. The thread is created on the host
    /// when the first message is sent.
    Draft {
        target: Option<Target>,
    },
    Thread(Box<OpenThread>),
}

/// An image pasted into the composer. It is referenced from the text as `[image-N]`:
/// the text is the truth, so deleting the token drops the image (see `attachments`).
#[derive(Clone)]
pub struct Attachment {
    pub n: u32,
    pub image: Arc<gpui::Image>,
}

/// An image an entry references by blob id (see `Dirigent::blobs`).
pub struct Blob {
    pub image: Arc<gpui::Image>,
    /// Width and height in pixels, turned upright the way gpui draws it (EXIF orientation);
    /// `None` if the header doesn't parse. Inline images size their box from it
    /// (`thread::inline_image`).
    pub size: Option<(f32, f32)>,
}

/// A saved model + effort combo, listed first in the model picker.
#[derive(Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct Preset {
    pub model: String,
    pub effort: Option<String>,
}

/// Sidebar sections that list threads under their nav row; each can be collapsed.
#[derive(Clone, Copy, PartialEq, Eq, Hash)]
pub enum Section {
    Inbox,
    Workpool,
}

/// The one popup that can be open.
#[derive(Clone, PartialEq)]
pub enum Menu {
    /// Model picker, presets page.
    Model,
    /// Model picker, searchable list of every model the host offers.
    AllModels,
    Effort,
    /// Draft: which project group the thread will run in.
    Target,
    /// Draft: which of the group's hosts.
    TargetHost,
    /// Sidebar actions for a project group (link / remove).
    Project {
        group_key: String,
    },
    /// Pick the group to link `group_key` into.
    Link {
        group_key: String,
    },
    /// Pick the host for a new project.
    NewProjectHost,
    /// Thread header: confirm deletion.
    ConfirmDelete,
    /// Thread header: archiving a thread that has a workspace asks what to do with it.
    ConfirmArchive,
    /// Composer chip on a thread: move into / out of a workspace.
    Workspace,
    /// The `New workspace from` popup (`base_search` over the repo's refs) behind the
    /// composer's workspace chip.
    Base,
    /// Usage page: what a series is (model / provider / everything).
    UsageGroup,
    /// Config page: theme, title model, title reasoning level.
    Theme,
    TitleModel,
    TitleEffort,
}

pub struct Dirigent {
    pub hosts: Vec<HostState>,
    pub links: Links,
    /// Project group keys in the order the user dragged them (see `projects::groups`).
    project_order: Vec<String>,
    /// The project group being dragged in the sidebar; its row is a gap meanwhile.
    pub dragging: Option<String>,
    pub pane: Pane,
    pub menu: Option<Menu>,
    /// The current mouse press closed a menu (clicked outside it). Suppresses `toggle_menu`
    /// for that click, so pressing a menu's own trigger closes it rather than reopening it.
    /// Reset at the start of every press by the root's capture handler.
    menu_dismissed: bool,
    /// Sidebar: user-dragged width, hidden with `[`, and "peek" when hidden but the mouse
    /// rests on the window's left edge.
    pub sidebar_width: f32,
    pub sidebar_hidden: bool,
    pub sidebar_peek: bool,
    pub collapsed: HashSet<Section>,
    /// Work log panel width (drag its left edge; `]` toggles the panel).
    pub log_width: f32,
    /// The log section is on: it sticks across threads, every thread opened showing its
    /// latest work (`latest_work`). Set by opening a log by hand (a work line, the panel
    /// button), cleared by closing the panel.
    pub log_on: bool,
    resizing: Option<Resize>,
    pub composer: Entity<TextInput>,
    pub path_input: Entity<TextInput>,
    /// The settings page's add-host form.
    pub host_name_input: Entity<TextInput>,
    pub host_url_input: Entity<TextInput>,
    /// Filter for the model picker's all-models page.
    pub model_search: Entity<TextInput>,
    /// Filter (or free text) for the workspace base picker.
    pub base_search: Entity<TextInput>,
    /// The usage page: window, grouping, hidden series and the hosts' buckets.
    pub usage: usage::State,
    /// Its custom range's date fields (`YYYY-MM-DD`; Enter in either applies both).
    pub usage_from: Entity<TextInput>,
    pub usage_to: Entity<TextInput>,
    /// Seconds since launch, from `tick`.
    ticks: u64,
    /// Model (`provider/id`) and thinking level the next prompt is sent with.
    pub model: Option<String>,
    pub effort: Option<String>,
    pub presets: Vec<Preset>,
    pub toasts: Vec<(String, Instant)>,
    /// `config.toml` as last loaded or saved, the fonts it resolves to, and the watcher
    /// that notices hand edits.
    pub config: config::Config,
    pub fonts: theme::Fonts,
    watch: config::Watch,
    /// Bumped on every keystroke in a text setting; the 300ms timer it starts only applies
    /// if no later keystroke bumped it again (`schedule_apply`).
    config_edit: u64,
    /// The Config page's inputs and title test.
    pub settings: settings::State,
    /// Threads whose title is being generated (since their first prompt), with when it started.
    titling: HashMap<(String, String), Instant>,
    /// Images pasted into the composer, in paste order. Only those whose `[image-N]` token
    /// is still in the text count (`attachments()`); the rest are pruned on send.
    /// Like the text, they belong to the thread (or draft) in view: `set_pane` saves them to
    /// the cache and loads the other thread's (see `save_draft`).
    attachments: Vec<Attachment>,
    /// The image shown full-size over everything; click or Esc closes it.
    pub preview: Option<Arc<gpui::Image>>,
    /// Mouse selection in the transcript or the work log.
    pub selection: Selection,
    /// Images referenced from entries, by blob id: filled from the cache db, else fetched
    /// from the host once (`blob_requests`) when the thread shows them.
    pub blobs: HashMap<String, Blob>,
    blob_requests: HashSet<String>,
    /// A draft was sent: the `CreateThread` request id and the message to prompt with once
    /// the host confirms the thread.
    pending: Option<(u64, String, Vec<Attachment>)>,
    /// Draft: start the thread in its own workspace (the composer's `isolated` chip), and
    /// from where (`None` = the vcs default; reset with every draft).
    pub isolate: bool,
    pub base: Option<String>,
    /// Version control state by `(host, path)`, as last reported (`request_vcs`): the
    /// draft's project directory, an open thread's project, a workspace before removal.
    pub vcs: HashMap<(String, String), VcsInfo>,
    net: Net,
    cache: Arc<Mutex<Cache>>,
    next_id: u64,
    focus_handle: FocusHandle,
    window: AnyWindowHandle,
}

impl Dirigent {
    pub fn new(cache: Cache, config: config::Config, window: &mut Window, cx: &mut Context<Self>) -> Self {
        // `local` is the embedded service; the rest come from hosts.toml.
        let mut hosts = vec![(hosts::LOCAL.to_owned(), format!("ws://127.0.0.1:{}/ws", dirigent_service::port()))];
        let hosts_error = match hosts::load() {
            Ok(remote) => {
                hosts.extend(remote);
                None
            }
            Err(e) => Some(format!("{e:#}")),
        };
        let host_states = hosts.iter().map(|(name, url)| host_state(&cache, name, url)).collect();
        let links = cache.links().unwrap_or_default().into_iter().map(|(h, p, g)| ((h, p), g)).collect();
        let project_order = cache.get(PROJECT_ORDER_KEY).and_then(|s| serde_json::from_str(&s).ok()).unwrap_or_default();
        let model = cache.get(LAST_MODEL_KEY);
        let effort = cache.get(LAST_EFFORT_KEY);
        let presets = cache.get(PRESETS_KEY).and_then(|s| serde_json::from_str(&s).ok()).unwrap_or_default();
        let isolate = cache.get(LAST_ISOLATE_KEY).as_deref() == Some("1");
        let sidebar_width = cache.get(SIDEBAR_WIDTH_KEY).and_then(|w| w.parse().ok()).unwrap_or(sidebar::WIDTH);
        let log_width = cache.get(LOG_WIDTH_KEY).and_then(|w| w.parse().ok()).unwrap_or(thread::LOG_WIDTH);
        let cache = Arc::new(Mutex::new(cache));

        let (tx, mut rx) = futures::channel::mpsc::unbounded();
        let net = Net::start(hosts, cache.clone(), tx);
        cx.spawn(async move |this, cx| {
            while let Some(msg) = rx.next().await {
                if this.update(cx, |this, cx| this.on_incoming(msg, cx)).is_err() {
                    break;
                }
            }
        })
        .detach();

        let composer = cx.new(|cx| TextInput::new(cx, "Calibre - The Game", false));
        let path_input = cx.new(|cx| TextInput::new(cx, "/absolute/path/to/project", true));
        cx.subscribe_in(&composer, window, |this, _, s: &Submitted, window, cx| this.send(s.queued, window, cx)).detach();
        cx.subscribe_in(&composer, window, |this, _, p: &ImagePasted, window, cx| this.attach(p.0.clone(), window, cx)).detach();
        cx.subscribe(&composer, |this, _, _: &input::Changed, cx| this.save_draft(cx)).detach();
        cx.subscribe_in(&path_input, window, |this, _, _: &Submitted, window, cx| this.add_project(window, cx)).detach();
        let host_name_input = cx.new(|cx| TextInput::new(cx, "name", true));
        let host_url_input = cx.new(|cx| TextInput::new(cx, "host:7777 or ws://host:7777/ws", true));
        for input in [&host_name_input, &host_url_input] {
            cx.subscribe_in(input, window, |this, _, _: &Submitted, window, cx| this.add_host(window, cx)).detach();
        }
        let model_search = cx.new(|cx| TextInput::new(cx, "Search models", true));
        cx.subscribe(&model_search, |this, _, _: &Submitted, cx| this.pick_first_model(cx)).detach();
        let base_search = cx.new(|cx| TextInput::new(cx, "Branch, commit or revset", true));
        cx.subscribe(&base_search, |this, _, _: &Submitted, cx| this.pick_first_base(cx)).detach();
        let usage_from = cx.new(|cx| TextInput::new(cx, "2026-01-01", true));
        let usage_to = cx.new(|cx| TextInput::new(cx, "2026-01-31", true));
        for input in [&usage_from, &usage_to] {
            cx.subscribe(input, |this, _, _: &Submitted, cx| this.apply_custom_usage(cx)).detach();
        }
        let mut settings = settings::State::new(cx);
        // Text settings apply as you type (debounced); nothing to press, nothing to lose.
        for input in [&settings.font_ui, &settings.font_prose, &settings.font_code, &settings.prompt] {
            cx.subscribe(input, |this, _, _: &input::Changed, cx| this.schedule_apply(cx)).detach();
        }
        cx.subscribe(&settings.sample, |this, _, _: &Submitted, cx| this.run_title_test(cx)).detach();
        let (fonts, missing) = config.fonts.resolve();
        settings.missing_fonts = missing.clone();

        let mut this = Self {
            hosts: host_states,
            links,
            project_order,
            dragging: None,
            pane: Pane::Inbox,
            menu: None,
            menu_dismissed: false,
            sidebar_width,
            sidebar_hidden: false,
            sidebar_peek: false,
            collapsed: HashSet::new(),
            log_width,
            log_on: false,
            resizing: None,
            composer,
            path_input,
            host_name_input,
            host_url_input,
            model_search,
            base_search,
            usage: usage::State::new(),
            usage_from,
            usage_to,
            ticks: 0,
            model,
            effort,
            presets,
            toasts: vec![],
            config,
            fonts,
            watch: config::Watch::new(),
            config_edit: 0,
            settings,
            titling: HashMap::new(),
            attachments: vec![],
            preview: None,
            selection: Selection::default(),
            blobs: HashMap::new(),
            blob_requests: HashSet::new(),
            pending: None,
            isolate,
            base: None,
            vcs: HashMap::new(),
            net,
            cache,
            next_id: 1,
            focus_handle: cx.focus_handle(),
            window: window.window_handle(),
        };
        // The window was opened with this theme's background already (see `main`).
        match theme::load(&this.config.theme) {
            Ok(t) => theme::set(t),
            Err(e) => this.toast(format!("{e:#}"), cx),
        }
        this.restore_pane(cx);
        this.tick(cx);
        if let Some(e) = hosts_error {
            this.toast(e, cx);
        }
        for font in missing {
            this.toast(format!("font `{font}` is not installed"), cx);
        }
        this
    }

    fn next_id(&mut self) -> u64 {
        self.next_id += 1;
        self.next_id
    }

    fn host_mut(&mut self, name: &str) -> Option<&mut HostState> {
        self.hosts.iter_mut().find(|h| h.name == name)
    }

    pub fn host(&self, name: &str) -> Option<&HostState> {
        self.hosts.iter().find(|h| h.name == name)
    }

    pub fn open(&self) -> Option<&OpenThread> {
        match &self.pane {
            Pane::Thread(t) => Some(t),
            _ => None,
        }
    }

    pub fn summary(&self, host: &str, id: &str) -> Option<&ThreadSummary> {
        self.host(host)?.threads.iter().find(|t| t.id == id)
    }

    /// The open thread has an agent working.
    pub fn is_running(&self) -> bool {
        self.open().and_then(|t| self.summary(&t.host, &t.id)).is_some_and(|t| t.running())
    }

    /// Finished since the user last opened it, on any device (never opened counts too). A
    /// thread waiting on jobs or sub-agents isn't finished yet, so it isn't unread either.
    pub fn unread(&self, host: &str, t: &ThreadSummary) -> bool {
        self.status(host, t).finished() && t.seen_at.is_none_or(|s| t.updated_at > s)
    }

    /// A thread's title colour in the sidebar and lists: amber while the model waits on an
    /// answer, accent while unread, else `fg`.
    pub fn title_color(&self, host: &str, t: &ThreadSummary) -> u32 {
        if t.asking() {
            theme::t().warn
        } else if self.unread(host, t) {
            theme::t().accent_ink
        } else {
            theme::t().fg
        }
    }

    /// `Status::of`, with an idle thread that has `async_bash` jobs running or sub-agents
    /// still at it reported as `Waiting`: their results will wake it.
    pub fn status(&self, host: &str, t: &ThreadSummary) -> Status {
        match Status::of(t) {
            Status::Done if t.jobs > 0 || self.has_busy_children(host, &t.id) => Status::Waiting,
            s => s,
        }
    }

    /// A sub-agent (at any depth) is working or has jobs running.
    fn has_busy_children(&self, host: &str, id: &str) -> bool {
        let Some(h) = self.host(host) else { return false };
        h.threads
            .iter()
            .filter(|t| t.parent_id.as_deref() == Some(id))
            .any(|t| t.running() || t.jobs > 0 || self.has_busy_children(host, &t.id))
    }

    pub fn pinned(&self, host: &str, thread_id: &str) -> bool {
        self.summary(host, thread_id).is_some_and(|t| t.pinned)
    }

    /// Pin keeps a thread on top of its status group (pinned done threads above the other
    /// done ones, pinned snoozed above the snoozed pile). The host keeps it (every device
    /// and agents see it); the flag is set here ahead of its `Threads` push.
    pub fn set_pinned(&mut self, host: &str, thread_id: &str, pinned: bool, cx: &mut Context<Self>) {
        let id = self.next_id();
        self.net.send(host, ClientMessage::PinThread { id, thread_id: thread_id.to_owned(), pinned });
        if let Some(t) = self.summary_mut(host, thread_id) {
            t.pinned = pinned;
        }
        cx.notify();
    }

    fn summary_mut(&mut self, host: &str, thread_id: &str) -> Option<&mut ThreadSummary> {
        self.host_mut(host).and_then(|h| h.threads.iter_mut().find(|t| t.id == thread_id))
    }

    /// Group rows by status (working, waiting, asking, done, snoozed, archived) with the
    /// pinned ones first in each group, keeping their order otherwise.
    fn sort_by_status(&self, rows: &mut [(&str, &ThreadSummary)]) {
        rows.sort_by_key(|(h, t)| (self.status(h, t), !t.pinned));
    }

    /// Order `rows` (most recently updated first on input) as a queue, newest on top.
    /// Working threads are ordered by when their run started, finished ones by when they
    /// finished (their last entry): neither key changes while the thread is on that list,
    /// so the order is fixed and a thread only moves when it enters the list. On top of
    /// that, `sort_by_status` lifts the pinned ones (and sinks snoozed finished ones).
    /// Shared by the Inbox, the Workpool and the project page.
    pub fn sort_as_queue(&self, rows: &mut [(&str, &ThreadSummary)]) {
        rows.sort_by_key(|(_, t)| std::cmp::Reverse(t.running_since));
        self.sort_by_status(rows);
    }

    /// Snooze hides a finished thread from the sidebar's inbox without archiving it; it
    /// comes back by itself once it runs again, or when the user wakes it. The host keeps
    /// it (agents can snooze too); the flag is set here ahead of its `Threads` push.
    pub fn set_snoozed(&mut self, host: &str, thread_id: &str, snoozed: bool, cx: &mut Context<Self>) {
        let id = self.next_id();
        self.net.send(host, ClientMessage::SnoozeThread { id, thread_id: thread_id.to_owned(), snoozed });
        if let Some(t) = self.summary_mut(host, thread_id) {
            t.snoozed = snoozed;
        }
        cx.notify();
    }

    /// What the sidebar lists under Inbox: every finished thread that isn't snoozed. The
    /// Inbox page itself lists them all (`inbox_all`).
    pub fn inbox_threads(&self) -> Vec<(&str, &ThreadSummary)> {
        let mut rows = self.inbox_all();
        rows.retain(|(_, t)| !t.snoozed);
        rows
    }

    /// The Inbox: threads waiting on the user, the ones asking a question first.
    pub fn inbox_all(&self) -> Vec<(&str, &ThreadSummary)> {
        self.threads_where(|s| s.finished())
    }

    /// The Workpool: threads with an agent at it, directly or through sub-agents.
    pub fn busy_threads(&self) -> Vec<(&str, &ThreadSummary)> {
        self.threads_where(|s| s.busy())
    }

    /// The user is looking at this thread: whatever it has now is read, on every device.
    /// Called again on each `Threads` push while the thread is open, so a mark goes out
    /// only once the thread is finished (a busy one can't be unread, see `unread`) and has
    /// moved since the last one: the host's push of the mark must not trigger another. The
    /// flag is set here ahead of that push.
    fn mark_seen(&mut self, host: &str, id: &str) {
        let Some(updated_at) = self
            .summary(host, id)
            .filter(|t| self.status(host, t).finished() && t.seen_at.is_none_or(|s| s < t.updated_at))
            .map(|t| t.updated_at)
        else {
            return;
        };
        let rid = self.next_id();
        self.net.send(host, ClientMessage::MarkSeen { id: rid, thread_id: id.to_owned(), updated_at });
        if let Some(t) = self.summary_mut(host, id) {
            t.seen_at = Some(updated_at);
        }
    }

    /// Every top-level thread on every host, most recently updated first, with its host.
    /// Sub-agents stay out of the lists: their parent's orchestration panel is their place.
    pub fn all_threads(&self) -> Vec<(&str, &ThreadSummary)> {
        let mut all: Vec<_> =
            self.hosts.iter().flat_map(|h| h.threads.iter().filter(|t| t.top_level()).map(move |t| (h.name.as_str(), t))).collect();
        all.sort_by_key(|(_, t)| std::cmp::Reverse(t.updated_at));
        all
    }

    fn threads_where(&self, keep: impl Fn(Status) -> bool) -> Vec<(&str, &ThreadSummary)> {
        let mut rows = self.all_threads();
        rows.retain(|(h, t)| keep(self.status(h, t)));
        self.sort_as_queue(&mut rows);
        rows
    }

    /// The family of a thread on `host`, for the orchestra panel (`orchestra::lineage`).
    pub fn lineage(&self, host: &str, id: &str) -> Vec<LineageNode<'_>> {
        let Some(h) = self.host(host) else { return vec![] };
        orchestra::lineage(&h.threads, id)
    }

    /// Group key of a project, if the host still lists it.
    pub fn group_key_of(&self, host: &str, project_id: &str) -> Option<String> {
        let project = self.host(host)?.projects.iter().find(|p| p.id == project_id)?;
        Some(projects::group_key(host, project, &self.links))
    }

    /// Directory of a thread's project, if the host still lists it.
    pub fn project_path(&self, host: &str, t: &ThreadSummary) -> Option<String> {
        let pid = t.project_id.as_deref()?;
        self.host(host)?.projects.iter().find(|p| p.id == pid).map(|p| p.path.clone())
    }

    /// Display name of a thread's project ("" for orphans).
    pub fn project_name(&self, host: &str, t: &ThreadSummary) -> String {
        t.project_id
            .as_deref()
            .and_then(|pid| self.host(host)?.projects.iter().find(|p| p.id == pid))
            .map(|p| projects::basename(&p.path).to_owned())
            .unwrap_or_default()
    }

    /// Host of whatever the composer currently targets: the draft's project, else the open thread.
    fn composer_host(&self) -> Option<&str> {
        match &self.pane {
            Pane::Draft { target: Some(t) } => Some(&t.host),
            Pane::Thread(t) => Some(&t.host),
            _ => None,
        }
    }

    /// Models known for the composer's host (cached list; refreshed on connect).
    pub fn models(&self) -> &[ModelInfo] {
        self.composer_host().and_then(|h| self.host(h)).map(|h| h.models.as_slice()).unwrap_or(&[])
    }

    pub fn current_model(&self) -> Option<&ModelInfo> {
        let key = self.model.as_deref()?;
        self.models().iter().find(|m| m.key() == key)
    }

    pub fn toast(&mut self, message: impl Into<String>, cx: &mut Context<Self>) {
        self.toasts.push((message.into(), Instant::now()));
        cx.spawn(async move |this, cx| {
            cx.background_executor().timer(TOAST_TTL).await;
            this.update(cx, |this, cx| {
                this.toasts.retain(|(_, t)| t.elapsed() < TOAST_TTL);
                cx.notify();
            })
            .ok();
        })
        .detach();
        cx.notify();
    }

    // ---- incoming from hosts ----

    fn on_incoming(&mut self, msg: Incoming, cx: &mut Context<Self>) {
        match msg {
            Incoming::Status { host, status } => {
                if let Some(h) = self.host_mut(&host) {
                    h.status = status;
                }
                // Requests made while offline are dropped, so the open thread's `GetThread`
                // from startup (or from before a reconnect) is repeated here.
                if status == HostStatus::Connected
                    && let Some(id) = self.open().filter(|t| t.host == host).map(|t| t.id.clone())
                {
                    let rid = self.next_id();
                    self.net.send(&host, ClientMessage::GetThread { id: rid, thread_id: id });
                }
                if status == HostStatus::Connected && matches!(self.pane, Pane::Usage) {
                    self.request_usage(cx);
                }
                if status == HostStatus::Connected && matches!(self.pane, Pane::Draft { .. }) {
                    self.refresh_draft_vcs();
                }
                if status == HostStatus::Connected {
                    // Snoozes, pins and seen marks from before the hosts kept them: handed over once.
                    let (snoozes, pins, seen) = {
                        let cache = self.cache.lock().unwrap();
                        (
                            cache.take_legacy_snoozes(&host).unwrap_or_default(),
                            cache.take_legacy_pins(&host).unwrap_or_default(),
                            cache.take_legacy_seen(&host).unwrap_or_default(),
                        )
                    };
                    for thread_id in snoozes {
                        let id = self.next_id();
                        self.net.send(&host, ClientMessage::SnoozeThread { id, thread_id, snoozed: true });
                    }
                    for thread_id in pins {
                        let id = self.next_id();
                        self.net.send(&host, ClientMessage::PinThread { id, thread_id, pinned: true });
                    }
                    for (thread_id, updated_at) in seen {
                        let id = self.next_id();
                        self.net.send(&host, ClientMessage::MarkSeen { id, thread_id, updated_at });
                    }
                }
            }
            Incoming::Message { host, msg } => self.on_server_message(host, *msg, cx),
        }
        cx.notify();
    }

    fn on_server_message(&mut self, host: String, msg: ServerMessage, cx: &mut Context<Self>) {
        match msg {
            ServerMessage::Hello { .. } | ServerMessage::Ok { .. } => {}
            ServerMessage::Projects { projects, .. } => {
                if let Some(h) = self.host_mut(&host) {
                    h.projects = projects;
                }
            }
            ServerMessage::ProjectAdded { project, .. } => {
                if let Some(h) = self.host_mut(&host)
                    && !h.projects.iter().any(|p| p.id == project.id)
                {
                    h.projects.push(project.clone());
                }
                // Straight into a first thread for the new project.
                self.new_draft(Some(Target { host, project_id: project.id }), cx);
            }
            ServerMessage::Models { models, .. } => {
                if let Some(h) = self.host_mut(&host) {
                    h.models = models;
                }
            }
            ServerMessage::Threads { threads, .. } => {
                let was_running = self.is_running();
                if let Some(h) = self.host_mut(&host) {
                    h.threads = threads;
                }
                if let Pane::Thread(t) = &self.pane
                    && t.host == host
                {
                    let id = t.id.clone();
                    if let Some(pins) = self.summary(&host, &id).map(|s| s.pinned_entries.iter().cloned().collect()) {
                        // The run ended without pi settling it (the runtime died under it):
                        // the host reports the thread idle, so nothing streams or runs any more.
                        let died = was_running && !self.is_running();
                        if let Pane::Thread(t) = &mut self.pane {
                            t.pins = pins;
                            if died {
                                t.streaming.settle();
                                let tail = t.timeline.items.len();
                                t.list.remeasure_items(tail..tail + 1);
                                if t.log.is_some() {
                                    t.log_list.remeasure();
                                }
                            }
                        }
                        self.mark_seen(&host, &id);
                        self.sync_asks(cx);
                        // The run ended, or the thread moved: the repo state may have changed.
                        self.request_thread_vcs(&host, &id);
                    } else {
                        // The open thread was deleted elsewhere.
                        self.show(Pane::Inbox, cx);
                    }
                }
            }
            ServerMessage::Thread { thread_id, entries, .. } => {
                if let Pane::Thread(t) = &mut self.pane
                    && t.host == host
                    && t.id == thread_id
                {
                    t.timeline = Timeline::from_entries(entries.iter().map(|e| &e.json));
                    t.entries = entries;
                    t.list.reset(t.timeline.items.len() + 1);
                    // The cache may have lagged: the log follows the latest work again.
                    if t.log.is_some() {
                        self.show_latest_log();
                    }
                    self.fetch_blobs();
                }
            }
            ServerMessage::Usage { reply_to, buckets } => {
                if self.usage.request == Some(reply_to) {
                    self.usage.buckets.insert(host, buckets);
                }
            }
            ServerMessage::Title { reply_to, title, model, input, output, cost, duration_ms } => {
                if self.settings.test_request.take_if(|(id, _)| *id == reply_to).is_some() {
                    self.settings.test_result = Some(Ok(settings::TitleResult { title, model, input, output, cost, duration_ms }));
                }
            }
            ServerMessage::Blob { blob_id, mime_type, data, .. } => {
                self.blob_requests.remove(&blob_id);
                if let Some(blob) = decode_blob(&mime_type, &data) {
                    self.blobs.insert(blob_id, blob);
                }
            }
            ServerMessage::VcsStatus { path, vcs, dirty, branch, refs, .. } => {
                self.vcs.insert((host, path), VcsInfo { vcs, dirty, branch, refs });
            }
            // An agent asked for the user's attention on a thread.
            ServerMessage::Focus { thread_id } => {
                if self.summary(&host, &thread_id).is_some() {
                    self.select_thread(&host, &thread_id, cx);
                }
            }
            ServerMessage::Search { .. } | ServerMessage::Hosts { .. } => {}
            ServerMessage::ThreadCreated { reply_to, mut thread } => {
                let pending = self.pending.take_if(|(id, ..)| *id == reply_to);
                if let Some((_, text, _)) = &pending {
                    // Our draft: it goes straight to work, so list it that way before the
                    // host's push confirms it (no flash through the inbox with no model,
                    // nor through "Untitled": the host names it the same way on `Prompt`).
                    thread.name = thread.name.or_else(|| prompt_title(text));
                    thread.running_since = Some(now_ms());
                    thread.model = self.model.clone();
                    thread.thinking_level = self.effort.clone();
                }
                if let Some(h) = self.host_mut(&host)
                    && !h.threads.iter().any(|t| t.id == thread.id)
                {
                    h.threads.insert(0, thread.clone());
                }
                if let Some((_, text, images)) = pending {
                    self.select_thread(&host, &thread.id, cx);
                    self.prompt(&host, &thread.id, text, images, PromptMode::Prompt, cx);
                }
            }
            ServerMessage::Error { reply_to, message } => {
                if self.settings.test_request.take_if(|(id, _)| Some(*id) == reply_to).is_some() {
                    self.settings.test_result = Some(Err(message));
                    return;
                }
                // A failed create puts the draft back so nothing typed is lost: into the
                // cache, and into the composer if the draft page is still in view.
                if let Some((_, text, images)) = self.pending.take_if(|(id, ..)| Some(*id) == reply_to) {
                    let content = input::Content { selection: text.len()..text.len(), text, reversed: false };
                    let (host, id) = DRAFT_KEY;
                    let _ = self.cache.lock().unwrap().set_draft(host, id, &content, &image_rows(&images));
                    if composer_key(&self.pane).is_some_and(|k| k == DRAFT_KEY) {
                        self.load_composer(cx);
                    }
                }
                self.toast(format!("{host}: {message}"), cx)
            }
            ServerMessage::Entry { thread_id, entry } => {
                // The generated name arrives as pi's `session_info` entry.
                if entry.kind == "session_info" {
                    self.titling.remove(&(host.clone(), thread_id.clone()));
                }
                if let Pane::Thread(t) = &mut self.pane
                    && t.host == host
                    && t.id == thread_id
                {
                    // The persisted message supersedes what we streamed / echoed optimistically.
                    match entry.json.pointer("/message/role").and_then(|r| r.as_str()) {
                        Some("assistant") => t.streaming.steps.clear(),
                        // A user message into an open work is a steer; `queue_update` already
                        // dropped its echo (see `Timeline::push_message`, `Streaming::apply`).
                        Some("user") if t.timeline.open_work().is_none() => t.streaming.confirm_prompt(),
                        _ => {}
                    }
                    if entry.json.get("customType").and_then(serde_json::Value::as_str) == Some("dirigent:answer") {
                        t.streaming.confirm_answer(&entry.json);
                    }
                    let n = t.timeline.items.len();
                    let changed = t.timeline.push(&entry.json);
                    t.entries.push(entry);
                    let m = t.timeline.items.len();
                    // The live work got its persisted item: the log panel follows it there.
                    if t.log == Some(thread::TAIL) {
                        t.log = Some(t.timeline.open_work().unwrap_or(thread::TAIL));
                    }
                    if m > n {
                        t.list.splice(n..n, m - n);
                    } else if m < n {
                        t.list.splice(m..n, 0);
                    }
                    // A tool result or new step lands in an existing work item; the tail always moves.
                    if let Some(c) = changed {
                        t.list.remeasure_items(c..n.min(m));
                    }
                    t.list.remeasure_items(m..m + 1);
                    if t.log.is_some() {
                        t.log_list.remeasure();
                    }
                    self.fetch_blobs();
                }
            }
            ServerMessage::Event { thread_id, event } => {
                // A warning on a thread being named is the runtime giving up on the title.
                if event.get("type").and_then(|v| v.as_str()) == Some("warning") {
                    self.titling.remove(&(host.clone(), thread_id.clone()));
                }
                if let Pane::Thread(t) = &mut self.pane
                    && t.host == host
                    && t.id == thread_id
                    && t.streaming.apply(&event, now_ms())
                {
                    let tail = t.timeline.items.len();
                    t.list.remeasure_items(tail..tail + 1);
                    if t.log.is_some() {
                        t.log_list.remeasure();
                    }
                }
            }
        }
    }

    // ---- navigation ----

    pub fn show(&mut self, pane: Pane, cx: &mut Context<Self>) {
        self.set_pane(pane, cx);
        self.menu = None;
        self.selection.clear();
        self.remember_pane();
        self.focus(self.focus_handle.clone(), cx);
        cx.notify();
    }

    /// Every pane change goes through here: the composer belongs to the thread (or draft)
    /// in view, so leaving one saves its text, selection and images and entering another
    /// restores its own (`composer_key`). Other panes don't show the composer and leave it
    /// alone.
    fn set_pane(&mut self, pane: Pane, cx: &mut Context<Self>) {
        let (from, to) = (composer_key(&self.pane), composer_key(&pane));
        let (leaving, entering) = (from != to && from.is_some(), from != to && to.is_some());
        if leaving {
            self.save_draft(cx);
        }
        self.pane = pane;
        if entering {
            self.load_composer(cx);
        }
    }

    /// Persist the pane so the next launch resumes there. The draft page and the new-project
    /// form leave the last stored pane alone (the draft's text is kept by `save_draft`, the
    /// page itself isn't).
    fn remember_pane(&self) {
        let value = match &self.pane {
            Pane::Inbox => "inbox".to_owned(),
            Pane::Workpool => "workpool".to_owned(),
            Pane::Project { group_key } => format!("project\n{group_key}"),
            Pane::Thread(t) => format!("thread\n{}\n{}", t.host, t.id),
            Pane::Config => "config".to_owned(),
            Pane::Settings => "settings".to_owned(),
            Pane::Usage => "usage".to_owned(),
            Pane::Draft { .. } | Pane::NewProject { .. } => return,
        };
        let _ = self.cache.lock().unwrap().set(PANE_KEY, &value);
    }

    /// Reopen the pane stored by `remember_pane`; a thread only if the cache still knows it.
    fn restore_pane(&mut self, cx: &mut Context<Self>) {
        let Some(value) = self.cache.lock().unwrap().get(PANE_KEY) else { return };
        let parts: Vec<&str> = value.split('\n').collect();
        match parts.as_slice() {
            ["workpool"] => self.set_pane(Pane::Workpool, cx),
            ["config"] => self.show_config(cx),
            ["settings"] => self.set_pane(Pane::Settings, cx),
            // The hosts aren't connected yet; the request goes out as each one connects.
            ["usage"] => self.set_pane(Pane::Usage, cx),
            ["project", key] => self.set_pane(Pane::Project { group_key: (*key).to_owned() }, cx),
            ["thread", host, id] if self.summary(host, id).is_some() => {
                let (host, id) = ((*host).to_owned(), (*id).to_owned());
                self.select_thread(&host, &id, cx);
            }
            _ => {}
        }
    }

    /// A click on a `dirigent://` link (see `select.rs`). `dirigent://thread/<id>` opens the
    /// thread wherever it lives: thread ids are unique across hosts, and sub-agents (not in
    /// the sidebar) open like any other thread. Anything else, or an id no connected host
    /// knows, gets a toast.
    fn open_app_link(&mut self, link: &OpenAppLink, _: &mut Window, cx: &mut Context<Self>) {
        let Some(id) = link.url.strip_prefix("dirigent://thread/") else {
            return self.toast(format!("Can't open {}", link.url), cx);
        };
        match self.hosts.iter().find(|h| h.threads.iter().any(|t| t.id == id)) {
            Some(h) => {
                let host = h.name.clone();
                self.select_thread(&host, id, cx);
            }
            None => self.toast("Thread not found: it was deleted, or lives on a host this desktop doesn't connect to", cx),
        }
    }

    /// Show cached entries immediately, then ask the host for the full thread.
    pub fn select_thread(&mut self, host: &str, id: &str, cx: &mut Context<Self>) {
        let entries = self.cache.lock().unwrap().entries(host, id).unwrap_or_default();
        let timeline = Timeline::from_entries(entries.iter().map(|e| &e.json));
        let list = ListState::new(timeline.items.len() + 1, ListAlignment::Top, px(300.));
        list.set_follow_mode(FollowMode::Tail);
        // The thread's own model if it has one, else whatever we used last.
        if let Some((m, e)) = self.summary(host, id).filter(|s| s.model.is_some()).map(|s| (s.model.clone(), s.thinking_level.clone())) {
            self.model = m;
            self.effort = e;
        }
        let pane = Pane::Thread(Box::new(OpenThread {
            host: host.to_owned(),
            id: id.to_owned(),
            entries,
            timeline,
            streaming: Streaming::default(),
            list,
            expanded: HashSet::new(),
            log: None,
            log_list: ListState::new(0, ListAlignment::Top, px(300.)),
            copied: None,
            pins: self.summary(host, id).map(|s| s.pinned_entries.iter().cloned().collect()).unwrap_or_default(),
            ruler: Default::default(),
            stats_hover: false,
            center_on: None,
            asks: vec![],
            panel: None,
        }));
        self.set_pane(pane, cx);
        if self.log_on {
            self.show_latest_log();
        }
        self.menu = None;
        self.base = None;
        self.selection.clear();
        self.remember_pane();
        self.mark_seen(host, id);
        self.sync_asks(cx);
        self.fetch_blobs();
        let rid = self.next_id();
        self.net.send(host, ClientMessage::GetThread { id: rid, thread_id: id.to_owned() });
        self.request_thread_vcs(host, id);
        self.focus_composer(cx);
        cx.notify();
    }

    // ---- version control ----

    /// Ask `host` about a directory; the reply lands in `vcs`.
    pub fn request_vcs(&mut self, host: &str, path: &str) {
        if self.host(host).is_none_or(|h| h.status != HostStatus::Connected) {
            return;
        }
        let id = self.next_id();
        self.net.send(host, ClientMessage::VcsStatus { id, path: path.to_owned() });
    }

    /// The repo state behind a thread: its project directory (is it a repo, is it clean,
    /// so the workspace chip knows what it can offer) and its workspace (dirty before removal).
    fn request_thread_vcs(&mut self, host: &str, id: &str) {
        let Some(s) = self.summary(host, id) else { return };
        // A missing workspace has nothing to report until it is recreated.
        let workspace = s.workspace.clone().filter(|_| !s.workspace_missing);
        let paths: Vec<String> = self.project_path(host, s).into_iter().chain(workspace).collect();
        for p in paths {
            self.request_vcs(host, &p);
        }
    }

    pub fn vcs_of(&self, host: &str, path: &str) -> Option<&VcsInfo> {
        self.vcs.get(&(host.to_owned(), path.to_owned()))
    }

    /// The draft's project directory and what the host knows about it.
    pub fn draft_vcs(&self) -> Option<(&Target, &str, Option<&VcsInfo>)> {
        let Pane::Draft { target: Some(t) } = &self.pane else { return None };
        let path = self.host(&t.host)?.projects.iter().find(|p| p.id == t.project_id)?.path.as_str();
        Some((t, path, self.vcs_of(&t.host, path)))
    }

    /// Threads working in `path` itself (not in a workspace of it) on `host`: a draft started
    /// there now would share the checkout with them.
    pub fn working_in(&self, host: &str, path: &str) -> Vec<&ThreadSummary> {
        self.host(host)
            .map(|h| h.threads.iter().filter(|t| t.running() && t.workspace.is_none() && t.cwd == path).collect())
            .unwrap_or_default()
    }

    pub fn set_isolate(&mut self, isolate: bool, cx: &mut Context<Self>) {
        self.isolate = isolate;
        let _ = self.cache.lock().unwrap().set(LAST_ISOLATE_KEY, if isolate { "1" } else { "0" });
        self.focus_composer(cx);
        cx.notify();
    }

    /// Open the base picker with an empty, focused search.
    pub fn open_base_picker(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.base_search.update(cx, |input, cx| input.set_text("", window, cx));
        self.menu = Some(Menu::Base);
        self.focus(self.base_search.focus_handle(cx), cx);
        cx.notify();
    }

    /// The refs behind the open base picker: the draft's project, or the open thread's.
    pub fn base_refs(&self) -> Vec<String> {
        let info = match &self.pane {
            Pane::Draft { .. } => self.draft_vcs().and_then(|(_, _, v)| v),
            Pane::Thread(t) => {
                let s = self.summary(&t.host, &t.id);
                s.and_then(|s| self.project_path(&t.host, s)).and_then(|p| self.vcs_of(&t.host, &p))
            }
            _ => None,
        };
        info.map(|v| v.refs.clone()).unwrap_or_default()
    }

    /// Refs containing the typed text, in host order.
    pub fn filtered_refs(&self, cx: &App) -> Vec<String> {
        let query = self.base_search.read(cx).text().to_lowercase();
        self.base_refs().into_iter().filter(|r| r.to_lowercase().contains(&query)).collect()
    }

    /// Enter in the base search: the top match, or the typed text as is (a commit, `main~3`,
    /// a revset: anything the vcs resolves). Empty means the default.
    fn pick_first_base(&mut self, cx: &mut Context<Self>) {
        let typed = self.base_search.read(cx).text().trim().to_owned();
        let base = match self.filtered_refs(cx).into_iter().next() {
            Some(r) if !typed.is_empty() => Some(r),
            _ if typed.is_empty() => None,
            _ => Some(typed),
        };
        self.pick_base(base, cx);
    }

    /// A base was picked in the `New workspace from` popup (`None` = the vcs default): a
    /// draft will start its thread in a workspace there; an open thread moves there now.
    pub fn pick_base(&mut self, base: Option<String>, cx: &mut Context<Self>) {
        match &self.pane {
            Pane::Thread(t) => {
                let (host, id) = (t.host.clone(), t.id.clone());
                self.move_to_workspace(&host, &id, base, cx);
            }
            _ => {
                self.base = base;
                self.set_isolate(true, cx);
            }
        }
        self.menu = None;
        self.focus_composer(cx);
        cx.notify();
    }

    /// Move the thread into its own workspace at `base`, or recreate one that went missing.
    pub fn move_to_workspace(&mut self, host: &str, thread_id: &str, base: Option<String>, cx: &mut Context<Self>) {
        let id = self.next_id();
        self.net.send(host, ClientMessage::MoveToWorkspace { id, thread_id: thread_id.to_owned(), base });
        self.menu = None;
        cx.notify();
    }

    /// Back to the project directory; `detach` keeps the workspace on disk.
    pub fn remove_workspace(&mut self, host: &str, thread_id: &str, detach: bool, cx: &mut Context<Self>) {
        let id = self.next_id();
        self.net.send(host, ClientMessage::RemoveWorkspace { id, thread_id: thread_id.to_owned(), detach });
        self.menu = None;
        cx.notify();
    }

    /// The workspace menu's `Copy path`: the directory to the clipboard, with a toast.
    pub fn copy_path(&mut self, path: String, cx: &mut Context<Self>) {
        cx.write_to_clipboard(ClipboardItem::new_string(path));
        self.menu = None;
        self.toast("Path copied", cx);
    }

    /// Archive a thread with a workspace, after `ConfirmArchive` decided its fate.
    pub fn archive_with_workspace(&mut self, host: &str, thread_id: &str, detach: bool, cx: &mut Context<Self>) {
        self.remove_workspace(host, thread_id, detach, cx);
        // Optimistic, like the archive flag: without it `set_archived` would ask again.
        if let Some(t) = self.host_mut(host).and_then(|h| h.threads.iter_mut().find(|t| t.id == thread_id)) {
            t.workspace = None;
        }
        self.set_archived(host, thread_id, true, cx);
    }

    /// Re-render once a second, for as long as the app lives, so every clock on screen
    /// (run and tool timers, `rel_time` ages) advances in whole-second steps. Also keeps
    /// the usage page's numbers fresh while it is open.
    fn tick(&mut self, cx: &mut Context<Self>) {
        cx.spawn(async move |this, cx| {
            loop {
                cx.background_executor().timer(Duration::from_secs(1)).await;
                let alive = this.update(cx, |this, cx| {
                    this.ticks += 1;
                    if this.ticks % USAGE_REFRESH_SECS == 0 && matches!(this.pane, Pane::Usage) && this.usage.at_now() {
                        this.request_usage(cx);
                    }
                    if this.watch.changed() {
                        this.reload_config(cx);
                    }
                    this.titling.retain(|_, since| since.elapsed() < TITLING_TTL);
                    // The repo changes under us (commits, rebases in a terminal): keep what the
                    // draft page and the workspace menus show fresh while they are on screen.
                    if this.ticks.is_multiple_of(VCS_REFRESH_SECS) {
                        match &this.pane {
                            Pane::Draft { .. } => this.refresh_draft_vcs(),
                            // A thread's chips show whether the checkout is clean (dirty greys
                            // `in project` out), so they are kept fresh too.
                            Pane::Thread(t) => {
                                let (host, id) = (t.host.clone(), t.id.clone());
                                this.request_thread_vcs(&host, &id);
                            }
                            _ => {}
                        }
                    }
                    cx.notify()
                });
                if alive.is_err() {
                    break;
                }
            }
        })
        .detach();
    }

    fn focus_composer(&self, cx: &mut Context<Self>) {
        self.focus(self.composer.focus_handle(cx), cx);
    }

    /// Focus `handle`; deferred because callers may not have the window at hand.
    fn focus(&self, handle: FocusHandle, cx: &mut Context<Self>) {
        let window = self.window;
        cx.defer(move |cx| {
            window.update(cx, |_, window, cx| window.focus(&handle, cx)).ok();
        });
    }

    // ---- threads ----

    /// Open the new-thread page. `target` defaults to the project in view, then the last one used.
    pub fn new_draft(&mut self, target: Option<Target>, cx: &mut Context<Self>) {
        let target = target.or_else(|| self.context_target()).or_else(|| self.last_target()).filter(|t| self.target_valid(t));
        self.set_pane(Pane::Draft { target }, cx);
        self.menu = None;
        self.base = None;
        self.selection.clear();
        self.refresh_draft_vcs();
        self.focus_composer(cx);
        cx.notify();
    }

    pub fn new_thread(&mut self, _: &NewThread, _: &mut Window, cx: &mut Context<Self>) {
        self.new_draft(None, cx);
    }

    /// The project implied by the current pane: the open thread's, or the project page's.
    fn context_target(&self) -> Option<Target> {
        match &self.pane {
            Pane::Thread(t) => {
                let pid = self.summary(&t.host, &t.id)?.project_id.clone()?;
                Some(Target { host: t.host.clone(), project_id: pid })
            }
            Pane::Draft { target } => target.clone(),
            Pane::Project { group_key } => {
                let (groups, _) = self.groups();
                let group = groups.into_iter().find(|g| g.key == *group_key)?;
                Some(group.member_for(None).into())
            }
            _ => None,
        }
    }

    fn last_target(&self) -> Option<Target> {
        let v = self.cache.lock().unwrap().get(LAST_TARGET_KEY)?;
        let (host, project_id) = v.split_once('\n')?;
        Some(Target { host: host.into(), project_id: project_id.into() })
    }

    fn target_valid(&self, t: &Target) -> bool {
        self.host(&t.host).is_some_and(|h| h.projects.iter().any(|p| p.id == t.project_id))
    }

    pub fn set_target(&mut self, target: Target, cx: &mut Context<Self>) {
        if let Pane::Draft { target: t } = &mut self.pane {
            *t = Some(target);
        }
        self.menu = None;
        self.refresh_draft_vcs();
        self.focus_composer(cx);
        cx.notify();
    }

    /// Ask the draft's host about its project directory (dirty? a repo at all?).
    fn refresh_draft_vcs(&mut self) {
        if let Some((t, path, _)) = self.draft_vcs() {
            let (host, path) = (t.host.clone(), path.to_owned());
            self.request_vcs(&host, &path);
        }
    }

    pub fn set_archived(&mut self, host: &str, thread_id: &str, archived: bool, cx: &mut Context<Self>) {
        // Archiving a thread that owns a workspace asks what to do with it, from its header.
        if archived && self.summary(host, thread_id).is_some_and(|t| t.workspace.is_some()) && self.menu != Some(Menu::ConfirmArchive) {
            if !self.open().is_some_and(|t| t.host == host && t.id == thread_id) {
                self.select_thread(host, thread_id, cx);
            }
            self.menu = Some(Menu::ConfirmArchive);
            self.request_thread_vcs(host, thread_id);
            cx.notify();
            return;
        }
        let id = self.next_id();
        self.net.send(host, ClientMessage::ArchiveThread { id, thread_id: thread_id.to_owned(), archived });
        // Optimistic: the host's `Threads` push confirms it.
        if let Some(t) = self.host_mut(host).and_then(|h| h.threads.iter_mut().find(|t| t.id == thread_id)) {
            t.archived = archived;
        }
        self.menu = None;
        cx.notify();
    }

    pub fn delete_thread(&mut self, host: &str, thread_id: &str, cx: &mut Context<Self>) {
        let id = self.next_id();
        self.net.send(host, ClientMessage::DeleteThread { id, thread_id: thread_id.to_owned() });
        if let Some(h) = self.host_mut(host) {
            h.threads.retain(|t| t.id != thread_id);
        }
        if self.open().is_some_and(|t| t.host == host && t.id == thread_id) {
            self.show(Pane::Inbox, cx);
        }
        let _ = self.cache.lock().unwrap().remove_draft(host, thread_id);
        self.menu = None;
        cx.notify();
    }

    // ---- projects ----

    /// Open the path form for a new project on `host` (default: first connected host).
    pub fn start_new_project(&mut self, host: Option<String>, cx: &mut Context<Self>) {
        let host =
            host.or_else(|| self.hosts.iter().find(|h| h.status == HostStatus::Connected).or(self.hosts.first()).map(|h| h.name.clone()));
        let Some(host) = host else { return };
        self.set_pane(Pane::NewProject { host }, cx);
        self.menu = None;
        self.focus(self.path_input.focus_handle(cx), cx);
        cx.notify();
    }

    fn add_project(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Pane::NewProject { host } = &self.pane else {
            return;
        };
        let host = host.clone();
        let path = self.path_input.read(cx).text().trim().to_owned();
        if path.is_empty() {
            return;
        }
        let id = self.next_id();
        self.net.send(&host, ClientMessage::AddProject { id, path });
        self.path_input.update(cx, |input, cx| input.set_text("", window, cx));
    }

    // ---- hosts ----

    /// The settings page; opening it also picks up hand edits to `hosts.toml`.
    pub fn show_settings(&mut self, cx: &mut Context<Self>) {
        self.show(Pane::Settings, cx);
        self.sync_hosts(cx);
        self.focus(self.host_name_input.focus_handle(cx), cx);
    }

    // ---- config ----

    /// The settings gear lands here: `config.toml`'s page.
    pub fn show_config(&mut self, cx: &mut Context<Self>) {
        self.show(Pane::Config, cx);
        self.fill_settings(cx);
    }

    /// Put the config into the Config page's fields (deferred: the fields need the window).
    fn fill_settings(&self, cx: &mut Context<Self>) {
        let (window, settings, config) = (self.window, self.settings.clone(), self.config.clone());
        cx.defer(move |cx| {
            window.update(cx, |_, window, cx| settings.fill(&config, window, cx)).ok();
        });
    }

    /// Make `config` the one in force: theme and fonts. Families that don't resolve are
    /// remembered for the page's warning marks and returned for whoever wants to toast.
    fn apply_config(&mut self, config: config::Config, cx: &mut Context<Self>) -> Vec<String> {
        let fonts_changed = config.fonts != self.config.fonts;
        self.config = config;
        match theme::load(&self.config.theme) {
            Ok(t) => self.set_theme_now(t, cx),
            Err(e) => self.toast(format!("{e:#}"), cx),
        }
        let mut missing = vec![];
        if fonts_changed {
            (self.fonts, missing) = self.config.fonts.resolve();
            self.settings.missing_fonts = missing.clone();
        }
        cx.notify();
        missing
    }

    /// A hand edit to `config.toml` or a theme file (`watch`, from `tick`): the file wins,
    /// so the page's fields are refilled too.
    fn reload_config(&mut self, cx: &mut Context<Self>) {
        match config::load() {
            Ok(c) => {
                for font in self.apply_config(c, cx) {
                    self.toast(format!("font `{font}` is not installed"), cx);
                }
                if matches!(self.pane, Pane::Config) {
                    self.fill_settings(cx);
                }
            }
            Err(e) => self.toast(format!("{e:#}"), cx),
        }
    }

    /// Change the config through `f`, write it, apply it. The write is swallowed by the
    /// watcher so it doesn't come back as a hand edit.
    fn edit_config(&mut self, f: impl FnOnce(&mut config::Config), cx: &mut Context<Self>) {
        let mut c = self.config.clone();
        f(&mut c);
        self.menu = None;
        if let Err(e) = config::save(&c) {
            return self.toast(format!("{e:#}"), cx);
        }
        self.watch.changed();
        self.apply_config(c, cx);
    }

    /// Make `t` the palette and give the window the background it needs (see-through for a
    /// translucent canvas). The window call is deferred: listeners don't hold the window.
    fn set_theme_now(&self, t: theme::Theme, cx: &mut Context<Self>) {
        theme::set(t);
        let (window, background) = (self.window, theme::background(&t));
        cx.defer(move |cx| {
            window.update(cx, |_, window, _| window.set_background_appearance(background)).ok();
        });
    }

    pub fn set_theme(&mut self, name: String, cx: &mut Context<Self>) {
        self.edit_config(|c| c.theme = name, cx);
    }

    pub fn set_title_model(&mut self, key: String, cx: &mut Context<Self>) {
        self.edit_config(|c| c.title.model = key, cx);
    }

    pub fn set_title_reasoning(&mut self, level: String, cx: &mut Context<Self>) {
        self.edit_config(|c| c.title.reasoning = level, cx);
    }

    pub fn toggle_telemetry(&mut self, cx: &mut Context<Self>) {
        self.edit_config(|c| c.telemetry = !c.telemetry, cx);
    }

    /// A switch in Settings › Config's tools section.
    pub fn toggle_tool(&mut self, name: &str, cx: &mut Context<Self>) {
        self.edit_config(
            |c| match name {
                "async_bash" => c.tools.async_bash = !c.tools.async_bash,
                "ask_user" => c.tools.ask_user = !c.tools.ask_user,
                "dirigent" => c.tools.dirigent = !c.tools.dirigent,
                _ => {}
            },
            cx,
        );
    }

    // ---- right panel ----

    /// Whether the right panel (lineage over the work log) is showing: the user's
    /// choice, else whenever there is something to show (a log or a family).
    pub fn panel_open(&self) -> bool {
        let Some(t) = self.open() else { return false };
        t.panel.unwrap_or_else(|| t.log.is_some() || !self.lineage(&t.host, &t.id).is_empty())
    }

    /// The header's panel button and `]`: close the panel, or open it on the log of the
    /// run in progress (else the last work).
    pub fn toggle_panel(&mut self, cx: &mut Context<Self>) {
        if self.panel_open() {
            self.close_panel();
        } else if let Pane::Thread(t) = &mut self.pane {
            t.panel = Some(true);
            self.log_on = true;
            self.show_latest_log();
        }
        self.focus_composer(cx);
        cx.notify();
    }

    /// Close the right panel and drop its log, so no work line stays tinted and the next
    /// open starts from the latest work.
    fn close_panel(&mut self) {
        if let Pane::Thread(t) = &mut self.pane {
            t.panel = Some(false);
            t.log = None;
            self.log_on = false;
        }
    }

    /// An open panel always shows a log: when it opened by itself (a family appeared) or
    /// had no work to show yet (a run just started), point it at the latest work. Called
    /// every frame, so every path into "open without a log" is covered.
    fn settle_panel(&mut self) {
        if self.panel_open() && self.open().is_some_and(|t| t.log.is_none()) {
            self.show_latest_log();
        }
    }

    /// The open thread's latest work: the run in progress (the tail while it has no
    /// item yet), else the last work.
    fn latest_work(&self) -> Option<usize> {
        let t = self.open()?;
        match t.timeline.open_work() {
            Some(i) => Some(i),
            None if self.is_running() => Some(thread::TAIL),
            None => t.timeline.items.iter().rposition(|i| matches!(i, Item::Work(_))),
        }
    }

    /// Point the log at the latest work, when there is one.
    fn show_latest_log(&mut self) {
        if let Some(key) = self.latest_work() {
            self.show_log(key);
        }
    }

    // ---- asks ----

    /// Make the open thread's forms match its summary's asks: a form (with a text field
    /// per question that takes one) for each new ask, forms of answered asks dropped.
    fn sync_asks(&mut self, cx: &mut Context<Self>) {
        let Pane::Thread(t) = &self.pane else { return };
        let asks = self.summary(&t.host, &t.id).map(|s| s.asks.clone()).unwrap_or_default();
        let mut forms = vec![];
        let Pane::Thread(t) = &mut self.pane else { return };
        for ask in asks {
            if let Some(i) = t.asks.iter().position(|f| f.ask.id == ask.id) {
                forms.push(t.asks.remove(i));
                continue;
            }
            let custom = ask
                .questions
                .iter()
                .enumerate()
                .map(|(qi, q)| {
                    (q.allow_custom || q.options.is_empty()).then(|| {
                        let input =
                            cx.new(|cx| TextInput::new(cx, if q.options.is_empty() { "Your answer" } else { "Something else" }, true));
                        let (ask_id, qi) = (ask.id.clone(), qi);
                        // Enter: on to the next question, or send once every one is answered.
                        cx.subscribe(&input, {
                            let ask_id = ask_id.clone();
                            move |this, _, _: &Submitted, cx| this.next_ask_tab(&ask_id, cx)
                        })
                        .detach();
                        // Typing is the answer to a single-choice question: the picked option goes.
                        if !q.multi {
                            cx.subscribe(&input, move |this, input, _: &input::Changed, cx| {
                                if !input.read(cx).text().trim().is_empty() {
                                    this.with_ask(&ask_id, cx, |f| f.picked[qi].clear());
                                }
                            })
                            .detach();
                        }
                        input
                    })
                })
                .collect();
            forms.push(AskForm { picked: vec![HashSet::new(); ask.questions.len()], custom, ask, current: 0, sent: false });
        }
        t.asks = forms;
        cx.notify();
    }

    /// Change the open thread's unsent form for `ask_id` and re-render.
    fn with_ask(&mut self, ask_id: &str, cx: &mut Context<Self>, f: impl FnOnce(&mut AskForm)) {
        if let Pane::Thread(t) = &mut self.pane
            && let Some(form) = t.asks.iter_mut().find(|f| f.ask.id == ask_id && !f.sent)
        {
            f(form);
            cx.notify();
        }
    }

    /// An option row: pick it (alone, or with the others for a multiple-choice question).
    /// On a single-choice question it also clears the typed text, which would be a second answer.
    pub fn pick_option(&mut self, ask_id: &str, question: usize, option: usize, cx: &mut Context<Self>) {
        let mut clear = None;
        self.with_ask(ask_id, cx, |form| {
            let multi = form.ask.questions[question].multi;
            let picked = &mut form.picked[question];
            if !picked.remove(&option) {
                if !multi {
                    picked.clear();
                    clear = form.custom[question].clone();
                }
                picked.insert(option);
            }
        });
        if let Some(input) = clear {
            let window = self.window;
            cx.defer(move |cx| {
                window.update(cx, |_, window, cx| input.update(cx, |i, cx| i.set_text("", window, cx))).ok();
            });
        }
        self.focus_composer(cx);
        cx.notify();
    }

    /// A tab: show that question. Its text field, if any, takes focus.
    pub fn show_ask_tab(&mut self, ask_id: &str, question: usize, cx: &mut Context<Self>) {
        let mut input = None;
        self.with_ask(ask_id, cx, |form| {
            form.current = question.min(form.ask.questions.len() - 1);
            input = form.custom[form.current].clone();
        });
        match input {
            Some(input) => self.focus(input.focus_handle(cx), cx),
            None => self.focus_composer(cx),
        }
    }

    /// `Next` (and Enter in a text field): the next unanswered question, or send once
    /// every one is answered.
    pub fn next_ask_tab(&mut self, ask_id: &str, cx: &mut Context<Self>) {
        let Pane::Thread(t) = &self.pane else { return };
        let Some(form) = t.asks.iter().find(|f| f.ask.id == ask_id && !f.sent) else { return };
        let (next, complete) = (form.next_unanswered(cx), form.complete(cx));
        if let Some(i) = next {
            self.show_ask_tab(ask_id, i, cx);
        } else if complete {
            self.answer_ask(ask_id, cx);
        }
    }

    /// Send the form's answers (`None`: dismiss it); the host hands them to the model.
    pub fn answer_ask(&mut self, ask_id: &str, cx: &mut Context<Self>) {
        self.send_ask(ask_id, true, cx);
    }

    pub fn dismiss_ask(&mut self, ask_id: &str, cx: &mut Context<Self>) {
        self.send_ask(ask_id, false, cx);
    }

    /// Also echoes the answers as a pending steer in the tail (see `Streaming::answers`).
    fn send_ask(&mut self, ask_id: &str, answer: bool, cx: &mut Context<Self>) {
        let id = self.next_id();
        let Pane::Thread(t) = &mut self.pane else { return };
        let Some(form) = t.asks.iter_mut().find(|f| f.ask.id == ask_id && !f.sent) else { return };
        form.sent = true;
        let answers = answer.then(|| form.answers(cx));
        t.streaming.echo_answer(ask_id, &form.ask.questions, answers.as_deref());
        let tail = t.timeline.items.len();
        t.list.remeasure_items(tail..tail + 1);
        let (host, thread_id) = (t.host.clone(), t.id.clone());
        self.net.send(&host, ClientMessage::Answer { id, thread_id, ask_id: ask_id.to_owned(), answers });
        self.focus_composer(cx);
        cx.notify();
    }

    /// A keystroke in a text setting: apply all of them 300ms after the last one.
    fn schedule_apply(&mut self, cx: &mut Context<Self>) {
        self.config_edit += 1;
        let edit = self.config_edit;
        cx.spawn(async move |this, cx| {
            cx.background_executor().timer(Duration::from_millis(300)).await;
            this.update(cx, |this, cx| {
                if this.config_edit == edit {
                    this.apply_config_fields(cx);
                }
            })
            .ok();
        })
        .detach();
    }

    /// Write every text setting as it stands. An emptied field keeps its old value (the
    /// user is mid-edit; the field is not refilled under them).
    fn apply_config_fields(&mut self, cx: &mut Context<Self>) {
        let read = |i: &Entity<TextInput>| i.read(cx).text().trim().to_owned();
        let s = &self.settings;
        let (ui, prose, code, prompt) = (read(&s.font_ui), read(&s.font_prose), read(&s.font_code), read(&s.prompt));
        self.edit_config(
            |c| {
                for (slot, value) in
                    [(&mut c.fonts.ui, ui), (&mut c.fonts.prose, prose), (&mut c.fonts.code, code), (&mut c.title.prompt, prompt)]
                {
                    if !value.is_empty() {
                        *slot = value;
                    }
                }
            },
            cx,
        );
    }

    pub fn toggle_title_test(&mut self, cx: &mut Context<Self>) {
        self.settings.testing = !self.settings.testing;
        if self.settings.testing {
            self.focus(self.settings.sample.focus_handle(cx), cx);
        }
        cx.notify();
    }

    /// Name the sample message with the current title settings, on the local host.
    pub fn run_title_test(&mut self, cx: &mut Context<Self>) {
        if self.settings.test_request.is_some() {
            return;
        }
        let text = self.settings.sample.read(cx).text().trim().to_owned();
        if text.is_empty() {
            return;
        }
        let id = self.next_id();
        self.settings.test_request = Some((id, Instant::now()));
        self.settings.test_result = None;
        self.net.send(hosts::LOCAL, ClientMessage::GenerateTitle { id, title: self.config.title.clone(), text });
        cx.notify();
    }

    /// The thread's title is still being generated (its first prompt was sent recently and
    /// no name has come back yet).
    pub fn titling(&self, host: &str, id: &str) -> bool {
        self.titling.contains_key(&(host.to_owned(), id.to_owned()))
    }

    /// Make the connections match `hosts.toml`: drop hosts that are gone or whose url
    /// changed (with everything cached for them), connect the new ones.
    fn sync_hosts(&mut self, cx: &mut Context<Self>) {
        let wanted = match hosts::load() {
            Ok(h) => h,
            Err(e) => return self.toast(format!("{e:#}"), cx),
        };
        let stale: Vec<String> =
            self.hosts.iter().filter(|h| h.name != hosts::LOCAL && wanted.get(&h.name) != Some(&h.url)).map(|h| h.name.clone()).collect();
        for name in stale {
            self.net.remove(&name);
            self.hosts.retain(|h| h.name != name);
            self.links.retain(|(h, _), _| *h != name);
            // Leave the thread first: its draft would otherwise be saved back after the wipe.
            if self.open().is_some_and(|t| t.host == name) {
                self.set_pane(Pane::Settings, cx);
            }
            let _ = self.cache.lock().unwrap().remove_host(&name);
        }
        for (name, url) in wanted {
            if self.host(&name).is_none() {
                self.hosts.push(host_state(&self.cache.lock().unwrap(), &name, &url));
                self.net.add(name, url);
            }
        }
        cx.notify();
    }

    fn add_host(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let name = self.host_name_input.read(cx).text().trim().to_owned();
        let url = self.host_url_input.read(cx).text().trim().to_owned();
        if name.is_empty() || url.is_empty() {
            return;
        }
        if name == hosts::LOCAL {
            return self.toast("`local` is this machine", cx);
        }
        let url = match hosts::normalize(&url) {
            Ok(u) => u,
            Err(e) => return self.toast(e.to_string(), cx),
        };
        self.edit_hosts(
            |h| {
                h.insert(name, url);
            },
            cx,
        );
        for input in [&self.host_name_input, &self.host_url_input] {
            input.update(cx, |input, cx| input.set_text("", window, cx));
        }
        self.focus(self.host_name_input.focus_handle(cx), cx);
    }

    pub fn remove_host(&mut self, name: &str, cx: &mut Context<Self>) {
        let name = name.to_owned();
        self.edit_hosts(
            |h| {
                h.remove(&name);
            },
            cx,
        );
    }

    /// Rewrite `hosts.toml` through `f`, then apply it.
    fn edit_hosts(&mut self, f: impl FnOnce(&mut std::collections::BTreeMap<String, String>), cx: &mut Context<Self>) {
        let result = hosts::load().and_then(|mut h| {
            f(&mut h);
            hosts::save(&h)
        });
        match result {
            Ok(()) => self.sync_hosts(cx),
            Err(e) => self.toast(format!("{e:#}"), cx),
        }
    }

    pub fn remove_project(&mut self, host: &str, project_id: &str, cx: &mut Context<Self>) {
        let id = self.next_id();
        self.net.send(host, ClientMessage::RemoveProject { id, project_id: project_id.to_owned() });
        let _ = self.cache.lock().unwrap().set_link(host, project_id, None);
        self.links.remove(&(host.to_owned(), project_id.to_owned()));
        self.menu = None;
        cx.notify();
    }

    /// Link every member of `group_key` into `target_key` (or clear their manual links).
    pub fn link_group(&mut self, group_key: &str, target_key: Option<&str>, cx: &mut Context<Self>) {
        let (groups, _) = self.groups();
        let Some(group) = groups.iter().find(|g| g.key == group_key) else {
            return;
        };
        let cache = self.cache.lock().unwrap();
        for m in &group.members {
            let _ = cache.set_link(&m.host, &m.project.id, target_key);
            let k = (m.host.clone(), m.project.id.clone());
            match target_key {
                Some(t) => {
                    self.links.insert(k, t.to_owned());
                }
                None => {
                    self.links.remove(&k);
                }
            }
        }
        self.menu = None;
        cx.notify();
    }

    /// The sidebar's project groups and the orphan threads.
    pub fn groups(&self) -> (Vec<projects::Group>, Vec<(String, ThreadSummary)>) {
        projects::groups(&self.hosts, &self.links, &self.project_order)
    }

    /// Put the group at `index` of the sidebar's project list (the drag's live reorder),
    /// freezing the whole list in its current order so nothing else moves.
    pub fn move_project(&mut self, group_key: &str, index: usize, cx: &mut Context<Self>) {
        let mut keys: Vec<String> = self.groups().0.into_iter().map(|g| g.key).collect();
        let Some(from) = keys.iter().position(|k| k == group_key) else { return };
        if from == index {
            return;
        }
        let key = keys.remove(from);
        keys.insert(index.min(keys.len()), key);
        if let Ok(json) = serde_json::to_string(&keys) {
            let _ = self.cache.lock().unwrap().set(PROJECT_ORDER_KEY, &json);
        }
        self.project_order = keys;
        cx.notify();
    }

    // ---- usage ----

    pub fn show_usage(&mut self, cx: &mut Context<Self>) {
        self.show(Pane::Usage, cx);
        self.request_usage(cx);
    }

    /// Ask every host for the window shown. Replies to older requests are ignored. The
    /// buckets already shown stay up until each host's reply replaces them, so a refresh
    /// of unchanged data is invisible.
    fn request_usage(&mut self, cx: &mut Context<Self>) {
        let id = self.next_id();
        self.usage.request = Some(id);
        let (since, until, bucket_ms) = (self.usage.since, self.usage.until, self.usage.bucket());
        for h in &self.hosts {
            self.net.send(&h.name, ClientMessage::ListUsage { id, since, until, bucket_ms });
        }
        cx.notify();
    }

    pub fn set_usage_range(&mut self, range: usage::Range, cx: &mut Context<Self>) {
        if range == usage::Range::Custom {
            // Seed the fields with the window shown, so Enter alone keeps it.
            let (from, to) = (usage::day_of(self.usage.since), usage::day_of(self.usage.until - 1));
            let (window, from_input, to_input) = (self.window, self.usage_from.clone(), self.usage_to.clone());
            cx.defer(move |cx| {
                window
                    .update(cx, |_, window, cx| {
                        from_input.update(cx, |i, cx| i.set_text(&from, window, cx));
                        to_input.update(cx, |i, cx| i.set_text(&to, window, cx));
                    })
                    .ok();
            });
            self.usage.range = range;
            self.focus(self.usage_from.focus_handle(cx), cx);
            cx.notify();
            return;
        }
        self.usage.set_range(range);
        self.request_usage(cx);
    }

    /// Enter in a date field: show `from..=to` (both `YYYY-MM-DD`).
    fn apply_custom_usage(&mut self, cx: &mut Context<Self>) {
        let parse = |input: &Entity<TextInput>, cx: &App| usage::parse_day(input.read(cx).text());
        match (parse(&self.usage_from, cx), parse(&self.usage_to, cx)) {
            (Some(from), Some(to)) => {
                self.usage.set_custom(from, to);
                self.request_usage(cx);
            }
            _ => self.toast("Dates as YYYY-MM-DD", cx),
        }
    }

    /// `‹` / `›`: the previous / next window of the same length.
    pub fn page_usage(&mut self, dir: i64, cx: &mut Context<Self>) {
        self.usage.page(dir);
        self.request_usage(cx);
    }

    pub fn set_usage_group(&mut self, group: usage::Group, cx: &mut Context<Self>) {
        self.usage.group = group;
        self.menu = None;
        cx.notify();
    }

    /// A legend row was clicked: hide its line, or show it again.
    pub fn toggle_usage_series(&mut self, key: &str, cx: &mut Context<Self>) {
        if !self.usage.hidden.remove(key) {
            self.usage.hidden.insert(key.to_owned());
        }
        cx.notify();
    }

    // ---- composer ----

    pub fn set_model(&mut self, key: String, cx: &mut Context<Self>) {
        // Keep the effort if the new model supports it, else "high" (or its highest level).
        let levels = self.models().iter().find(|m| m.key() == key).map(|m| m.thinking_levels.clone()).unwrap_or_default();
        if !self.effort.as_ref().is_some_and(|e| levels.contains(e)) {
            self.effort = levels.iter().find(|l| *l == "high").or(levels.last()).cloned();
        }
        self.model = Some(key);
        self.remember_model();
        self.menu = None;
        self.focus_composer(cx);
        cx.notify();
    }

    pub fn set_effort(&mut self, level: String, cx: &mut Context<Self>) {
        self.effort = Some(level);
        self.remember_model();
        self.menu = None;
        self.focus_composer(cx);
        cx.notify();
    }

    fn remember_model(&self) {
        let cache = self.cache.lock().unwrap();
        if let Some(m) = &self.model {
            let _ = cache.set(LAST_MODEL_KEY, m);
        }
        if let Some(e) = &self.effort {
            let _ = cache.set(LAST_EFFORT_KEY, e);
        }
    }

    // ---- model picker ----

    /// The current model + effort, if a model is picked.
    pub fn current_preset(&self) -> Option<Preset> {
        self.model.clone().map(|model| Preset { model, effort: self.effort.clone() })
    }

    pub fn use_preset(&mut self, p: &Preset, cx: &mut Context<Self>) {
        self.model = Some(p.model.clone());
        self.effort = p.effort.clone();
        self.remember_model();
        self.menu = None;
        self.focus_composer(cx);
        cx.notify();
    }

    /// Save the current combo (no-op without a model or when it's already saved).
    pub fn save_preset(&mut self, cx: &mut Context<Self>) {
        if let Some(p) = self.current_preset()
            && !self.presets.contains(&p)
        {
            self.presets.push(p);
            self.store_presets();
            cx.notify();
        }
    }

    pub fn remove_preset(&mut self, i: usize, cx: &mut Context<Self>) {
        if i < self.presets.len() {
            self.presets.remove(i);
            self.store_presets();
            cx.notify();
        }
    }

    fn store_presets(&self) {
        if let Ok(json) = serde_json::to_string(&self.presets) {
            let _ = self.cache.lock().unwrap().set(PRESETS_KEY, &json);
        }
    }

    /// Flip the model picker to the all-models page with an empty, focused search.
    pub fn open_all_models(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.model_search.update(cx, |input, cx| input.set_text("", window, cx));
        self.menu = Some(Menu::AllModels);
        self.focus(self.model_search.focus_handle(cx), cx);
        cx.notify();
    }

    /// Host models matching every word of the search, in host order.
    pub fn filtered_models(&self, cx: &App) -> Vec<&ModelInfo> {
        let query = self.model_search.read(cx).text().to_lowercase();
        let words: Vec<&str> = query.split_whitespace().collect();
        self.models()
            .iter()
            .filter(|m| {
                let hay = format!("{} {}", m.name, m.key()).to_lowercase();
                words.iter().all(|w| hay.contains(w))
            })
            .collect()
    }

    /// Enter in the search: pick the top match.
    fn pick_first_model(&mut self, cx: &mut Context<Self>) {
        if let Some(key) = self.filtered_models(cx).first().map(|m| m.key()) {
            self.set_model(key, cx);
        }
    }

    // ---- attachments ----

    /// The pasted image gets the next number in this draft and its token goes in at the
    /// cursor (which saves the draft, images included, through `Changed`).
    fn attach(&mut self, image: gpui::Image, window: &mut Window, cx: &mut Context<Self>) {
        let n = self.attachments.iter().map(|a| a.n).max().unwrap_or(0) + 1;
        self.attachments.push(Attachment { n, image: Arc::new(image) });
        self.composer.update(cx, |input, cx| input.replace_text_in_range(None, &format!("{} ", image_token(n)), window, cx));
        cx.notify();
    }

    /// Attachments whose token is still in the composer, in paste order.
    pub fn attachments(&self, cx: &App) -> Vec<Attachment> {
        let text = self.composer.read(cx).text();
        self.attachments.iter().filter(|a| text.contains(&image_token(a.n))).cloned().collect()
    }

    /// The ✕ on a thumbnail: drop the image and its token from the text.
    pub fn remove_attachment(&mut self, n: u32, window: &mut Window, cx: &mut Context<Self>) {
        self.attachments.retain(|a| a.n != n);
        let token = image_token(n);
        self.composer.update(cx, |input, cx| {
            input.remove_all(&format!("{token} "), window, cx);
            input.remove_all(&token, window, cx);
        });
        self.save_draft(cx);
        self.focus_composer(cx);
        cx.notify();
    }

    /// Write what the composer holds under the thread (or draft) in view. Called on every
    /// edit and when leaving the pane; an empty composer deletes the row instead, so a
    /// draft that was typed and erased leaves nothing behind (as does leaving a thread
    /// that was deleted under us).
    fn save_draft(&self, cx: &App) {
        let Some((host, id)) = composer_key(&self.pane) else { return };
        let content = self.composer.read(cx).content();
        let gone = (host, id) != DRAFT_KEY && self.summary(host, id).is_none();
        let mut cache = self.cache.lock().unwrap();
        if gone || content.text.trim().is_empty() {
            let _ = cache.remove_draft(host, id);
        } else {
            // Images are kept while their token is in the text; `attachments()` prunes the rest.
            let _ = cache.set_draft(host, id, &content, &image_rows(&self.attachments));
        }
    }

    /// Fill the composer with the saved draft of the thread (or draft page) in view, or
    /// empty it. The text goes in deferred, as `set_content` needs the window.
    fn load_composer(&mut self, cx: &mut Context<Self>) {
        let Some((host, id)) = composer_key(&self.pane) else { return };
        let (content, images) = self.cache.lock().unwrap().draft(host, id).unwrap_or_default();
        self.attachments =
            images.into_iter().filter_map(|(n, mime, bytes)| Some(Attachment { n, image: image_from(&mime, bytes)? })).collect();
        let (window, composer) = (self.window, self.composer.clone());
        cx.defer(move |cx| {
            window.update(cx, |_, window, cx| composer.update(cx, |input, cx| input.set_content(content, window, cx))).ok();
        });
    }

    /// Make sure every image the open thread shows is in `blobs`: from the cache db, else
    /// asked of the host (once).
    fn fetch_blobs(&mut self) {
        let Pane::Thread(t) = &self.pane else { return };
        let host = t.host.clone();
        let ids: Vec<String> = view::blobs_shown(&t.timeline.items)
            .into_iter()
            .filter(|id| !self.blobs.contains_key(id) && !self.blob_requests.contains(id))
            .collect();
        for id in ids {
            let cached = self.cache.lock().unwrap().blob(&id);
            match cached.and_then(|(mime, bytes)| blob_from(&mime, bytes)) {
                Some(blob) => {
                    self.blobs.insert(id, blob);
                }
                None => {
                    let rid = self.next_id();
                    self.blob_requests.insert(id.clone());
                    self.net.send(&host, ClientMessage::GetBlob { id: rid, blob_id: id });
                }
            }
        }
    }

    pub fn set_preview(&mut self, image: Option<Arc<gpui::Image>>, cx: &mut Context<Self>) {
        self.preview = image;
        if self.preview.is_none() {
            self.focus_composer(cx);
        }
        cx.notify();
    }

    /// Submit from the composer: prompt the open thread, or create one for the draft.
    /// While a run is active, `queued` sends a follow-up instead of steering.
    fn send(&mut self, queued: bool, window: &mut Window, cx: &mut Context<Self>) {
        let text = self.composer.read(cx).text().trim().to_owned();
        if text.is_empty() {
            return;
        }
        let images = self.attachments(cx);
        if let Pane::Draft { target } = &self.pane {
            let Some(target) = target.clone() else {
                self.toast("Pick a project first", cx);
                self.menu = Some(Menu::Target);
                return;
            };
            if self.model.is_none() {
                self.toast("Pick a model first", cx);
                self.menu = Some(Menu::Model);
                return;
            }
            if self.pending.is_some() {
                return;
            }
            let _ = self.cache.lock().unwrap().set(LAST_TARGET_KEY, &format!("{}\n{}", target.host, target.project_id));
            // Isolation is only on offer for a repo (the chip hides otherwise).
            let workspace = self.isolate && self.draft_vcs().is_some_and(|(_, _, v)| v.is_some_and(|v| v.vcs.is_some()));
            let base = self.base.clone().filter(|_| workspace);
            let id = self.next_id();
            self.pending = Some((id, text, images));
            self.clear_composer(window, cx);
            self.net.send(
                &target.host,
                ClientMessage::CreateThread {
                    id,
                    project_id: target.project_id,
                    workspace,
                    base,
                    parent: None,
                    name: None,
                    report: Default::default(),
                    notify: Default::default(),
                },
            );
            return;
        }
        if let Pane::Thread(t) = &self.pane {
            if self.model.is_none() {
                self.toast("Pick a model first", cx);
                self.menu = Some(Menu::Model);
                return;
            }
            if self.summary(&t.host, &t.id).is_some_and(|s| s.workspace_missing) {
                self.toast("The thread's workspace is gone; recreate it or move the thread back first", cx);
                return;
            }
            let (host, thread_id) = (t.host.clone(), t.id.clone());
            let mode = match (self.is_running(), queued) {
                (false, _) => PromptMode::Prompt,
                (true, false) => PromptMode::Steer,
                (true, true) => PromptMode::FollowUp,
            };
            self.clear_composer(window, cx);
            self.prompt(&host, &thread_id, text, images, mode, cx);
        }
    }

    /// Sent: the composer and the saved draft of the thread in view are done with.
    fn clear_composer(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.attachments.clear();
        self.composer.update(cx, |input, cx| {
            input.take(window, cx);
        });
        if let Some((host, id)) = composer_key(&self.pane) {
            let _ = self.cache.lock().unwrap().remove_draft(host, id);
        }
    }

    fn prompt(&mut self, host: &str, thread_id: &str, text: String, images: Vec<Attachment>, mode: PromptMode, cx: &mut Context<Self>) {
        let id = self.next_id();
        // A thread's first prompt also names it (see `config.toml` `[title]`).
        let first = mode == PromptMode::Prompt && self.summary(host, thread_id).is_some_and(|s| s.entry_count == 0);
        let title: Option<TitleGen> = first.then(|| self.config.title.clone());
        if first {
            self.titling.insert((host.to_owned(), thread_id.to_owned()), Instant::now());
        }
        let images = images
            .iter()
            .map(|a| ImageAttachment {
                mime_type: a.image.format.mime_type().to_owned(),
                data: base64::engine::general_purpose::STANDARD.encode(&a.image.bytes),
            })
            .collect();
        self.net.send(
            host,
            ClientMessage::Prompt {
                id,
                thread_id: thread_id.to_owned(),
                text: text.clone(),
                mode,
                model: self.model.clone(),
                thinking_level: self.effort.clone(),
                images,
                title,
                tools: Some(self.config.tools.enabled()),
            },
        );
        self.remember_model();
        // Echo the message in the tail until the host's persisted entry replaces it.
        if let Pane::Thread(t) = &mut self.pane
            && t.host == host
            && t.id == thread_id
        {
            t.streaming.echo.push((text, mode));
            let tail = t.timeline.items.len();
            t.list.remeasure_items(tail..tail + 1);
            // An open log follows the new run.
            if t.log.is_some() {
                t.log = Some(t.timeline.open_work().unwrap_or(thread::TAIL));
                t.log_list.set_follow_mode(FollowMode::Tail);
            }
        }
        cx.notify();
    }

    /// Escape: close the image preview or the open menu, drop the text selection, else
    /// stop the running agent, else close the log panel.
    pub fn abort(&mut self, _: &Abort, _: &mut Window, cx: &mut Context<Self>) {
        if self.preview.is_some() {
            self.set_preview(None, cx);
            return;
        }
        if self.menu.is_some() {
            self.close_menu(cx);
            return;
        }
        if self.selection.clear() {
            cx.notify();
            return;
        }
        if self.is_running() {
            self.stop(cx);
        } else if self.panel_open() {
            self.close_panel();
            cx.notify();
        }
    }

    // ---- sidebar ----

    /// `[`. The composer is focused almost always, so the key only types a bracket when
    /// the focused input already has text; in an empty one it toggles the sidebar.
    pub fn toggle_sidebar(&mut self, _: &ToggleSidebar, window: &mut Window, cx: &mut Context<Self>) {
        if self.typed_bracket("[", window, cx) {
            return;
        }
        self.set_sidebar_hidden(!self.sidebar_hidden, cx);
    }

    pub fn set_sidebar_hidden(&mut self, hidden: bool, cx: &mut Context<Self>) {
        self.sidebar_hidden = hidden;
        self.sidebar_peek = false;
        cx.notify();
    }

    /// `]`: toggle the right panel (see `toggle_panel`).
    pub fn toggle_log_key(&mut self, _: &ToggleLog, window: &mut Window, cx: &mut Context<Self>) {
        if self.typed_bracket("]", window, cx) {
            return;
        }
        self.toggle_panel(cx);
    }

    /// A bracket key with text in the focused input types the bracket instead of acting.
    fn typed_bracket(&mut self, bracket: &str, window: &mut Window, cx: &mut Context<Self>) -> bool {
        let mut inputs = vec![
            self.composer.clone(),
            self.path_input.clone(),
            self.host_name_input.clone(),
            self.host_url_input.clone(),
            self.model_search.clone(),
            self.base_search.clone(),
            self.usage_from.clone(),
            self.usage_to.clone(),
        ];
        if let Pane::Thread(t) = &self.pane {
            inputs.extend(t.asks.iter().flat_map(|f| f.custom.iter().flatten().cloned()));
        }
        for input in inputs {
            if input.focus_handle(cx).is_focused(window) && !input.read(cx).text().is_empty() {
                input.update(cx, |i, cx| i.replace_text_in_range(None, bracket, window, cx));
                return true;
            }
        }
        false
    }

    pub fn toggle_section(&mut self, section: Section, cx: &mut Context<Self>) {
        if !self.collapsed.remove(&section) {
            self.collapsed.insert(section);
        }
        cx.notify();
    }

    pub fn set_peek(&mut self, peek: bool, cx: &mut Context<Self>) {
        if self.sidebar_peek != peek {
            self.sidebar_peek = peek;
            cx.notify();
        }
    }

    pub fn start_resize(&mut self, what: Resize, cx: &mut Context<Self>) {
        self.resizing = Some(what);
        cx.notify();
    }

    fn on_mouse_move(&mut self, ev: &MouseMoveEvent, window: &mut Window, cx: &mut Context<Self>) {
        if self.selection.is_dragging() {
            self.drag_selection(ev.position, cx);
            return;
        }
        match self.resizing {
            Some(Resize::Sidebar) => self.sidebar_width = f32::from(ev.position.x).clamp(SIDEBAR_MIN, SIDEBAR_MAX),
            Some(Resize::Log) => {
                // The panel's right edge is the main panel's: the window edge minus the gutter and border.
                let right = f32::from(window.viewport_size().width) - theme::GUTTER - 1.;
                self.log_width = (right - f32::from(ev.position.x)).clamp(LOG_MIN, LOG_MAX);
            }
            None => return,
        }
        cx.notify();
    }

    /// A press that no text block took (see `select::Block`) drops the selection.
    fn on_mouse_down(&mut self, _: &gpui::MouseDownEvent, _: &mut Window, cx: &mut Context<Self>) {
        if !self.selection.is_dragging() && self.selection.clear() {
            cx.notify();
        }
    }

    fn on_mouse_up(&mut self, _: &MouseUpEvent, _: &mut Window, cx: &mut Context<Self>) {
        if self.dragging.take().is_some() {
            cx.notify();
        }
        if self.selection.is_dragging() {
            self.selection.end_drag();
            // Primary selection (middle click), as any text widget on Linux.
            if let Some(text) = self.selected_text(cx) {
                cx.write_to_primary(ClipboardItem::new_string(text));
            }
            return;
        }
        let (key, width) = match self.resizing.take() {
            Some(Resize::Sidebar) => (SIDEBAR_WIDTH_KEY, self.sidebar_width),
            Some(Resize::Log) => (LOG_WIDTH_KEY, self.log_width),
            None => return,
        };
        let _ = self.cache.lock().unwrap().set(key, &width.round().to_string());
        cx.notify();
    }

    /// Extend the selection to the mouse, scrolling its document when the mouse is past
    /// the document's edge.
    fn drag_selection(&mut self, p: Point<Pixels>, cx: &mut Context<Self>) {
        if let (Some(doc), Pane::Thread(t)) = (self.selection.doc(), &self.pane) {
            let list = match doc {
                Doc::Thread => &t.list,
                Doc::Log => &t.log_list,
            };
            let viewport = list.viewport_bounds();
            let overshoot = if p.y < viewport.top() { p.y - viewport.top() } else { (p.y - viewport.bottom()).max(px(0.)) };
            if overshoot != px(0.) {
                list.scroll_by(overshoot * 0.3);
                cx.notify();
            }
        }
        if self.selection.drag_to(p) {
            cx.notify();
        }
    }

    /// The selected transcript / log text. The blocks register their text when they are
    /// constructed, so the selected items are constructed again here, layout-free, to read
    /// the off-screen ones too.
    fn selected_text(&mut self, cx: &mut Context<Self>) -> Option<String> {
        let (start, end) = self.selection.range()?;
        self.selection.collect();
        match start.key.doc {
            Doc::Thread => {
                for ix in start.key.item..=end.key.item {
                    thread::render_item(self, ix, cx);
                }
            }
            Doc::Log => {
                if let Some((work, live)) = thread::shown_work(self) {
                    for six in start.key.item..=end.key.item {
                        thread::render_step(self, &work, live, six, cx);
                    }
                }
            }
        }
        Some(self.selection.text()).filter(|t| !t.is_empty())
    }

    /// ctrl+C with no input selection to copy (see `TextInput::copy`).
    fn copy_selection(&mut self, _: &input::Copy, _: &mut Window, cx: &mut Context<Self>) {
        if let Some(text) = self.selected_text(cx) {
            cx.write_to_clipboard(ClipboardItem::new_string(text));
        }
    }

    /// A message's copy button: its markdown to the clipboard, and a check on the button
    /// for a moment.
    pub fn copy_item(&mut self, ix: usize, text: String, cx: &mut Context<Self>) {
        cx.write_to_clipboard(ClipboardItem::new_string(text));
        let Pane::Thread(t) = &mut self.pane else { return };
        t.copied = Some(ix);
        cx.spawn(async move |this, cx| {
            cx.background_executor().timer(COPIED_TTL).await;
            this.update(cx, |this, cx| {
                if let Pane::Thread(t) = &mut this.pane
                    && t.copied == Some(ix)
                {
                    t.copied = None;
                    cx.notify();
                }
            })
            .ok();
        })
        .detach();
        cx.notify();
    }

    /// A ruler mark was clicked: scroll the transcript so that item is in the middle of
    /// the viewport. It goes to the top now; `settle_jump` centers it once it's measured.
    pub fn jump_to(&mut self, ix: usize, cx: &mut Context<Self>) {
        let Pane::Thread(t) = &mut self.pane else { return };
        t.list.scroll_to(ListOffset { item_ix: ix, offset_in_item: px(0.) });
        t.center_on = Some((ix, 10));
        // The click blurred the composer; keep typing working.
        self.focus_composer(cx);
        cx.notify();
    }

    /// Each frame after a `jump_to`: shift the item to the viewport's middle (a tall one
    /// stays at its top). Items above it may be unmeasured (0px to `scroll_by`) until
    /// they've been laid out, so this nudges and re-checks until it lands, a few frames
    /// at most.
    fn settle_jump(&mut self, window: &Window) {
        let Pane::Thread(t) = &mut self.pane else { return };
        let Some((ix, tries)) = t.center_on.take() else { return };
        let vp = t.list.viewport_bounds();
        if let Some(b) = t.list.bounds_for_item(ix) {
            let target = vp.top() + ((vp.size.height - b.size.height) / 2.).max(px(0.));
            let delta = b.top() - target;
            if delta.abs() < px(1.) {
                return;
            }
            t.list.scroll_by(delta);
        }
        if tries > 0 {
            t.center_on = Some((ix, tries - 1));
            window.request_animation_frame();
        }
    }

    /// A message's pin button: toggle it (the ruler shows pinned marks in amber). The host
    /// keeps the pin, by the entry id behind the item; the open thread's set and the
    /// summary are updated ahead of its `Threads` push.
    pub fn toggle_pin(&mut self, ix: usize, cx: &mut Context<Self>) {
        let id = self.next_id();
        let Pane::Thread(t) = &mut self.pane else { return };
        let Some(entry_id) = t.timeline.entry_id(ix).filter(|e| !e.is_empty()).map(str::to_owned) else { return };
        let pinned = !t.pins.remove(&entry_id);
        if pinned {
            t.pins.insert(entry_id.clone());
        }
        let (host, thread_id) = (t.host.clone(), t.id.clone());
        if let Some(s) = self.summary_mut(&host, &thread_id) {
            s.pinned_entries.retain(|e| *e != entry_id);
            if pinned {
                s.pinned_entries.push(entry_id.clone());
            }
        }
        self.net.send(&host, ClientMessage::PinMessage { id, thread_id, entry_id, pinned });
        cx.notify();
    }

    /// Drop a queued steer / follow-up the model hasn't seen yet. Pi confirms with a
    /// `queue_update`; the echo goes now so the row doesn't linger for the round trip.
    pub fn cancel_queued(&mut self, text: String, cx: &mut Context<Self>) {
        let id = self.next_id();
        if let Pane::Thread(t) = &mut self.pane {
            if let Some(i) = t.streaming.echo.iter().position(|(e, _)| *e == text) {
                t.streaming.echo.remove(i);
            }
            let tail = t.timeline.items.len();
            t.list.remeasure_items(tail..tail + 1);
            self.net.send(&t.host, ClientMessage::CancelQueued { id, thread_id: t.id.clone(), text });
            cx.notify();
        }
    }

    /// Stop the open thread's run.
    pub fn stop(&mut self, cx: &mut Context<Self>) {
        if let Pane::Thread(t) = &self.pane
            && self.is_running()
        {
            let (host, thread_id) = (t.host.clone(), t.id.clone());
            let id = self.next_id();
            self.net.send(&host, ClientMessage::Abort { id, thread_id });
            cx.notify();
        }
    }

    pub fn toggle_menu(&mut self, menu: Menu, cx: &mut Context<Self>) {
        if self.menu_dismissed {
            return;
        }
        self.menu = if self.menu.as_ref() == Some(&menu) { None } else { Some(menu) };
        // The workspace menus say whether there is uncommitted work: ask again as they open.
        if matches!(self.menu, Some(Menu::Workspace | Menu::Base | Menu::ConfirmArchive))
            && let Some((host, id)) = self.open().map(|t| (t.host.clone(), t.id.clone()))
        {
            self.request_thread_vcs(&host, &id);
        }
        cx.notify();
    }

    /// Close the open menu. Leaving the model search hands focus back to the composer.
    pub fn close_menu(&mut self, cx: &mut Context<Self>) {
        match self.menu.take() {
            Some(Menu::AllModels | Menu::Base) => self.focus_composer(cx),
            Some(_) => {}
            None => return,
        }
        cx.notify();
    }

    /// A press outside the open menu: close it and swallow this click's `toggle_menu`.
    pub fn dismiss_menu(&mut self, cx: &mut Context<Self>) {
        self.menu_dismissed = self.menu.is_some();
        self.close_menu(cx);
    }

    /// Clicking a work line: show its log in the panel (opening it), or close the panel
    /// if that log is the one showing.
    pub fn toggle_log(&mut self, key: usize, cx: &mut Context<Self>) {
        let Pane::Thread(t) = &mut self.pane else { return };
        // A log only ever shows in an open panel (`close_panel` drops it).
        if t.log == Some(key) {
            self.close_panel();
        } else {
            t.panel = None;
            self.log_on = true;
            self.show_log(key);
        }
        self.focus_composer(cx);
        cx.notify();
    }

    /// Point the panel's log section at a work: a finished one opens at the top, a live
    /// one follows its latest step. `log_panel` fills the list in at the next render.
    fn show_log(&mut self, key: usize) {
        if let Pane::Thread(t) = &mut self.pane {
            let live = t.timeline.open_work().map_or(key == thread::TAIL, |i| i == key);
            t.log = Some(key);
            t.log_list.reset(0);
            t.log_list.set_follow_mode(if live { FollowMode::Tail } else { FollowMode::Normal });
        }
    }

    /// Show / hide a tool call's arguments and output in the log panel.
    pub fn toggle_expanded(&mut self, key: (usize, usize), cx: &mut Context<Self>) {
        if let Pane::Thread(t) = &mut self.pane {
            if !t.expanded.remove(&key) {
                t.expanded.insert(key);
            }
            if t.log == Some(key.0) {
                t.log_list.remeasure_items(key.1..key.1 + 1);
            }
            // Clicking in the panel blurs the composer; keep typing working.
            self.focus_composer(cx);
            cx.notify();
        }
    }
}

impl Focusable for Dirigent {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

fn now_ms() -> i64 {
    std::time::UNIX_EPOCH.elapsed().map(|d| d.as_millis() as i64).unwrap_or(0)
}

/// The cache key of the new-thread draft's composer state (threads use their host and id).
const DRAFT_KEY: (&str, &str) = ("", "");

/// Whose state the composer shows on `pane`: the open thread's, the new-thread draft's,
/// or nobody's (the composer isn't on screen).
fn composer_key(pane: &Pane) -> Option<(&str, &str)> {
    match pane {
        Pane::Thread(t) => Some((&t.host, &t.id)),
        Pane::Draft { .. } => Some(DRAFT_KEY),
        _ => None,
    }
}

/// Attachments as the cache stores them.
fn image_rows(images: &[Attachment]) -> Vec<(u32, &str, &[u8])> {
    images.iter().map(|a| (a.n, a.image.format.mime_type(), a.image.bytes.as_slice())).collect()
}

/// A renderable image from stored bytes; `None` for formats gpui can't decode.
fn image_from(mime: &str, bytes: Vec<u8>) -> Option<Arc<gpui::Image>> {
    let format = gpui::ImageFormat::from_mime_type(mime)?;
    let id = gpui::hash(&bytes);
    Some(Arc::new(gpui::Image { format, bytes, id }))
}

/// A host as the cache last saw it; the connection fills in the rest.
fn host_state(cache: &Cache, name: &str, url: &str) -> HostState {
    HostState {
        name: name.to_owned(),
        url: url.to_owned(),
        status: HostStatus::Disconnected,
        projects: cache.projects(name).unwrap_or_default(),
        threads: cache.threads(name).unwrap_or_default(),
        models: cache.models(name),
    }
}

/// A blob's image and its upright size; `None` for formats gpui can't decode.
fn blob_from(mime: &str, bytes: Vec<u8>) -> Option<Blob> {
    let size = upright_size(&bytes);
    Some(Blob { image: image_from(mime, bytes)?, size })
}

/// Width and height of the image in `bytes`, from its header alone, turned the way gpui
/// draws it (EXIF orientation).
fn upright_size(bytes: &[u8]) -> Option<(f32, f32)> {
    use image::{ImageDecoder, metadata::Orientation::*};
    let mut decoder = image::ImageReader::new(std::io::Cursor::new(bytes)).with_guessed_format().ok()?.into_decoder().ok()?;
    let (w, h) = decoder.dimensions();
    if w == 0 || h == 0 {
        return None;
    }
    let turned = matches!(decoder.orientation().ok()?, Rotate90 | Rotate270 | Rotate90FlipH | Rotate270FlipH);
    Some(if turned { (h as f32, w as f32) } else { (w as f32, h as f32) })
}

fn decode_blob(mime: &str, data: &str) -> Option<Blob> {
    blob_from(mime, base64::engine::general_purpose::STANDARD.decode(data).ok()?)
}

impl Render for Dirigent {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        self.selection.begin_frame();
        self.settle_jump(window);
        self.settle_panel();
        let toasts = self.toasts.iter().map(|(m, _)| m.clone()).collect::<Vec<_>>();
        let main = match &self.pane {
            Pane::Thread(_) | Pane::Draft { .. } => thread::render(self, cx),
            _ => lists::render(self, cx),
        };
        let docked = !self.sidebar_hidden;
        // The main pane is a rounded panel inset from the canvas on every side but the
        // sidebar's (Linear). When the sidebar is hidden the panel is inset on the left too.
        let panel = div()
            .flex_1()
            .min_w_0()
            .my(px(theme::GUTTER))
            .mr(px(theme::GUTTER))
            .when(!docked, |d| d.ml(px(theme::GUTTER)))
            .rounded_lg()
            .bg(rgba(theme::t().panel))
            .border_1()
            .border_color(rgba(theme::t().line_soft))
            .overflow_hidden()
            .flex()
            .flex_col()
            .child(main);
        div()
            .key_context(KEY_CONTEXT)
            .track_focus(&self.focus_handle)
            .on_action(cx.listener(Self::new_thread))
            .on_action(cx.listener(Self::abort))
            .on_action(cx.listener(Self::toggle_sidebar))
            .on_action(cx.listener(Self::toggle_log_key))
            .on_action(|_: &Quit, _, cx| cx.quit())
            .on_action(cx.listener(Self::copy_selection))
            .on_action(cx.listener(Self::open_app_link))
            // Capture runs root-first, i.e. before the menu's `on_mouse_down_out`.
            .capture_any_mouse_down(cx.listener(|this, _, _, _| this.menu_dismissed = false))
            .on_mouse_down(MouseButton::Left, cx.listener(Self::on_mouse_down))
            .on_mouse_move(cx.listener(Self::on_mouse_move))
            .on_mouse_up(MouseButton::Left, cx.listener(Self::on_mouse_up))
            .relative()
            .size_full()
            .flex()
            .flex_row()
            .bg(rgba(theme::t().canvas))
            .text_color(rgba(theme::t().fg))
            .font_family(self.fonts.ui.clone())
            // No ligatures, anywhere: `calt` is what Lilex uses for them, `liga` for good measure.
            .font_features(FontFeatures(Arc::new(vec![("calt".into(), 0), ("liga".into(), 0)])))
            .text_size(px(13.))
            .when(docked, |d| d.child(sidebar::docked(self, cx)))
            .child(panel)
            .when(!docked, |d| d.child(sidebar::peek(self, cx)))
            .children(self.preview.clone().map(|image| thread::preview(image, cx)))
            .children((!toasts.is_empty()).then(|| {
                div().absolute().bottom_4().right_4().flex().flex_col().gap_1().children(toasts.into_iter().map(|m| {
                    div()
                        .max_w(px(420.))
                        .px_3()
                        .py_2()
                        .rounded_md()
                        .flex()
                        .flex_row()
                        .items_center()
                        .gap_2()
                        .bg(rgba(theme::t().popup))
                        .border_1()
                        .border_color(rgba(theme::t().line))
                        .shadow_md()
                        .child(div().size(px(6.)).rounded_full().bg(rgba(theme::t().error)).flex_shrink_0())
                        // Shrinkable, so a long message wraps inside `max_w`.
                        .child(div().flex_1().min_w_0().child(m))
                }))
            }))
    }
}
