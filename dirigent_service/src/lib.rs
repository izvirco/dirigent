//! dirigent_service: owns this host's threads (sqlite), runs their agents (`dirigent_agent`)
//! and serves them to desktops over WebSocket.
//!
//! A library so the desktop can run the local service in-process (`run`) and the `dirigent`
//! command line (`dirigent_cli`) shares its dirs; the `dirigent_service` binary is a thin
//! wrapper for remote hosts.

mod agent;
mod auth;
mod dirigent;
mod dirs;
pub mod forge;
mod git;
pub mod hosts;
mod live;
mod logging;
mod mcp;
pub mod migration;
pub mod platform;
mod proxy;
mod screen;
mod server;
mod storage;
mod title;
pub mod tunnel;
mod vcs;

use anyhow::{Result, anyhow};
use dirigent_agent::config::Dirs as AgentDirs;
use dirigent_agent::proxy::Proxy;
use dirigent_agent::session::{Prompt, Session, Thread};
use dirigent_agent::{Agent, Out};
use dirigent_protocol::transcript::{Body, Entry, Event, Notice};
use dirigent_protocol::{
    ClientMessage, FirstPrompt, HostInfo, ModelInfo, Notify, PromptMode, Report, ServerMessage, Setup, ThreadSummary, TitleGen, Vcs,
};
use live::Live;
use server::Conn;
use std::collections::HashMap;
use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use storage::Storage;
use tokio::sync::broadcast;
use tracing::{debug, error, info, warn};

pub use auth::{KEY_USAGE, key_command};
pub use logging::init_logging;

const IDLE_UNLOAD: Duration = Duration::from_secs(30 * 60);
/// How long `shutdown` waits for the agent's MCP servers and screens to close.
const SHUTDOWN_WAIT: Duration = Duration::from_secs(3);
pub const DEFAULT_PORT: u16 = 7777;
/// Sub-agents can't nest deeper than this (a root thread is depth 0).
const MAX_DEPTH: usize = 3;
/// How much of a sub-agent's reply a parent's notice carries.
const NOTICE_CHARS: usize = 6000;

/// The local service's port: `DIRIGENT_PORT`, else 7777, or 7776 in a nightly build. The
/// desktop uses it both to reach and to embed the local service, so a test instance with its
/// own port never touches the real one, and a nightly desktop never finds a stable one's
/// service (it would use it as its own).
pub fn port() -> u16 {
    let default = if dirigent_protocol::release::nightly() { DEFAULT_PORT - 1 } else { DEFAULT_PORT };
    std::env::var("DIRIGENT_PORT").ok().and_then(|p| p.parse().ok()).unwrap_or(default)
}

pub struct Service {
    storage: Mutex<Storage>,
    /// This host's agent: the live sessions of its threads.
    agent: Arc<Agent>,
    /// The way to the models: the agent's, and what lists them and names threads.
    models: Arc<Proxy>,
    /// What the threads' sessions are up to (`live.rs`).
    live: Live,
    /// Threads whose workspace was gone (or no longer known to the vcs) at the last
    /// `sweep_workspaces`: what `workspace_missing` reports between sweeps.
    missing: Mutex<std::collections::HashSet<String>>,
    /// Threads being created with their first prompt, until it is delivered: they take no
    /// other prompt meanwhile. `Some(Workspace)` while their workspace is being made.
    starting: Mutex<HashMap<String, Option<Setup>>>,
    /// The MCP sign-ins in progress (`mcp.rs`).
    logins: mcp::Logins,
    /// This host's CLIProxyAPI as last seen (`proxy.rs`).
    proxy: proxy::Poller,
    /// The pictures of the threads using the computer (`screen.rs`).
    screens: screen::Screens,
    /// Pushes to every connected client.
    pub push: broadcast::Sender<ServerMessage>,
    pub host_name: String,
    /// `Storage::service_id`, sent in every `Hello`.
    pub service_id: String,
}

impl Service {
    fn push(&self, msg: ServerMessage) {
        let _ = self.push.send(msg);
    }

    fn threads(&self) -> Result<Vec<ThreadSummary>> {
        let mut threads = self.storage.lock().unwrap().list_threads()?;
        let missing = self.missing.lock().unwrap();
        let starting = self.starting.lock().unwrap();
        for t in &mut threads {
            // A thread still starting works, whatever its session is up to: `abandon` drops
            // the session, and the run in it, before the thread is gone.
            t.running_since = self.live.running_since(&t.id).or(t.prompted_at.filter(|_| starting.contains_key(&t.id)));
            (t.jobs, t.free_jobs) = self.live.jobs(&t.id);
            // A workspace removed outside dirigent: the thread can't run until it is dealt with.
            t.workspace_missing = missing.contains(&t.id);
            t.setup = starting.get(&t.id).copied().flatten().or(self.live.nix_shell(&t.id).then_some(Setup::NixShell));
            t.compacting_since = self.live.compacting_since(&t.id);
            // A screen is its run's: one left by a run that never settled doesn't count.
            t.screen = t.running_since.is_some() && self.screens.has(&t.id);
        }
        Ok(threads)
    }

    /// Drop the thread's session, and with it the run it may have been in (no `RunEnd` comes
    /// for that one), its jobs and its screen. A run begun for a session still to load (its
    /// prompt on the way) isn't the session's to end: a new thread goes on running while it
    /// moves into its workspace, and whoever waited on a load that failed settles the run.
    fn unload(&self, thread_id: &str) {
        self.screens.clear(thread_id);
        let loaded = self.agent.session(thread_id).is_some();
        self.agent.unload(thread_id);
        self.live.unloaded(thread_id, loaded);
    }

    /// The tool switches the thread's last prompt had on.
    pub(crate) fn thread_tools(&self, thread_id: &str) -> Result<Vec<String>> {
        self.storage.lock().unwrap().thread_tools(thread_id)
    }

    fn push_threads(&self) {
        match self.threads() {
            Ok(threads) => self.push(ServerMessage::Threads { reply_to: None, threads }),
            Err(e) => error!("list threads: {e:#}"),
        }
    }

    /// Notice workspaces removed outside dirigent (the directory deleted, or the vcs made to
    /// forget it: `jj workspace forget`, a pruned worktree): push the thread list when the
    /// set of missing ones changed since the last look. Runs periodically and on `GetThread`;
    /// the vcs probe is too slow for every `threads()`.
    fn sweep_workspaces(&self) {
        let threads = match self.storage.lock().unwrap().list_threads() {
            Ok(threads) => threads,
            Err(_) => return,
        };
        let now: std::collections::HashSet<String> = threads
            .into_iter()
            .filter(|t| t.workspace.as_deref().is_some_and(|w| !vcs::is_workspace(Path::new(w))))
            .map(|t| t.id)
            .collect();
        let changed = {
            let mut missing = self.missing.lock().unwrap();
            // Explains a `Recreate` / `Back to project` banner after the fact. Debug: removing
            // workspaces by hand is normal, and the banner already tells the user.
            for id in now.difference(&missing) {
                debug!(thread = %id, "workspace missing");
            }
            let changed = *missing != now;
            *missing = now;
            changed
        };
        if changed {
            self.push_threads();
        }
    }

    fn push_projects(&self) {
        match self.storage.lock().unwrap().list_projects() {
            Ok(projects) => self.push(ServerMessage::Projects { reply_to: None, projects }),
            Err(e) => error!("list projects: {e:#}"),
        }
    }

