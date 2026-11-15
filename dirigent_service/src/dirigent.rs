//! The `dirigent` tool: a thread's agent acting on dirigent itself. It spawns sub-agent
//! threads, starts top-level ones, and finds, reads, messages and manages threads, on this
//! host and on the hosts this one reaches (its `hosts.toml`).
//!
//! Every action is a few protocol requests (`ClientMessage`), the ones a desktop makes: to
//! this service directly (`Service::handle`), to another host over its WebSocket, each as
//! `ListHosts` says it is reached now (an ssh host's tunnel end, a public host's key).

use crate::Service;
use crate::server::Conn;
use anyhow::{Result, anyhow, bail};
use async_trait::async_trait;
use dirigent_agent::tool::{Call, Tool, ToolOutput, ToolSpec};
use dirigent_protocol::transcript::{Body, Stop, branch, text_of};
use dirigent_protocol::{
    ClientMessage, FirstPrompt, HostInfo, Notify, Project, PromptMode, Report, SearchHit, ServerMessage, ThreadSummary,
};
use futures_util::{SinkExt, StreamExt};
use serde::Deserialize;
use serde_json::{Value, json};
use std::collections::HashMap;
use std::sync::{Arc, Mutex, Weak};
use std::time::Duration;
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tokio_tungstenite::tungstenite::http::header::AUTHORIZATION;

/// This host's own service, as its agents (and its desktop) call it.
const LOCAL: &str = "local";
/// A host that doesn't answer a connect this fast is reported unreachable (an offline
/// tailscale peer never refuses).
const CONNECT: Duration = Duration::from_secs(3);
/// Every reply is quick, so past this the host is dead or the reply lost. But for a
/// `Prompt` or `CreateThread`, which wait until the model has the message (a workspace to
/// make, a nix dev shell to capture): one of those taking longer times out a request that
/// still goes through, and its error says so.
const REQUEST: Duration = Duration::from_secs(30);
const LINK: &str = "dirigent://thread/";
/// Largest message read from another host (as the desktop reads: `net.rs`).
const MAX_MESSAGE: usize = 64 << 20;

/// Which host (by name) each thread lives on, as learned; threads never move. By name: how
/// a host is reached (an ssh tunnel's port) can change between calls.
static OWNERS: Mutex<Option<HashMap<String, String>>> = Mutex::new(None);

fn owner(thread_id: &str) -> Option<String> {
    OWNERS.lock().unwrap().as_ref()?.get(thread_id).cloned()
}

fn learn(thread_id: &str, host: Option<&str>) {
    let mut owners = OWNERS.lock().unwrap();
    let owners = owners.get_or_insert_default();
    match host {
        Some(host) => owners.insert(thread_id.to_owned(), host.to_owned()),
        None => owners.remove(thread_id),
    };
}

pub struct Dirigent {
    svc: Weak<Service>,
    thread_id: String,
}

impl Dirigent {
    pub fn new(svc: Weak<Service>, thread_id: &str) -> Self {
        Self { svc, thread_id: thread_id.to_owned() }
    }
}

#[derive(Deserialize)]
struct Input {
    action: String,
    thread_id: Option<String>,
    task: Option<String>,
    host: Option<String>,
    project: Option<String>,
    report: Option<Report>,
    notify: Option<Notify>,
    text: Option<String>,
    mode: Option<PromptMode>,
    model: Option<String>,
    effort: Option<String>,
    workspace: Option<bool>,
    base: Option<String>,
    query: Option<String>,
    limit: Option<u64>,
    full: Option<bool>,
    archived: Option<bool>,
    snoozed: Option<bool>,
    pinned: Option<bool>,
    name: Option<String>,
}

