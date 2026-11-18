use crate::add_project;
use crate::ambient::{self, Pattern, Reach};
use crate::cache::Cache;
use crate::hosts_tab;
use crate::input::{self, Arrow, ImagePasted, Submitted, TextInput, image_token};
use crate::mcp;
use crate::net::{CONNECT_LIST_ID, HostStatus, Incoming, Net};
use crate::orchestra::{self, LineageNode};
use crate::outbox::{Change, Outbox};
use crate::projects::{self, HostState, Links};
use crate::proxy;
use crate::select::{Doc, OpenAppLink, Selection};
use crate::theme::{self, Status, text, u};
use crate::view::{self, Item, Streaming, Timeline};
use crate::{account, leaderboard, lists, menu, onboarding, ruler, sidebar, thread, titlebar, usage};
use crate::{changelog, config, hosts, images, instance, settings, toasts, update, visual};
use base64::Engine;
use dirigent_protocol::transcript::{Body, Entry, Event, Notice, text_of};
use dirigent_protocol::{
    Ask, ClientMessage, FirstPrompt, ForkedFrom, ImageAttachment, ModelInfo, PromptMode, ServerMessage, Setup, ThreadSummary, TitleGen,
    Vcs, prompt_title,
};
use futures::StreamExt;
use gpui::EntityInputHandler;
use gpui::{
    AnyElement, AnyWindowHandle, App, ClipboardItem, Context, ElementId, Entity, FocusHandle, Focusable, FollowMode, FontFeatures,
    ListAlignment, ListOffset, ListState, MouseButton, MouseMoveEvent, MouseUpEvent, Pixels, Point, ScrollHandle, Window, actions, div,
    prelude::*, px, rgba,
};
use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

actions!(dirigent, [NewThread, Dismiss, FocusComposer, ToggleSidebar, ToggleLog, Quit, TextBigger, TextSmaller, TextReset]);

pub const KEY_CONTEXT: &str = "Dirigent";
const VCS_REFRESH_SECS: u64 = 2;
const LAST_MODEL_KEY: &str = "last_model";
const LAST_EFFORT_KEY: &str = "last_effort";
const LAST_TARGET_KEY: &str = "last_target";
const LAST_ISOLATE_KEY: &str = "last_isolate";
const PRESETS_KEY: &str = "presets";
const SIDEBAR_WIDTH_KEY: &str = "sidebar_width";
const COLLAPSED_KEY: &str = "collapsed";
const PINNED_PROJECTS_KEY: &str = "pinned_projects";
const GROUP_ORDER_KEY: &str = "group_order";
const PANE_KEY: &str = "pane";
const SIDEBAR_MIN: f32 = 180.;
pub const PANE_MIN: f32 = 32.;
const LOG_WIDTH_KEY: &str = "log_width";
const PANEL_HIDDEN_KEY: &str = "panel_hidden";
const LOG_MIN: f32 = 280.;
const COPIED_TTL: Duration = Duration::from_millis(1200);
const USAGE_REFRESH_SECS: u64 = 30;
const PROXY_UPDATE_CHECK_SECS: u64 = 60 * 60;
const TITLING_TTL: Duration = Duration::from_secs(60);
const VISUAL_WAIT: Duration = Duration::from_millis(100);

#[derive(Clone, Copy, PartialEq)]
pub enum Resize {
    Sidebar,
    Log,
}

#[derive(Clone, Copy, PartialEq)]
pub enum PanelPart {
    Lineage,
    Log,
}

pub struct OpenThread {
    pub host: String,
    pub id: String,
    pub entries: Vec<Entry>,
    pub timeline: Timeline,
    pub streaming: Streaming,
    pub list: ListState,
    pub expanded: HashSet<(usize, usize)>,
    pub log: Option<usize>,
    pub log_list: ListState,
    pub copied: Option<ElementId>,
    pub pins: HashSet<String>,
    pub ruler: ruler::State,
    pub stats_hover: bool,
    center_on: Option<(usize, u8)>,
    pub new_output: bool,
    pub asks: Vec<AskForm>,
    pub ask_h: Pixels,
    // MUST KILL: type: an enum (Auto | Open | Closed) instead of Option<bool>
    pub panel: Option<bool>,
    // MUST KILL: type: a named struct (request id, last seq held) for fetch_open's stale cut
    fetching: Option<(u64, Option<u64>)>,
    pub editing: Option<Editing>,
    pub rewound: Option<Rewound>,
    jump_entry: Option<String>,
    pub screen: Option<crate::screen::Screen>,
    pub visuals: visual::Visuals,
}

pub struct Editing {
    pub entry_id: String,
    stash: input::Content,
    images: Vec<Attachment>,
}

pub struct Rewound {
    request: u64,
    entry_id: String,
    text: String,
    images: Vec<Attachment>,
}

fn timeline_of(entries: &[Entry]) -> Timeline {
    let branch = entries.last().map(|last| dirigent_protocol::transcript::branch(entries, &last.id)).unwrap_or_default();
    Timeline::from_entries(branch)
}

#[derive(Default)]
struct LogMemory {
    tops: HashMap<usize, ListOffset>,
    expanded: HashSet<(usize, usize)>,
}

impl OpenThread {
    fn new(host: &str, id: &str, entries: Vec<Entry>) -> Self {
        let timeline = timeline_of(&entries);
        let list = ListState::new(timeline.items.len() + 1, ListAlignment::Top, px(300.));
        list.set_follow_mode(FollowMode::Tail);
        Self {
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
            pins: HashSet::new(),
            ruler: Default::default(),
            stats_hover: false,
            center_on: None,
            new_output: false,
            asks: vec![],
            ask_h: px(0.),
            panel: None,
            fetching: None,
            editing: None,
            rewound: None,
            jump_entry: None,
            screen: None,
            visuals: Default::default(),
        }
    }

    // MUST KILL: rename: reset_keeping_scroll, unlike ListState::reset it keeps a scrolled-up position
    fn relist(&mut self) {
        let top = (!self.list.is_following_tail()).then(|| self.list.logical_scroll_top());
        self.list.reset(self.timeline.items.len() + 1);
        if let Some(top) = top {
            self.list.scroll_to(top);
        }
    }

    pub fn is_pinned(&self, ix: usize) -> bool {
        self.timeline.entry_id(ix).is_some_and(|id| self.pins.contains(id))
    }

    // MUST KILL: type: a named result (items before, first changed) instead of (usize, Option<usize>)
    fn fold(&mut self, entry: Entry) -> (usize, Option<usize>) {
        match &entry.body {
            Body::Assistant(_) => self.streaming.steps.clear(),
            Body::User { content } if self.timeline.open_work().is_none() => self.streaming.confirm_prompt(&text_of(content)),
            Body::Notice(Notice::Answer { ask_id, .. }) => self.streaming.confirm_answer(ask_id),
            _ => {}
        }
        let n = self.timeline.items.len();
        let branched = self.entries.last().is_some_and(|last| entry.parent_id.as_deref() != Some(last.id.as_str()));
        self.entries.push(entry);
        if branched {
            self.rewound = None;
            self.timeline = timeline_of(&self.entries);
            return (n, Some(0));
        }
        if self.rewound.is_some() {
            return (n, None);
        }
        let changed = self.timeline.push(&self.entries[self.entries.len() - 1]);
        (n, changed)
    }

    pub fn typed_message(&self, ix: usize) -> Option<&Entry> {
        if !matches!(self.timeline.items.get(ix), Some(Item::User { .. })) {
            return None;
        }
        let id = self.timeline.entry_id(ix)?;
        self.entries.iter().rev().find(|e| e.id == id).filter(|e| matches!(e.body, Body::User { .. }))
    }

    fn item_at(&self, id: &str) -> Option<usize> {
        let branch: HashSet<&str> = dirigent_protocol::transcript::branch(&self.entries, id).into_iter().map(|e| e.id.as_str()).collect();
        (0..self.timeline.items.len()).rev().find(|&ix| self.timeline.entry_id(ix).is_some_and(|e| branch.contains(e)))
    }
}

pub struct AskForm {
    pub ask: Ask,
    pub picked: Vec<HashSet<usize>>,
    pub custom: Vec<Option<Entity<TextInput>>>,
    pub current: usize,
    pub sent: bool,
}

impl AskForm {
    fn typed(&self, i: usize, cx: &App) -> Option<String> {
        self.custom[i].as_ref().map(|input| input.read(cx).text().trim().to_owned()).filter(|t| !t.is_empty())
    }

    pub fn answered(&self, i: usize, cx: &App) -> bool {
        !self.picked[i].is_empty() || self.typed(i, cx).is_some()
    }

    pub fn complete(&self, cx: &App) -> bool {
        (0..self.ask.questions.len()).all(|i| self.answered(i, cx))
    }

    pub fn next_unanswered(&self, cx: &App) -> Option<usize> {
        let n = self.ask.questions.len();
        (1..n).map(|k| (self.current + k) % n).find(|&i| !self.answered(i, cx))
    }

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

#[derive(Clone, PartialEq, Eq, Debug)]
pub struct VcsInfo {
    pub vcs: Option<Vcs>,
    pub dirty: bool,
    pub branch: Option<String>,
    pub refs: Vec<String>,
}

// MUST KILL: rearchitect: one default base in dirigent_protocol, shared with dirigent_service::vcs
pub fn default_base(vcs: Vcs) -> &'static str {
    match vcs {
        Vcs::Git => "HEAD",
        Vcs::Jj => "@-",
    }
}

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
    Inbox,
    Workpool,
    Project { group_key: String },
    Projects,
    Draft { target: Option<Target> },
    Thread(Box<OpenThread>),
}

#[derive(Clone)]
pub struct Attachment {
    pub n: u32,
    pub blob: Blob,
}

type Draft = (String, Vec<Attachment>);

struct PendingDraft {
    id: u64,
    target: Target,
    base: Option<String>,
    text: String,
    images: Vec<Attachment>,
    thread: ThreadSummary,
}

struct SentPrompt {
    host: String,
    thread_id: String,
    text: String,
    images: Vec<Attachment>,
    mode: PromptMode,
}

struct OpenFolder {
    path: String,
    request: Option<u64>,
}

#[derive(Clone)]
pub struct Blob {
    pub image: Arc<gpui::Image>,
    pub size: Option<(f32, f32)>,
}

#[derive(Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct Preset {
    pub model: String,
    pub effort: Option<String>,
}

#[derive(Clone, Copy, PartialEq, Eq, Hash)]
pub enum Section {
    Inbox,
    Workpool,
}

impl Section {
    const ALL: [Section; 2] = [Section::Inbox, Section::Workpool];

    fn key(self) -> &'static str {
        match self {
            Section::Inbox => "inbox",
            Section::Workpool => "workpool",
        }
    }

    fn parse_folded(saved: Option<&str>) -> HashSet<Section> {
        match saved {
            Some(s) => Self::ALL.into_iter().filter(|x| s.split_whitespace().any(|w| w == x.key())).collect(),
            None => HashSet::from([Section::Workpool]),
        }
    }
}

#[derive(Clone, PartialEq)]
pub enum Menu {
    Model,
    AllModels,
    Effort,
    Target,
    TargetHost,
    Project { group_key: String },
    Link { group_key: String },
    NewProjectHost,
    ConfirmDelete,
    ConfirmArchive { host: String, id: String, at: ArchiveAt },
    Workspace,
    Base,
    UsageGroup,
    UsageMetric,
    Theme,
    Background,
    TitleModel,
    TitleEffort,
    SettingsHost,
    AddHost,
    HostAuth,
    McpExposure,
    Update,
    Toasts,
    ThreadActions { host: String, id: String, at: Point<Pixels> },
}

#[derive(Clone)]
pub struct DragProject {
    pub key: String,
    pub sidebar: bool,
}

