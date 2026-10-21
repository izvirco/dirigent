//! Root view: all UI state, incoming host messages, and user actions.
//! Rendering lives in `sidebar.rs` (navigation), `lists.rs` (inbox / workpool / project
//! pages) and `thread.rs` (one thread + composer).

use crate::cache::Cache;
use crate::hosts;
use crate::input::{self, ImagePasted, Submitted, TextInput, image_token};
use crate::net::{HostStatus, Incoming, Net};
use crate::projects::{self, HostState, Links};
use crate::select::{Doc, Selection};
use crate::theme::{self, Fonts, Status};
use crate::view::{Item, Streaming, Timeline};
use crate::{lists, sidebar, thread};
use base64::Engine;
use dirigent_protocol::{ClientMessage, Entry, ImageAttachment, ModelInfo, PromptMode, ServerMessage, ThreadSummary};
use futures::StreamExt;
use gpui::EntityInputHandler;
use gpui::{
    AnyWindowHandle, App, ClipboardItem, Context, Entity, FocusHandle, Focusable, FollowMode, FontFeatures, ListAlignment, ListState,
    MouseButton, MouseMoveEvent, MouseUpEvent, Pixels, Point, Window, actions, div, prelude::*, px, rgb,
};
use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

actions!(dirigent, [NewThread, Abort, ToggleSidebar, ToggleLog, Quit]);

pub const KEY_CONTEXT: &str = "Dirigent";
const TOAST_TTL: Duration = Duration::from_secs(8);
const LAST_MODEL_KEY: &str = "last_model";
const LAST_EFFORT_KEY: &str = "last_effort";
const LAST_TARGET_KEY: &str = "last_target";
const PRESETS_KEY: &str = "presets";
const SIDEBAR_WIDTH_KEY: &str = "sidebar_width";
/// Where the user was when the app last closed (see `remember_pane`).
const PANE_KEY: &str = "pane";
const SIDEBAR_MIN: f32 = 180.;
const SIDEBAR_MAX: f32 = 420.;
const LOG_WIDTH_KEY: &str = "log_width";
const LOG_MIN: f32 = 280.;
const LOG_MAX: f32 = 800.;
/// How long a message's copy button shows its check.
const COPIED_TTL: Duration = Duration::from_millis(1200);

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
    /// The work whose full log the right panel shows (item index, `thread::TAIL` for a
    /// live work that has no persisted item yet). `None` = panel closed.
    pub log: Option<usize>,
    /// The log panel's steps, one item each; follows the tail while it shows a live work.
    pub log_list: ListState,
    /// The item whose copy button was just clicked (it shows a check for a moment).
    pub copied: Option<usize>,
}

/// Where a new thread will run: a project on a host.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct Target {
    pub host: String,
    pub project_id: String,
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
    /// Settings: remote hosts (`hosts.toml`) with their status, and the form to add one.
    Settings,
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
    /// Draft: which project (and host) the thread will run in.
    Target,
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
}

pub struct Dirigent {
    pub hosts: Vec<HostState>,
    pub links: Links,
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
    /// Per thread, its `updated_at` when the user last looked at it; a finished thread
    /// updated since (or never looked at) is unread.
    seen: HashMap<(String, String), i64>,
    /// Work log panel width (drag its left edge; `]` toggles the panel).
    pub log_width: f32,
    resizing: Option<Resize>,
    /// A 1s re-render loop is alive (see `tick`).
    ticking: bool,
    pub composer: Entity<TextInput>,
    pub path_input: Entity<TextInput>,
    /// The settings page's add-host form.
    pub host_name_input: Entity<TextInput>,
    pub host_url_input: Entity<TextInput>,
    /// Filter for the model picker's all-models page.
    pub model_search: Entity<TextInput>,
    /// Model (`provider/id`) and thinking level the next prompt is sent with.
    pub model: Option<String>,
    pub effort: Option<String>,
    pub presets: Vec<Preset>,
    pub toasts: Vec<(String, Instant)>,
    pub fonts: Fonts,
    /// Images pasted into the composer, in paste order. Only those whose `[image-N]` token
    /// is still in the text count (`attachments()`); the rest are pruned on send.
    attachments: Vec<Attachment>,
    next_attachment: u32,
    /// The image shown full-size over everything; click or Esc closes it.
    pub preview: Option<Arc<gpui::Image>>,
    /// Mouse selection in the transcript or the work log.
    pub selection: Selection,
    /// Images referenced from entries, by blob id: filled from the cache db, else fetched
    /// from the host once (`blob_requests`) when the thread shows them.
    pub blobs: HashMap<String, Arc<gpui::Image>>,
    blob_requests: HashSet<String>,
    /// A draft was sent: the `CreateThread` request id and the message to prompt with once
    /// the host confirms the thread.
    pending: Option<(u64, String, Vec<Attachment>)>,
    net: Net,
    cache: Arc<Mutex<Cache>>,
    next_id: u64,
    focus_handle: FocusHandle,
    window: AnyWindowHandle,
}