#[async_trait]
impl Tool for Dirigent {
    fn spec(&self) -> ToolSpec {
        let string = |description: &str| json!({ "type": "string", "description": description });
        let boolean = |description: &str| json!({ "type": "boolean", "description": description });
        ToolSpec {
            snippet: Some("Spawn sub-agents, start threads, manage other threads, or add projects, when the user asks".into()),
            ..ToolSpec::new(
                "dirigent",
                "Work with dirigent threads and projects when the user asks you to: spawn sub-agents, start a top-level thread \
                 (`new`), find, read, message and manage other threads, or add a project.",
                json!({
                    "type": "object",
                    "properties": {
                        "action": {
                            "type": "string",
                            "enum": ["spawn", "new", "send", "read", "status", "list", "search", "projects", "add_project", "archive", "snooze", "pin", "abort", "open"],
                            "description": "spawn: a sub-agent called `name` for `task`. \
                                new: a top-level thread for the user (not a sub-agent) called `name` for `task`, in any project on any host. \
                                send: message `thread_id` with `text`. \
                                read: a thread's final reply (`full`: the whole conversation). \
                                status: your sub-agents, or one `thread_id`. \
                                list: the latest top-level threads on every host. \
                                search: threads on every host whose name or messages contain `query`. \
                                projects: every host's projects. \
                                add_project: make the directory a project in dirigent. \
                                archive / snooze / pin / abort / open (in the user's desktop): act on `thread_id`.",
                        },
                        "thread_id": string("A thread id, or its dirigent://thread/<id> link."),
                        "task": string("spawn / new: the thread's full prompt. A `new` thread knows nothing of you or this conversation: make it self-contained."),
                        "host": string("new / add_project: a host name from `projects` (default \"local\", the host you run on)."),
                        "project": string("new: a project id or path on that host (default: your project; on another host, the one with the same git origin). add_project: the directory's path there, absolute or under `~/`."),
                        "report": {
                            "type": "string",
                            "enum": ["reply", "done"],
                            "description": "spawn: what you are told when it finishes: \"reply\" (default) its final reply, \"done\" only that it finished (use `read` for the replies you need; keeps many sub-agents' replies out of your context).",
                        },
                        "notify": {
                            "type": "string",
                            "enum": ["asap", "when_idle"],
                            "description": "When the result reaches you: \"asap\" (default) after your current tool calls, \"when_idle\" once your turn has ended.",
                        },
                        "text": string("send: the message."),
                        "mode": {
                            "type": "string",
                            "enum": ["steer", "follow_up"],
                            "description": "send, when the thread is running: interrupt it (steer, default) or wait for its turn to end.",
                        },
                        "model": string("spawn / new: a model's id; default: your own model."),
                        "effort": string("spawn / new: reasoning effort; default: your own."),
                        "workspace": boolean("spawn / new: give it its own git worktree / jj workspace (from `base`, default HEAD) instead of your directory (spawn) or the project's (new). Use it when it edits files while others work in the same checkout."),
                        "base": { "type": "string" },
                        "query": { "type": "string" },
                        "limit": { "type": "number" },
                        "full": { "type": "boolean" },
                        "archived": boolean("archive: false to unarchive."),
                        "snoozed": boolean("snooze: false to wake."),
                        "pinned": boolean("pin: false to unpin."),
                        "name": string("spawn / new (required): the thread's name, a few words on what it is for."),
                    },
                    "required": ["action"],
                }),
            )
        }
    }