    /// Handle one client request; the reply is sent back on the requesting connection.
    pub async fn handle(self: &Arc<Self>, req: ClientMessage, conn: &Conn) -> ServerMessage {
        let id = match &req {
            ClientMessage::ListProjects { id }
            | ClientMessage::AddProject { id, .. }
            | ClientMessage::ListDirs { id, .. }
            | ClientMessage::RemoveProject { id, .. }
            | ClientMessage::ListThreads { id }
            | ClientMessage::GetThread { id, .. }
            | ClientMessage::CreateThread { id, .. }
            | ClientMessage::MoveToWorkspace { id, .. }
            | ClientMessage::RemoveWorkspace { id, .. }
            | ClientMessage::VcsStatus { id, .. }
            | ClientMessage::Prompt { id, .. }
            | ClientMessage::EditMessage { id, .. }
            | ClientMessage::ForkThread { id, .. }
            | ClientMessage::Answer { id, .. }
            | ClientMessage::Abort { id, .. }
            | ClientMessage::CancelQueued { id, .. }
            | ClientMessage::ArchiveThread { id, .. }
            | ClientMessage::RenameThread { id, .. }
            | ClientMessage::RetitleThread { id, .. }
            | ClientMessage::SnoozeThread { id, .. }
            | ClientMessage::PinThread { id, .. }
            | ClientMessage::PinMessage { id, .. }
            | ClientMessage::MarkSeen { id, .. }
            | ClientMessage::FocusThread { id, .. }
            | ClientMessage::SearchThreads { id, .. }
            | ClientMessage::DeleteThread { id, .. }
            | ClientMessage::ListModels { id }
            | ClientMessage::LoginAnswer { id, .. }
            | ClientMessage::LoginCancel { id, .. }
            | ClientMessage::ListMcpServers { id }
            | ClientMessage::CheckMcpServer { id, .. }
            | ClientMessage::SaveMcpServer { id, .. }
            | ClientMessage::RemoveMcpServer { id, .. }
            | ClientMessage::McpLogin { id, .. }
            | ClientMessage::McpLogout { id, .. }
            | ClientMessage::ListHosts { id }
            | ClientMessage::GenerateTitle { id, .. }
            | ClientMessage::GetBlob { id, .. }
            | ClientMessage::GetScreen { id, .. }
            | ClientMessage::ListUsage { id, .. }
            | ClientMessage::ListProxy { id, .. }
            | ClientMessage::SetProxy { id, .. } => *id,
        };
        match self.try_handle(req, conn).await {
            Ok(reply) => reply,
            Err(e) => {
                warn!("request {id} failed: {e:#}");
                ServerMessage::Error { reply_to: Some(id), message: format!("{e:#}") }
            }
        }
    }

