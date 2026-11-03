//! Supervises the `dirigent_runtime` bun child: JSONL over stdin/stdout, respawn on exit.
//! The runtime is stateless, so a crash just means "nothing is loaded anymore".

use anyhow::{Result, anyhow};
use dirigent_protocol::{Ask, AuthMethod, ImageAttachment, LoginStep, ModelInfo, PromptMode, ProviderInfo, TitleGen};
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::Command;
use tokio::sync::{mpsc, oneshot};
use tracing::{error, info, warn};

const LOAD_TIMEOUT: Duration = Duration::from_secs(60);
const RESPAWN_DELAY: Duration = Duration::from_secs(1);

/// Lines the runtime writes to stdout.
#[derive(Debug, Clone, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum RuntimeMessage {
    Loaded {
        thread_id: String,
        model: Option<String>,
        header: Value,
    },
    Entry {
        thread_id: String,
        entry: Value,
    },
    Event {
        thread_id: String,
        event: Value,
    },
    Warning {
        thread_id: String,
        message: String,
    },
    /// The model called `ask_user`; the service keeps the ask until it is answered.
    Ask {
        thread_id: String,
        ask: Ask,
    },
    /// A thread's counts of running `async_bash` jobs changed: those it waits on (their end
    /// wakes it) and those it left running (`notify: "never"`: servers, watchers).
    Jobs {
        thread_id: String,
        running: u32,
        free: u32,
    },
    /// A session load started (`active`) or finished capturing its nix dev shell.
    NixShell {
        thread_id: String,
        active: bool,
    },
    Error {
        thread_id: Option<String>,
        message: String,
    },
    /// The outcome of the request `id` (`Runtime::request`): what it gives, or why it failed.
    Reply {
        id: u64,
        value: Value,
    },
    Failed {
        id: u64,
        message: String,
    },
    /// A step of the login started by request `id`, for whoever started it.
    LoginStep {
        id: u64,
        step: LoginStep,
    },
    /// Synthesized by the supervisor when the child exits; all loaded state is gone.
    #[serde(skip)]
    Crashed,
}

/// Lines we write to the runtime's stdin.
#[derive(Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum Command_<'a> {
    Load {
        thread_id: &'a str,
        cwd: &'a str,
        entries: &'a [Value],
        /// Initial model / thinking level; the runtime only applies them to a fresh thread.
        model: Option<&'a str>,
        thinking_level: Option<&'a str>,
        /// The dirigent tools to activate (`async_bash`, `ask_user`).
        tools: &'a [String],
    },
    Unload {
        thread_id: &'a str,
    },
    Prompt {
        thread_id: &'a str,
        text: &'a str,
        mode: PromptMode,
        model: Option<&'a str>,
        thinking_level: Option<&'a str>,
        images: &'a [ImageAttachment],
        title: Option<&'a TitleGen>,
        tools: Option<&'a [String]>,
        /// The user message the prompt replaces (`EditMessage`): the session goes back to
        /// just before it first.
        #[serde(skip_serializing_if = "Option::is_none")]
        edit: Option<&'a str>,
    },
    /// The user's answers to `ask` (`None` = dismissed); the runtime hands them to the model.
    Answer {
        thread_id: &'a str,
        ask: &'a Ask,
        answers: Option<&'a [Vec<String>]>,
    },
    /// A notice for the model (a sub-agent ending): a custom message queued as
    /// a steer (`asap`) or follow-up (`when_idle`), starting a turn if the model is idle.
    Notify {
        thread_id: &'a str,
        custom_type: &'a str,
        content: &'a str,
        details: &'a Value,
        notify: &'a str,
    },
    GenerateTitle {
        id: u64,
        title: &'a TitleGen,
        text: &'a str,
    },
    Abort {
        thread_id: &'a str,
    },
    CancelQueued {
        thread_id: &'a str,
        text: &'a str,
    },
    ListModels {
        id: u64,
    },
    ListProviders {
        id: u64,
    },
    Login {
        id: u64,
        provider: &'a str,
        method: AuthMethod,
    },
    LoginAnswer {
        id: u64,
        n: u32,
        value: &'a str,
    },
    LoginCancel {
        id: u64,
    },
    Logout {
        id: u64,
        provider: &'a str,
    },
}