    async fn call(&self, input: Value, _call: &Call) -> Result<ToolOutput> {
        let p: Input = serde_json::from_value(input)?;
        let svc = self.svc.upgrade().ok_or_else(|| anyhow!("the service is shutting down"))?;
        let mut d = Hosts::new(svc, &self.thread_id);
        let need = |value: &Option<String>, what: &str| -> Result<String> {
            value.clone().filter(|v| !v.is_empty()).ok_or_else(|| anyhow!("`{what}` is required for {}", p.action))
        };
        let text = match p.action.as_str() {
            "spawn" => {
                let name = need(&p.name, "name")?;
                let id = d.spawn(&p, &name, &need(&p.task, "task")?).await?;
                let what = if p.report == Some(Report::Done) { "that it finished" } else { "its reply" };
                let when = if p.notify == Some(Notify::WhenIdle) { " (once your turn has ended)" } else { "" };
                // The desktop finds the id by this prefix (`view::spawned_id`): keep it.
                format!(
                    "Spawned sub-agent thread {id}. You will get a message with {what} when it finishes{when}; use status / read to \
                     look in on it, send to steer it. Put {} in your reply so the user can open it.",
                    link(&id, Some(&name))
                )
            }
            "new" => {
                let f = d.create(&p, &need(&p.name, "name")?, &need(&p.task, "task")?).await?;
                let path = f.project.as_ref().map_or("?", |p| p.path.as_str());
                format!(
                    "Started {} on {} in {path}. It is the user's now: you won't hear from it. Put that link in your reply so the \
                     user can open it.",
                    f.link(),
                    f.host.name
                )
            }
            "send" => {
                let f = d.send(&need(&p.thread_id, "thread_id")?, &need(&p.text, "text")?, p.mode.unwrap_or(PromptMode::Steer)).await?;
                format!("Sent to {}.", f.link())
            }
            "read" => {
                let full = p.full.unwrap_or(false);
                let (f, reply, transcript) = d.read(&need(&p.thread_id, "thread_id")?).await?;
                let body = match full {
                    true if transcript.is_empty() => "(empty)".to_owned(),
                    true => transcript,
                    false => reply.unwrap_or_else(|| "(no final reply yet)".into()),
                };
                format!("{}\n\n{body}", f.describe())
            }
            "status" => match &p.thread_id {
                Some(id) => d.locate(id).await?.describe(),
                None => {
                    let local = d.this.clone();
                    // In the desktop panel's order: by when each was last given a task.
                    let mut kids: Vec<ThreadSummary> =
                        d.threads(&local).await?.into_iter().filter(|t| t.parent_id.as_deref() == Some(&self.thread_id)).collect();
                    kids.sort_by(|a, b| {
                        (a.prompted_at.unwrap_or(a.created_at), a.created_at, &a.id).cmp(&(
                            b.prompted_at.unwrap_or(b.created_at),
                            b.created_at,
                            &b.id,
                        ))
                    });
                    let mut lines = vec![];
                    for kid in kids {
                        lines.push(d.found(&local, kid, None).await.describe());
                    }
                    let kids = if lines.is_empty() { "No sub-agents.".to_owned() } else { format!("Sub-agents:\n{}", lines.join("\n")) };
                    format!("You are thread {}.\n{kids}", self.thread_id)
                }
            },
            "list" => listing(d.list(p.limit.unwrap_or(20) as usize).await?, "No threads."),
            "search" => listing(d.search(&need(&p.query, "query")?, p.limit.unwrap_or(10) as usize).await?, "No thread matches."),
            "projects" => {
                let mut lines = vec![];
                for host in d.hosts().await {
                    lines.push(match d.projects(&host).await {
                        Err(e) => format!("{}: not reached ({e:#})", host.name),
                        Ok(projects) if projects.is_empty() => format!("{}:\n  (no projects)", host.name),
                        Ok(projects) => {
                            let origin = |p: &Project| p.origin.as_ref().map(|o| format!(" ({o})")).unwrap_or_default();
                            let lines: Vec<String> = projects.iter().map(|p| format!("  - {}{} id={}", p.path, origin(p), p.id)).collect();
                            format!("{}:\n{}", host.name, lines.join("\n"))
                        }
                    });
                }
                lines.extend(d.unlisted().map(|why| format!("hosts.toml: not read ({why})")));
                lines.join("\n")
            }
            "add_project" => {
                let host = d.host(p.host.as_deref().unwrap_or(LOCAL)).await?;
                let project = d.add_project(&host, &need(&p.project, "project")?).await?;
                format!(
                    "{} is a project on {} (id={}): on the user's Projects page, not pinned to their sidebar. `new` starts threads in it.",
                    project.path, host.name, project.id
                )
            }
            action @ ("archive" | "snooze" | "pin" | "abort" | "open") => {
                let f = d.locate(&need(&p.thread_id, "thread_id")?).await?;
                let thread_id = f.thread.id.clone();
                let (done, request): (&str, Box<dyn FnOnce(u64) -> ClientMessage + Send>) = match action {
                    "archive" => {
                        let archived = p.archived.unwrap_or(true);
                        (
                            if archived { "Archived" } else { "Unarchived" },
                            Box::new(move |id| ClientMessage::ArchiveThread { id, thread_id, archived }),
                        )
                    }
                    "snooze" => {
                        let snoozed = p.snoozed.unwrap_or(true);
                        (
                            if snoozed { "Snoozed" } else { "Woke" },
                            Box::new(move |id| ClientMessage::SnoozeThread { id, thread_id, snoozed }),
                        )
                    }
                    "pin" => {
                        let pinned = p.pinned.unwrap_or(true);
                        (if pinned { "Pinned" } else { "Unpinned" }, Box::new(move |id| ClientMessage::PinThread { id, thread_id, pinned }))
                    }
                    "abort" => ("Aborted", Box::new(move |id| ClientMessage::Abort { id, thread_id })),
                    _ => ("open", Box::new(move |id| ClientMessage::FocusThread { id, thread_id })),
                };
                d.request(&f.host, request).await?;
                match done {
                    "open" => format!(
                        "Asked the desktops connected to {0} to open {1} (a desktop that doesn't connect to {0} won't).",
                        f.host.name,
                        f.link()
                    ),
                    done => format!("{done} {}.", f.link()),
                }
            }
            other => bail!("unknown action \"{other}\""),
        };
        Ok(ToolOutput::text(text))
    }
}

/// Found threads, one per line, then the hosts that didn't answer.
fn listing((found, failed): (Vec<Found>, Vec<String>), none: &str) -> String {
    let mut out = if found.is_empty() { none.to_owned() } else { found.iter().map(Found::describe).collect::<Vec<_>>().join("\n") };
    if !failed.is_empty() {
        out.push_str(&format!("\nNot reached: {}", failed.join("; ")));
    }
    out
}