impl Dirigent {
    pub fn new(cache: Cache, fonts: Fonts, window: &mut Window, cx: &mut Context<Self>) -> Self {
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
        let seen = cache.seen().unwrap_or_default().into_iter().map(|(h, t, u)| ((h, t), u)).collect();
        let model = cache.get(LAST_MODEL_KEY);
        let effort = cache.get(LAST_EFFORT_KEY);
        let presets = cache.get(PRESETS_KEY).and_then(|s| serde_json::from_str(&s).ok()).unwrap_or_default();
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
        cx.subscribe_in(&path_input, window, |this, _, _: &Submitted, window, cx| this.add_project(window, cx)).detach();
        let host_name_input = cx.new(|cx| TextInput::new(cx, "name", true));
        let host_url_input = cx.new(|cx| TextInput::new(cx, "host:7777 or ws://host:7777/ws", true));
        for input in [&host_name_input, &host_url_input] {
            cx.subscribe_in(input, window, |this, _, _: &Submitted, window, cx| this.add_host(window, cx)).detach();
        }
        let model_search = cx.new(|cx| TextInput::new(cx, "Search models", true));
        cx.subscribe(&model_search, |this, _, _: &Submitted, cx| this.pick_first_model(cx)).detach();

        let mut this = Self {
            hosts: host_states,
            links,
            pane: Pane::Inbox,
            menu: None,
            menu_dismissed: false,
            sidebar_width,
            sidebar_hidden: false,
            sidebar_peek: false,
            collapsed: HashSet::new(),
            seen,
            log_width,
            resizing: None,
            ticking: false,
            composer,
            path_input,
            host_name_input,
            host_url_input,
            model_search,
            model,
            effort,
            presets,
            toasts: vec![],
            fonts,
            attachments: vec![],
            next_attachment: 1,
            preview: None,
            selection: Selection::default(),
            blobs: HashMap::new(),
            blob_requests: HashSet::new(),
            pending: None,
            net,
            cache,
            next_id: 1,
            focus_handle: cx.focus_handle(),
            window: window.window_handle(),
        };
        this.restore_pane(cx);
        if let Some(e) = hosts_error {
            this.toast(e, cx);
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

    fn any_running(&self) -> bool {
        self.hosts.iter().any(|h| h.threads.iter().any(|t| t.running()))
    }

    /// Finished since the user last opened it (never opened counts too).
    pub fn unread(&self, host: &str, t: &ThreadSummary) -> bool {
        Status::of(t) == Status::Done && self.seen.get(&(host.to_owned(), t.id.clone())).is_none_or(|&u| t.updated_at > u)
    }

    /// What the sidebar lists under Inbox: unread threads, plus the open one while it's
    /// finished (so the thread being read stays in view) unless archived.
    pub fn inbox_threads(&self) -> Vec<(&str, &ThreadSummary)> {
        let open = self.open().map(|o| (o.host.as_str(), o.id.as_str()));
        self.all_threads()
            .into_iter()
            .filter(|(h, t)| self.unread(h, t) || (open == Some((h, &t.id)) && Status::of(t) == Status::Done))
            .collect()
    }

    /// The user is looking at this thread: whatever it has now is read.
    fn mark_seen(&mut self, host: &str, id: &str) {
        let Some(updated_at) = self.summary(host, id).map(|t| t.updated_at) else { return };
        self.seen.insert((host.to_owned(), id.to_owned()), updated_at);
        let _ = self.cache.lock().unwrap().set_seen(host, id, updated_at);
    }

    /// Every thread on every host, newest first, with its host.
    pub fn all_threads(&self) -> Vec<(&str, &ThreadSummary)> {
        let mut all: Vec<_> = self.hosts.iter().flat_map(|h| h.threads.iter().map(move |t| (h.name.as_str(), t))).collect();
        all.sort_by_key(|(_, t)| std::cmp::Reverse(t.updated_at));
        all
    }

    pub fn threads_with(&self, status: Status) -> Vec<(&str, &ThreadSummary)> {
        self.all_threads().into_iter().filter(|(_, t)| Status::of(t) == status).collect()
    }

    /// Group key of a project, if the host still lists it.
    pub fn group_key_of(&self, host: &str, project_id: &str) -> Option<String> {
        let project = self.host(host)?.projects.iter().find(|p| p.id == project_id)?;
        Some(projects::group_key(host, project, &self.links))
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
                if let Some(h) = self.host_mut(&host) {
                    h.threads = threads;
                }
                self.tick(cx);
                if let Pane::Thread(t) = &self.pane
                    && t.host == host
                {
                    let id = t.id.clone();
                    if self.summary(&host, &id).is_some() {
                        self.mark_seen(&host, &id);
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
                    self.fetch_blobs();
                }
            }
            ServerMessage::Blob { blob_id, mime_type, data, .. } => {
                self.blob_requests.remove(&blob_id);
                if let Some(image) = decode_blob(&mime_type, &data) {
                    self.blobs.insert(blob_id, image);
                }
            }
            ServerMessage::ThreadCreated { reply_to, mut thread } => {
                let pending = self.pending.take_if(|(id, ..)| *id == reply_to);
                if pending.is_some() {
                    // Our draft: it goes straight to work, so list it that way before the
                    // host's push confirms it (no flash through the inbox with no model).
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
                // A failed create leaves the draft intact so nothing typed is lost.
                if let Some((_, text, images)) = self.pending.take_if(|(id, ..)| Some(*id) == reply_to) {
                    self.attachments.extend(images);
                    let (window, composer) = (self.window, self.composer.clone());
                    cx.defer(move |cx| {
                        window.update(cx, |_, window, cx| composer.update(cx, |input, cx| input.set_text(&text, window, cx))).ok();
                    });
                }
                self.toast(format!("{host}: {message}"), cx)
            }
            ServerMessage::Entry { thread_id, entry } => {
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
        self.pane = pane;
        self.menu = None;
        self.selection.clear();
        self.remember_pane();
        self.focus(self.focus_handle.clone(), cx);
        cx.notify();
    }

    /// Persist the pane so the next launch resumes there. Drafts and the new-project form
    /// are transient (their text isn't persisted), so they leave the last stored pane alone.
    fn remember_pane(&self) {
        let value = match &self.pane {
            Pane::Inbox => "inbox".to_owned(),
            Pane::Workpool => "workpool".to_owned(),
            Pane::Project { group_key } => format!("project\n{group_key}"),
            Pane::Thread(t) => format!("thread\n{}\n{}", t.host, t.id),
            Pane::Settings => "settings".to_owned(),
            Pane::Draft { .. } | Pane::NewProject { .. } => return,
        };
        let _ = self.cache.lock().unwrap().set(PANE_KEY, &value);
    }

    /// Reopen the pane stored by `remember_pane`; a thread only if the cache still knows it.
    fn restore_pane(&mut self, cx: &mut Context<Self>) {
        let Some(value) = self.cache.lock().unwrap().get(PANE_KEY) else { return };
        let parts: Vec<&str> = value.split('\n').collect();
        match parts.as_slice() {
            ["workpool"] => self.pane = Pane::Workpool,
            ["settings"] => self.pane = Pane::Settings,
            ["project", key] => self.pane = Pane::Project { group_key: (*key).to_owned() },
            ["thread", host, id] if self.summary(host, id).is_some() => {
                let (host, id) = ((*host).to_owned(), (*id).to_owned());
                self.select_thread(&host, &id, cx);
            }
            _ => {}
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
        self.pane = Pane::Thread(Box::new(OpenThread {
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
        }));
        self.menu = None;
        self.selection.clear();
        self.remember_pane();
        self.mark_seen(host, id);
        self.fetch_blobs();
        let rid = self.next_id();
        self.net.send(host, ClientMessage::GetThread { id: rid, thread_id: id.to_owned() });
        self.focus_composer(cx);
        self.tick(cx);
        cx.notify();
    }

    /// While any thread runs, re-render once a second so durations count up.
    fn tick(&mut self, cx: &mut Context<Self>) {
        if self.ticking || !self.any_running() {
            return;
        }
        self.ticking = true;
        cx.spawn(async move |this, cx| {
            loop {
                cx.background_executor().timer(Duration::from_secs(1)).await;
                let go_on = this.update(cx, |this, cx| {
                    this.ticking = this.any_running();
                    cx.notify();
                    this.ticking
                });
                if !matches!(go_on, Ok(true)) {
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
        self.pane = Pane::Draft { target };
        self.menu = None;
        self.selection.clear();
        self.focus_composer(cx);
        cx.notify();
    }

    pub fn new_thread(&mut self, _: &NewThread, _: &mut Window, cx: &mut Context<Self>) {
        self.new_draft(None, cx);
    }

    /// The project implied by the current pane: the open thread's, or the project page's
    /// (when the group lives on one host).
    fn context_target(&self) -> Option<Target> {
        match &self.pane {
            Pane::Thread(t) => {
                let pid = self.summary(&t.host, &t.id)?.project_id.clone()?;
                Some(Target { host: t.host.clone(), project_id: pid })
            }
            Pane::Draft { target } => target.clone(),
            Pane::Project { group_key } => {
                let (groups, _) = projects::groups(&self.hosts, &self.links);
                let group = groups.into_iter().find(|g| g.key == *group_key)?;
                match group.members.as_slice() {
                    [m] => Some(Target { host: m.host.clone(), project_id: m.project.id.clone() }),
                    _ => None,
                }
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
        self.focus_composer(cx);
        cx.notify();
    }

    pub fn set_archived(&mut self, host: &str, thread_id: &str, archived: bool, cx: &mut Context<Self>) {
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
        self.menu = None;
        cx.notify();
    }

    // ---- projects ----

    /// Open the path form for a new project on `host` (default: first connected host).
    pub fn start_new_project(&mut self, host: Option<String>, cx: &mut Context<Self>) {
        let host =
            host.or_else(|| self.hosts.iter().find(|h| h.status == HostStatus::Connected).or(self.hosts.first()).map(|h| h.name.clone()));
        let Some(host) = host else { return };
        self.pane = Pane::NewProject { host };
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
            self.seen.retain(|(h, _), _| *h != name);
            let _ = self.cache.lock().unwrap().remove_host(&name);
            if self.open().is_some_and(|t| t.host == name) {
                self.pane = Pane::Settings;
            }
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
        let (groups, _) = projects::groups(&self.hosts, &self.links);
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

    /// The pasted image gets the next number and its token goes in at the cursor.
    fn attach(&mut self, image: gpui::Image, window: &mut Window, cx: &mut Context<Self>) {
        let n = self.next_attachment;
        self.next_attachment += 1;
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
        self.focus_composer(cx);
        cx.notify();
    }

    /// Make sure every image the open thread shows is in `blobs`: from the cache db, else
    /// asked of the host (once).
    fn fetch_blobs(&mut self) {
        let Pane::Thread(t) = &self.pane else { return };
        let host = t.host.clone();
        let ids: Vec<String> = t
            .timeline
            .items
            .iter()
            .filter_map(|i| if let Item::User { images, .. } = i { Some(images) } else { None })
            .flatten()
            .filter(|id| !self.blobs.contains_key(*id) && !self.blob_requests.contains(*id))
            .cloned()
            .collect();
        for id in ids {
            let cached = self.cache.lock().unwrap().blob(&id);
            match cached.and_then(|(mime, bytes)| image_from(&mime, bytes)) {
                Some(image) => {
                    self.blobs.insert(id, image);
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
            let id = self.next_id();
            self.pending = Some((id, text, images));
            self.attachments.clear();
            self.composer.update(cx, |input, cx| {
                input.take(window, cx);
            });
            self.net.send(&target.host, ClientMessage::CreateThread { id, project_id: target.project_id });
            return;
        }
        if let Pane::Thread(t) = &self.pane {
            if self.model.is_none() {
                self.toast("Pick a model first", cx);
                self.menu = Some(Menu::Model);
                return;
            }
            let (host, thread_id) = (t.host.clone(), t.id.clone());
            let mode = match (self.is_running(), queued) {
                (false, _) => PromptMode::Prompt,
                (true, false) => PromptMode::Steer,
                (true, true) => PromptMode::FollowUp,
            };
            self.attachments.clear();
            self.composer.update(cx, |input, cx| {
                input.take(window, cx);
            });
            self.prompt(&host, &thread_id, text, images, mode, cx);
        }
    }

    fn prompt(&mut self, host: &str, thread_id: &str, text: String, images: Vec<Attachment>, mode: PromptMode, cx: &mut Context<Self>) {
        let id = self.next_id();
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
            // An open log panel follows the new run.
            if t.log.is_some() {
                t.log = Some(t.timeline.open_work().unwrap_or(thread::TAIL));
                t.log_list.set_follow_mode(FollowMode::Tail);
            }
        }
        self.tick(cx);
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
        } else if let Pane::Thread(t) = &mut self.pane
            && t.log.take().is_some()
        {
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

    /// `]`: close the work log panel, or open it on the run in progress, else the last work.
    pub fn toggle_log_key(&mut self, _: &ToggleLog, window: &mut Window, cx: &mut Context<Self>) {
        if self.typed_bracket("]", window, cx) {
            return;
        }
        let Pane::Thread(t) = &self.pane else { return };
        let key = match t.log {
            Some(key) => key,
            None if self.is_running() => t.timeline.open_work().unwrap_or(thread::TAIL),
            None => match t.timeline.items.iter().rposition(|i| matches!(i, Item::Work(_))) {
                Some(i) => i,
                None => return,
            },
        };
        self.toggle_log(key, cx);
    }

    /// A bracket key with text in the focused input types the bracket instead of acting.
    fn typed_bracket(&mut self, bracket: &str, window: &mut Window, cx: &mut Context<Self>) -> bool {
        for input in [&self.composer, &self.path_input, &self.host_name_input, &self.host_url_input, &self.model_search] {
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
        cx.notify();
    }

    /// Close the open menu. Leaving the model search hands focus back to the composer.
    pub fn close_menu(&mut self, cx: &mut Context<Self>) {
        match self.menu.take() {
            Some(Menu::AllModels) => self.focus_composer(cx),
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

    /// Open the log panel on a work (clicking its header), or close it if it's showing it.
    pub fn toggle_log(&mut self, key: usize, cx: &mut Context<Self>) {
        if let Pane::Thread(t) = &mut self.pane {
            let live = t.timeline.open_work().map_or(key == thread::TAIL, |i| i == key);
            t.log = (t.log != Some(key)).then_some(key);
            // A finished work opens at the top, a live one follows its latest step.
            // `log_panel` fills the list in at the next render.
            t.log_list.reset(0);
            t.log_list.set_follow_mode(if live { FollowMode::Tail } else { FollowMode::Normal });
            self.focus_composer(cx);
            cx.notify();
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

fn decode_blob(mime: &str, data: &str) -> Option<Arc<gpui::Image>> {
    image_from(mime, base64::engine::general_purpose::STANDARD.decode(data).ok()?)
}

impl Render for Dirigent {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        self.selection.begin_frame();
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
            .bg(rgb(theme::PANEL))
            .border_1()
            .border_color(rgb(theme::LINE_SOFT))
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
            // Capture runs root-first, i.e. before the menu's `on_mouse_down_out`.
            .capture_any_mouse_down(cx.listener(|this, _, _, _| this.menu_dismissed = false))
            .on_mouse_down(MouseButton::Left, cx.listener(Self::on_mouse_down))
            .on_mouse_move(cx.listener(Self::on_mouse_move))
            .on_mouse_up(MouseButton::Left, cx.listener(Self::on_mouse_up))
            .relative()
            .size_full()
            .flex()
            .flex_row()
            .bg(rgb(theme::CANVAS))
            .text_color(rgb(theme::FG))
            .font_family(self.fonts.mono.clone())
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
                        .bg(rgb(theme::HOVER))
                        .border_1()
                        .border_color(rgb(theme::LINE))
                        .shadow_md()
                        .child(div().size(px(6.)).rounded_full().bg(rgb(theme::ERROR)).flex_shrink_0())
                        .child(m)
                }))
            }))
    }
}
