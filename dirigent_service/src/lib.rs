//! dirigent_service: owns this host's threads (sqlite), drives the pi runtime,
//! and serves them to desktops over WebSocket. See plan.md §3.
//!
//! A library so the desktop can run the local service in-process (`run`); the
//! `dirigent_service` binary is a thin wrapper for remote hosts.

mod blobs;
mod git;
pub mod hosts;
mod runtime;
mod server;
mod storage;
mod vcs;

use anyhow::{Result, anyhow};
use dirigent_protocol::{ClientMessage, HostInfo, PromptMode, Report, ServerMessage, ThreadSummary, Vcs};
use runtime::{Runtime, RuntimeMessage, now};
use serde_json::{Value, json};
use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use storage::Storage;
use tokio::sync::broadcast;

const IDLE_UNLOAD: Duration = Duration::from_secs(30 * 60);
pub const DEFAULT_PORT: u16 = 7777;
/// Sub-agents can't nest deeper than this (a root thread is depth 0).
const MAX_DEPTH: usize = 3;
/// How much of a sub-agent's reply a parent's notice carries.
const NOTICE_CHARS: usize = 6000;

/// The local service's port: `DIRIGENT_PORT` or 7777. The desktop uses it both to reach and
/// to embed the local service, so a test instance with its own port never touches the real one.
pub fn port() -> u16 {
    std::env::var("DIRIGENT_PORT").ok().and_then(|p| p.parse().ok()).unwrap_or(DEFAULT_PORT)
}

pub struct Service {
    storage: Mutex<Storage>,
    runtime: Arc<Runtime>,
    /// Threads whose workspace was gone (or no longer known to the vcs) at the last
    /// `sweep_workspaces`: what `workspace_missing` reports between sweeps.
    missing: Mutex<std::collections::HashSet<String>>,
    /// Pushes to every connected client.
    pub push: broadcast::Sender<ServerMessage>,
    pub host_name: String,
}

impl Service {
    fn push(&self, msg: ServerMessage) {
        let _ = self.push.send(msg);
    }

    fn threads(&self) -> Result<Vec<ThreadSummary>> {
        let mut threads = self.storage.lock().unwrap().list_threads()?;
        let missing = self.missing.lock().unwrap();
        for t in &mut threads {
            t.running_since = self.runtime.running_since(&t.id);
            t.jobs = self.runtime.jobs(&t.id);
            // A workspace removed outside dirigent: the thread can't run until it is dealt with.
            t.workspace_missing = missing.contains(&t.id);
        }
        Ok(threads)
    }

    fn push_threads(&self) {
        match self.threads() {
            Ok(threads) => self.push(ServerMessage::Threads { reply_to: None, threads }),
            Err(e) => eprintln!("[service] list threads: {e:#}"),
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
            Err(e) => eprintln!("[service] list projects: {e:#}"),
        }
    }

    /// Handle one client request; the reply is sent back on the requesting connection.
    pub async fn handle(self: &Arc<Self>, req: ClientMessage) -> ServerMessage {
        let id = match &req {
            ClientMessage::ListProjects { id }
            | ClientMessage::AddProject { id, .. }
            | ClientMessage::RemoveProject { id, .. }
            | ClientMessage::ListThreads { id }
            | ClientMessage::GetThread { id, .. }
            | ClientMessage::CreateThread { id, .. }
            | ClientMessage::MoveToWorkspace { id, .. }
            | ClientMessage::RemoveWorkspace { id, .. }
            | ClientMessage::VcsStatus { id, .. }
            | ClientMessage::Prompt { id, .. }
            | ClientMessage::Answer { id, .. }
            | ClientMessage::Abort { id, .. }
            | ClientMessage::CancelQueued { id, .. }
            | ClientMessage::ArchiveThread { id, .. }
            | ClientMessage::SnoozeThread { id, .. }
            | ClientMessage::FocusThread { id, .. }
            | ClientMessage::SearchThreads { id, .. }
            | ClientMessage::DeleteThread { id, .. }
            | ClientMessage::ListModels { id }
            | ClientMessage::ListHosts { id }
            | ClientMessage::GenerateTitle { id, .. }
            | ClientMessage::GetBlob { id, .. }
            | ClientMessage::ListUsage { id, .. } => *id,
        };
        match self.try_handle(req).await {
            Ok(reply) => reply,
            Err(e) => ServerMessage::Error { reply_to: Some(id), message: format!("{e:#}") },
        }
    }