/// A thread found on some host, with its project (when it still has one) and, from a
/// search, the passage that matched.
struct Found {
    host: HostInfo,
    thread: ThreadSummary,
    project: Option<Project>,
    snippet: Option<String>,
}

impl Found {
    fn link(&self) -> String {
        link(&self.thread.id, self.thread.name.as_deref())
    }

    /// One line for the model: link (name and id), status, host, project, model, age.
    fn describe(&self) -> String {
        let (t, now) = (&self.thread, crate::now());
        let status = match t.running_since {
            Some(since) => format!("working {}", ago(now - since)),
            None if !t.asks.is_empty() => "asking the user".into(),
            None if t.archived => "archived".into(),
            None if t.snoozed => "snoozed".into(),
            None => "done".into(),
        };
        let count = |n: u32, what: &str| if n > 0 { format!(", {n} job{} {what}", if n == 1 { "" } else { "s" }) } else { String::new() };
        let jobs = count(t.jobs, "running") + &count(t.free_jobs, "left running");
        let parent = t.parent_id.as_ref().map(|p| format!(" parent={p}")).unwrap_or_default();
        let project = self.project.as_ref().map_or(" project=(removed)".to_owned(), |p| format!(" project={}", p.path));
        // A thread in its own workspace works somewhere else than its project.
        let cwd = if self.project.as_ref().is_some_and(|p| p.path == t.cwd) { String::new() } else { format!(" cwd={}", t.cwd) };
        let snippet = self.snippet.as_ref().map(|s| format!("\n    …{s}…")).unwrap_or_default();
        format!(
            "- {} [{status}{jobs}] host={}{project}{cwd} model={}{parent} updated {} ago{snippet}",
            self.link(),
            self.host.name,
            t.model.as_deref().unwrap_or("?"),
            ago(now - t.updated_at)
        )
    }
}

/// The markdown link the desktop opens a thread from.
fn link(id: &str, name: Option<&str>) -> String {
    format!("[{}]({LINK}{id})", name.unwrap_or("untitled").replace(['[', ']'], ""))
}

fn ago(ms: i64) -> String {
    match (ms.max(0) as f64 / 1000.).round() as i64 {
        s if s < 60 => format!("{s}s"),
        s if s < 3600 => format!("{}m", s / 60),
        s if s < 86400 => format!("{}h", s / 3600),
        s => format!("{}d", s / 86400),
    }
}

/// A connection to another host: numbered requests, their replies picked out of what it
/// sends (its pushes are passed over).
struct Remote {
    ws: tokio::sync::Mutex<tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>>>,
}

impl Remote {
    async fn connect(host: &HostInfo) -> Result<Self> {
        if let Some(error) = &host.error {
            bail!("{error}");
        }
        let mut request = host.url.as_str().into_client_request()?;
        if let Some(bearer) = &host.bearer {
            request.headers_mut().insert(AUTHORIZATION, format!("Bearer {bearer}").parse()?);
        }
        let config = tokio_tungstenite::tungstenite::protocol::WebSocketConfig::default()
            .max_message_size(Some(MAX_MESSAGE))
            .max_frame_size(Some(MAX_MESSAGE));
        // Connected once it said hello: a host that speaks another protocol is not talked to.
        let connect = async {
            let (mut ws, _) = tokio_tungstenite::connect_async_with_config(request, Some(config), false).await?;
            while let Some(message) = ws.next().await {
                let Message::Text(text) = message? else { continue };
                let Ok(ServerMessage::Hello { version, protocol, .. }) = serde_json::from_str(&text) else { continue };
                if protocol != dirigent_protocol::PROTOCOL {
                    let (ours, which) =
                        (dirigent_protocol::release::VERSION, if protocol < dirigent_protocol::PROTOCOL { "it" } else { "this host" });
                    bail!("it runs dirigent {version}, which {ours} here can't talk to: update {which}");
                }
                return Ok(ws);
            }
            bail!("connection closed")
        };
        match tokio::time::timeout(CONNECT, connect).await {
            Ok(Ok(ws)) => Ok(Self { ws: tokio::sync::Mutex::new(ws) }),
            Ok(Err(e)) => bail!("cannot reach {}: {e:#}", host.url),
            Err(_) => bail!("cannot reach {} (no answer in {}s)", host.url, CONNECT.as_secs()),
        }
    }

    async fn request(&self, request: ClientMessage, id: u64) -> Result<ServerMessage> {
        let mut ws = self.ws.lock().await;
        ws.send(Message::Text(serde_json::to_string(&request)?.into())).await?;
        let reply = async {
            while let Some(message) = ws.next().await {
                let Message::Text(text) = message? else { continue };
                // What this build doesn't know (a newer host's push) is not the reply.
                let Ok(message) = serde_json::from_str::<ServerMessage>(&text) else { continue };
                if reply_to(&message) == Some(id) {
                    return Ok(message);
                }
            }
            bail!("connection closed")
        };
        tokio::time::timeout(REQUEST, reply).await.map_err(|_| anyhow!("no reply in {}s", REQUEST.as_secs()))?
    }
}