/// Where a login's steps go while it runs (`Runtime::login`). Called with the runtime's
/// state locked, so it must not call back into `Runtime`.
pub type Steps = Box<dyn Fn(LoginStep) + Send>;

/// What `ListProviders` gives.
#[derive(Deserialize)]
pub struct Providers {
    pub providers: Vec<ProviderInfo>,
    pub auth_path: String,
}

/// A generated title and what it cost (see `ServerMessage::Title`).
#[derive(Debug, Clone, Deserialize)]
pub struct TitleResult {
    pub title: String,
    pub model: String,
    pub input: u64,
    pub output: u64,
    pub cost: f64,
    pub duration_ms: i64,
}

impl TitleResult {
    /// The reply to the request `id` that asked for it.
    pub fn reply(self, id: u64) -> dirigent_protocol::ServerMessage {
        dirigent_protocol::ServerMessage::Title {
            reply_to: id,
            title: self.title,
            model: self.model,
            input: self.input,
            output: self.output,
            cost: self.cost,
            duration_ms: self.duration_ms,
        }
    }
}

pub struct Runtime {
    cmd: Vec<String>,
    /// The service's own ws url, handed to the child as `DIRIGENT_URL` (the `dirigent`
    /// tool is a client of the service it runs under).
    url: String,
    inner: Mutex<Inner>,
}

#[derive(Default)]
struct Inner {
    /// Stdin of the current child, fed by a writer task. `None` while respawning.
    stdin: Option<mpsc::UnboundedSender<String>>,
    loaded: HashSet<String>,
    /// Threads with a run in progress, with the start time (unix ms): from the prompt being
    /// sent (`begin_run`) or pi's `agent_start`, until `agent_settled`.
    running: HashMap<String, i64>,
    /// Running `async_bash` jobs per thread, as the runtime last reported (`jobs`): those it
    /// waits on and those it left running (`free_jobs`). Absent = none.
    jobs: HashMap<String, (u32, u32)>,
    /// Nix dev shell captures in progress per thread (`nix_shell`): a count, as a session
    /// reloaded meanwhile starts another while the first still runs; absent = none.
    nix_shells: HashMap<String, u32>,
    /// Threads whose context pi is compacting, since when (unix ms): `compaction_start` to
    /// `compaction_end`.
    compacting: HashMap<String, i64>,
    /// Callers waiting on a `loaded` reply, per thread.
    pending_loads: HashMap<String, Vec<oneshot::Sender<Result<(), String>>>>,
    /// Callers waiting on a `reply` / `failed`, by the request's id (`request`).
    pending: HashMap<u64, oneshot::Sender<Result<Value, String>>>,
    /// Where the steps of the logins in progress go, by their request id.
    login_steps: HashMap<u64, Steps>,
    next_request: u64,
    last_activity: HashMap<String, Instant>,
}

impl Runtime {
    /// Spawn the child (respawning forever) and return the supervisor plus the stream of its messages.
    pub fn start(cmd: Vec<String>, url: String) -> (Arc<Self>, mpsc::UnboundedReceiver<RuntimeMessage>) {
        let rt = Arc::new(Self { cmd, url, inner: Mutex::default() });
        let (tx, rx) = mpsc::unbounded_channel();
        tokio::spawn(rt.clone().supervise(tx));
        (rt, rx)
    }

    async fn supervise(self: Arc<Self>, out: mpsc::UnboundedSender<RuntimeMessage>) {
        loop {
            match self.run_child(&out).await {
                Ok(status) => error!("runtime exited: {status}"),
                Err(e) => error!("runtime failed: {e:#}"),
            }
            self.on_crash();
            let _ = out.send(RuntimeMessage::Crashed);
            tokio::time::sleep(RESPAWN_DELAY).await;
        }
    }