    async fn try_handle(self: &Arc<Self>, req: ClientMessage, conn: &Conn) -> Result<ServerMessage> {
        Ok(match req {
            ClientMessage::ListProjects { id } => {
                ServerMessage::Projects { reply_to: Some(id), projects: self.storage.lock().unwrap().list_projects()? }
            }
            ClientMessage::AddProject { id, path } => {
                let path = dirs::expand(&path)?;
                anyhow::ensure!(path.is_dir(), "not a directory on this host: {}", path.display());
                let origin = git::origin(&path);
                let project_id = uuid::Uuid::now_v7().to_string();
                // The one it already is, if that path is a project.
                let project = self.storage.lock().unwrap().add_project(&project_id, &path.to_string_lossy(), origin.as_deref(), now())?;
                if project.id == project_id {
                    info!(project = %project_id, path = %path.display(), "project added");
                    self.push_projects();
                }
                ServerMessage::ProjectAdded { reply_to: id, project }
            }
            ClientMessage::ListDirs { id, path } => {
                // A big directory is many stats: off the async workers.
                let dir = dirs::expand(&path)?;
                ServerMessage::Dirs { reply_to: id, dirs: tokio::task::spawn_blocking(move || dirs::list(&dir)).await?? }
            }
            ClientMessage::RemoveProject { id, project_id } => {
                self.storage.lock().unwrap().remove_project(&project_id, now())?;
                info!(project = %project_id, "project removed");
                self.push_projects();
                self.push_threads();
                ServerMessage::Ok { reply_to: id }
            }
            ClientMessage::ListThreads { id } => ServerMessage::Threads { reply_to: Some(id), threads: self.threads()? },
            ClientMessage::GetThread { id, thread_id } => {
                self.sweep_workspaces();
                let entries = self.storage.lock().unwrap().get_thread(&thread_id)?.ok_or_else(|| anyhow!("no such thread"))?;
                ServerMessage::Thread { reply_to: id, thread_id, entries }
            }
            ClientMessage::CreateThread { id, thread_id, project_id, workspace, base, parent, name, report, notify, prompt } => {
                let thread_id = new_thread_id(thread_id)?;
                let thread = {
                    let storage = self.storage.lock().unwrap();
                    let mut cwd = storage.project_path(&project_id)?.ok_or_else(|| anyhow!("no such project"))?;
                    // A sub-agent works where its parent works, unless it gets its own workspace.
                    if let Some(p) = &parent {
                        let parent_cwd = storage.thread_cwd(p)?.ok_or_else(|| anyhow!("no such parent thread"))?;
                        anyhow::ensure!(storage.depth(p)? + 1 < MAX_DEPTH, "sub-agents can't nest deeper than {MAX_DEPTH} levels");
                        if !workspace {
                            cwd = parent_cwd;
                        }
                    }
                    let thread = storage.create_thread(&thread_id, &project_id, &cwd, parent.as_deref(), name.as_deref(), now())?;
                    // A sub-agent's prompts come from the `dirigent` tool, which knows nothing of
                    // the user's tool settings: it gets its parent's tools.
                    if let Some(p) = &parent {
                        storage.set_tools(&thread_id, &storage.thread_tools(p)?)?;
                        storage.set_report(&thread_id, report, notify)?;
                    }
                    // With its first prompt along, the thread works from the start: its run begins
                    // under the lock that creates it, so any `Threads` push that lists it (other
                    // threads' activity) lists it running, never idle in the inbox while its
                    // workspace is made or its prompt is on the way.
                    if let Some(p) = &prompt {
                        storage.set_prompted(&thread_id, &p.text, p.model.as_deref(), p.effort.as_deref(), now())?;
                        self.live.begin_run(&thread_id);
                        self.starting.lock().unwrap().insert(thread_id.clone(), workspace.then_some(Setup::Workspace));
                    }
                    thread
                };
                info!(
                    thread = %thread_id,
                    project = %project_id,
                    parent = parent.as_deref().map(tracing::field::display),
                    workspace,
                    "thread created"
                );
                if workspace {
                    // Clients see it making its workspace. Without a prompt there is no push:
                    // the creator's `Prompt` right after pushes it running.
                    if prompt.is_some() {
                        self.push_threads();
                    }
                    // Off the async workers: `git worktree add` / `jj workspace add` can take a
                    // while, and the push above would wait behind it on this one.
                    let (this, id) = (self.clone(), thread_id.clone());
                    let made = tokio::task::spawn_blocking(move || this.move_to_workspace(&id, true, base.as_deref()))
                        .await
                        .unwrap_or_else(|e| Err(anyhow!("making the workspace failed: {e}")));
                    if let Err(e) = made {
                        return Err(self.abandon(&thread_id, e));
                    }
                }
                if let Some(p) = &prompt {
                    if let Some(setup) = self.starting.lock().unwrap().get_mut(&thread_id) {
                        *setup = None;
                    }
                    self.push_threads();
                    if let Err(e) = self.deliver(&thread_id, PromptMode::Prompt, p, None).await {
                        return Err(self.abandon(&thread_id, e));
                    }
                    self.starting.lock().unwrap().remove(&thread_id);
                }
                // As it is listed now: in its workspace, prompted, running.
                let thread = self.threads()?.into_iter().find(|t| t.id == thread_id).unwrap_or(thread);
                ServerMessage::ThreadCreated { reply_to: id, thread: Box::new(thread) }
            }
            ClientMessage::MoveToWorkspace { id, thread_id, base } => {
                self.move_to_workspace(&thread_id, false, base.as_deref())?;
                self.push_threads();
                ServerMessage::Ok { reply_to: id }
            }
            ClientMessage::RemoveWorkspace { id, thread_id, detach } => {
                anyhow::ensure!(self.live.running_since(&thread_id).is_none(), "the thread is running");
                self.remove_workspace(&thread_id, detach)?;
                self.push_threads();
                ServerMessage::Ok { reply_to: id }
            }
            ClientMessage::VcsStatus { id, path } => {
                let dir = Path::new(&path);
                // A background probe, so a working copy the vcs can't answer for (a forgotten
                // workspace) reads as no repo rather than an error.
                let (vcs, dirty) = match vcs::detect(dir).map(|v| (v, vcs::dirty(v, dir))) {
                    Some((v, Ok(dirty))) => (Some(v), dirty),
                    _ => (None, false),
                };
                let branch = vcs.and_then(|v| vcs::branch(v, dir));
                let refs = vcs.map(|v| vcs::refs(v, dir)).unwrap_or_default();
                ServerMessage::VcsStatus { reply_to: id, path, vcs, dirty, branch, refs }
            }
            ClientMessage::Prompt { id, thread_id, text, mode, model, effort, images, title, tools } => {
                // Its first prompt goes first: a second one now would load its session ahead
                // of it (in the project, while its workspace is being made).
                anyhow::ensure!(!self.starting(&thread_id), "the thread is still starting; try again in a moment");
                self.prompt(&thread_id, mode, &FirstPrompt { text, model, effort, images, title, tools }, None).await?;
                ServerMessage::Ok { reply_to: id }
            }
            ClientMessage::EditMessage { id, thread_id, entry_id, text, model, effort, images, tools } => {
                anyhow::ensure!(!self.starting(&thread_id), "the thread is still starting; try again in a moment");
                // A thread can't go back under a run; its session refuses it too.
                anyhow::ensure!(self.live.running_since(&thread_id).is_none(), "the thread is running; stop it first");
                anyhow::ensure!(
                    self.storage.lock().unwrap().is_user_message(&thread_id, &entry_id)?,
                    "only a message the user sent can be edited"
                );
                let p = FirstPrompt { text, model, effort, images, title: None, tools };
                self.prompt(&thread_id, PromptMode::Prompt, &p, Some(&entry_id)).await?;
                ServerMessage::Ok { reply_to: id }
            }
            ClientMessage::ForkThread { id, thread_id, entry_id, fork_id } => {
                let fork_id = new_thread_id(fork_id)?;
                let fork = self.storage.lock().unwrap().fork_thread(&thread_id, &entry_id, &fork_id, now())?;
                info!(thread = %fork_id, source = %thread_id, entry = %entry_id, "thread forked");
                self.push_threads();
                ServerMessage::ThreadCreated { reply_to: id, thread: Box::new(fork) }
            }
            ClientMessage::Answer { id, thread_id, ask_id, answers, silent } => {
                self.takes_messages(&thread_id)?;
                // Taken before anything else so a second client answering the same ask is refused.
                let ask = self.storage.lock().unwrap().take_ask(&thread_id, &ask_id)?.ok_or_else(|| anyhow!("already answered"))?;
                info!(thread = %thread_id, dismissed = answers.is_none(), silent, "ask answered");
                if silent {
                    self.push_threads();
                    return Ok(ServerMessage::Ok { reply_to: id });
                }
                // An answer is a message: an archived thread comes back for it.
                self.storage.lock().unwrap().unarchive(&thread_id)?;
                // The answer starts a turn if the model is idle (and is queued if it isn't).
                self.live.begin_run(&thread_id);
                self.push_threads();
                let sent = match self.ensure_loaded(&thread_id) {
                    Ok(session) => session.answer(ask, answers).await,
                    Err(e) => Err(e),
                };
                if sent.is_err() && self.live.settle(&thread_id).is_some() {
                    self.push_threads();
                }
                sent?;
                ServerMessage::Ok { reply_to: id }
            }
            ClientMessage::ListModels { id } => {
                // A host without a proxy has no models: that is no error, it is how clients
                // see that it has to be set up.
                let set_up = !dirigent_agent::config::Config::load(&self.agent.dirs)?.proxy.url.is_empty();
                let models = if set_up { self.models.models().await? } else { vec![] };
                let info = |m: dirigent_agent::proxy::Model| ModelInfo {
                    id: m.id,
                    name: m.name,
                    owner: m.owner,
                    context_window: m.context_window,
                    efforts: m.efforts,
                };
                ServerMessage::Models { reply_to: id, models: models.into_iter().map(info).collect() }
            }
            ClientMessage::McpLogin { id, name } => self.mcp_login(conn, id, &name).await,
            ClientMessage::LoginAnswer { id, login, prompt, value } => {
                self.logins.answer(conn.id, login, prompt, value)?;
                ServerMessage::Ok { reply_to: id }
            }
            ClientMessage::LoginCancel { id, login } => {
                self.logins.cancel(conn.id, login);
                ServerMessage::Ok { reply_to: id }
            }
            ClientMessage::ListMcpServers { id } => {
                let (servers, path) = dirigent_agent::mcp::list(&self.agent.dirs)?;
                ServerMessage::McpServers { reply_to: id, servers, path }
            }
            ClientMessage::CheckMcpServer { id, name } => {
                ServerMessage::McpStatus { reply_to: id, status: dirigent_agent::mcp::check(&self.agent.dirs, &name).await? }
            }
            ClientMessage::SaveMcpServer { id, name, config } => {
                dirigent_agent::mcp::save(&self.agent.dirs, &name, config)?;
                info!(server = %name, "MCP server saved");
                self.unload_idle_sessions();
                ServerMessage::Ok { reply_to: id }
            }
            ClientMessage::RemoveMcpServer { id, name } => {
                dirigent_agent::mcp::remove(&self.agent.dirs, &name)?;
                info!(server = %name, "MCP server removed");
                self.unload_idle_sessions();
                ServerMessage::Ok { reply_to: id }
            }
            ClientMessage::McpLogout { id, name } => {
                dirigent_agent::mcp::sign_out(&self.agent.dirs, &name)?;
                info!(server = %name, "signed out of MCP server");
                ServerMessage::Ok { reply_to: id }
            }
            // Read on every request, so edits to the file apply at once. Each host as its
            // agents connect to it: an `ssh://` one through its tunnel, started now if need
            // be (all at once: one slow ssh doesn't hold up the others), with its bearer.
            ClientMessage::ListHosts { id } => {
                let hosts = hosts::load()?;
                tunnel::retain(hosts.values().map(|h| h.url.as_str()));
                let hosts = futures_util::future::join_all(hosts.into_iter().map(|(name, host)| async move {
                    match host.endpoint().await {
                        Ok(e) => HostInfo { name, url: e.url, bearer: e.bearer, error: None },
                        Err(e) => HostInfo { name, url: host.url, bearer: None, error: Some(format!("{e:#}")) },
                    }
                }))
                .await;
                ServerMessage::Hosts { reply_to: id, hosts }
            }
            ClientMessage::GenerateTitle { id, title, text } => title::generate(&*self.models, &title, &text, None).await?.reply(id),
            ClientMessage::RetitleThread { id, thread_id, model, reasoning } => {
                let (prompts, reply) = self.storage.lock().unwrap().conversation(&thread_id)?;
                anyhow::ensure!(!prompts.is_empty(), "the thread has no messages to name it from");
                let settings = TitleGen { model, reasoning, prompt: title::PROMPT.into() };
                let r = title::generate(&*self.models, &settings, &title::digest(&prompts, reply.as_deref()), None).await?;
                self.storage.lock().unwrap().set_name(&thread_id, &r.title)?;
                info!(thread = %thread_id, "thread retitled");
                self.push_threads();
                r.reply(id)
            }
            ClientMessage::GetBlob { id, blob_id } => {
                let (mime_type, bytes) = self.storage.lock().unwrap().blob(&blob_id)?.ok_or_else(|| anyhow!("no such blob"))?;
                ServerMessage::Blob {
                    reply_to: id,
                    blob_id,
                    mime_type,
                    data: base64::Engine::encode(&base64::prelude::BASE64_STANDARD, bytes),
                }
            }
            ClientMessage::GetScreen { id, thread_id, frame, offset } => {
                let piece = self.screens.piece(&conn.reading, &thread_id, frame, usize::try_from(offset)?).await;
                ServerMessage::Screen {
                    reply_to: id,
                    thread_id,
                    frame: piece.frame,
                    mime_type: "image/jpeg".into(),
                    total: piece.total as u64,
                    offset,
                    data: piece.data,
                }
            }
            ClientMessage::ListUsage { id, since, until, bucket_ms } => {
                ServerMessage::Usage { reply_to: id, buckets: self.storage.lock().unwrap().usage(since, until, bucket_ms)? }
            }
            ClientMessage::ListProxy { id, since, until, bucket_ms } => {
                self.poll_proxy(proxy::FRESH_MS).await;
                let samples = if until > since { self.storage.lock().unwrap().proxy_samples(since, until, bucket_ms)? } else { vec![] };
                ServerMessage::Proxy { reply_to: id, proxy: self.proxy(samples, false).await }
            }
            ClientMessage::SetProxy { id, url, key, management_key } => {
                self.set_proxy(&url, &key, &management_key).await?;
                ServerMessage::Proxy { reply_to: id, proxy: self.proxy(vec![], true).await }
            }
            ClientMessage::Abort { id, thread_id } => {
                info!(thread = %thread_id, "abort");
                match self.agent.session(&thread_id) {
                    Some(session) => session.abort(),
                    // Nothing runs it: if it still counts as running, it doesn't any more.
                    None if self.live.settle(&thread_id).is_some() => self.push_threads(),
                    None => {}
                }
                ServerMessage::Ok { reply_to: id }
            }
            ClientMessage::CancelQueued { id, thread_id, text } => {
                if let Some(session) = self.agent.session(&thread_id) {
                    session.cancel_queued(&text);
                }
                ServerMessage::Ok { reply_to: id }
            }
            ClientMessage::ArchiveThread { id, thread_id, archived } => {
                {
                    let storage = self.storage.lock().unwrap();
                    // Archive holds threads at rest: nothing runs or has jobs going it waits
                    // on, the thread itself nor its sub-agents. Jobs they left running
                    // (servers, watchers) go with it: unloading a session kills its jobs. A
                    // message or run brings it back (`unarchive`); it doesn't go back by itself.
                    let family = if archived { storage.family(&thread_id)? } else { vec![] };
                    let busy = family.iter().any(|t| self.live.running_since(t).is_some() || self.live.jobs(t).0 > 0);
                    anyhow::ensure!(!busy, "the thread or one of its sub-agents is still working; archive it once they are done");
                    storage.set_archived(&thread_id, archived, now())?;
                    for t in family.iter().filter(|t| self.live.jobs(t).1 > 0) {
                        info!(thread = %t, "archived: stopping the jobs it left running");
                        self.unload(t);
                    }
                }
                self.push_threads();
                ServerMessage::Ok { reply_to: id }
            }
            ClientMessage::RenameThread { id, thread_id, name } => {
                let name = name.trim();
                anyhow::ensure!(!name.is_empty(), "a thread's name can't be empty");
                self.storage.lock().unwrap().set_name(&thread_id, name)?;
                info!(thread = %thread_id, "thread renamed");
                self.push_threads();
                ServerMessage::Ok { reply_to: id }
            }
            ClientMessage::SnoozeThread { id, thread_id, snoozed } => {
                self.storage.lock().unwrap().set_snoozed(&thread_id, snoozed)?;
                self.push_threads();
                ServerMessage::Ok { reply_to: id }
            }
            ClientMessage::PinThread { id, thread_id, pinned } => {
                self.storage.lock().unwrap().set_pinned(&thread_id, pinned)?;
                self.push_threads();
                ServerMessage::Ok { reply_to: id }
            }
            ClientMessage::PinMessage { id, thread_id, entry_id, pinned } => {
                self.storage.lock().unwrap().set_message_pin(&thread_id, &entry_id, pinned)?;
                self.push_threads();
                ServerMessage::Ok { reply_to: id }
            }
            ClientMessage::MarkSeen { id, thread_id, updated_at } => {
                // No push when nothing moved: the desktop re-marks on every `Threads` push
                // while a thread is open, which would otherwise ping-pong.
                if self.storage.lock().unwrap().set_seen(&thread_id, updated_at)? {
                    self.push_threads();
                }
                ServerMessage::Ok { reply_to: id }
            }
            ClientMessage::FocusThread { id, thread_id } => {
                anyhow::ensure!(self.storage.lock().unwrap().thread_cwd(&thread_id)?.is_some(), "no such thread");
                self.push(ServerMessage::Focus { thread_id });
                ServerMessage::Ok { reply_to: id }
            }
            ClientMessage::SearchThreads { id, query, limit } => {
                ServerMessage::Search { reply_to: id, hits: self.storage.lock().unwrap().search(&query, limit.clamp(1, 100))? }
            }
            ClientMessage::DeleteThread { id, thread_id } => {
                // Its `CreateThread` deletes it if it doesn't start.
                anyhow::ensure!(!self.starting(&thread_id), "the thread is still starting; try again in a moment");
                self.delete_thread(&thread_id)?;
                ServerMessage::Ok { reply_to: id }
            }
        })
    }