/// The request a message answers, if it is a reply.
fn reply_to(message: &ServerMessage) -> Option<u64> {
    serde_json::to_value(message).ok()?.get("reply_to")?.as_u64()
}

/// The hosts one call of the tool talks to, as the calling thread.
struct Hosts {
    svc: Arc<Service>,
    me: String,
    /// This host, as the others are named.
    this: HostInfo,
    /// `local` first, then the others; and why the others couldn't be listed.
    hosts: Option<(Vec<HostInfo>, Option<String>)>,
    remotes: HashMap<String, Arc<Remote>>,
    next: u64,
}

impl Hosts {
    fn new(svc: Arc<Service>, me: &str) -> Self {
        let this = HostInfo { name: LOCAL.into(), url: String::new(), bearer: None, error: None };
        Self { svc, me: me.to_owned(), this, hosts: None, remotes: HashMap::new(), next: 0 }
    }

    /// One request to `host`; an `Error` reply is an error.
    async fn request(&mut self, host: &HostInfo, request: impl FnOnce(u64) -> ClientMessage) -> Result<ServerMessage> {
        self.next += 1;
        let id = self.next;
        let reply = if host.name == LOCAL {
            self.local(request(id)).await
        } else {
            self.remote(host).await?.request(request(id), id).await.map_err(|e| anyhow!("{}: {e:#}", host.name))?
        };
        match reply {
            ServerMessage::Error { message, .. } => bail!("{message}"),
            reply => Ok(reply),
        }
    }

    /// A request to this host's own service, without a connection: nothing one would be
    /// told (a login's steps, a screen's pieces) is asked for here.
    ///
    /// On a task of its own, as a connection's requests are: the tool's call can be dropped
    /// (a stopped run, a script that ended first), and a `CreateThread` cut short half-way
    /// would leave a thread starting for ever.
    async fn local(&self, request: ClientMessage) -> ServerMessage {
        let svc = self.svc.clone();
        let handled = tokio::spawn(async move {
            let conn = Conn { id: u64::MAX, out: tokio::sync::mpsc::unbounded_channel().0, reading: Default::default() };
            svc.handle(request, &conn).await
        });
        handled.await.unwrap_or_else(|e| ServerMessage::Error { reply_to: None, message: format!("the request was not finished: {e}") })
    }

    /// The connection to `host`, made once per call. One that can't be made asks for the
    /// hosts again, once: it may be reached differently since (an ssh tunnel restarted on
    /// another port).
    async fn remote(&mut self, host: &HostInfo) -> Result<Arc<Remote>> {
        if let Some(remote) = self.remotes.get(&host.name) {
            return Ok(remote.clone());
        }
        let remote = match Remote::connect(host).await {
            Ok(remote) => remote,
            Err(e) => {
                self.hosts = None;
                let fresh = self.hosts().await.into_iter().find(|h| h.name == host.name);
                match fresh.filter(|f| f.error.is_none() && (f.url != host.url || f.bearer != host.bearer)) {
                    Some(fresh) => Remote::connect(&fresh).await?,
                    None => return Err(e),
                }
            }
        };
        let remote = Arc::new(remote);
        self.remotes.insert(host.name.clone(), remote.clone());
        Ok(remote)
    }

    /// `local` first, then the hosts its `hosts.toml` lists. A broken file leaves just
    /// `local` (and is reported by `each`), so it never blocks work here.
    async fn hosts(&mut self) -> Vec<HostInfo> {
        if self.hosts.is_none() {
            let listed = match self.local(ClientMessage::ListHosts { id: 0 }).await {
                ServerMessage::Hosts { hosts, .. } => (hosts, None),
                ServerMessage::Error { message, .. } => (vec![], Some(message)),
                _ => (vec![], Some("the service didn't list its hosts".into())),
            };
            self.hosts = Some((std::iter::once(self.this.clone()).chain(listed.0).collect(), listed.1));
        }
        self.hosts.as_ref().map(|(hosts, _)| hosts.clone()).unwrap_or_default()
    }

    async fn host(&mut self, name: &str) -> Result<HostInfo> {
        let hosts = self.hosts().await;
        let names = hosts.iter().map(|h| h.name.as_str()).collect::<Vec<_>>().join(", ");
        hosts.into_iter().find(|h| h.name == name).ok_or_else(|| anyhow!("no host \"{name}\"; hosts: {names}"))
    }

