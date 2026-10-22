//! dirigent_service: owns this host's threads (sqlite), drives the pi runtime,
//! and serves them to desktops over WebSocket. See plan.md §3.
//!
//! A library so the desktop can run the local service in-process (`run`); the
//! `dirigent_service` binary is a thin wrapper for remote hosts.

mod blobs;
mod git;
mod runtime;
mod server;
mod storage;
mod vcs;

use anyhow::{Result, anyhow};
use dirigent_protocol::{ClientMessage, PromptMode, ServerMessage, ThreadSummary, Vcs};
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

/// The local service's port: `DIRIGENT_PORT` or 7777. The desktop uses it both to reach and
/// to embed the local service, so a test instance with its own port never touches the real one.
pub fn port() -> u16 {
    std::env::var("DIRIGENT_PORT").ok().and_then(|p| p.parse().ok()).unwrap_or(DEFAULT_PORT)
}

pub struct Service {
    storage: Mutex<Storage>,
    runtime: Arc<Runtime>,
    /// Threads whose workspace directory was gone at the last `sweep_workspaces`.
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
        for t in &mut threads {
            t.running_since = self.runtime.running_since(&t.id);
            // A workspace removed outside dirigent: the thread can't run until it is dealt with.
            t.workspace_missing = t.workspace.as_deref().is_some_and(|w| !Path::new(w).is_dir());
        }
        Ok(threads)
    }

    fn push_threads(&self) {
        match self.threads() {
            Ok(threads) => self.push(ServerMessage::Threads { reply_to: None, threads }),
            Err(e) => eprintln!("[service] list threads: {e:#}"),
        }
    }

    /// Notice workspaces removed outside dirigent: push the thread list when the set of
    /// missing ones changed since the last look. Runs periodically and on `GetThread`.
    fn sweep_workspaces(&self) {
        let now: std::collections::HashSet<String> = match self.threads() {
            Ok(threads) => threads.into_iter().filter(|t| t.workspace_missing).map(|t| t.id).collect(),
            Err(_) => return,
        };
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
    pub async fn handle(&self, req: ClientMessage) -> ServerMessage {
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
            | ClientMessage::DeleteThread { id, .. }
            | ClientMessage::ListModels { id }
            | ClientMessage::GenerateTitle { id, .. }
            | ClientMessage::GetBlob { id, .. }
            | ClientMessage::ListUsage { id, .. } => *id,
        };
        match self.try_handle(req).await {
            Ok(reply) => reply,
            Err(e) => ServerMessage::Error { reply_to: Some(id), message: format!("{e:#}") },
        }
    }

    async fn try_handle(&self, req: ClientMessage) -> Result<ServerMessage> {
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
            ClientMessage::CreateThread { id, project_id, workspace, base } => {
                let thread_id = uuid::Uuid::now_v7().to_string();
                let mut thread = {
                    let storage = self.storage.lock().unwrap();
                    let cwd = storage.project_path(&project_id)?.ok_or_else(|| anyhow!("no such project"))?;
                    storage.create_thread(&thread_id, &project_id, &cwd, now())?
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
                let vcs = vcs::detect(dir);
                let dirty = vcs.map(|v| vcs::dirty(v, dir)).transpose()?.unwrap_or(false);
                let branch = vcs.and_then(|v| vcs::branch(v, dir));
                let refs = vcs.map(|v| vcs::refs(v, dir)).unwrap_or_default();
                ServerMessage::VcsStatus { reply_to: id, path, vcs, dirty, branch, refs }
            }
            ClientMessage::Prompt { id, thread_id, text, mode, model, thinking_level, images, title, tools } => {
                // The run starts now, not at pi's `agent_start`: loading the session (and a nix
                // dev shell) can take seconds, and clients should see the thread working with
                // the model it will use for all of it.
                if mode == PromptMode::Prompt {
                    self.storage.lock().unwrap().set_model(&thread_id, model.as_deref(), thinking_level.as_deref())?;
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

    /// `$XDG_DATA_HOME/dirigent/workspaces/<name>`: the directory is named like its branch.
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
    fn on_runtime(&self, msg: RuntimeMessage) {
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

fn data_dir() -> PathBuf {
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
    let (runtime, mut runtime_rx) = Runtime::start(runtime_cmd()?);
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
            loop {
                tokio::time::sleep(Duration::from_secs(10)).await;
                svc.sweep_workspaces();
                n += 1;
                if n.is_multiple_of(6) {
                    for id in svc.runtime.idle_threads(IDLE_UNLOAD) {
                        eprintln!("[service] unloading idle {id}");
                        let _ = svc.runtime.unload(&id);
                    }
                }
            }
        }
    });

    server::serve(svc, &bind_addrs(port)).await
}