    /// The MCP servers changed. Sessions read them when they start, so the idle ones go:
    /// their next prompt loads them with the change. Running ones (and those with jobs,
    /// which unloading would kill) keep the servers they have until they are unloaded idle.
    fn unload_idle_sessions(&self) {
        for id in self.unload_idle(Duration::ZERO) {
            info!(thread = %id, "unloaded idle session: the MCP servers changed");
        }
    }

    /// Unload the sessions that have done nothing for `max_idle` (`Agent::unload_idle`).
    fn unload_idle(&self, max_idle: Duration) -> Vec<String> {
        let idle = self.agent.unload_idle(max_idle);
        for id in &idle {
            self.screens.clear(id);
            self.live.unloaded(id, true);
        }
        idle
    }

    /// A connection is gone: the sign-ins it started are cancelled, as nobody is left to
    /// finish them (and a browser sign-in's callback server would hold its port).
    fn disconnected(&self, conn: u64) {
        self.logins.disconnected(conn);
    }

    /// An error for a thread that takes no message (`ThreadSummary::locked`), before
    /// anything is done for one.
    fn takes_messages(&self, thread_id: &str) -> Result<()> {
        anyhow::ensure!(
            !self.storage.lock().unwrap().thread_locked(thread_id)?,
            "This thread could not be converted from pi whole: it can be read and forked, but takes no message"
        );
        Ok(())
    }