    /// Why the hosts besides `local` couldn't be listed, if they couldn't (a broken `hosts.toml`).
    fn unlisted(&self) -> Option<String> {
        self.hosts.as_ref()?.1.clone()
    }

    async fn threads(&mut self, host: &HostInfo) -> Result<Vec<ThreadSummary>> {
        let ServerMessage::Threads { threads, .. } = self.request(host, |id| ClientMessage::ListThreads { id }).await? else {
            bail!("{} didn't list its threads", host.name)
        };
        for t in &threads {
            learn(&t.id, Some(&host.name));
        }
        Ok(threads)
    }

    async fn projects(&mut self, host: &HostInfo) -> Result<Vec<Project>> {
        match self.request(host, |id| ClientMessage::ListProjects { id }).await? {
            ServerMessage::Projects { projects, .. } => Ok(projects),
            _ => bail!("{} didn't list its projects", host.name),
        }
    }

    async fn add_project(&mut self, host: &HostInfo, path: &str) -> Result<Project> {
        match self.request(host, |id| ClientMessage::AddProject { id, path: path.to_owned() }).await? {
            ServerMessage::ProjectAdded { project, .. } => Ok(project),
            _ => bail!("{} didn't add the project", host.name),
        }
    }

    /// `thread` with its host and project.
    async fn found(&mut self, host: &HostInfo, thread: ThreadSummary, snippet: Option<String>) -> Found {
        let projects = self.projects(host).await.unwrap_or_default();
        let project = projects.into_iter().find(|p| Some(&p.id) == thread.project_id.as_ref());
        Found { host: host.clone(), thread, project, snippet }
    }

    /// The thread with `id` (or its `dirigent://thread/` link) and the host it lives on:
    /// the one it was last seen on first, then every host.
    async fn locate(&mut self, id: &str) -> Result<Found> {
        let id = id.trim().strip_prefix(LINK).unwrap_or(id.trim()).to_owned();
        let mut hosts = self.hosts().await;
        if let Some(known) = owner(&id) {
            hosts.sort_by_key(|h| h.name != known);
        }
        let mut errors = vec![];
        for host in hosts {
            match self.threads(&host).await {
                Ok(threads) => {
                    if let Some(thread) = threads.into_iter().find(|t| t.id == id) {
                        return Ok(self.found(&host, thread, None).await);
                    }
                }
                Err(e) => errors.push(format!("{}: {e:#}", host.name)),
            }
        }
        learn(&id, None);
        match errors.is_empty() {
            true => bail!("no thread {id}"),
            false => bail!("no thread {id} (not asked: {})", errors.join("; ")),
        }
    }

    async fn me(&mut self) -> Result<ThreadSummary> {
        let local = self.this.clone();
        let me = self.me.clone();
        self.threads(&local).await?.into_iter().find(|t| t.id == me).ok_or_else(|| anyhow!("no thread {me} here"))
    }

    /// Refuse a model `host` doesn't serve, naming the ones it does.
    async fn check_model(&mut self, host: &HostInfo, model: &str) -> Result<()> {
        let ServerMessage::Models { models, .. } = self.request(host, |id| ClientMessage::ListModels { id }).await? else {
            bail!("{} didn't list its models", host.name)
        };
        if !models.iter().any(|m| m.id == model) {
            bail!("{} doesn't serve {model}; it has: {}", host.name, models.iter().map(|m| m.id.as_str()).collect::<Vec<_>>().join(", "));
        }
        Ok(())
    }

    /// Create a sub-agent thread called `name` here and send it `task`. Returns its id.
    async fn spawn(&mut self, p: &Input, name: &str, task: &str) -> Result<String> {
        let me = self.me().await?;
        let project_id = me.project_id.clone().ok_or_else(|| anyhow!("no project to spawn in"))?;
        let local = self.this.clone();
        if let Some(model) = &p.model {
            self.check_model(&local, model).await?;
        }
        let tail = match p.report {
            Some(Report::Done) => "When you finish, it is told that you did and may read your final reply",
            _ => "When you finish, your final reply is reported to it",
        };
        // Picked here so the prompt, which goes with the create, can tell the child its id.
        let thread_id = uuid::Uuid::now_v7().to_string();
        let text = format!(
            "{}\n\n---\nYou are a sub-agent (thread {thread_id}) spawned by thread {}. {tail}, so make that reply a complete, \
             self-contained summary of what you did, found and left undone.",
            task.trim(),
            self.me
        );
        // The parent's tools reach the child through the service (`CreateThread::parent`).
        let prompt = FirstPrompt {
            text,
            model: p.model.clone().or(me.model),
            effort: p.effort.clone().or(me.effort),
            images: vec![],
            title: None,
            tools: None,
        };
        let parent = self.me.clone();
        let create = |id| ClientMessage::CreateThread {
            id,
            thread_id: Some(thread_id.clone()),
            project_id,
            workspace: p.workspace.unwrap_or(false),
            base: p.base.clone(),
            parent: Some(parent),
            name: Some(name.to_owned()),
            report: p.report.unwrap_or_default(),
            notify: p.notify.unwrap_or_default(),
            prompt: Some(prompt),
        };
        let sent = self.request(&local, create).await;
        match sent.map_err(|e| may_have(e, &format!("it may have started anyway, as thread {thread_id}")))? {
            ServerMessage::ThreadCreated { thread, .. } => Ok(thread.id),
            _ => bail!("the thread wasn't created"),
        }
    }