    /// One child lifetime: spawn, pump stdin/stdout, return when it exits.
    async fn run_child(&self, out: &mpsc::UnboundedSender<RuntimeMessage>) -> Result<std::process::ExitStatus> {
        let mut child = Command::from(crate::platform::command(&self.cmd[0]))
            .args(&self.cmd[1..])
            .env("DIRIGENT_URL", &self.url)
            .envs(crate::runtime_env())
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .kill_on_drop(true)
            .spawn()
            .map_err(|e| anyhow!("spawn {:?}: {e}", self.cmd))?;
        info!(pid = child.id(), "runtime started");

        // Its stderr (failures it reports, bun's own complaints) goes to our log.
        let mut stderr = BufReader::new(child.stderr.take().unwrap()).lines();
        tokio::spawn(async move {
            while let Ok(Some(line)) = stderr.next_line().await {
                warn!("runtime stderr: {line}");
            }
        });

        let mut stdin = child.stdin.take().unwrap();
        let (tx, mut rx) = mpsc::unbounded_channel::<String>();
        self.inner.lock().unwrap().stdin = Some(tx);
        tokio::spawn(async move {
            while let Some(line) = rx.recv().await {
                if stdin.write_all(line.as_bytes()).await.is_err() {
                    break;
                }
            }
        });

        let mut lines = BufReader::new(child.stdout.take().unwrap()).lines();
        while let Some(line) = lines.next_line().await? {
            match serde_json::from_str::<RuntimeMessage>(&line) {
                Ok(msg) => {
                    self.observe(&msg);
                    let _ = out.send(msg);
                }
                Err(e) => warn!("bad line from the runtime ({e}): {}", &line[..line.floor_char_boundary(200)]),
            }
        }
        Ok(child.wait().await?)
    }

    /// Keep loaded/running/pending bookkeeping in sync with what the child reports.
    fn observe(&self, msg: &RuntimeMessage) {
        let mut inner = self.inner.lock().unwrap();
        match msg {
            RuntimeMessage::Loaded { thread_id, .. } => {
                inner.loaded.insert(thread_id.clone());
                inner.last_activity.insert(thread_id.clone(), Instant::now());
                for w in inner.pending_loads.remove(thread_id).unwrap_or_default() {
                    let _ = w.send(Ok(()));
                }
            }
            RuntimeMessage::Reply { id, value } => {
                if let Some(w) = inner.pending.remove(id) {
                    let _ = w.send(Ok(value.clone()));
                }
            }
            RuntimeMessage::Failed { id, message } => {
                if let Some(w) = inner.pending.remove(id) {
                    let _ = w.send(Err(message.clone()));
                }
            }
            // Handed on here, in stdout order: the reply that ends the login comes after.
            RuntimeMessage::LoginStep { id, step } => {
                if let Some(steps) = inner.login_steps.get(id) {
                    steps(step.clone());
                }
            }
            RuntimeMessage::Error { thread_id: Some(thread_id), message } => {
                for w in inner.pending_loads.remove(thread_id).unwrap_or_default() {
                    let _ = w.send(Err(message.clone()));
                }
            }
            RuntimeMessage::Event { thread_id, event } => {
                inner.last_activity.insert(thread_id.clone(), Instant::now());
                match event.get("type").and_then(Value::as_str) {
                    // `agent_settled` is cleared by the service (`settle`), which records the run
                    // length. A run begun by `begin_run` keeps its earlier start.
                    Some("agent_start") => {
                        inner.running.entry(thread_id.clone()).or_insert_with(now);
                    }
                    // Pi compacts on its own inside a run, but may do it before `agent_start`
                    // (`_prepareRun`): a run nobody `begin_run` (a job's notice waking the thread)
                    // counts from here. Not a `manual` one (`session.compact()`), outside any run.
                    Some("compaction_start") => {
                        if event.get("reason").and_then(Value::as_str) != Some("manual") {
                            inner.running.entry(thread_id.clone()).or_insert_with(now);
                        }
                        inner.compacting.insert(thread_id.clone(), now());
                    }
                    Some("compaction_end") => {
                        inner.compacting.remove(thread_id);
                    }
                    _ => {}
                }
            }
            RuntimeMessage::Jobs { thread_id, running: 0, free: 0 } => {
                inner.jobs.remove(thread_id);
            }
            RuntimeMessage::Jobs { thread_id, running, free } => {
                inner.jobs.insert(thread_id.clone(), (*running, *free));
            }
            RuntimeMessage::NixShell { thread_id, active: true } => {
                *inner.nix_shells.entry(thread_id.clone()).or_default() += 1;
            }
            RuntimeMessage::NixShell { thread_id, active: false } => {
                if let Some(n) = inner.nix_shells.get_mut(thread_id) {
                    *n -= 1;
                    if *n == 0 {
                        inner.nix_shells.remove(thread_id);
                    }
                }
            }
            _ => {}
        }
    }