    /// Hand a prompt to the thread's model, loading its session if needed. `p` holds a
    /// `Prompt`'s fields; `edit`, the user message it replaces (`EditMessage`).
    async fn prompt(self: &Arc<Self>, thread_id: &str, mode: PromptMode, p: &FirstPrompt, edit: Option<&str>) -> Result<()> {
        self.takes_messages(thread_id)?;
        // A steer or follow-up into a run that is over (the sender's view was stale: it
        // ended, or this service restarted under it) is a new turn. The session does the
        // same when the run ends while the message is on its way.
        let idle = self.live.running_since(thread_id).is_none();
        let mode = if idle { PromptMode::Prompt } else { mode };
        // A message brings an archived thread back.
        let unarchived = self.storage.lock().unwrap().unarchive(thread_id)?;
        // The run starts now, not at the session's `RunStart`: a nix dev shell can take
        // seconds to capture, and clients should see the thread working with
        // the model it will use for all of it. A new turn is a new task, too
        // (`prompted_at`: steers, follow-ups and answers don't count). `CreateThread` does
        // the same for its first prompt, earlier.
        let before = self.storage.lock().unwrap().thread_model(thread_id)?;
        if mode == PromptMode::Prompt {
            self.storage.lock().unwrap().set_prompted(thread_id, &p.text, p.model.as_deref(), p.effort.as_deref(), now())?;
            self.live.begin_run(thread_id);
        }
        if mode == PromptMode::Prompt || unarchived {
            self.push_threads();
        }
        let sent = self.deliver(thread_id, mode, p, edit).await;
        // The run begun above never started. A message refused under a run in progress (a
        // steer, a prompt from a client that didn't know of the run) leaves that run alone.
        if sent.is_err() && idle && self.live.settle(thread_id).is_some() {
            // Nor does the thread keep the model of a message it refused (one the proxy
            // doesn't serve): the next message would be refused for it again.
            let (model, effort) = before;
            self.storage.lock().unwrap().set_model(thread_id, model.as_deref(), effort.as_deref())?;
            self.push_threads();
        }
        sent
    }

    /// `prompt` after the run's start: load the session and hand it the prompt. Returns once
    /// the model has the message, or with why it has not (the session didn't load, it
    /// refused the message: `Session::prompt`); the caller settles the run then, and the
    /// sender still has the message to give back to its user. `p.title` (a thread's first
    /// prompt) names the thread from the text alongside the run.
    async fn deliver(self: &Arc<Self>, thread_id: &str, mode: PromptMode, p: &FirstPrompt, edit: Option<&str>) -> Result<()> {
        info!(thread = %thread_id, ?mode, model = p.model.as_deref().map(tracing::field::display), "prompt");
        if let Some(tools) = &p.tools {
            self.storage.lock().unwrap().set_tools(thread_id, tools)?;
        }
        let session = self.ensure_loaded(thread_id)?;
        let prompt = Prompt {
            text: p.text.clone(),
            images: p.images.clone(),
            mode: Some(mode),
            model: p.model.clone(),
            effort: p.effort.clone(),
            tools: p.tools.clone(),
            edit: edit.map(str::to_owned),
        };
        session.prompt(prompt).await?;
        if let Some(settings) = p.title.clone() {
            let (this, thread_id, text) = (self.clone(), thread_id.to_owned(), p.text.clone());
            tokio::spawn(async move { this.name_thread(&thread_id, &settings, &text).await });
        }
        Ok(())
    }

    /// Name a thread from its first prompt. A name given by hand meanwhile stays.
    async fn name_thread(&self, thread_id: &str, settings: &TitleGen, text: &str) {
        let own = self.storage.lock().unwrap().thread_model(thread_id).ok().and_then(|(model, _)| model);
        let title = title::generate(&*self.models, settings, text, own.as_deref()).await;
        // On its own line: the storage's lock is let go before the push takes it.
        let named = title.and_then(|title| self.storage.lock().unwrap().set_title(thread_id, &title.title));
        match named {
            Ok(()) => {
                self.push(ServerMessage::Event { thread_id: thread_id.to_owned(), event: Event::Titled });
                self.push_threads();
            }
            Err(e) => {
                warn!(thread = %thread_id, "title generation failed: {e:#}");
                let event = Event::Warning { message: format!("title generation failed: {e:#}") };
                self.push(ServerMessage::Event { thread_id: thread_id.to_owned(), event });
            }
        }
    }

    /// A `CreateThread` with a prompt is still setting the thread up (`starting`).
    fn starting(&self, thread_id: &str) -> bool {
        self.starting.lock().unwrap().contains_key(thread_id)
    }

    /// A thread being created couldn't be set up or prompted: it goes again, so the creator
    /// keeps its draft instead of a thread that never started. It stays `starting` until it
    /// is gone, so no push lists it idle (`threads`): the deletion's push drops it. Returns
    /// `e` for the request's error.
    fn abandon(&self, thread_id: &str, e: anyhow::Error) -> anyhow::Error {
        if let Err(e) = self.delete_thread(thread_id) {
            error!(thread = %thread_id, "delete unstarted thread: {e:#}");
        }
        self.live.settle(thread_id);
        self.starting.lock().unwrap().remove(thread_id);
        e
    }

    /// Delete a thread and what is its alone: its loaded session, workspace and job logs.
    fn delete_thread(&self, thread_id: &str) -> Result<()> {
        // Also when it is still loading, or its session would go live in a deleted thread.
        self.unload(thread_id);
        // Its workspace has no owner without the thread.
        if self.storage.lock().unwrap().thread_workspace(thread_id)?.is_some()
            && let Err(e) = self.remove_workspace(thread_id, false)
        {
            error!(thread = %thread_id, "remove workspace: {e:#}");
        }
        self.storage.lock().unwrap().delete_thread(thread_id)?;
        info!(thread = %thread_id, "thread deleted");
        // Its `async_bash` logs go with it.
        let _ = std::fs::remove_dir_all(data_dir().join("jobs").join(thread_id));
        self.push_threads();
        Ok(())
    }

    // ---- sub-agents ----

    /// Hand a notice to a thread's model (loading the session if needed), as the user's
    /// answers to an ask are: it starts a turn when the model is idle. The caller has
    /// already counted the thread as running (`begin_run`) and pushed that; a notice that
    /// can't be delivered settles it again.
    async fn notify_thread(&self, thread_id: &str, notice: Notice, when: Notify) -> Result<()> {
        let sent = match self.ensure_loaded(thread_id) {
            Ok(session) => session.notify(notice, when).await,
            Err(e) => Err(e),
        };
        if sent.is_err() && self.live.settle(thread_id).is_some() {
            self.push_threads();
        }
        sent
    }

    /// A sub-agent's run ended: tell its parent, with the child's final reply (or, if it was
    /// spawned with `Report::Done`, only that it finished), as it was spawned to (`Notify`).
    /// A child that stopped to ask the user is reported as waiting, not finished. The
    /// parent already counts as running (see `on_out`); `notify_thread` settles it
    /// again if the notice can't be delivered.
    async fn on_child_settled(&self, thread_id: &str, parent: &str) {
        let (name, asks, reply, (report, notify)) = {
            let storage = self.storage.lock().unwrap();
            let summary = storage.list_threads().ok().and_then(|ts| ts.into_iter().find(|t| t.id == thread_id));
            let name = summary.as_ref().and_then(|t| t.name.clone());
            let asks: Vec<String> =
                summary.map(|t| t.asks.iter().flat_map(|a| a.questions.iter().map(|q| q.text.clone())).collect()).unwrap_or_default();
            (name, asks, storage.last_reply(thread_id).ok().flatten(), storage.report(thread_id).unwrap_or_default())
        };
        let name = name.unwrap_or_else(|| "untitled".into());
        let who = format!("Sub-agent \"{name}\" (thread {thread_id})");
        let content = if !asks.is_empty() {
            format!("{who} is waiting on the user's answer to:\n{}", asks.iter().map(|q| format!("- {q}")).collect::<Vec<_>>().join("\n"))
        } else {
            match &reply {
                Some(_) if report == Report::Done => format!("{who} finished. Use `read` for its reply."),
                Some(r) => format!("{who} finished. Its reply:\n\n{}", truncate(r, NOTICE_CHARS)),
                // No final reply after a run: it was aborted or failed.
                None => format!("{who} stopped without a reply (aborted or failed)."),
            }
        };
        let notice = Notice::Child {
            text: content,
            thread_id: thread_id.to_owned(),
            name,
            asking: !asks.is_empty(),
            failed: asks.is_empty() && reply.is_none(),
        };
        info!(thread = %parent, child = %thread_id, "notifying parent of sub-agent");
        if let Err(e) = self.notify_thread(parent, notice, notify).await {
            error!(thread = %parent, child = %thread_id, "notify parent: {e:#}");
        }
    }

