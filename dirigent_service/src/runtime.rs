//! Supervises the `dirigent_runtime` bun child: JSONL over stdin/stdout, respawn on exit.
//! The runtime is stateless, so a crash just means "nothing is loaded anymore".

use anyhow::{Result, anyhow};
use dirigent_protocol::{Ask, ImageAttachment, ModelInfo, PromptMode, TitleGen};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::Command;
use tokio::sync::{mpsc, oneshot};

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
    /// A thread's count of running `async_bash` jobs changed.
    Jobs {
        thread_id: String,
        running: u32,
    },
    Error {
        thread_id: Option<String>,
        message: String,
    },
    Models {
        models: Vec<ModelInfo>,
    },
    /// Reply to `generate_title`.
    Title(TitleResult),
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
    ListModels,
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
    /// Running `async_bash` jobs per thread, as the runtime last reported (`jobs`); absent = none.
    jobs: HashMap<String, u32>,
    /// Callers waiting on a `loaded` reply, per thread.
    pending_loads: HashMap<String, Vec<oneshot::Sender<Result<(), String>>>>,
    /// Callers waiting on a `models` reply (the runtime has no request ids; replies are FIFO).
    pending_models: Vec<oneshot::Sender<Vec<ModelInfo>>>,
    /// Callers waiting on a `title` reply, FIFO like `pending_models`. A failed generation
    /// comes back as an `error` without a thread id, which answers the oldest waiter.
    pending_titles: Vec<oneshot::Sender<Result<TitleResult, String>>>,
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
                Ok(status) => eprintln!("[runtime] exited: {status}"),
                Err(e) => eprintln!("[runtime] failed: {e:#}"),
            }
            self.on_crash();
            let _ = out.send(RuntimeMessage::Crashed);
            tokio::time::sleep(RESPAWN_DELAY).await;
        }
    }

    /// One child lifetime: spawn, pump stdin/stdout, return when it exits.
    async fn run_child(&self, out: &mpsc::UnboundedSender<RuntimeMessage>) -> Result<std::process::ExitStatus> {
        let mut child = Command::new(&self.cmd[0])
            .args(&self.cmd[1..])
            .env("DIRIGENT_URL", &self.url)
            .env("DIRIGENT_DATA_DIR", crate::data_dir())
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .kill_on_drop(true)
            .spawn()
            .map_err(|e| anyhow!("spawn {:?}: {e}", self.cmd))?;
        eprintln!("[runtime] started pid {:?}", child.id());

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
                Err(e) => eprintln!("[runtime] bad line ({e}): {}", &line[..line.len().min(200)]),
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
            RuntimeMessage::Models { models } => {
                for w in inner.pending_models.drain(..) {
                    let _ = w.send(models.clone());
                }
            }
            RuntimeMessage::Title(result) => {
                if !inner.pending_titles.is_empty() {
                    let _ = inner.pending_titles.remove(0).send(Ok(result.clone()));
                }
            }
            RuntimeMessage::Error { thread_id: Some(thread_id), message } => {
                for w in inner.pending_loads.remove(thread_id).unwrap_or_default() {
                    let _ = w.send(Err(message.clone()));
                }
            }
            RuntimeMessage::Error { thread_id: None, message } => {
                if !inner.pending_titles.is_empty() {
                    let _ = inner.pending_titles.remove(0).send(Err(message.clone()));
                }
            }
            RuntimeMessage::Event { thread_id, event } => {
                inner.last_activity.insert(thread_id.clone(), Instant::now());
                // `agent_settled` is cleared by the service (`settle`), which records the run length.
                // A run begun by `begin_run` keeps its earlier start.
                if event.get("type").and_then(Value::as_str) == Some("agent_start") {
                    inner.running.entry(thread_id.clone()).or_insert_with(now);
                }
            }
            RuntimeMessage::Jobs { thread_id, running: 0 } => {
                inner.jobs.remove(thread_id);
            }
            RuntimeMessage::Jobs { thread_id, running } => {
                inner.jobs.insert(thread_id.clone(), *running);
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
        for (_, waiters) in inner.pending_loads.drain() {
            for w in waiters {
                let _ = w.send(Err("runtime exited".into()));
            }
        }
        inner.pending_models.clear();
        inner.pending_titles.clear();
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

    /// How many `async_bash` jobs the thread has running.
    pub fn jobs(&self, thread_id: &str) -> u32 {
        self.inner.lock().unwrap().jobs.get(thread_id).copied().unwrap_or(0)
    }

    /// A prompt is on its way: count the thread as running from now, so clients see it work
    /// while the session loads rather than only once pi reports `agent_start`.
    pub fn begin_run(&self, thread_id: &str) {
        self.inner.lock().unwrap().running.entry(thread_id.to_owned()).or_insert_with(now);
    }

    /// The run is over: forget it and return how long it took.
    pub fn settle(&self, thread_id: &str) -> Option<i64> {
        self.inner.lock().unwrap().running.remove(thread_id).map(|since| now() - since)
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
    ) -> Result<()> {
        self.inner.lock().unwrap().last_activity.insert(thread_id.to_owned(), Instant::now());
        self.send(&Command_::Prompt { thread_id, text, mode, model, thinking_level, images, title, tools })
    }

    pub fn answer(&self, thread_id: &str, ask: &Ask, answers: Option<&[Vec<String>]>) -> Result<()> {
        self.inner.lock().unwrap().last_activity.insert(thread_id.to_owned(), Instant::now());
        self.send(&Command_::Answer { thread_id, ask, answers })
    }

    pub fn notify(&self, thread_id: &str, custom_type: &str, content: &str, details: &Value, notify: &str) -> Result<()> {
        self.inner.lock().unwrap().last_activity.insert(thread_id.to_owned(), Instant::now());
        self.send(&Command_::Notify { thread_id, custom_type, content, details, notify })
    }

    pub async fn generate_title(&self, title: &TitleGen, text: &str) -> Result<TitleResult> {
        let (tx, rx) = oneshot::channel();
        self.inner.lock().unwrap().pending_titles.push(tx);
        self.send(&Command_::GenerateTitle { title, text })?;
        match tokio::time::timeout(LOAD_TIMEOUT, rx).await {
            Ok(Ok(Ok(result))) => Ok(result),
            Ok(Ok(Err(message))) => Err(anyhow!("{message}")),
            _ => Err(anyhow!("runtime did not answer generate_title")),
        }
    }

    pub async fn list_models(&self) -> Result<Vec<ModelInfo>> {
        let (tx, rx) = oneshot::channel();
        self.inner.lock().unwrap().pending_models.push(tx);
        self.send(&Command_::ListModels)?;
        match tokio::time::timeout(LOAD_TIMEOUT, rx).await {
            Ok(Ok(models)) => Ok(models),
            _ => Err(anyhow!("runtime did not answer list_models")),
        }
    }

    pub fn abort(&self, thread_id: &str) -> Result<()> {
        self.send(&Command_::Abort { thread_id })
    }

    pub fn cancel_queued(&self, thread_id: &str, text: &str) -> Result<()> {
        self.send(&Command_::CancelQueued { thread_id, text })
    }

    /// Loaded, not running, no jobs running (unloading would kill them), and quiet for
    /// longer than `max_idle`.
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