    /// Start a top-level thread on any reachable host: a thread in the user's sidebar,
    /// prompted with `task` as it is. Nobody is told how it goes.
    async fn create(&mut self, p: &Input, name: &str, task: &str) -> Result<Found> {
        let me = self.me().await?;
        let host = self.host(p.host.as_deref().unwrap_or(LOCAL)).await?;
        let project = self.pick_project(&host, p.project.as_deref(), &me).await?;
        let model = p.model.clone().or(me.model.clone()).ok_or_else(|| anyhow!("no model: pass `model`"))?;
        self.check_model(&host, &model).await?;
        // No parent to inherit tools from: it gets the ones the caller has.
        let tools = self.svc.thread_tools(&self.me)?;
        let prompt = FirstPrompt {
            text: task.trim().to_owned(),
            model: Some(model),
            effort: p.effort.clone().or(me.effort),
            images: vec![],
            title: None,
            tools: Some(tools),
        };
        let create = |id| ClientMessage::CreateThread {
            id,
            thread_id: None,
            project_id: project.id.clone(),
            workspace: p.workspace.unwrap_or(false),
            base: p.base.clone(),
            parent: None,
            name: Some(name.to_owned()),
            report: Report::default(),
            notify: Notify::default(),
            prompt: Some(prompt),
        };
        let sent = self.request(&host, create).await;
        let ServerMessage::ThreadCreated { thread, .. } =
            sent.map_err(|e| may_have(e, "the thread may have started anyway: `list` shows it if it has"))?
        else {
            bail!("the thread wasn't created")
        };
        learn(&thread.id, Some(&host.name));
        Ok(Found { host, thread: *thread, project: Some(project), snippet: None })
    }

    /// `want` (an id or a path) on `host`; by default the caller's project, which on another
    /// host is the one with the same origin.
    async fn pick_project(&mut self, host: &HostInfo, want: Option<&str>, me: &ThreadSummary) -> Result<Project> {
        let projects = self.projects(host).await?;
        let listing = || match projects.is_empty() {
            true => "; it has no projects".to_owned(),
            false => format!("; its projects: {}", projects.iter().map(|p| p.path.as_str()).collect::<Vec<_>>().join(", ")),
        };
        if let Some(want) = want {
            let path = want.trim_end_matches('/');
            let found = projects.iter().find(|p| p.id == want || p.path.trim_end_matches('/') == path);
            return found.cloned().ok_or_else(|| anyhow!("{} has no project \"{want}\"{}", host.name, listing()));
        }
        let local = self.this.clone();
        let mine = self.projects(&local).await?.into_iter().find(|p| Some(&p.id) == me.project_id.as_ref());
        match mine {
            Some(mine) if host.name == LOCAL => Ok(mine),
            mine => {
                let origin = mine.and_then(|p| p.origin);
                let mut same = projects.iter().filter(|p| origin.is_some() && p.origin == origin);
                match (same.next(), same.next()) {
                    (Some(only), None) => Ok(only.clone()),
                    _ => bail!("pass `project`: no single project on {} matches yours{}", host.name, listing()),
                }
            }
        }
    }

    /// Message a thread: a new turn if it is idle, a steer or follow-up if it runs.
    async fn send(&mut self, id: &str, text: &str, mode: PromptMode) -> Result<Found> {
        let f = self.locate(id).await?;
        let mode = if f.thread.running_since.is_some() { mode } else { PromptMode::Prompt };
        let (thread_id, text) = (f.thread.id.clone(), text.to_owned());
        let prompt =
            |id| ClientMessage::Prompt { id, thread_id, text, mode, model: None, effort: None, images: vec![], title: None, tools: None };
        let sent = self.request(&f.host, prompt).await;
        sent.map_err(|e| may_have(e, "the thread may have got the message anyway: `read` it before sending again"))?;
        Ok(f)
    }