    /// `<data dir>/workspaces/<name>`: the directory is named like its branch.
    fn workspace_dir(&self, thread_id: &str) -> PathBuf {
        data_dir().join("workspaces").join(dirigent_protocol::workspace_name(thread_id))
    }

    /// The repo a thread's project lives in and which system drives it.
    fn thread_repo(&self, thread_id: &str) -> Result<(PathBuf, Vcs)> {
        let repo = self.storage.lock().unwrap().thread_project_path(thread_id)?.ok_or_else(|| anyhow!("no such thread"))?;
        let repo = PathBuf::from(repo);
        let vcs = vcs::detect(&repo).ok_or_else(|| anyhow!("{} is not a git or jj repository", repo.display()))?;
        Ok((repo, vcs))
    }

    /// Give the thread its own workspace (or recreate one whose directory went missing) and
    /// point it there. The session is unloaded so the next prompt starts in the new cwd; a
    /// thread with history also gets a notice entry so the model knows where it is now.
    /// A thread that already worked is refused while it runs, and when it would leave its
    /// uncommitted edits behind in the main checkout. A `new` one (being created) starts
    /// from HEAD on purpose and may already count as running: its first prompt waits on this.
    fn move_to_workspace(&self, thread_id: &str, new: bool, base: Option<&str>) -> Result<()> {
        anyhow::ensure!(new || self.live.running_since(thread_id).is_none(), "the thread is running");
        let (repo, vcs) = self.thread_repo(thread_id)?;
        let recreating = self.storage.lock().unwrap().thread_workspace(thread_id)?.is_some();
        anyhow::ensure!(recreating || new || !vcs::dirty(vcs, &repo)?, "the working copy has uncommitted changes; commit them first");
        let dest = self.workspace_dir(thread_id);
        // Recreating one the vcs forgot (`jj workspace forget` keeps the files): its leftover
        // directory makes way. The user picked `Recreate` over `Back to project`.
        if recreating && dest.exists() && !vcs::is_workspace(&dest) {
            std::fs::remove_dir_all(&dest)?;
        }
        vcs::add_workspace(vcs, &repo, &dest, thread_id, base)?;
        info!(thread = %thread_id, path = %dest.display(), base = base.map(tracing::field::display), "workspace added");
        let dest = dest.to_string_lossy().into_owned();
        let kind = match vcs {
            Vcs::Git => "git worktree",
            Vcs::Jj => "jj workspace",
        };
        self.relocate(
            thread_id,
            &dest,
            Some(&dest),
            &format!("Working directory is now {dest}, an isolated {kind} of {}; use paths under it from now on.", repo.display()),
        )
    }

    /// Take the thread back to its project directory, removing the workspace unless `detach`.
    /// Not under a running agent: `RemoveWorkspace` refuses that; `delete_thread` may, as
    /// the thread goes anyway.
    fn remove_workspace(&self, thread_id: &str, detach: bool) -> Result<()> {
        let ws = self.storage.lock().unwrap().thread_workspace(thread_id)?.ok_or_else(|| anyhow!("the thread has no workspace"))?;
        let repo = self.storage.lock().unwrap().thread_project_path(thread_id)?.ok_or_else(|| anyhow!("no such thread"))?;
        if !detach {
            match vcs::detect(Path::new(&repo)) {
                Some(vcs) => vcs::remove_workspace(vcs, Path::new(&repo), Path::new(&ws), thread_id)?,
                // The repo itself is gone; all that is left to clean is the directory.
                None if Path::new(&ws).exists() => std::fs::remove_dir_all(&ws)?,
                None => {}
            }
        }
        self.relocate(thread_id, &repo, None, &format!("Working directory is now {repo} again; use paths under it from now on."))?;
        info!(thread = %thread_id, path = %ws, detach, "workspace removed");
        Ok(())
    }

    /// Point the thread at `cwd`, drop its live session, and leave `notice` for the model
    /// if it has history (a fresh thread learns its cwd from the system prompt).
    fn relocate(&self, thread_id: &str, cwd: &str, workspace: Option<&str>, notice: &str) -> Result<()> {
        let storage = self.storage.lock().unwrap();
        storage.set_cwd(thread_id, cwd, workspace)?;
        if let Some(parent) = storage.last_entry_id(thread_id)? {
            let entry = Entry {
                seq: 0,
                id: uuid::Uuid::new_v4().simple().to_string()[..16].to_owned(),
                parent_id: Some(parent),
                at: now(),
                body: Body::Notice(Notice::Info { text: notice.to_owned() }),
            };
            let entry = storage.append_entry(thread_id, entry, now())?;
            self.push(ServerMessage::Entry { thread_id: thread_id.to_owned(), entry });
        }
        drop(storage);
        // Its workspace is fresh, or it has none: nothing is missing until a sweep says so.
        self.missing.lock().unwrap().remove(thread_id);
        // Its session works at the old cwd. The run begun for a new thread's first prompt goes
        // on: it has no session yet (`unload`).
        self.unload(thread_id);
        Ok(())
    }

    /// The thread's live session, loaded from its stored entries if it has none.
    fn ensure_loaded(&self, thread_id: &str) -> Result<Arc<Session>> {
        if let Some(session) = self.agent.session(thread_id) {
            return Ok(session);
        }
        let thread = {
            let storage = self.storage.lock().unwrap();
            let cwd = storage.thread_cwd(thread_id)?.ok_or_else(|| anyhow!("no such thread"))?;
            // A jj workspace the user rebased from elsewhere is made current before the
            // agent runs in it.
            if storage.thread_workspace(thread_id)?.is_some() {
                vcs::update_stale(Path::new(&cwd));
            }
            let (model, effort) = storage.thread_model(thread_id)?;
            Thread {
                id: thread_id.to_owned(),
                cwd,
                entries: storage.get_thread(thread_id)?.unwrap_or_default(),
                model,
                effort,
                tools: storage.thread_tools(thread_id)?,
            }
        };
        // A new session has done nothing on the computer yet.
        self.screens.clear(thread_id);
        info!(thread = %thread_id, "session loaded");
        Ok(self.agent.load(thread))
    }