    fn on_crash(&self) {
        let mut inner = self.inner.lock().unwrap();
        inner.stdin = None;
        inner.loaded.clear();
        inner.running.clear();
        inner.jobs.clear();
        inner.nix_shells.clear();
        inner.compacting.clear();
        for (_, waiters) in inner.pending_loads.drain() {
            for w in waiters {
                let _ = w.send(Err("runtime exited".into()));
            }
        }
        for (_, w) in inner.pending.drain() {
            let _ = w.send(Err("runtime exited".into()));
        }
        inner.login_steps.clear();
    }

    fn send(&self, cmd: &Command_) -> Result<()> {
        let inner = self.inner.lock().unwrap();
        let stdin = inner.stdin.as_ref().ok_or_else(|| anyhow!("runtime not running"))?;
        let mut line = serde_json::to_string(cmd)?;
        line.push('\n');
        stdin.send(line).map_err(|_| anyhow!("runtime stdin closed"))
    }

    pub fn is_loaded(&self, thread_id: &str) -> bool {
        self.inner.lock().unwrap().loaded.contains(thread_id)
    }

    pub fn running_since(&self, thread_id: &str) -> Option<i64> {
        self.inner.lock().unwrap().running.get(thread_id).copied()
    }

    /// How many `async_bash` jobs the thread has running that it waits on.
    pub fn jobs(&self, thread_id: &str) -> u32 {
        self.inner.lock().unwrap().jobs.get(thread_id).map_or(0, |j| j.0)
    }

    /// How many `async_bash` jobs the thread left running (`notify: "never"`).
    pub fn free_jobs(&self, thread_id: &str) -> u32 {
        self.inner.lock().unwrap().jobs.get(thread_id).map_or(0, |j| j.1)
    }

    /// The thread's session is capturing its nix dev shell.
    pub fn nix_shell(&self, thread_id: &str) -> bool {
        self.inner.lock().unwrap().nix_shells.contains_key(thread_id)
    }

    /// Since when (unix ms) pi is compacting the thread's context, if it is.
    pub fn compacting_since(&self, thread_id: &str) -> Option<i64> {
        self.inner.lock().unwrap().compacting.get(thread_id).copied()
    }

    /// A prompt is on its way: count the thread as running from now, so clients see it work
    /// while the session loads rather than only once pi reports `agent_start`.
    pub fn begin_run(&self, thread_id: &str) {
        self.inner.lock().unwrap().running.entry(thread_id.to_owned()).or_insert_with(now);
    }

    /// The run is over: forget it and return how long it took.
    pub fn settle(&self, thread_id: &str) -> Option<i64> {
        let mut inner = self.inner.lock().unwrap();
        // Pi's own compactions never outlive their run.
        inner.compacting.remove(thread_id);
        inner.running.remove(thread_id).map(|since| now() - since)
    }