#[derive(Clone, PartialEq)]
pub enum Renaming {
    Project(String),
    Thread { host: String, id: String },
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Modal {
    Settings,
    Usage,
    Changelog,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum ArchiveAt {
    Header,
    Sidebar,
    List,
    Tree,
}

pub struct Dirigent {
    pub hosts: Vec<HostState>,
    pub links: Links,
    pub names: projects::Names,
    pub renaming: Option<Renaming>,
    pub rename_input: Entity<TextInput>,
    pub pinned_projects: Vec<String>,
    pub group_order: Vec<String>,
    pub expanded_projects: HashSet<String>,
    pub new_project_host: Option<String>,
    pub add_project: Option<add_project::State>,
    dir_requests: HashSet<u64>,
    pub dragging: Option<DragProject>,
    pub hovered_card: Option<(String, String)>,
    pub unfolded: HashSet<String>,
    pub pane: Pane,
    pub modal: Option<Modal>,
    pub onboarding: Option<onboarding::State>,
    pub menu: Option<Menu>,
    menu_dismissed: bool,
    pub caption_press: bool,
    pub sidebar_width: f32,
    pub sidebar_hidden: bool,
    pub sidebar_peek: bool,
    pub collapsed: HashSet<Section>,
    pub inbox_scroll: ScrollHandle,
    pub workpool_scroll: ScrollHandle,
    pub log_width: f32,
    pub log_on: bool,
    pub panel_hidden: Option<PanelPart>,
    logs: HashMap<String, LogMemory>,
    resizing: Option<Resize>,
    pub composer: Entity<TextInput>,
    pub path_input: Entity<TextInput>,
    pub hosts_tab: hosts_tab::State,
    pub model_search: Entity<TextInput>,
    pub base_search: Entity<TextInput>,
    pub target_search: Entity<TextInput>,
    pub link_search: Entity<TextInput>,
    pub usage: usage::State,
    pub usage_from: Entity<TextInput>,
    pub usage_to: Entity<TextInput>,
    pub leaderboard: leaderboard::State,
    pub forge: account::State,
    ticks: u64,
    pub clock: i64,
    pub model: Option<String>,
    pub effort: Option<String>,
    pub presets: Vec<Preset>,
    pub toasts: toasts::Toasts,
    pub copied: Option<ElementId>,
    pub config: config::Config,
    pub fonts: theme::Fonts,
    pub field: ambient::Field,
    watch: config::Watch,
    config_edit: u64,
    pub settings: settings::State,
    pub login: crate::login::State,
    pub proxy: proxy::State,
    pub mcp: mcp::State,
    titling: HashMap<(String, String), Instant>,
    retitles: HashMap<u64, (String, String)>,
    attachments: Vec<Attachment>,
    fitting: usize,
    pub preview: Option<Arc<gpui::Image>>,
    pub screen_preview: bool,
    pub selection: Selection,
    pub blobs: HashMap<String, Blob>,
    blob_requests: HashMap<String, HashMap<u64, String>>,
    pending: Vec<PendingDraft>,
    prompts: HashMap<u64, SentPrompt>,
    forks: HashMap<u64, (String, Option<Draft>)>,
    pub isolate: bool,
    pub base: Option<String>,
    pub vcs: HashMap<(String, String), VcsInfo>,
    pub updater: update::State,
    open_folder: Option<OpenFolder>,
    net: Net,
    pub(crate) cache: Arc<Mutex<Cache>>,
    next_id: u64,
    focus_handle: FocusHandle,
    window: AnyWindowHandle,
}

impl Dirigent {
    pub fn new(cache: Cache, config: config::Config, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let mut hosts = vec![(hosts::LOCAL.to_owned(), hosts::Host::new(format!("ws://127.0.0.1:{}/ws", dirigent_service::port())))];
        let hosts_error = match hosts::load() {
            Ok(remote) => {
                hosts.extend(remote);
                None
            }
            Err(e) => Some(format!("{e:#}")),
        };
        let updated = changelog::after_update(&cache);
        let onboard = onboarding::needed(&cache);
        let host_states = hosts.iter().map(|(name, host)| host_state(&cache, name, host)).collect();
        let links = cache.links().unwrap_or_default().into_iter().map(|(h, p, g)| ((h, p), g)).collect();
        let names = cache.project_names().unwrap_or_default().into_iter().collect();
        let pinned_projects: Vec<String> = cache.get(PINNED_PROJECTS_KEY).and_then(|s| serde_json::from_str(&s).ok()).unwrap_or_default();
        let group_order: Vec<String> = match cache.get(GROUP_ORDER_KEY).and_then(|s| serde_json::from_str(&s).ok()) {
            Some(order) => order,
            None => {
                let _ = cache.set(GROUP_ORDER_KEY, &serde_json::to_string(&pinned_projects).unwrap_or_default());
                pinned_projects.clone()
            }
        };
        let model = cache.get(LAST_MODEL_KEY);
        let effort = cache.get(LAST_EFFORT_KEY);
        let presets: Vec<Preset> = cache.get(PRESETS_KEY).and_then(|s| serde_json::from_str(&s).ok()).unwrap_or_default();
        let isolate = cache.get(LAST_ISOLATE_KEY).as_deref() == Some("1");
        let sidebar_width = cache.get(SIDEBAR_WIDTH_KEY).and_then(|w| w.parse().ok()).unwrap_or(sidebar::WIDTH);
        let log_width = cache.get(LOG_WIDTH_KEY).and_then(|w| w.parse().ok()).unwrap_or(thread::LOG_WIDTH);
        let collapsed = Section::parse_folded(cache.get(COLLAPSED_KEY).as_deref());
        let panel_hidden = match cache.get(PANEL_HIDDEN_KEY).as_deref() {
            Some("lineage") => Some(PanelPart::Lineage),
            Some("log") => Some(PanelPart::Log),
            _ => None,
        };
        let forge = account::State::new();
        let leaderboard = leaderboard::State::new(&cache);
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
        // A focused input the frame no longer draws (an edited message scrolled out of the list) takes keys out of
        // every context; the root takes the focus back.
        cx.on_focus_lost(window, |this, _, cx| this.focus_root(cx)).detach();

        let composer = cx.new(|cx| TextInput::new(cx, "Makoto - Golden Girl feat. MC Conrad (Lenzman VIP)", false));
        let path_input = cx.new(|cx| TextInput::new(cx, "~/path/to/project", true));
        cx.subscribe_in(&composer, window, |this, _, s: &Submitted, window, cx| this.send(s.queued, window, cx)).detach();
        cx.subscribe(&composer, |this, _, p: &ImagePasted, cx| this.attach(p.0.clone(), cx)).detach();
        cx.subscribe(&composer, |this, _, _: &input::Changed, cx| this.save_draft(cx)).detach();
        cx.subscribe(&path_input, |this, _, s: &Submitted, cx| if s.ctrl { this.confirm_add_project(cx) } else { this.enter_dir(cx) })
            .detach();
        cx.subscribe(&path_input, |this, _, _: &input::Changed, cx| this.project_path_changed(cx)).detach();
        cx.subscribe(&path_input, |this, _, a: &Arrow, cx| this.move_dir_selection(a.0, cx)).detach();
        let rename_input = cx.new(|cx| TextInput::new(cx, "name", true));
        cx.subscribe(&rename_input, |this, _, _: &Submitted, cx| this.finish_rename(true, cx)).detach();
        let hosts_tab = hosts_tab::State::new(cx);
        let model_search = cx.new(|cx| TextInput::new(cx, "Search models", true));
        let base_search = cx.new(|cx| TextInput::new(cx, "Branch, commit or revset", true));
        let target_search = cx.new(|cx| TextInput::new(cx, "Search projects", true));
        let link_search = cx.new(|cx| TextInput::new(cx, "Search projects", true));
        for input in [&model_search, &base_search, &target_search, &link_search] {
            cx.subscribe(input, |_, _, _: &input::Changed, cx| menu::Nav::restart(cx)).detach();
        }
        let usage_from = cx.new(|cx| TextInput::new(cx, "2026-01-01", true));
        let usage_to = cx.new(|cx| TextInput::new(cx, "2026-01-31", true));
        for input in [&usage_from, &usage_to] {
            cx.subscribe(input, |this, _, _: &Submitted, cx| this.apply_custom_usage(cx)).detach();
        }
        let mut settings = settings::State::new(cx);
        for input in [&settings.font_ui, &settings.font_prose, &settings.font_code, &settings.prompt] {
            cx.subscribe(input, |this, _, _: &input::Changed, cx| this.schedule_apply(cx)).detach();
        }
        cx.subscribe(&settings.sample, |this, _, _: &Submitted, cx| this.run_title_test(cx)).detach();
        let login = crate::login::State::new(cx);
        let proxy = proxy::State::new(cx);
        let mcp = mcp::State::new(cx);
        let (fonts, missing) = config.fonts.resolve(cx);
        settings.missing_fonts = missing.clone();
        theme::set_text_size(config.fonts.size());

        let mut this = Self {
            hosts: host_states,
            links,
            names,
            renaming: None,
            rename_input,
            pinned_projects,
            group_order,
            expanded_projects: HashSet::new(),
            new_project_host: None,
            add_project: None,
            dir_requests: HashSet::new(),
            dragging: None,
            hovered_card: None,
            unfolded: HashSet::new(),
            pane: Pane::Inbox,
            modal: None,
            onboarding: None,
            menu: None,
            menu_dismissed: false,
            caption_press: false,
            sidebar_width,
            sidebar_hidden: false,
            sidebar_peek: false,
            collapsed,
            inbox_scroll: ScrollHandle::new(),
            workpool_scroll: ScrollHandle::new(),
            log_width,
            log_on: false,
            panel_hidden,
            logs: HashMap::new(),
            resizing: None,
            composer,
            path_input,
            hosts_tab,
            model_search,
            base_search,
            target_search,
            link_search,
            usage: usage::State::new(),
            usage_from,
            usage_to,
            leaderboard,
            forge,
            ticks: 0,
            clock: this_second_ms(),
            model,
            effort,
            presets,
            toasts: toasts::Toasts::default(),
            copied: None,
            config,
            fonts,
            field: ambient::Field::default(),
            watch: config::Watch::new(),
            config_edit: 0,
            settings,
            login,
            proxy,
            mcp,
            titling: HashMap::new(),
            retitles: HashMap::new(),
            attachments: vec![],
            fitting: 0,
            preview: None,
            screen_preview: false,
            selection: Selection::default(),
            blobs: HashMap::new(),
            blob_requests: HashMap::new(),
            pending: vec![],
            prompts: HashMap::new(),
            forks: HashMap::new(),
            isolate,
            base: None,
            vcs: HashMap::new(),
            updater: update::State::default(),
            open_folder: None,
            net,
            cache,
            next_id: 1,
            focus_handle: cx.focus_handle(),
            window: window.window_handle(),
        };
        match theme::load(&this.config.theme) {
            Ok(t) => theme::set(t),
            Err(e) => this.toast(format!("{e:#}"), cx),
        }
        this.restore_pane(cx);
        if onboard {
            this.open_onboarding(cx);
        } else if updated {
            this.open_changelog(cx);
        }
        this.tick(cx);
        update::start(cx);
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

    pub fn request(&mut self, host: &str, make: impl FnOnce(u64) -> ClientMessage) -> u64 {
        let id = self.next_id();
        self.net.send(host, make(id));
        id
    }

    fn change(&mut self, host: &str, change: Change) {
        let Some(h) = self.host_mut(host) else { return };
        change.apply(&mut h.projects, &mut h.threads);
        h.outbox.push(change);
        self.save_outbox(host);
        self.send_changes(host);
    }

    fn save_outbox(&self, host: &str) {
        let Some(h) = self.host(host) else { return };
        if let Err(e) = self.cache.lock().unwrap().set_outbox(host, h.outbox.unanswered()) {
            tracing::warn!(%host, "outbox: {e:#}");
        }
    }

    fn send_changes(&mut self, host: &str) {
        let id = self.next_id();
        if let Some(request) = self.host_mut(host).and_then(|h| h.outbox.next_request(id)) {
            self.net.send(host, request);
        }
    }

    fn change_answered(&mut self, host: &str, request: u64) -> bool {
        if !self.host_mut(host).is_some_and(|h| h.outbox.answered(request)) {
            return false;
        }
        self.save_outbox(host);
        self.send_changes(host);
        true
    }

    pub fn set_input(&self, input: &Entity<TextInput>, text: &str, cx: &mut Context<Self>) {
        let (window, input, text) = (self.window, input.clone(), text.to_owned());
        cx.defer(move |cx| {
            window.update(cx, |_, window, cx| input.update(cx, |i, cx| i.set_text(&text, window, cx))).ok();
        });
    }

    fn host_mut(&mut self, name: &str) -> Option<&mut HostState> {
        self.hosts.iter_mut().find(|h| h.name == name)
    }

    pub fn host(&self, name: &str) -> Option<&HostState> {
        self.hosts.iter().find(|h| h.name == name)
    }

    pub fn connected(&self, host: &str) -> bool {
        self.host(host).is_some_and(|h| h.status == HostStatus::Connected)
    }

    pub fn composer_offline(&self) -> bool {
        self.composer_host().is_some_and(|h| !self.connected(h))
    }

    pub fn host_label(&self, host: &str) -> String {
        if self.connected(host) { host.to_owned() } else { format!("{host} (offline)") }
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

    fn is_pending(&self, host: &str, id: &str) -> bool {
        self.pending.iter().any(|p| p.target.host == host && p.thread.id == id)
    }

    fn take_pending(&mut self, host: &str, request: u64) -> Option<PendingDraft> {
        let i = self.pending.iter().position(|p| p.id == request && p.target.host == host)?;
        Some(self.pending.remove(i))
    }

    pub fn not_ready(&self, host: &str, id: &str) -> Option<&'static str> {
        if self.summary(host, id).is_some_and(|s| s.locked) {
            return Some("This thread could not be converted from pi whole: fork it from an earlier message to go on");
        }
        match self.summary(host, id).and_then(|s| s.setup) {
            Some(Setup::Workspace) => Some("Still creating the thread's workspace"),
            Some(Setup::NixShell) => Some("Still activating the nix dev shell"),
            None if self.is_pending(host, id) => Some("The thread is still starting"),
            None => None,
        }
    }

    pub fn is_running(&self) -> bool {
        self.open().and_then(|t| self.summary(&t.host, &t.id)).is_some_and(|t| t.running())
    }

    pub fn unread(&self, host: &str, t: &ThreadSummary) -> bool {
        self.snoozable(host, t) && t.seen_at.is_none_or(|s| t.updated_at > s)
    }

    pub fn snoozable(&self, host: &str, t: &ThreadSummary) -> bool {
        self.status(host, t).finished() && !t.running()
    }

    pub fn at_rest(&self, host: &str, t: &ThreadSummary) -> bool {
        !t.running() && t.jobs == 0 && !self.has_busy_children(host, &t.id)
    }

    pub fn title_color(&self, host: &str, t: &ThreadSummary) -> u32 {
        if t.asking() {
            theme::t().warn
        } else if self.unread(host, t) {
            theme::t().accent_ink
        } else {
            theme::t().fg
        }
    }

    pub fn status(&self, host: &str, t: &ThreadSummary) -> Status {
        match Status::of(t) {
            Status::Done if t.jobs > 0 || self.has_busy_children(host, &t.id) => Status::Waiting,
            s => s,
        }
    }

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

    pub fn set_pinned(&mut self, host: &str, thread_id: &str, pinned: bool, cx: &mut Context<Self>) {
        self.change(host, Change::Pin { thread_id: thread_id.to_owned(), pinned });
        cx.notify();
    }

    fn sort_by_status(&self, rows: &mut [(&str, &ThreadSummary)]) {
        rows.sort_by_key(|(h, t)| (self.status(h, t), !t.pinned));
    }

    pub fn sort_as_queue(&self, rows: &mut [(&str, &ThreadSummary)]) {
        rows.sort_by_key(|(_, t)| std::cmp::Reverse(t.running_since));
        self.sort_by_status(rows);
    }

    pub fn set_snoozed(&mut self, host: &str, thread_id: &str, snoozed: bool, cx: &mut Context<Self>) {
        let Some(updated_at) = self.summary(host, thread_id).map(|t| t.updated_at) else { return };
        self.change(host, Change::Snooze { thread_id: thread_id.to_owned(), snoozed, updated_at });
        cx.notify();
    }

    pub fn inbox_threads(&self) -> Vec<(&str, &ThreadSummary)> {
        let mut rows = self.inbox_all();
        rows.retain(|(_, t)| !t.snoozed);
        rows
    }

    pub fn inbox_all(&self) -> Vec<(&str, &ThreadSummary)> {
        self.threads_where(|s| s.finished())
    }

    pub fn busy_threads(&self) -> Vec<(&str, &ThreadSummary)> {
        self.threads_where(|s| s.busy())
    }

    fn on_sidebar(&self, host: &str, id: &str) -> bool {
        let shown = |section, rows: Vec<(&str, &ThreadSummary)>| {
            !self.collapsed.contains(&section) && rows.iter().any(|(h, t)| *h == host && t.id == id)
        };
        shown(Section::Inbox, self.inbox_threads()) || shown(Section::Workpool, self.busy_threads())
    }

    fn drop_gone_card(&mut self, cx: &mut Context<Self>) {
        if let Some(Menu::ThreadActions { host, id, .. }) = &self.menu
            && !self.on_sidebar(host, id)
        {
            self.menu = None;
        }
        if let Some(Renaming::Thread { host, id }) = &self.renaming
            && !self.on_sidebar(host, id)
        {
            self.renaming = None;
            self.focus(self.focus_handle.clone(), cx);
        }
    }

    fn mark_seen(&mut self, host: &str, id: &str) {
        let Some(updated_at) = self.summary(host, id).filter(|t| self.unread(host, t)).map(|t| t.updated_at) else {
            return;
        };
        self.change(host, Change::Seen { thread_id: id.to_owned(), updated_at });
    }

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

    pub fn lineage(&self, host: &str, id: &str) -> Vec<LineageNode<'_>> {
        let Some(h) = self.host(host) else { return vec![] };
        orchestra::lineage(&h.threads, id, &self.unfolded)
    }

    pub fn toggle_unfolded(&mut self, parent: &str, cx: &mut Context<Self>) {
        if !self.unfolded.remove(parent) {
            self.unfolded.insert(parent.to_owned());
        }
        cx.notify();
    }

    pub fn group_key_of(&self, host: &str, project_id: &str) -> Option<String> {
        let project = self.host(host)?.projects.iter().find(|p| p.id == project_id)?;
        Some(projects::group_key(host, project, &self.links))
    }

    pub fn project_path(&self, host: &str, t: &ThreadSummary) -> Option<String> {
        let pid = t.project_id.as_deref()?;
        self.host(host)?.projects.iter().find(|p| p.id == pid).map(|p| p.path.clone())
    }

    pub fn project_name(&self, host: &str, t: &ThreadSummary) -> String {
        t.project_id
            .as_deref()
            .and_then(|pid| self.host(host)?.projects.iter().find(|p| p.id == pid))
            .map(|p| {
                let key = projects::group_key(host, p, &self.links);
                self.names.get(&key).cloned().unwrap_or_else(|| projects::basename(&p.path).to_owned())
            })
            .unwrap_or_default()
    }

    pub fn composer_host(&self) -> Option<&str> {
        match &self.pane {
            Pane::Draft { target: Some(t) } => Some(&t.host),
            Pane::Thread(t) => Some(&t.host),
            _ => None,
        }
    }

    pub fn models(&self) -> &[ModelInfo] {
        self.composer_host().and_then(|h| self.host(h)).map(|h| h.models.as_slice()).unwrap_or(&[])
    }

    pub fn current_model(&self) -> Option<&ModelInfo> {
        let key = self.model.as_deref()?;
        self.models().iter().find(|m| m.id == key)
    }

    fn on_incoming(&mut self, msg: Incoming, cx: &mut Context<Self>) {
        match msg {
            Incoming::Status { host, status, error } => {
                if let Some(h) = self.host_mut(&host) {
                    h.status = status;
                    if status != HostStatus::Connecting {
                        h.error = error;
                    }
                }
                if status == HostStatus::Disconnected {
                    self.blob_requests.remove(&host);
                    self.forks.retain(|_, (h, _)| *h != host);
                    self.leaderboard.drop_uploads(&host);
                    if let Some(h) = self.host_mut(&host) {
                        h.outbox.disconnected();
                        h.proxy_update = None;
                        for t in &mut h.threads {
                            t.running_since = None;
                        }
                    }
                    if self.open().is_some_and(|t| t.host == host) {
                        self.settle_open();
                        self.unrewind(cx);
                        self.drop_screen(cx);
                        if let Pane::Thread(t) = &mut self.pane {
                            t.asks.iter_mut().for_each(|f| f.sent = false);
                        }
                    }
                }
                if status == HostStatus::Connected && self.open().is_some_and(|t| t.host == host) {
                    self.fetch_open();
                }
                if let Some(s) = self.add_project.as_mut().filter(|_| self.new_project_host.as_deref() == Some(&host)) {
                    if status == HostStatus::Disconnected {
                        s.add_request = None;
                    }
                    if status == HostStatus::Connected {
                        self.request_dirs(true, cx);
                    }
                }
                if status == HostStatus::Connected && self.modal == Some(Modal::Usage) {
                    self.request_usage(cx);
                }
                self.shown_host_status(&host, status, cx);
                if status == HostStatus::Disconnected {
                    let lost: Vec<_> = self.pending.extract_if(.., |p| p.target.host == host).collect();
                    if !lost.is_empty() {
                        self.toast(format!("{host}: connection lost, the message is back in the draft"), cx);
                    }
                    for p in lost {
                        self.unsend_draft(p, cx);
                    }
                    self.prompts.retain(|_, p| p.host != host);
                }
                if status == HostStatus::Connected && matches!(self.pane, Pane::Draft { .. }) {
                    self.refresh_draft_vcs();
                }
                if status == HostStatus::Connected {
                    self.request(&host, proxy::list_without_samples);
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
        self.resume_open_folder(cx);
        cx.notify();
    }

    fn on_server_message(&mut self, host: String, msg: ServerMessage, cx: &mut Context<Self>) {
        match msg {
            ServerMessage::Hello { service_id, .. } => {
                if let Some(h) = self.host_mut(&host) {
                    h.service_id = Some(service_id).filter(|id| !id.is_empty());
                }
                self.upload_usage(&host);
            }
            ServerMessage::Ok { reply_to } => {
                self.prompts.remove(&reply_to);
                let _ = self.change_answered(&host, reply_to)
                    || self.on_login_reply(&host, reply_to, Ok(()), cx)
                    || self.on_mcp_reply(&host, reply_to, Ok(()), cx);
            }
            ServerMessage::McpServers { reply_to, servers, path } => self.on_mcp_servers(&host, reply_to, servers, path, cx),
            ServerMessage::McpStatus { reply_to, status } => self.on_mcp_status(&host, reply_to, status),
            ServerMessage::LoginStep { login, step } => self.on_login_step(&host, login, step, cx),
            ServerMessage::Projects { projects, .. } => {
                if let Some(h) = self.host_mut(&host) {
                    h.projects = projects;
                    h.outbox.apply(&mut h.projects, &mut h.threads);
                }
            }
            ServerMessage::ProjectAdded { reply_to, project } => {
                if let Some(h) = self.host_mut(&host)
                    && !h.projects.iter().any(|p| p.id == project.id)
                {
                    h.projects.push(project.clone());
                }
                if let Some(f) = self.open_folder.as_mut().filter(|f| host == hosts::LOCAL && f.request == Some(reply_to)) {
                    f.path = project.path.clone();
                }
                let ours = self.add_project.as_ref().is_some_and(|s| s.add_request == Some(reply_to));
                if ours {
                    match add_project::offer(&self.groups().0, &host, &project.id) {
                        Some(offer) => {
                            if let Some(s) = &mut self.add_project {
                                s.link = Some(offer);
                            }
                        }
                        None => self.add_project = None,
                    }
                    self.focus(self.focus_handle.clone(), cx);
                }
            }
            ServerMessage::Dirs { reply_to, dirs } => {
                self.dir_requests.remove(&reply_to);
                if let Some(s) = self.add_project.as_mut().filter(|s| s.list_request == Some(reply_to)) {
                    s.dirs = Some(Ok(dirs));
                    s.selected = 0;
                }
            }
            ServerMessage::Models { models, .. } => {
                if let Some(h) = self.host_mut(&host) {
                    h.models = models;
                }
                self.hold_effort();
            }
            ServerMessage::Threads { reply_to, threads } => {
                let was_running = self.is_running();
                let ran: HashSet<String> =
                    self.host(&host).map(|h| h.threads.iter().filter(|t| t.running()).map(|t| t.id.clone()).collect()).unwrap_or_default();
                let ours: Vec<_> = self.pending.iter().filter(|p| p.target.host == host).map(|p| p.thread.clone()).collect();
                let listed_on_connect = reply_to == Some(CONNECT_LIST_ID);
                if let Some(h) = self.host_mut(&host) {
                    if listed_on_connect {
                        h.outbox.host_listed_its_threads(&threads);
                    }
                    h.threads = threads;
                    h.outbox.apply(&mut h.projects, &mut h.threads);
                    for t in ours {
                        if !h.threads.iter().any(|x| x.id == t.id) {
                            h.threads.insert(0, t);
                        }
                    }
                }
                if listed_on_connect {
                    self.save_outbox(&host);
                    self.send_changes(&host);
                }
                self.request_sidebar_vcs(&host, &ran);
                if !was_running && self.is_running() && self.menu == Some(Menu::Base) {
                    self.close_menu(cx);
                }
                if let Pane::Thread(t) = &self.pane
                    && t.host == host
                {
                    let id = t.id.clone();
                    if let Some(pins) = self.summary(&host, &id).map(|s| s.pinned_entries.iter().cloned().collect()) {
                        // MUST KILL: rearchitect: have the host send RunEnd for refused runs so the idle listing needn't end them here
                        let over = was_running && !self.is_running();
                        if let Pane::Thread(t) = &mut self.pane {
                            t.pins = pins;
                        }
                        if over {
                            self.settle_open();
                            self.unrewind(cx);
                        }
                        self.mark_seen(&host, &id);
                        self.sync_asks(cx);
                        self.request_thread_vcs(&host, &id);
                    } else {
                        self.show(Pane::Inbox, cx);
                    }
                }
                self.drop_gone_card(cx);
            }
            ServerMessage::Thread { reply_to, thread_id, entries, .. } => {
                if let Pane::Thread(t) = &mut self.pane
                    && t.host == host
                    && t.id == thread_id
                {
                    let end = entries.last().map(|e| e.seq);
                    let stale = t
                        .fetching
                        .take_if(|(rid, _)| *rid == reply_to)
                        .and_then(|(_, held)| held)
                        .filter(|held| end.is_none_or(|end| *held > end));
                    if let Some(to) = stale
                        && let Err(e) = self.cache.lock().unwrap().remove_entries(&host, &thread_id, end, to)
                    {
                        tracing::error!("cache: {e:#}");
                    }
                    let (kept, fresh) = merge_snapshot(entries, std::mem::take(&mut t.entries), stale);
                    t.timeline = timeline_of(&kept);
                    t.entries = kept;
                    for e in fresh {
                        t.fold(e);
                    }
                    t.relist();
                    self.end_stale_edit(cx);
                    let Pane::Thread(t) = &mut self.pane else { return };
                    if let Some(ix) = t.jump_entry.take().and_then(|e| t.item_at(&e)) {
                        self.jump_to(ix, cx);
                    }
                    if self.open().is_some_and(|t| t.log.is_some()) {
                        self.show_latest_log();
                    }
                    self.fetch_blobs();
                }
            }
            ServerMessage::Usage { reply_to, buckets } => {
                if self.on_upload_usage(reply_to, &buckets, cx) {
                    return;
                }
                if self.usage.request == Some(reply_to) {
                    self.usage.buckets.insert(host, buckets);
                }
            }
            ServerMessage::Proxy { reply_to, proxy } => {
                if let Some(h) = self.host_mut(&host) {
                    h.proxy_update = proxy.update.clone();
                }
                if self.usage.proxy_request == Some(reply_to) {
                    self.usage.proxies.insert(host, proxy);
                } else {
                    self.on_proxy_settings(reply_to, Ok(proxy), cx);
                }
            }
            ServerMessage::Title { reply_to, title, model, input, output, cost, duration_ms } => {
                if self.settings.test_request.take_if(|(id, _)| *id == reply_to).is_some() {
                    self.settings.test_result = Some(Ok(settings::TitleResult { title, model, input, output, cost, duration_ms }));
                }
                if let Some(thread) = self.retitles.remove(&reply_to) {
                    self.titling.remove(&thread);
                }
            }
            ServerMessage::Blob { reply_to, blob_id, mime_type, data } => {
                if let Some(asked) = self.blob_requests.get_mut(&host) {
                    asked.remove(&reply_to);
                }
                if let Some(blob) = decode_blob(&mime_type, &data) {
                    self.blobs.insert(blob_id, blob);
                }
            }
            ServerMessage::Screen { reply_to, frame, total, offset, data, .. } => {
                let piece = crate::screen::Piece { frame, total: total as usize, offset: offset as usize, data: &data };
                self.on_screen(&host, reply_to, &piece, cx)
            }
            ServerMessage::VcsStatus { path, vcs, dirty, branch, refs, .. } => {
                self.vcs.insert((host, path), VcsInfo { vcs, dirty, branch, refs });
            }
            ServerMessage::Focus { thread_id } => self.show_thread(&host, &thread_id, cx),
            ServerMessage::Search { .. } | ServerMessage::Hosts { .. } => {}
            ServerMessage::ThreadCreated { reply_to, thread } => {
                let pending = self.take_pending(&host, reply_to);
                if let Some(h) = self.host_mut(&host)
                    && !h.threads.iter().any(|t| t.id == thread.id)
                {
                    h.threads.insert(0, (*thread).clone());
                }
                if let Some((_, draft)) = self.forks.remove(&reply_to).filter(|(h, _)| *h == host) {
                    if let Some((text, images)) = draft {
                        let content = input::Content { selection: text.len()..text.len(), text, reversed: false };
                        let _ = self.cache.lock().unwrap().set_draft(&host, &thread.id, &content, &image_rows(&images));
                    }
                    self.select_thread(&host, &thread.id, cx);
                }
                if let Some(p) = pending {
                    let open = self.open().is_some_and(|t| t.host == host && t.id == p.thread.id);
                    if thread.id != p.thread.id {
                        if let Some(h) = self.host_mut(&host) {
                            h.threads.retain(|t| t.id != p.thread.id);
                        }
                        self.titling.remove(&(host.clone(), p.thread.id.clone()));
                        self.titling.insert((host.clone(), thread.id.clone()), Instant::now());
                    }
                    if open && thread.id == p.thread.id {
                        self.fetch_open();
                    } else if open {
                        self.select_thread(&host, &thread.id, cx);
                    }
                }
            }
            ServerMessage::Error { reply_to, message } => {
                if reply_to.is_some_and(|id| self.change_answered(&host, id)) {
                    self.request(&host, |id| ClientMessage::ListProjects { id });
                    self.request(&host, |id| ClientMessage::ListThreads { id });
                }
                if let Some(blob) = reply_to.and_then(|id| self.blob_requests.get(&host)?.get(&id)) {
                    tracing::info!(%host, %blob, "blob fetch failed: {message}");
                    return;
                }
                if self.settings.test_request.take_if(|(id, _)| Some(*id) == reply_to).is_some() {
                    self.settings.test_result = Some(Err(message));
                    return;
                }
                if let Some(id) = reply_to
                    && (self.on_login_reply(&host, id, Err(message.clone()), cx)
                        || self.on_proxy_settings(id, Err(message.clone()), cx)
                        || self.on_mcp_reply(&host, id, Err(message.clone()), cx))
                {
                    return;
                }
                // A host that can't say what its proxy is (a service from before `ListProxy`)
                // has none, as far as the Usage modal goes.
                if reply_to.is_some() && reply_to == self.usage.proxy_request {
                    tracing::info!(%host, "no proxy: {message}");
                    self.usage.proxies.insert(host, dirigent_protocol::Proxy::default());
                    return;
                }
                if let Some(thread) = reply_to.and_then(|id| self.retitles.remove(&id)) {
                    self.titling.remove(&thread);
                }
                if let Some(id) = reply_to.filter(|id| self.dir_requests.remove(id)) {
                    if let Some(s) = self.add_project.as_mut().filter(|s| s.list_request == Some(id)) {
                        // An older service doesn't know `ListDirs`.
                        let message = if message.starts_with("bad request") {
                            "This host's service can't list directories (too old). Type a full path.".to_owned()
                        } else {
                            message
                        };
                        s.dirs = Some(Err(message));
                    }
                    return;
                }
                if let Some(f) = self.open_folder.take_if(|f| host == hosts::LOCAL && reply_to.is_some() && f.request == reply_to) {
                    self.toast(format!("Can't open {}: {message}", f.path), cx);
                    return;
                }
                if let Some(s) = self.add_project.as_mut().filter(|s| reply_to.is_some() && s.add_request == reply_to) {
                    s.add_request = None;
                    s.error = Some(message);
                    return;
                }
                if let Some(p) = reply_to.and_then(|id| self.take_pending(&host, id)) {
                    self.unsend_draft(p, cx);
                }
                if let Some(p) = reply_to.and_then(|id| self.prompts.remove(&id)) {
                    self.unsend_prompt(p, cx);
                }
                if self.open().and_then(|t| t.rewound.as_ref()).is_some_and(|r| Some(r.request) == reply_to) {
                    self.unrewind(cx);
                }
                if let Some(id) = reply_to {
                    self.forks.remove(&id);
                }
                // A request newer than the host's service (rename, retitle, ...): serde's
                // list of every variant it does know says little.
                let message = if message.starts_with("bad request: unknown variant") {
                    "its service is too old for that; update it".to_owned()
                } else {
                    message
                };
                self.toast(format!("{host}: {message}"), cx)
            }
            ServerMessage::Entry { thread_id, entry } => {
                if let Pane::Thread(t) = &mut self.pane
                    && t.host == host
                    && t.id == thread_id
                {
                    // MUST KILL: extract: an OpenThread method for a push a snapshot already folded in
                    if t.entries.last().is_some_and(|e| entry.seq <= e.seq) {
                        if matches!(entry.body, Body::Assistant(_)) && !t.streaming.steps.is_empty() {
                            t.streaming.steps.clear();
                            let tail = t.timeline.items.len();
                            t.list.remeasure_items(tail..tail + 1);
                            if t.log.is_some() {
                                t.log_list.remeasure();
                            }
                        }
                        return;
                    }
                    let (n, changed) = t.fold(entry);
                    let m = t.timeline.items.len();
                    if t.log == Some(thread::TAIL) {
                        t.log = Some(t.timeline.open_work().unwrap_or(thread::TAIL));
                    }
                    if m > n {
                        t.list.splice(n..n, m - n);
                    } else if m < n {
                        t.list.splice(m..n, 0);
                    }
                    if let Some(c) = changed {
                        t.list.remeasure_items(c..n.min(m));
                    }
                    t.new_output |= (m != n || changed.is_some()) && !t.list.is_following_tail();
                    t.list.remeasure_items(m..m + 1);
                    if t.log.is_some() {
                        t.log_list.remeasure();
                    }
                    self.end_stale_edit(cx);
                    self.fetch_blobs();
                }
            }
            ServerMessage::Event { thread_id, event } => {
                match &event {
                    Event::Warning { message } => {
                        self.titling.remove(&(host.clone(), thread_id.clone()));
                        self.thread_toast(&host, &thread_id, message, cx);
                    }
                    Event::Titled if !self.retitling(&host, &thread_id) => {
                        self.titling.remove(&(host.clone(), thread_id.clone()));
                    }
                    Event::Screen { .. } => return self.on_screen_event(&host, &thread_id, &event),
                    _ => {}
                }
                if let Pane::Thread(t) = &mut self.pane
                    && t.host == host
                    && t.id == thread_id
                    && t.streaming.apply(&event, now_ms())
                {
                    t.new_output |= !t.list.is_following_tail();
                    let tail = t.timeline.items.len();
                    t.list.remeasure_items(tail..tail + 1);
                    if t.log.is_some() {
                        t.log_list.remeasure();
                        if t.timeline.begins_work(&event) {
                            self.show_latest_log();
                        }
                    }
                }
            }
        }
    }

    pub fn show(&mut self, pane: Pane, cx: &mut Context<Self>) {
        self.set_pane(pane, cx);
        self.menu = None;
        self.selection.clear();
        self.remember_pane();
        self.focus(self.focus_handle.clone(), cx);
        cx.notify();
    }

    fn set_pane(&mut self, pane: Pane, cx: &mut Context<Self>) {
        self.renaming.take_if(|r| matches!(r, Renaming::Project(_)));
        self.add_project = None;
        let (from, to) = (composer_key(&self.pane), composer_key(&pane));
        let (leaving, entering) = (from != to && from.is_some(), from != to && to.is_some());
        if leaving {
            self.save_draft(cx);
        } else {
            self.end_edit(cx);
        }
        self.stash_log();
        self.drop_screen(cx);
        if let Pane::Thread(t) = &mut self.pane {
            let stale = t.visuals.clear();
            self.drop_images(stale, cx);
        }
        self.pane = pane;
        self.hold_effort();
        if entering {
            self.load_composer(cx);
        }
    }

    fn remember_pane(&self) {
        let value = match &self.pane {
            Pane::Inbox => "inbox".to_owned(),
            Pane::Workpool => "workpool".to_owned(),
            Pane::Project { group_key } => format!("project\n{group_key}"),
            Pane::Thread(t) => format!("thread\n{}\n{}", t.host, t.id),
            Pane::Projects | Pane::Draft { .. } => return,
        };
        let _ = self.cache.lock().unwrap().set(PANE_KEY, &value);
    }

    fn restore_pane(&mut self, cx: &mut Context<Self>) {
        let Some(value) = self.cache.lock().unwrap().get(PANE_KEY) else { return };
        self.open_pane(&value, cx);
    }

    fn go_back(&mut self, cx: &mut Context<Self>) {
        let back = self.cache.lock().unwrap().get(PANE_KEY);
        self.show(Pane::Inbox, cx);
        if let Some(value) = back {
            self.open_pane(&value, cx);
            self.remember_pane();
        }
    }

    fn open_pane(&mut self, value: &str, cx: &mut Context<Self>) {
        let parts: Vec<&str> = value.split('\n').collect();
        match parts.as_slice() {
            ["workpool"] => self.set_pane(Pane::Workpool, cx),
            ["project", key] => self.set_pane(Pane::Project { group_key: (*key).to_owned() }, cx),
            ["thread", host, id] if self.summary(host, id).is_some() => {
                let (host, id) = ((*host).to_owned(), (*id).to_owned());
                self.select_thread(&host, &id, cx);
            }
            _ => {}
        }
    }

    fn open_app_link(&mut self, link: &OpenAppLink, _: &mut Window, cx: &mut Context<Self>) {
        if let Some(id) = link.url.strip_prefix("dirigent://image/") {
            return match self.blobs.get(id) {
                Some(blob) => self.set_preview(Some(blob.image.clone()), cx),
                None => self.toast("The image hasn't loaded yet", cx),
            };
        }
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

    pub fn show_thread(&mut self, host: &str, id: &str, cx: &mut Context<Self>) {
        if self.summary(host, id).is_some() {
            self.modal = None;
            self.select_thread(host, id, cx);
        }
    }

    pub fn select_thread(&mut self, host: &str, id: &str, cx: &mut Context<Self>) {
        self.stash_log();
        let entries = self.cache.lock().unwrap().entries(host, id).unwrap_or_default();
        if let Some((m, e)) = self.summary(host, id).filter(|s| s.model.is_some()).map(|s| (s.model.clone(), s.effort.clone())) {
            self.model = m;
            self.effort = e;
        }
        let mut thread = OpenThread::new(host, id, entries);
        thread.expanded = self.logs.get(id).map(|m| m.expanded.clone()).unwrap_or_default();
        thread.pins = self.summary(host, id).map(|s| s.pinned_entries.iter().cloned().collect()).unwrap_or_default();
        self.set_pane(Pane::Thread(Box::new(thread)), cx);
        if self.log_on {
            self.show_latest_log();
        }
        self.menu = None;
        self.selection.clear();
        self.remember_pane();
        self.mark_seen(host, id);
        self.sync_asks(cx);
        self.fetch_blobs();
        if !self.is_pending(host, id) {
            self.fetch_open();
        }
        self.request_thread_vcs(host, id);
        self.focus_composer(cx);
        cx.notify();
    }

    fn fetch_open(&mut self) {
        let Some(host) = self.open().map(|t| t.host.clone()) else { return };
        if self.host(&host).is_none_or(|h| h.status != HostStatus::Connected) {
            return;
        }
        let rid = self.next_id();
        let Pane::Thread(t) = &mut self.pane else { return };
        t.fetching = Some((rid, t.entries.last().map(|e| e.seq)));
        self.net.send(&host, ClientMessage::GetThread { id: rid, thread_id: t.id.clone() });
    }

    pub fn request_vcs(&mut self, host: &str, path: &str) {
        if self.host(host).is_none_or(|h| h.status != HostStatus::Connected) {
            return;
        }
        let id = self.next_id();
        self.net.send(host, ClientMessage::VcsStatus { id, path: path.to_owned() });
    }

    fn request_thread_vcs(&mut self, host: &str, id: &str) {
        let Some(s) = self.summary(host, id) else { return };
        let workspace = s.workspace.clone().filter(|_| !s.workspace_missing);
        let paths: Vec<String> = self.project_path(host, s).into_iter().chain(workspace).collect();
        for p in paths {
            self.request_vcs(host, &p);
        }
    }

    fn request_sidebar_vcs(&mut self, host: &str, ran: &HashSet<String>) {
        let mut paths: Vec<String> = vec![];
        for (h, t) in self.inbox_threads().into_iter().chain(self.busy_threads()) {
            if h != host || t.workspace.is_some() {
                continue;
            }
            let ended = ran.contains(&t.id) && !t.running();
            if (ended || self.vcs_of(host, &t.cwd).is_none()) && !paths.contains(&t.cwd) {
                paths.push(t.cwd.clone());
            }
        }
        for p in paths {
            self.request_vcs(host, &p);
        }
    }

    pub fn vcs_of(&self, host: &str, path: &str) -> Option<&VcsInfo> {
        self.vcs.get(&(host.to_owned(), path.to_owned()))
    }

    pub fn draft_vcs(&self) -> Option<(&Target, &str, Option<&VcsInfo>)> {
        let Pane::Draft { target: Some(t) } = &self.pane else { return None };
        let path = self.host(&t.host)?.projects.iter().find(|p| p.id == t.project_id)?.path.as_str();
        Some((t, path, self.vcs_of(&t.host, path)))
    }

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

    pub fn toggle_base_picker(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.menu == Some(Menu::Base) {
            self.close_menu(cx);
        } else if self.open_menu(Menu::Base, cx) {
            self.base_search.update(cx, |input, cx| input.set_text("", window, cx));
            self.focus(self.base_search.focus_handle(cx), cx);
        }
    }

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

    pub fn filtered_refs(&self, cx: &App) -> Vec<String> {
        let query = self.base_search.read(cx).text().to_lowercase();
        self.base_refs().into_iter().filter(|r| r.to_lowercase().contains(&query)).collect()
    }

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

    pub fn move_to_workspace(&mut self, host: &str, thread_id: &str, base: Option<String>, cx: &mut Context<Self>) {
        let id = self.next_id();
        self.net.send(host, ClientMessage::MoveToWorkspace { id, thread_id: thread_id.to_owned(), base });
        self.menu = None;
        cx.notify();
    }

    pub fn remove_workspace(&mut self, host: &str, thread_id: &str, detach: bool, cx: &mut Context<Self>) {
        let id = self.next_id();
        self.net.send(host, ClientMessage::RemoveWorkspace { id, thread_id: thread_id.to_owned(), detach });
        self.menu = None;
        cx.notify();
    }

    pub fn archive_with_workspace(&mut self, host: &str, thread_id: &str, detach: bool, cx: &mut Context<Self>) {
        if !self.connected(host) {
            return self.toast(format!("{host} is offline"), cx);
        }
        self.remove_workspace(host, thread_id, detach, cx);
        if let Some(t) = self.host_mut(host).and_then(|h| h.threads.iter_mut().find(|t| t.id == thread_id)) {
            t.workspace = None;
        }
        self.set_archived(host, thread_id, true, ArchiveAt::Header, cx);
    }

    fn tick(&mut self, cx: &mut Context<Self>) {
        cx.spawn(async move |this, cx| {
            loop {
                cx.background_executor().timer(until_next_second()).await;
                let alive = this.update(cx, |this, cx| {
                    let second = this_second_ms();
                    let woke_early = second == this.clock;
                    if woke_early {
                        return;
                    }
                    this.clock = second;
                    this.ticks += 1;
                    if this.ticks % USAGE_REFRESH_SECS == 0 && this.modal == Some(Modal::Usage) {
                        match this.usage.tab {
                            usage::Tab::Mine if this.usage.at_now() => this.request_usage(cx),
                            usage::Tab::Board => this.fetch_board(cx),
                            usage::Tab::Proxy => this.request_proxy(cx),
                            _ => {}
                        }
                    }
                    if this.ticks % account::CHECK_EVERY == 0 {
                        this.sync_forge(cx);
                    }
                    if this.ticks % leaderboard::UPLOAD_EVERY == 0 {
                        this.upload_all();
                    }
                    if this.ticks % PROXY_UPDATE_CHECK_SECS == 0 {
                        this.check_proxy_updates();
                    }
                    if this.watch.changed() {
                        this.reload_config(cx);
                    }
                    this.titling.retain(|_, since| since.elapsed() < TITLING_TTL);
                    let titling = &this.titling;
                    this.retitles.retain(|_, thread| titling.contains_key(thread));
                    this.tick_update(cx);
                    this.redraw_field(this.ticks.is_multiple_of(ambient::CLOCK_SECS), cx);
                    if this.ticks.is_multiple_of(VCS_REFRESH_SECS) {
                        match &this.pane {
                            Pane::Draft { .. } => this.refresh_draft_vcs(),
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

    pub fn focus_composer(&self, cx: &mut Context<Self>) {
        self.focus(self.composer.focus_handle(cx), cx);
    }

    pub fn focus_root(&self, cx: &mut Context<Self>) {
        self.focus(self.focus_handle.clone(), cx);
    }

    pub fn focus(&self, handle: FocusHandle, cx: &mut Context<Self>) {
        let window = self.window;
        cx.defer(move |cx| {
            window.update(cx, |_, window, cx| window.focus(&handle, cx)).ok();
        });
    }

    pub fn new_draft(&mut self, target: Option<Target>, cx: &mut Context<Self>) {
        if let Some(t) = target.filter(|t| self.target_valid(t)) {
            self.remember_target(&t);
        }
        let target = self.last_target().filter(|t| self.target_valid(t));
        (self.model, self.effort) = {
            let cache = self.cache.lock().unwrap();
            (cache.get(LAST_MODEL_KEY), cache.get(LAST_EFFORT_KEY))
        };
        self.set_pane(Pane::Draft { target }, cx);
        self.menu = None;
        self.selection.clear();
        self.refresh_draft_vcs();
        self.focus_composer(cx);
        cx.notify();
    }

    pub fn new_thread(&mut self, _: &NewThread, _: &mut Window, cx: &mut Context<Self>) {
        if self.add_project.is_some() || self.modal.is_some() || self.onboarding_open() {
            return;
        }
        self.new_draft(None, cx);
    }

    fn remember_target(&mut self, target: &Target) {
        if self.last_target().as_ref() != Some(target) {
            self.base = None;
        }
        let _ = self.cache.lock().unwrap().set(LAST_TARGET_KEY, &format!("{}\n{}", target.host, target.project_id));
    }

    pub(crate) fn last_target(&self) -> Option<Target> {
        let v = self.cache.lock().unwrap().get(LAST_TARGET_KEY)?;
        let (host, project_id) = v.split_once('\n')?;
        Some(Target { host: host.into(), project_id: project_id.into() })
    }

    pub(crate) fn target_valid(&self, t: &Target) -> bool {
        self.host(&t.host).is_some_and(|h| h.projects.iter().any(|p| p.id == t.project_id))
    }

    pub fn toggle_target_picker(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.menu == Some(Menu::Target) {
            self.close_menu(cx);
        } else if self.open_menu(Menu::Target, cx) {
            self.clear_target_search(window, cx);
        }
    }

    fn clear_target_search(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.groups().0.is_empty() {
            return;
        }
        self.target_search.update(cx, |input, cx| input.set_text("", window, cx));
        self.focus(self.target_search.focus_handle(cx), cx);
    }

    pub fn filtered_targets(&self, cx: &App) -> Vec<projects::Group> {
        projects::search(self.groups().0, self.target_search.read(cx).text())
    }

    pub fn set_target(&mut self, target: Target, cx: &mut Context<Self>) {
        self.remember_target(&target);
        if let Pane::Draft { target: t } = &mut self.pane {
            *t = Some(target);
        }
        self.hold_effort();
        self.menu = None;
        self.refresh_draft_vcs();
        self.focus_composer(cx);
        cx.notify();
    }

    fn refresh_draft_vcs(&mut self) {
        if let Some((t, path, _)) = self.draft_vcs() {
            let (host, path) = (t.host.clone(), path.to_owned());
            self.request_vcs(&host, &path);
        }
    }

    pub fn set_archived(&mut self, host: &str, thread_id: &str, archived: bool, at: ArchiveAt, cx: &mut Context<Self>) {
        if archived && !self.summary(host, thread_id).is_some_and(|t| self.at_rest(host, t)) {
            return;
        }
        let asking = matches!(&self.menu, Some(Menu::ConfirmArchive { host: h, id, .. }) if h == host && id == thread_id);
        if archived && !asking && self.connected(host) && self.summary(host, thread_id).is_some_and(|t| t.workspace.is_some()) {
            if self.open_menu(Menu::ConfirmArchive { host: host.to_owned(), id: thread_id.to_owned(), at }, cx) {
                self.request_thread_vcs(host, thread_id);
            }
            return;
        }
        let Some(updated_at) = self.summary(host, thread_id).map(|t| t.updated_at) else { return };
        self.change(host, Change::Archive { thread_id: thread_id.to_owned(), archived, updated_at });
        self.menu = None;
        cx.notify();
    }

    pub fn delete_thread(&mut self, host: &str, thread_id: &str, cx: &mut Context<Self>) {
        self.change(host, Change::Delete { thread_id: thread_id.to_owned() });
        if self.open().is_some_and(|t| t.host == host && t.id == thread_id) {
            self.show(Pane::Inbox, cx);
        }
        let _ = self.cache.lock().unwrap().remove_draft(host, thread_id);
        self.menu = None;
        cx.notify();
    }

    pub fn serve_launches(
        &mut self,
        open: Option<String>,
        launches: Option<futures::channel::mpsc::UnboundedReceiver<instance::Launch>>,
        cx: &mut Context<Self>,
    ) {
        if let Some(path) = open {
            self.open_folder = Some(OpenFolder { path, request: None });
            self.resume_open_folder(cx);
        }
        let Some(mut launches) = launches else { return };
        cx.spawn(async move |this, cx| {
            while let Some(launch) = launches.next().await {
                if this.update(cx, |this, cx| this.on_launch(launch, cx)).is_err() {
                    break;
                }
            }
        })
        .detach();
    }

    fn on_launch(&mut self, launch: instance::Launch, cx: &mut Context<Self>) {
        self.window.update(cx, |_, window, _| window.activate_window()).ok();
        if let Some(path) = launch.open {
            self.open_folder = Some(OpenFolder { path, request: None });
            self.resume_open_folder(cx);
        }
        cx.notify();
    }

    fn resume_open_folder(&mut self, cx: &mut Context<Self>) {
        let Some(f) = &self.open_folder else { return };
        let Some(local) = self.host(hosts::LOCAL) else { return };
        if let Some(project) = local.projects.iter().find(|p| projects::same_local_path(&p.path, &f.path)) {
            let target = Target { host: hosts::LOCAL.to_owned(), project_id: project.id.clone() };
            self.open_folder = None;
            self.modal = None;
            self.add_project = None;
            self.new_draft(Some(target), cx);
        } else if f.request.is_none() && local.status == HostStatus::Connected {
            let path = f.path.clone();
            let id = self.next_id();
            self.net.send(hosts::LOCAL, ClientMessage::AddProject { id, path });
            if let Some(f) = &mut self.open_folder {
                f.request = Some(id);
            }
        }
    }

    pub fn show_projects(&mut self, cx: &mut Context<Self>) {
        self.show(Pane::Projects, cx);
        if self.groups().0.is_empty() {
            self.open_add_project(cx);
        }
    }

    pub fn new_project_host(&self) -> Option<String> {
        let picked = self.new_project_host.as_ref().filter(|h| self.hosts.iter().any(|s| s.name == **h));
        picked
            .cloned()
            .or_else(|| self.hosts.iter().find(|h| h.status == HostStatus::Connected).or(self.hosts.first()).map(|h| h.name.clone()))
    }

    pub fn pick_new_project_host(&mut self, host: String, cx: &mut Context<Self>) {
        self.new_project_host = Some(host);
        self.menu = None;
        self.set_project_path("~/".to_owned(), cx);
        self.focus(self.path_input.focus_handle(cx), cx);
        cx.notify();
    }

    pub fn open_add_project(&mut self, cx: &mut Context<Self>) {
        self.renaming = None;
        self.add_project = Some(add_project::State::default());
        self.new_project_host = self.new_project_host();
        self.menu = None;
        self.set_project_path("~/".to_owned(), cx);
        self.focus(self.path_input.focus_handle(cx), cx);
        cx.notify();
    }

    pub fn set_project_path(&mut self, text: String, cx: &mut Context<Self>) {
        let (window, input, this) = (self.window, self.path_input.clone(), cx.entity().downgrade());
        cx.defer(move |cx| {
            window.update(cx, |_, window, cx| input.update(cx, |i, cx| i.set_text(&text, window, cx))).ok();
            this.update(cx, |this, cx| this.project_path_changed(cx)).ok();
        });
    }

    fn project_path_changed(&mut self, cx: &mut Context<Self>) {
        if let Some(s) = &mut self.add_project {
            s.selected = 0;
            s.error = None;
            add_project::reveal(s);
        }
        self.request_dirs(false, cx);
        cx.notify();
    }

    fn request_dirs(&mut self, force: bool, cx: &mut Context<Self>) {
        let text = self.path_input.read(cx).text().to_owned();
        let (dir, _) = add_project::split(&text);
        let host = self.new_project_host().unwrap_or_default();
        let id = self.next_id();
        let Some(s) = &mut self.add_project else { return };
        if !force && s.host == host && s.dir == dir && s.list_request.is_some() {
            return;
        }
        (s.host, s.dir, s.dirs) = (host.clone(), dir.to_owned(), None);
        s.list_request = (!dir.is_empty()).then_some(id);
        if !dir.is_empty() {
            self.dir_requests.insert(id);
            self.net.send(&host, ClientMessage::ListDirs { id, path: dir.to_owned() });
        }
    }

    fn move_dir_selection(&mut self, by: i32, cx: &mut Context<Self>) {
        let text = self.path_input.read(cx).text().to_owned();
        let Some(s) = &mut self.add_project else { return };
        let n = s.visible(&text).len();
        if n > 0 {
            s.selected = (s.selected as i32 + by).rem_euclid(n as i32) as usize;
            add_project::reveal(s);
            cx.notify();
        }
    }

    fn enter_dir(&mut self, cx: &mut Context<Self>) {
        let text = self.path_input.read(cx).text().to_owned();
        let Some(s) = &self.add_project else { return };
        if let Some(d) = s.visible(&text).get(s.selected) {
            let path = format!("{}{}/", add_project::split(&text).0, d.name);
            self.set_project_path(path, cx);
        }
    }

    pub fn confirm_add_project(&mut self, cx: &mut Context<Self>) {
        let path = self.path_input.read(cx).text().trim().to_owned();
        let Some(host) = self.new_project_host().filter(|_| !path.is_empty()) else { return };
        let connected = self.host(&host).is_some_and(|h| h.status == HostStatus::Connected);
        let id = self.next_id();
        let Some(s) = self.add_project.as_mut().filter(|s| s.link.is_none() && s.add_request.is_none()) else { return };
        if !connected {
            s.error = Some(format!("{host} is not connected"));
            return cx.notify();
        }
        s.add_request = Some(id);
        s.error = None;
        self.net.send(&host, ClientMessage::AddProject { id, path });
        cx.notify();
    }

    pub fn finish_add_project(&mut self, target: Option<String>, cx: &mut Context<Self>) {
        if self.add_project.as_ref().is_none_or(|s| s.link.is_none()) {
            return;
        }
        let Some(offer) = self.add_project.take().and_then(|s| s.link) else { return };
        if let Some(target) = target {
            self.link_group(&offer.key, Some(&target), cx);
        }
        self.focus(self.focus_handle.clone(), cx);
        cx.notify();
    }

    pub(crate) fn close_add_project(&mut self, cx: &mut Context<Self>) {
        if self.add_project.as_ref().is_some_and(|s| s.link.is_some()) {
            return self.finish_add_project(None, cx);
        }
        self.add_project = None;
        self.focus(self.focus_handle.clone(), cx);
        cx.notify();
    }

    pub fn dim_pressed(&mut self, cx: &mut Context<Self>) {
        if self.menu_dismissed {
            return;
        }
        if self.add_project.is_some() {
            self.close_add_project(cx);
        } else {
            self.close_modal(cx);
        }
    }

    pub fn open_settings(&mut self, cx: &mut Context<Self>) {
        self.modal = Some(Modal::Settings);
        self.menu = None;
        self.fill_settings(cx);
        self.set_settings_tab(self.settings.tab, cx);
    }

    pub fn set_settings_tab(&mut self, tab: settings::Tab, cx: &mut Context<Self>) {
        self.settings.tab = tab;
        if tab == settings::Tab::Hosts {
            self.sync_hosts(cx);
            match self.hosts_tab.form {
                Some(_) => self.focus(self.hosts_tab.name.focus_handle(cx), cx),
                None => self.focus_root(cx),
            }
        } else if tab == settings::Tab::Proxy {
            self.enter_proxy_settings(cx);
        } else if tab == settings::Tab::Mcp {
            self.enter_mcp(cx);
        } else {
            if tab == settings::Tab::Forge {
                self.fetch_forge_picture(cx);
            }
            self.focus(self.focus_handle.clone(), cx);
        }
        cx.notify();
    }

    pub fn open_usage(&mut self, cx: &mut Context<Self>) {
        self.modal = Some(Modal::Usage);
        self.menu = None;
        self.focus(self.focus_handle.clone(), cx);
        self.request_usage(cx);
        if self.usage.tab == usage::Tab::Board {
            self.fetch_board(cx);
        }
    }

    pub fn open_changelog(&mut self, cx: &mut Context<Self>) {
        self.modal = Some(Modal::Changelog);
        self.menu = None;
        self.focus(self.focus_handle.clone(), cx);
        cx.notify();
    }

    fn close_modal(&mut self, cx: &mut Context<Self>) {
        self.modal = None;
        self.menu = None;
        self.cancel_login(cx);
        self.hosts_tab.form = None;
        self.drop_mcp_form();
        self.usage.hover = None;
        self.selection.clear();
        if composer_key(&self.pane).is_some() {
            self.focus_composer(cx);
        } else {
            self.focus(self.focus_handle.clone(), cx);
        }
        cx.notify();
    }

    fn fill_settings(&self, cx: &mut Context<Self>) {
        let (window, settings, config) = (self.window, self.settings.clone(), self.config.clone());
        cx.defer(move |cx| {
            window.update(cx, |_, window, cx| settings.fill(&config, window, cx)).ok();
        });
    }

    fn apply_config(&mut self, config: config::Config, cx: &mut Context<Self>) -> Vec<String> {
        let families_changed = config.fonts.families() != self.config.fonts.families();
        self.config = config;
        match theme::load(&self.config.theme) {
            Ok(t) => self.set_theme_now(t, cx),
            Err(e) => self.toast(format!("{e:#}"), cx),
        }
        let mut missing = vec![];
        if families_changed {
            (self.fonts, missing) = self.config.fonts.resolve(cx);
            self.settings.missing_fonts = missing.clone();
        }
        if theme::set_text_size(self.config.fonts.size())
            && let Pane::Thread(t) = &self.pane
        {
            t.list.remeasure();
            t.log_list.remeasure();
        }
        cx.notify();
        missing
    }

    fn reload_config(&mut self, cx: &mut Context<Self>) {
        match config::load() {
            Ok(c) => {
                for font in self.apply_config(c, cx) {
                    self.toast(format!("font `{font}` is not installed"), cx);
                }
                if self.modal == Some(Modal::Settings) {
                    self.fill_settings(cx);
                }
            }
            Err(e) => self.toast(format!("{e:#}"), cx),
        }
    }

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

    pub fn step_text_size(&mut self, delta: f32, cx: &mut Context<Self>) {
        let size = self.config.fonts.size();
        let next = if delta > 0. { size.floor() + 1. } else { size.ceil() - 1. };
        self.set_text_size(next.clamp(config::MIN_SIZE, config::MAX_SIZE), cx);
    }

    fn set_text_size(&mut self, size: f32, cx: &mut Context<Self>) {
        if size != self.config.fonts.size() {
            self.edit_config(|c| c.fonts.size = size, cx);
        }
    }

    fn text_bigger(&mut self, _: &TextBigger, _: &mut Window, cx: &mut Context<Self>) {
        self.step_text_size(1., cx);
    }

    fn text_smaller(&mut self, _: &TextSmaller, _: &mut Window, cx: &mut Context<Self>) {
        self.step_text_size(-1., cx);
    }

    fn text_reset(&mut self, _: &TextReset, _: &mut Window, cx: &mut Context<Self>) {
        self.set_text_size(theme::DESIGN_SIZE, cx);
    }

    pub fn toggle_telemetry(&mut self, cx: &mut Context<Self>) {
        self.edit_config(|c| c.telemetry = !c.telemetry, cx);
    }

    pub fn set_background(&mut self, pattern: Pattern, cx: &mut Context<Self>) {
        self.edit_config(|c| c.background.pattern = pattern, cx);
    }

    pub fn toggle_background_reach(&mut self, cx: &mut Context<Self>) {
        let reach = if self.config.background.reach == Reach::Full { Reach::Edge } else { Reach::Full };
        self.edit_config(|c| c.background.reach = reach, cx);
    }

    fn field_spec(&self, window: &Window) -> Option<ambient::Spec> {
        let bg = self.config.background;
        if bg.pattern == Pattern::Off {
            return None;
        }
        let view = window.viewport_size();
        let width = if self.onboarding.is_some() || bg.reach == Reach::Full {
            view.width
        } else {
            u(self.sidebar_width + 2. * theme::GUTTER).min(view.width)
        };
        Some(ambient::Spec::new(bg.pattern, f32::from(width), f32::from(view.height), window.scale_factor(), theme::t().pattern))
    }

    fn redraw_field(&mut self, tick: bool, cx: &mut Context<Self>) {
        let Some(spec) = self.field.due(tick) else { return };
        let now = std::time::UNIX_EPOCH.elapsed().map(|d| d.as_secs_f64() * 1000.).unwrap_or(0.);
        let image = cx.background_spawn(async move { ambient::draw(spec, now) });
        cx.spawn(async move |this, cx| {
            let image = image.await;
            this.update(cx, |this, cx| {
                if let Some(old) = this.field.show(spec, image) {
                    cx.drop_image(old, None);
                }
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    pub fn render_with_visuals(&mut self, ix: usize, cx: &mut Context<Self>) -> AnyElement {
        let rendered = thread::render_item(self, ix, cx);
        let Pane::Thread(t) = &mut self.pane else { return rendered };
        let draw = |(html, spec): (gpui::SharedString, visual::Spec)| {
            let drawing = cx.background_spawn({
                let (html, spec) = (html.clone(), spec.clone());
                async move { visual::draw(&html, &spec) }
            });
            (html, spec, drawing)
        };
        let drawings: Vec<_> = t.visuals.due().into_iter().map(draw).collect();
        let deadline = Instant::now() + VISUAL_WAIT;
        let mut drawn_in_time = false;
        for (html, spec, drawing) in drawings {
            let left = deadline.saturating_duration_since(Instant::now());
            match cx.foreground_executor().block_with_timeout(left, drawing) {
                Ok(picture) => {
                    drawn_in_time |= picture.is_some();
                    self.show_visual(html, &spec, picture, cx);
                }
                Err(drawing) => cx
                    .spawn(async move |this, cx| {
                        let picture = drawing.await;
                        this.update(cx, |this, cx| {
                            this.show_visual(html, &spec, picture, cx);
                            cx.notify();
                        })
                        .ok();
                    })
                    .detach(),
            }
        }
        if drawn_in_time { thread::render_item(self, ix, cx) } else { rendered }
    }

    fn show_visual(&mut self, html: gpui::SharedString, spec: &visual::Spec, picture: Option<visual::Picture>, cx: &mut Context<Self>) {
        let Pane::Thread(t) = &mut self.pane else { return };
        let stale = t.visuals.show(html, spec, picture);
        self.drop_images(stale.into_iter().collect(), cx);
    }

    /// Takes `images` out of gpui's atlas as the next frame starts: until then the frame on
    /// screen may show them.
    fn drop_images(&self, images: Vec<visual::StaleImage>, cx: &mut Context<Self>) {
        if images.is_empty() {
            return;
        }
        let handle = self.window;
        cx.defer(move |cx| {
            let drop = |window: &mut Window, cx: &mut App| images.into_iter().for_each(|image| cx.drop_image(image.0, Some(&mut *window)));
            handle.update(cx, |_, window, _| window.on_next_frame(drop)).ok();
        });
    }

    pub fn toggle_tool(&mut self, name: &str, cx: &mut Context<Self>) {
        self.edit_config(
            |c| match name {
                "async_bash" => c.tools.async_bash = !c.tools.async_bash,
                "bash_background" => c.tools.bash_background = !c.tools.bash_background,
                "ask_user" => c.tools.ask_user = !c.tools.ask_user,
                "dirigent" => c.tools.dirigent = !c.tools.dirigent,
                "computer" => c.tools.computer = !c.tools.computer,
                "computer_cursor" => c.tools.computer_cursor = !c.tools.computer_cursor,
                "codemode" => c.tools.codemode = !c.tools.codemode,
                _ => {}
            },
            cx,
        );
    }

    pub fn panel_open(&self) -> bool {
        let Some(t) = self.open() else { return false };
        t.panel.unwrap_or_else(|| {
            t.screen.is_some() || t.log.is_some() || (self.panel_shows(PanelPart::Lineage) && !self.lineage(&t.host, &t.id).is_empty())
        })
    }

    pub fn panel_shows(&self, part: PanelPart) -> bool {
        self.panel_hidden != Some(part)
    }

    pub fn toggle_panel_part(&mut self, part: PanelPart, cx: &mut Context<Self>) {
        let hidden = match self.panel_hidden {
            Some(p) if p == part => None,
            Some(_) => return,
            None => Some(part),
        };
        if let Pane::Thread(t) = &mut self.pane {
            t.panel = Some(true);
        }
        self.set_panel_hidden(hidden);
        self.focus_composer(cx);
        cx.notify();
    }

    fn set_panel_hidden(&mut self, hidden: Option<PanelPart>) {
        self.panel_hidden = hidden;
        let key = match hidden {
            Some(PanelPart::Lineage) => "lineage",
            Some(PanelPart::Log) => "log",
            None => "",
        };
        let _ = self.cache.lock().unwrap().set(PANEL_HIDDEN_KEY, key);
        if hidden == Some(PanelPart::Log) {
            self.stash_log();
            if let Pane::Thread(t) = &mut self.pane {
                t.log = None;
            }
        }
    }

    pub fn toggle_panel(&mut self, cx: &mut Context<Self>) {
        if self.panel_open() {
            self.close_panel();
        } else if let Pane::Thread(t) = &mut self.pane {
            t.panel = Some(true);
            self.log_on = true;
            self.show_latest_log();
        }
        cx.notify();
    }

    fn close_panel(&mut self) {
        self.stash_log();
        if let Pane::Thread(t) = &mut self.pane {
            t.panel = Some(false);
            t.log = None;
            self.log_on = false;
        }
    }

    // MUST KILL: rearchitect: point the log at the latest work where the panel opens, not by a per-frame fix-up
    fn settle_panel(&mut self) {
        if self.panel_open() && self.open().is_some_and(|t| t.log.is_none()) {
            self.show_latest_log();
        }
    }

    fn latest_work(&self) -> Option<usize> {
        let t = self.open()?;
        match t.timeline.open_work() {
            Some(i) => Some(i),
            None if self.is_running() => Some(thread::TAIL),
            None => t.timeline.items.iter().rposition(|i| matches!(i, Item::Work(_))),
        }
    }

    fn show_latest_log(&mut self) {
        if !self.panel_shows(PanelPart::Log) {
            return;
        }
        if let Some(key) = self.latest_work() {
            self.show_log(key);
        }
    }

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
                        cx.subscribe(&input, {
                            let ask_id = ask_id.clone();
                            move |this, _, _: &Submitted, cx| this.next_ask_tab(&ask_id, cx)
                        })
                        .detach();
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

    fn with_ask(&mut self, ask_id: &str, cx: &mut Context<Self>, f: impl FnOnce(&mut AskForm)) {
        if let Pane::Thread(t) = &mut self.pane
            && let Some(form) = t.asks.iter_mut().find(|f| f.ask.id == ask_id && !f.sent)
        {
            f(form);
            cx.notify();
        }
    }

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

    pub fn answer_ask(&mut self, ask_id: &str, cx: &mut Context<Self>) {
        self.send_ask(ask_id, true, cx);
    }

    pub fn dismiss_ask(&mut self, ask_id: &str, cx: &mut Context<Self>) {
        self.send_ask(ask_id, false, cx);
    }

    pub fn insert_ask(&mut self, ask_id: &str, window: &Window, cx: &mut Context<Self>) {
        let id = self.next_id();
        let Pane::Thread(t) = &mut self.pane else { return };
        let Some(form) = t.asks.iter_mut().find(|f| f.ask.id == ask_id && !f.sent) else { return };
        form.sent = true;
        let (draft, cursor) = view::draft_reply(&form.ask.questions, &form.answers(cx));
        let (host, thread_id) = (t.host.clone(), t.id.clone());
        self.net.send(&host, ClientMessage::Answer { id, thread_id, ask_id: ask_id.to_owned(), answers: None, silent: true });
        self.composer.update(cx, |input, cx| {
            let before = input.text().trim_end();
            let before = if before.is_empty() { String::new() } else { format!("{before}\n\n") };
            input.set_text(&format!("{before}{draft}"), window, cx);
            input.move_to(before.len() + cursor, window, cx);
        });
        self.save_draft(cx);
        self.focus_composer(cx);
        cx.notify();
    }

    pub fn set_ask_height(&mut self, h: Pixels, window: &mut Window, cx: &mut Context<Self>) {
        let Pane::Thread(t) = &mut self.pane else { return };
        if (t.ask_h - h).abs() < px(0.5) {
            return;
        }
        t.ask_h = h;
        let tail = t.timeline.items.len();
        t.list.remeasure_items(tail..tail + 1);
        cx.defer_in(window, |_, _, cx| cx.notify());
    }

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
        self.net.send(&host, ClientMessage::Answer { id, thread_id, ask_id: ask_id.to_owned(), answers, silent: false });
        self.focus_composer(cx);
        cx.notify();
    }

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

    pub fn titling(&self, host: &str, id: &str) -> bool {
        self.titling.contains_key(&(host.to_owned(), id.to_owned()))
    }

    pub(crate) fn sync_hosts(&mut self, cx: &mut Context<Self>) {
        let wanted = match hosts::load() {
            Ok(h) => h,
            Err(e) => return self.toast(format!("{e:#}"), cx),
        };
        dirigent_service::tunnel::retain(wanted.values().map(|h| h.url.as_str()));
        let stale: Vec<String> = self
            .hosts
            .iter()
            .filter(|h| h.name != hosts::LOCAL && wanted.get(&h.name) != Some(&h.config))
            .map(|h| h.name.clone())
            .collect();
        for name in stale {
            self.net.remove(&name);
            self.hosts.retain(|h| h.name != name);
            self.blob_requests.remove(&name);
            if self.open().is_some_and(|t| t.host == name) {
                self.set_pane(Pane::Inbox, cx);
            }
            if !wanted.contains_key(&name) {
                self.links.retain(|(h, _), _| *h != name);
                let _ = self.cache.lock().unwrap().remove_host(&name);
            }
        }
        for (name, host) in wanted {
            if self.host(&name).is_none() {
                self.hosts.push(host_state(&self.cache.lock().unwrap(), &name, &host));
                self.net.add(name, host);
            }
        }
        cx.notify();
    }

    pub(crate) fn rename_host(&mut self, old: &str, new: &str, cx: &mut Context<Self>) {
        let open = self.open().filter(|t| t.host == old).map(|t| t.id.clone());
        if open.is_some() {
            self.set_pane(Pane::Inbox, cx);
        }
        let moved = self.cache.lock().unwrap().rename_host(old, new);
        if let Err(e) = moved {
            self.toast(format!("{e:#}"), cx);
        }
        self.links = std::mem::take(&mut self.links)
            .into_iter()
            .map(|((host, project), group)| ((if host == old { new.to_owned() } else { host }, project), group))
            .collect();
        if let Some(t) = self.last_target().filter(|t| t.host == old) {
            self.remember_target(&Target { host: new.to_owned(), ..t });
        }
        if let Pane::Draft { target: Some(t) } = &mut self.pane
            && t.host == old
        {
            t.host = new.to_owned();
        }
        self.sync_hosts(cx);
        if let Some(id) = open {
            self.select_thread(new, &id, cx);
        }
    }

    pub fn remove_project(&mut self, host: &str, project_id: &str, cx: &mut Context<Self>) {
        self.change(host, Change::RemoveProject { project_id: project_id.to_owned() });
        let _ = self.cache.lock().unwrap().set_link(host, project_id, None);
        self.links.remove(&(host.to_owned(), project_id.to_owned()));
        self.menu = None;
        cx.notify();
    }

    pub fn link_group(&mut self, group_key: &str, target_key: Option<&str>, cx: &mut Context<Self>) {
        self.relink(group_key, None, target_key);
        if let Some(t) = target_key
            && let Some(name) = self.names.get(group_key).cloned()
        {
            self.set_name(group_key, None);
            if !self.names.contains_key(t) {
                self.set_name(t, Some(name));
            }
        }
        self.menu = None;
        cx.notify();
    }

    pub fn link_into(&mut self, group_key: &str, other_key: &str, cx: &mut Context<Self>) {
        self.relink(other_key, None, Some(group_key));
        self.expanded_projects.remove(other_key);
        self.expanded_projects.insert(group_key.to_owned());
        self.menu = None;
        self.focus_root(cx);
        cx.notify();
    }

    pub fn unlink_member(&mut self, host: &str, project_id: &str, cx: &mut Context<Self>) {
        let Some(key) = self.groups().0.into_iter().find(|g| g.has(host, project_id)).map(|g| g.key) else { return };
        self.relink(&key, Some((host, project_id)), None);
        cx.notify();
    }

    fn relink(&mut self, group_key: &str, only: Option<(&str, &str)>, target_key: Option<&str>) {
        let (groups, _) = self.groups();
        let Some(group) = groups.iter().find(|g| g.key == group_key) else {
            return;
        };
        let cache = self.cache.lock().unwrap();
        for m in group.members.iter().filter(|m| only.is_none_or(|(h, id)| m.host == h && m.project.id == id)) {
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
        // MUST KILL: rearchitect: let save_pins take the held cache lock instead of an early drop
        drop(cache);
        let mut new: Vec<String> = group.members.iter().map(|m| projects::group_key(&m.host, &m.project, &self.links)).collect();
        new.sort_by_key(|k| k != group_key);
        self.pinned_projects = projects::rekey(&self.pinned_projects, group_key, &new);
        self.group_order = projects::rekey(&self.group_order, group_key, &new);
        self.save_pins();
        self.save_order();
    }

    pub fn toggle_project_expanded(&mut self, group_key: &str, cx: &mut Context<Self>) {
        if !self.expanded_projects.remove(group_key) {
            self.expanded_projects.insert(group_key.to_owned());
        }
        cx.notify();
    }

    pub fn open_link_picker(&mut self, group_key: &str, cx: &mut Context<Self>) {
        self.menu = Some(Menu::Link { group_key: group_key.to_owned() });
        let (window, input) = (self.window, self.link_search.clone());
        cx.defer(move |cx| {
            window.update(cx, |_, window, cx| input.update(cx, |i, cx| i.set_text("", window, cx))).ok();
        });
        self.focus(self.link_search.focus_handle(cx), cx);
        cx.notify();
    }

    pub fn link_candidates(&self, group_key: &str, cx: &App) -> Vec<projects::Group> {
        let others = self.groups().0.into_iter().filter(|g| g.key != group_key).collect();
        projects::search(others, self.link_search.read(cx).text())
    }

    pub fn groups(&self) -> (Vec<projects::Group>, Vec<(String, ThreadSummary)>) {
        projects::groups(&self.hosts, &self.links, &self.names, &self.group_order)
    }

    pub fn start_rename(&mut self, group_key: &str, cx: &mut Context<Self>) {
        let name = self.groups().0.into_iter().find(|g| g.key == group_key).map(|g| g.name).unwrap_or_default();
        self.begin_rename(Renaming::Project(group_key.to_owned()), name, cx);
    }

    pub fn start_thread_rename(&mut self, host: &str, id: &str, cx: &mut Context<Self>) {
        let name = self.summary(host, id).and_then(|t| t.name.clone()).unwrap_or_default();
        self.begin_rename(Renaming::Thread { host: host.to_owned(), id: id.to_owned() }, name, cx);
    }

    fn begin_rename(&mut self, target: Renaming, name: String, cx: &mut Context<Self>) {
        self.renaming = Some(target);
        self.menu = None;
        let (window, input) = (self.window, self.rename_input.clone());
        cx.defer(move |cx| {
            window.update(cx, |_, window, cx| input.update(cx, |i, cx| i.set_text(&name, window, cx))).ok();
        });
        self.focus(self.rename_input.focus_handle(cx), cx);
        cx.notify();
    }

    fn finish_rename(&mut self, refocus: bool, cx: &mut Context<Self>) {
        let Some(target) = self.renaming.take() else { return };
        let name = self.rename_input.read(cx).text().trim().to_owned();
        match target {
            Renaming::Project(key) => {
                let dir_name = self.groups().0.into_iter().find(|g| g.key == key).map(|g| g.dir_name);
                let name = Some(name).filter(|n| !n.is_empty() && Some(n) != dir_name.as_ref());
                self.set_name(&key, name);
            }
            Renaming::Thread { host, id } => self.rename_thread(&host, &id, name),
        }
        if refocus {
            self.focus(self.focus_handle.clone(), cx);
        }
        cx.notify();
    }

    fn rename_thread(&mut self, host: &str, thread_id: &str, name: String) {
        if name.is_empty() || self.summary(host, thread_id).is_none_or(|t| t.name.as_ref() == Some(&name)) {
            return;
        }
        self.titling.remove(&(host.to_owned(), thread_id.to_owned()));
        self.change(host, Change::Rename { thread_id: thread_id.to_owned(), name });
    }

    pub fn show_thread_menu(&mut self, host: &str, id: &str, at: Point<Pixels>, cx: &mut Context<Self>) {
        self.finish_rename(false, cx);
        self.menu = Some(Menu::ThreadActions { host: host.to_owned(), id: id.to_owned(), at });
        cx.notify();
    }

    pub fn retitling(&self, host: &str, thread_id: &str) -> bool {
        self.retitles.values().any(|(h, t)| h == host && t == thread_id)
    }

    pub fn retitle(&mut self, host: &str, thread_id: &str, cx: &mut Context<Self>) {
        self.menu = None;
        if self.retitling(host, thread_id) {
            return cx.notify();
        }
        let id = self.next_id();
        let TitleGen { model, reasoning, .. } = self.config.title.clone();
        self.net.send(host, ClientMessage::RetitleThread { id, thread_id: thread_id.to_owned(), model, reasoning });
        let thread = (host.to_owned(), thread_id.to_owned());
        self.titling.insert(thread.clone(), Instant::now());
        self.retitles.insert(id, thread);
        cx.notify();
    }

    fn set_name(&mut self, key: &str, name: Option<String>) {
        let _ = self.cache.lock().unwrap().set_project_name(key, name.as_deref());
        match name {
            Some(n) => self.names.insert(key.to_owned(), n),
            None => self.names.remove(key),
        };
    }

    pub fn pinned_keys(&self) -> Vec<String> {
        let (groups, _) = self.groups();
        projects::pinned(&groups, &self.pinned_projects).into_iter().map(|g| g.key.clone()).collect()
    }

    pub fn is_pinned_project(&self, group_key: &str) -> bool {
        self.pinned_projects.iter().any(|k| k == group_key)
    }

    pub fn set_project_pinned(&mut self, group_key: &str, pinned: bool, cx: &mut Context<Self>) {
        self.pinned_projects.retain(|k| k != group_key);
        if pinned {
            self.pinned_projects.push(group_key.to_owned());
        }
        self.save_pins();
        cx.notify();
    }

    fn save_pins(&self) {
        if let Ok(json) = serde_json::to_string(&self.pinned_projects) {
            let _ = self.cache.lock().unwrap().set(PINNED_PROJECTS_KEY, &json);
        }
    }

    fn save_order(&self) {
        if let Ok(json) = serde_json::to_string(&self.group_order) {
            let _ = self.cache.lock().unwrap().set(GROUP_ORDER_KEY, &json);
        }
    }

    pub fn move_project(&mut self, group_key: &str, onto: &str, cx: &mut Context<Self>) {
        if group_key == onto {
            return;
        }
        let (groups, _) = self.groups();
        self.group_order = projects::moved(&self.group_order, &groups, group_key, onto);
        self.save_order();
        cx.notify();
    }

    fn request_usage(&mut self, cx: &mut Context<Self>) {
        let id = self.next_id();
        self.usage.request = Some(id);
        let (since, until, bucket_ms) = (self.usage.since, self.usage.until, self.usage.bucket());
        for h in &self.hosts {
            self.net.send(&h.name, ClientMessage::ListUsage { id, since, until, bucket_ms });
        }
        if self.usage.tab == usage::Tab::Proxy {
            self.request_proxy(cx);
        }
        cx.notify();
    }

    fn request_proxy(&mut self, cx: &mut Context<Self>) {
        let id = self.next_id();
        self.usage.proxy_request = Some(id);
        let hosts = &self.hosts;
        self.usage.proxies.retain(|name, _| hosts.iter().any(|h| h.name == *name && h.status == HostStatus::Connected));
        let (since, until, bucket_ms) = (self.usage.since, self.usage.until, self.usage.bucket());
        for h in &self.hosts {
            self.net.send(&h.name, ClientMessage::ListProxy { id, since, until, bucket_ms });
        }
        cx.notify();
    }

    fn check_proxy_updates(&mut self) {
        let id = self.next_id();
        for h in &self.hosts {
            self.net.send(&h.name, proxy::list_without_samples(id));
        }
    }

    pub fn set_usage_limit(&mut self, limit: String, cx: &mut Context<Self>) {
        self.usage.limit = limit;
        self.usage.hover = None;
        cx.notify();
    }

    pub fn set_usage_range(&mut self, range: usage::Range, cx: &mut Context<Self>) {
        if range == usage::Range::Custom {
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

    pub fn page_usage(&mut self, dir: i64, cx: &mut Context<Self>) {
        self.usage.page(dir);
        self.request_usage(cx);
    }

    pub fn set_usage_group(&mut self, group: usage::Group, cx: &mut Context<Self>) {
        self.usage.group = group;
        self.menu = None;
        cx.notify();
    }

    pub fn set_usage_tab(&mut self, tab: usage::Tab, cx: &mut Context<Self>) {
        self.usage.tab = tab;
        self.usage.hover = None;
        self.menu = None;
        match tab {
            usage::Tab::Board => self.fetch_board(cx),
            usage::Tab::Proxy => self.request_proxy(cx),
            usage::Tab::Mine => {}
        }
        cx.notify();
    }

    pub fn set_usage_metric(&mut self, metric: usage::Metric, cx: &mut Context<Self>) {
        self.usage.view_mut().metric = metric;
        self.menu = None;
        cx.notify();
    }

    pub fn set_usage_running(&mut self, running: bool, cx: &mut Context<Self>) {
        self.usage.view_mut().running = running;
        cx.notify();
    }

    pub fn toggle_usage_series(&mut self, key: &str, cx: &mut Context<Self>) {
        let hidden = &mut self.usage.view_mut().hidden;
        if !hidden.remove(key) {
            hidden.insert(key.to_owned());
        }
        cx.notify();
    }

    pub fn set_model(&mut self, key: String, cx: &mut Context<Self>) {
        let levels = self.models().iter().find(|m| m.id == key).map(|m| m.efforts.clone()).unwrap_or_default();
        self.effort = held_effort(self.effort.as_deref().unwrap_or("high"), &levels);
        self.model = Some(key);
        self.remember_draft_model();
        self.menu = None;
        self.focus_composer(cx);
        cx.notify();
    }

    pub fn set_effort(&mut self, level: String, cx: &mut Context<Self>) {
        self.effort = Some(level);
        self.remember_draft_model();
        self.menu = None;
        self.focus_composer(cx);
        cx.notify();
    }

    fn hold_effort(&mut self) {
        if let Some(model) = self.current_model()
            && let Some(effort) = &self.effort
        {
            self.effort = held_effort(effort, &model.efforts);
        }
    }

    fn remember_draft_model(&self) {
        if !matches!(self.pane, Pane::Draft { .. }) {
            return;
        }
        let cache = self.cache.lock().unwrap();
        if let Some(m) = &self.model {
            let _ = cache.set(LAST_MODEL_KEY, m);
        }
        let _ = match &self.effort {
            Some(e) => cache.set(LAST_EFFORT_KEY, e),
            None => cache.remove(LAST_EFFORT_KEY),
        };
    }

    pub fn current_preset(&self) -> Option<Preset> {
        self.model.clone().map(|model| Preset { model, effort: self.effort.clone() })
    }

    pub fn use_preset(&mut self, p: &Preset, cx: &mut Context<Self>) {
        self.model = Some(p.model.clone());
        self.effort = p.effort.clone();
        self.hold_effort();
        self.remember_draft_model();
        self.menu = None;
        self.focus_composer(cx);
        cx.notify();
    }

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

    pub fn open_all_models(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.model_search.update(cx, |input, cx| input.set_text("", window, cx));
        self.menu = Some(Menu::AllModels);
        self.focus(self.model_search.focus_handle(cx), cx);
        cx.notify();
    }

    pub fn filtered_models(&self, cx: &App) -> Vec<&ModelInfo> {
        let query = self.model_search.read(cx).text().to_lowercase();
        let words: Vec<&str> = query.split_whitespace().collect();
        self.models()
            .iter()
            .filter(|m| {
                let hay = format!("{} {}", m.name, m.id).to_lowercase();
                words.iter().all(|w| hay.contains(w))
            })
            .collect()
    }

    fn attach(&mut self, image: gpui::Image, cx: &mut Context<Self>) {
        let key = composer_key(&self.pane).map(|(h, id)| (h.to_owned(), id.to_owned()));
        let fitted = cx.background_spawn(async move {
            let image = images::fit(image);
            let size = upright_size(&image.bytes);
            Blob { image: Arc::new(image), size }
        });
        self.fitting += 1;
        cx.spawn(async move |this, cx| {
            let blob = fitted.await;
            this.update(cx, |this, cx| {
                this.fitting -= 1;
                if composer_key(&this.pane).map(|(h, id)| (h.to_owned(), id.to_owned())) != key {
                    return;
                }
                let n = this.attachments.iter().map(|a| a.n).max().unwrap_or(0) + 1;
                this.attachments.push(Attachment { n, blob });
                let (window, composer) = (this.window, this.composer.clone());
                window
                    .update(cx, |_, window, cx| {
                        composer.update(cx, |input, cx| input.replace_text_in_range(None, &format!("{} ", image_token(n)), window, cx))
                    })
                    .ok();
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    pub fn attachments(&self, cx: &App) -> Vec<Attachment> {
        let text = self.composer.read(cx).text();
        self.attachments.iter().filter(|a| text.contains(&image_token(a.n))).cloned().collect()
    }

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

    fn save_draft(&self, cx: &App) {
        let Some((host, id)) = composer_key(&self.pane) else { return };
        let (content, images) = match self.open().and_then(|t| t.editing.as_ref()) {
            Some(e) => (e.stash.clone(), &e.images),
            None => (self.composer.read(cx).content(), &self.attachments),
        };
        let gone = (host, id) != DRAFT_KEY && self.summary(host, id).is_none();
        let mut cache = self.cache.lock().unwrap();
        if gone || content.text.trim().is_empty() {
            let _ = cache.remove_draft(host, id);
        } else {
            let _ = cache.set_draft(host, id, &content, &image_rows(images));
        }
    }

    fn unsend_draft(&mut self, p: PendingDraft, cx: &mut Context<Self>) {
        let (host, id) = (p.target.host.clone(), p.thread.id);
        if let Some(h) = self.host_mut(&host) {
            h.threads.retain(|t| t.id != id);
        }
        self.titling.remove(&(host.clone(), id.clone()));
        let open = self.open().is_some_and(|t| t.host == host && t.id == id);
        if open {
            let typed = (self.composer.read(cx).text().to_owned(), self.attachments(cx));
            self.give_back(DRAFT_KEY, typed, cx);
        }
        self.give_back(DRAFT_KEY, (p.text, p.images), cx);
        if open {
            self.new_draft(Some(p.target), cx);
            self.base = p.base;
        }
    }

    fn unsend_prompt(&mut self, p: SentPrompt, cx: &mut Context<Self>) {
        if self.summary(&p.host, &p.thread_id).is_none() {
            return;
        }
        if !self.retitling(&p.host, &p.thread_id) {
            self.titling.remove(&(p.host.clone(), p.thread_id.clone()));
        }
        if let Pane::Thread(t) = &mut self.pane
            && t.host == p.host
            && t.id == p.thread_id
        {
            if let Some(i) = t.streaming.echo.iter().position(|(text, mode)| *text == p.text && *mode == p.mode) {
                t.streaming.echo.remove(i);
            }
            let tail = t.timeline.items.len();
            t.list.remeasure_items(tail..tail + 1);
        }
        self.give_back((&p.host, &p.thread_id), (p.text, p.images), cx);
    }

    fn give_back(&mut self, key: (&str, &str), message: Draft, cx: &mut Context<Self>) {
        let (host, id) = key;
        if message.0.trim().is_empty() {
            return;
        }
        let typed = self.cache.lock().unwrap().draft(host, id).map(|(c, images)| (c.text, attachments_from(images))).unwrap_or_default();
        let (text, images) = rejoin(message, typed);
        let content = input::Content { selection: text.len()..text.len(), text, reversed: false };
        let _ = self.cache.lock().unwrap().set_draft(host, id, &content, &image_rows(&images));
        if composer_key(&self.pane) != Some(key) {
            return;
        }
        match &mut self.pane {
            Pane::Thread(t) if t.editing.is_some() => {
                let e = t.editing.as_mut().unwrap();
                (e.stash, e.images) = (content, images);
            }
            _ => self.load_composer(cx),
        }
    }

    fn load_composer(&mut self, cx: &mut Context<Self>) {
        let Some((host, id)) = composer_key(&self.pane) else { return };
        let (content, images) = self.cache.lock().unwrap().draft(host, id).unwrap_or_default();
        self.attachments = attachments_from(images);
        self.set_composer(content, cx);
    }

    fn set_composer(&self, content: input::Content, cx: &mut Context<Self>) {
        let (window, composer) = (self.window, self.composer.clone());
        cx.defer(move |cx| {
            window.update(cx, |_, window, cx| composer.update(cx, |input, cx| input.set_content(content, window, cx))).ok();
        });
    }

    fn fetch_blobs(&mut self) {
        let Pane::Thread(t) = &self.pane else { return };
        let host = t.host.clone();
        let asked = self.blob_requests.get(&host);
        let ids: Vec<String> = view::blobs_shown(&t.timeline.items, &t.expanded)
            .into_iter()
            .filter(|id| !self.blobs.contains_key(id) && !asked.is_some_and(|a| a.values().any(|b| b == id)))
            .collect();
        for id in ids {
            let cached = self.cache.lock().unwrap().blob(&id);
            match cached.and_then(|(mime, bytes)| blob_from(&mime, bytes)) {
                Some(blob) => {
                    self.blobs.insert(id, blob);
                }
                None => {
                    let rid = self.next_id();
                    self.blob_requests.entry(host.clone()).or_default().insert(rid, id.clone());
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

    fn send(&mut self, queued: bool, window: &mut Window, cx: &mut Context<Self>) {
        let text = self.composer.read(cx).text().trim().to_owned();
        if self.fitting > 0 {
            self.toast("Still shrinking the pasted image", cx);
            return;
        }
        if text.is_empty() {
            return;
        }
        let images = self.attachments(cx);
        if self.open().is_some_and(|t| t.editing.is_some()) {
            return self.send_edit(text, images, window, cx);
        }
        if let Pane::Draft { target } = &self.pane {
            let Some(target) = target.clone() else {
                self.toast("Pick a project first", cx);
                self.menu = Some(Menu::Target);
                self.clear_target_search(window, cx);
                return;
            };
            if self.model.is_none() {
                self.toast("Pick a model first", cx);
                self.menu = Some(Menu::Model);
                return;
            }
            if !self.connected(&target.host) {
                self.toast(format!("{} is offline", target.host), cx);
                return;
            }
            let workspace = self.isolate && self.draft_vcs().is_some_and(|(_, _, v)| v.is_some_and(|v| v.vcs.is_some()));
            let base = self.base.clone().filter(|_| workspace);
            let cwd = self.draft_vcs().map(|(_, path, _)| path.to_owned()).unwrap_or_default();
            let prompt = FirstPrompt {
                text: text.clone(),
                model: self.model.clone(),
                effort: self.effort.clone(),
                images: wire_images(&images),
                title: Some(self.config.title.clone()),
                tools: Some(self.config.tools.enabled()),
            };
            let id = self.next_id();
            let now = now_ms();
            let thread = ThreadSummary {
                id: uuid::Uuid::now_v7().to_string(),
                project_id: Some(target.project_id.clone()),
                cwd,
                name: prompt_title(&text),
                model: self.model.clone(),
                effort: self.effort.clone(),
                created_at: now,
                updated_at: now,
                running_since: Some(now),
                prompted_at: Some(now),
                setup: workspace.then_some(Setup::Workspace),
                ..Default::default()
            };
            let thread_id = thread.id.clone();
            self.clear_composer(window, cx);
            self.net.send(
                &target.host,
                ClientMessage::CreateThread {
                    id,
                    thread_id: Some(thread_id.clone()),
                    project_id: target.project_id.clone(),
                    workspace,
                    base,
                    parent: None,
                    name: None,
                    report: Default::default(),
                    notify: Default::default(),
                    prompt: Some(prompt),
                },
            );
            if let Some(h) = self.host_mut(&target.host) {
                h.threads.insert(0, thread.clone());
            }
            let host = target.host.clone();
            self.pending.push(PendingDraft { id, target, base: self.base.take(), text: text.clone(), images, thread });
            self.titling.insert((host.clone(), thread_id.clone()), Instant::now());
            self.select_thread(&host, &thread_id, cx);
            self.echo(&host, &thread_id, text, PromptMode::Prompt);
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
            if let Some(why) = self.not_ready(&t.host, &t.id) {
                self.toast(why, cx);
                return;
            }
            if !self.connected(&t.host) {
                self.toast(format!("{} is offline", t.host), cx);
                return;
            }
            let (host, thread_id) = (t.host.clone(), t.id.clone());
            let mode = match (self.is_running(), queued) {
                (false, _) => PromptMode::Prompt,
                (true, false) => PromptMode::Steer,
                (true, true) => PromptMode::FollowUp,
            };
            self.clear_composer(window, cx);
            self.prompt(&host, &thread_id, text, &images, mode, cx);
        }
    }

    fn settle_open(&mut self) {
        let Pane::Thread(t) = &mut self.pane else { return };
        t.streaming.settle();
        let tail = t.timeline.items.len();
        t.list.remeasure_items(tail..tail + 1);
        if t.log.is_some() {
            t.log_list.remeasure();
        }
    }

    fn clear_composer(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.attachments.clear();
        self.composer.update(cx, |input, cx| {
            input.take(window, cx);
        });
        if let Some((host, id)) = composer_key(&self.pane) {
            let _ = self.cache.lock().unwrap().remove_draft(host, id);
        }
    }

    fn prompt(&mut self, host: &str, thread_id: &str, text: String, images: &[Attachment], mode: PromptMode, cx: &mut Context<Self>) {
        let id = self.next_id();
        let first = mode == PromptMode::Prompt && self.summary(host, thread_id).is_some_and(|s| s.entry_count == 0);
        let title: Option<TitleGen> = first.then(|| self.config.title.clone());
        if first {
            self.titling.insert((host.to_owned(), thread_id.to_owned()), Instant::now());
        }
        self.net.send(
            host,
            ClientMessage::Prompt {
                id,
                thread_id: thread_id.to_owned(),
                text: text.clone(),
                mode,
                model: self.model.clone(),
                effort: self.effort.clone(),
                images: wire_images(images),
                title,
                tools: Some(self.config.tools.enabled()),
            },
        );
        let sent = SentPrompt { host: host.to_owned(), thread_id: thread_id.to_owned(), text: text.clone(), images: images.to_vec(), mode };
        self.prompts.insert(id, sent);
        self.echo(host, thread_id, text, mode);
        cx.notify();
    }

    fn echo(&mut self, host: &str, thread_id: &str, text: String, mode: PromptMode) {
        let Pane::Thread(t) = &mut self.pane else { return };
        if t.host != host || t.id != thread_id {
            return;
        }
        t.streaming.echo.push((text, mode));
        let tail = t.timeline.items.len();
        t.list.remeasure_items(tail..tail + 1);
        if t.log.is_some() {
            self.stash_log();
            let Pane::Thread(t) = &mut self.pane else { return };
            t.log = Some(t.timeline.open_work().unwrap_or(thread::TAIL));
            t.log_list.set_follow_mode(FollowMode::Tail);
        }
    }

    pub fn dismiss(&mut self, _: &Dismiss, _: &mut Window, cx: &mut Context<Self>) {
        if self.preview.is_some() {
            self.set_preview(None, cx);
            return;
        }
        if self.screen_preview {
            self.set_screen_preview(false, cx);
            return;
        }
        if self.menu.is_some() {
            self.close_menu(cx);
            return;
        }
        if self.add_project.is_some() {
            return self.close_add_project(cx);
        }
        if self.onboarding_open() {
            return;
        }
        if self.host_form_escape(cx) || self.mcp_form_escape(cx) {
            return;
        }
        if self.modal.is_some() {
            return self.close_modal(cx);
        }
        if self.renaming.take().is_some() {
            self.focus(self.focus_handle.clone(), cx);
            return cx.notify();
        }
        if self.end_edit(cx) {
            return cx.notify();
        }
        if self.selection.clear() {
            cx.notify();
            return;
        }
        if matches!(self.pane, Pane::Projects) {
            self.go_back(cx);
        } else {
            self.focus_root(cx);
        }
    }

    fn overlay_open(&self) -> bool {
        self.preview.is_some()
            || self.screen_preview
            || self.menu.is_some()
            || self.add_project.is_some()
            || self.onboarding_open()
            || self.modal.is_some()
            || self.renaming.is_some()
    }

    pub fn focus_composer_key(&mut self, _: &FocusComposer, _: &mut Window, cx: &mut Context<Self>) {
        if composer_key(&self.pane).is_some() && !self.overlay_open() {
            self.focus_composer(cx);
        }
    }

    pub fn toggle_sidebar(&mut self, _: &ToggleSidebar, window: &mut Window, cx: &mut Context<Self>) {
        if self.typed_bracket("[", window, cx) || self.onboarding_open() {
            return;
        }
        self.set_sidebar_hidden(!self.sidebar_hidden, cx);
    }

    pub fn set_sidebar_hidden(&mut self, hidden: bool, cx: &mut Context<Self>) {
        self.sidebar_hidden = hidden;
        self.sidebar_peek = false;
        cx.notify();
    }

    pub fn toggle_log_key(&mut self, _: &ToggleLog, window: &mut Window, cx: &mut Context<Self>) {
        if self.typed_bracket("]", window, cx) || self.onboarding_open() {
            return;
        }
        self.toggle_panel(cx);
    }

    fn typed_bracket(&mut self, bracket: &str, window: &mut Window, cx: &mut Context<Self>) -> bool {
        let mut inputs = vec![
            self.composer.clone(),
            self.path_input.clone(),
            self.rename_input.clone(),
            self.model_search.clone(),
            self.base_search.clone(),
            self.target_search.clone(),
            self.link_search.clone(),
            self.usage_from.clone(),
            self.usage_to.clone(),
            self.settings.font_ui.clone(),
            self.settings.font_prose.clone(),
            self.settings.font_code.clone(),
            self.settings.prompt.clone(),
            self.settings.sample.clone(),
        ];
        inputs.extend(self.hosts_tab.inputs());
        inputs.extend(self.mcp.inputs());
        if let Pane::Thread(t) = &self.pane {
            inputs.extend(t.asks.iter().flat_map(|f| f.custom.iter().flatten().cloned()));
        }
        for input in inputs {
            let typing = input != self.composer || !input.read(cx).text().is_empty();
            if input.focus_handle(cx).is_focused(window) && typing {
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
        let saved: Vec<_> = Section::ALL.into_iter().filter(|s| self.collapsed.contains(s)).map(Section::key).collect();
        let _ = self.cache.lock().unwrap().set(COLLAPSED_KEY, &saved.join(" "));
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
            Some(Resize::Sidebar) => self.sidebar_width = (f32::from(ev.position.x) / theme::zoom()).max(SIDEBAR_MIN),
            Some(Resize::Log) => {
                let width = window.viewport_size().width - ev.position.x;
                self.log_width = (f32::from(width) / theme::zoom()).max(LOG_MIN);
            }
            None => return,
        }
        cx.notify();
    }

    fn on_mouse_down(&mut self, _: &gpui::MouseDownEvent, _: &mut Window, cx: &mut Context<Self>) {
        if std::mem::take(&mut self.caption_press) {
            return;
        }
        if self.renaming.is_some() {
            self.finish_rename(false, cx);
        }
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
                input::write_to_primary(text, cx);
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

    fn drag_selection(&mut self, p: Point<Pixels>, cx: &mut Context<Self>) {
        let list = match (self.selection.doc(), &self.pane) {
            (Some(Doc::Thread), Pane::Thread(t)) => Some(&t.list),
            (Some(Doc::Log), Pane::Thread(t)) => Some(&t.log_list),
            _ => None,
        };
        if let Some(list) = list {
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

    // MUST KILL: rearchitect: read the selected text from the timeline, not by re-rendering items to harvest their blocks
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
            Doc::Changelog => {
                if self.modal == Some(Modal::Changelog) {
                    changelog::render(self, cx);
                }
            }
        }
        Some(self.selection.text()).filter(|t| !t.is_empty())
    }

    fn copy_selection(&mut self, _: &input::Copy, _: &mut Window, cx: &mut Context<Self>) {
        if let Some(text) = self.selected_text(cx) {
            cx.write_to_clipboard(ClipboardItem::new_string(text));
        }
    }

    pub fn copy_item(&mut self, id: ElementId, text: String, cx: &mut Context<Self>) {
        cx.write_to_clipboard(ClipboardItem::new_string(text));
        *self.copied_mut() = Some(id.clone());
        cx.spawn(async move |this, cx| {
            cx.background_executor().timer(COPIED_TTL).await;
            this.update(cx, |this, cx| {
                let open = match &mut this.pane {
                    Pane::Thread(t) => Some(&mut t.copied),
                    _ => None,
                };
                for copied in std::iter::once(&mut this.copied).chain(open) {
                    if copied.as_ref() == Some(&id) {
                        *copied = None;
                        cx.notify();
                    }
                }
            })
            .ok();
        })
        .detach();
        cx.notify();
    }

    pub fn copied(&self) -> Option<&ElementId> {
        match &self.pane {
            Pane::Thread(t) => t.copied.as_ref(),
            _ => self.copied.as_ref(),
        }
    }

    fn copied_mut(&mut self) -> &mut Option<ElementId> {
        match &mut self.pane {
            Pane::Thread(t) => &mut t.copied,
            _ => &mut self.copied,
        }
    }

    pub fn jump_to(&mut self, ix: usize, cx: &mut Context<Self>) {
        let Pane::Thread(t) = &mut self.pane else { return };
        t.list.scroll_to(ListOffset { item_ix: ix, offset_in_item: px(0.) });
        t.center_on = Some((ix, 10));
        cx.notify();
    }

    pub fn jump_to_end(&mut self, cx: &mut Context<Self>) {
        let Pane::Thread(t) = &mut self.pane else { return };
        t.center_on = None;
        t.list.set_follow_mode(FollowMode::Tail);
        cx.notify();
    }

    /// Items above it may be unmeasured (0px to `scroll_by`) until they've been laid out, so
    /// this nudges and re-checks until it lands, a few frames at most.
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

    pub fn toggle_pin(&mut self, ix: usize, cx: &mut Context<Self>) {
        let Pane::Thread(t) = &mut self.pane else { return };
        let Some(entry_id) = t.timeline.entry_id(ix).filter(|e| !e.is_empty()).map(str::to_owned) else { return };
        let pinned = !t.pins.remove(&entry_id);
        if pinned {
            t.pins.insert(entry_id.clone());
        }
        let (host, thread_id) = (t.host.clone(), t.id.clone());
        self.change(&host, Change::PinMessage { thread_id, entry_id, pinned });
        cx.notify();
    }

    fn bubble_draft(&self, text: &str, images: &[String]) -> Draft {
        let (body, refs) = view::split_image_refs(text);
        let ns = if refs.is_empty() { input::image_numbers(body) } else { refs };
        let images = ns.into_iter().zip(images).filter_map(|(n, id)| Some(Attachment { n, blob: self.blobs.get(id)?.clone() })).collect();
        (body.to_owned(), images)
    }

    pub fn start_edit(&mut self, ix: usize, cx: &mut Context<Self>) {
        if self.is_running() {
            return self.toast("The thread is running; stop it first", cx);
        }
        let Some(t) = self.open().filter(|t| t.rewound.is_none()) else { return };
        let Some(entry_id) = t.typed_message(ix).map(|e| e.id.clone()) else { return };
        let Some(Item::User { text, images, .. }) = t.timeline.items.get(ix) else { return };
        let (text, images) = self.bubble_draft(text, images);
        let (stash, stash_images) = match t.editing.as_ref() {
            Some(e) => (e.stash.clone(), e.images.clone()),
            None => (self.composer.read(cx).content(), self.attachments.clone()),
        };
        self.attachments = images;
        let Pane::Thread(t) = &mut self.pane else { return };
        t.editing = Some(Editing { entry_id, stash, images: stash_images });
        self.menu = None;
        self.set_composer(input::Content { selection: text.len()..text.len(), text, reversed: false }, cx);
        self.focus_composer(cx);
        cx.notify();
    }

    pub fn cancel_edit(&mut self, cx: &mut Context<Self>) {
        if self.end_edit(cx) {
            self.focus_composer(cx);
            cx.notify();
        }
    }

    fn end_edit(&mut self, cx: &mut Context<Self>) -> bool {
        let Pane::Thread(t) = &mut self.pane else { return false };
        let Some(e) = t.editing.take() else { return false };
        self.attachments = e.images;
        self.set_composer(e.stash, cx);
        true
    }

    fn send_edit(&mut self, text: String, images: Vec<Attachment>, window: &mut Window, cx: &mut Context<Self>) {
        if self.model.is_none() {
            self.toast("Pick a model first", cx);
            self.menu = Some(Menu::Model);
            return;
        }
        if self.is_running() {
            return self.toast("The thread is running; stop it first", cx);
        }
        let Some(t) = self.open() else { return };
        if self.summary(&t.host, &t.id).is_some_and(|s| s.workspace_missing) {
            return self.toast("The thread's workspace is gone; recreate it or move the thread back first", cx);
        }
        if let Some(why) = self.not_ready(&t.host, &t.id) {
            return self.toast(why, cx);
        }
        if !self.connected(&t.host) {
            return self.toast(format!("{} is offline", t.host), cx);
        }
        let id = self.next_id();
        let Pane::Thread(t) = &mut self.pane else { return };
        let Some(e) = t.editing.take() else { return };
        let timeline = {
            let branch = t.entries.last().map(|last| dirigent_protocol::transcript::branch(&t.entries, &last.id)).unwrap_or_default();
            Timeline::from_entries(branch.into_iter().take_while(|x| x.id != e.entry_id))
        };
        t.timeline = timeline;
        t.relist();
        t.list.set_follow_mode(FollowMode::Tail);
        t.rewound = Some(Rewound { request: id, entry_id: e.entry_id.clone(), text: text.clone(), images: images.clone() });
        let (host, thread_id) = (t.host.clone(), t.id.clone());
        self.net.send(
            &host,
            ClientMessage::EditMessage {
                id,
                thread_id: thread_id.clone(),
                entry_id: e.entry_id,
                text: text.clone(),
                model: self.model.clone(),
                effort: self.effort.clone(),
                images: wire_images(&images),
                tools: Some(self.config.tools.enabled()),
            },
        );
        self.attachments = e.images;
        self.composer.update(cx, |input, cx| input.set_content(e.stash, window, cx));
        self.echo(&host, &thread_id, text, PromptMode::Prompt);
        cx.notify();
    }

    fn unrewind(&mut self, cx: &mut Context<Self>) {
        if self.open().is_none_or(|t| t.rewound.is_none()) {
            return;
        }
        let (stash, stash_images) = (self.composer.read(cx).content(), self.attachments.clone());
        let Pane::Thread(t) = &mut self.pane else { return };
        let Some(r) = t.rewound.take() else { return };
        t.timeline = timeline_of(&t.entries);
        t.streaming.echo.retain(|(text, mode)| !(*mode == PromptMode::Prompt && *text == r.text));
        t.relist();
        t.editing = Some(Editing { entry_id: r.entry_id, stash, images: stash_images });
        self.attachments = r.images;
        self.set_composer(input::Content { selection: r.text.len()..r.text.len(), text: r.text, reversed: false }, cx);
        cx.notify();
    }

    fn end_stale_edit(&mut self, cx: &mut Context<Self>) {
        let Some(t) = self.open() else { return };
        let Some(e) = &t.editing else { return };
        if !(0..t.timeline.items.len()).any(|ix| t.timeline.entry_id(ix) == Some(e.entry_id.as_str())) {
            self.end_edit(cx);
        }
    }

    pub fn fork_at(&mut self, ix: usize) {
        let Some((host, thread_id, entry_id, draft)) = self.open().and_then(|t| {
            let (entry_id, draft) = match (t.typed_message(ix), t.timeline.items.get(ix)) {
                (Some(e), Some(Item::User { text, images, .. })) => (e.parent_id.clone()?, Some(self.bubble_draft(text, images))),
                _ => (t.timeline.entry_id(ix).filter(|e| !e.is_empty())?.to_owned(), None),
            };
            Some((t.host.clone(), t.id.clone(), entry_id, draft))
        }) else {
            return;
        };
        let id = self.next_id();
        self.forks.insert(id, (host.clone(), draft));
        self.net.send(&host, ClientMessage::ForkThread { id, thread_id, entry_id, fork_id: None });
    }

    pub fn open_fork_source(&mut self, host: &str, from: &ForkedFrom, cx: &mut Context<Self>) {
        self.select_thread(host, &from.thread_id, cx);
        let Pane::Thread(t) = &mut self.pane else { return };
        t.jump_entry = Some(from.entry_id.clone());
        if let Some(ix) = t.item_at(&from.entry_id) {
            self.jump_to(ix, cx);
        }
    }

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
        if self.menu.as_ref() == Some(&menu) {
            self.close_menu(cx);
        } else {
            self.open_menu(menu, cx);
        }
    }

    pub fn open_menu(&mut self, menu: Menu, cx: &mut Context<Self>) -> bool {
        if self.menu_dismissed {
            return false;
        }
        if matches!(menu, Menu::Workspace | Menu::Base)
            && let Some((host, id)) = self.open().map(|t| (t.host.clone(), t.id.clone()))
        {
            self.request_thread_vcs(&host, &id);
        }
        match self.menu {
            Some(Menu::AllModels | Menu::Base | Menu::Target) => self.focus_composer(cx),
            Some(Menu::Link { .. }) => self.focus_root(cx),
            _ => {}
        }
        self.menu = Some(menu);
        cx.notify();
        true
    }

    pub fn close_menu(&mut self, cx: &mut Context<Self>) {
        match self.menu.take() {
            Some(Menu::AllModels | Menu::Base | Menu::Target) => self.focus_composer(cx),
            Some(Menu::Link { .. }) => self.focus_root(cx),
            Some(_) => {}
            None => return,
        }
        cx.notify();
    }

    pub fn dismiss_menu(&mut self, cx: &mut Context<Self>) {
        self.menu_dismissed = self.menu.is_some();
        self.close_menu(cx);
    }

    pub fn toggle_log(&mut self, key: usize, cx: &mut Context<Self>) {
        let Pane::Thread(t) = &mut self.pane else { return };
        if t.log == Some(key) {
            self.close_panel();
        } else {
            t.panel = None;
            self.log_on = true;
            if !self.panel_shows(PanelPart::Log) {
                self.set_panel_hidden(None);
            }
            self.show_log(key);
        }
        self.focus_composer(cx);
        cx.notify();
    }

    fn show_log(&mut self, key: usize) {
        self.stash_log();
        let Pane::Thread(t) = &mut self.pane else { return };
        t.log = Some(key);
        let top = self.logs.get(&t.id).and_then(|m| m.tops.get(&key)).copied();
        let n = thread::shown_work(self).map_or(0, |(w, _)| w.steps.len().max(1));
        let Pane::Thread(t) = &self.pane else { return };
        t.log_list.reset(n);
        t.log_list.set_follow_mode(FollowMode::Tail);
        if let Some(top) = top {
            t.log_list.scroll_to(top);
        }
    }

    fn stash_log(&mut self) {
        let Pane::Thread(t) = &self.pane else { return };
        let m = self.logs.entry(t.id.clone()).or_default();
        m.expanded = t.expanded.iter().filter(|(ix, _)| *ix != thread::TAIL).copied().collect();
        match t.log.filter(|&key| key != thread::TAIL) {
            Some(key) if t.log_list.is_following_tail() => {
                m.tops.remove(&key);
            }
            Some(key) => {
                m.tops.insert(key, t.log_list.logical_scroll_top());
            }
            None => {}
        }
    }

    pub fn toggle_expanded(&mut self, key: (usize, usize), cx: &mut Context<Self>) {
        if let Pane::Thread(t) = &mut self.pane {
            if !t.expanded.remove(&key) {
                t.expanded.insert(key);
            }
            if t.log == Some(key.0) {
                t.log_list.remeasure_items(key.1..key.1 + 1);
            }
            self.fetch_blobs();
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

pub fn now_ms() -> i64 {
    std::time::UNIX_EPOCH.elapsed().map(|d| d.as_millis() as i64).unwrap_or(0)
}

fn this_second_ms() -> i64 {
    now_ms() / 1000 * 1000
}

fn until_next_second() -> Duration {
    Duration::from_millis((1000 - now_ms() % 1000) as u64)
}

const DRAFT_KEY: (&str, &str) = ("", "");

fn composer_key(pane: &Pane) -> Option<(&str, &str)> {
    match pane {
        Pane::Thread(t) => Some((&t.host, &t.id)),
        Pane::Draft { .. } => Some(DRAFT_KEY),
        _ => None,
    }
}

fn merge_snapshot(mut snapshot: Vec<Entry>, have: Vec<Entry>, stale: Option<u64>) -> (Vec<Entry>, Vec<Entry>) {
    let end = have.last().map(|e| e.seq);
    let fresh = snapshot.split_off(snapshot.partition_point(|e| end.is_some_and(|end| e.seq <= end)));
    let snapshot_end = snapshot.last().map(|e| e.seq);
    let past = |e: &Entry| snapshot_end.is_none_or(|s| e.seq > s) && stale.is_none_or(|s| e.seq > s);
    snapshot.extend(have.into_iter().filter(past));
    (snapshot, fresh)
}

fn wire_images(images: &[Attachment]) -> Vec<ImageAttachment> {
    images
        .iter()
        .map(|a| ImageAttachment {
            mime_type: a.blob.image.format.mime_type().to_owned(),
            data: base64::engine::general_purpose::STANDARD.encode(&a.blob.image.bytes),
            n: Some(a.n),
        })
        .collect()
}

fn rejoin(back: Draft, typed: Draft) -> Draft {
    let ((mut text, mut images), (typed, typed_images)) = (back, typed);
    if typed.trim().is_empty() {
        return (text, images);
    }
    let shift = typed_images.iter().map(|a| a.n).max().unwrap_or(0);
    // MUST KILL: rearchitect: renumber tokens in one pass so no descending order is needed to avoid collisions
    images.sort_by_key(|a| std::cmp::Reverse(a.n));
    for a in images.iter_mut().filter(|_| shift > 0) {
        text = text.replace(&image_token(a.n), &image_token(a.n + shift));
        a.n += shift;
    }
    images.extend(typed_images);
    images.sort_by_key(|a| a.n);
    (format!("{text}\n\n{typed}"), images)
}

fn attachments_from(rows: Vec<crate::cache::DraftImage>) -> Vec<Attachment> {
    rows.into_iter().filter_map(|(n, mime, bytes)| Some(Attachment { n, blob: blob_from(&mime, bytes)? })).collect()
}

fn image_rows(images: &[Attachment]) -> Vec<(u32, &str, &[u8])> {
    images.iter().map(|a| (a.n, a.blob.image.format.mime_type(), a.blob.image.bytes.as_slice())).collect()
}

pub fn image_from(mime: &str, bytes: Vec<u8>) -> Option<Arc<gpui::Image>> {
    let format = gpui::ImageFormat::from_mime_type(mime)?;
    let id = gpui::hash(&bytes);
    Some(Arc::new(gpui::Image { format, bytes, id }))
}

// MUST KILL: rearchitect: migrate stored pi efforts (off, minimal) once instead of mapping them on every hold
fn held_effort(effort: &str, efforts: &[String]) -> Option<String> {
    let named = |name: &str| efforts.iter().find(|e| *e == name);
    let nearest = if matches!(effort, "off" | "minimal") { efforts.first() } else { named("high").or(efforts.last()) };
    named(effort).or(nearest).cloned()
}

fn host_state(cache: &Cache, name: &str, host: &hosts::Host) -> HostState {
    let mut state = HostState {
        name: name.to_owned(),
        config: host.clone(),
        status: HostStatus::Disconnected,
        error: None,
        projects: cache.projects(name).unwrap_or_default(),
        threads: cache.threads(name).unwrap_or_default(),
        models: cache.models(name),
        service_id: None,
        outbox: Outbox::new(cache.outbox(name)),
        proxy_update: None,
    };
    state.outbox.apply(&mut state.projects, &mut state.threads);
    state
}

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

/// The root's left presses, mouse moves and left releases (dropping the selection, saving a
/// rename; resizing, dragging a selection or a project), at window level for the reason
/// `reset_menu_dismissed` is: a root listener only fires while the root is hovered, and an
/// occluder under the mouse (a header's actions, the open sidebar peek, the image preview,
/// a modal's dim) would keep a press from saving a rename, or leave a drag running after
/// the release. Bubble phase, painted first, so they run last, as the root's did: after a
/// text block marks its press as a selection drag, and only if the rename field didn't stop it.
fn track_mouse(cx: &mut Context<Dirigent>) -> impl IntoElement {
    let this = cx.entity().downgrade();
    gpui::canvas(
        |_, _, _| {},
        move |_, _, window, _| {
            let pressed = this.clone();
            window.on_mouse_event(move |ev: &gpui::MouseDownEvent, phase, window, cx| {
                if phase == gpui::DispatchPhase::Bubble && ev.button == MouseButton::Left {
                    pressed.update(cx, |this, cx| this.on_mouse_down(ev, window, cx)).ok();
                }
            });
            let moved = this.clone();
            window.on_mouse_event(move |ev: &MouseMoveEvent, phase, window, cx| {
                if phase == gpui::DispatchPhase::Bubble {
                    moved.update(cx, |this, cx| this.on_mouse_move(ev, window, cx)).ok();
                }
            });
            let released = this.clone();
            window.on_mouse_event(move |ev: &MouseUpEvent, phase, window, cx| {
                if phase == gpui::DispatchPhase::Bubble && ev.button == MouseButton::Left {
                    released.update(cx, |this, cx| this.on_mouse_up(ev, window, cx)).ok();
                }
            });
        },
    )
    .absolute()
    .size_0()
}

/// Clears `menu_dismissed` at the start of every press, before any menu's
/// `on_mouse_down_out` (capture runs in paint order and this paints first). A window-level
/// listener rather than the root's `capture_any_mouse_down`: that one only fires while the
/// root is hovered, and an occluding overlay (the Add project modal) unhovers it, which
/// left the flag stuck and swallowed every later menu-trigger click.
// MUST KILL: rename: reset_press_flags, it clears caption_press too
fn reset_menu_dismissed(cx: &mut Context<Dirigent>) -> impl IntoElement {
    let this = cx.entity().downgrade();
    gpui::canvas(
        |_, _, _| {},
        move |_, _, window, _| {
            let this = this.clone();
            window.on_mouse_event(move |_: &gpui::MouseDownEvent, phase, _, cx| {
                if phase == gpui::DispatchPhase::Capture {
                    this.update(cx, |this, _| {
                        this.menu_dismissed = false;
                        this.caption_press = false;
                    })
                    .ok();
                }
            });
        },
    )
    .absolute()
    .size_0()
}

impl Render for Dirigent {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        // gpui's spacing (`p_2`, `gap_1`, ...) is in rems: 16px, zoomed like every `u` size.
        window.set_rem_size(u(16.));
        self.selection.begin_frame();
        menu::Nav::begin(self.menu.as_ref(), cx);
        self.settle_jump(window);
        self.settle_panel();
        self.sync_screen(cx);
        self.field.want = self.field_spec(window);
        self.redraw_field(false, cx);
        if let Pane::Thread(t) = &mut self.pane {
            t.visuals.spec = Some(visual::Spec::new(window.scale_factor(), &self.fonts));
        }
        let docked = !self.sidebar_hidden;
        let bg = self.config.background;
        let mut fill = theme::t().panel;
        if bg.pattern != Pattern::Off && bg.reach == Reach::Full {
            fill = theme::alpha(fill, ((fill & 0xff) as f32 * ambient::PANE).round() as u32);
        }
        let shown = self.app_shown();
        if self.onboarding.as_ref().is_some_and(|s| s.leaving.is_some()) {
            window.request_animation_frame();
        }
        let app = (!self.onboarding_open()).then(|| {
            let main = match &self.pane {
                Pane::Thread(_) | Pane::Draft { .. } => thread::render(self, window, cx),
                _ => lists::render(self, cx),
            };
            let panel = div()
                .relative()
                .flex_1()
                .min_w(u(PANE_MIN))
                .bg(rgba(fill))
                .overflow_hidden()
                .flex()
                .flex_col()
                .when(titlebar::CUSTOM, |d| d.child(titlebar::strip(cx)))
                .child(main);
            div()
                .relative()
                .size_full()
                .flex()
                .flex_row()
                .when(shown < 1., |d| d.opacity(shown))
                .when(docked, |d| d.child(sidebar::docked(self, cx)))
                .child(panel)
                .when(!docked, |d| d.child(sidebar::peek(self, cx)))
        });
        let field = self.field.layer(px(0.));
        let field = match &self.onboarding {
            Some(_) => field.map(|l| onboarding::curtain(self, l)),
            None => field.filter(|_| bg.reach == Reach::Full).map(IntoElement::into_any_element),
        };
        let key_context = match &self.menu {
            Some(_) => format!("{KEY_CONTEXT} {}", menu::KEY_CONTEXT),
            None if self.onboarding_open() => format!("{KEY_CONTEXT} {}", onboarding::KEY_CONTEXT),
            None => KEY_CONTEXT.to_owned(),
        };
        div()
            .key_context(key_context.as_str())
            .track_focus(&self.focus_handle)
            .on_action(cx.listener(menu::up))
            .on_action(cx.listener(menu::down))
            .on_action(cx.listener(menu::left))
            .on_action(cx.listener(menu::right))
            .on_action(cx.listener(menu::pick))
            .on_action(cx.listener(Self::new_thread))
            .on_action(cx.listener(Self::dismiss))
            .on_action(cx.listener(Self::focus_composer_key))
            .on_action(cx.listener(Self::toggle_sidebar))
            .on_action(cx.listener(Self::toggle_log_key))
            .on_action(|_: &Quit, _, cx| cx.quit())
            .on_action(cx.listener(Self::text_bigger))
            .on_action(cx.listener(Self::text_smaller))
            .on_action(cx.listener(Self::text_reset))
            .on_action(cx.listener(Self::copy_selection))
            .on_action(cx.listener(Self::open_app_link))
            .on_action(cx.listener(Self::onboarding_continue))
            .child(reset_menu_dismissed(cx))
            .child(track_mouse(cx))
            .relative()
            .size_full()
            .flex()
            .flex_row()
            .bg(rgba(theme::t().canvas))
            .text_color(rgba(theme::t().fg))
            .font_family(self.fonts.ui.clone())
            // No ligatures, anywhere: `calt` is what Lilex uses for them, `liga` for good measure.
            .font_features(FontFeatures(Arc::new(vec![("calt".into(), 0), ("liga".into(), 0)])))
            .text_size(text::base())
            .children(field)
            .children(app)
            .children(self.onboarding.is_some().then(|| onboarding::render(self, cx)))
            .children(self.add_project.as_ref().filter(|_| self.onboarding.is_none()).map(|s| add_project::render(self, s, cx)))
            .children(self.modal.map(|m| match m {
                Modal::Settings => settings::render(self, cx),
                Modal::Usage => usage::render(self, cx),
                Modal::Changelog => changelog::render(self, cx),
            }))
            .children(self.preview.clone().map(|image| thread::preview(image, cx)))
            .children(crate::screen::preview(self, window.viewport_size(), cx))
            .children(toasts::corner(self, cx))
            .when(titlebar::CUSTOM, |d| d.child(titlebar::controls(window)))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use dirigent_protocol::transcript::{Api, Assistant, Block, Stop};

    fn entry(seq: u64) -> Entry {
        user(seq, &format!("e{seq}"), None, "")
    }

    fn seqs(entries: &[Entry]) -> Vec<u64> {
        entries.iter().map(|e| e.seq).collect()
    }

    #[test]
    fn a_message_given_back_joins_what_was_typed_since() {
        let image = |n: u32, byte: u8| Attachment { n, blob: Blob { image: image_from("image/png", vec![byte]).unwrap(), size: None } };
        let images = |d: &Draft| d.1.iter().map(|a| (a.n, a.blob.image.bytes[0])).collect::<Vec<_>>();
        let back = || ("a [image-1] b [image-3]".to_owned(), vec![image(1, 11), image(3, 13)]);
        let d = rejoin(back(), (" \n".into(), vec![]));
        assert_eq!((d.0.as_str(), images(&d)), ("a [image-1] b [image-3]", vec![(1, 11), (3, 13)]));
        let d = rejoin(back(), ("no images".into(), vec![]));
        assert_eq!((d.0.as_str(), images(&d)), ("a [image-1] b [image-3]\n\nno images", vec![(1, 11), (3, 13)]));
        let d = rejoin(back(), ("new [image-2] [image-1]".into(), vec![image(1, 1), image(2, 2)]));
        assert_eq!(d.0, "a [image-3] b [image-5]\n\nnew [image-2] [image-1]");
        assert_eq!(images(&d), [(1, 1), (2, 2), (3, 11), (5, 13)]);
    }

    fn at(seq: u64, id: &str, parent: Option<&str>, body: Body) -> Entry {
        Entry { seq, id: id.into(), parent_id: parent.map(str::to_owned), at: 0, body }
    }

    fn user(seq: u64, id: &str, parent: Option<&str>, text: &str) -> Entry {
        at(seq, id, parent, Body::User { content: vec![Block::text(text)] })
    }

    fn reply(seq: u64, id: &str, parent: Option<&str>, text: &str) -> Entry {
        let reply = Assistant {
            content: vec![Block::text(text)],
            model: "m".into(),
            provider: String::new(),
            api: Api::Anthropic,
            effort: None,
            usage: Default::default(),
            stop: Stop::End,
            error: None,
            duration_ms: 0,
        };
        at(seq, id, parent, Body::Assistant(reply))
    }

    #[test]
    fn snapshot_merges_with_pushes_either_way() {
        let range = |a: u64, b: u64| (a..b).map(entry).collect::<Vec<_>>();
        let merge = |snapshot, have, stale| {
            let (kept, fresh) = merge_snapshot(snapshot, have, stale);
            (seqs(&kept), seqs(&fresh))
        };
        assert_eq!(merge(range(0, 4), range(0, 2), None), (vec![0, 1], vec![2, 3]));
        assert_eq!(merge(range(0, 2), vec![], None), (vec![], vec![0, 1]));
        assert_eq!(merge(range(0, 2), range(0, 4), None), (vec![0, 1, 2, 3], vec![]));
        assert_eq!(merge(range(0, 2), range(0, 4), Some(2)), (vec![0, 1, 3], vec![]));
        assert_eq!(merge(range(0, 4), vec![entry(0), entry(3)], None), (vec![0, 1, 2, 3], vec![]));
    }

    #[test]
    fn edits_branch_the_timeline() {
        let mut t = OpenThread::new("h", "t", vec![]);
        let texts = |t: &OpenThread| {
            t.timeline
                .items
                .iter()
                .map(|i| match i {
                    Item::User { text, .. } | Item::Assistant(text) => text.clone(),
                    other => format!("{other:?}"),
                })
                .collect::<Vec<_>>()
        };
        t.fold(user(0, "u1", None, "hi"));
        t.fold(reply(1, "r1", Some("u1"), "hello"));
        assert_eq!(texts(&t), ["hi", "hello"]);
        assert_eq!(t.typed_message(0).map(|e| e.id.as_str()), Some("u1"));
        assert!(t.typed_message(1).is_none());
        assert_eq!(t.fold(user(2, "u2", None, "hey")), (2, Some(0)));
        assert_eq!(texts(&t), ["hey"]);
        t.fold(reply(3, "r2", Some("u2"), "howdy"));
        assert_eq!(texts(&t), ["hey", "howdy"]);
        assert_eq!(t.item_at("r2"), Some(1));
        assert_eq!(t.item_at("r1"), None);
        t.timeline = Timeline::default();
        t.rewound = Some(Rewound { request: 1, entry_id: "u2".into(), text: "yo".into(), images: vec![] });
        assert_eq!(t.fold(reply(4, "r3", Some("r2"), "still here")), (0, None));
        assert!(texts(&t).is_empty());
        t.fold(user(5, "u3", None, "yo"));
        assert!(t.rewound.is_none());
        assert_eq!(texts(&t), ["yo"]);
    }

    #[test]
    fn an_effort_is_held_to_what_the_model_takes() {
        let claude = ["low", "medium", "high", "xhigh", "max"].map(String::from);
        assert_eq!(held_effort("medium", &claude).as_deref(), Some("medium"));
        assert_eq!(held_effort("off", &claude).as_deref(), Some("low"));
        assert_eq!(held_effort("minimal", &claude).as_deref(), Some("low"));
        assert_eq!(held_effort("xhigh", &claude[..3]).as_deref(), Some("high"));
        assert_eq!(held_effort("high", &claude[..2]).as_deref(), Some("medium"));
        assert_eq!(held_effort("high", &[]), None);
    }

    #[test]
    fn relisting_keeps_the_scroll_position() {
        let entries = vec![
            user(0, "u1", None, "hi"),
            reply(1, "r1", Some("u1"), "hello"),
            user(2, "u2", Some("r1"), "and?"),
            reply(3, "r2", Some("u2"), "that's all"),
        ];
        let mut t = OpenThread::new("h", "t", entries[..2].to_vec());
        let top = |t: &OpenThread| {
            let top = t.list.logical_scroll_top();
            (top.item_ix, top.offset_in_item)
        };
        t.relist();
        assert!(t.list.is_following_tail());
        t.list.scroll_to(ListOffset { item_ix: 1, offset_in_item: px(12.) });
        t.timeline = timeline_of(&entries);
        t.relist();
        assert_eq!(t.list.item_count(), 5);
        assert!(!t.list.is_following_tail());
        assert_eq!(top(&t), (1, px(12.)));
        t.list.scroll_to(ListOffset { item_ix: 3, offset_in_item: px(12.) });
        t.timeline = timeline_of(&entries[..1]);
        t.relist();
        assert_eq!(top(&t), (2, px(0.)));
    }
}