    /// Keep and fan out what comes of a thread's session.
    fn on_out(self: &Arc<Self>, thread_id: &str, out: Out) {
        let thread_id = thread_id.to_owned();
        match out {
            Out::Entry(entry) => {
                match self.storage.lock().unwrap().append_entry(&thread_id, entry, now()) {
                    Ok(entry) => self.push(ServerMessage::Entry { thread_id, entry }),
                    Err(e) => error!(thread = %thread_id, "append entry: {e:#}"),
                }
                self.push_threads();
            }
            Out::Event(event) => {
                // Changes the thread list shows (`running_since`, `compacting_since`).
                let lifecycle = matches!(event, Event::RunStart | Event::RunEnd | Event::CompactionStart | Event::CompactionEnd);
                let mut parent = None;
                match &event {
                    Event::RunStart => {
                        info!(thread = %thread_id, "run started");
                        self.live.begin_run(&thread_id);
                        // Whatever woke it (a message does that earlier, a job's notice only
                        // here), a working thread isn't archived. The lifecycle push below shows it.
                        if let Err(e) = self.storage.lock().unwrap().unarchive(&thread_id) {
                            error!(thread = %thread_id, "unarchive: {e:#}");
                        }
                    }
                    Event::RunEnd => {
                        self.screens.clear(&thread_id);
                        let ms = self.live.settle(&thread_id);
                        info!(thread = %thread_id, secs = ms.map(|ms| ms / 1000), "run ended");
                        if let Some(ms) = ms
                            && let Err(e) = self.storage.lock().unwrap().set_last_run(&thread_id, ms)
                        {
                            error!(thread = %thread_id, "set last run: {e:#}");
                        }
                        // A sub-agent's run ended: its parent is about to be woken
                        // (`on_child_settled`), so it counts as running from now, in the same
                        // `Threads` push that settles the child; otherwise clients would see it
                        // finished for a moment and move it to the inbox and back.
                        parent = self.storage.lock().unwrap().thread_parent(&thread_id).ok().flatten();
                    }
                    Event::CompactionStart => self.live.set_compacting(&thread_id, true),
                    Event::CompactionEnd => self.live.set_compacting(&thread_id, false),
                    Event::Warning { message } => warn!(thread = %thread_id, "{message}"),
                    _ => {}
                }
                if let Some(parent) = parent {
                    // The notice is a message: an archived parent comes back for it.
                    if let Err(e) = self.storage.lock().unwrap().unarchive(&parent) {
                        error!(thread = %parent, "unarchive: {e:#}");
                    }
                    self.live.begin_run(&parent);
                    let (this, id) = (self.clone(), thread_id.clone());
                    tokio::spawn(async move { this.on_child_settled(&id, &parent).await });
                }
                self.push(ServerMessage::Event { thread_id, event });
                if lifecycle {
                    self.push_threads();
                }
            }
            Out::Ask(ask) => {
                info!(thread = %thread_id, "asking the user");
                if let Err(e) = self.storage.lock().unwrap().add_ask(&thread_id, &ask, now()) {
                    error!(thread = %thread_id, "add ask: {e:#}");
                }
                self.push_threads();
            }
            Out::Jobs { running, free } => {
                self.live.set_jobs(&thread_id, running, free);
                self.push_threads();
            }
            Out::NixShell(active) => {
                self.live.set_nix_shell(&thread_id, active);
                self.push_threads();
            }
            Out::Screen(screen) => {
                let (frame, new) = self.screens.update(&thread_id, screen.image);
                let event = Event::Screen { frame, action: screen.action, cursor: screen.cursor, desktop: screen.desktop };
                self.push(ServerMessage::Event { thread_id, event });
                // The thread list says which threads have a screen to show.
                if new {
                    self.push_threads();
                }
            }
        }
    }
}

/// The service as its agent sees it (`dirigent_agent::Host`).
struct AgentHost(std::sync::Weak<Service>);

impl dirigent_agent::Host for AgentHost {
    fn emit(&self, thread_id: &str, out: Out) {
        if let Some(svc) = self.0.upgrade() {
            svc.on_out(thread_id, out);
        }
    }

    fn put_blob(&self, mime: &str, bytes: &[u8]) -> String {
        let Some(svc) = self.0.upgrade() else { return String::new() };
        let kept = svc.storage.lock().unwrap().put_blob(mime, bytes);
        kept.unwrap_or_else(|e| {
            error!("keep a blob: {e:#}");
            String::new()
        })
    }

    fn blob(&self, id: &str) -> Option<(String, Vec<u8>)> {
        self.0.upgrade()?.storage.lock().unwrap().blob(id).ok().flatten()
    }

    fn children_busy(&self, thread_id: &str) -> bool {
        let Some(svc) = self.0.upgrade() else { return false };
        // Not known: wait then.
        svc.threads()
            .map_or(true, |threads| threads.iter().any(|t| t.parent_id.as_deref() == Some(thread_id) && (t.running() || t.jobs > 0)))
    }

    fn tools(&self, thread_id: &str) -> Vec<Arc<dyn dirigent_agent::tool::Tool>> {
        vec![Arc::new(dirigent::Dirigent::new(self.0.clone(), thread_id))]
    }
}

/// Unix millis.
pub(crate) fn now() -> i64 {
    std::time::UNIX_EPOCH.elapsed().map(|d| d.as_millis() as i64).unwrap_or(0)
}

/// A new thread's id: the client's pick (a lowercase, hyphenated uuid), so it can open the
/// thread before the reply, else a fresh one.
fn new_thread_id(pick: Option<String>) -> Result<String> {
    match pick {
        Some(t) => {
            anyhow::ensure!(uuid::Uuid::parse_str(&t).is_ok_and(|u| u.to_string() == t), "not a lowercase uuid: {t}");
            Ok(t)
        }
        None => Ok(uuid::Uuid::now_v7().to_string()),
    }
}

fn truncate(s: &str, max: usize) -> String {
    if s.len() <= max {
        return s.to_owned();
    }
    let cut = (0..=max).rev().find(|i| s.is_char_boundary(*i)).unwrap_or(0);
    format!("{}…", &s[..cut])
}

/// Directories given on the command line (`--data-dir`, `--config-dir`); see `init_dirs`.
#[derive(Debug, Default, PartialEq)]
struct Dirs {
    data: Option<PathBuf>,
    config: Option<PathBuf>,
}

static DIRS: std::sync::OnceLock<Dirs> = std::sync::OnceLock::new();

pub const USAGE: &str = "options: --data-dir DIR (dbs, workspaces, jobs, logs; default $XDG_DATA_HOME/dirigent, \
                         %LOCALAPPDATA%\\dirigent on Windows) --config-dir DIR (agent.toml, AGENTS.md, skills, hosts.toml, config.toml, themes; \
                         default $XDG_CONFIG_HOME/dirigent, %APPDATA%\\dirigent on Windows); a nightly build's \
                         are dirigent-nightly";

/// The running service's agent and the runtime it lives on, for `shutdown`.
static RUNNING: Mutex<Option<(Arc<Agent>, tokio::runtime::Handle)>> = Mutex::new(None);

/// The process is about to exit (the desktop quits, the service was told to stop): what the
/// agent's sessions started goes with it, where it would live on by itself: their commands
/// (killed at once), MCP servers and screens (`Agent::shutdown`). Blocks, for
/// `SHUTDOWN_WAIT` at the most: not for a thread of the service's runtime to call.
pub fn shutdown() {
    let Some((agent, runtime)) = RUNNING.lock().unwrap().take() else { return };
    let (done, wait) = std::sync::mpsc::channel();
    runtime.spawn(async move {
        agent.shutdown().await;
        let _ = done.send(());
    });
    match wait.recv_timeout(SHUTDOWN_WAIT) {
        Ok(()) => info!("the agent shut down"),
        Err(e) => warn!("the agent did not shut down in time: {e}"),
    }
}

/// This instance's storage, `<data dir>/service.db`. Not one that is pi's still: the
/// migration (`migration.rs`) comes first, and is the service's or the desktop's to make.
pub(crate) fn open_storage(data: &Path) -> Result<Storage> {
    let db = data.join("service.db");
    anyhow::ensure!(
        !storage::from_pi(&db)?,
        "{} is from before Dirigent had its own agent: start Dirigent once to migrate it",
        db.display()
    );
    Storage::open(&db)
}

/// Take `--data-dir DIR` / `--config-dir DIR` from the command line, every binary's only
/// options, and return the operands (the desktop takes a folder to open, `dirigent` and the
/// service a command).
/// Call it first thing in `main`, before anything asks for a directory. A test instance
/// points both elsewhere so it never touches the real dbs or config (and, unlike overriding
/// `XDG_*`, nothing it spawns sees a changed environment).
pub fn init_dirs() -> Result<Vec<String>> {
    let (dirs, operands) = parse_dirs(std::env::args().skip(1))?;
    let _ = DIRS.set(dirs);
    Ok(operands)
}