    /// Load a thread from its stored header + entries and wait until the runtime confirms.
    /// Concurrent loads of the same thread share one request.
    pub async fn load(
        &self,
        thread_id: &str,
        cwd: &str,
        entries: &[Value],
        model: Option<&str>,
        thinking_level: Option<&str>,
        tools: &[String],
    ) -> Result<()> {
        let (tx, rx) = oneshot::channel();
        let first = {
            let mut inner = self.inner.lock().unwrap();
            if inner.loaded.contains(thread_id) {
                return Ok(());
            }
            let waiters = inner.pending_loads.entry(thread_id.to_owned()).or_default();
            waiters.push(tx);
            waiters.len() == 1
        };
        if first && let Err(e) = self.send(&Command_::Load { thread_id, cwd, entries, model, thinking_level, tools }) {
            self.inner.lock().unwrap().pending_loads.remove(thread_id);
            return Err(e);
        }
        match tokio::time::timeout(LOAD_TIMEOUT, rx).await {
            Ok(Ok(Ok(()))) => Ok(()),
            Ok(Ok(Err(msg))) => Err(anyhow!("load failed: {msg}")),
            Ok(Err(_)) => Err(anyhow!("runtime exited during load")),
            Err(_) => {
                self.inner.lock().unwrap().pending_loads.remove(thread_id);
                Err(anyhow!("load timed out"))
            }
        }
    }

    pub fn unload(&self, thread_id: &str) -> Result<()> {
        self.send(&Command_::Unload { thread_id })?;
        let mut inner = self.inner.lock().unwrap();
        inner.loaded.remove(thread_id);
        inner.running.remove(thread_id);
        inner.compacting.remove(thread_id);
        inner.last_activity.remove(thread_id);
        Ok(())
    }

    #[allow(clippy::too_many_arguments)]
    pub fn prompt(
        &self,
        thread_id: &str,
        text: &str,
        mode: PromptMode,
        model: Option<&str>,
        thinking_level: Option<&str>,
        images: &[ImageAttachment],
        title: Option<&TitleGen>,
        tools: Option<&[String]>,
        edit: Option<&str>,
    ) -> Result<()> {
        self.inner.lock().unwrap().last_activity.insert(thread_id.to_owned(), Instant::now());
        self.send(&Command_::Prompt { thread_id, text, mode, model, thinking_level, images, title, tools, edit })
    }

    pub fn answer(&self, thread_id: &str, ask: &Ask, answers: Option<&[Vec<String>]>) -> Result<()> {
        self.inner.lock().unwrap().last_activity.insert(thread_id.to_owned(), Instant::now());
        self.send(&Command_::Answer { thread_id, ask, answers })
    }

    pub fn notify(&self, thread_id: &str, custom_type: &str, content: &str, details: &Value, notify: &str) -> Result<()> {
        self.inner.lock().unwrap().last_activity.insert(thread_id.to_owned(), Instant::now());
        self.send(&Command_::Notify { thread_id, custom_type, content, details, notify })
    }

    /// A fresh request id: what `request` sends a command under, and a login is known by.
    pub fn next_id(&self) -> u64 {
        let mut inner = self.inner.lock().unwrap();
        inner.next_request += 1;
        inner.next_request
    }

    /// Send `cmd` (made with request `id`) and wait for its `reply` or `failed`: at most
    /// `timeout`, or as long as it takes (a login waits on the user). Requests run side by
    /// side in the runtime and finish in any order.
    async fn request<T: DeserializeOwned>(&self, id: u64, timeout: Option<Duration>, cmd: Command_<'_>) -> Result<T> {
        let (tx, rx) = oneshot::channel();
        self.inner.lock().unwrap().pending.insert(id, tx);
        let answer = match self.send(&cmd) {
            Ok(()) => match timeout {
                Some(t) => tokio::time::timeout(t, rx).await.ok().and_then(Result::ok),
                None => rx.await.ok(),
            },
            Err(e) => Some(Err(format!("{e:#}"))),
        };
        // Answered or not, the waiter is done: an answer coming late finds none.
        self.inner.lock().unwrap().pending.remove(&id);
        match answer {
            Some(Ok(value)) => Ok(serde_json::from_value(value)?),
            Some(Err(message)) => Err(anyhow!("{message}")),
            None => Err(anyhow!("the runtime did not answer")),
        }
    }