    async fn try_handle(self: &Arc<Self>, req: ClientMessage) -> Result<ServerMessage> {
        Ok(match req {
            ClientMessage::ListProjects { id } => {
                ServerMessage::Projects { reply_to: Some(id), projects: self.storage.lock().unwrap().list_projects()? }
            }
            ClientMessage::AddProject { id, path } => {
                let path = std::path::PathBuf::from(path.trim());
                anyhow::ensure!(path.is_absolute(), "path must be absolute");
                anyhow::ensure!(path.is_dir(), "not a directory on this host: {}", path.display());
                let origin = git::origin(&path);
                let project_id = uuid::Uuid::now_v7().to_string();
                let project = self.storage.lock().unwrap().add_project(&project_id, &path.to_string_lossy(), origin.as_deref(), now())?;
                self.push_projects();
                ServerMessage::ProjectAdded { reply_to: id, project }
            }
            ClientMessage::RemoveProject { id, project_id } => {
                self.storage.lock().unwrap().remove_project(&project_id, now())?;
                self.push_projects();
                self.push_threads();
                ServerMessage::Ok { reply_to: id }
            }
            ClientMessage::ListThreads { id } => ServerMessage::Threads { reply_to: Some(id), threads: self.threads()? },
            ClientMessage::GetThread { id, thread_id } => {
                self.sweep_workspaces();
                let (header, entries) = self.storage.lock().unwrap().get_thread(&thread_id)?.ok_or_else(|| anyhow!("no such thread"))?;
                ServerMessage::Thread { reply_to: id, thread_id, header, entries }
            }
            ClientMessage::CreateThread { id, project_id, workspace, base, parent, name, report, notify } => {
                let thread_id = uuid::Uuid::now_v7().to_string();
                let mut thread = {
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
                    thread
                };
                if workspace {
                    // A thread that can't get its workspace is never created: the draft stays.
                    if let Err(e) = self.move_to_workspace(&thread_id, false, base.as_deref()) {
                        let _ = self.storage.lock().unwrap().delete_thread(&thread_id);
                        return Err(e);
                    }
                    let ws = self.workspace_dir(&thread_id).to_string_lossy().into_owned();
                    thread.cwd = ws.clone();
                    thread.workspace = Some(ws);
                }
                // No push: the creator's `Prompt` follows at once and pushes the thread already running.
                ServerMessage::ThreadCreated { reply_to: id, thread }
            }
            ClientMessage::MoveToWorkspace { id, thread_id, base } => {
                self.move_to_workspace(&thread_id, true, base.as_deref())?;
                self.push_threads();
                ServerMessage::Ok { reply_to: id }
            }
            ClientMessage::RemoveWorkspace { id, thread_id, detach } => {
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
            ClientMessage::Prompt { id, thread_id, text, mode, model, thinking_level, images, title, tools } => {
                // The run starts now, not at pi's `agent_start`: loading the session (and a nix
                // dev shell) can take seconds, and clients should see the thread working with
                // the model it will use for all of it. A new turn is a new task, too
                // (`prompted_at`: steers, follow-ups and answers don't count).
                if mode == PromptMode::Prompt {
                    self.storage.lock().unwrap().set_prompted(&thread_id, model.as_deref(), thinking_level.as_deref(), now())?;
                    self.runtime.begin_run(&thread_id);
                    self.push_threads();
                }
                if let Some(tools) = &tools {
                    self.storage.lock().unwrap().set_tools(&thread_id, tools)?;
                }
                let sent = match self.ensure_loaded(&thread_id, model.as_deref(), thinking_level.as_deref()).await {
                    Ok(()) => self.runtime.prompt(
                        &thread_id,
                        &text,
                        mode,
                        model.as_deref(),
                        thinking_level.as_deref(),
                        &images,
                        title.as_ref(),
                        tools.as_deref(),
                    ),
                    Err(e) => Err(e),
                };
                if sent.is_err() && self.runtime.settle(&thread_id).is_some() {
                    self.push_threads();
                }
                sent?;
                ServerMessage::Ok { reply_to: id }
            }
            ClientMessage::Answer { id, thread_id, ask_id, answers } => {
                // Taken before anything else so a second client answering the same ask is refused.
                let ask = self.storage.lock().unwrap().take_ask(&thread_id, &ask_id)?.ok_or_else(|| anyhow!("already answered"))?;
                // The answer starts a turn if the model is idle (and is queued if it isn't).
                self.runtime.begin_run(&thread_id);
                self.push_threads();
                let sent = match self.ensure_loaded(&thread_id, None, None).await {
                    Ok(()) => self.runtime.answer(&thread_id, &ask, answers.as_deref()),
                    Err(e) => Err(e),
                };
                if sent.is_err() && self.runtime.settle(&thread_id).is_some() {
                    self.push_threads();
                }
                sent?;
                ServerMessage::Ok { reply_to: id }
            }
            ClientMessage::ListModels { id } => ServerMessage::Models { reply_to: id, models: self.runtime.list_models().await? },
            // Read on every request, so edits to the file apply at once.
            ClientMessage::ListHosts { id } => {
                ServerMessage::Hosts { reply_to: id, hosts: hosts::load()?.into_iter().map(|(name, url)| HostInfo { name, url }).collect() }
            }
            ClientMessage::GenerateTitle { id, title, text } => {
                let r = self.runtime.generate_title(&title, &text).await?;
                ServerMessage::Title {
                    reply_to: id,
                    title: r.title,
                    model: r.model,
                    input: r.input,
                    output: r.output,
                    cost: r.cost,
                    duration_ms: r.duration_ms,
                }
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
            ClientMessage::ListUsage { id, since, until, bucket_ms } => {
                ServerMessage::Usage { reply_to: id, buckets: self.storage.lock().unwrap().usage(since, until, bucket_ms)? }
            }
            ClientMessage::Abort { id, thread_id } => {
                self.runtime.abort(&thread_id)?;
                ServerMessage::Ok { reply_to: id }
            }
            ClientMessage::CancelQueued { id, thread_id, text } => {
                self.runtime.cancel_queued(&thread_id, &text)?;
                ServerMessage::Ok { reply_to: id }
            }
            ClientMessage::ArchiveThread { id, thread_id, archived } => {
                self.storage.lock().unwrap().set_archived(&thread_id, archived, now())?;
                self.push_threads();
                ServerMessage::Ok { reply_to: id }
            }
            ClientMessage::SnoozeThread { id, thread_id, snoozed } => {
                self.storage.lock().unwrap().set_snoozed(&thread_id, snoozed)?;
                self.push_threads();
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
                if self.runtime.is_loaded(&thread_id) {
                    let _ = self.runtime.unload(&thread_id);
                }
                // Its workspace has no owner without the thread.
                if self.storage.lock().unwrap().thread_workspace(&thread_id)?.is_some()
                    && let Err(e) = self.remove_workspace(&thread_id, false)
                {
                    eprintln!("[service] remove workspace of {thread_id}: {e:#}");
                }
                self.storage.lock().unwrap().delete_thread(&thread_id)?;
                // Its `async_bash` logs go with it.
                let _ = std::fs::remove_dir_all(data_dir().join("jobs").join(&thread_id));
                self.push_threads();
                ServerMessage::Ok { reply_to: id }
            }
        })
    }

    // ---- sub-agents ----

    /// Hand a notice to a thread's model (loading the session if needed), as the user's
    /// answers to an ask are: it starts a turn when the model is idle. The caller has
    /// already counted the thread as running (`begin_run`) and pushed that; a notice that
    /// can't be delivered settles it again.
    async fn notify_thread(&self, thread_id: &str, custom_type: &str, content: &str, details: Value, notify: &str) -> Result<()> {
        let sent = match self.ensure_loaded(thread_id, None, None).await {
            Ok(()) => self.runtime.notify(thread_id, custom_type, content, &details, notify),
            Err(e) => Err(e),
        };
        if sent.is_err() && self.runtime.settle(thread_id).is_some() {
            self.push_threads();
        }
        sent
    }

    /// A sub-agent's run ended: tell its parent, with the child's final reply (or, if it was
    /// spawned with `Report::Done`, only that it finished), as it was spawned to (`Notify`).
    /// A child that stopped to ask the user is reported as waiting, not finished. The
    /// parent already counts as running (see `on_runtime`); `notify_thread` settles it
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
        let details = json!({
            "thread_id": thread_id, "name": name, "reply": reply,
            "error": asks.is_empty() && reply.is_none(), "asking": !asks.is_empty(),
        });
        if let Err(e) = self.notify_thread(parent, "dirigent:child", &content, details, notify.as_str()).await {
            eprintln!("[service] notify parent {parent} of {thread_id}: {e:#}");
        }
    }

    /// `<data dir>/workspaces/<name>`: the directory is named like its branch.
    fn workspace_dir(&self, thread_id: &str) -> PathBuf {
        data_dir().join("workspaces").join(vcs::workspace_name(thread_id))
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
    /// `must_be_clean`: moving a thread that already worked in the main checkout would
    /// leave its uncommitted edits behind, so that is refused; a new thread starts from
    /// HEAD on purpose.
    fn move_to_workspace(&self, thread_id: &str, must_be_clean: bool, base: Option<&str>) -> Result<()> {
        anyhow::ensure!(self.runtime.running_since(thread_id).is_none(), "the thread is running");
        let (repo, vcs) = self.thread_repo(thread_id)?;
        let recreating = self.storage.lock().unwrap().thread_workspace(thread_id)?.is_some();
        anyhow::ensure!(
            recreating || !must_be_clean || !vcs::dirty(vcs, &repo)?,
            "the working copy has uncommitted changes; commit them first"
        );
        let dest = self.workspace_dir(thread_id);
        // Recreating one the vcs forgot (`jj workspace forget` keeps the files): its leftover
        // directory makes way. The user picked `Recreate` over `Back to project`.
        if recreating && dest.exists() && !vcs::is_workspace(&dest) {
            std::fs::remove_dir_all(&dest)?;
        }
        vcs::add_workspace(vcs, &repo, &dest, thread_id, base)?;
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
    fn remove_workspace(&self, thread_id: &str, detach: bool) -> Result<()> {
        anyhow::ensure!(self.runtime.running_since(thread_id).is_none(), "the thread is running");
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
        self.relocate(thread_id, &repo, None, &format!("Working directory is now {repo} again; use paths under it from now on."))
    }

    /// Point the thread at `cwd`, drop its live session, and leave `notice` for the model
    /// if it has history (a fresh thread learns its cwd from the system prompt).
    fn relocate(&self, thread_id: &str, cwd: &str, workspace: Option<&str>, notice: &str) -> Result<()> {
        let storage = self.storage.lock().unwrap();
        storage.set_cwd(thread_id, cwd, workspace)?;
        if let Some(parent) = storage.last_entry_id(thread_id)? {
            let entry = json!({
                "type": "custom_message",
                "customType": "dirigent:workspace",
                "content": notice,
                "display": true,
                "id": uuid::Uuid::new_v4().simple().to_string()[..8],
                "parentId": parent,
                "timestamp": runtime::iso_now(),
            });
            let entry = storage.append_entry(thread_id, entry, now())?;
            self.push(ServerMessage::Entry { thread_id: thread_id.to_owned(), entry });
        }
        drop(storage);
        // Its workspace is fresh, or it has none: nothing is missing until a sweep says so.
        self.missing.lock().unwrap().remove(thread_id);
        if self.runtime.is_loaded(thread_id) {
            let _ = self.runtime.unload(thread_id);
        }
        Ok(())
    }

    /// Replay the thread from sqlite into the runtime if it isn't live yet. `model` and
    /// `thinking_level` seed a fresh thread's session (see `Sessions.load` in the runtime).
    async fn ensure_loaded(&self, thread_id: &str, model: Option<&str>, thinking_level: Option<&str>) -> Result<()> {
        if self.runtime.is_loaded(thread_id) {
            return Ok(());
        }
        let (cwd, file_entries, tools) = {
            let storage = self.storage.lock().unwrap();
            let cwd = storage.thread_cwd(thread_id)?.ok_or_else(|| anyhow!("no such thread"))?;
            // A jj workspace the user rebased from elsewhere is made current before the
            // agent runs in it.
            if storage.thread_workspace(thread_id)?.is_some() {
                vcs::update_stale(Path::new(&cwd));
            }
            let tools = storage.thread_tools(thread_id)?;
            let (header, entries) = storage.get_thread(thread_id)?.unwrap();
            let mut file_entries: Vec<Value> = Vec::with_capacity(entries.len() + 1);
            if !header.is_null() {
                file_entries.push(header);
            }
            file_entries.extend(entries.into_iter().map(|e| e.json));
            // Pi wants its images inline again.
            storage.inflate(&mut file_entries);
            (cwd, file_entries, tools)
        };
        self.runtime.load(thread_id, &cwd, &file_entries, model, thinking_level, &tools).await
    }

    /// Persist and fan out what the runtime reports.
    fn on_runtime(self: &Arc<Self>, msg: RuntimeMessage) {
        match msg {
            RuntimeMessage::Loaded { thread_id, model, header, .. } => {
                eprintln!("[service] loaded {thread_id} model={}", model.as_deref().unwrap_or("?"));
                if let Err(e) = self.storage.lock().unwrap().set_header(&thread_id, &header) {
                    eprintln!("[service] set header: {e:#}");
                }
            }
            RuntimeMessage::Entry { thread_id, entry } => {
                match self.storage.lock().unwrap().append_entry(&thread_id, entry, now()) {
                    Ok(entry) => self.push(ServerMessage::Entry { thread_id, entry }),
                    Err(e) => eprintln!("[service] append entry: {e:#}"),
                }
                self.push_threads();
            }
            RuntimeMessage::Event { thread_id, event } => {
                let kind = event.get("type").and_then(Value::as_str).unwrap_or("");
                let lifecycle = matches!(kind, "agent_start" | "agent_settled");
                if kind == "agent_settled"
                    && let Some(ms) = self.runtime.settle(&thread_id)
                    && let Err(e) = self.storage.lock().unwrap().set_last_run(&thread_id, ms)
                {
                    eprintln!("[service] set last run: {e:#}");
                }
                // A sub-agent settled: its parent is about to be woken (`on_child_settled`), so
                // it counts as running from now, in the same `Threads` push that settles the
                // child; otherwise clients would see it finished for a moment and move it to
                // the inbox and back.
                let parent = match kind {
                    "agent_settled" => self.storage.lock().unwrap().thread_parent(&thread_id).ok().flatten(),
                    _ => None,
                };
                if let Some(parent) = parent {
                    self.runtime.begin_run(&parent);
                    let (this, id) = (self.clone(), thread_id.clone());
                    tokio::spawn(async move { this.on_child_settled(&id, &parent).await });
                }
                self.push(ServerMessage::Event { thread_id, event });
                if lifecycle {
                    self.push_threads();
                }
            }
            RuntimeMessage::Warning { thread_id, message } => {
                eprintln!("[service] warning {thread_id}: {message}");
                self.push(ServerMessage::Event { thread_id, event: json!({ "type": "warning", "message": message }) });
            }
            RuntimeMessage::Ask { thread_id, ask } => {
                if let Err(e) = self.storage.lock().unwrap().add_ask(&thread_id, &ask, now()) {
                    eprintln!("[service] add ask: {e:#}");
                }
                self.push_threads();
            }
            RuntimeMessage::Jobs { .. } => self.push_threads(),
            RuntimeMessage::Error { thread_id, message } => {
                eprintln!("[service] runtime error {thread_id:?}: {message}");
                // A prompt that failed before pi started (unknown model, nix shell) never settles.
                if let Some(t) = &thread_id
                    && self.runtime.settle(t).is_some()
                {
                    self.push_threads();
                }
                let message = match thread_id {
                    Some(t) => format!("{t}: {message}"),
                    None => message,
                };
                self.push(ServerMessage::Error { reply_to: None, message });
            }
            RuntimeMessage::Models { .. } | RuntimeMessage::Title(_) => {}
            RuntimeMessage::Crashed => {
                self.push(ServerMessage::Error { reply_to: None, message: "runtime restarted".into() });
                self.push_threads();
            }
        }
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

pub const USAGE: &str = "options: --data-dir DIR (dbs, workspaces, jobs; default $XDG_DATA_HOME/dirigent) \
                         --config-dir DIR (config.toml, hosts.toml, themes; default $XDG_CONFIG_HOME/dirigent)";

/// Take `--data-dir DIR` / `--config-dir DIR` from the command line, both binaries' only
/// options. Call it first thing in `main`, before anything asks for a directory. A test
/// instance points both elsewhere so it never touches the real dbs or config (and, unlike
/// overriding `XDG_*`, nothing it spawns sees a changed environment).
pub fn init_dirs() -> Result<()> {
    let dirs = parse_dirs(std::env::args().skip(1))?;
    let _ = DIRS.set(dirs);
    Ok(())
}

fn parse_dirs(args: impl Iterator<Item = String>) -> Result<Dirs> {
    let mut dirs = Dirs::default();
    let mut args = args.peekable();
    while let Some(arg) = args.next() {
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
        // Absolute, so it means the same to the runtime (`DIRIGENT_DATA_DIR`) whatever its cwd.
        *slot = Some(std::path::absolute(&value)?);
    }
    Ok(dirs)
}

/// `--config-dir`, else `$XDG_CONFIG_HOME/dirigent` (else `~/.config/dirigent`): the
/// desktop's `config.toml` and the machine's `hosts.toml`.
pub fn config_dir() -> PathBuf {
    if let Some(dir) = DIRS.get().and_then(|d| d.config.clone()) {
        return dir;
    }
    std::env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".config")))
        .expect("HOME not set")
        .join("dirigent")
}

/// `--data-dir`, else `$XDG_DATA_HOME/dirigent` (else `~/.local/share/dirigent`): the
/// service's and the desktop's dbs, workspaces, job logs. The runtime gets it as
/// `DIRIGENT_DATA_DIR`.
pub fn data_dir() -> PathBuf {
    if let Some(dir) = DIRS.get().and_then(|d| d.data.clone()) {
        return dir;
    }
    std::env::var_os("XDG_DATA_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".local/share")))
        .expect("HOME not set")
        .join("dirigent")
}

/// `DIRIGENT_RUNTIME` or, in a dev checkout, `../../dirigent_runtime/src/index.ts` relative to the binary.
fn runtime_cmd() -> Result<Vec<String>> {
    let path = match std::env::var("DIRIGENT_RUNTIME") {
        Ok(p) => PathBuf::from(p),
        Err(_) => {
            let exe = std::env::current_exe()?;
            let candidate = exe.ancestors().nth(3).map(|root| root.join("dirigent_runtime/src/index.ts"));
            candidate.filter(|p| p.exists()).ok_or_else(|| anyhow!("set DIRIGENT_RUNTIME to the runtime entry point"))?
        }
    };
    Ok(vec!["bun".into(), "run".into(), path.to_string_lossy().into_owned()])
}

/// Where the service listens: `DIRIGENT_BIND` (comma-separated IPs) if set, else loopback
/// plus this machine's tailscale addresses, so a host is reachable from the tailnet and
/// nowhere else. Never a wildcard: there is no auth.
fn bind_addrs(port: u16) -> Vec<SocketAddr> {
    let ips: Vec<IpAddr> = match std::env::var("DIRIGENT_BIND") {
        Ok(list) => list.split(',').filter_map(|s| s.trim().parse().ok()).collect(),
        Err(_) => std::iter::once(IpAddr::V4(Ipv4Addr::LOCALHOST)).chain(tailscale_ips()).collect(),
    };
    ips.into_iter().map(|ip| SocketAddr::new(ip, port)).collect()
}

/// The tailnet addresses `tailscale ip` reports; none when tailscale isn't installed or up.
fn tailscale_ips() -> Vec<IpAddr> {
    std::process::Command::new("tailscale")
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
    let storage = Storage::open(&data_dir().join("service.db"))?;
    let cmd = runtime_cmd()?;
    let (runtime, mut runtime_rx) = Runtime::start(cmd, format!("ws://127.0.0.1:{port}/ws"));
    let host_name = std::fs::read_to_string("/etc/hostname").map(|s| s.trim().to_owned()).unwrap_or_else(|_| "local".into());
    let svc = Arc::new(Service {
        storage: Mutex::new(storage),
        runtime,
        missing: Mutex::new(Default::default()),
        push: broadcast::channel(1024).0,
        host_name,
    });

    tokio::spawn({
        let svc = svc.clone();
        async move {
            while let Some(msg) = runtime_rx.recv().await {
                svc.on_runtime(msg);
            }
        }
    });
    tokio::spawn({
        let svc = svc.clone();
        async move {
            let mut n = 0u64;
            // The first sweep at once: `workspace_missing` knows nothing before it.
            loop {
                svc.sweep_workspaces();
                n += 1;
                if n.is_multiple_of(6) {
                    for id in svc.runtime.idle_threads(IDLE_UNLOAD) {
                        eprintln!("[service] unloading idle {id}");
                        let _ = svc.runtime.unload(&id);
                    }
                }
                tokio::time::sleep(Duration::from_secs(10)).await;
            }
        }
    });

    server::serve(svc, &bind_addrs(port)).await
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(args: &[&str]) -> Result<Dirs> {
        parse_dirs(args.iter().map(|s| s.to_string()))
    }

    #[test]
    fn dir_flags() {
        let cwd = std::env::current_dir().unwrap();
        assert_eq!(parse(&[]).unwrap(), Dirs::default());
        let d = parse(&["--data-dir", "/tmp/d", "--config-dir=rel/c"]).unwrap();
        assert_eq!(d.data, Some("/tmp/d".into()));
        assert_eq!(d.config, Some(cwd.join("rel/c")));
        assert!(parse(&["--data-dir"]).is_err());
        assert!(parse(&["--port", "1"]).is_err());
    }
}