/// The options `init_dirs` took, as arguments again: what a restarted instance gets.
pub fn dir_args() -> Vec<String> {
    let Some(dirs) = DIRS.get() else { return vec![] };
    let flag = |name: &str, dir: &Option<PathBuf>| dir.iter().flat_map(|d| [name.to_owned(), d.display().to_string()]).collect::<Vec<_>>();
    [flag("--data-dir", &dirs.data), flag("--config-dir", &dirs.config)].concat()
}

fn parse_dirs(args: impl Iterator<Item = String>) -> Result<(Dirs, Vec<String>)> {
    let mut dirs = Dirs::default();
    let mut operands = vec![];
    let mut args = args.peekable();
    while let Some(arg) = args.next() {
        // Options end at the first operand: what follows a command (`key add …`) is its own.
        if !arg.starts_with("--") {
            operands.push(arg);
            operands.extend(args);
            break;
        }
        let (flag, value) = match arg.split_once('=') {
            Some((f, v)) => (f.to_owned(), Some(v.to_owned())),
            None => (arg, None),
        };
        let slot = match flag.as_str() {
            "--data-dir" => &mut dirs.data,
            "--config-dir" => &mut dirs.config,
            _ => return Err(anyhow!("unknown argument {flag}\n{USAGE}")),
        };
        let value = value.or_else(|| args.next()).ok_or_else(|| anyhow!("{flag} needs a directory\n{USAGE}"))?;
        // Absolute, so it means the same whatever the cwd of who is told it.
        *slot = Some(std::path::absolute(&value)?);
    }
    Ok((dirs, operands))
}

/// Only one of `--data-dir` and `--config-dir` was given: the other dir is the real
/// install's.
pub(crate) fn half_elsewhere() -> bool {
    DIRS.get().is_some_and(|dirs| dirs.data.is_some() != dirs.config.is_some())
}

/// `--config-dir`, else the platform's (`platform::config_dir`): the desktop's
/// `config.toml`, the machine's `hosts.toml` and the agent's files (`agent.rs`).
pub fn config_dir() -> PathBuf {
    DIRS.get().and_then(|d| d.config.clone()).unwrap_or_else(platform::config_dir)
}

/// `--data-dir`, else the platform's (`platform::data_dir`): the service's and the
/// desktop's dbs, workspaces, job logs, attached images (`attachments/`) and logs.
pub fn data_dir() -> PathBuf {
    DIRS.get().and_then(|d| d.data.clone()).unwrap_or_else(platform::data_dir)
}

/// Where the service listens openly: `DIRIGENT_BIND` (comma-separated IPs) if set, else
/// loopback plus this machine's tailscale addresses, so a host is reachable from the
/// tailnet (and over ssh, `tunnel.rs`) and nowhere else. Never a wildcard: these take
/// anyone. Reaching it from elsewhere is the public listener's job (`auth.rs`).
fn bind_addrs(port: u16) -> Vec<SocketAddr> {
    let ips: Vec<IpAddr> = match std::env::var("DIRIGENT_BIND") {
        Ok(list) => list.split(',').filter_map(|s| s.trim().parse().ok()).collect(),
        Err(_) => std::iter::once(IpAddr::V4(Ipv4Addr::LOCALHOST)).chain(tailscale_ips()).collect(),
    };
    ips.into_iter().map(|ip| SocketAddr::new(ip, port)).collect()
}

/// The tailnet addresses `tailscale ip` reports; none when tailscale isn't installed or up.
fn tailscale_ips() -> Vec<IpAddr> {
    platform::command("tailscale")
        .arg("ip")
        .output()
        .ok()
        .filter(|out| out.status.success())
        .map(|out| String::from_utf8_lossy(&out.stdout).lines().filter_map(|l| l.trim().parse().ok()).collect())
        .unwrap_or_default()
}

/// Run the service until a listener fails. An `AddrInUse` error means another service
/// already owns this host.
pub async fn run(port: u16) -> Result<()> {
    let dirs = AgentDirs { config: config_dir(), data: data_dir() };
    // Bound before anything is set up: a service that can't listen (another one owns this
    // host; the desktop retries its embedded one) touches no file and logs nothing more.
    let mut addrs: Vec<(SocketAddr, Option<Arc<auth::Gate>>)> = bind_addrs(port).into_iter().map(|a| (a, None)).collect();
    if let Some(public) = auth::load()?.public {
        info!(url = public.url.as_deref().unwrap_or("?"), "public listener on {}", public.listen);
        let gate = auth::Gate::new(&public);
        if let Err(why) = gate.audience() {
            warn!("public listener: API keys only, no forge tokens: {why}");
        }
        addrs.push((public.listen, Some(Arc::new(gate))));
    }
    let listeners = server::bind(addrs).await?;
    // The first start after pi, where no desktop has made it in its window. Off the async
    // workers: it copies and converts a whole db.
    tokio::task::spawn_blocking(migration::headless).await??;
    agent::prepare(&dirs);
    let storage = open_storage(&dirs.data)?;
    let service_id = storage.service_id()?;
    let host_name = platform::host_name();
    info!(host = %host_name, data = %data_dir().display(), version = dirigent_protocol::release::VERSION, "service started");
    let models = Arc::new(Proxy::new(dirs.clone()));
    let svc = Arc::new_cyclic(|this| Service {
        storage: Mutex::new(storage),
        agent: Agent::new(dirs, Arc::new(AgentHost(this.clone())), models.clone()),
        models,
        live: Live::default(),
        missing: Mutex::new(Default::default()),
        starting: Mutex::new(Default::default()),
        logins: Default::default(),
        proxy: Default::default(),
        screens: Default::default(),
        push: broadcast::channel(1024).0,
        host_name,
        service_id,
    });
    *RUNNING.lock().unwrap() = Some((svc.agent.clone(), tokio::runtime::Handle::current()));

    tokio::spawn({
        let svc = svc.clone();
        async move {
            let mut n = 0u64;
            // The first sweep at once: `workspace_missing` knows nothing before it.
            loop {
                svc.sweep_workspaces();
                n += 1;
                if n.is_multiple_of(6) {
                    for id in svc.unload_idle(IDLE_UNLOAD) {
                        info!(thread = %id, "unloaded idle session");
                    }
                }
                tokio::time::sleep(Duration::from_secs(10)).await;
            }
        }
    });
    // The proxy's limits, sampled for their history (a host without a proxy has nothing to ask).
    tokio::spawn({
        let svc = svc.clone();
        async move {
            loop {
                svc.poll_proxy(0).await;
                tokio::time::sleep(Duration::from_millis(proxy::SAMPLE_MS as u64)).await;
            }
        }
    });

    server::serve(svc, listeners).await
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(args: &[&str]) -> Result<Dirs> {
        let (dirs, operands) = parse_dirs(args.iter().map(|s| s.to_string()))?;
        assert!(operands.is_empty());
        Ok(dirs)
    }

    #[test]
    fn dir_flags() {
        let cwd = std::env::current_dir().unwrap();
        assert_eq!(parse(&[]).unwrap(), Dirs::default());
        let abs = std::env::temp_dir().join("d");
        let d = parse(&["--data-dir", abs.to_str().unwrap(), "--config-dir=rel/c"]).unwrap();
        assert_eq!(d.data, Some(abs));
        assert_eq!(d.config, Some(cwd.join("rel/c")));
        assert!(parse(&["--data-dir"]).is_err());
        assert!(parse(&["--port", "1"]).is_err());
        // The desktop's folder to open; a command's own options are left to it.
        let (_, operands) = parse_dirs(["--data-dir=d", "/some/dir"].map(String::from).into_iter()).unwrap();
        assert_eq!(operands, ["/some/dir"]);
        let (_, operands) = parse_dirs(["key", "add", "--data-dir"].map(String::from).into_iter()).unwrap();
        assert_eq!(operands, ["key", "add", "--data-dir"]);
    }
}