    pub async fn generate_title(&self, title: &TitleGen, text: &str) -> Result<TitleResult> {
        let id = self.next_id();
        self.request(id, Some(LOAD_TIMEOUT), Command_::GenerateTitle { id, title, text }).await
    }

    pub async fn list_models(&self) -> Result<Vec<ModelInfo>> {
        let id = self.next_id();
        self.request(id, Some(LOAD_TIMEOUT), Command_::ListModels { id }).await
    }

    pub async fn list_providers(&self) -> Result<Providers> {
        let id = self.next_id();
        self.request(id, Some(LOAD_TIMEOUT), Command_::ListProviders { id }).await
    }

    /// Run login `id` (from `next_id`) to its end, handing its steps to `steps` meanwhile.
    /// No timeout: it waits on the user (`login_cancel` ends it).
    pub async fn login(&self, id: u64, provider: &str, method: AuthMethod, steps: Steps) -> Result<()> {
        self.inner.lock().unwrap().login_steps.insert(id, steps);
        let result = self.request::<Value>(id, None, Command_::Login { id, provider, method }).await;
        self.inner.lock().unwrap().login_steps.remove(&id);
        result.map(drop)
    }

    /// Answer prompt `n` of login `id`.
    pub fn login_answer(&self, id: u64, n: u32, value: &str) -> Result<()> {
        self.send(&Command_::LoginAnswer { id, n, value })
    }

    pub fn login_cancel(&self, id: u64) -> Result<()> {
        self.send(&Command_::LoginCancel { id })
    }

    pub async fn logout(&self, provider: &str) -> Result<()> {
        let id = self.next_id();
        self.request::<Value>(id, Some(LOAD_TIMEOUT), Command_::Logout { id, provider }).await.map(drop)
    }

    pub fn abort(&self, thread_id: &str) -> Result<()> {
        self.send(&Command_::Abort { thread_id })
    }

    pub fn cancel_queued(&self, thread_id: &str, text: &str) -> Result<()> {
        self.send(&Command_::CancelQueued { thread_id, text })
    }

    /// Loaded, not running, no jobs running (unloading would kill them, even those it left
    /// running), and quiet for longer than `max_idle`.
    pub fn idle_threads(&self, max_idle: Duration) -> Vec<String> {
        let inner = self.inner.lock().unwrap();
        inner
            .loaded
            .iter()
            .filter(|id| !inner.running.contains_key(*id))
            .filter(|id| !inner.jobs.contains_key(*id))
            .filter(|id| inner.last_activity.get(*id).is_none_or(|t| t.elapsed() > max_idle))
            .cloned()
            .collect()
    }
}

/// Unix millis.
pub fn now() -> i64 {
    std::time::UNIX_EPOCH.elapsed().map(|d| d.as_millis() as i64).unwrap_or(0)
}

/// Now as pi writes entry timestamps: `2026-01-02T03:04:05.678Z`.
pub fn iso_now() -> String {
    chrono::Utc::now().format("%Y-%m-%dT%H:%M:%S%.3fZ").to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A request's outcome comes back under its id: what it gives, or why it failed.
    #[test]
    fn replies_carry_their_id() {
        let line = r#"{"type":"reply","id":7,"value":{"title":"Fix it","model":"a/b","input":1,"output":2,"cost":0.5,"duration_ms":9}}"#;
        let Ok(RuntimeMessage::Reply { id: 7, value }) = serde_json::from_str(line) else { panic!("{line}") };
        assert_eq!(serde_json::from_value::<TitleResult>(value).unwrap().title, "Fix it");
        let line = r#"{"type":"failed","id":8,"message":"no credentials"}"#;
        assert!(matches!(serde_json::from_str(line), Ok(RuntimeMessage::Failed { id: 8, .. })));
    }
}