    /// A thread, its final reply (its last message that ended a turn) and its whole
    /// conversation as text.
    async fn read(&mut self, id: &str) -> Result<(Found, Option<String>, String)> {
        let f = self.locate(id).await?;
        let thread_id = f.thread.id.clone();
        let ServerMessage::Thread { entries, .. } = self.request(&f.host, |id| ClientMessage::GetThread { id, thread_id }).await? else {
            bail!("{} didn't hand over the thread", f.host.name)
        };
        let current = entries.last().map(|last| branch(&entries, &last.id)).unwrap_or_default();
        let turns: Vec<(&str, String)> = current
            .iter()
            .filter_map(|e| match &e.body {
                Body::User { content } => Some(("user", text_of(content))),
                Body::Assistant(a) if a.stop != Stop::ToolUse => Some(("assistant", text_of(&a.content))),
                _ => None,
            })
            .collect();
        let reply = turns.iter().rev().find(|(who, _)| *who == "assistant").map(|(_, text)| text.clone());
        let transcript = turns.iter().map(|(who, text)| format!("{who}: {text}")).collect::<Vec<_>>().join("\n\n");
        Ok((f, reply, transcript))
    }

    /// Threads on every host whose name contains every word of `query` (first) or whose
    /// messages do (the host's full-text search), and the hosts that didn't answer. Hosts'
    /// ranks don't compare, so their lists are interleaved, local first, up to `limit`.
    async fn search(&mut self, query: &str, limit: usize) -> Result<(Vec<Found>, Vec<String>)> {
        let words: Vec<String> = query.to_lowercase().split_whitespace().map(str::to_owned).collect();
        if words.is_empty() {
            bail!("`query` is empty");
        }
        let (mut lists, mut failed) = (vec![], vec![]);
        for host in self.hosts().await {
            match self.search_host(&host, query, &words, limit).await {
                Ok(found) => lists.push(found),
                Err(e) => failed.push(format!("{} ({e:#})", host.name)),
            }
        }
        failed.extend(self.unlisted().map(|why| format!("hosts.toml ({why})")));
        let mut found = vec![];
        while found.len() < limit && lists.iter().any(|l| !l.is_empty()) {
            for list in &mut lists {
                if !list.is_empty() && found.len() < limit {
                    found.push(list.remove(0));
                }
            }
        }
        Ok((found, failed))
    }

    async fn search_host(&mut self, host: &HostInfo, query: &str, words: &[String], limit: usize) -> Result<Vec<Found>> {
        let search = |id| ClientMessage::SearchThreads { id, query: query.to_owned(), limit: limit as u64 };
        let hits: Vec<SearchHit> = match self.request(host, search).await? {
            ServerMessage::Search { hits, .. } => hits,
            _ => vec![],
        };
        let threads = self.threads(host).await?;
        let named = |t: &&ThreadSummary| t.name.as_ref().is_some_and(|n| words.iter().all(|w| n.to_lowercase().contains(w)));
        let mut matches: Vec<(ThreadSummary, Option<String>)> = threads.iter().filter(named).map(|t| (t.clone(), None)).collect();
        for hit in hits {
            if let Some(t) = threads.iter().find(|t| t.id == hit.thread_id).filter(|t| !matches.iter().any(|(m, _)| m.id == t.id)) {
                matches.push((t.clone(), Some(hit.snippet)));
            }
        }
        let mut found = vec![];
        for (thread, snippet) in matches.into_iter().take(limit) {
            found.push(self.found(host, thread, snippet).await);
        }
        Ok(found)
    }

    /// The most recently active top-level threads (what the sidebar shows) on every host,
    /// and the hosts that didn't answer.
    async fn list(&mut self, limit: usize) -> Result<(Vec<Found>, Vec<String>)> {
        let (mut found, mut failed) = (vec![], vec![]);
        for host in self.hosts().await {
            match self.threads(&host).await {
                Ok(threads) => {
                    for thread in threads.into_iter().filter(|t| t.parent_id.is_none() && !t.archived).take(limit) {
                        found.push(self.found(&host, thread, None).await);
                    }
                }
                Err(e) => failed.push(format!("{} ({e:#})", host.name)),
            }
        }
        failed.extend(self.unlisted().map(|why| format!("hosts.toml ({why})")));
        found.sort_by_key(|f| std::cmp::Reverse(f.thread.updated_at));
        found.truncate(limit);
        Ok((found, failed))
    }
}

/// A request that hands a model a message and got no reply in time may still have gone
/// through: its error says so (`maybe`), so the caller looks before it sends again.
fn may_have(e: anyhow::Error, maybe: &str) -> anyhow::Error {
    if format!("{e:#}").contains("no reply in") { anyhow!("{e:#}; {maybe}") } else { e }
}
